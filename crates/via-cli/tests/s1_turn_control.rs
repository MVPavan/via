//! Turn control through the real `via` binary and daemon (design T3 §2–§5,
//! §11): `cancel` (C1 §3.5), `close` and the `closing` gate (§3.6, §7.1),
//! the idle deadline (§4 `deadlines.idle_ms`), and F19–F21. Waits are bounded
//! waits on durable rows, fake gates, failpoint acknowledgements or process
//! exit; a sleep only lets time pass, never orders two events.

#[path = "support/daemon.rs"]
#[expect(
    dead_code,
    reason = "shared support; this file uses the direct status probe"
)]
mod daemon;
#[path = "support/evidenced.rs"]
mod evidenced;
#[cfg(feature = "test-failpoints")]
#[path = "support/failpoints.rs"]
mod failpoints;
#[cfg(feature = "test-failpoints")]
#[path = "support/hits.rs"]
mod hits;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
#[expect(
    dead_code,
    reason = "shared support; the daemon module uses part of it"
)]
mod scenario;
mod support;

use std::error::Error;
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use evidenced::evidenced;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A finished CLI call.
struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

/// Runs `command` to exit, killing it after `timeout`.
fn run_command(command: &mut Command, timeout: Duration) -> TestResult<Captured> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            break (child.wait()?, true);
        }
        thread::sleep(Duration::from_millis(5));
    };
    let read = |file: &mut File| -> TestResult<Vec<u8>> {
        file.rewind()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    Ok(Captured {
        status,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
        timed_out,
    })
}

/// One isolated deployment: private state, runtime and fake sync dirs, the
/// fake fixture, and extra daemon environment.
struct Sandbox {
    root: tempfile::TempDir,
    /// The scenario's evidence, collected when the sandbox is dropped.
    evidence: Option<support::evidence::Evidence>,
    /// Cleared by a scenario with no Store by design.
    store_expected: std::sync::atomic::AtomicBool,
    /// Cleared by a scenario whose turns launch no vendor by design.
    folders_expected: std::sync::atomic::AtomicBool,
    via: PathBuf,
    fake: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    sync: PathBuf,
    fixture: PathBuf,
    env: Vec<(&'static str, String)>,
    #[cfg(feature = "test-failpoints")]
    failpoints: failpoints::Failpoints,
}

/// Collects the scenario's evidence once every daemon has exited: the
/// children it started are reaped by their guards, which borrow the
/// sandbox, and [`evidenced::stop_daemons`] proves the rest gone.
impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(evidence) = self.evidence.take() {
            self.root.disable_cleanup(true);
            let exited = evidenced::stop_daemons(&self.runtime, &self.state, || {
                let _ = self.run(&["daemon", "stop", "--force", "--json"]);
            });
            let expected = evidenced::Expected {
                store: self
                    .store_expected
                    .load(std::sync::atomic::Ordering::Relaxed),
                folders: self
                    .folders_expected
                    .load(std::sync::atomic::Ordering::Relaxed),
            };
            evidenced::park(
                evidence,
                self.root.path().to_owned(),
                &self.state,
                expected,
                exited,
            );
        }
    }
}

impl Sandbox {
    /// Declares a scenario whose turns launch no vendor by design: only
    /// their evidence folders are waived; the Store, envelopes, events and
    /// cleanup stay required, and a launched turn must have its folder.
    #[cfg(feature = "test-failpoints")]
    fn no_launch(&self) {
        self.folders_expected
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn new(fixture: &Value) -> TestResult<Self> {
        let via = PathBuf::from(env!("CARGO_BIN_EXE_via"));
        let fake = via
            .parent()
            .ok_or("via binary has no parent directory")?
            .join("via-fake-agent");
        if !fake.is_file() {
            return Err(
                format!("missing {}; build -p via-fake-agent first", fake.display()).into(),
            );
        }
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let [state, runtime, sync] =
            ["state", "runtime", "sync"].map(|name| root.path().join(name));
        for path in [&state, &runtime, &sync] {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        let fixture_path = root.path().join("fixture.json");
        fs::write(&fixture_path, serde_json::to_vec(fixture)?)?;
        #[cfg(feature = "test-failpoints")]
        let failpoints = failpoints::Failpoints::new(root.path())?;
        let evidence = evidenced::open(&fake, &fixture_path)?;
        Ok(Self {
            root,
            evidence: Some(evidence),
            store_expected: std::sync::atomic::AtomicBool::new(true),
            folders_expected: std::sync::atomic::AtomicBool::new(true),
            via,
            fake,
            state,
            runtime,
            sync,
            fixture: fixture_path,
            env: Vec::new(),
            #[cfg(feature = "test-failpoints")]
            failpoints,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.via);
        command.env_clear();
        command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        command.env("VIA_STATE_DIR", &self.state);
        command.env("VIA_RUNTIME_DIR", &self.runtime);
        command.env("VIA_FAKE_AGENT_BINARY", &self.fake);
        command.env("VIA_FAKE_SCENARIO", &self.fixture);
        command.env("VIA_FAKE_SYNC_DIR", &self.sync);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    fn run(&self, args: &[&str]) -> TestResult<Captured> {
        let mut command = self.command();
        command.args(args);
        let captured = run_command(&mut command, Duration::from_secs(60))?;
        if captured.timed_out {
            return Err(format!("via {args:?} timed out").into());
        }
        Ok(captured)
    }

    /// One successful CLI call's JSON output.
    fn ok(&self, args: &[&str]) -> TestResult<Value> {
        let captured = self.run(args)?;
        if !captured.status.success() {
            return Err(format!(
                "via {args:?} exited {}: {}",
                captured.status,
                String::from_utf8_lossy(&captured.stderr)
            )
            .into());
        }
        Ok(serde_json::from_slice(&captured.stdout)?)
    }

    /// One CLI call refused with request error `kind`: the error object.
    fn refused(&self, args: &[&str], kind: &str) -> TestResult<Value> {
        let captured = self.run(args)?;
        let error: Value = serde_json::from_slice(&captured.stderr).map_err(|_| {
            format!(
                "via {args:?}: expected {kind}, got exit {} stdout {} stderr {}",
                captured.status,
                String::from_utf8_lossy(&captured.stdout),
                String::from_utf8_lossy(&captured.stderr)
            )
        })?;
        if captured.status.code() != Some(2) || error["data"]["kind"] != kind {
            return Err(format!("via {args:?}: expected {kind}, got {error}").into());
        }
        Ok(error)
    }

    fn start(&self) -> TestResult<Daemon<'_>> {
        let mut command = self.command();
        #[cfg(feature = "test-failpoints")]
        self.failpoints.activate(&mut command);
        let trace = self.root.path().join("daemon.trace");
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::options().create(true).append(true).open(&trace)?);
        let mut daemon = Daemon {
            child: command.spawn()?,
            sandbox: self,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = daemon.child.try_wait()? {
                return Err(format!("daemon exited before readiness: {status}").into());
            }
            // A direct probe: never auto-starts a second daemon, which could
            // win `daemon.lock` over the child, even over a stale socket file.
            if daemon::serving_pid(&self.runtime) == Some(daemon.child.id()) {
                return Ok(daemon);
            }
            if Instant::now() >= deadline {
                return Err("daemon readiness deadline elapsed".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Spawns turn 1 in the background: `(session, handle)`.
    fn spawn(&self, prompt: &str, extra: &[&str]) -> TestResult<(String, String)> {
        let mut args = vec![
            "spawn",
            "--harness",
            "fake",
            "--model",
            "fake",
            "--prompt",
            prompt,
            "--background",
            "--json",
        ];
        args.extend_from_slice(extra);
        let receipt = self.ok(&args)?;
        let session = receipt["session_id"]
            .as_str()
            .ok_or("receipt has no session")?;
        let handle = receipt["handle"].as_str().ok_or("receipt has no handle")?;
        Ok((session.to_owned(), handle.to_owned()))
    }

    fn resume(&self, session: &str, handle: &str, prompt: &str) -> TestResult<Value> {
        self.ok(&[
            "resume", session, "--prompt", prompt, "--handle", handle, "--json",
        ])
    }

    fn wait(&self, address: &str) -> TestResult<Value> {
        self.ok(&["wait", address, "--timeout-ms", "30000", "--json"])
    }

    /// `via status <session>` (C1 §3.7).
    fn status(&self, session: &str) -> TestResult<Value> {
        self.ok(&["status", session, "--json"])
    }

    fn events(&self, session: &str) -> TestResult<Vec<Value>> {
        let page = self.ok(&["events", session, "--json"])?;
        Ok(page["events"].as_array().cloned().unwrap_or_default())
    }

    /// Waits until the fake created `name` in its sync dir.
    fn await_file(&self, name: &str) -> TestResult {
        let path = self.sync.join(name);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !path.exists() {
            if Instant::now() >= deadline {
                return Err(format!("the fake did not create {name}").into());
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn release(&self, gate: &str) -> TestResult {
        fs::write(self.sync.join(format!("{gate}.release")), b"")?;
        Ok(())
    }

    fn pid(&self, name: &str) -> TestResult<u32> {
        self.await_file(name)?;
        Ok(fs::read_to_string(self.sync.join(name))?.trim().parse()?)
    }

    /// One value read from the live Store through a read-only connection.
    fn query<T: rusqlite::types::FromSql>(&self, sql: &str) -> TestResult<T> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        Ok(store.query_row(sql, [], |row| row.get(0))?)
    }

    /// Waits until `sql` reads `expected`.
    fn await_row(&self, sql: &str, expected: &str) -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if self
                .query::<String>(sql)
                .is_ok_and(|value| value == expected)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("{sql} never read {expected}").into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// A file in the turn's evidence folder (Task 4 design §7.1).
    fn evidence_file(&self, session: &str, turn: u32, name: &str) -> TestResult<Vec<u8>> {
        Ok(fs::read(
            self.state
                .join("evidence")
                .join(session)
                .join(turn.to_string())
                .join(name),
        )?)
    }

    /// Everything the daemon wrote: its stderr trace, then `via.log`
    /// (Task 4 design §7.6).
    fn trace(&self) -> String {
        let mut trace =
            fs::read_to_string(self.root.path().join("daemon.trace")).unwrap_or_default();
        trace.push_str(&fs::read_to_string(self.state.join("via.log")).unwrap_or_default());
        trace
    }
}

/// The daemon child. `finish` force-stops it and proves outer cleanup;
/// dropping it force-stops and reaps it.
struct Daemon<'a> {
    child: Child,
    sandbox: &'a Sandbox,
}

impl Daemon<'_> {
    #[cfg(feature = "test-failpoints")]
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits for the daemon to exit by itself.
    fn exit(&mut self, within: Duration) -> TestResult<ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err("the daemon did not exit in time".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Force-stops the daemon, then proves every anchor's group absent, as
    /// the runtime §11.2 outer harness does.
    fn finish(mut self) -> TestResult {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.sandbox.run(&["daemon", "stop", "--force", "--json"]);
        }
        self.exit(Duration::from_secs(15))?;
        self.verify()
    }

    fn verify(&self) -> TestResult {
        let rows = outer_cleanup::snapshot(&self.sandbox.state.join("store.sqlite3"))?;
        let anchors = outer_cleanup::verify(&rows, Instant::now() + Duration::from_secs(10));
        let absent = anchors["status"] == "quiescent" && anchors["absence_proven"] == true;
        let none = anchors["status"] == "no_anchors";
        if absent || none {
            Ok(())
        } else {
            Err(format!("outer cleanup is unverified: {anchors}").into())
        }
    }
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.sandbox.run(&["daemon", "stop", "--force", "--json"]);
            let deadline = Instant::now() + Duration::from_secs(15);
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn accepted(turn: u32) -> Value {
    json!({"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":format!("fake-turn-{turn}")}})
}

fn terminal(turn: u32, status: &str, stop_reason: &str) -> Value {
    json!({"action":"emit","message":{"type":"terminal","vendor_turn_id":format!("fake-turn-{turn}"),
        "status":status,"final_text":"done","stop_reason":stop_reason}})
}

fn gate(name: &str) -> Value {
    json!({"action":"gate","name":name})
}

/// Reads the interrupt Route sends at most once, then reports `interrupted`.
fn interrupted(turn: u32) -> [Value; 2] {
    [
        json!({"action":"expect_request","expected":{"type":"interrupt"}}),
        terminal(turn, "interrupted", "interrupted"),
    ]
}

fn script(prompt: &str, turn: u32, steps: Vec<Value>) -> Value {
    let mut script = json!({"expected_request":{"type":"start","turn":turn,"prompt":prompt}});
    script["steps"] = Value::Array(steps);
    script
}

fn scripts(scripts: Vec<Value>) -> Value {
    let mut fixture = json!({});
    fixture["scripts"] = Value::Array(scripts);
    fixture
}

/// A completing turn `turn` for `prompt`.
fn completes(prompt: &str, turn: u32) -> Value {
    script(
        prompt,
        turn,
        vec![accepted(turn), terminal(turn, "completed", "end_turn")],
    )
}

fn check(condition: bool, message: impl FnOnce() -> String) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message().into())
    }
}

/// `pid` is gone, or a zombie awaiting its reaper.
fn process_live(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|text| {
            let state = text.get(text.rfind(')')? + 2..)?.chars().next()?;
            Some(state != 'Z' && state != 'X')
        })
        .unwrap_or(false)
}

fn event_types(events: &[Value], turn: u32) -> Vec<String> {
    events
        .iter()
        .filter(|event| event["turn"] == turn)
        .filter_map(|event| event["type"].as_str().map(str::to_owned))
        .collect()
}

fn epoch_ms(at: &Value) -> TestResult<i128> {
    let text = at.as_str().ok_or("timestamp is not a string")?;
    let parsed = humantime_rfc3339(text)?;
    Ok(i128::try_from(
        parsed.duration_since(SystemTime::UNIX_EPOCH)?.as_millis(),
    )?)
}

/// Parses the daemon's RFC 3339 UTC timestamps (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
fn humantime_rfc3339(text: &str) -> TestResult<SystemTime> {
    let bytes = text.as_bytes();
    let number = |range: std::ops::Range<usize>| -> TestResult<u64> {
        Ok(text.get(range).ok_or("short timestamp")?.parse::<u64>()?)
    };
    if bytes.len() < 20 || bytes[10] != b'T' {
        return Err(format!("unexpected timestamp {text}").into());
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let millis = match text.get(19..20) {
        Some(".") => {
            let digits: String = text[20..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            let padded = format!("{digits:0<3}");
            padded[..3].parse::<u64>()?
        }
        _ => 0,
    };
    // Days from civil (Howard Hinnant's algorithm).
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    Ok(SystemTime::UNIX_EPOCH + Duration::from_millis(seconds * 1000 + millis))
}

// ---------------------------------------------------------------- cancel

/// Design §3.2, §11: a queued turn is dropped `cancelled`, `acknowledged`,
/// `quiescent`, with `cancel_cause = 'cancel'` and no submission; the
/// successor still runs, and nothing launched for the cancelled turn.
#[test]
fn s1_cancel_queued_turn_is_acknowledged_quiescent() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(vec![
            script(
                "first",
                1,
                vec![
                    accepted(1),
                    gate("hold1"),
                    terminal(1, "completed", "end_turn"),
                ],
            ),
            completes("third", 3),
        ]))?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("first", &[])?;
        sandbox.await_file("hold1.entered")?;
        sandbox.resume(&session, &handle, "second")?;
        let reply = sandbox.ok(&[
            "cancel", &session, "--turn", "2", "--handle", &handle, "--json",
        ])?;
        let address = format!("{session}/2");
        check(
            reply["turn"] == address.as_str()
                && reply["state"] == "cancelled"
                && reply["already_terminal"] == false
                && reply["cancel"]["outcome"] == "acknowledged"
                && reply["cancel"]["cleanup"] == "quiescent"
                && reply["cancel"]["requested_at"].is_string()
                && reply["cancel"]["settled_at"].is_string(),
            || format!("queued cancel reply: {reply}"),
        )?;
        let envelope = sandbox.ok(&["result", &address, "--json"])?;
        check(
            envelope["state"] == "cancelled"
                && envelope["stop_reason"] == "interrupted"
                && envelope["timestamps"]["submitted_at"].is_null()
                && envelope["cancel"] == reply["cancel"],
            || format!("cancelled envelope: {envelope}"),
        )?;
        let cause: String = sandbox.query(&format!(
            "SELECT cancel_cause FROM turns WHERE session_id='{session}' AND number=2"
        ))?;
        check(cause == "cancel", || format!("cancel_cause {cause}"))?;
        // Idempotent: the terminal turn replies with its recorded cancel.
        let again = sandbox.ok(&[
            "cancel", &session, "--turn", "2", "--handle", &handle, "--json",
        ])?;
        check(
            again["already_terminal"] == true && again["cancel"] == reply["cancel"],
            || format!("repeated cancel: {again}"),
        )?;
        sandbox.resume(&session, &handle, "third")?;
        sandbox.release("hold1")?;
        let third = sandbox.wait(&format!("{session}/3"))?;
        check(third["state"] == "completed", || {
            format!("successor: {third}")
        })?;
        let launched: i64 = sandbox.query(&format!(
            "SELECT count(*) FROM anchors WHERE owner_session='{session}' AND owner_turn=2"
        ))?;
        check(launched == 0, || {
            "the cancelled queued turn launched".to_owned()
        })?;
        daemon.finish()
    })
}

/// Design §2, §3.3, §11: the running turn gets one interrupt; its
/// `interrupted` terminal ends it `cancelled`, `acknowledged`, cleanup
/// `quiescent`; the running-turn reply is the acknowledgement (A8), a second
/// cancel coalesces with no second interrupt, and the terminal turn then
/// replies `already_terminal: true`.
#[test]
fn s1_cancel_running_turn_acknowledged() -> TestResult {
    evidenced(|| {
        let mut steps = vec![accepted(1)];
        let [expect, end] = interrupted(1);
        steps.extend([expect, gate("after_interrupt"), end]);
        let sandbox = Sandbox::new(&script("run", 1, steps))?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("run", &[])?;
        sandbox.await_row(
            &format!("SELECT state FROM turns WHERE session_id='{session}' AND number=1"),
            "running",
        )?;
        let started = sandbox.wait_for_event(&session, "turn.started")?;
        drop(started);
        let reply = sandbox.ok(&["cancel", &session, "--handle", &handle, "--json"])?;
        check(
            reply["turn"] == format!("{session}/1").as_str()
                && reply["state"] == "running"
                && reply["already_terminal"] == false
                && reply["cancel"]["outcome"] == "requested"
                && reply["cancel"]["cleanup"] == "pending"
                && reply["cancel"]["settled_at"].is_null(),
            || format!("running cancel reply: {reply}"),
        )?;
        sandbox.await_file("after_interrupt.entered")?;
        let second = sandbox.ok(&["cancel", &session, "--handle", &handle, "--json"])?;
        check(
            second["state"] == "running"
                && second["cancel"]["requested_at"] == reply["cancel"]["requested_at"],
            || format!("coalesced cancel: {second}"),
        )?;
        sandbox.release("after_interrupt")?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(
            envelope["state"] == "cancelled"
                && envelope["stop_reason"] == "interrupted"
                && envelope["failure"].is_null()
                && envelope["cancel"]["outcome"] == "acknowledged"
                && envelope["cancel"]["cleanup"] == "quiescent"
                && envelope["cancel"]["requested_at"] == reply["cancel"]["requested_at"],
            || format!("acknowledged envelope: {envelope}"),
        )?;
        let types = event_types(&sandbox.events(&session)?, 1);
        let requested = types
            .iter()
            .filter(|kind| *kind == "cancel.requested")
            .count();
        let settled = types
            .iter()
            .filter(|kind| *kind == "cancel.settled")
            .count();
        check(requested == 1 && settled == 1, || {
            format!("events: {types:?}")
        })?;
        // The fake read exactly one interrupt: a second one makes it report
        // "invalid typed interrupt request" on its stderr, `stderr.log`.
        let stderr = sandbox.evidence_file(&session, 1, "stderr.log")?;
        check(stderr.is_empty(), || {
            format!("the fake reported: {}", String::from_utf8_lossy(&stderr))
        })?;
        let cause: String = sandbox.query(&format!(
            "SELECT cancel_cause FROM turns WHERE session_id='{session}' AND number=1"
        ))?;
        check(cause == "cancel", || format!("cancel_cause {cause}"))?;
        let terminal_reply = sandbox.ok(&["cancel", &session, "--handle", &handle, "--json"])?;
        check(
            terminal_reply["already_terminal"] == true
                && terminal_reply["state"] == "cancelled"
                && terminal_reply["cancel"] == envelope["cancel"],
            || format!("terminal cancel reply: {terminal_reply}"),
        )?;
        daemon.finish()
    })
}

impl Sandbox {
    /// Waits for the session's first event of `kind`.
    fn wait_for_event(&self, session: &str, kind: &str) -> TestResult<Value> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(event) = self
                .events(session)?
                .into_iter()
                .find(|event| event["type"] == kind)
            {
                return Ok(event);
            }
            if Instant::now() >= deadline {
                return Err(format!("no {kind} event").into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Design §2 rule 3, §11: a vendor that ignores the interrupt is
/// force-closed at `force_after_ms`: `cancelled`, `forced`, `quiescent`, and
/// the agent and its grandchild are gone. `wait: true` replies with the
/// terminal.
#[test]
fn s1_cancel_running_turn_forced_after_grace() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "hang",
            1,
            vec![
                accepted(1),
                json!({"action":"spawn_grandchild","name":"gc"}),
                json!({"action":"report_pids"}),
                json!({"action":"hang"}),
            ],
        ))?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("hang", &[])?;
        let agent = sandbox.pid("agent.pid")?;
        let grandchild = sandbox.pid("gc.pid")?;
        sandbox.wait_for_event(&session, "turn.started")?;
        let reply = sandbox.ok(&[
            "cancel",
            &session,
            "--force-after",
            "300",
            "--wait",
            "--handle",
            &handle,
            "--json",
        ])?;
        check(
            reply["state"] == "cancelled"
                && reply["already_terminal"] == false
                && reply["cancel"]["outcome"] == "forced"
                && reply["cancel"]["cleanup"] == "quiescent",
            || format!("forced cancel reply: {reply}"),
        )?;
        check(!process_live(agent) && !process_live(grandchild), || {
            "the agent or its grandchild survived the forced cancel".to_owned()
        })?;
        daemon.finish()
    })
}

/// F20 (design §11): an agent that ignores SIGTERM is killed; its group is
/// gone within 3 s of `force_at`, with outcome `forced`.
#[test]
fn s1_f20_sigterm_ignored_escalates_to_kill() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "stubborn",
            1,
            vec![
                accepted(1),
                json!({"action":"report_pids"}),
                json!({"action":"ignore_term"}),
            ],
        ))?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("stubborn", &[])?;
        let agent = sandbox.pid("agent.pid")?;
        sandbox.await_file("ignore_term.entered")?;
        let requested = Instant::now();
        let reply = sandbox.ok(&[
            "cancel",
            &session,
            "--force-after",
            "200",
            "--wait",
            "--handle",
            &handle,
            "--json",
        ])?;
        let elapsed = requested.elapsed();
        check(
            reply["state"] == "cancelled" && reply["cancel"]["outcome"] == "forced",
            || format!("F20 reply: {reply}"),
        )?;
        check(!process_live(agent), || {
            "the SIGTERM-ignoring agent survived".to_owned()
        })?;
        check(elapsed < Duration::from_millis(200 + 3000 + 1000), || {
            format!("the group outlived force_at + 3 s: {elapsed:?}")
        })?;
        daemon.finish()
    })
}

// --------------------------------------------------------- idle deadline

/// F19 (design §5, §11 [r1.9, r1.10]): with no meaningful progress within
/// `idle_ms` the turn is ordered to stop with cause `idle_deadline`; the
/// vendor's `interrupted` ends it `failed(deadline_idle)`. Unknown messages and
/// stderr during the window do not reset the idle clock.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one idle-deadline scenario keeps its setup and checks together"
)]
fn s1_f19_idle_deadline_fails_turn_and_clears_group() -> TestResult {
    evidenced(|| {
        let noise = |name: &str| {
            [
                gate(name),
                json!({"action":"emit","message":{"type":"heartbeat"}}),
                json!({"action":"emit_raw","text":"stderr noise\n","stream":"stderr"}),
            ]
        };
        let mut steps = vec![accepted(1)];
        steps.extend(noise("noise1"));
        steps.extend(noise("noise2"));
        steps.extend(interrupted(1));
        let responsive = script("idle", 1, steps);
        // A vendor that never answers, with a grandchild: the idle order's
        // `force_at` is capped at the wall deadline, and the turn stays
        // `deadline_idle` [r1.9].
        let silent = script(
            "silent",
            1,
            vec![
                accepted(1),
                json!({"action":"spawn_grandchild","name":"gc"}),
                json!({"action":"report_pids"}),
                json!({"action":"hang"}),
            ],
        );
        let sandbox = Sandbox::new(&scripts(vec![responsive, silent]))?;
        let daemon = sandbox.start()?;
        // The durable timestamps are wall-clock; a clock step during the window
        // shows as a gap between these two elapsed times.
        let (wall_start, monotonic_start) = (SystemTime::now(), Instant::now());
        let (session, _) = sandbox.spawn("idle", &["--idle-ms", "2000", "--wall-ms", "20000"])?;
        sandbox.await_file("noise1.entered")?;
        // Elapsed time only: noise lands inside the idle window.
        thread::sleep(Duration::from_millis(800));
        sandbox.release("noise1")?;
        sandbox.await_file("noise2.entered")?;
        thread::sleep(Duration::from_millis(600));
        // Task 4 design §2.3, §2.6: an unknown message is no event; it moves
        // only the activity clock. Its arrival is seen in `status` before the
        // idle order, so the noise reached VIA without resetting idle.
        let before = sandbox.status(&session)?["progress"]["last_activity_at"].clone();
        sandbox.release("noise2")?;
        let noise_seen = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let progress = sandbox.status(&session)?["progress"].clone();
                let moved = progress["last_activity_at"].as_str() > before.as_str();
                if moved || Instant::now() >= deadline {
                    break moved;
                }
                thread::sleep(Duration::from_millis(10));
            }
        };
        // A monotonic observation of the idle order itself, on the clock that
        // started before the spawn request, so at or before the idle origin.
        sandbox.wait_for_event(&session, "cancel.requested")?;
        let ordered_seen = monotonic_start.elapsed();
        let monotonic = ordered_seen;
        let wall = SystemTime::now()
            .duration_since(wall_start)
            .unwrap_or_default();
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(
            envelope["state"] == "failed"
                && envelope["failure"]["class"] == "deadline_idle"
                && envelope["stop_reason"] == "deadline"
                && envelope["cancel"]["outcome"] == "acknowledged"
                && envelope["cancel"]["cleanup"] == "quiescent",
            || format!("idle envelope: {envelope}"),
        )?;
        let events = sandbox.events(&session)?;
        let at = |kind: &str| {
            events
                .iter()
                .find(|event| event["type"] == kind)
                .map(|event| event["at"].clone())
                .ok_or_else(|| format!("no {kind} event"))
        };
        let idle_after = epoch_ms(&at("cancel.requested")?)? - epoch_ms(&at("turn.started")?)?;
        // A wall-clock step (seen under WSL2 load) moves the durable timestamps
        // but not the monotonic clock: the idle order is then bounded on the
        // harness's one monotonic clock, from before the spawn request (at or
        // before the idle origin, the submission clock) to the first read of
        // `cancel.requested` (10 ms polling) [s2-r1.2, s2-r2.4].
        let stepped = wall.abs_diff(monotonic) > Duration::from_millis(250);
        let timely = if stepped {
            (Duration::from_millis(1950)..Duration::from_millis(3000)).contains(&ordered_seen)
        } else {
            (1950..2800).contains(&idle_after)
        };
        check(timely, || {
            format!(
                "the idle stop came {idle_after} ms after acceptance (harness: order seen \
             {ordered_seen:?} after the spawn request; elapsed wall {wall:?}, monotonic \
             {monotonic:?}; events {events:?})"
            )
        })?;
        check(noise_seen && before.is_string(), || {
            "the unknown messages did not reach the activity clock".to_owned()
        })?;

        let (silent_session, _) =
            sandbox.spawn("silent", &["--idle-ms", "1000", "--wall-ms", "2500"])?;
        let agent = sandbox.pid("agent.pid")?;
        let grandchild = sandbox.pid("gc.pid")?;
        let envelope = sandbox.wait(&format!("{silent_session}/1"))?;
        check(
            envelope["state"] == "failed"
                && envelope["failure"]["class"] == "deadline_idle"
                && envelope["cancel"]["cleanup"] == "quiescent",
            || format!("capped idle envelope: {envelope}"),
        )?;
        // The setsid limit: a grandchild that leaves the group is out of reach;
        // this one stays in it and is gone with the group.
        check(!process_live(agent) && !process_live(grandchild), || {
            "the idle-stopped group survived".to_owned()
        })?;
        daemon.finish()
    })
}

/// F19 wall variant (characterization: the wall deadline already
/// force-closed the group before S2): `deadline_wall`, the grandchild gone.
#[test]
fn s1_f19_wall_deadline_clears_grandchild() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "wall",
            1,
            vec![
                accepted(1),
                json!({"action":"spawn_grandchild","name":"gc"}),
                json!({"action":"report_pids"}),
                json!({"action":"hang"}),
            ],
        ))?;
        let daemon = sandbox.start()?;
        let (session, _) = sandbox.spawn("wall", &["--wall-ms", "1000"])?;
        let grandchild = sandbox.pid("gc.pid")?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(
            envelope["state"] == "failed"
                && envelope["failure"]["class"] == "deadline_wall"
                && envelope["cancel"]["cleanup"] == "quiescent",
            || format!("wall envelope: {envelope}"),
        )?;
        check(!process_live(grandchild), || {
            "the grandchild survived".to_owned()
        })?;
        daemon.finish()
    })
}

/// F21 (design §2; characterization of S1's Route mapping end to end): an
/// exit after an unterminated line is `failed(process_exited)`, and the
/// partial bytes are the turn's `undecoded.bin` (Task 4 design §7.3).
#[test]
fn s1_f21_crash_mid_line_is_process_exited() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "crash",
            1,
            vec![
                accepted(1),
                json!({"action":"emit_raw","text":"{\"type\":\"text\",\"partial-f21"}),
                json!({"action":"exit","code":1}),
            ],
        ))?;
        let daemon = sandbox.start()?;
        let (session, _) = sandbox.spawn("crash", &[])?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(
            envelope["state"] == "failed" && envelope["failure"]["class"] == "process_exited",
            || format!("F21 envelope: {envelope}"),
        )?;
        let saved = sandbox.evidence_file(&session, 1, "undecoded.bin")?;
        check(saved == br#"{"type":"text","partial-f21"#, || {
            format!("undecoded.bin holds {}", String::from_utf8_lossy(&saved))
        })?;
        daemon.finish()
    })
}

/// Design §5: `idle_ms` is frozen at acceptance, inherited like `wall_ms`,
/// defaults to 600 000, and 0 is `invalid_params`.
#[test]
fn s1_idle_ms_is_frozen_inherited_and_positive() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(vec![completes("a", 1), completes("b", 2)]))?;
        let daemon = sandbox.start()?;
        sandbox.refused(
            &[
                "spawn",
                "--harness",
                "fake",
                "--model",
                "fake",
                "--prompt",
                "a",
                "--idle-ms",
                "0",
                "--json",
            ],
            "invalid_params",
        )?;
        let (session, handle) = sandbox.spawn("a", &["--idle-ms", "4321"])?;
        let first = sandbox.wait(&format!("{session}/1"))?;
        check(first["state"] == "completed", || format!("{first}"))?;
        let receipt = sandbox.resume(&session, &handle, "b")?;
        check(receipt["effective"]["deadlines"]["idle_ms"] == 4321, || {
            format!("inherited idle_ms: {receipt}")
        })?;
        sandbox.wait(&format!("{session}/2"))?;
        let (other, _) = sandbox.spawn("a", &[])?;
        let default = sandbox.wait(&format!("{other}/1"))?;
        drop(default);
        let effective: String = sandbox.query(&format!(
            "SELECT effective FROM turns WHERE session_id='{other}' AND number=1"
        ))?;
        let effective: Value = serde_json::from_str(&effective)?;
        check(effective["deadlines"]["idle_ms"] == 600_000, || {
            format!("default idle_ms: {effective}")
        })?;
        daemon.finish()
    })
}

// ----------------------------------------------------------------- close

/// Design §4, §11: `close` makes the session `closing` (`resume` is
/// `session_closed` then and after), cancels the queue and the running turn
/// with `cancel_cause = 'close'`, and commits `session.closed {reason:
/// close}` with the derived `cancelled_turns` and `cleanup`.
#[test]
fn s1_close_cancels_queue_and_running_turn() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "first",
            1,
            vec![accepted(1), json!({"action":"hang"})],
        ))?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("first", &[])?;
        sandbox.wait_for_event(&session, "turn.started")?;
        sandbox.resume(&session, &handle, "second")?;
        sandbox.resume(&session, &handle, "third")?;
        let result = thread::scope(|scope| -> TestResult<Value> {
            let close = scope.spawn(|| {
                sandbox
                    .ok(&[
                        "close",
                        &session,
                        "--deadline-ms",
                        "4000",
                        "--handle",
                        &handle,
                        "--json",
                    ])
                    .map_err(|error| error.to_string())
            });
            sandbox.await_row(
                &format!("SELECT admission FROM sessions WHERE id='{session}'"),
                "closing",
            )?;
            sandbox.refused(
                &[
                    "resume", &session, "--prompt", "late", "--handle", &handle, "--json",
                ],
                "session_closed",
            )?;
            Ok(close.join().map_err(|_| "close panicked")??)
        })?;
        let expected: Vec<String> = (1..=3).map(|turn| format!("{session}/{turn}")).collect();
        check(
            result["session_id"] == session.as_str()
                && result["state"] == "closed"
                && result["cancelled_turns"] == json!(expected)
                && result["cleanup"] == "quiescent",
            || format!("close result: {result}"),
        )?;
        let first = sandbox.ok(&["result", &format!("{session}/1"), "--json"])?;
        check(
            first["state"] == "cancelled" && first["cancel"]["outcome"] == "forced",
            || format!("closed running turn: {first}"),
        )?;
        for turn in 2..=3 {
            let queued = sandbox.ok(&["result", &format!("{session}/{turn}"), "--json"])?;
            check(
                queued["state"] == "cancelled"
                    && queued["cancel"]["outcome"] == "acknowledged"
                    && queued["timestamps"]["submitted_at"].is_null(),
                || format!("closed queued turn: {queued}"),
            )?;
        }
        let causes: i64 = sandbox.query(&format!(
            "SELECT count(*) FROM turns WHERE session_id='{session}' AND cancel_cause='close'"
        ))?;
        check(causes == 3, || {
            format!("{causes} turns carry cancel_cause close")
        })?;
        let events = sandbox.events(&session)?;
        let closed = events.last().cloned().unwrap_or_default();
        check(
            closed["type"] == "session.closed" && closed["reason"] == "close",
            || format!("last event: {closed}"),
        )?;
        sandbox.refused(
            &[
                "resume", &session, "--prompt", "late", "--handle", &handle, "--json",
            ],
            "session_closed",
        )?;
        // A close of the closed session replies with the stored result.
        let again = sandbox.ok(&["close", &session, "--handle", &handle, "--json"])?;
        check(again == result, || format!("repeated close: {again}"))?;
        daemon.finish()
    })
}

/// Design §4 step 2 [r1.5]: an `op_key` replay returns the same result, and
/// other params under the key are `idempotency_conflict`.
#[test]
fn s1_close_keyed_retry_and_second_close() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&completes("done", 1))?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("done", &[])?;
        sandbox.wait(&format!("{session}/1"))?;
        let keyed = [
            "close", &session, "--op-key", "k1", "--handle", &handle, "--json",
        ];
        let first = sandbox.ok(&keyed)?;
        check(
            first["state"] == "closed"
                && first["cancelled_turns"] == json!([])
                && first["cleanup"] == "quiescent",
            || format!("idle close: {first}"),
        )?;
        let replay = sandbox.ok(&keyed)?;
        check(replay == first, || format!("keyed replay: {replay}"))?;
        sandbox.refused(
            &[
                "close", &session, "--op-key", "k1", "--mode", "force", "--handle", &handle,
                "--json",
            ],
            "invalid_params",
        )?;
        daemon.finish()
    })
}

/// Design §4 "Force" [r4.6]: `daemon stop --force` during a close closes the
/// session `daemon_stop_force`, the close waiter gets `daemon_stopping`, and
/// the exit is 0 with positive cleanup.
#[test]
fn s1_close_racing_force_stop() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "hang",
            1,
            vec![accepted(1), json!({"action":"hang"})],
        ))?;
        let mut daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("hang", &[])?;
        sandbox.wait_for_event(&session, "turn.started")?;
        thread::scope(|scope| -> TestResult {
            let close = scope.spawn(|| {
                sandbox
                    .refused(
                        &[
                            "close",
                            &session,
                            "--deadline-ms",
                            "20000",
                            "--handle",
                            &handle,
                            "--json",
                        ],
                        "daemon_stopping",
                    )
                    .map_err(|error| error.to_string())
            });
            sandbox.await_row(
                &format!("SELECT admission FROM sessions WHERE id='{session}'"),
                "closing",
            )?;
            sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
            close.join().map_err(|_| "close panicked")??;
            Ok(())
        })?;
        let status = daemon.exit(Duration::from_secs(15))?;
        check(status.success(), || {
            format!("force exit {status}: {}", sandbox.trace())
        })?;
        let reason: String = sandbox.query(&format!(
            "SELECT json_extract(event,'$.reason') FROM events WHERE session_id='{session}' \
         AND json_extract(event,'$.type')='session.closed'"
        ))?;
        check(reason == "daemon_stop_force", || {
            format!("closed by {reason}")
        })?;
        daemon.verify()
    })
}

// ------------------------------------------------------ failpoint seams

#[cfg(feature = "test-failpoints")]
impl Sandbox {
    fn arm(&self, point: &str, occurrence: u64, action: &str) -> TestResult {
        Ok(self.failpoints.arm(point, occurrence, action)?)
    }

    fn ack(&self, daemon: &Daemon<'_>, point: &str, occurrence: u64, action: &str) -> TestResult {
        self.failpoints.wait_ack(
            point,
            occurrence,
            action,
            daemon.pid(),
            Duration::from_secs(20),
        )?;
        Ok(())
    }

    /// Waits for the acknowledgement of a process other than `Daemon` (an
    /// anchor, or a daemon the harness did not start): its pid is read from
    /// the acknowledgement, which is then checked in full.
    fn process_ack(&self, point: &str, occurrence: u64, action: &str) -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(20);
        let bytes = loop {
            if let Ok(bytes) = self.failpoints.ack_bytes(point, occurrence) {
                break bytes;
            }
            if Instant::now() >= deadline {
                return Err(format!("no acknowledgement of {point} #{occurrence}").into());
            }
            thread::sleep(Duration::from_millis(5));
        };
        let ack: Value = serde_json::from_slice(&bytes)?;
        let pid = ack["pid"].as_u64().ok_or("acknowledgement has no pid")?;
        self.failpoints.wait_ack(
            point,
            occurrence,
            action,
            u32::try_from(pid)?,
            Duration::from_secs(1),
        )?;
        Ok(())
    }

    fn resume_point(&self, point: &str, occurrence: u64) -> TestResult {
        Ok(self.failpoints.release(point, occurrence)?)
    }

    /// Whether `point` acknowledged `occurrence`.
    fn acked(&self, point: &str, occurrence: u64) -> bool {
        self.failpoints.ack_bytes(point, occurrence).is_ok()
    }

    fn disarm(&self, point: &str) -> TestResult {
        Ok(self.failpoints.disarm(point)?)
    }

    /// Starts counting `point` (see `support/hits.rs`).
    fn count(&self, point: &str) -> TestResult {
        Ok(hits::count(&self.root.path().join("failpoints"), point)?)
    }

    /// The next occurrence of a counted `point`.
    fn next_hit(&self, point: &str) -> TestResult<u64> {
        Ok(hits::hits(&self.root.path().join("failpoints"), point)? + 1)
    }
}

/// Design §3.1 [r1.1], §11: a close set while the dispatcher holds a claimed
/// turn at `core.dispatch.before_grant` makes the claim step roll back, and
/// the close pass cancels the turn with no submission; a close set while
/// `core.submit.before_commit` holds its submission is attached to the
/// claim, so the turn starts with the order set and launches nothing. Both
/// end `cancelled` with `cancel_cause = 'close'`. The close caller's
/// `core.close.before_subscribe` acknowledgement witnesses its order.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_reaches_claimed_turn() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(vec![
            completes("claimed", 1),
            completes("submitting", 1),
        ]))?;
        // Both turns are closed before their launch.
        sandbox.no_launch();
        let daemon = sandbox.start()?;
        for (occurrence, point, prompt) in [
            (1, "core.dispatch.before_grant", "claimed"),
            (2, "core.submit.before_commit", "submitting"),
        ] {
            sandbox.arm(point, 1, "pause")?;
            sandbox.arm("core.close.before_subscribe", occurrence, "fail_io")?;
            let (session, handle) = sandbox.spawn(prompt, &[])?;
            sandbox.ack(&daemon, point, 1, "pause")?;
            let result = thread::scope(|scope| -> TestResult<Value> {
                let close = scope.spawn(|| {
                    sandbox
                        .ok(&["close", &session, "--handle", &handle, "--json"])
                        .map_err(|error| error.to_string())
                });
                sandbox.ack(
                    &daemon,
                    "core.close.before_subscribe",
                    occurrence,
                    "fail_io",
                )?;
                sandbox.resume_point(point, 1)?;
                Ok(close.join().map_err(|_| "close panicked")??)
            })?;
            check(
                result["cancelled_turns"] == json!([format!("{session}/1")])
                    && result["cleanup"] == "quiescent",
                || format!("{point}: close result {result}"),
            )?;
            let envelope = sandbox.ok(&["result", &format!("{session}/1"), "--json"])?;
            let submitted = !envelope["timestamps"]["submitted_at"].is_null();
            check(
                envelope["state"] == "cancelled"
                    && envelope["stop_reason"] == "interrupted"
                    && envelope["cancel"]["cleanup"] == "quiescent"
                    && submitted == (point == "core.submit.before_commit"),
                || format!("{point}: envelope {envelope}"),
            )?;
            let cause: String = sandbox.query(&format!(
                "SELECT cancel_cause FROM turns WHERE session_id='{session}' AND number=1"
            ))?;
            let launched: i64 = sandbox.query(&format!(
                "SELECT count(*) FROM anchors WHERE owner_session='{session}'"
            ))?;
            check(cause == "close" && launched == 0, || {
                format!("{point}: cause {cause}, {launched} anchors")
            })?;
            sandbox.disarm(point)?;
        }
        daemon.finish()
    })
}

/// Design §3.1 [r1.2], §11: a turn waiting at `core.dispatch.awaiting_slot`
/// stays `Waiting`, so a cancel takes it: `cancelled` without submission; no
/// permit is consumed, and the next turn launches once the slot frees.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_while_waiting_for_slot() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new(&scripts(vec![
            script(
                "holder",
                1,
                vec![
                    accepted(1),
                    gate("holder"),
                    terminal(1, "completed", "end_turn"),
                ],
            ),
            completes("later", 1),
        ]))?;
        sandbox
            .env
            .push(("VIA_TEST_CONNECTION_SLOTS", "1".to_owned()));
        let daemon = sandbox.start()?;
        let (holder, _) = sandbox.spawn("holder", &[])?;
        sandbox.await_file("holder.entered")?;
        sandbox.arm("core.dispatch.awaiting_slot", 1, "pause")?;
        let (waiting, handle) = sandbox.spawn("waiting", &[])?;
        sandbox.ack(&daemon, "core.dispatch.awaiting_slot", 1, "pause")?;
        let reply = sandbox.ok(&["cancel", &waiting, "--handle", &handle, "--json"])?;
        check(
            reply["state"] == "cancelled"
                && reply["already_terminal"] == false
                && reply["cancel"]["outcome"] == "acknowledged",
            || format!("waiting-for-slot cancel: {reply}"),
        )?;
        sandbox.resume_point("core.dispatch.awaiting_slot", 1)?;
        let envelope = sandbox.ok(&["result", &format!("{waiting}/1"), "--json"])?;
        check(envelope["timestamps"]["submitted_at"].is_null(), || {
            format!("a waiting turn was submitted: {envelope}")
        })?;
        sandbox.release("holder")?;
        sandbox.wait(&format!("{holder}/1"))?;
        let (later, _) = sandbox.spawn("later", &[])?;
        let later = sandbox.wait(&format!("{later}/1"))?;
        check(later["state"] == "completed", || {
            format!("later turn: {later}")
        })?;
        daemon.finish()
    })
}

/// Design §4, §11 (adapted): a turn waiting for the only connection slot,
/// held by another session's live turn, is cancelled by `close`; the close
/// completes and the dispatcher exits while the slot stays held.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_cancels_turn_waiting_for_slot() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new(&script(
            "holder",
            1,
            vec![
                accepted(1),
                gate("holder"),
                terminal(1, "completed", "end_turn"),
            ],
        ))?;
        sandbox
            .env
            .push(("VIA_TEST_CONNECTION_SLOTS", "1".to_owned()));
        let daemon = sandbox.start()?;
        let (holder, _) = sandbox.spawn("holder", &[])?;
        sandbox.await_file("holder.entered")?;
        sandbox.arm("core.dispatch.awaiting_slot", 1, "fail_io")?;
        let (waiting, handle) = sandbox.spawn("waiting", &[])?;
        // `fail_io` acknowledges the registered wait and gives it up once;
        // the dispatcher decides again and waits again.
        sandbox.ack(&daemon, "core.dispatch.awaiting_slot", 1, "fail_io")?;
        let result = sandbox.ok(&["close", &waiting, "--handle", &handle, "--json"])?;
        check(
            result["cancelled_turns"] == json!([format!("{waiting}/1")])
                && result["cleanup"] == "quiescent",
            || format!("close of a waiting turn: {result}"),
        )?;
        let status = sandbox.ok(&["daemon", "status", "--json"])?;
        check(status["sessions"]["active"] == 1, || {
            format!("only the holder is active: {status}")
        })?;
        sandbox.release("holder")?;
        sandbox.wait(&format!("{holder}/1"))?;
        daemon.finish()
    })
}

/// Design §3.3 [r1.4], §11: a cancel while the run loop is paused at
/// `core.run.settling` sends no order (`core.cancel.settling` witnesses the
/// step); its reply comes after the drop,
/// `already_terminal: true` with the envelope's `cancel` (none here).
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_during_settlement_completes() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&completes("fast", 1))?;
        let daemon = sandbox.start()?;
        sandbox.arm("core.run.settling", 1, "pause")?;
        let (session, handle) = sandbox.spawn("fast", &[])?;
        sandbox.ack(&daemon, "core.run.settling", 1, "pause")?;
        sandbox.arm("core.cancel.settling", 1, "fail_io")?;
        let reply = thread::scope(|scope| -> TestResult<Value> {
            let cancel = scope.spawn(|| {
                sandbox
                    .ok(&["cancel", &session, "--handle", &handle, "--json"])
                    .map_err(|error| error.to_string())
            });
            // The cancel found the turn settling and sent no order.
            sandbox.ack(&daemon, "core.cancel.settling", 1, "fail_io")?;
            check(!cancel.is_finished(), || {
                "the cancel replied before the settlement ended".to_owned()
            })?;
            sandbox.resume_point("core.run.settling", 1)?;
            Ok(cancel.join().map_err(|_| "cancel panicked")??)
        })?;
        check(
            reply["state"] == "completed"
                && reply["already_terminal"] == true
                && reply["cancel"].is_null(),
            || format!("settling cancel reply: {reply}"),
        )?;
        let types = event_types(&sandbox.events(&session)?, 1);
        check(
            !types.iter().any(|kind| kind.starts_with("cancel.")),
            || format!("a settling turn got an order: {types:?}"),
        )?;
        daemon.finish()
    })
}

/// Design §2, §11: the anchor's `Stop` reply is lost
/// (`host.anchor.final_reply_lost`), so no stop evidence proves the vendor
/// was live: the turn is `unknown` (`stop_reason: error`) with outcome
/// `requested`, and its cleanup is decided independently.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_lost_stop_reply_is_unknown() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "hang",
            1,
            vec![accepted(1), json!({"action":"hang"})],
        ))?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("hang", &[])?;
        sandbox.wait_for_event(&session, "turn.started")?;
        sandbox.arm("host.anchor.final_reply_lost", 1, "fail_io")?;
        let reply = sandbox.ok(&[
            "cancel",
            &session,
            "--force-after",
            "200",
            "--wait",
            "--handle",
            &handle,
            "--json",
        ])?;
        let cleanup = reply["cancel"]["cleanup"].as_str().unwrap_or_default();
        check(
            reply["state"] == "unknown"
                && reply["cancel"]["outcome"] == "requested"
                && matches!(cleanup, "quiescent" | "uncertain"),
            || format!("lost stop reply: {reply}"),
        )?;
        let envelope = sandbox.ok(&["result", &format!("{session}/1"), "--json"])?;
        check(envelope["stop_reason"] == "error", || format!("{envelope}"))?;
        daemon.finish()
    })
}

/// Design §2 rule 1 [r1.8], §11: an order that reaches Host's pre-ARM gate
/// launches nothing: `cancelled`, `requested`, with the acquisition's own
/// absence evidence. The anchor held at `host.anchor.before_eof_cleanup`
/// leaves it unproven, so cleanup is `uncertain`; released, the same case
/// is `quiescent`.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_before_launch_slow_anchor_is_uncertain() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(vec![
            script(
                "slow",
                1,
                vec![json!({"action":"report_pids"}), accepted(1)],
            ),
            script(
                "prompt",
                1,
                vec![json!({"action":"report_pids"}), accepted(1)],
            ),
        ]))?;
        let daemon = sandbox.start()?;
        for (occurrence, prompt, held) in [(1, "slow", true), (2, "prompt", false)] {
            sandbox.arm("host.anchor.after_arm_intent_commit", occurrence, "pause")?;
            if held {
                sandbox.arm("host.anchor.before_eof_cleanup", 1, "pause")?;
            }
            let (session, handle) = sandbox.spawn(prompt, &[])?;
            sandbox.ack(
                &daemon,
                "host.anchor.after_arm_intent_commit",
                occurrence,
                "pause",
            )?;
            let reply = sandbox.ok(&["cancel", &session, "--handle", &handle, "--json"])?;
            check(
                reply["state"] == "running" && reply["cancel"]["outcome"] == "requested",
                || format!("{prompt}: acknowledgement {reply}"),
            )?;
            sandbox.resume_point("host.anchor.after_arm_intent_commit", occurrence)?;
            let envelope = sandbox.wait(&format!("{session}/1"))?;
            let expected = if held { "uncertain" } else { "quiescent" };
            check(
                envelope["state"] == "cancelled"
                    && envelope["stop_reason"] == "interrupted"
                    && envelope["cancel"]["outcome"] == "requested"
                    && envelope["cancel"]["cleanup"] == expected,
                || format!("{prompt}: envelope {envelope}"),
            )?;
            if held {
                sandbox.process_ack("host.anchor.before_eof_cleanup", 1, "pause")?;
                sandbox.resume_point("host.anchor.before_eof_cleanup", 1)?;
                sandbox.disarm("host.anchor.before_eof_cleanup")?;
            }
        }
        check(!sandbox.sync.join("agent.pid").exists(), || {
            "a vendor launched past the gate".to_owned()
        })?;
        daemon.finish()
    })
}

/// Design §3.2 [r1.13]: a queued cancellation whose read fails
/// (`store.read.queued_turn`) rolls back, writes nothing and replies a
/// plain `store_error`; a retried cancel succeeds.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_queued_read_failure_is_plain_store_error() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "first",
            1,
            vec![
                accepted(1),
                gate("hold"),
                terminal(1, "completed", "end_turn"),
            ],
        ))?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("first", &[])?;
        sandbox.await_file("hold.entered")?;
        sandbox.resume(&session, &handle, "second")?;
        // Occurrence 1 was turn 1's submission read.
        sandbox.arm("store.read.queued_turn", 2, "fail_io")?;
        let cancel = [
            "cancel", &session, "--turn", "2", "--handle", &handle, "--json",
        ];
        let error = sandbox.refused(&cancel, "store_error")?;
        sandbox.ack(&daemon, "store.read.queued_turn", 2, "fail_io")?;
        let data = &error["data"];
        check(
            data["session"].is_null()
                && data["turn"].is_null()
                && data["durable_state"].is_null()
                && data["terminal_persisted"].is_null(),
            || format!("the read failure is not plain: {error}"),
        )?;
        let state: String = sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{session}' AND number=2"
        ))?;
        check(state == "queued", || {
            format!("the failed cancel wrote {state}")
        })?;
        let reply = sandbox.ok(&cancel)?;
        check(reply["state"] == "cancelled", || {
            format!("retried cancel: {reply}")
        })?;
        sandbox.release("hold")?;
        daemon.finish()
    })
}

/// Design §4 [r1.6], §11: the daemon crashes after the close's first queued
/// cancellation (`store.commit.cancel`), and the restart crashes again at the
/// next; the final `cancelled_turns` lists every turn the close cancelled,
/// identical across restarts.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_partial_restarts_keep_one_result() -> TestResult {
    evidenced(|| {
        let mut steps = vec![accepted(1)];
        steps.extend(interrupted(1));
        let sandbox = Sandbox::new(&script("first", 1, steps))?;
        let mut daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("first", &[])?;
        sandbox.wait_for_event(&session, "turn.started")?;
        sandbox.resume(&session, &handle, "second")?;
        sandbox.resume(&session, &handle, "third")?;
        // Turn 2's cancellation commits; turn 3's crashes the daemon.
        sandbox.arm("store.commit.cancel", 2, "crash")?;
        let crashed = sandbox.run(&["close", &session, "--handle", &handle, "--json"])?;
        check(!crashed.status.success(), || {
            "the close survived the crash".to_owned()
        })?;
        sandbox.ack(&daemon, "store.commit.cancel", 2, "crash")?;
        daemon.exit(Duration::from_secs(15))?;
        drop(daemon);
        let committed: i64 = sandbox.query(&format!(
            "SELECT count(*) FROM turns WHERE session_id='{session}' AND cancel_cause='close'"
        ))?;
        check(committed == 2, || {
            format!("{committed} close cancellations before the crash")
        })?;
        // The restart handoff crashes at turn 3's cancellation.
        sandbox.arm("store.commit.cancel", 1, "crash")?;
        let mut failed = sandbox.command();
        sandbox.failpoints.activate(&mut failed);
        let restart = run_command(failed.arg("daemon"), Duration::from_secs(30))?;
        check(!restart.status.success(), || {
            "the restart survived the crash".to_owned()
        })?;
        sandbox.process_ack("store.commit.cancel", 1, "crash")?;
        sandbox.disarm("store.commit.cancel")?;
        let daemon = sandbox.start()?;
        let result = sandbox.ok(&["close", &session, "--handle", &handle, "--json"])?;
        let expected: Vec<String> = (1..=3).map(|turn| format!("{session}/{turn}")).collect();
        check(
            result["state"] == "closed" && result["cancelled_turns"] == json!(expected),
            || format!("close result after restarts: {result}"),
        )?;
        daemon.finish()?;
        let daemon = sandbox.start()?;
        let again = sandbox.ok(&["close", &session, "--handle", &handle, "--json"])?;
        check(again == result, || {
            format!("the result changed across restarts: {again}")
        })?;
        daemon.finish()
    })
}

/// Design §4 "Force" [r4.6, r5.8, r6.6], latch variant: a keyed replay of
/// a close in progress reaches the close watch and is paused at
/// `core.close.before_subscribe`, holding `admission`. Another session's
/// terminal commit and its one same-sequence retry fail (persistent
/// `store.commit.terminal`, an escalation; re-pointed in S5, since one
/// failure is now retried and scoped), and the latch's first phase (which
/// needs no `admission`) forces the closing session's running turn; the
/// dispatcher's latch exit publishes `store_error` in that window.
/// The first caller receives it, and the paused replay, released after the
/// publication, still receives it from the retained outcome. (The design's
/// force variant needs a caller entering after force is accepted; the S2
/// server stops accepting connections at force, so that variant is S3's.)
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_outcome_retained_for_late_subscriber() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(vec![
            script("hang", 1, vec![accepted(1), json!({"action":"hang"})]),
            script(
                "other",
                1,
                vec![
                    accepted(1),
                    gate("other"),
                    terminal(1, "completed", "end_turn"),
                ],
            ),
        ]))?;
        let mut daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("hang", &[])?;
        sandbox.wait_for_event(&session, "turn.started")?;
        sandbox.spawn("other", &[])?;
        sandbox.await_file("other.entered")?;
        let keyed = [
            "close",
            &session,
            "--op-key",
            "k1",
            "--deadline-ms",
            "60000",
            "--handle",
            &handle,
            "--json",
        ];
        thread::scope(|scope| -> TestResult {
            let first = scope.spawn(|| {
                sandbox
                    .refused(&keyed, "store_error")
                    .map_err(|error| error.to_string())
            });
            // The running turn observed the close order.
            sandbox.wait_for_event(&session, "cancel.requested")?;
            // The first caller passed the seam unarmed: this is its second hit.
            sandbox.arm("core.close.before_subscribe", 2, "pause")?;
            let replay = scope.spawn(|| {
                sandbox
                    .refused(&keyed, "store_error")
                    .map_err(|error| error.to_string())
            });
            sandbox.ack(&daemon, "core.close.before_subscribe", 2, "pause")?;
            // No terminal committed yet: the other session's is the first, and
            // its retry the second.
            sandbox.arm("store.commit.terminal", 1, "fail_io_persist")?;
            sandbox.release("other")?;
            sandbox.ack(&daemon, "store.commit.terminal", 2, "fail_io")?;
            // The latch exit published: the first caller has its outcome.
            first.join().map_err(|_| "first close panicked")??;
            check(!replay.is_finished(), || {
                "the paused replay replied before its release".to_owned()
            })?;
            sandbox.resume_point("core.close.before_subscribe", 2)?;
            replay.join().map_err(|_| "replay panicked")??;
            Ok(())
        })?;
        let status = daemon.exit(Duration::from_secs(15))?;
        check(status.code() == Some(4), || {
            format!("latched exit {status}: {}", sandbox.trace())
        })?;
        Ok(())
    })
}

/// Design §5 deadline origin [r1.11]: the wall deadline runs from the
/// submission clock, taken before `core.submit.before_commit`. A submission
/// held there past a short `wall_ms` gets no extra time: `deadline_wall`,
/// and nothing launches.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f19_delayed_submission_gets_no_extra_wall_time() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "late",
            1,
            vec![
                json!({"action":"report_pids"}),
                accepted(1),
                terminal(1, "completed", "end_turn"),
            ],
        ))?;
        let daemon = sandbox.start()?;
        sandbox.arm("core.submit.before_commit", 1, "pause")?;
        let (session, _) = sandbox.spawn("late", &["--wall-ms", "300"])?;
        sandbox.ack(&daemon, "core.submit.before_commit", 1, "pause")?;
        // Elapsed time only: the submission is held past its wall deadline.
        thread::sleep(Duration::from_millis(600));
        sandbox.resume_point("core.submit.before_commit", 1)?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(
            envelope["state"] == "failed"
                && envelope["failure"]["class"] == "deadline_wall"
                && envelope["timestamps"]["accepted_at"].is_null(),
            || format!("delayed submission: {envelope}"),
        )?;
        check(!sandbox.sync.join("agent.pid").exists(), || {
            "a vendor launched after the wall deadline".to_owned()
        })?;
        daemon.finish()
    })
}

/// Design §2 [r1.23]: the vendor acknowledges the interrupt (`interrupted`),
/// then holds its stdout open past the wall deadline. The acknowledgement
/// stands: `cancelled`, `acknowledged`, not `deadline_wall`.
#[test]
fn s1_cancel_acknowledged_survives_wall_expiry() -> TestResult {
    evidenced(|| {
        let mut steps = vec![accepted(1)];
        steps.extend(interrupted(1));
        steps.push(json!({"action":"hang"}));
        let sandbox = Sandbox::new(&script("linger", 1, steps))?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("linger", &["--wall-ms", "1500"])?;
        sandbox.wait_for_event(&session, "turn.started")?;
        sandbox.ok(&["cancel", &session, "--handle", &handle, "--json"])?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(
            envelope["state"] == "cancelled"
                && envelope["stop_reason"] == "interrupted"
                && envelope["failure"].is_null()
                && envelope["cancel"]["outcome"] == "acknowledged",
            || format!("acknowledged past the wall deadline: {envelope}"),
        )?;
        daemon.finish()
    })
}

/// Design §4 dispatcher step 5 [r1.8]: the session's group is left unproven
/// with its anchor held at `host.anchor.before_eof_cleanup` (the close's
/// order reaches the turn at Host's pre-ARM gate, and the anchor's EOF
/// cleanup is slow); the close's absence check reaches its bound and the
/// close cleanup is `uncertain`. Released within the close deadline, the
/// same case is `quiescent`. (The design names a transport-loss terminal;
/// the fake's transport loss does not reach the EOF seam, and the pre-ARM
/// cancellation is the fixture that does.)
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_cleanup_uncertain_with_unproven_group() -> TestResult {
    evidenced(|| {
        let unlaunched = |prompt: &str| script(prompt, 1, vec![json!({"action":"report_pids"})]);
        let sandbox = Sandbox::new(&scripts(vec![unlaunched("held"), unlaunched("released")]))?;
        let daemon = sandbox.start()?;
        for (occurrence, prompt, deadline) in [(1, "held", "1500"), (2, "released", "20000")] {
            let held = occurrence == 1;
            sandbox.arm("host.anchor.after_arm_intent_commit", occurrence, "pause")?;
            sandbox.arm("host.anchor.before_eof_cleanup", 1, "pause")?;
            sandbox.arm("core.close.before_subscribe", occurrence, "fail_io")?;
            let (session, handle) = sandbox.spawn(prompt, &[])?;
            sandbox.ack(
                &daemon,
                "host.anchor.after_arm_intent_commit",
                occurrence,
                "pause",
            )?;
            let result = thread::scope(|scope| -> TestResult<Value> {
                let close = scope.spawn(|| {
                    sandbox
                        .ok(&[
                            "close",
                            &session,
                            "--deadline-ms",
                            deadline,
                            "--handle",
                            &handle,
                            "--json",
                        ])
                        .map_err(|error| error.to_string())
                });
                // The close order is set before the turn reaches the gate.
                sandbox.ack(
                    &daemon,
                    "core.close.before_subscribe",
                    occurrence,
                    "fail_io",
                )?;
                sandbox.resume_point("host.anchor.after_arm_intent_commit", occurrence)?;
                sandbox.process_ack("host.anchor.before_eof_cleanup", 1, "pause")?;
                if !held {
                    // The turn's terminal is committed; the close pass follows.
                    let envelope = sandbox.wait(&format!("{session}/1"))?;
                    check(envelope["cancel"]["cleanup"] == "uncertain", || {
                        format!("the turn's own cleanup: {envelope}")
                    })?;
                    sandbox.resume_point("host.anchor.before_eof_cleanup", 1)?;
                }
                Ok(close.join().map_err(|_| "close panicked")??)
            })?;
            let expected = if held { "uncertain" } else { "quiescent" };
            check(
                result["state"] == "closed"
                    && result["cancelled_turns"] == json!([format!("{session}/1")])
                    && result["cleanup"] == expected,
                || format!("{prompt}: close result {result}"),
            )?;
            if held {
                sandbox.resume_point("host.anchor.before_eof_cleanup", 1)?;
            }
        }
        sandbox.disarm("host.anchor.before_eof_cleanup")?;
        check(!sandbox.sync.join("agent.pid").exists(), || {
            "a vendor launched past the gate".to_owned()
        })?;
        daemon.finish()
    })
}

/// Design §5 [s2-r1.1]: the idle timer disarms once any order exists. The
/// run loop is held at `core.run.idle_expired` (its timer fired, its idle
/// order not yet issued) while a cancel with a 15 s grace attaches its
/// order (`core.cancel.ordered`). Released, the timer issues nothing: the
/// cancel's `force_at` is not shortened to the idle order's 10 s, so the
/// vendor, which ignores the interrupt, is still running 11.5 s later; a
/// second, short cancel then forces it.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_idle_timer_disarms_once_an_order_exists() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "hang",
            1,
            vec![accepted(1), json!({"action":"hang"})],
        ))?;
        let daemon = sandbox.start()?;
        sandbox.arm("core.run.idle_expired", 1, "pause")?;
        let (session, handle) = sandbox.spawn("hang", &["--idle-ms", "500"])?;
        sandbox.ack(&daemon, "core.run.idle_expired", 1, "pause")?;
        sandbox.arm("core.cancel.ordered", 1, "fail_io")?;
        let reply = thread::scope(|scope| -> TestResult<Value> {
            let cancel = scope.spawn(|| {
                sandbox
                    .ok(&[
                        "cancel",
                        &session,
                        "--force-after",
                        "15000",
                        "--handle",
                        &handle,
                        "--json",
                    ])
                    .map_err(|error| error.to_string())
            });
            // The cancel's order is attached before the timer issues its own.
            sandbox.ack(&daemon, "core.cancel.ordered", 1, "fail_io")?;
            sandbox.resume_point("core.run.idle_expired", 1)?;
            Ok(cancel.join().map_err(|_| "cancel panicked")??)
        })?;
        check(
            reply["state"] == "running" && reply["cancel"]["outcome"] == "requested",
            || format!("cancel acknowledgement: {reply}"),
        )?;
        // Elapsed time only: past an idle order's 10 s grace, short of 15 s.
        thread::sleep(Duration::from_millis(11_500));
        let state: String = sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{session}' AND number=1"
        ))?;
        check(state == "running", || {
            format!("the idle timer shortened the cancel's grace: the turn is {state}")
        })?;
        let forced = sandbox.ok(&[
            "cancel",
            &session,
            "--force-after",
            "100",
            "--wait",
            "--handle",
            &handle,
            "--json",
        ])?;
        check(
            forced["state"] == "cancelled"
                && forced["cancel"]["outcome"] == "forced"
                && forced["cancel"]["requested_at"] == reply["cancel"]["requested_at"],
            || format!("forced after the second cancel: {forced}"),
        )?;
        daemon.finish()
    })
}

/// The last final-shutdown summary in the daemon trace or `via.log`.
#[cfg(feature = "test-failpoints")]
fn shutdown_summary(sandbox: &Sandbox) -> TestResult<Value> {
    sandbox
        .trace()
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|line| line.get("daemon_shutdown").cloned())
        .ok_or_else(|| "the daemon wrote no shutdown summary".into())
}

/// Design §4 step 4, §6.8 entry [r3.2, r4.1]. **Before the fence:** a drain
/// is accepted and its last turn ends; daemon main, entering final
/// shutdown, is paused at `daemon.shutdown.before_fence`, still serving. A
/// close commits `Closing` and requests its dispatcher start; released,
/// the start drain starts it (`queued_drives: 1`) and the close completes.
/// **After the fence:** paused at `daemon.shutdown.after_fence`, a new close
/// is `daemon_stopping`, and a keyed replay of a committed close replays.
/// (The incomplete keyed close whose `Closed` failed is S5's.)
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_at_final_shutdown_entry_refused() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(vec![
            completes("idle", 1),
            script(
                "last",
                1,
                vec![
                    accepted(1),
                    gate("last"),
                    terminal(1, "completed", "end_turn"),
                ],
            ),
            completes("keyed", 1),
        ]))?;
        let before = "daemon.shutdown.before_fence";
        sandbox.arm(before, 1, "pause")?;
        let mut daemon = sandbox.start()?;
        let (idle, idle_handle) = sandbox.spawn("idle", &[])?;
        sandbox.wait(&format!("{idle}/1"))?;
        let (last, last_handle) = sandbox.spawn("last", &[])?;
        sandbox.await_file("last.entered")?;
        let stopping = sandbox.ok(&["daemon", "stop", "--drain", "--json"])?;
        check(stopping["stopping"] == true, || stopping.to_string())?;
        sandbox.release("last")?;
        sandbox.ack(&daemon, before, 1, "pause")?;
        let closed = thread::scope(|scope| -> TestResult<Value> {
            let close = scope.spawn(|| {
                sandbox
                    .ok(&["close", &idle, "--handle", &idle_handle, "--json"])
                    .map_err(|error| error.to_string())
            });
            // `Closing` is durable, so its dispatcher start was requested.
            sandbox.await_row(
                &format!("SELECT admission FROM sessions WHERE id='{idle}'"),
                "closing",
            )?;
            sandbox.resume_point(before, 1)?;
            Ok(close.join().map_err(|_| "close panicked")??)
        })?;
        check(closed["state"] == "closed", || format!("close: {closed}"))?;
        let status = daemon.exit(Duration::from_secs(15))?;
        let summary = shutdown_summary(&sandbox)?;
        check(
            status.code() == Some(0) && summary["queued_drives"] == 1,
            || format!("drain exit {status}: {summary}"),
        )?;
        drop(daemon);
        sandbox.disarm(before)?;
        let after = "daemon.shutdown.after_fence";
        sandbox.arm(after, 1, "pause")?;
        let mut daemon = sandbox.start()?;
        let (keyed, keyed_handle) = sandbox.spawn("keyed", &[])?;
        sandbox.wait(&format!("{keyed}/1"))?;
        let replayed = [
            "close",
            &keyed,
            "--op-key",
            "k1",
            "--handle",
            &keyed_handle,
            "--json",
        ];
        let first = sandbox.ok(&replayed)?;
        let stopping = sandbox.ok(&["daemon", "stop", "--drain", "--json"])?;
        check(stopping["stopping"] == true, || stopping.to_string())?;
        sandbox.ack(&daemon, after, 1, "pause")?;
        sandbox.refused(
            &["close", &last, "--handle", &last_handle, "--json"],
            "daemon_stopping",
        )?;
        let replay = sandbox.ok(&replayed)?;
        check(replay == first, || format!("keyed replay: {replay}"))?;
        sandbox.resume_point(after, 1)?;
        let status = daemon.exit(Duration::from_secs(15))?;
        check(status.code() == Some(0), || format!("drain exit {status}"))?;
        drop(daemon);
        sandbox.disarm(after)?;
        Ok(())
    })
}

/// Design §7.4 (S1 critic finding 1): the final-shutdown bound starts when
/// entry begins. A force is accepted and daemon main, entering, is paused
/// at `daemon.shutdown.before_fence`; a close then takes `admission` and
/// its first Store read is held at `store.read.stall`, never released.
/// Entry waits for `admission`, so it cannot finish: at the deadline daemon
/// main takes the incomplete exit (4), and its summary says the entry
/// expired. Before the fix the daemon stayed alive for as long as the read
/// was held.
#[cfg(feature = "test-failpoints")]
#[test]
#[expect(
    clippy::print_stderr,
    reason = "the measured time is evidence, never asserted (T4-A50)"
)]
fn s1_close_stalled_read_bounds_final_shutdown_entry() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&completes("idle", 1))?;
        let before = "daemon.shutdown.before_fence";
        let stall = "store.read.stall";
        sandbox.arm(before, 1, "pause")?;
        sandbox.count(stall)?;
        let mut daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("idle", &[])?;
        sandbox.wait(&format!("{session}/1"))?;
        let forced_at = Instant::now();
        let stopping = sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
        check(stopping["stopping"] == true, || stopping.to_string())?;
        sandbox.ack(&daemon, before, 1, "pause")?;
        // The close's first two reads (the session and its handle, F15)
        // come before `admission`; its third, the snapshot, holds it.
        let next = sandbox.next_hit(stall)? + 2;
        sandbox.arm(stall, next, "pause")?;
        let status = thread::scope(|scope| -> TestResult<ExitStatus> {
            let close = scope.spawn(|| {
                sandbox
                    .run(&["close", &session, "--handle", &handle, "--json"])
                    .map_err(|error| error.to_string())
            });
            // The close holds `admission` in its read; entry now waits for it.
            sandbox.ack(&daemon, stall, next, "pause")?;
            sandbox.resume_point(before, 1)?;
            let status = daemon.exit(Duration::from_secs(30))?;
            // The close never got a result: its daemon exited under it.
            let closed = close.join().map_err(|_| "close panicked")??;
            check(!closed.status.success(), || {
                format!(
                    "the close succeeded: {}",
                    String::from_utf8_lossy(&closed.stdout)
                )
            })?;
            Ok(status)
        })?;
        // Evidence only (T4-A50): the time from the force to the exit.
        eprintln!("force to exit: {:?}", forced_at.elapsed());
        let summary = shutdown_summary(&sandbox)?;
        check(
            status.code() == Some(4)
                && summary["disposition"] == "incomplete"
                && summary["entry"] == "expired",
            || format!("exit {status}: {summary}"),
        )?;
        drop(daemon);
        sandbox.disarm(before)?;
        sandbox.disarm(stall)?;
        sandbox.start()?.finish()
    })
}

/// Design §6.3, §6.6 [r3.5], the status half (S5 adds the failed
/// `Closed`): a close whose session's group is unproven (its anchor held at
/// `host.anchor.before_eof_cleanup`) waits in its bounded absence check,
/// holding no `admission`. Meanwhile the session is in the durable closing
/// set: `sessions.closing` is 1 with no active turn, and a plain stop is
/// refused. Once the group is gone the close completes, the count is 0 and
/// a plain stop is accepted.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_failed_closed_keeps_closing_count() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "slow",
            1,
            vec![json!({"action":"report_pids"}), accepted(1)],
        ))?;
        let mut daemon = sandbox.start()?;
        let arm_intent = "host.anchor.after_arm_intent_commit";
        let eof_cleanup = "host.anchor.before_eof_cleanup";
        sandbox.arm(arm_intent, 1, "pause")?;
        sandbox.arm(eof_cleanup, 1, "pause")?;
        let (session, handle) = sandbox.spawn("slow", &[])?;
        sandbox.ack(&daemon, arm_intent, 1, "pause")?;
        sandbox.ok(&["cancel", &session, "--handle", &handle, "--json"])?;
        sandbox.resume_point(arm_intent, 1)?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(envelope["cancel"]["cleanup"] == "uncertain", || {
            format!("the group was proved absent: {envelope}")
        })?;
        thread::scope(|scope| -> TestResult {
            let close = scope.spawn(|| {
                sandbox
                    .ok(&[
                        "close",
                        &session,
                        "--deadline-ms",
                        "30000",
                        "--handle",
                        &handle,
                        "--json",
                    ])
                    .map_err(|error| error.to_string())
            });
            sandbox.await_row(
                &format!("SELECT admission FROM sessions WHERE id='{session}'"),
                "closing",
            )?;
            let status = sandbox.ok(&["daemon", "status", "--json"])?;
            check(
                status["sessions"]["closing"] == 1 && status["sessions"]["active"] == 0,
                || format!("closing count: {status}"),
            )?;
            let refused = sandbox.refused(&["daemon", "stop", "--json"], "admission_refused")?;
            check(refused["message"] == "sessions are active", || {
                refused.to_string()
            })?;
            sandbox.process_ack(eof_cleanup, 1, "pause")?;
            sandbox.resume_point(eof_cleanup, 1)?;
            let closed = close.join().map_err(|_| "close panicked")??;
            check(closed["state"] == "closed", || closed.to_string())
        })?;
        sandbox.disarm(eof_cleanup)?;
        let status = sandbox.ok(&["daemon", "status", "--json"])?;
        check(status["sessions"]["closing"] == 0, || status.to_string())?;
        let stopping = sandbox.ok(&["daemon", "stop", "--json"])?;
        check(stopping["stopping"] == true, || stopping.to_string())?;
        let exit = daemon.exit(Duration::from_secs(15))?;
        check(exit.code() == Some(0), || format!("plain stop exit {exit}"))?;
        drop(daemon);
        Ok(())
    })
}

/// Design §7.2 row 11, §6.6 [r3.1, r3.5, r4.1, r4.9], the failure half of
/// `s1_close_failed_closed_keeps_closing_count` (carried from S3 [s3.8]):
/// a `Closed` commit that is not committed (`store.commit.closed`,
/// `fail_io`) replies `store_error` and keeps `closing` durable:
/// `sessions.closing` is 1 and a plain stop is refused. The daemon is still
/// serving at twice `VIA_TEST_IDLE_EXIT_MS` after the failure's
/// acknowledgement (a bounded negative: the sleep only lets time pass). A
/// keyed replay, with no close in progress, passes steps 3–4 and reaches
/// step 5 (the next `Closed` commit, failed again); a retried close then
/// completes, and the count goes to 0.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_failed_closed_keeps_closing_count_after_failure() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new(&script(
            "done",
            1,
            vec![accepted(1), terminal(1, "completed", "end_turn")],
        ))?;
        let idle = Duration::from_millis(500);
        sandbox
            .env
            .push(("VIA_TEST_IDLE_EXIT_MS", idle.as_millis().to_string()));
        let mut daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("done", &[])?;
        sandbox.wait(&format!("{session}/1"))?;
        let closed = "store.commit.closed";
        sandbox.arm(closed, 1, "fail_io")?;
        let keyed = [
            "close", &session, "--op-key", "k1", "--handle", &handle, "--json",
        ];
        sandbox.refused(&keyed, "store_error")?;
        sandbox.ack(&daemon, closed, 1, "fail_io")?;
        let failed = Instant::now();
        sandbox.await_row(
            &format!("SELECT admission FROM sessions WHERE id='{session}'"),
            "closing",
        )?;
        let status = sandbox.ok(&["daemon", "status", "--json"])?;
        check(
            status["sessions"]["closing"] == 1
                && status["sessions"]["active"] == 0
                && status["health"] == "healthy",
            || format!("after the failed Closed: {status}"),
        )?;
        sandbox.refused(&["daemon", "stop", "--json"], "admission_refused")?;
        // Bounded negative: no idle exit while the close is durable.
        thread::sleep((failed + idle * 2).saturating_duration_since(Instant::now()));
        check(daemon.child.try_wait()?.is_none(), || {
            "the daemon exited with a durable close".to_owned()
        })?;
        sandbox.arm(closed, 2, "fail_io")?;
        sandbox.refused(&keyed, "store_error")?;
        sandbox.ack(&daemon, closed, 2, "fail_io")?;
        sandbox.disarm(closed)?;
        let done = sandbox.ok(&keyed)?;
        check(done["state"] == "closed", || {
            format!("retried close: {done}")
        })?;
        let status = sandbox.ok(&["daemon", "status", "--json"])?;
        check(status["sessions"]["closing"] == 0, || status.to_string())?;
        let stopping = sandbox.ok(&["daemon", "stop", "--json"])?;
        check(stopping["stopping"] == true, || stopping.to_string())?;
        let exit = daemon.exit(Duration::from_secs(15))?;
        check(exit.code() == Some(0), || format!("plain stop exit {exit}"))?;
        drop(daemon);
        Ok(())
    })
}

/// Design §4 steps 4–5, §6.8 [r4.6, r5.9], the force variant (S5 owns the
/// latch variant): a close waits in its bounded absence check, its
/// session's group unproven (the anchor held at
/// `host.anchor.before_eof_cleanup`), and a second close subscribes to the
/// same attempt. The second close holds `admission` from its pause at
/// `core.close.before_subscribe` through its subscription, and a stop
/// takes `admission`, so it subscribes before the force. `daemon stop
/// --force` ends the absence check on the force watch, so the attempt
/// publishes no close result, and both waiters reply `daemon_stopping`. No
/// waiter is left: the daemon exits once the group is gone.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_waiter_resolves_on_force() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "slow",
            1,
            vec![json!({"action":"report_pids"}), accepted(1)],
        ))?;
        let mut daemon = sandbox.start()?;
        let arm_intent = "host.anchor.after_arm_intent_commit";
        let eof_cleanup = "host.anchor.before_eof_cleanup";
        let subscribe = "core.close.before_subscribe";
        sandbox.arm(arm_intent, 1, "pause")?;
        sandbox.arm(eof_cleanup, 1, "pause")?;
        let (session, handle) = sandbox.spawn("slow", &[])?;
        sandbox.ack(&daemon, arm_intent, 1, "pause")?;
        sandbox.ok(&["cancel", &session, "--handle", &handle, "--json"])?;
        sandbox.resume_point(arm_intent, 1)?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(envelope["cancel"]["cleanup"] == "uncertain", || {
            format!("the group was proved absent: {envelope}")
        })?;
        sandbox.arm(subscribe, 2, "pause")?;
        let close = [
            "close",
            &session,
            "--deadline-ms",
            "30000",
            "--handle",
            &handle,
            "--json",
        ];
        let (first, second) = thread::scope(|scope| -> TestResult<(Value, Value)> {
            let first = scope.spawn(|| {
                sandbox
                    .refused(&close, "daemon_stopping")
                    .map_err(|error| error.to_string())
            });
            sandbox.await_row(
                &format!("SELECT admission FROM sessions WHERE id='{session}'"),
                "closing",
            )?;
            let second = scope.spawn(|| {
                sandbox
                    .refused(&close, "daemon_stopping")
                    .map_err(|error| error.to_string())
            });
            sandbox.ack(&daemon, subscribe, 2, "pause")?;
            sandbox.resume_point(subscribe, 2)?;
            let stopping = sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
            check(stopping["stopping"] == true, || stopping.to_string())?;
            let first = first.join().map_err(|_| "first close panicked")??;
            let second = second.join().map_err(|_| "second close panicked")??;
            Ok((first, second))
        })?;
        check(
            first["data"]["kind"] == "daemon_stopping"
                && second["data"]["kind"] == "daemon_stopping",
            || format!("close replies: {first} {second}"),
        )?;
        sandbox.process_ack(eof_cleanup, 1, "pause")?;
        sandbox.resume_point(eof_cleanup, 1)?;
        let status = daemon.exit(Duration::from_secs(15))?;
        check(status.code() == Some(0), || {
            format!("force exit {status}: {}", sandbox.trace())
        })?;
        drop(daemon);
        sandbox.disarm(eof_cleanup)?;
        sandbox.disarm(subscribe)?;
        Ok(())
    })
}

/// Design §3.4, §6.8 [r3.4, r4.9]: a `cancel --wait` whose order is in
/// place, then `daemon stop --force`. The turn is handed to final shutdown,
/// paused at `core.shutdown.before_forced_terminal`; the waiter has not
/// replied (a dropped sender is not a terminal). Released, it replies with
/// the committed forced terminal.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_wait_across_force_handoff() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "hang",
            1,
            vec![accepted(1), json!({"action":"hang"})],
        ))?;
        let mut daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("hang", &[])?;
        sandbox.wait_for_event(&session, "turn.started")?;
        let point = "core.shutdown.before_forced_terminal";
        sandbox.arm(point, 1, "pause")?;
        let reply = thread::scope(|scope| -> TestResult<Value> {
            let waiter = scope.spawn(|| {
                sandbox
                    .ok(&[
                        "cancel",
                        &session,
                        "--force-after",
                        "60000",
                        "--wait",
                        "--handle",
                        &handle,
                        "--json",
                    ])
                    .map_err(|error| error.to_string())
            });
            sandbox.wait_for_event(&session, "cancel.requested")?;
            let stopping = sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
            check(stopping["stopping"] == true, || stopping.to_string())?;
            sandbox.ack(&daemon, point, 1, "pause")?;
            check(!waiter.is_finished(), || {
                "the waiter replied before the forced terminal".to_owned()
            })?;
            sandbox.resume_point(point, 1)?;
            Ok(waiter.join().map_err(|_| "waiter panicked")??)
        })?;
        check(
            reply["state"] == "cancelled" && reply["cancel"]["outcome"] == "forced",
            || format!("waiter reply: {reply}"),
        )?;
        let status = daemon.exit(Duration::from_secs(15))?;
        check(status.code() == Some(0), || {
            format!("force exit {status}: {}", sandbox.trace())
        })?;
        drop(daemon);
        sandbox.disarm(point)?;
        Ok(())
    })
}

/// Design §4 steps 4–5, §7.4 [r4.6, r5.9], the latch variant of
/// `s1_close_waiter_resolves_on_force_and_latch` (carried from S3 [s3.8]):
/// a close waits in its bounded absence check (the anchor held at
/// `host.anchor.before_eof_cleanup`) and a second close subscribes to the
/// same attempt. Another session's receipt then latches Store failure (its
/// reply is lost, `store.commit.reply_lost`): the absence check ends on the
/// force watch, and both waiters reply `store_error`. No waiter is left:
/// the daemon exits 4 once the group is gone.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_waiter_resolves_on_latch() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "slow",
            1,
            vec![json!({"action":"report_pids"}), accepted(1)],
        ))?;
        sandbox.count("store.commit.reply_lost")?;
        let mut daemon = sandbox.start()?;
        let arm_intent = "host.anchor.after_arm_intent_commit";
        let eof_cleanup = "host.anchor.before_eof_cleanup";
        let subscribe = "core.close.before_subscribe";
        sandbox.arm(arm_intent, 1, "pause")?;
        sandbox.arm(eof_cleanup, 1, "pause")?;
        let (session, handle) = sandbox.spawn("slow", &[])?;
        sandbox.ack(&daemon, arm_intent, 1, "pause")?;
        sandbox.ok(&["cancel", &session, "--handle", &handle, "--json"])?;
        sandbox.resume_point(arm_intent, 1)?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(envelope["cancel"]["cleanup"] == "uncertain", || {
            format!("the group was proved absent: {envelope}")
        })?;
        sandbox.arm(subscribe, 2, "pause")?;
        let close = [
            "close",
            &session,
            "--deadline-ms",
            "30000",
            "--handle",
            &handle,
            "--json",
        ];
        let (first, second) = thread::scope(|scope| -> TestResult<(Value, Value)> {
            let first = scope.spawn(|| {
                sandbox
                    .refused(&close, "store_error")
                    .map_err(|error| error.to_string())
            });
            sandbox.await_row(
                &format!("SELECT admission FROM sessions WHERE id='{session}'"),
                "closing",
            )?;
            let second = scope.spawn(|| {
                sandbox
                    .refused(&close, "store_error")
                    .map_err(|error| error.to_string())
            });
            sandbox.ack(&daemon, subscribe, 2, "pause")?;
            sandbox.resume_point(subscribe, 2)?;
            // The Store's one worker serves this read after the `Closing`
            // commit, whose reply hit precedes it: the count is settled.
            sandbox.events(&session)?;
            let lost = sandbox.next_hit("store.commit.reply_lost")?;
            sandbox.arm("store.commit.reply_lost", lost, "fail_io")?;
            sandbox.refused(
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "latch",
                    "--background",
                    "--json",
                ],
                "store_error",
            )?;
            sandbox.ack(&daemon, "store.commit.reply_lost", lost, "fail_io")?;
            let first = first.join().map_err(|_| "first close panicked")??;
            let second = second.join().map_err(|_| "second close panicked")??;
            Ok((first, second))
        })?;
        check(
            first["data"]["kind"] == "store_error" && second["data"]["kind"] == "store_error",
            || format!("close replies: {first} {second}"),
        )?;
        sandbox.process_ack(eof_cleanup, 1, "pause")?;
        sandbox.resume_point(eof_cleanup, 1)?;
        let status = daemon.exit(Duration::from_secs(15))?;
        check(status.code() == Some(4), || {
            format!("latched exit {status}: {}", sandbox.trace())
        })?;
        drop(daemon);
        sandbox.disarm(eof_cleanup)?;
        sandbox.disarm(subscribe)?;
        Ok(())
    })
}

/// Design §3.3 "Drop without an acknowledgement", §3.4 [r1.4, r3.4, r4.9],
/// the variant of `s1_cancel_wait_across_force_handoff` with an
/// unacknowledged order (carried from S3 [s3.8]): a `cancel --wait` is
/// admitted (held at `core.cancel.admitted`, past step 3) before
/// `daemon stop --force` is accepted. The forced turn settles (its run loop
/// held at `core.run.settling`), so the cancel sends no order and waits for
/// the drop (`core.cancel.settling`). Final shutdown is held at
/// `core.shutdown.before_forced_terminal` and the waiter has not replied;
/// released, it replies the committed forced terminal with
/// `already_terminal: true`.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_wait_across_force_handoff_unacknowledged() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "hang",
            1,
            vec![accepted(1), json!({"action":"hang"})],
        ))?;
        let mut daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("hang", &[])?;
        sandbox.wait_for_event(&session, "turn.started")?;
        let admitted = "core.cancel.admitted";
        let run_settling = "core.run.settling";
        let settling = "core.cancel.settling";
        let terminal = "core.shutdown.before_forced_terminal";
        sandbox.arm(admitted, 1, "pause")?;
        sandbox.arm(run_settling, 1, "pause")?;
        sandbox.arm(settling, 1, "fail_io")?;
        sandbox.arm(terminal, 1, "pause")?;
        let reply = thread::scope(|scope| -> TestResult<Value> {
            let waiter = scope.spawn(|| {
                sandbox
                    .ok(&[
                        "cancel",
                        &session,
                        "--force-after",
                        "60000",
                        "--wait",
                        "--handle",
                        &handle,
                        "--json",
                    ])
                    .map_err(|error| error.to_string())
            });
            sandbox.ack(&daemon, admitted, 1, "pause")?;
            let stopping = sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
            check(stopping["stopping"] == true, || stopping.to_string())?;
            sandbox.ack(&daemon, run_settling, 1, "pause")?;
            sandbox.resume_point(admitted, 1)?;
            sandbox.ack(&daemon, settling, 1, "fail_io")?;
            sandbox.resume_point(run_settling, 1)?;
            sandbox.ack(&daemon, terminal, 1, "pause")?;
            check(!waiter.is_finished(), || {
                "the waiter replied before the forced terminal".to_owned()
            })?;
            sandbox.resume_point(terminal, 1)?;
            Ok(waiter.join().map_err(|_| "waiter panicked")??)
        })?;
        check(
            reply["state"] == "cancelled"
                && reply["already_terminal"] == true
                && reply["cancel"]["outcome"] == "forced",
            || format!("waiter reply: {reply}"),
        )?;
        let status = daemon.exit(Duration::from_secs(15))?;
        check(status.code() == Some(0), || {
            format!("force exit {status}: {}", sandbox.trace())
        })?;
        drop(daemon);
        for point in [admitted, run_settling, settling, terminal] {
            sandbox.disarm(point)?;
        }
        Ok(())
    })
}

/// Design §3.4, §7.2 row 15 [r3.4, r3.11], variant of
/// `s1_cancel_wait_across_force_handoff` whose forced terminal never
/// commits (carried from S3 [s3.8]): the acknowledged `cancel --wait`
/// waits through the handoff; final shutdown's forced terminal is not
/// committed (`store.commit.terminal`, `fail_io`: no latch). Once final
/// shutdown finalized, the waiter replies as `wait` does: `store_error`
/// for the turn recorded unpersisted, with its durable state. The exit is 4
/// and the summary counts one uncommitted turn.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_wait_across_force_handoff_terminal_not_committed() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "hang",
            1,
            vec![accepted(1), json!({"action":"hang"})],
        ))?;
        let mut daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("hang", &[])?;
        sandbox.wait_for_event(&session, "turn.started")?;
        let point = "core.shutdown.before_forced_terminal";
        sandbox.arm(point, 1, "pause")?;
        let error = thread::scope(|scope| -> TestResult<Value> {
            let waiter = scope.spawn(|| {
                sandbox
                    .refused(
                        &[
                            "cancel",
                            &session,
                            "--force-after",
                            "60000",
                            "--wait",
                            "--handle",
                            &handle,
                            "--json",
                        ],
                        "store_error",
                    )
                    .map_err(|error| error.to_string())
            });
            sandbox.wait_for_event(&session, "cancel.requested")?;
            let stopping = sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
            check(stopping["stopping"] == true, || stopping.to_string())?;
            sandbox.ack(&daemon, point, 1, "pause")?;
            check(!waiter.is_finished(), || {
                "the waiter replied before the forced terminal".to_owned()
            })?;
            // The forced terminal is the first terminal commit.
            sandbox.arm("store.commit.terminal", 1, "fail_io")?;
            sandbox.resume_point(point, 1)?;
            sandbox.ack(&daemon, "store.commit.terminal", 1, "fail_io")?;
            Ok(waiter.join().map_err(|_| "waiter panicked")??)
        })?;
        check(
            error["data"]["durable_state"] == "running"
                && error["data"]["terminal_persisted"] == false,
            || format!("waiter reply: {error}"),
        )?;
        let status = daemon.exit(Duration::from_secs(15))?;
        check(status.code() == Some(4), || {
            format!("exit {status}: {}", sandbox.trace())
        })?;
        let summary = sandbox
            .trace()
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|line| line.get("daemon_shutdown").cloned())
            .ok_or("no shutdown summary")?;
        check(
            summary["uncommitted_turns"] == 1 && summary["store_failed"] == false,
            || format!("summary: {summary}"),
        )?;
        drop(daemon);
        sandbox.disarm(point)?;
        Ok(())
    })
}

/// Design §4 "Force" [r4.6, r5.8, r6.6], force variant of
/// `s1_close_outcome_retained_for_late_subscriber`: a keyed close is in
/// progress when a force is accepted. Daemon main, paused at
/// `daemon.shutdown.after_fence`, still serves, and the running turn's
/// dispatcher is held at `core.run.before_handoff`, so the close is still
/// in progress: a keyed replay connecting after the force reaches the close
/// watch and is paused at `core.close.before_subscribe`. The dispatcher's
/// force exit then publishes `daemon_stopping` to the first caller, and the
/// replay, released after the publication, receives it from the retained
/// outcome. A third keyed replay, after the force exit, finds no attempt in
/// progress and is fenced [r5.8].
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_outcome_retained_for_late_subscriber_under_force() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "hang",
            1,
            vec![accepted(1), json!({"action":"hang"})],
        ))?;
        let mut daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("hang", &[])?;
        sandbox.wait_for_event(&session, "turn.started")?;
        let keyed = [
            "close",
            &session,
            "--op-key",
            "k1",
            "--deadline-ms",
            "60000",
            "--handle",
            &handle,
            "--json",
        ];
        let fence = "daemon.shutdown.after_fence";
        let handoff = "core.run.before_handoff";
        let subscribe = "core.close.before_subscribe";
        thread::scope(|scope| -> TestResult {
            let first = scope.spawn(|| {
                sandbox
                    .refused(&keyed, "daemon_stopping")
                    .map_err(|error| error.to_string())
            });
            sandbox.wait_for_event(&session, "cancel.requested")?;
            sandbox.arm(fence, 1, "pause")?;
            sandbox.arm(handoff, 1, "pause")?;
            // The first caller passed the seam unarmed: this is its second hit.
            sandbox.arm(subscribe, 2, "pause")?;
            let stopping = sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
            check(stopping["stopping"] == true, || stopping.to_string())?;
            sandbox.ack(&daemon, fence, 1, "pause")?;
            sandbox.ack(&daemon, handoff, 1, "pause")?;
            let replay = scope.spawn(|| {
                sandbox
                    .refused(&keyed, "daemon_stopping")
                    .map_err(|error| error.to_string())
            });
            sandbox.ack(&daemon, subscribe, 2, "pause")?;
            check(!first.is_finished(), || {
                "the close replied before the force exit".to_owned()
            })?;
            sandbox.resume_point(handoff, 1)?;
            // The force exit published: the first caller has its outcome.
            first.join().map_err(|_| "first close panicked")??;
            check(!replay.is_finished(), || {
                "the paused replay replied before its release".to_owned()
            })?;
            sandbox.resume_point(subscribe, 2)?;
            replay.join().map_err(|_| "replay panicked")??;
            // After the force exit no attempt is in progress: a keyed replay is
            // fenced before it could subscribe, so the armed pause never acts.
            sandbox.arm(subscribe, 3, "pause")?;
            sandbox.refused(&keyed, "daemon_stopping")?;
            check(!sandbox.acked(subscribe, 3), || {
                "the fenced replay reached the close watch".to_owned()
            })?;
            sandbox.resume_point(fence, 1)?;
            Ok(())
        })?;
        let status = daemon.exit(Duration::from_secs(15))?;
        check(status.code() == Some(0), || {
            format!("force exit {status}: {}", sandbox.trace())
        })?;
        drop(daemon);
        for point in [fence, handoff, subscribe] {
            sandbox.disarm(point)?;
        }
        Ok(())
    })
}
