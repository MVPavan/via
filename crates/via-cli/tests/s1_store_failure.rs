//! Store failures through the real `via` binary and daemon (F12; design T3
//! §7, owner decision O1). A write whose outcome is known not committed is
//! scoped to its request or turn; an uncertain one, an escalation or SQLite
//! corruption latches, and final shutdown then runs the diagnostic window
//! and the failure-resolution batch (§7.4). Each test arms §10 seams and
//! waits only on failpoint acknowledgements, durable rows, sockets or
//! process exit; a sleep only lets time pass, never orders two events.
#![cfg(feature = "test-failpoints")]

#[path = "support/failpoints.rs"]
mod failpoints;
#[path = "support/hits.rs"]
mod hits;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;

use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// This binary's version, which the harness's own connections present.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A fixed, valid session handle, so a keyed retry's params are identical.
const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// A finished command.
struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
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
    let Some(status) = wait_child(&mut child, timeout)? else {
        child.kill()?;
        child.wait()?;
        return Err(format!("{command:?} timed out").into());
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
    })
}

/// Waits up to `within` for `child` to exit.
fn wait_child(child: &mut Child, within: Duration) -> TestResult<Option<ExitStatus>> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Waits up to `within` until `ready` holds.
fn wait_until(what: &str, within: Duration, mut ready: impl FnMut() -> bool) -> TestResult {
    let deadline = Instant::now() + within;
    while !ready() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting until {what}").into());
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn check(condition: bool, message: impl FnOnce() -> String) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message().into())
    }
}

/// One isolated deployment: private state, runtime and fake sync dirs, the
/// fake fixture, extra environment and the failpoint controller.
struct Sandbox {
    root: tempfile::TempDir,
    via: PathBuf,
    fake: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    sync: PathBuf,
    fixture: PathBuf,
    env: Vec<(&'static str, String)>,
    runs: std::cell::Cell<u32>,
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
            runs: std::cell::Cell::new(0),
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
        run_command(&mut command, Duration::from_secs(60))
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

    /// Starts a daemon directly, with the failpoint controller, and waits
    /// until it answers; readiness never auto-starts another daemon.
    fn start(&self) -> TestResult<Daemon<'_>> {
        let run = self.runs.get() + 1;
        self.runs.set(run);
        let trace = self.root.path().join(format!("daemon-{run}.trace"));
        let mut command = self.command();
        self.failpoints.activate(&mut command);
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&trace)?);
        let mut daemon = Daemon {
            child: command.spawn()?,
            sandbox: self,
            trace,
        };
        daemon.ready()?;
        Ok(daemon)
    }

    /// `daemon/status` over a direct connection: never auto-starts.
    fn status(&self) -> TestResult<Value> {
        let mut raw = Raw::hello(&self.runtime)?;
        let reply = raw.call("daemon/status", &json!({}))?;
        reply
            .get("result")
            .cloned()
            .ok_or_else(|| format!("daemon/status refused: {reply}").into())
    }

    /// Spawns turn 1 in the background: `(session, handle)`.
    fn spawn(&self, prompt: &str) -> TestResult<(String, String)> {
        let receipt = self.ok(&spawn_args(prompt, &[]))?;
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
        wait_until(
            &format!("the fake creates {name}"),
            Duration::from_secs(20),
            || path.exists(),
        )
    }

    fn release(&self, gate: &str) -> TestResult {
        fs::write(self.sync.join(format!("{gate}.release")), b"")?;
        Ok(())
    }

    /// One value read from the Store through a read-only connection.
    fn query<T: rusqlite::types::FromSql>(&self, sql: &str) -> TestResult<T> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        Ok(store.query_row(sql, [], |row| row.get(0))?)
    }

    /// Proves every committed anchor's group absent, as the runtime §11.2
    /// outer harness does.
    fn verify_anchors(&self) -> TestResult {
        let rows = outer_cleanup::snapshot(&self.state.join("store.sqlite3"))?;
        let anchors = outer_cleanup::verify(&rows, Instant::now() + Duration::from_secs(10));
        let absent = anchors["status"] == "quiescent" && anchors["absence_proven"] == true;
        let none = anchors["status"] == "no_anchors";
        check(absent || none, || {
            format!("outer cleanup is unverified: {anchors}")
        })
    }

    fn arm(&self, point: &str, occurrence: u64, action: &str) -> TestResult {
        Ok(self.failpoints.arm(point, occurrence, action)?)
    }

    fn ack(&self, daemon: &Daemon<'_>, point: &str, occurrence: u64, action: &str) -> TestResult {
        let pid = self.process_ack(point, occurrence, action)?;
        check(pid == daemon.pid(), || {
            format!("{point} #{occurrence} was acknowledged by pid {pid}")
        })
    }

    /// Waits for the acknowledgement of `point`'s `occurrence` by any
    /// process (the daemon or an anchor) and returns its pid, after checking
    /// the acknowledgement in full.
    fn process_ack(&self, point: &str, occurrence: u64, action: &str) -> TestResult<u32> {
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
        let pid = u32::try_from(ack["pid"].as_u64().ok_or("acknowledgement has no pid")?)?;
        self.failpoints
            .wait_ack(point, occurrence, action, pid, Duration::from_secs(1))?;
        Ok(pid)
    }

    fn resume_point(&self, point: &str, occurrence: u64) -> TestResult {
        Ok(self.failpoints.release(point, occurrence)?)
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

fn spawn_args<'a>(prompt: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
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
    args
}

/// A direct C1 connection, which never auto-starts a daemon.
struct Raw {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
}

impl Raw {
    /// Connects and says `hello`.
    fn hello(runtime: &Path) -> TestResult<Self> {
        let stream = UnixStream::connect(runtime.join("via.sock"))
            .map_err(|error| format!("the daemon is not serving: {error}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut raw = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next: 0,
        };
        let hello = raw.call(
            "hello",
            &json!({"api_version":1,"client_version":VERSION,"client":"s1-store-failure"}),
        )?;
        check(hello.get("result").is_some(), || {
            format!("hello refused: {hello}")
        })?;
        Ok(raw)
    }

    /// One request and its whole reply.
    fn call(&mut self, method: &str, params: &Value) -> TestResult<Value> {
        self.next += 1;
        let line = json!({"jsonrpc":"2.0","id":self.next,"method":method,"params":params});
        self.writer.write_all(format!("{line}\n").as_bytes())?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply)? == 0 {
            return Err(
                format!("the daemon closed the connection before replying to {method}").into(),
            );
        }
        Ok(serde_json::from_str(&reply)?)
    }
}

/// A daemon child the harness started. `finish` force-stops it and proves
/// outer cleanup; dropping it force-stops and reaps it.
struct Daemon<'a> {
    child: Child,
    sandbox: &'a Sandbox,
    trace: PathBuf,
}

impl Daemon<'_> {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn ready(&mut self) -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Err(format!(
                    "daemon exited before readiness: {status}: {}",
                    fs::read_to_string(&self.trace).unwrap_or_default()
                )
                .into());
            }
            if self.sandbox.status().is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("daemon readiness deadline elapsed".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the daemon to exit by itself.
    fn exit(&mut self, within: Duration) -> TestResult<ExitStatus> {
        wait_child(&mut self.child, within)?.ok_or_else(|| {
            format!(
                "the daemon did not exit in time: {}",
                fs::read_to_string(&self.trace).unwrap_or_default()
            )
            .into()
        })
    }

    /// The daemon's final shutdown summary.
    fn summary(&self) -> TestResult<Value> {
        fs::read_to_string(&self.trace)?
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|line| line.get("daemon_shutdown").cloned())
            .ok_or_else(|| "the daemon wrote no shutdown summary".into())
    }

    /// Everything the daemon wrote to its trace.
    fn trace(&self) -> String {
        fs::read_to_string(&self.trace).unwrap_or_default()
    }

    /// A plain `daemon stop` once no work is active; the exit must be 0.
    fn stop_clean(mut self) -> TestResult {
        wait_until("the daemon is idle", Duration::from_secs(20), || {
            self.sandbox
                .status()
                .is_ok_and(|status| status["sessions"]["active"] == 0)
        })?;
        self.sandbox.ok(&["daemon", "stop", "--json"])?;
        let status = self.exit(Duration::from_secs(15))?;
        check(status.code() == Some(0), || {
            format!("a plain stop exited {status}: {}", self.trace())
        })?;
        self.sandbox.verify_anchors()
    }

    /// Waits for a latched daemon's own exit: 4 with `store_failed`.
    fn latched_exit(mut self) -> TestResult<Value> {
        let status = self.exit(Duration::from_secs(20))?;
        let summary = self.summary()?;
        check(
            status.code() == Some(4) && summary["store_failed"] == true,
            || format!("expected the latch's exit 4, got {status}: {summary}"),
        )?;
        self.sandbox.verify_anchors()?;
        Ok(summary)
    }
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.sandbox.run(&["daemon", "stop", "--force", "--json"]);
            let _ = wait_child(&mut self.child, Duration::from_secs(15));
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn accepted(turn: u32) -> Value {
    json!({"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":format!("fake-turn-{turn}")}})
}

fn terminal(turn: u32) -> Value {
    json!({"action":"emit","message":{"type":"terminal","vendor_turn_id":format!("fake-turn-{turn}"),
        "status":"completed","final_text":"done","stop_reason":"end_turn"}})
}

fn gate(name: &str) -> Value {
    json!({"action":"gate","name":name})
}

fn script(prompt: &str, turn: u32, steps: Vec<Value>) -> Value {
    let mut script = json!({"expected_request":{"type":"start","turn":turn,"prompt":prompt}});
    script["steps"] = Value::Array(steps);
    script
}

fn scripts(scripts: &[Value]) -> Value {
    json!({ "scripts": scripts })
}

/// A completing turn `turn` for `prompt`.
fn completes(prompt: &str, turn: u32) -> Value {
    script(prompt, turn, vec![accepted(turn), terminal(turn)])
}

/// A turn `turn` for `prompt` that holds at gate `prompt` before completing.
fn held(prompt: &str, turn: u32) -> Value {
    script(
        prompt,
        turn,
        vec![accepted(turn), gate(prompt), terminal(turn)],
    )
}

/// The event types of `turn`, in sequence order.
fn event_types(events: &[Value], turn: u32) -> Vec<String> {
    events
        .iter()
        .filter(|event| event["turn"] == turn)
        .filter_map(|event| event["type"].as_str().map(str::to_owned))
        .collect()
}

/// The session's event sequence numbers are dense from 1 (C1 §6.1).
fn dense(events: &[Value]) -> TestResult {
    let seqs: Vec<u64> = events
        .iter()
        .filter_map(|event| event["seq"].as_u64())
        .collect();
    let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
    check(seqs == expected, || {
        format!("events are not dense: {seqs:?}")
    })
}

/// Design §11 F12's scoped-test conditions: a second session's turn, held
/// at gate `other` while the failure happened, still completes; no latch
/// (`health: healthy`, the daemon still serves); and a plain stop later
/// exits 0.
fn scoped_end(daemon: Daemon<'_>, other: &str) -> TestResult {
    let sandbox = daemon.sandbox;
    let status = sandbox.status()?;
    check(status["health"] == "healthy", || {
        format!("a scoped failure latched: {status}")
    })?;
    sandbox.release("other")?;
    let envelope = sandbox.wait(&format!("{other}/1"))?;
    check(envelope["state"] == "completed", || {
        format!("the second session's turn was affected: {envelope}")
    })?;
    daemon.stop_clean()
}

/// The second session of [`scoped_end`]: its turn 1 is held at gate
/// `other`.
fn other_session(sandbox: &Sandbox) -> TestResult<String> {
    let (other, _) = sandbox.spawn("other")?;
    sandbox.await_file("other.entered")?;
    Ok(other)
}

/// `store_failure` from `daemon/status`, checked to carry no prompt,
/// handle or payload (design §7.5).
fn store_failure(sandbox: &Sandbox) -> TestResult<Value> {
    let status = sandbox.status()?;
    let failure = status["store_failure"].clone();
    let keys = failure.as_object().map(|object| {
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    });
    check(
        keys == Some(vec!["affected", "count", "kind", "scope", "since"]),
        || format!("unexpected store_failure: {status}"),
    )?;
    Ok(failure)
}

// ------------------------------------------------------------ receipts (row 1)

/// Design §7.2 row 1: a `spawn` receipt whose commit is not committed is
/// `store_error` with `commit_outcome: not_committed`; nothing latches, and
/// the same keyed request then commits exactly once. `store_failure.scope`
/// is `request`.
#[test]
fn s1_f12_receipt_not_committed_is_scoped() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[held("other", 1), completes("keyed", 1)]))?;
    let daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    // The second session's receipt was the first.
    sandbox.arm("store.commit.receipt", 2, "fail_io")?;
    let args = spawn_args("keyed", &["--idempotency-key", "key-1", "--handle", HANDLE]);
    let error = sandbox.refused(&args, "store_error")?;
    sandbox.ack(&daemon, "store.commit.receipt", 2, "fail_io")?;
    check(
        error["data"] == json!({"kind":"store_error","commit_outcome":"not_committed"}),
        || format!("unexpected refusal: {error}"),
    )?;
    let failure = store_failure(&sandbox)?;
    check(
        failure["kind"] == "commit_failed"
            && failure["scope"] == "request"
            && failure["count"] == 1
            && failure["affected"] == json!({"addresses":[],"count":0}),
        || format!("unexpected store_failure: {failure}"),
    )?;
    let receipt = sandbox.ok(&args)?;
    let replay = sandbox.ok(&args)?;
    check(receipt == replay, || {
        format!("the keyed replay differs: {receipt} vs {replay}")
    })?;
    let sessions: i64 = sandbox.query("SELECT count(*) FROM sessions")?;
    check(sessions == 2, || {
        format!("expected the other and one keyed session, found {sessions}")
    })?;
    let session = receipt["session_id"].as_str().ok_or("no session")?;
    let envelope = sandbox.wait(&format!("{session}/1"))?;
    check(envelope["state"] == "completed", || {
        format!("the keyed turn did not run: {envelope}")
    })?;
    scoped_end(daemon, &other)
}

/// Design §7.1 (report contradiction 11): a request the SQLite writer's
/// queue never took is not committed, so its receipt reports
/// `not_committed`, not `unknown`, and nothing latches.
#[test]
fn s1_f12_request_never_enqueued_is_not_committed() -> TestResult {
    let sandbox = Sandbox::new(&completes("after", 1))?;
    sandbox.count("store.request.not_enqueued")?;
    let daemon = sandbox.start()?;
    // An idle daemon sends nothing more to the writer: the spawn's receipt
    // commit is its next request.
    let next = sandbox.next_hit("store.request.not_enqueued")?;
    sandbox.arm("store.request.not_enqueued", next, "fail_io")?;
    let error = sandbox.refused(&spawn_args("lost", &[]), "store_error")?;
    sandbox.ack(&daemon, "store.request.not_enqueued", next, "fail_io")?;
    check(error["data"]["commit_outcome"] == "not_committed", || {
        format!("a request never enqueued is not committed: {error}")
    })?;
    sandbox.disarm("store.request.not_enqueued")?;
    let status = sandbox.status()?;
    check(
        status["health"] == "healthy" && status["store_failure"]["scope"] == "request",
        || format!("the failure was not scoped: {status}"),
    )?;
    let (session, _) = sandbox.spawn("after")?;
    let envelope = sandbox.wait(&format!("{session}/1"))?;
    check(envelope["state"] == "completed", || {
        format!("the daemon stopped serving: {envelope}")
    })?;
    daemon.stop_clean()
}

/// Design §7.1, characterization: a writer that is gone (`WriterLost`)
/// may have committed, so the receipt reports `unknown` with `retry:
/// same_key_only`, the daemon latches, and it exits 4.
#[test]
fn s1_f12_writer_lost_latches() -> TestResult {
    let sandbox = Sandbox::new(&completes("lost", 1))?;
    sandbox.count("store.writer.lost")?;
    let daemon = sandbox.start()?;
    let next = sandbox.next_hit("store.writer.lost")?;
    sandbox.arm("store.writer.lost", next, "fail_io")?;
    let error = sandbox.refused(&spawn_args("lost", &[]), "store_error")?;
    sandbox.ack(&daemon, "store.writer.lost", next, "fail_io")?;
    check(
        error["data"]
            == json!({"kind":"store_error","commit_outcome":"unknown","retry":"same_key_only"}),
        || format!("a lost writer's outcome is unknown: {error}"),
    )?;
    daemon.latched_exit()?;
    Ok(())
}

// ------------------------------------------------- close (rows 10 and 11)

/// Design §7.2 rows 10 and 11 [O1.D12]: a `Closing` commit that is not
/// committed is `store_error` with no state; a `Closed` commit that is not
/// committed keeps `closing` durable, so `resume` is refused, and a later
/// `close` completes. Nothing latches; the scopes are `request`, then
/// `session`.
#[test]
fn s1_f12_closing_and_closed_not_committed() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[held("other", 1), completes("first", 1)]))?;
    let daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    let (session, handle) = sandbox.spawn("first")?;
    sandbox.wait(&format!("{session}/1"))?;
    let close = ["close", &session, "--handle", &handle, "--json"];
    sandbox.arm("store.commit.closing", 1, "fail_io")?;
    let error = sandbox.refused(&close, "store_error")?;
    sandbox.ack(&daemon, "store.commit.closing", 1, "fail_io")?;
    check(error["data"]["commit_outcome"] == "not_committed", || {
        format!("unexpected Closing refusal: {error}")
    })?;
    let admission: String = sandbox.query(&format!(
        "SELECT admission FROM sessions WHERE id='{session}'"
    ))?;
    let status = sandbox.status()?;
    check(
        admission == "open"
            && status["sessions"]["closing"] == 0
            && status["store_failure"]["scope"] == "request",
        || format!("a failed Closing left state: {admission} {status}"),
    )?;
    sandbox.arm("store.commit.closed", 1, "fail_io")?;
    let error = sandbox.refused(&close, "store_error")?;
    sandbox.ack(&daemon, "store.commit.closed", 1, "fail_io")?;
    check(error["data"]["commit_outcome"] == "not_committed", || {
        format!("unexpected Closed refusal: {error}")
    })?;
    let admission: String = sandbox.query(&format!(
        "SELECT admission FROM sessions WHERE id='{session}'"
    ))?;
    check(admission == "closing", || {
        format!("closing is not durable: {admission}")
    })?;
    let failure = store_failure(&sandbox)?;
    check(
        failure["scope"] == "session"
            && failure["affected"] == json!({"addresses":[session.as_str()],"count":1}),
        || format!("unexpected store_failure: {failure}"),
    )?;
    sandbox.refused(
        &[
            "resume", &session, "--prompt", "second", "--handle", &handle, "--json",
        ],
        "session_closed",
    )?;
    let closed = sandbox.ok(&close)?;
    check(closed["state"] == "closed", || {
        format!("the later close did not complete: {closed}")
    })?;
    scoped_end(daemon, &other)
}

// ----------------------------------------- final shutdown (rows 14 and 15)

/// Design §7.2 row 14: the force closure pass's standalone
/// `session.closed` is not committed. The session counts in
/// `unclosed_sessions` (exit 4), and nothing latches: `store_failed: false`.
#[test]
fn s1_f12_force_closure_not_committed_counts_unclosed() -> TestResult {
    let sandbox = Sandbox::new(&completes("first", 1))?;
    let mut daemon = sandbox.start()?;
    sandbox.arm("core.run.settling", 1, "pause")?;
    let (session, _) = sandbox.spawn("first")?;
    // The turn's execution ended; its terminal commits after force, so the
    // closure pass closes the session alone.
    sandbox.ack(&daemon, "core.run.settling", 1, "pause")?;
    sandbox.arm("store.commit.session_closed", 1, "fail_io")?;
    sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
    sandbox.resume_point("core.run.settling", 1)?;
    sandbox.ack(&daemon, "store.commit.session_closed", 1, "fail_io")?;
    let status = daemon.exit(Duration::from_secs(20))?;
    let summary = daemon.summary()?;
    check(
        status.code() == Some(4)
            && summary["unclosed_sessions"] == 1
            && summary["store_failed"] == false,
        || format!("unexpected exit {status}: {summary}"),
    )?;
    let state: String = sandbox.query(&format!(
        "SELECT state FROM turns WHERE session_id='{session}' AND number=1"
    ))?;
    check(state == "completed", || format!("turn 1 is {state}"))?;
    sandbox.verify_anchors()
}

/// Design §7.2 row 15 [r3.11]: a forced terminal in final shutdown that is
/// not committed counts in `uncommitted_turns` (exit 4), with no latch and
/// no retry. With `store.commit.reply_lost` instead, the uncertain commit
/// latches: `store_failed: true`.
#[test]
fn s1_f12_forced_terminal_not_committed_in_shutdown() -> TestResult {
    for uncertain in [false, true] {
        let sandbox = Sandbox::new(&held("held", 1))?;
        sandbox.count("store.commit.reply_lost")?;
        let mut daemon = sandbox.start()?;
        sandbox.spawn("held")?;
        sandbox.await_file("held.entered")?;
        sandbox.arm("core.shutdown.before_forced_terminal", 1, "pause")?;
        sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
        sandbox.ack(&daemon, "core.shutdown.before_forced_terminal", 1, "pause")?;
        let (point, occurrence) = if uncertain {
            // The forced turn's `cancel.settled`, then its terminal.
            let next = sandbox.next_hit("store.commit.reply_lost")? + 1;
            ("store.commit.reply_lost", next)
        } else {
            ("store.commit.terminal", 1)
        };
        sandbox.arm(point, occurrence, "fail_io")?;
        sandbox.resume_point("core.shutdown.before_forced_terminal", 1)?;
        sandbox.ack(&daemon, point, occurrence, "fail_io")?;
        let status = daemon.exit(Duration::from_secs(20))?;
        let summary = daemon.summary()?;
        let expected = if uncertain {
            summary["store_failed"] == true
        } else {
            summary["uncommitted_turns"] == 1 && summary["store_failed"] == false
        };
        check(status.code() == Some(4) && expected, || {
            format!("uncertain {uncertain}: unexpected exit {status}: {summary}")
        })?;
        sandbox.verify_anchors()?;
    }
    Ok(())
}

// ------------------------------------------------------------- status (§7.5)

/// Design §7.5 [O1.D6]: `store_failure` reports the latest of two scoped
/// failures (`count: 2`, its scope and addresses) while `health` stays
/// `healthy`; neither the status nor the daemon's trace carries the
/// prompt, a payload or the handle.
#[test]
fn s1_f12_status_reports_latest_failure() -> TestResult {
    let prompt = "a-secret-prompt-5f1c";
    let sandbox = Sandbox::new(&completes(prompt, 1))?;
    let daemon = sandbox.start()?;
    let status = sandbox.status()?;
    check(
        status["health"] == "healthy" && status["store_failure"].is_null(),
        || format!("a fresh daemon reports a failure: {status}"),
    )?;
    sandbox.arm("store.commit.receipt", 1, "fail_io")?;
    sandbox.refused(&spawn_args(prompt, &["--handle", HANDLE]), "store_error")?;
    sandbox.ack(&daemon, "store.commit.receipt", 1, "fail_io")?;
    // Direct: a CLI call would start another daemon if this one had latched.
    let status = sandbox.status()?;
    check(status["health"] == "healthy", || {
        format!("the receipt failure latched: {status}")
    })?;
    let (session, handle) = sandbox.spawn(prompt)?;
    sandbox.wait(&format!("{session}/1"))?;
    sandbox.arm("store.commit.closed", 1, "fail_io")?;
    sandbox.refused(
        &["close", &session, "--handle", &handle, "--json"],
        "store_error",
    )?;
    let status = sandbox.status()?;
    let failure = store_failure(&sandbox)?;
    check(
        status["health"] == "healthy"
            && failure["kind"] == "commit_failed"
            && failure["scope"] == "session"
            && failure["count"] == 2
            && failure["since"].is_string()
            && failure["affected"] == json!({"addresses":[session.as_str()],"count":1}),
        || format!("unexpected status: {status}"),
    )?;
    let scanned = format!("{status}{}", daemon.trace());
    for secret in [prompt, HANDLE, handle.as_str()] {
        check(!scanned.contains(secret), || {
            format!("the status or trace carries {secret}")
        })?;
    }
    sandbox.ok(&["close", &session, "--handle", &handle, "--json"])?;
    daemon.stop_clean()
}

// ------------------------------------------------ queued cancel (rows 8, 9)

/// Design §7.2 row 8: a caller's `queued → cancelled` that is not committed
/// rolls back to `Waiting` with `store_error` (`not_committed`); nothing
/// latches, and the caller's retry commits it.
#[test]
fn s1_f12_queued_cancel_not_committed() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[
        held("other", 1),
        held("first", 1),
        completes("third", 3),
    ]))?;
    let daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    let (session, handle) = sandbox.spawn("first")?;
    sandbox.await_file("first.entered")?;
    sandbox.resume(&session, &handle, "second")?;
    sandbox.resume(&session, &handle, "third")?;
    let cancel = [
        "cancel", &session, "--turn", "2", "--handle", &handle, "--json",
    ];
    sandbox.arm("store.commit.cancel", 1, "fail_io")?;
    let error = sandbox.refused(&cancel, "store_error")?;
    sandbox.ack(&daemon, "store.commit.cancel", 1, "fail_io")?;
    check(error["data"]["commit_outcome"] == "not_committed", || {
        format!("unexpected cancel refusal: {error}")
    })?;
    let failure = store_failure(&sandbox)?;
    check(failure["scope"] == "request", || {
        format!("unexpected store_failure: {failure}")
    })?;
    let reply = sandbox.ok(&cancel)?;
    check(
        reply["state"] == "cancelled" && reply["already_terminal"] == false,
        || format!("the retried cancel did not commit: {reply}"),
    )?;
    sandbox.release("first")?;
    let third = sandbox.wait(&format!("{session}/3"))?;
    check(third["state"] == "completed", || {
        format!("the successor did not run: {third}")
    })?;
    let events = sandbox.events(&session)?;
    dense(&events)?;
    check(
        event_types(&events, 2) == ["turn.queued", "turn.ended"],
        || format!("turn 2 events: {events:?}"),
    )?;
    scoped_end(daemon, &other)
}
