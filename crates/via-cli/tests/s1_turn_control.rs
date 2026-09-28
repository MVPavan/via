//! Turn control through the real `via` binary and daemon (design T3 §2–§5,
//! §11): `cancel` (C1 §3.5), `close` and the `closing` gate (§3.6, §7.1),
//! the idle deadline (§4 `deadlines.idle_ms`), and F19–F21. Waits are bounded
//! waits on durable rows, fake gates, failpoint acknowledgements or process
//! exit; a sleep only lets time pass, never orders two events.

#[cfg(feature = "test-failpoints")]
#[path = "support/failpoints.rs"]
mod failpoints;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;

use std::error::Error;
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

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

impl Sandbox {
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
        Ok(Self {
            root,
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
            let status = self.run(&["daemon", "status", "--json"])?;
            if status.status.success() {
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

    /// Occurrences of `needle` across every raw log file.
    fn raw_count(&self, needle: &str) -> TestResult<usize> {
        let mut count = 0;
        for entry in fs::read_dir(self.state.join("raw"))? {
            let bytes = fs::read(entry?.path())?;
            let text = String::from_utf8_lossy(&bytes);
            count += text.matches(needle).count();
        }
        Ok(count)
    }

    fn trace(&self) -> String {
        fs::read_to_string(self.root.path().join("daemon.trace")).unwrap_or_default()
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
}

/// Design §2, §3.3, §11: the running turn gets one interrupt; its
/// `interrupted` terminal ends it `cancelled`, `acknowledged`, cleanup
/// `quiescent`; the running-turn reply is the acknowledgement (A8), a second
/// cancel coalesces with no second interrupt, and the terminal turn then
/// replies `already_terminal: true`.
#[test]
fn s1_cancel_running_turn_acknowledged() -> TestResult {
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
    let interrupts = sandbox.raw_count(r#""type":"interrupt","id":2"#)?;
    check(interrupts == 1, || {
        format!("{interrupts} interrupts were sent")
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
}

/// F20 (design §11): an agent that ignores SIGTERM is killed; its group is
/// gone within 3 s of `force_at`, with outcome `forced`.
#[test]
fn s1_f20_sigterm_ignored_escalates_to_kill() -> TestResult {
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
}

// --------------------------------------------------------- idle deadline

/// F19 (design §5, §11 [r1.9, r1.10]): with no meaningful progress within
/// `idle_ms` the turn is ordered to stop with cause `idle_deadline`; the
/// vendor's `interrupted` ends it `failed(deadline_idle)`. Unknown frames and
/// stderr during the window do not reset the idle clock.
#[test]
fn s1_f19_idle_deadline_fails_turn_and_clears_group() -> TestResult {
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
    sandbox.release("noise2")?;
    let envelope = sandbox.wait(&format!("{session}/1"))?;
    let monotonic = monotonic_start.elapsed();
    let wall = SystemTime::now()
        .duration_since(wall_start)
        .unwrap_or_default();
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
    // but not the monotonic clock: the interval is then bounded from the
    // harness's monotonic spawn-to-terminal time instead.
    let stepped = wall.abs_diff(monotonic) > Duration::from_millis(250);
    let timely = if stepped {
        (Duration::from_millis(1950)..Duration::from_millis(4000)).contains(&monotonic)
    } else {
        (1950..2800).contains(&idle_after)
    };
    check(timely, || {
        format!(
            "the idle stop came {idle_after} ms after acceptance (harness elapsed: \
             wall {wall:?}, monotonic {monotonic:?}; events {events:?})"
        )
    })?;
    check(
        events.iter().any(|event| event["type"] == "vendor.other"),
        || "the unknown frames were not recorded".to_owned(),
    )?;

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
}

/// F19 wall variant (characterization: the wall deadline already
/// force-closed the group before S2): `deadline_wall`, the grandchild gone.
#[test]
fn s1_f19_wall_deadline_clears_grandchild() -> TestResult {
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
}

/// F21 (design §2; characterization of S1's Route mapping end to end): an
/// exit after an unterminated line is `failed(process_exited)`, and the
/// partial bytes are in the raw log.
#[test]
fn s1_f21_crash_mid_line_is_process_exited() -> TestResult {
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
    check(sandbox.raw_count("partial-f21")? >= 1, || {
        "the partial line is not in the raw log".to_owned()
    })?;
    daemon.finish()
}

/// Design §5: `idle_ms` is frozen at acceptance, inherited like `wall_ms`,
/// defaults to 600 000, and 0 is `invalid_params`.
#[test]
fn s1_idle_ms_is_frozen_inherited_and_positive() -> TestResult {
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
}

// ----------------------------------------------------------------- close

/// Design §4, §11: `close` makes the session `closing` (`resume` is
/// `session_closed` then and after), cancels the queue and the running turn
/// with `cancel_cause = 'close'`, and commits `session.closed {reason:
/// close}` with the derived `cancelled_turns` and `cleanup`.
#[test]
fn s1_close_cancels_queue_and_running_turn() -> TestResult {
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
}

/// Design §4 step 2 [r1.5]: an `op_key` replay returns the same result, and
/// other params under the key are `idempotency_conflict`.
#[test]
fn s1_close_keyed_retry_and_second_close() -> TestResult {
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
            "close", &session, "--op-key", "k1", "--mode", "force", "--handle", &handle, "--json",
        ],
        "invalid_params",
    )?;
    daemon.finish()
}

/// Design §4 "Force" [r4.6]: `daemon stop --force` during a close closes the
/// session `daemon_stop_force`, the close waiter gets `daemon_stopping`, and
/// the exit is 0 with positive cleanup.
#[test]
fn s1_close_racing_force_stop() -> TestResult {
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

    fn disarm(&self, point: &str) -> TestResult {
        Ok(self.failpoints.disarm(point)?)
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
    let sandbox = Sandbox::new(&scripts(vec![
        completes("claimed", 1),
        completes("submitting", 1),
    ]))?;
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
}

/// Design §3.1 [r1.2], §11: a turn waiting at `core.dispatch.awaiting_slot`
/// stays `Waiting`, so a cancel takes it: `cancelled` without submission; no
/// permit is consumed, and the next turn launches once the slot frees.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_while_waiting_for_slot() -> TestResult {
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
}

/// Design §4, §11 (adapted): a turn waiting for the only connection slot,
/// held by another session's live turn, is cancelled by `close`; the close
/// completes and the dispatcher exits while the slot stays held.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_cancels_turn_waiting_for_slot() -> TestResult {
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
}

/// Design §3.3 [r1.4], §11: a cancel while the run loop is paused at
/// `core.run.settling` sends no order (`core.cancel.settling` witnesses the
/// step); its reply comes after the drop,
/// `already_terminal: true` with the envelope's `cancel` (none here).
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_during_settlement_completes() -> TestResult {
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
}

/// Design §2, §11: the anchor's `Stop` reply is lost
/// (`host.anchor.final_reply_lost`), so no stop evidence proves the vendor
/// was live: the turn is `unknown` (`stop_reason: error`) with outcome
/// `requested`, and its cleanup is decided independently.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_lost_stop_reply_is_unknown() -> TestResult {
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
}

/// Design §2 rule 1 [r1.8], §11: an order that reaches Host's pre-ARM gate
/// launches nothing: `cancelled`, `requested`, with the acquisition's own
/// absence evidence. The anchor held at `host.anchor.before_eof_cleanup`
/// leaves it unproven, so cleanup is `uncertain`; released, the same case
/// is `quiescent`.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_before_launch_slow_anchor_is_uncertain() -> TestResult {
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
}

/// Design §3.2 [r1.13]: a queued cancellation whose read fails
/// (`store.read.queued_turn`) rolls back, writes nothing and replies a
/// plain `store_error`; a retried cancel succeeds.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_cancel_queued_read_failure_is_plain_store_error() -> TestResult {
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
}

/// Design §4 [r1.6], §11: the daemon crashes after the close's first queued
/// cancellation (`store.commit.cancel`), and the restart crashes again at the
/// next; the final `cancelled_turns` lists every turn the close cancelled,
/// identical across restarts.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_partial_restarts_keep_one_result() -> TestResult {
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
}

/// Design §4 "Force" [r4.6, r5.8, r6.6], latch variant: a keyed replay of
/// a close in progress reaches the close watch and is paused at
/// `core.close.before_subscribe`, holding `admission`. Another session's
/// terminal commit fails (`store.commit.terminal`), and the latch's first
/// phase (which needs no `admission`) forces the closing session's running
/// turn; the dispatcher's latch exit publishes `store_error` in that window.
/// The first caller receives it, and the paused replay, released after the
/// publication, still receives it from the retained outcome. (The design's
/// force variant needs a caller entering after force is accepted; the S2
/// server stops accepting connections at force, so that variant is S3's.)
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_close_outcome_retained_for_late_subscriber() -> TestResult {
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
        // No terminal committed yet: the other session's is the first.
        sandbox.arm("store.commit.terminal", 1, "fail_io")?;
        sandbox.release("other")?;
        sandbox.ack(&daemon, "store.commit.terminal", 1, "fail_io")?;
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
}

/// Design §5 deadline origin [r1.11]: the wall deadline runs from the
/// submission clock, taken before `core.submit.before_commit`. A submission
/// held there past a short `wall_ms` gets no extra time: `deadline_wall`,
/// and nothing launches.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f19_delayed_submission_gets_no_extra_wall_time() -> TestResult {
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
}

/// Design §2 [r1.23]: the vendor acknowledges the interrupt (`interrupted`),
/// then holds its stdout open past the wall deadline. The acknowledgement
/// stands: `cancelled`, `acknowledged`, not `deadline_wall`.
#[test]
fn s1_cancel_acknowledged_survives_wall_expiry() -> TestResult {
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
}
