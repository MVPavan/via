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

    /// Anchors committed for `session`'s turn `turn`: zero when nothing
    /// launched.
    fn anchors(&self, session: &str, turn: u32) -> TestResult<i64> {
        self.query(&format!(
            "SELECT count(*) FROM anchors WHERE owner_session='{session}' AND owner_turn={turn}"
        ))
    }

    /// Waits until the turn's acceptance is durable.
    fn await_accepted(&self, session: &str, turn: u32) -> TestResult {
        let sql = format!(
            "SELECT count(*) FROM turns WHERE session_id='{session}' AND number={turn}
             AND accepted_at IS NOT NULL"
        );
        wait_until("the acceptance is durable", Duration::from_secs(20), || {
            self.query::<i64>(&sql).is_ok_and(|count| count == 1)
        })
    }

    /// Runs one CLI call on its own thread, for a call that blocks.
    fn background(&self, args: &[&str]) -> thread::JoinHandle<Result<Captured, String>> {
        let mut command = self.command();
        command.args(args);
        thread::spawn(move || {
            run_command(&mut command, Duration::from_secs(60)).map_err(|error| error.to_string())
        })
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

fn text(content: &str) -> Value {
    json!({"action":"emit","message":{"type":"assistant_text","text":content}})
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

/// A background call's JSON result.
fn joined(call: thread::JoinHandle<Result<Captured, String>>) -> TestResult<Value> {
    let captured = call.join().map_err(|_| "the background call panicked")??;
    if !captured.status.success() {
        return Err(format!(
            "a background call exited {}: {}",
            captured.status,
            String::from_utf8_lossy(&captured.stderr)
        )
        .into());
    }
    Ok(serde_json::from_slice(&captured.stdout)?)
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
    scoped_end(daemon, &other)?;
    close_pass_cancel_retried(false)?;
    close_pass_cancel_retried(true)
}

/// Design §7.2 row 9: the close pass's `queued → cancelled` of turn 2 is
/// not committed. Retried once, holding the session head, it commits and
/// the close completes with nothing latched; failing `persistent`ly, the
/// retry fails too, so the daemon latches (the escalation) and the close
/// replies `store_error`.
fn close_pass_cancel_retried(persistent: bool) -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[held("first", 1)]))?;
    let daemon = sandbox.start()?;
    let (session, handle) = sandbox.spawn("first")?;
    sandbox.await_file("first.entered")?;
    sandbox.resume(&session, &handle, "second")?;
    let action = if persistent {
        "fail_io_persist"
    } else {
        "fail_io"
    };
    // Turn 1 is running: its terminal is `store.commit.terminal`, so turn 2's
    // cancellation is the first `store.commit.cancel`.
    sandbox.arm("store.commit.cancel", 1, action)?;
    let close = [
        "close", &session, "--mode", "force", "--handle", &handle, "--json",
    ];
    if persistent {
        sandbox.refused(&close, "store_error")?;
        sandbox.ack(&daemon, "store.commit.cancel", 2, "fail_io")?;
        daemon.latched_exit()?;
        return Ok(());
    }
    let closed = sandbox.ok(&close)?;
    sandbox.ack(&daemon, "store.commit.cancel", 1, "fail_io")?;
    check(
        closed["state"] == "closed"
            && closed["cancelled_turns"] == json!([format!("{session}/1"), format!("{session}/2")]),
        || format!("the close did not complete: {closed}"),
    )?;
    let events = sandbox.events(&session)?;
    dense(&events)?;
    check(
        event_types(&events, 2) == ["turn.queued", "turn.ended"],
        || format!("turn 2 events: {events:?}"),
    )?;
    let status = sandbox.status()?;
    check(
        status["health"] == "healthy"
            && status["store_failure"]["scope"] == "turn"
            && status["store_failure"]["affected"]["addresses"] == json!([format!("{session}/2")]),
        || format!("the retried cancellation latched: {status}"),
    )?;
    daemon.stop_clean()
}

// ------------------------------------------- submission and events (rows 2, 5)

/// Design §7.2 row 2: a submission that is not committed fails the turn
/// `failed(store)` through `commit_submit_failed`, with `submitted_at` and
/// `cancel: null`. Nothing launched, the connection permit is free again,
/// and the successor runs.
#[test]
fn s1_f12_submission_not_committed_fails_turn_without_launch() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[held("other", 1), completes("second", 2)]))?;
    let daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    // The second session's submission was the first of each point.
    sandbox.arm("core.submit.before_commit", 2, "pause")?;
    let (session, handle) = sandbox.spawn("first")?;
    sandbox.ack(&daemon, "core.submit.before_commit", 2, "pause")?;
    sandbox.resume(&session, &handle, "second")?;
    sandbox.arm("store.commit.submission", 2, "fail_io")?;
    sandbox.resume_point("core.submit.before_commit", 2)?;
    sandbox.ack(&daemon, "store.commit.submission", 2, "fail_io")?;
    let first = sandbox.wait(&format!("{session}/1"))?;
    check(
        first["state"] == "failed"
            && first["failure"]["class"] == "store"
            && first["timestamps"]["submitted_at"].is_string()
            && first["cancel"].is_null(),
        || format!("unexpected turn 1: {first}"),
    )?;
    check(sandbox.anchors(&session, 1)? == 0, || {
        "turn 1 launched".to_owned()
    })?;
    let second = sandbox.wait(&format!("{session}/2"))?;
    check(second["state"] == "completed", || {
        format!("the successor did not run: {second}")
    })?;
    let events = sandbox.events(&session)?;
    dense(&events)?;
    check(
        event_types(&events, 1) == ["turn.queued", "turn.submitted", "turn.ended"],
        || format!("turn 1 events: {events:?}"),
    )?;
    // Only the second session's group holds a permit.
    wait_until("the permit is free", Duration::from_secs(20), || {
        sandbox
            .status()
            .is_ok_and(|status| status["connections"]["in_use"] == 1)
    })?;
    let failure = store_failure(&sandbox)?;
    check(
        failure["scope"] == "turn"
            && failure["affected"]["addresses"] == json!([format!("{session}/1")]),
        || format!("unexpected store_failure: {failure}"),
    )?;
    scoped_end(daemon, &other)
}

/// Design §7.2 row 5, §7.1: an `assistant.text` commit that is not
/// committed sets a stop order with cause `store`; the turn ends
/// `failed(store)` with `cancel` evidence and no later events, its group is
/// stopped, and `turn.ended` reuses the failed event's sequence number, so
/// the session's events stay dense.
#[test]
fn s1_f12_event_not_committed_stops_turn_and_reuses_seq() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[
        held("other", 1),
        script(
            "first",
            1,
            vec![
                accepted(1),
                gate("accepted"),
                text("lost"),
                gate("first"),
                terminal(1),
            ],
        ),
    ]))?;
    sandbox.count("store.commit.event")?;
    let daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    let (session, _) = sandbox.spawn("first")?;
    sandbox.await_file("accepted.entered")?;
    sandbox.await_accepted(&session, 1)?;
    let next = sandbox.next_hit("store.commit.event")?;
    sandbox.arm("store.commit.event", next, "fail_io")?;
    sandbox.release("accepted")?;
    sandbox.ack(&daemon, "store.commit.event", next, "fail_io")?;
    let first = sandbox.wait(&format!("{session}/1"))?;
    check(
        first["state"] == "failed"
            && first["failure"]["class"] == "store"
            && first["stop_reason"] == "error"
            && first["cancel"]["requested_at"].is_string()
            && first["cancel"]["cleanup"] == "quiescent",
        || format!("unexpected turn 1: {first}"),
    )?;
    let events = sandbox.events(&session)?;
    dense(&events)?;
    check(
        event_types(&events, 1)
            == [
                "turn.queued",
                "turn.submitted",
                "turn.started",
                "turn.ended",
            ],
        || format!("turn 1 events: {events:?}"),
    )?;
    let failure = store_failure(&sandbox)?;
    check(failure["scope"] == "turn", || {
        format!("unexpected store_failure: {failure}")
    })?;
    scoped_end(daemon, &other)
}

/// Design §7.2 row 5, §2 durability: a caller's cancel whose
/// `cancel.requested` commit is not committed gets `store_error`; the order
/// is upgraded to cause `store`, and the turn ends `failed(store)` with the
/// order's `cancel` object.
#[test]
fn s1_f12_cancel_requested_not_committed() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[held("other", 1), held("first", 1)]))?;
    sandbox.count("store.commit.event")?;
    let daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    let (session, handle) = sandbox.spawn("first")?;
    sandbox.await_file("first.entered")?;
    sandbox.await_accepted(&session, 1)?;
    let next = sandbox.next_hit("store.commit.event")?;
    sandbox.arm("store.commit.event", next, "fail_io")?;
    let error = sandbox.refused(
        &["cancel", &session, "--handle", &handle, "--json"],
        "store_error",
    )?;
    sandbox.ack(&daemon, "store.commit.event", next, "fail_io")?;
    check(error["data"] == json!({"kind":"store_error"}), || {
        format!("unexpected cancel refusal: {error}")
    })?;
    let first = sandbox.wait(&format!("{session}/1"))?;
    check(
        first["state"] == "failed"
            && first["failure"]["class"] == "store"
            && first["cancel"]["requested_at"].is_string(),
        || format!("unexpected turn 1: {first}"),
    )?;
    let events = sandbox.events(&session)?;
    dense(&events)?;
    check(
        event_types(&events, 1)
            == [
                "turn.queued",
                "turn.submitted",
                "turn.started",
                "turn.ended",
            ],
        || format!("turn 1 events: {events:?}"),
    )?;
    scoped_end(daemon, &other)
}

// ------------------------------------------ terminals and retries (rows 7, 9)

/// Design §7.2 row 7 [r3.7, r3.13]: a natural terminal that is not
/// committed is retried once with the same content and sequence number;
/// the retry commits the vendor's result, and nothing latches.
#[test]
fn s1_f12_terminal_retry_once() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[held("other", 1), completes("first", 1)]))?;
    let daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    sandbox.arm("store.commit.terminal", 1, "fail_io")?;
    let (session, _) = sandbox.spawn("first")?;
    sandbox.ack(&daemon, "store.commit.terminal", 1, "fail_io")?;
    let first = sandbox.wait(&format!("{session}/1"))?;
    check(
        first["state"] == "completed" && first["final_text"] == "done",
        || format!("the retry lost the vendor's result: {first}"),
    )?;
    let events = sandbox.events(&session)?;
    dense(&events)?;
    let ended = events
        .iter()
        .find(|event| event["type"] == "turn.ended")
        .ok_or("no turn.ended")?;
    check(first["events"]["last_seq"] == ended["seq"], || {
        format!("the envelope's range ends elsewhere: {first}")
    })?;
    let failure = store_failure(&sandbox)?;
    check(
        failure["kind"] == "commit_failed" && failure["scope"] == "turn" && failure["count"] == 1,
        || format!("unexpected store_failure: {failure}"),
    )?;
    scoped_end(daemon, &other)
}

/// Design §7.2 escalation: with `store.commit.fail_persistent` armed at a
/// turn event, the event is not committed and the resolution write fails
/// too, so the daemon latches: `health: store_failed`, `store_failure`
/// scope `daemon`, and exit 4. The same for a natural terminal whose one
/// retry fails.
#[test]
fn s1_f12_escalation_latches() -> TestResult {
    for at_terminal in [false, true] {
        let sandbox = Sandbox::new(&script(
            "first",
            1,
            vec![
                accepted(1),
                gate("accepted"),
                text("lost"),
                gate("first"),
                terminal(1),
            ],
        ))?;
        sandbox.count("store.commit.fail_persistent")?;
        let daemon = sandbox.start()?;
        if at_terminal {
            sandbox.arm("core.run.settling", 1, "pause")?;
        }
        let (session, _) = sandbox.spawn("first")?;
        sandbox.await_file("accepted.entered")?;
        sandbox.await_accepted(&session, 1)?;
        if at_terminal {
            sandbox.release("accepted")?;
            sandbox.release("first")?;
            sandbox.ack(&daemon, "core.run.settling", 1, "pause")?;
        }
        let next = sandbox.next_hit("store.commit.fail_persistent")?;
        sandbox.arm("store.commit.fail_persistent", next, "fail_io_persist")?;
        if at_terminal {
            sandbox.resume_point("core.run.settling", 1)?;
        } else {
            sandbox.release("accepted")?;
        }
        sandbox.ack(&daemon, "store.commit.fail_persistent", next + 1, "fail_io")?;
        // Design §7.5, served in the diagnostic window (§7.4).
        wait_until("the latch", Duration::from_secs(5), || {
            sandbox
                .status()
                .is_ok_and(|status| status["health"] == "store_failed")
        })?;
        let failure = store_failure(&sandbox)?;
        check(
            failure["scope"] == "daemon" && failure["kind"] == "commit_failed",
            || format!("at_terminal {at_terminal}: {failure}"),
        )?;
        daemon.latched_exit()?;
        let state: String = sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{session}' AND number=1"
        ))?;
        check(state == "running", || {
            format!("at_terminal {at_terminal}: nothing committed, yet turn 1 is {state}")
        })?;
    }
    Ok(())
}

/// Design §7.2 same-sequence retries [r3.7, r4.9]: while a natural
/// terminal's retry is paused at `core.retry.before`, holding the session
/// head, a `resume` receipt of the same session blocks on the head
/// (`core.head.contended`). On release the terminal commits at the failed
/// sequence number, then the receipt takes the next one. The same for the
/// close pass's cancellation (row 9) against a caller's cancel.
#[test]
fn s1_f12_retry_holds_head_against_competing_writer() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[completes("first", 1), completes("second", 2)]))?;
    sandbox.count("core.head.contended")?;
    let daemon = sandbox.start()?;
    sandbox.arm("store.commit.terminal", 1, "fail_io")?;
    sandbox.arm("core.retry.before", 1, "pause")?;
    let (session, handle) = sandbox.spawn("first")?;
    sandbox.ack(&daemon, "core.retry.before", 1, "pause")?;
    let contended = sandbox.next_hit("core.head.contended")?;
    sandbox.arm("core.head.contended", contended, "fail_io")?;
    let resume = sandbox.background(&[
        "resume", &session, "--prompt", "second", "--handle", &handle, "--json",
    ]);
    sandbox.ack(&daemon, "core.head.contended", contended, "fail_io")?;
    sandbox.resume_point("core.retry.before", 1)?;
    joined(resume)?;
    let second = sandbox.wait(&format!("{session}/2"))?;
    check(second["state"] == "completed", || {
        format!("turn 2 did not run: {second}")
    })?;
    let events = sandbox.events(&session)?;
    dense(&events)?;
    let order: Vec<(u64, String)> = events
        .iter()
        .filter_map(|event| Some((event["turn"].as_u64()?, event["type"].as_str()?.to_owned())))
        .collect();
    let ended = order
        .iter()
        .position(|entry| *entry == (1, "turn.ended".to_owned()));
    let queued = order
        .iter()
        .position(|entry| *entry == (2, "turn.queued".to_owned()));
    check(
        ended.is_some() && queued.is_some() && ended < queued,
        || format!("the receipt did not wait for the retry: {order:?}"),
    )?;
    check(sandbox.status()?["health"] == "healthy", || {
        "the retry latched".to_owned()
    })?;
    daemon.stop_clean()?;
    cancel_retry_holds_head()
}

/// [`s1_f12_retry_holds_head_against_competing_writer`]'s row 9 half: the
/// close pass's cancellation of turn 2 is retried holding the head while a
/// caller's cancel of turn 3 waits for it.
fn cancel_retry_holds_head() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[held("first", 1)]))?;
    sandbox.count("core.head.contended")?;
    let daemon = sandbox.start()?;
    let (session, handle) = sandbox.spawn("first")?;
    sandbox.await_file("first.entered")?;
    sandbox.resume(&session, &handle, "second")?;
    sandbox.resume(&session, &handle, "third")?;
    sandbox.arm("store.commit.cancel", 1, "fail_io")?;
    sandbox.arm("core.retry.before", 1, "pause")?;
    let close = sandbox.background(&[
        "close", &session, "--mode", "force", "--handle", &handle, "--json",
    ]);
    sandbox.ack(&daemon, "core.retry.before", 1, "pause")?;
    let contended = sandbox.next_hit("core.head.contended")?;
    sandbox.arm("core.head.contended", contended, "fail_io")?;
    let cancel = sandbox.background(&[
        "cancel", &session, "--turn", "3", "--handle", &handle, "--json",
    ]);
    sandbox.ack(&daemon, "core.head.contended", contended, "fail_io")?;
    sandbox.resume_point("core.retry.before", 1)?;
    let cancelled = joined(cancel)?;
    check(cancelled["state"] == "cancelled", || {
        format!("the caller's cancel failed: {cancelled}")
    })?;
    let closed = joined(close)?;
    check(closed["state"] == "closed", || {
        format!("the close failed: {closed}")
    })?;
    let events = sandbox.events(&session)?;
    dense(&events)?;
    let ends: Vec<u64> = events
        .iter()
        .filter(|event| event["type"] == "turn.ended")
        .filter_map(|event| event["turn"].as_u64())
        .collect();
    check(ends == [1, 2, 3], || {
        format!("turn 3's cancel did not wait for the retry: {ends:?}")
    })?;
    check(sandbox.status()?["health"] == "healthy", || {
        "the retry latched".to_owned()
    })?;
    daemon.stop_clean()
}

/// Design §7.2 row 9 [r3.6]: under force, the last queued turn's
/// cancellation carries `session.closed` in the same transaction. With
/// `store.commit.rider` once, that transaction rolls back; the retry, holding
/// the head, commits both and nothing latches. Failing persistently, the
/// retry fails and the daemon latches. With another turn of the session
/// unfinished at the retry, the cancellation commits alone and the session
/// counts in `unclosed_sessions`.
#[test]
fn s1_f12_force_rider_rollback_retries() -> TestResult {
    for variant in ["once", "persistent", "unfinished"] {
        let mut sandbox = Sandbox::new(&scripts(&[held("holder", 1)]))?;
        // One permit: the second session's turn waits for it, queued.
        sandbox
            .env
            .push(("VIA_TEST_CONNECTION_SLOTS", "1".to_owned()));
        sandbox.count("core.dispatch.awaiting_slot")?;
        let mut daemon = sandbox.start()?;
        sandbox.spawn("holder")?;
        sandbox.await_file("holder.entered")?;
        let (session, _) = sandbox.spawn("waiting")?;
        wait_until("the turn waits for a slot", Duration::from_secs(20), || {
            sandbox
                .next_hit("core.dispatch.awaiting_slot")
                .is_ok_and(|next| next > 1)
        })?;
        let action = if variant == "persistent" {
            "fail_io_persist"
        } else {
            "fail_io"
        };
        sandbox.arm("store.commit.rider", 1, action)?;
        if variant == "unfinished" {
            sandbox.arm("core.retry.before", 1, "pause")?;
        }
        sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
        sandbox.ack(&daemon, "store.commit.rider", 1, "fail_io")?;
        if variant == "unfinished" {
            sandbox.ack(&daemon, "core.retry.before", 1, "pause")?;
            // A durable turn the daemon does not know: Store refuses the
            // retried close while it is unfinished.
            let store = rusqlite::Connection::open(sandbox.state.join("store.sqlite3"))?;
            store.execute(
                "INSERT INTO turns(session_id,number,prompt,effective,state,queued_at,queued_seq)
                 VALUES(?1,2,'unregistered','{}','queued','2026-01-01T00:00:00.000Z',1000)",
                [session.as_str()],
            )?;
            drop(store);
            sandbox.resume_point("core.retry.before", 1)?;
        }
        let status = daemon.exit(Duration::from_secs(20))?;
        let summary = daemon.summary()?;
        let turn: String = sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{session}' AND number=1"
        ))?;
        let closed: i64 = sandbox.query(&format!(
            "SELECT count(*) FROM sessions WHERE id='{session}' AND state='closed'"
        ))?;
        let expected = match variant {
            "once" => {
                status.code() == Some(0)
                    && summary["store_failed"] == false
                    && turn == "cancelled"
                    && closed == 1
            }
            "persistent" => {
                status.code() == Some(4) && summary["store_failed"] == true && turn == "queued"
            }
            _ => {
                status.code() == Some(4)
                    && summary["store_failed"] == false
                    && summary["unclosed_sessions"] == 1
                    && turn == "cancelled"
                    && closed == 0
            }
        };
        check(expected, || {
            format!("{variant}: exit {status}, turn 1 {turn}, closed {closed}: {summary}")
        })?;
        sandbox.verify_anchors()?;
    }
    Ok(())
}

// ------------------------------------- Host journal and raw evidence (3, 4, 6)

/// `pid` is live: neither gone nor a zombie awaiting its reaper.
fn process_live(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|text| {
            let state = text.get(text.rfind(')')? + 2..)?.chars().next()?;
            Some(state != 'Z' && state != 'X')
        })
        .unwrap_or(false)
}

/// Design §7.2 row 3 [O1.D10]: an anchor intent that is not committed
/// starts no process; the turn ends `failed(store)` with `requested` and
/// `quiescent` evidence, and the session's next turn launches.
#[test]
fn s1_f12_anchor_intent_not_committed() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[
        held("other", 1),
        script("first", 1, vec![gate("first"), accepted(1), terminal(1)]),
        completes("second", 2),
    ]))?;
    let daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    // The second session's anchor intent was the first.
    sandbox.arm("store.journal.anchor_intent", 2, "fail_io")?;
    let (session, handle) = sandbox.spawn("first")?;
    sandbox.ack(&daemon, "store.journal.anchor_intent", 2, "fail_io")?;
    let first = sandbox.wait(&format!("{session}/1"))?;
    check(
        first["state"] == "failed"
            && first["failure"]["class"] == "store"
            && first["stop_reason"] == "error"
            && first["cancel"]["outcome"] == "requested"
            && first["cancel"]["cleanup"] == "quiescent",
        || format!("unexpected turn 1: {first}"),
    )?;
    check(
        sandbox.anchors(&session, 1)? == 0 && !sandbox.sync.join("first.entered").exists(),
        || "turn 1 launched".to_owned(),
    )?;
    sandbox.resume(&session, &handle, "second")?;
    let second = sandbox.wait(&format!("{session}/2"))?;
    check(second["state"] == "completed", || {
        format!("the next turn did not launch: {second}")
    })?;
    dense(&sandbox.events(&session)?)?;
    let failure = store_failure(&sandbox)?;
    check(
        failure["kind"] == "journal_failed" && failure["scope"] == "turn",
        || format!("unexpected store_failure: {failure}"),
    )?;
    scoped_end(daemon, &other)
}

/// Design §7.2 row 4 [O1.D10, r3.12]: an anchor identified, ARM intent or
/// vendor facts write that is not committed stops the group through the
/// live control, from the identity Host recorded before the commit, and
/// proves it absent: the turn ends `failed(store)` with `cancel` evidence
/// and cleanup `quiescent`, and the session's next turn launches. The
/// outer harness proves every anchor's group absent at the end.
#[test]
fn s1_f12_host_journal_failure_stops_group() -> TestResult {
    for point in [
        "store.journal.identified",
        "store.journal.arm_intent",
        "store.journal.vendor_facts",
    ] {
        host_journal_failure_stops_group(point).map_err(|error| format!("{point}: {error}"))?;
    }
    Ok(())
}

fn host_journal_failure_stops_group(point: &str) -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[
        held("other", 1),
        script(
            "first",
            1,
            vec![
                json!({"action":"report_pids"}),
                accepted(1),
                gate("first"),
                terminal(1),
            ],
        ),
        completes("second", 2),
    ]))?;
    let daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    // The second session's write at `point` was the first.
    sandbox.arm(point, 2, "fail_io")?;
    let (session, handle) = sandbox.spawn("first")?;
    sandbox.ack(&daemon, point, 2, "fail_io")?;
    let first = sandbox.wait(&format!("{session}/1"))?;
    check(
        first["state"] == "failed"
            && first["failure"]["class"] == "store"
            && first["cancel"]["requested_at"].is_string()
            && first["cancel"]["cleanup"] == "quiescent",
        || format!("unexpected turn 1: {first}"),
    )?;
    // The vendor, if it ran, is gone.
    let agent = sandbox.sync.join("agent.pid");
    if agent.exists() {
        let pid: u32 = fs::read_to_string(&agent)?.trim().parse()?;
        check(!process_live(pid), || "the vendor survived".to_owned())?;
    }
    sandbox.resume(&session, &handle, "second")?;
    let second = sandbox.wait(&format!("{session}/2"))?;
    check(second["state"] == "completed", || {
        format!("the next turn did not launch: {second}")
    })?;
    let failure = store_failure(&sandbox)?;
    check(
        failure["kind"] == "journal_failed" && failure["scope"] == "turn",
        || format!("unexpected store_failure: {failure}"),
    )?;
    scoped_end(daemon, &other)
}

/// Design §7.2 row 6 [O1.D11, r3.10]: a raw append that fails fails the
/// connection; the group is force-closed and proved absent, and the
/// terminal is `failed(store)` with `raw_log.incomplete` sequenced just
/// before `turn.ended` in the same transaction, the `raw_log_incomplete`
/// warning and the cleanup evidence.
#[test]
fn s1_f12_raw_failure_records_incomplete() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[
        held("other", 1),
        script(
            "first",
            1,
            vec![
                accepted(1),
                gate("accepted"),
                text("lost"),
                gate("first"),
                terminal(1),
            ],
        ),
    ]))?;
    sandbox.count("raw.append.fail")?;
    let daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    let (session, _) = sandbox.spawn("first")?;
    sandbox.await_file("accepted.entered")?;
    sandbox.await_accepted(&session, 1)?;
    let next = sandbox.next_hit("raw.append.fail")?;
    sandbox.arm("raw.append.fail", next, "fail_io")?;
    sandbox.release("accepted")?;
    sandbox.ack(&daemon, "raw.append.fail", next, "fail_io")?;
    let first = sandbox.wait(&format!("{session}/1"))?;
    let warned = first["warnings"]
        .as_array()
        .is_some_and(|warnings| warnings.iter().any(|w| w["code"] == "raw_log_incomplete"));
    check(
        first["state"] == "failed"
            && first["failure"]["class"] == "store"
            && first["cancel"]["cleanup"] == "quiescent"
            && warned,
        || format!("unexpected turn 1: {first}"),
    )?;
    let events = sandbox.events(&session)?;
    dense(&events)?;
    check(
        event_types(&events, 1)
            == [
                "turn.queued",
                "turn.submitted",
                "turn.started",
                "raw_log.incomplete",
                "turn.ended",
            ],
        || format!("turn 1 events: {events:?}"),
    )?;
    let failure = store_failure(&sandbox)?;
    check(
        failure["kind"] == "raw_failed" && failure["scope"] == "turn",
        || format!("unexpected store_failure: {failure}"),
    )?;
    scoped_end(daemon, &other)
}

// ------------------------------------------------ re-probe proofs (rows 4, 12)

/// Design §7.2 rows 4 and 12 [s1.6]: an anchor identified write that is
/// not committed, with the anchor held at `host.anchor.before_eof_cleanup`,
/// leaves absence unproven: the slot stays held
/// (`connections.held_unproven`). After release, re-probe commits the
/// proof with the identity Host kept in memory and frees the slot. The
/// proof's own commit: not committed once, it is retried on the next pass
/// and nothing latches; uncertain (the commit outlives the pass), the
/// daemon latches.
#[test]
fn s1_f12_host_journal_failure_unproven_slot_reprobed() -> TestResult {
    for proof in ["commits", "not_committed", "uncertain"] {
        unproven_slot_reprobed(proof).map_err(|error| format!("{proof}: {error}"))?;
    }
    Ok(())
}

fn unproven_slot_reprobed(proof: &str) -> TestResult {
    let sandbox = Sandbox::new(&script(
        "first",
        1,
        vec![accepted(1), gate("first"), terminal(1)],
    ))?;
    let daemon = sandbox.start()?;
    let eof_cleanup = "host.anchor.before_eof_cleanup";
    sandbox.arm(eof_cleanup, 1, "pause")?;
    sandbox.arm("store.journal.identified", 1, "fail_io")?;
    let (session, _) = sandbox.spawn("first")?;
    sandbox.ack(&daemon, "store.journal.identified", 1, "fail_io")?;
    let first = sandbox.wait(&format!("{session}/1"))?;
    check(
        first["state"] == "failed"
            && first["failure"]["class"] == "store"
            && first["cancel"]["cleanup"] == "uncertain",
        || format!("unexpected turn 1: {first}"),
    )?;
    let held = sandbox.status()?;
    check(held["connections"]["held_unproven"] == 1, || {
        format!("the slot is not held: {held}")
    })?;
    match proof {
        "not_committed" => sandbox.arm("store.journal.absence", 1, "fail_io")?,
        "uncertain" => sandbox.arm("store.journal.absence", 1, "pause")?,
        _ => {}
    }
    sandbox.process_ack(eof_cleanup, 1, "pause")?;
    sandbox.resume_point(eof_cleanup, 1)?;
    sandbox.disarm(eof_cleanup)?;
    if proof == "uncertain" {
        sandbox.ack(&daemon, "store.journal.absence", 1, "pause")?;
        // The proof's commit outlives the pass's bound: uncertain. The
        // latch shows in `daemon/status` while the window serves, and the
        // socket is gone after it.
        wait_until("the daemon latches", Duration::from_secs(20), || {
            !sandbox.runtime.join("via.sock").exists()
                || sandbox
                    .status()
                    .is_ok_and(|status| status["health"] == "store_failed")
        })?;
        sandbox.resume_point("store.journal.absence", 1)?;
        daemon.latched_exit()?;
        return Ok(());
    }
    if proof == "not_committed" {
        sandbox.ack(&daemon, "store.journal.absence", 1, "fail_io")?;
    }
    wait_until(
        "the re-probe frees the slot",
        Duration::from_secs(30),
        || {
            sandbox
                .status()
                .is_ok_and(|status| status["connections"]["held_unproven"] == 0)
        },
    )?;
    let proved: i64 = sandbox.query(&format!(
        "SELECT count(*) FROM anchors WHERE owner_session='{session}'
         AND pgid IS NOT NULL AND absence_time IS NOT NULL"
    ))?;
    check(proved == 1, || {
        "the proof was not committed with the identity".to_owned()
    })?;
    let status = sandbox.status()?;
    check(status["health"] == "healthy", || {
        format!("a scoped failure latched: {status}")
    })?;
    if proof == "not_committed" {
        // The identified write's failure, then the proof's.
        let failure = store_failure(&sandbox)?;
        check(
            failure["kind"] == "journal_failed"
                && failure["scope"] == "session"
                && failure["count"] == 2,
            || format!("unexpected store_failure: {failure}"),
        )?;
    }
    daemon.stop_clean()
}

// ------------------------------------------- reads and corruption (§7.3)

/// The lowered read-failure streak (design §11: `VIA_TEST_READ_FAILURE_MS`).
const READ_STREAK_MS: &str = "1000";

/// The failure message of a turn failed by the read streak.
const READ_FAILED: &str = "the turn's queued state could not be read";

/// Design §7.3 [r3.8]: the dispatcher cannot complete its head's read
/// sequence (persistent `store.read.dispatch`). After the lowered streak it
/// fails the head turn `failed(store)` through row 2's resolution write,
/// without launch; a drain issued while it retries still finishes, exit 0.
/// With the resolution write also failing (`commit_submit_failed` hits
/// `store.commit.terminal`), the daemon latches (escalation).
#[test]
fn s1_f12_dispatcher_reads_fail_then_turn_fails() -> TestResult {
    dispatcher_reads_fail("store.read.dispatch", false)?;
    dispatcher_reads_fail("store.read.dispatch", true)
}

/// Design §7.3 [r3.8]: only the queued-row read fails (persistent
/// `store.read.queued_turn`) while the predecessor reads succeed. The
/// streak does not reset on those successful reads: the head turn still
/// fails at the lowered deadline, without launch.
#[test]
fn s1_f12_selective_queued_row_read_failure() -> TestResult {
    dispatcher_reads_fail("store.read.queued_turn", false)
}

/// Turn 1 of a new session meets persistent `point` failures once its
/// dispatcher starts (held at `daemon.dispatcher.before_start` while the
/// failure is armed); a second session's running turn is unaffected.
fn dispatcher_reads_fail(point: &str, resolution_fails: bool) -> TestResult {
    let mut sandbox = Sandbox::new(&scripts(&[held("other", 1)]))?;
    sandbox
        .env
        .push(("VIA_TEST_READ_FAILURE_MS", READ_STREAK_MS.to_owned()));
    sandbox.count(point)?;
    if point != "store.read.dispatch" {
        sandbox.count("store.read.dispatch")?;
    }
    let mut daemon = sandbox.start()?;
    let other = other_session(&sandbox)?;
    let start = "daemon.dispatcher.before_start";
    // The second session's dispatcher start is the second hit.
    sandbox.arm(start, 2, "pause")?;
    let (session, _) = sandbox.spawn("first")?;
    sandbox.ack(&daemon, start, 2, "pause")?;
    let first_failure = sandbox.next_hit(point)?;
    let reads_before = sandbox.next_hit("store.read.dispatch")?;
    sandbox.arm(point, first_failure, "fail_io_persist")?;
    if resolution_fails {
        // Nothing has committed a terminal yet: the resolution write is first.
        sandbox.arm("store.commit.terminal", 1, "fail_io_persist")?;
    }
    sandbox.resume_point(start, 2)?;
    sandbox.ack(&daemon, point, first_failure, "fail_io")?;
    if resolution_fails {
        sandbox.ack(&daemon, "store.commit.terminal", 1, "fail_io")?;
        sandbox.disarm(point)?;
        sandbox.disarm("store.commit.terminal")?;
        daemon.latched_exit()?;
        let state: String = sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{session}' AND number=1"
        ))?;
        check(state != "failed", || {
            "the failed resolution write was recorded".to_owned()
        })?;
        return check(sandbox.anchors(&session, 1)? == 0, || {
            "turn 1 launched".to_owned()
        });
    }
    // Issued while the dispatcher retries: the streak bounds the drain.
    sandbox.ok(&["daemon", "stop", "--drain", "--json"])?;
    let first = sandbox.wait(&format!("{session}/1"))?;
    check(
        first["state"] == "failed"
            && first["failure"]["class"] == "store"
            && first["failure"]["message"] == READ_FAILED
            && first["timestamps"]["submitted_at"].is_string()
            && first["cancel"].is_null(),
        || format!("unexpected turn 1: {first}"),
    )?;
    check(sandbox.anchors(&session, 1)? == 0, || {
        "turn 1 launched".to_owned()
    })?;
    let events = sandbox.events(&session)?;
    dense(&events)?;
    check(
        event_types(&events, 1) == ["turn.queued", "turn.submitted", "turn.ended"],
        || format!("turn 1 events: {events:?}"),
    )?;
    if point != "store.read.dispatch" {
        // Each retry read the predecessors successfully first.
        let reads = sandbox.next_hit("store.read.dispatch")? - reads_before;
        check(reads >= 4, || {
            format!("only {reads} dispatch reads during the streak")
        })?;
    }
    let status = sandbox.status()?;
    let failure = store_failure(&sandbox)?;
    check(
        status["health"] == "healthy"
            && failure["kind"] == "read_failed"
            && failure["scope"] == "turn"
            && failure["affected"]["addresses"] == json!([format!("{session}/1")]),
        || format!("unexpected status: {status}"),
    )?;
    sandbox.disarm(point)?;
    sandbox.release("other")?;
    let envelope = sandbox.wait(&format!("{other}/1"))?;
    check(envelope["state"] == "completed", || {
        format!("the second session's turn was affected: {envelope}")
    })?;
    let status = daemon.exit(Duration::from_secs(20))?;
    check(status.code() == Some(0), || {
        format!("the drain exited {status}: {}", daemon.trace())
    })?;
    sandbox.verify_anchors()
}

/// Design §7.1, §7.3: SQLite-level corruption on a read latches (exit 4),
/// unlike any other read failure. A request's read (`events`) replies
/// `store_error`; in the second run the dispatcher's predecessor read
/// meets it, and the queued turn never launches.
#[test]
fn s1_f12_sqlite_corruption_latches() -> TestResult {
    let point = "store.sqlite.corrupt";
    // A request's read.
    let sandbox = Sandbox::new(&scripts(&[completes("first", 1)]))?;
    sandbox.count(point)?;
    let daemon = sandbox.start()?;
    let (session, _) = sandbox.spawn("first")?;
    sandbox.wait(&format!("{session}/1"))?;
    let next = sandbox.next_hit(point)?;
    sandbox.arm(point, next, "fail_io")?;
    sandbox.refused(&["events", &session, "--json"], "store_error")?;
    sandbox.ack(&daemon, point, next, "fail_io")?;
    daemon.latched_exit()?;
    // The dispatcher's read.
    let sandbox = Sandbox::new(&scripts(&[completes("first", 1)]))?;
    sandbox.count(point)?;
    let daemon = sandbox.start()?;
    let start = "daemon.dispatcher.before_start";
    sandbox.arm(start, 1, "pause")?;
    let (session, _) = sandbox.spawn("first")?;
    sandbox.ack(&daemon, start, 1, "pause")?;
    let next = sandbox.next_hit(point)?;
    sandbox.arm(point, next, "fail_io")?;
    sandbox.resume_point(start, 1)?;
    sandbox.ack(&daemon, point, next, "fail_io")?;
    daemon.latched_exit()?;
    check(sandbox.anchors(&session, 1)? == 0, || {
        "turn 1 launched after the corruption".to_owned()
    })
}

// -------------------------------------------------------- latch path (§7.4)

/// Slack for the harness's own polling and process exit when it checks a
/// daemon bound against its own clock.
const SLACK: Duration = Duration::from_millis(500);

/// The vendor pid the fake reported (`report_pids`).
fn vendor_pid(sandbox: &Sandbox) -> TestResult<u32> {
    sandbox.await_file("agent.pid")?;
    Ok(fs::read_to_string(sandbox.sync.join("agent.pid"))?
        .trim()
        .parse()?)
}

/// A turn that reports its pids, is accepted and holds at gate `prompt`.
fn reported(prompt: &str) -> Value {
    script(
        prompt,
        1,
        vec![
            json!({"action":"report_pids"}),
            accepted(1),
            gate(prompt),
            text("observed"),
            terminal(1),
        ],
    )
}

/// Design §7.4 [O1.D5, r3.17]: a latch (a receipt whose reply is lost,
/// `store.commit.reply_lost`) keeps the daemon serving through the diagnostic
/// window: `daemon/status` reports `store_failed` with scope `daemon`, a
/// mutation gets `store_error` and `daemon/stop` `{"stopping": true}`. The
/// running group is gone within 3 s of the latch (Host's early stop). New
/// connections are refused from `failed_at + 5 s`, and the daemon exits 4
/// by `failed_at + 10 s`. The latch lies between `before` and `after`,
/// the harness's clock readings around the failing request.
#[test]
fn s1_f12_latch_window_bound_and_host_stop() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[reported("other"), completes("lost", 1)]))?;
    sandbox.count("store.commit.reply_lost")?;
    let mut daemon = sandbox.start()?;
    let (other, _) = sandbox.spawn("other")?;
    sandbox.await_file("other.entered")?;
    sandbox.await_accepted(&other, 1)?;
    // The Store's one worker serves this read after the acceptance commit,
    // whose reply (and its hit) precedes it: the count below is settled.
    sandbox.events(&other)?;
    let vendor = vendor_pid(&sandbox)?;
    let next = sandbox.next_hit("store.commit.reply_lost")?;
    sandbox.arm("store.commit.reply_lost", next, "fail_io")?;
    let before = Instant::now();
    sandbox.refused(&spawn_args("lost", &[]), "store_error")?;
    let after = Instant::now();
    sandbox.ack(&daemon, "store.commit.reply_lost", next, "fail_io")?;
    // Inside the window: reads are served, mutations refused.
    let status = sandbox.status()?;
    check(
        status["health"] == "store_failed"
            && status["store_failure"]["scope"] == "daemon"
            && status["store_failure"]["kind"] == "commit_uncertain",
        || format!("the window's status: {status}"),
    )?;
    sandbox.refused(&spawn_args("late", &[]), "store_error")?;
    let stop = sandbox.ok(&["daemon", "stop", "--json"])?;
    check(stop["stopping"] == true, || format!("daemon/stop: {stop}"))?;
    // Host's early stop: the running group is gone within 3 s.
    let stopped_by = after + Duration::from_secs(3) + SLACK;
    wait_until(
        "the running group is gone",
        stopped_by.saturating_duration_since(Instant::now()),
        || !process_live(vendor),
    )?;
    // The window ends at failed_at + 5 s: new connections are refused.
    let socket = sandbox.runtime.join("via.sock");
    wait_until("the window closes", Duration::from_secs(10), || {
        UnixStream::connect(&socket).is_err()
    })?;
    let closed = Instant::now();
    check(
        closed >= before + Duration::from_secs(5)
            && closed <= after + Duration::from_secs(5) + SLACK,
        || {
            format!(
                "the window closed {:?} after the latch request",
                closed - before
            )
        },
    )?;
    let status = daemon.exit(Duration::from_secs(10))?;
    let exited = Instant::now();
    check(
        status.code() == Some(4) && exited <= after + Duration::from_secs(10) + SLACK,
        || {
            format!(
                "exit {status} {:?} after the latch request",
                exited - before
            )
        },
    )?;
    let summary = daemon.summary()?;
    check(summary["store_failed"] == true, || {
        format!("summary: {summary}")
    })?;
    let state: String = sandbox.query(&format!(
        "SELECT state FROM turns WHERE session_id='{other}' AND number=1"
    ))?;
    check(state == "cancelled", || {
        format!("the running turn ended {state}")
    })?;
    sandbox.verify_anchors()
}

/// Design §7.4 [O1.D13]: after the latch, `cancel` (of a running and of a
/// queued turn) and `close` return `store_error`, served in the window,
/// and the latch's force stop performs the cleanup: the running turn ends
/// by the force row and its group is proved absent. After the latch the
/// force path writes no queued cancellation, so the queued turn stays
/// `queued`, never submitted, for the restart handoff (§7.4).
#[test]
fn s1_f12_latch_cancel_and_close_return_store_error() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[held("other", 1), completes("lost", 1)]))?;
    sandbox.count("store.commit.reply_lost")?;
    let daemon = sandbox.start()?;
    let (other, handle) = sandbox.spawn("other")?;
    sandbox.await_file("other.entered")?;
    sandbox.await_accepted(&other, 1)?;
    sandbox.resume(&other, &handle, "queued")?;
    let next = sandbox.next_hit("store.commit.reply_lost")?;
    sandbox.arm("store.commit.reply_lost", next, "fail_io")?;
    sandbox.refused(&spawn_args("lost", &[]), "store_error")?;
    sandbox.ack(&daemon, "store.commit.reply_lost", next, "fail_io")?;
    for turn in ["1", "2"] {
        sandbox.refused(
            &[
                "cancel", &other, "--turn", turn, "--handle", &handle, "--json",
            ],
            "store_error",
        )?;
    }
    sandbox.refused(
        &["close", &other, "--handle", &handle, "--json"],
        "store_error",
    )?;
    daemon.latched_exit()?;
    let states: String = sandbox.query(&format!(
        "SELECT group_concat(state, ',') FROM
         (SELECT state FROM turns WHERE session_id='{other}' ORDER BY number)"
    ))?;
    check(states == "cancelled,queued", || {
        format!("the force stop left {states}")
    })?;
    let submitted: i64 = sandbox.query(&format!(
        "SELECT count(*) FROM turns WHERE session_id='{other}' AND number=2
         AND submitted_at IS NOT NULL"
    ))?;
    check(submitted == 0, || {
        "the queued turn was submitted".to_owned()
    })
}

/// Design §7.4 [O1.D4]: an uncertain event (`store.commit.reply_lost`) on a
/// turn with queued successors latches; final shutdown's batch commits the
/// turn `failed(store)` and cancels the queued turns (`cancel: null`) in
/// one transaction: `failure_batches: {committed: 1, skipped: 0}`. With
/// every later commit failing (`store.commit.fail_persistent`), the batch
/// is skipped and nothing retries it: `skipped: 1`, the turns keep their
/// states. (Characterization of the batch built in 5c4eef9.)
#[test]
fn s1_f12_latch_batch_commits_or_is_skipped() -> TestResult {
    for skipped in [false, true] {
        let sandbox = Sandbox::new(&script(
            "first",
            1,
            vec![
                accepted(1),
                gate("accepted"),
                text("lost"),
                gate("first"),
                terminal(1),
            ],
        ))?;
        sandbox.count("store.commit.reply_lost")?;
        sandbox.count("store.commit.fail_persistent")?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("first")?;
        sandbox.await_file("accepted.entered")?;
        sandbox.await_accepted(&session, 1)?;
        sandbox.resume(&session, &handle, "second")?;
        sandbox.resume(&session, &handle, "third")?;
        let lost = sandbox.next_hit("store.commit.reply_lost")?;
        sandbox.arm("store.commit.reply_lost", lost, "fail_io")?;
        if skipped {
            // The event itself commits; every later commit fails.
            let later = sandbox.next_hit("store.commit.fail_persistent")? + 1;
            sandbox.arm("store.commit.fail_persistent", later, "fail_io_persist")?;
        }
        sandbox.release("accepted")?;
        sandbox.ack(&daemon, "store.commit.reply_lost", lost, "fail_io")?;
        let summary = daemon.latched_exit()?;
        let expected = json!({"committed": u8::from(!skipped), "skipped": u8::from(skipped)});
        check(summary["failure_batches"] == expected, || {
            format!("skipped {skipped}: {summary}")
        })?;
        let states: String = sandbox.query(&format!(
            "SELECT group_concat(state, ',') FROM
             (SELECT state FROM turns WHERE session_id='{session}' ORDER BY number)"
        ))?;
        let want = if skipped {
            "running,queued,queued"
        } else {
            "failed,cancelled,cancelled"
        };
        check(states == want, || format!("skipped {skipped}: {states}"))?;
        if !skipped {
            let envelope: String = sandbox.query(&format!(
                "SELECT envelope FROM turns WHERE session_id='{session}' AND number=1"
            ))?;
            let envelope: Value = serde_json::from_str(&envelope)?;
            check(envelope["failure"]["class"] == "store", || {
                format!("turn 1: {envelope}")
            })?;
            let cancels: i64 = sandbox.query(&format!(
                "SELECT count(*) FROM turns WHERE session_id='{session}' AND number>1
                 AND json_extract(envelope,'$.cancel') IS NULL"
            ))?;
            check(cancels == 2, || {
                "a batch cancellation has a cancel".to_owned()
            })?;
        }
    }
    Ok(())
}

/// Design §6.8, §7.4 [r4.3, r5.7]: Host's early stop is independent of
/// Core, the dispatchers and Store. Turn B's dispatcher is parked at
/// `core.commit.before_send` before sending its observation commit, so the
/// writer stays free; the daemon latches through turn A's receipt, whose
/// reply is lost (`store.commit.reply_lost`). Before B is released,
/// `host.early_stop.sent` is acknowledged for B's group (the only live
/// one) and the group is gone. After release B ends by the force row, and
/// the exit is 4.
#[test]
fn s1_f12_host_early_stop_independent_of_store() -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[reported("b"), completes("a", 1)]))?;
    sandbox.count("core.commit.before_send")?;
    sandbox.count("store.commit.reply_lost")?;
    let daemon = sandbox.start()?;
    let (b, _) = sandbox.spawn("b")?;
    sandbox.await_file("b.entered")?;
    sandbox.await_accepted(&b, 1)?;
    let vendor = vendor_pid(&sandbox)?;
    let send = sandbox.next_hit("core.commit.before_send")?;
    sandbox.arm("core.commit.before_send", send, "pause")?;
    sandbox.release("b")?;
    sandbox.ack(&daemon, "core.commit.before_send", send, "pause")?;
    sandbox.arm("host.early_stop.sent", 1, "fail_io")?;
    let lost = sandbox.next_hit("store.commit.reply_lost")?;
    sandbox.arm("store.commit.reply_lost", lost, "fail_io")?;
    sandbox.refused(&spawn_args("a", &[]), "store_error")?;
    sandbox.ack(&daemon, "store.commit.reply_lost", lost, "fail_io")?;
    sandbox.ack(&daemon, "host.early_stop.sent", 1, "fail_io")?;
    wait_until("B's group is gone", Duration::from_secs(3), || {
        !process_live(vendor)
    })?;
    sandbox.resume_point("core.commit.before_send", send)?;
    daemon.latched_exit()?;
    let envelope: String = sandbox.query(&format!(
        "SELECT envelope FROM turns WHERE session_id='{b}' AND number=1"
    ))?;
    let envelope: Value = serde_json::from_str(&envelope)?;
    check(envelope["cancel"]["outcome"] == "forced", || {
        format!("B did not end by the force row: {envelope}")
    })
}
