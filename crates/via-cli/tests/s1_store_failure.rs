//! Store failures through the real `via` binary and daemon (F12; design T3
//! §7, owner decision O1). A write whose outcome is known not committed is
//! scoped to its request or turn; an uncertain one, an escalation or SQLite
//! corruption latches, and final shutdown then runs the diagnostic window
//! and the failure-resolution batch (§7.4). Each test arms §10 seams and
//! waits only on failpoint acknowledgements, durable rows, sockets or
//! process exit; a sleep only lets time pass, never orders two events.
#![cfg(feature = "test-failpoints")]

#[path = "support/anchors.rs"]
mod anchors;
#[path = "support/evidenced.rs"]
mod evidenced;
#[path = "support/failpoints.rs"]
mod failpoints;
#[path = "support/hits.rs"]
mod hits;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/process.rs"]
mod process;
#[path = "support/scenario.rs"]
#[expect(dead_code, reason = "shared support; evidenced uses its typed errors")]
mod scenario;
mod support;

use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use evidenced::evidenced;
use scenario::ScenarioError;
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

/// A typed timeout, which `evidenced` records as `timeout` (runtime §11.2).
fn timeout(detail: impl Into<String>) -> Box<dyn Error> {
    Box::new(ScenarioError::Timeout(detail.into()))
}

/// A thread's error, sendable to its joiner with its type kept: a typed
/// [`ScenarioError`] stays itself, so a timeout stays a timeout; any other
/// error is a failure, as `evidenced` records it (S1-evidence2 fix round
/// 3, `via-t76`).
fn sendable(error: Box<dyn Error>) -> ScenarioError {
    match error.downcast::<ScenarioError>() {
        Ok(typed) => *typed,
        Err(other) => ScenarioError::Failure(other.to_string()),
    }
}

/// Runs `command` to exit, killing it after `timeout`: a typed timeout,
/// with the kill's bounded reap beside it.
fn run_command(command: &mut Command, within: Duration) -> TestResult<Captured> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let mut child = command.spawn()?;
    let Some(status) = wait_child(&mut child, within)? else {
        let reaped = outer_cleanup::kill_and_reap(&mut child, Instant::now() + outer_cleanup::REAP);
        return Err(timeout(format!(
            "{command:?} timed out (killed, reaped in 1 s: {reaped})"
        )));
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

/// Waits up to `within` for `child` to exit. Each observation is
/// timestamped after it returns: an exit observed only after the deadline
/// is `None`.
fn wait_child(child: &mut Child, within: Duration) -> TestResult<Option<ExitStatus>> {
    let deadline = Instant::now() + within;
    loop {
        let status = child.try_wait()?;
        if Instant::now() > deadline {
            return Ok(None);
        }
        if status.is_some() {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Waits up to `within` until `ready` holds, observed by the deadline; a
/// typed timeout otherwise.
fn wait_until(what: &str, within: Duration, mut ready: impl FnMut() -> bool) -> TestResult {
    let deadline = Instant::now() + within;
    loop {
        let reached = ready();
        if Instant::now() > deadline {
            return Err(timeout(format!("timed out waiting until {what}")));
        }
        if reached {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5));
    }
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
    /// The scenario's evidence, collected when the sandbox is dropped.
    evidence: Option<support::evidence::Evidence>,
    /// The scenario's final teardown, shared by its guards and its drop.
    teardown: outer_cleanup::Teardown,
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
    runs: std::cell::Cell<u32>,
    failpoints: failpoints::Failpoints,
}

/// Collects the scenario's evidence once every daemon has exited: the
/// children it started are reaped by their guards, which borrow the
/// sandbox, and [`evidenced::stop_daemons`] proves the rest gone.
impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(evidence) = self.evidence.take() {
            self.root.disable_cleanup(true);
            let exited =
                evidenced::stop_daemons(&self.runtime, &self.state, &self.teardown, |by| {
                    outer_cleanup::run_within(
                        self.command().args(["daemon", "stop", "--force", "--json"]),
                        by,
                    )
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
        let failpoints = failpoints::Failpoints::new(root.path())?;
        let evidence = evidenced::open(&fake, &fixture_path)?;
        Ok(Self {
            root,
            evidence: Some(evidence),
            teardown: outer_cleanup::Teardown::new(),
            store_expected: std::sync::atomic::AtomicBool::new(true),
            folders_expected: std::sync::atomic::AtomicBool::new(true),
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
        self.ok_within(args, Duration::from_secs(60))
    }

    /// [`Self::ok`], the call killed after `within`.
    fn ok_within(&self, args: &[&str], within: Duration) -> TestResult<Value> {
        let mut command = self.command();
        command.args(args);
        let captured = run_command(&mut command, within)?;
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
        let mut daemon = self.launch()?;
        daemon.ready()?;
        Ok(daemon)
    }

    /// Starts a daemon directly, with the failpoint controller, without
    /// waiting for it to answer.
    fn launch(&self) -> TestResult<Daemon<'_>> {
        if self.teardown.begun() {
            return Err("a daemon started after the final teardown began".into());
        }
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
        Ok(Daemon {
            child: command.spawn()?,
            sandbox: self,
            trace,
        })
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
    fn background(&self, args: &[&str]) -> thread::JoinHandle<Result<Captured, ScenarioError>> {
        self.background_within(args, Duration::from_secs(60))
    }

    /// [`Self::background`], killed after `within`.
    fn background_within(
        &self,
        args: &[&str],
        within: Duration,
    ) -> thread::JoinHandle<Result<Captured, ScenarioError>> {
        let mut command = self.command();
        command.args(args);
        thread::spawn(move || run_command(&mut command, within).map_err(sendable))
    }

    /// The scenario's final outer cleanup: proves every committed anchor's
    /// group absent, as the runtime §11.2 outer harness does, within the
    /// scenario's one teardown deadline, which this begins or joins.
    fn verify_anchors(&self) -> TestResult {
        self.verify_anchors_by(self.teardown.begin())
    }

    /// Proves every committed anchor's group absent by `deadline`.
    fn verify_anchors_by(&self, deadline: Instant) -> TestResult {
        let anchors = outer_cleanup::anchors_by(&self.state.join("store.sqlite3"), None, deadline);
        check(outer_cleanup::anchors_proven(&anchors), || {
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
                return Err(timeout(format!(
                    "no acknowledgement of {point} #{occurrence}"
                )));
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
            let ready = self.sandbox.status().is_ok();
            if Instant::now() > deadline {
                return Err(timeout("daemon readiness deadline elapsed"));
            }
            if ready {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the daemon to exit by itself.
    fn exit(&mut self, within: Duration) -> TestResult<ExitStatus> {
        wait_child(&mut self.child, within)?.ok_or_else(|| {
            timeout(format!(
                "the daemon did not exit in time: {}",
                fs::read_to_string(&self.trace).unwrap_or_default()
            ))
        })
    }

    /// The daemon's final shutdown summary: the last in `via.log` (Task 4
    /// design §7.6).
    fn summary(&self) -> TestResult<Value> {
        fs::read_to_string(self.sandbox.state.join("via.log"))?
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|line| line.get("daemon_shutdown").cloned())
            .ok_or_else(|| "the daemon wrote no shutdown summary".into())
    }

    /// Everything the daemon wrote: its stderr trace, then `via.log`.
    fn trace(&self) -> String {
        let mut trace = fs::read_to_string(&self.trace).unwrap_or_default();
        trace.push_str(&fs::read_to_string(self.sandbox.state.join("via.log")).unwrap_or_default());
        trace
    }

    /// The scenario's final stop, within its one teardown deadline, which
    /// this begins or joins before any stop work (runtime §11.2): a plain
    /// `daemon stop` once no work is active, the exit 0, then the final
    /// outer cleanup.
    fn stop_clean(self) -> TestResult {
        let deadline = self.sandbox.teardown.begin();
        self.stop_clean_by(deadline)
    }

    /// [`Self::stop_clean`] before a restart: a deliberate intermediate
    /// stop with its own runtime §11.2 bound; the final teardown has not
    /// begun.
    fn stop_clean_between(self) -> TestResult {
        self.stop_clean_by(Instant::now() + outer_cleanup::TEARDOWN)
    }

    /// A plain `daemon stop` once no work is active, the exit 0 and the
    /// outer cleanup, all by `deadline`: every wait gets only the time left
    /// (S1-evidence2 fix round 3, Sol r3 finding 1).
    fn stop_clean_by(mut self, deadline: Instant) -> TestResult {
        let sandbox = self.sandbox;
        wait_until("the daemon is idle", outer_cleanup::left(deadline), || {
            sandbox
                .status()
                .is_ok_and(|status| status["sessions"]["active"] == 0)
        })?;
        sandbox.ok_within(&["daemon", "stop", "--json"], outer_cleanup::left(deadline))?;
        let status = self.exit(outer_cleanup::left(deadline))?;
        check(status.code() == Some(0), || {
            format!("a plain stop exited {status}: {}", self.trace())
        })?;
        drop(self);
        sandbox.verify_anchors_by(deadline)
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
    /// Final teardown (runtime §11.2): a live daemon's drop begins, or
    /// joins, the scenario's one teardown deadline, which bounds the
    /// force-stop (at most 2 s), the exit wait and the kill's 1 s reap
    /// ([`outer_cleanup::teardown_child`]); an exited child's drop begins
    /// nothing. Either records its direct child's reap status in the
    /// teardown, for `cleanup.json`. A deliberate mid-test stop exits the
    /// daemon first ([`Daemon::exit`]).
    fn drop(&mut self) {
        let sandbox = self.sandbox;
        let live = !matches!(self.child.try_wait(), Ok(Some(_)));
        let deadline = if live {
            sandbox.teardown.begin()
        } else {
            Instant::now()
        };
        let generation = format!("guard-{}", self.child.id());
        sandbox.teardown.daemon_generation(
            &generation,
            deadline,
            &mut self.child,
            None,
            None,
            |by| {
                outer_cleanup::run_within(
                    sandbox
                        .command()
                        .args(["daemon", "stop", "--force", "--json"]),
                    by,
                )
            },
        );
    }
}

fn accepted(turn: u32) -> Value {
    json!({"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":format!("fake-turn-{turn}")}})
}

/// Model output of turn 1: a `model` mark, not an event (Task 4 design
/// §2.1, §2.4).
fn text(content: &str) -> Value {
    json!({"action":"emit","message":{"type":"text","vendor_turn_id":"fake-turn-1","text":content}})
}

/// A tool round of turn 1: the next model output ends the step.
fn tool_round() -> [Value; 2] {
    [
        json!({"action":"emit","message":{"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t","name":"shell"}}),
        json!({"action":"emit","message":{"type":"tool_ended","vendor_turn_id":"fake-turn-1","tool_id":"t"}}),
    ]
}

/// Turn 1 accepted and holding at gate `accepted` with a tool result in
/// its first step. Released, its model output ends step 1, so the turn's
/// next Store write is step 1's row (Task 4 design §3.2), then it holds at
/// gate `then` before its terminal.
fn row_after_accepted(then: &str) -> Vec<Value> {
    let mut steps = vec![accepted(1), text("first")];
    steps.extend(tool_round());
    steps.extend([gate("accepted"), text("lost"), gate(then), terminal(1)]);
    steps
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
fn joined(call: thread::JoinHandle<Result<Captured, ScenarioError>>) -> TestResult<Value> {
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
    evidenced(|| {
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
    })
}

/// Design §7.1 (report contradiction 11): a request the SQLite writer's
/// queue never took is not committed, so its receipt reports
/// `not_committed`, not `unknown`, and nothing latches.
#[test]
fn s1_f12_request_never_enqueued_is_not_committed() -> TestResult {
    evidenced(|| {
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
    })
}

/// Design §7.1, characterization: a writer that is gone (`WriterLost`)
/// may have committed, so the receipt reports `unknown` with `retry:
/// same_key_only`, the daemon latches, and it exits 4.
#[test]
fn s1_f12_writer_lost_latches() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&completes("lost", 1))?;
        // The spawn is never committed: no turn.
        sandbox.no_launch();
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
    })
}

// ------------------------------------------------- close (rows 10 and 11)

/// Design §7.2 rows 10 and 11 [O1.D12]: a `Closing` commit that is not
/// committed is `store_error` with no state; a `Closed` commit that is not
/// committed keeps `closing` durable, so `resume` is refused, and a later
/// `close` completes. Nothing latches; the scopes are `request`, then
/// `session`.
#[test]
fn s1_f12_closing_and_closed_not_committed() -> TestResult {
    evidenced(|| {
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
    })
}

// ----------------------------------------- final shutdown (rows 14 and 15)

/// Design §7.2 row 14: the force closure pass's standalone
/// `session.closed` is not committed. The session counts in
/// `unclosed_sessions` (exit 4), and nothing latches: `store_failed: false`.
#[test]
fn s1_f12_force_closure_not_committed_counts_unclosed() -> TestResult {
    evidenced(|| {
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
    })
}

/// Design §7.2 row 15 [r3.11]: a forced terminal in final shutdown that is
/// not committed counts in `uncommitted_turns` (exit 4), with no latch and
/// no retry. With `store.commit.reply_lost` instead, the uncertain commit
/// latches: `store_failed: true`. The forced terminal carried
/// `session.closed` and its reply was lost after it committed, so the
/// session is durably closed and `unclosed_sessions` is 0: the pass that
/// can no longer run counts only what is durably open (T3-S5 round 2,
/// decision 12).
#[test]
fn s1_f12_forced_terminal_not_committed_in_shutdown() -> TestResult {
    evidenced(|| {
        for uncertain in [false, true] {
            let sandbox = Sandbox::new(&held("held", 1))?;
            sandbox.count("store.commit.reply_lost")?;
            let mut daemon = sandbox.start()?;
            let (session, _) = sandbox.spawn("held")?;
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
            let state: String =
                sandbox.query(&format!("SELECT state FROM sessions WHERE id='{session}'"))?;
            let expected = if uncertain {
                summary["store_failed"] == true
                    && state == "closed"
                    && summary["unclosed_sessions"] == 0
            } else {
                summary["uncommitted_turns"] == 1 && summary["store_failed"] == false
            };
            check(status.code() == Some(4) && expected, || {
                format!(
                    "uncertain {uncertain}: unexpected exit {status}, session {state}: {summary}"
                )
            })?;
            // Bead via-23b fix round 1: a latch first raised in final
            // shutdown is logged once, with its cause.
            let trace = daemon.trace();
            let latched: Vec<&str> = trace
                .lines()
                .filter(|line| line.contains("store failure latched"))
                .collect();
            check(
                latched.len() == usize::from(uncertain)
                    && latched
                        .iter()
                        .all(|line| line.contains("ERROR") && line.contains("failure=")),
                || format!("uncertain {uncertain}: latch lines {latched:?} in {trace}"),
            )?;
            sandbox.verify_anchors()?;
        }
        Ok(())
    })
}

// ------------------------------------------------------------- status (§7.5)

/// Design §7.5 [O1.D6]: `store_failure` reports the latest of two scoped
/// failures (`count: 2`, its scope and addresses) while `health` stays
/// `healthy`; neither the status nor the daemon's trace carries the
/// prompt, a payload or the handle.
#[test]
fn s1_f12_status_reports_latest_failure() -> TestResult {
    evidenced(|| {
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
    })
}

// ------------------------------------------------ queued cancel (rows 8, 9)

/// Design §7.2 row 8: a caller's `queued → cancelled` that is not committed
/// rolls back to `Waiting` with `store_error` (`not_committed`); nothing
/// latches, and the caller's retry commits it.
#[test]
fn s1_f12_queued_cancel_not_committed() -> TestResult {
    evidenced(|| {
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
    })
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
    evidenced(|| {
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
                .is_ok_and(|status| status["harness_processes"]["in_use"] == 1)
        })?;
        let failure = store_failure(&sandbox)?;
        check(
            failure["scope"] == "turn"
                && failure["affected"]["addresses"] == json!([format!("{session}/1")]),
            || format!("unexpected store_failure: {failure}"),
        )?;
        scoped_end(daemon, &other)
    })
}

/// Design §7.2 row 5, §7.1, Task 4 A24: a turn write that is not
/// committed, first a step row's (`store.commit.step`), then a
/// `cancel.requested` (`store.commit.event`), sets a stop order with cause
/// `store`; the turn ends `failed(store)` with `cancel` evidence and no
/// later events, its group is stopped, and the session's events stay dense:
/// `turn.ended` takes the number the failed event would have had. The
/// refused row and the open step's ride in the terminal (Task 4 §3.2).
#[test]
fn s1_f12_event_not_committed_stops_turn_and_reuses_seq() -> TestResult {
    evidenced(|| {
        for row in [true, false] {
            let sandbox = Sandbox::new(&scripts(&[
                held("other", 1),
                script("first", 1, row_after_accepted("first")),
            ]))?;
            let point = if row {
                "store.commit.step"
            } else {
                "store.commit.event"
            };
            sandbox.count(point)?;
            let daemon = sandbox.start()?;
            let other = other_session(&sandbox)?;
            let (session, handle) = sandbox.spawn("first")?;
            sandbox.await_file("accepted.entered")?;
            sandbox.await_accepted(&session, 1)?;
            let next = sandbox.next_hit(point)?;
            sandbox.arm(point, next, "fail_io")?;
            let cancel = if row {
                sandbox.release("accepted")?;
                None
            } else {
                Some(sandbox.background(&["cancel", &session, "--handle", &handle, "--json"]))
            };
            sandbox.ack(&daemon, point, next, "fail_io")?;
            let first = sandbox.wait(&format!("{session}/1"))?;
            check(
                first["state"] == "failed"
                    && first["failure"]["class"] == "store"
                    && first["stop_reason"] == "error"
                    && first["cancel"]["requested_at"].is_string()
                    && first["cancel"]["cleanup"] == "quiescent",
                || format!("row {row}: unexpected turn 1: {first}"),
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
                || format!("row {row}: turn 1 events: {events:?}"),
            )?;
            // `turn.ended` takes 4: the number of the failed `cancel.requested`,
            // or the next one after a row, which takes none.
            check(events.last().is_some_and(|ended| ended["seq"] == 4), || {
                format!("row {row}: turn.ended did not reuse the number: {events:?}")
            })?;
            if row {
                let rows: i64 = sandbox.query(&format!(
                    "SELECT count(*) FROM steps WHERE session_id='{session}' AND turn=1"
                ))?;
                check(rows == 2, || format!("{rows} step rows, not 2"))?;
            }
            let failure = store_failure(&sandbox)?;
            check(failure["scope"] == "turn", || {
                format!("row {row}: unexpected store_failure: {failure}")
            })?;
            if let Some(cancel) = cancel {
                let _ = cancel.join();
            }
            scoped_end(daemon, &other)?;
        }
        Ok(())
    })
}

/// Design §7.2 row 5, §2 durability: a caller's cancel whose
/// `cancel.requested` commit is not committed gets `store_error`; the order
/// is upgraded to cause `store`, and the turn ends `failed(store)` with the
/// order's `cancel` object.
#[test]
fn s1_f12_cancel_requested_not_committed() -> TestResult {
    evidenced(|| {
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
    })
}

// ------------------------------------------ terminals and retries (rows 7, 9)

/// Design §7.2 row 7 [r3.7, r3.13]: a natural terminal that is not
/// committed is retried once with the same content and sequence number;
/// the retry commits the vendor's result, and nothing latches.
#[test]
fn s1_f12_terminal_retry_once() -> TestResult {
    evidenced(|| {
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
            failure["kind"] == "commit_failed"
                && failure["scope"] == "turn"
                && failure["count"] == 1,
            || format!("unexpected store_failure: {failure}"),
        )?;
        scoped_end(daemon, &other)
    })
}

/// Design §7.2 escalation: a turn write (Task 4: step 1's row, refused at
/// `store.commit.step`) is not committed and, with
/// `store.commit.fail_persistent` armed, the resolution write fails too, so
/// the daemon latches: `health: store_failed`, `store_failure` scope
/// `daemon`, and exit 4. The same for a natural terminal whose one
/// retry fails.
#[test]
fn s1_f12_escalation_latches() -> TestResult {
    evidenced(|| {
        for at_terminal in [false, true] {
            let sandbox = Sandbox::new(&script("first", 1, row_after_accepted("first")))?;
            sandbox.count("store.commit.fail_persistent")?;
            sandbox.count("store.commit.step")?;
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
                // Task 4 A24: the turn write is step 1's row, refused at its own
                // seam; the resolution write is then the next commit.
                let row = sandbox.next_hit("store.commit.step")?;
                sandbox.arm("store.commit.step", row, "fail_io")?;
                sandbox.release("accepted")?;
                sandbox.ack(&daemon, "store.commit.step", row, "fail_io")?;
            }
            // The terminal and its retry, or the resolution write.
            let failed = if at_terminal { next + 1 } else { next };
            sandbox.ack(&daemon, "store.commit.fail_persistent", failed, "fail_io")?;
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
    })
}

/// Design §7.2 same-sequence retries [r3.7, r4.9]: while a natural
/// terminal's retry is paused at `core.retry.before`, holding the session
/// head, a `resume` receipt of the same session blocks on the head
/// (`core.head.contended`). On release the terminal commits at the failed
/// sequence number, then the receipt takes the next one. The same for the
/// close pass's cancellation (row 9) against a caller's cancel.
#[test]
fn s1_f12_retry_holds_head_against_competing_writer() -> TestResult {
    evidenced(|| {
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
    })
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
    evidenced(|| {
        for variant in ["once", "persistent", "unfinished"] {
            let mut sandbox = Sandbox::new(&scripts(&[held("holder", 1)]))?;
            // One permit: the second session's turn waits for it, queued.
            sandbox
                .env
                .push(("VIA_TEST_HARNESS_PROCESSES", "1".to_owned()));
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
    })
}

// ------------------------------------------------ Host journal (3, 4)

/// `pid` is live: neither gone nor a zombie awaiting its reaper.
/// Unreadable process state is an error, never absence.
fn process_live(pid: u32) -> TestResult<bool> {
    Ok(!process::exited(pid)?)
}

/// Waits up to `within` until `pid` exited ([`process_live`]), observed by
/// the deadline; a typed timeout otherwise.
fn wait_exited(what: &str, within: Duration, pid: u32) -> TestResult {
    let deadline = Instant::now() + within;
    loop {
        let live = process_live(pid)?;
        if Instant::now() > deadline {
            return Err(timeout(format!("timed out waiting until {what}")));
        }
        if !live {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Design §7.2 row 3 [O1.D10]: an anchor intent that is not committed
/// starts no process; the turn ends `failed(store)` with `requested` and
/// `quiescent` evidence, and the session's next turn launches.
#[test]
fn s1_f12_anchor_intent_not_committed() -> TestResult {
    evidenced(|| {
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
    })
}

/// Design §7.2 row 4 [O1.D10, r3.12]: an anchor identified, ARM intent or
/// vendor facts write that is not committed stops the group through the
/// live control, from the identity Host recorded before the commit, and
/// proves it absent: the turn ends `failed(store)` with `cancel` evidence
/// and cleanup `quiescent`, and the session's next turn launches. The
/// outer harness proves every anchor's group absent at the end.
#[test]
fn s1_f12_host_journal_failure_stops_group() -> TestResult {
    evidenced(|| {
        for point in [
            "store.journal.identified",
            "store.journal.arm_intent",
            "store.journal.vendor_facts",
        ] {
            host_journal_failure_stops_group(point).map_err(|error| format!("{point}: {error}"))?;
        }
        Ok(())
    })
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
    // Bead via-23b: the journal write's failure is the turn's Store
    // failure, never a `launch_failed` warning (C1 §6.1).
    let events = sandbox.events(&session)?;
    check(
        !events
            .iter()
            .any(|event| event["type"] == "warning" && event["code"] == "launch_failed"),
        || format!("a journal write reported as launch_failed: {events:?}"),
    )?;
    // The vendor, if it ran, is gone.
    let agent = sandbox.sync.join("agent.pid");
    if agent.exists() {
        let pid: u32 = fs::read_to_string(&agent)?.trim().parse()?;
        check(!process_live(pid)?, || "the vendor survived".to_owned())?;
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

// ------------------------------------------------ re-probe proofs (rows 4, 12)

/// Design §7.2 rows 4 and 12 [s1.6]: an anchor identified write that is
/// not committed, with the anchor held at `host.anchor.before_eof_cleanup`,
/// leaves absence unproven: the slot stays held
/// (`harness_processes.held_unproven`). After release, re-probe commits the
/// proof with the identity Host kept in memory and frees the slot. The
/// proof's own commit: not committed once, it is retried on the next pass
/// and nothing latches; uncertain (the commit outlives the pass), the
/// daemon latches.
#[test]
fn s1_f12_host_journal_failure_unproven_slot_reprobed() -> TestResult {
    evidenced(|| {
        for proof in ["commits", "not_committed", "uncertain"] {
            unproven_slot_reprobed(proof).map_err(|error| format!("{proof}: {error}"))?;
        }
        Ok(())
    })
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
    check(held["harness_processes"]["held_unproven"] == 1, || {
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
                .is_ok_and(|status| status["harness_processes"]["held_unproven"] == 0)
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
        // The identified write's failure, then the proof's, which carries
        // the owner session (T3-S5 round 1, decision 6).
        let failure = store_failure(&sandbox)?;
        check(
            failure["kind"] == "journal_failed"
                && failure["scope"] == "session"
                && failure["count"] == 2
                && failure["affected"]["addresses"] == json!([session]),
            || format!("unexpected store_failure: {failure}"),
        )?;
    }
    daemon.stop_clean()
}

/// Restarts a stopped daemon over 300 proven synthetic anchors and one
/// unread one, `1-unread`, whose group (a pid above any `pid_max`) is absent
/// but not yet proved, all owned by `owner`'s turn 1. Startup reconciliation
/// stops at its deadline at a page boundary before `1-unread`, so it counts
/// it unread and the re-probe loop's resumed paging reads it (design §8).
fn restart_with_unread<'a>(sandbox: &'a Sandbox, owner: &str) -> TestResult<Daemon<'a>> {
    let store = sandbox.state.join("store.sqlite3");
    anchors::insert_proven_absent(&store, owner, "0-synthetic", 300)?;
    let unread = rusqlite::Connection::open(&store)?.execute(
        "INSERT INTO anchors(anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,phase,record_version,pid,pgid,start_ticks,absence_time)
         SELECT '1-unread','g1-unread',a.marker,'/nonexistent',?1,1,a.uid,a.boot_id,a.pid_namespace,'arm_intent',1,4195000,4195000,1,NULL
         FROM anchors a WHERE a.pid IS NOT NULL LIMIT 1",
        [owner],
    )?;
    check(unread == 1, || "no real anchor to copy".to_owned())?;
    let boundary = "core.recovery.page_boundary";
    sandbox.arm(boundary, 1, "pause")?;
    let mut daemon = sandbox.launch()?;
    sandbox.ack(&daemon, boundary, 1, "pause")?;
    // Recovery's deadline began before the acknowledgement: time only has to
    // pass until it has, so paging stops with the unread anchor counted.
    thread::sleep(Duration::from_millis(5_200));
    sandbox.resume_point(boundary, 1)?;
    daemon.ready()?;
    sandbox.disarm(boundary)?;
    Ok(daemon)
}

/// Removes the synthetic anchors of [`restart_with_unread`] from a stopped
/// daemon's Store, then checks every real one is proved absent.
fn remove_synthetic_anchors(sandbox: &Sandbox) -> TestResult {
    let store = sandbox.state.join("store.sqlite3");
    anchors::delete_synthetic(&store, "0-synthetic")?;
    anchors::delete_synthetic(&store, "1-unread")?;
    sandbox.verify_anchors()
}

/// Seeds one completed session, stops the daemon, and restarts it with an
/// unread anchor (see [`restart_with_unread`]) after `arm` armed the seams.
fn resumed_paging_daemon(
    sandbox: &Sandbox,
    arm: impl FnOnce(&Sandbox) -> TestResult,
) -> TestResult<Daemon<'_>> {
    let daemon = sandbox.start()?;
    let (seed, _) = sandbox.spawn("seed")?;
    sandbox.wait(&format!("{seed}/1"))?;
    daemon.stop_clean_between()?;
    arm(sandbox)?;
    restart_with_unread(sandbox, &seed)
}

/// Design §7.2 row 12 [O1.D10], resumed paging (design §8 step 2): the
/// absence-proof commit of an anchor read by resumed paging may have
/// committed but did not answer within the pass bound. That uncertain
/// outcome latches like a re-probe proof's does, instead of leaving the page
/// unread for another pass. `1-unread`'s proof commit is held past the
/// pass's 3 s bound.
#[test]
fn s1_f12_resumed_paging_uncertain_proof_latches() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(&[completes("seed", 1)]))?;
        let proof = "store.journal.absence";
        let mut daemon = resumed_paging_daemon(&sandbox, |sandbox| sandbox.arm(proof, 1, "pause"))?;
        sandbox.ack(&daemon, proof, 1, "pause")?;
        wait_until("the daemon latches", Duration::from_secs(20), || {
            !sandbox.runtime.join("via.sock").exists()
                || sandbox
                    .status()
                    .is_ok_and(|status| status["health"] == "store_failed")
        })?;
        sandbox.resume_point(proof, 1)?;
        let status = daemon.exit(Duration::from_secs(20))?;
        let summary = daemon.summary()?;
        check(
            status.code() == Some(4) && summary["store_failed"] == true,
            || format!("expected the latch's exit 4, got {status}: {summary}"),
        )?;
        drop(daemon);
        remove_synthetic_anchors(&sandbox)
    })
}

/// Design §7.2 row 12, resumed paging: an absence-proof commit that did not
/// commit (its `host.recovery.absence_commit` seam fails once) leaves the
/// page unread and the slot held; the next pass reads it again and proves
/// the anchor. Nothing latches.
#[test]
fn s1_f12_resumed_paging_unproved_page_is_retried() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(&[completes("seed", 1)]))?;
        let proof = "host.recovery.absence_commit";
        let mut daemon =
            resumed_paging_daemon(&sandbox, |sandbox| sandbox.arm(proof, 1, "fail_io"))?;
        sandbox.ack(&daemon, proof, 1, "fail_io")?;
        wait_until(
            "the retried page proves the anchor",
            Duration::from_secs(30),
            || {
                sandbox
                .query::<i64>(
                    "SELECT count(*) FROM anchors WHERE anchor_id='1-unread' AND absence_time IS NOT NULL",
                )
                .is_ok_and(|proved| proved == 1)
            },
        )?;
        let status = sandbox.status()?;
        check(
            status["health"] == "healthy" && status["harness_processes"]["held_unproven"] == 0,
            || format!("the retry latched or kept the slot: {status}"),
        )?;
        sandbox.ok(&["daemon", "stop", "--json"])?;
        let exit = daemon.exit(Duration::from_secs(15))?;
        check(exit.code() == Some(0), || {
            format!("a plain stop exited {exit}: {}", daemon.trace())
        })?;
        drop(daemon);
        remove_synthetic_anchors(&sandbox)
    })
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
    evidenced(|| {
        dispatcher_reads_fail("store.read.dispatch", false)?;
        dispatcher_reads_fail("store.read.dispatch", true)
    })
}

/// Design §7.3 [r3.8]: only the queued-row read fails (persistent
/// `store.read.queued_turn`) while the predecessor reads succeed. The
/// streak does not reset on those successful reads: the head turn still
/// fails at the lowered deadline, without launch.
#[test]
fn s1_f12_selective_queued_row_read_failure() -> TestResult {
    evidenced(|| dispatcher_reads_fail("store.read.queued_turn", false))
}

/// Design §3.1 `Cancelling{dispatcher}`, §7.3 (S1 critic finding 2): the
/// close pass's cancellation of queued turn 2 cannot read its queued row
/// (persistent `store.read.queued_turn`). The failed reads feed the
/// dispatcher's read streak; at the lowered deadline turn 2 is cancelled
/// from its committed `turn.queued`, keeping cause `close`, so the close
/// completes and a drain issued meanwhile finishes (exit 0) once a second
/// session's turn, held meanwhile, completes. Before the fix the pass
/// retried forever.
#[test]
fn s1_f12_close_cancellation_read_failure_ends_at_the_streak() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new(&scripts(&[held("other", 1), held("first", 1)]))?;
        sandbox
            .env
            .push(("VIA_TEST_READ_FAILURE_MS", READ_STREAK_MS.to_owned()));
        let point = "store.read.queued_turn";
        sandbox.count(point)?;
        let mut daemon = sandbox.start()?;
        let other = other_session(&sandbox)?;
        let (session, handle) = sandbox.spawn("first")?;
        sandbox.await_file("first.entered")?;
        sandbox.resume(&session, &handle, "second")?;
        // Turn 1's submission read is done: the next one is turn 2's cancellation.
        let first_failure = sandbox.next_hit(point)?;
        sandbox.arm(point, first_failure, "fail_io_persist")?;
        let close = sandbox.background(&[
            "close", &session, "--mode", "force", "--handle", &handle, "--json",
        ]);
        sandbox.ack(&daemon, point, first_failure, "fail_io")?;
        // Issued while the close pass retries: the streak bounds the drain.
        sandbox.ok(&["daemon", "stop", "--drain", "--json"])?;
        let closed = joined(close)?;
        check(
            closed["state"] == "closed"
                && closed["cancelled_turns"]
                    == json!([format!("{session}/1"), format!("{session}/2")]),
            || format!("the close did not complete: {closed}"),
        )?;
        queued_turn_cancelled(&sandbox, &session, "close")?;
        other_completes(&sandbox, &other)?;
        let status = daemon.exit(Duration::from_secs(20))?;
        check(status.code() == Some(0), || {
            format!("the drain exited {status}: {}", daemon.trace())
        })?;
        drop(daemon);
        sandbox.disarm(point)?;
        sandbox.verify_anchors()
    })
}

/// Design §3.1 `Cancelling{dispatcher}`, §7.3 (S1 critic finding 2): a
/// caller `cancel` attaches its order while the dispatcher holds queued
/// turn 2 claimed (`core.dispatch.before_grant`); the submission's queued
/// row read then fails persistently, so the claim rolls back to
/// `Cancelling{dispatcher}`. Its failed reads feed the read streak; at the
/// lowered deadline turn 2 is cancelled from its committed `turn.queued`
/// with cause `cancel`, and a drain issued meanwhile finishes (exit 0) once
/// a second session's turn, held meanwhile, completes.
#[test]
fn s1_f12_cancel_read_failure_ends_at_the_streak() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new(&scripts(&[held("other", 1), completes("first", 1)]))?;
        sandbox
            .env
            .push(("VIA_TEST_READ_FAILURE_MS", READ_STREAK_MS.to_owned()));
        let point = "store.read.queued_turn";
        let grant = "core.dispatch.before_grant";
        let ordered = "core.cancel.ordered";
        sandbox.count(point)?;
        let mut daemon = sandbox.start()?;
        let other = other_session(&sandbox)?;
        let (session, handle) = sandbox.spawn("first")?;
        sandbox.wait(&format!("{session}/1"))?;
        // The second session's claim and turn 1's were the first two.
        sandbox.arm(grant, 3, "pause")?;
        // Acknowledgement only: the order is attached.
        sandbox.arm(ordered, 1, "fail_io")?;
        sandbox.resume(&session, &handle, "second")?;
        sandbox.ack(&daemon, grant, 3, "pause")?;
        let cancel = sandbox.background(&[
            "cancel", &session, "--turn", "2", "--handle", &handle, "--json",
        ]);
        sandbox.ack(&daemon, ordered, 1, "fail_io")?;
        // The next read is the submission's queued row.
        let first_failure = sandbox.next_hit(point)?;
        sandbox.arm(point, first_failure, "fail_io_persist")?;
        sandbox.resume_point(grant, 3)?;
        sandbox.ack(&daemon, point, first_failure, "fail_io")?;
        // Issued while the dispatcher retries: the streak bounds the drain.
        sandbox.ok(&["daemon", "stop", "--drain", "--json"])?;
        // The caller joined the dispatcher's cancellation: a plain `store_error`
        // for a failed read, or the committed cancellation.
        let replied = cancel.join().map_err(|_| "the cancel panicked")??;
        let reply = String::from_utf8_lossy(&replied.stdout);
        let error = String::from_utf8_lossy(&replied.stderr);
        check(
            (replied.status.success() && reply.contains("\"cancelled\""))
                || (replied.status.code() == Some(2) && error.contains("store_error")),
            || format!("cancel exited {}: {reply} {error}", replied.status),
        )?;
        let ended = sandbox.wait(&format!("{session}/2"))?;
        check(ended["state"] == "cancelled", || {
            format!("turn 2 did not end cancelled: {ended}")
        })?;
        queued_turn_cancelled(&sandbox, &session, "cancel")?;
        other_completes(&sandbox, &other)?;
        let status = daemon.exit(Duration::from_secs(20))?;
        check(status.code() == Some(0), || {
            format!("the drain exited {status}: {}", daemon.trace())
        })?;
        drop(daemon);
        sandbox.disarm(point)?;
        sandbox.disarm(grant)?;
        sandbox.disarm(ordered)?;
        sandbox.verify_anchors()
    })
}

/// Releases the second session's held turn 1, which completes unaffected;
/// it kept the drain open for the harness's own reads.
fn other_completes(sandbox: &Sandbox, other: &str) -> TestResult {
    sandbox.release("other")?;
    let envelope = sandbox.wait(&format!("{other}/1"))?;
    check(envelope["state"] == "completed", || {
        format!("the second session's turn was affected: {envelope}")
    })
}

/// Turn 2 of `session` is durably `cancelled` with `cause`, never submitted
/// and never launched: `turn.queued`, then `turn.ended`.
fn queued_turn_cancelled(sandbox: &Sandbox, session: &str, cause: &str) -> TestResult {
    let row: String = sandbox.query(&format!(
        "SELECT state || ' ' || cancel_cause || ' '
                || json_extract(envelope,'$.cancel.outcome') || ' '
                || json_extract(envelope,'$.cancel.cleanup')
         FROM turns WHERE session_id='{session}' AND number=2"
    ))?;
    check(
        row == format!("cancelled {cause} acknowledged quiescent"),
        || format!("turn 2: {row}"),
    )?;
    check(sandbox.anchors(session, 2)? == 0, || {
        "turn 2 launched".to_owned()
    })?;
    let events = sandbox.events(session)?;
    dense(&events)?;
    check(
        event_types(&events, 2) == ["turn.queued", "turn.ended"],
        || format!("turn 2 events: {events:?}"),
    )
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
    evidenced(|| {
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
        // The dispatcher's read; the turn never launches.
        let sandbox = Sandbox::new(&scripts(&[completes("first", 1)]))?;
        sandbox.no_launch();
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
    })
}

/// T3-S5 round 1, decision 2 (design §7.1): SQLite corruption on the
/// session-head read that a receipt (`resume`) or `Closed` (`close`) takes
/// latches, rather than being a plain or scoped failure. A restarted
/// daemon keeps no head for an idle session, so the verb's commit reads it
/// (`store.read.corrupt.next_seq`); the request replies `store_error`,
/// `store_failure` is `corrupt_store` with scope `daemon`, and the exit is 4.
#[test]
fn s1_f12_corrupt_head_read_latches() -> TestResult {
    evidenced(|| {
        for verb in ["resume", "close"] {
            corrupt_head_read(verb).map_err(|error| format!("{verb}: {error}"))?;
        }
        Ok(())
    })
}

fn corrupt_head_read(verb: &str) -> TestResult {
    let sandbox = Sandbox::new(&scripts(&[completes("first", 1)]))?;
    let daemon = sandbox.start()?;
    let (session, handle) = sandbox.spawn("first")?;
    let first = sandbox.wait(&format!("{session}/1"))?;
    check(first["state"] == "completed", || format!("turn 1: {first}"))?;
    daemon.stop_clean_between()?;
    let daemon = sandbox.start()?;
    let point = "store.read.corrupt.next_seq";
    sandbox.arm(point, 1, "fail_io")?;
    let args: Vec<&str> = if verb == "resume" {
        vec![
            "resume", &session, "--prompt", "second", "--handle", &handle, "--json",
        ]
    } else {
        vec!["close", &session, "--handle", &handle, "--json"]
    };
    sandbox.refused(&args, "store_error")?;
    sandbox.ack(&daemon, point, 1, "fail_io")?;
    let failure = store_failure(&sandbox)?;
    check(
        failure["kind"] == "corrupt_store" && failure["scope"] == "daemon",
        || format!("unexpected store_failure: {failure}"),
    )?;
    daemon.latched_exit()?;
    Ok(())
}

/// S1 critic r2 finding 1 (runtime §7, design §7.1): SQLite corruption on
/// the acceptance write itself (`store.commit.corrupt.acceptance`), after
/// its prerequisite head read succeeded, latches at once: `daemon/status`
/// reports `store_failed` with a `corrupt_store` failure of scope `daemon`,
/// a new spawn is refused `store_error`, and a second session's turn held
/// before its dispatch grant (`core.dispatch.before_grant`) never launches.
#[test]
fn s1_f12_corrupt_acceptance_write_latches() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(&[completes("first", 1), completes("second", 1)]))?;
        sandbox.no_launch();
        let daemon = sandbox.start()?;
        let accept = "core.accept.before_commit";
        let corrupt = "store.commit.corrupt.acceptance";
        let grant = "core.dispatch.before_grant";
        sandbox.arm(accept, 1, "pause")?;
        sandbox.arm(corrupt, 1, "fail_io")?;
        let (_first, _) = sandbox.spawn("first")?;
        sandbox.ack(&daemon, accept, 1, "pause")?;
        // The first session's dispatch took grant hit 1; the second's is 2.
        sandbox.arm(grant, 2, "pause")?;
        let (second, _) = sandbox.spawn("second")?;
        sandbox.ack(&daemon, grant, 2, "pause")?;
        sandbox.resume_point(accept, 1)?;
        sandbox.ack(&daemon, corrupt, 1, "fail_io")?;
        wait_until("the latch", Duration::from_secs(5), || {
            sandbox
                .status()
                .is_ok_and(|status| status["health"] == "store_failed")
        })?;
        let failure = store_failure(&sandbox)?;
        check(
            failure["kind"] == "corrupt_store" && failure["scope"] == "daemon",
            || format!("unexpected store_failure: {failure}"),
        )?;
        sandbox.refused(&spawn_args("late", &[]), "store_error")?;
        sandbox.resume_point(grant, 2)?;
        daemon.latched_exit()?;
        check(sandbox.anchors(&second, 1)? == 0, || {
            "the second session's turn launched after the latch".to_owned()
        })
    })
}

/// S1-runtime2 fix round 2 (runtime §7): a natural terminal commit whose
/// reply is lost (`store.commit.reply_lost`) latches before its read-back:
/// while the read-back is held (`core.terminal.read_back`), `daemon/status`
/// already reports `health: store_failed` and a new spawn is refused
/// `store_error`. Released, the read-back finds the committed terminal,
/// which stays `completed`; the failure is `commit_uncertain`.
#[test]
fn s1_f12_uncertain_terminal_latches_before_its_read_back() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&held("first", 1))?;
        sandbox.count("store.commit.reply_lost")?;
        let daemon = sandbox.start()?;
        let settling = "core.run.settling";
        let read_back = "core.terminal.read_back";
        sandbox.arm(settling, 1, "pause")?;
        let (session, _) = sandbox.spawn("first")?;
        sandbox.await_file("first.entered")?;
        sandbox.release("first")?;
        sandbox.ack(&daemon, settling, 1, "pause")?;
        // The terminal's commit is the next lifecycle reply.
        let lost = sandbox.next_hit("store.commit.reply_lost")?;
        sandbox.arm("store.commit.reply_lost", lost, "fail_io")?;
        sandbox.arm(read_back, 1, "pause")?;
        sandbox.resume_point(settling, 1)?;
        sandbox.ack(&daemon, "store.commit.reply_lost", lost, "fail_io")?;
        sandbox.ack(&daemon, read_back, 1, "pause")?;
        let status = sandbox.status()?;
        check(status["health"] == "store_failed", || {
            format!("not latched before the read-back: {status}")
        })?;
        sandbox.refused(&spawn_args("late", &[]), "store_error")?;
        sandbox.resume_point(read_back, 1)?;
        wait_until("the failure record", Duration::from_secs(5), || {
            store_failure(&sandbox).is_ok_and(|failure| failure["kind"] == "commit_uncertain")
        })?;
        let failure = store_failure(&sandbox)?;
        check(failure["scope"] == "daemon", || {
            format!("unexpected store_failure: {failure}")
        })?;
        daemon.latched_exit()?;
        // Bead via-23b: the latch, visible before the read-back, is logged
        // once with its own cause, whenever final shutdown began.
        let log = fs::read_to_string(sandbox.state.join("via.log"))?;
        let latched: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("store failure latched"))
            .collect();
        check(
            latched.len() == 1
                && latched[0].contains("commit_uncertain")
                && latched[0].contains(&format!("{session}/1")),
            || format!("latch lines {latched:?} in {log}"),
        )?;
        let state: String = sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{session}' AND number=1"
        ))?;
        check(state == "completed", || format!("turn 1 is {state}"))
    })
}

/// S1-runtime2 fix round 2 (design §7.1): SQLite corruption on the
/// terminal write itself (`store.commit.corrupt.terminal`) latches as
/// `corrupt_store`, not `commit_uncertain`, although its read-back finds no
/// terminal; final shutdown's batch then resolves the turn.
#[test]
fn s1_f12_corrupt_terminal_write_latches_as_corruption() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&completes("first", 1))?;
        let daemon = sandbox.start()?;
        let point = "store.commit.corrupt.terminal";
        sandbox.arm(point, 1, "fail_io")?;
        let (session, _) = sandbox.spawn("first")?;
        sandbox.ack(&daemon, point, 1, "fail_io")?;
        wait_until("the latch", Duration::from_secs(5), || {
            sandbox
                .status()
                .is_ok_and(|status| status["health"] == "store_failed")
        })?;
        wait_until("the failure record", Duration::from_secs(5), || {
            store_failure(&sandbox).is_ok_and(|failure| !failure.is_null())
        })?;
        let failure = store_failure(&sandbox)?;
        check(
            failure["kind"] == "corrupt_store" && failure["scope"] == "daemon",
            || format!("unexpected store_failure: {failure}"),
        )?;
        daemon.latched_exit()?;
        let state: String = sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{session}' AND number=1"
        ))?;
        // Nothing was written; final shutdown's failure-resolution batch
        // ends the affected turn `failed(store)` (design §7.4).
        check(state == "failed", || {
            format!("the corrupt terminal write left turn 1 {state}")
        })
    })
}

/// S1-runtime2 fix round 2 (design §2 rule 4, Task 4 design §2.3): the
/// final text Core received before the daemon force took the turn is kept
/// in the forced terminal. Core has handled the terminal's `"done"` once it
/// holds at the next observation (`core.observations.pause`, hit 3); the
/// force comes then, from `daemon stop --force` or from a latch (a
/// receipt's lost reply). Either way the durable forced terminal carries
/// `final_text: "done"`.
#[test]
fn s1_force_keeps_the_final_text_core_received() -> TestResult {
    evidenced(|| {
        for latch in [false, true] {
            let sandbox = Sandbox::new(&scripts(&[script(
                "first",
                1,
                vec![
                    accepted(1),
                    terminal(1),
                    text("after"),
                    json!({"action":"hang"}),
                ],
            )]))?;
            sandbox.count("store.commit.reply_lost")?;
            let mut daemon = sandbox.start()?;
            let observed = "core.observations.pause";
            // Acceptance, the final text, then the late `text`.
            sandbox.arm(observed, 3, "pause")?;
            let (session, _) = sandbox.spawn("first")?;
            sandbox.ack(&daemon, observed, 3, "pause")?;
            if latch {
                let lost = sandbox.next_hit("store.commit.reply_lost")?;
                sandbox.arm("store.commit.reply_lost", lost, "fail_io")?;
                sandbox.refused(&spawn_args("lost", &[]), "store_error")?;
                sandbox.ack(&daemon, "store.commit.reply_lost", lost, "fail_io")?;
            } else {
                sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
            }
            sandbox.resume_point(observed, 3)?;
            let status = daemon.exit(Duration::from_secs(20))?;
            check(status.code() == Some(if latch { 4 } else { 0 }), || {
                format!(
                    "latch {latch}: the daemon exited {status}: {}",
                    daemon.trace()
                )
            })?;
            let envelope: String = sandbox.query(&format!(
                "SELECT envelope FROM turns WHERE session_id='{session}' AND number=1"
            ))?;
            let envelope: Value = serde_json::from_str(&envelope)?;
            check(
                envelope["state"] == "cancelled"
                    && envelope["final_text"] == "done"
                    && envelope["cancel"]["outcome"] == "forced",
                || format!("latch {latch}: unexpected forced terminal: {envelope}"),
            )?;
        }
        Ok(())
    })
}

/// T3-S5 round 1, decision 10 (design §7.1): SQLite corruption on the
/// session-head read of final shutdown's failure-resolution batch is
/// reported as corruption. A lost event reply (`store.commit.reply_lost`,
/// on a caller's `cancel.requested`) latches and leaves the head unknown, so the batch reads it
/// (`store.read.corrupt.next_seq`): the batch is skipped, nothing more is
/// written, and the latest `store_failure`, served in the diagnostic
/// window, is `corrupt_store`.
#[test]
fn s1_f12_batch_corrupt_head_read_is_corrupt() -> TestResult {
    evidenced(|| {
        let point = "store.read.corrupt.next_seq";
        let sandbox = Sandbox::new(&script("first", 1, row_after_accepted("first")))?;
        sandbox.count("store.commit.reply_lost")?;
        sandbox.count(point)?;
        let daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("first")?;
        sandbox.await_file("accepted.entered")?;
        sandbox.await_accepted(&session, 1)?;
        // The acceptance's reply hit is counted after its row is durable; a
        // receipt's reply returns after its own hit, so the count is settled.
        sandbox.resume(&session, &handle, "second")?;
        let lost = sandbox.next_hit("store.commit.reply_lost")?;
        sandbox.arm("store.commit.reply_lost", lost, "fail_io")?;
        let next = sandbox.next_hit(point)?;
        sandbox.arm(point, next, "fail_io")?;
        // Task 4 A24: the turn event whose reply is lost is a caller's
        // `cancel.requested`; model text is not an event.
        let cancel = sandbox.background(&["cancel", &session, "--handle", &handle, "--json"]);
        sandbox.ack(&daemon, "store.commit.reply_lost", lost, "fail_io")?;
        sandbox.ack(&daemon, point, next, "fail_io")?;
        // The latch, then the head read's corruption at Store's read reply
        // (T3-S5 round 2, decision 11). The batch's write it aborted records
        // nothing more (round 3, decision 13); the summary shows it skipped.
        wait_until("the head read's failure", Duration::from_secs(4), || {
            store_failure(&sandbox).is_ok_and(|failure| failure["count"] == 2)
        })?;
        let failure = store_failure(&sandbox)?;
        check(
            failure["kind"] == "corrupt_store" && failure["scope"] == "daemon",
            || format!("unexpected store_failure: {failure}"),
        )?;
        let summary = daemon.latched_exit()?;
        let _ = cancel.join();
        check(
            summary["failure_batches"] == json!({"committed": 0, "skipped": 1}),
            || format!("unexpected summary: {summary}"),
        )?;
        let states: String = sandbox.query(&format!(
            "SELECT group_concat(state, ',') FROM
         (SELECT state FROM turns WHERE session_id='{session}' ORDER BY number)"
        ))?;
        check(states == "running,queued", || format!("turns: {states}"))
    })
}

/// T3-S5 round 3, decision 15 (design §6.1, §7.1): SQLite corruption on a
/// startup recovery read (`store.read.corrupt.unfinished`) fails startup
/// before anything is served. The daemon exits 4, its bound socket is
/// unlinked, and `daemon.lock` and `store.lock` are both released; a
/// later start without the fault serves.
#[test]
fn s1_f12_startup_recovery_corrupt_read_fails_startup() -> TestResult {
    evidenced(|| {
        let point = "store.read.corrupt.unfinished";
        let sandbox = Sandbox::new(&completes("first", 1))?;
        // Startup fails before any turn.
        sandbox.no_launch();
        sandbox.arm(point, 1, "fail_io")?;
        let mut daemon = sandbox.launch()?;
        sandbox.ack(&daemon, point, 1, "fail_io")?;
        let status = daemon.exit(Duration::from_secs(20))?;
        let trace = daemon.trace();
        check(
            status.code() == Some(4) && trace.contains("store_error"),
            || format!("startup did not fail with the Store failure ({status}): {trace}"),
        )?;
        let socket = sandbox.runtime.join("via.sock");
        check(!socket.exists(), || {
            format!("the failed startup left {}", socket.display())
        })?;
        for lock in [
            sandbox.runtime.join("daemon.lock"),
            sandbox.state.join("store.lock"),
        ] {
            let file = File::open(&lock)?;
            check(file.try_lock().is_ok(), || {
                format!("{} is still held", lock.display())
            })?;
        }
        drop(daemon);
        sandbox.disarm(point)?;
        sandbox.start()?.stop_clean()
    })
}

/// T3-S5 round 2, decision 11 (design §7.1): SQLite corruption on the force
/// closure pass's snapshot read (`store.read.corrupt.snapshot`) is reported
/// at Store's read reply and latches: the session counts in
/// `unclosed_sessions`, `store_failed` is true, and the exit is 4.
#[test]
fn s1_f12_force_closure_corrupt_snapshot_read_latches() -> TestResult {
    evidenced(|| {
        let point = "store.read.corrupt.snapshot";
        let sandbox = Sandbox::new(&completes("first", 1))?;
        sandbox.count(point)?;
        let mut daemon = sandbox.start()?;
        sandbox.arm("core.run.settling", 1, "pause")?;
        let (session, _) = sandbox.spawn("first")?;
        // The turn's execution ended; its terminal commits after force, so the
        // closure pass reads the session alone.
        sandbox.ack(&daemon, "core.run.settling", 1, "pause")?;
        let next = sandbox.next_hit(point)?;
        sandbox.arm(point, next, "fail_io")?;
        sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
        sandbox.resume_point("core.run.settling", 1)?;
        sandbox.ack(&daemon, point, next, "fail_io")?;
        let status = daemon.exit(Duration::from_secs(20))?;
        let summary = daemon.summary()?;
        check(
            status.code() == Some(4)
                && summary["unclosed_sessions"] == 1
                && summary["store_failed"] == true,
            || format!("unexpected exit {status}: {summary}"),
        )?;
        let state: String =
            sandbox.query(&format!("SELECT state FROM sessions WHERE id='{session}'"))?;
        check(state != "closed", || format!("session is {state}"))?;
        sandbox.verify_anchors()
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
    evidenced(|| {
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
        wait_exited(
            "the running group is gone",
            stopped_by.saturating_duration_since(Instant::now()),
            vendor,
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
    })
}

/// Design §7.4 [O1.D13]: after the latch, `cancel` (of a running and of a
/// queued turn) and `close` return `store_error`, served in the window,
/// and the latch's force stop performs the cleanup: the running turn ends
/// by the force row and its group is proved absent. After the latch the
/// force path writes no queued cancellation, so the queued turn stays
/// `queued`, never submitted, for the restart handoff (§7.4).
#[test]
fn s1_f12_latch_cancel_and_close_return_store_error() -> TestResult {
    evidenced(|| {
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
    })
}

/// Design §7.4 [O1.D4]: an uncertain write (step 1's row, Task 4 A24;
/// `store.commit.reply_lost`) on a
/// turn with queued successors latches; final shutdown's batch commits the
/// turn `failed(store)` and cancels the queued turns (`cancel: null`) in
/// one transaction: `failure_batches: {committed: 1, skipped: 0}`. With
/// every later commit failing (`store.commit.fail_persistent`), the batch
/// is skipped and nothing retries it: `skipped: 1`, the turns keep their
/// states. (Characterization of the batch built in 5c4eef9.)
#[test]
fn s1_f12_latch_batch_commits_or_is_skipped() -> TestResult {
    evidenced(|| {
        for skipped in [false, true] {
            let sandbox = Sandbox::new(&script("first", 1, row_after_accepted("first")))?;
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
                // The row itself commits (it has no such seam); every later
                // commit fails.
                let later = sandbox.next_hit("store.commit.fail_persistent")?;
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
    })
}

/// Design §7.4 [O1.D4, r3.17], runtime contracts §7: the failure batch's
/// write gets no reply (`store.commit.terminal` paused on the batch's own
/// hit, the writer held before it commits). The batch gives up at its own
/// bound and is skipped, once and never retried; no terminal is invented
/// for the turn or its queued successors, which keep `running` and
/// `queued`; and final shutdown completes its report, abandoning the
/// stalled Store join to process exit: exit 4. A batch that waited past its
/// bound would leave the pipeline to the latch deadline instead
/// (`failure_batches: null`, and a `host_failure`).
#[test]
fn s1_f12_latch_batch_no_reply_is_skipped_within_the_deadline() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script("first", 1, row_after_accepted("first")))?;
        sandbox.count("store.commit.reply_lost")?;
        sandbox.count("store.commit.terminal")?;
        let mut daemon = sandbox.start()?;
        let (session, handle) = sandbox.spawn("first")?;
        sandbox.await_file("accepted.entered")?;
        sandbox.await_accepted(&session, 1)?;
        sandbox.resume(&session, &handle, "second")?;
        sandbox.resume(&session, &handle, "third")?;
        let lost = sandbox.next_hit("store.commit.reply_lost")?;
        sandbox.arm("store.commit.reply_lost", lost, "fail_io")?;
        // No terminal commits before the batch's: the turn is running until
        // final shutdown forces it.
        let batch = sandbox.next_hit("store.commit.terminal")?;
        sandbox.arm("store.commit.terminal", batch, "pause")?;
        sandbox.release("accepted")?;
        sandbox.ack(&daemon, "store.commit.reply_lost", lost, "fail_io")?;
        // The batch reached its write; its reply never comes.
        sandbox.ack(&daemon, "store.commit.terminal", batch, "pause")?;
        // The watchdog is generous and asserts no duration: on a loaded machine
        // the deadline (`failed_at + 10 s`) may pass before shutdown returns,
        // and only the outcome is the contract. A batch that waited past its
        // own bound leaves the pipeline to that deadline, and shows below as
        // `failure_batches: null` and a `host_failure`.
        let exit = daemon.exit(Duration::from_secs(90))?;
        let summary = daemon.summary()?;
        check(
            exit.code() == Some(4) && summary["store_failed"] == true,
            || format!("expected the latch's exit 4, got {exit}: {summary}"),
        )?;
        check(
            summary["failure_batches"] == json!({"committed": 0, "skipped": 1}),
            || format!("unexpected summary: {summary}"),
        )?;
        check(summary["store"] == "join_timed_out", || {
            format!("the stalled Store join was not abandoned: {summary}")
        })?;
        // The turn and its two queued successors stay unresolved.
        check(summary["unresolved_turns"] == 3, || {
            format!("unexpected unresolved turns: {summary}")
        })?;
        check(summary["host_failure"].is_null(), || {
            format!("Host reconciliation failed: {summary}")
        })?;
        sandbox.verify_anchors()?;
        let turns: String = sandbox.query(&format!(
            "SELECT group_concat(state, ',') FROM
         (SELECT state FROM turns WHERE session_id='{session}' ORDER BY number)"
        ))?;
        check(turns == "running,queued,queued", || {
            format!("turns: {turns}")
        })?;
        let invented: i64 = sandbox.query(&format!(
            "SELECT (SELECT count(*) FROM turns WHERE session_id='{session}'
                 AND envelope IS NOT NULL)
              + (SELECT count(*) FROM events WHERE session_id='{session}'
                 AND json_extract(event,'$.type')='turn.ended')"
        ))?;
        check(invented == 0, || {
            format!("{invented} terminal records were invented")
        })
    })
}

/// Design §6.8 steps 3–6, §7.4 [r3.3, r4.2, r4.9], carried from S2:
/// an uncertain write on turn A (step 1's row; its reply is lost,
/// `store.commit.reply_lost`) latches while turn B runs, and both run loops
/// are forced. B's run loop is held at `core.run.before_handoff`. (A's
/// handoff commits nothing after its failure, so B is first held at its
/// own handoff commit, `core.commit.before_send`, while A passes the
/// handoff seam's first occurrence; the seam's second occurrence is then
/// B's.) Meanwhile the window serves `daemon/status`, Host's early stop
/// has stopped the groups (`host.early_stop.sent`, and B's vendor is
/// gone), and final shutdown has not entered Host reconciliation
/// (`core.shutdown.reconcile_entry`) with B's handoff outstanding. After
/// release, reconciliation is entered once, B's forced terminal commits
/// with its evidence (`forced`, cleanup `quiescent`), and A's batch commits
/// (`failed(store)`), all before the exit (4).
#[test]
fn s1_f12_latch_pipeline_orders_handoffs() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(&[
            reported("b"),
            script("a", 1, row_after_accepted("a")),
        ]))?;
        let handoff = "core.run.before_handoff";
        let send = "core.commit.before_send";
        let reconcile = "core.shutdown.reconcile_entry";
        let early = "host.early_stop.sent";
        let lost = "store.commit.reply_lost";
        for point in [send, reconcile, early, lost] {
            sandbox.count(point)?;
        }
        let daemon = sandbox.start()?;
        let (b, _) = sandbox.spawn("b")?;
        sandbox.await_file("b.entered")?;
        sandbox.await_accepted(&b, 1)?;
        let vendor = vendor_pid(&sandbox)?;
        let (a, _) = sandbox.spawn("a")?;
        sandbox.await_file("accepted.entered")?;
        sandbox.await_accepted(&a, 1)?;
        // The Store's one worker serves this read after both acceptances, whose
        // hits precede it: the counts below are settled.
        sandbox.events(&a)?;
        let next = sandbox.next_hit(lost)?;
        // A's write is step 1's row, which has no send seam (Task 4 A24); B's
        // first commit after the latch is the next send.
        let b_send = sandbox.next_hit(send)?;
        sandbox.arm(lost, next, "fail_io")?;
        sandbox.arm(send, b_send, "pause")?;
        sandbox.arm(handoff, 1, "pause")?;
        sandbox.release("accepted")?;
        sandbox.ack(&daemon, lost, next, "fail_io")?;
        sandbox.ack(&daemon, send, b_send, "pause")?;
        sandbox.ack(&daemon, handoff, 1, "pause")?;
        // A is held at its handoff: arming the next occurrence keeps it held
        // until its release below.
        sandbox.arm(handoff, 2, "pause")?;
        sandbox.resume_point(handoff, 1)?;
        sandbox.resume_point(send, b_send)?;
        sandbox.ack(&daemon, handoff, 2, "pause")?;
        let status = sandbox.status()?;
        check(status["health"] == "store_failed", || {
            format!("the window's status: {status}")
        })?;
        wait_exited("B's group is gone", Duration::from_secs(3), vendor)?;
        check(sandbox.next_hit(early)? > 1, || {
            "no early stop was sent".to_owned()
        })?;
        check(sandbox.next_hit(reconcile)? == 1, || {
            "reconciliation entered with B's handoff outstanding".to_owned()
        })?;
        sandbox.resume_point(handoff, 2)?;
        let summary = daemon.latched_exit()?;
        check(sandbox.next_hit(reconcile)? == 2, || {
            "reconciliation was not entered once".to_owned()
        })?;
        check(
            summary["failure_batches"] == json!({"committed": 1, "skipped": 0}),
            || format!("summary: {summary}"),
        )?;
        let envelope = |session: &str| -> TestResult<Value> {
            let raw: String = sandbox.query(&format!(
                "SELECT envelope FROM turns WHERE session_id='{session}' AND number=1"
            ))?;
            Ok(serde_json::from_str(&raw)?)
        };
        let (a, b) = (envelope(&a)?, envelope(&b)?);
        check(
            a["state"] == "failed" && a["failure"]["class"] == "store",
            || format!("turn A: {a}"),
        )?;
        check(
            b["cancel"]["outcome"] == "forced" && b["cancel"]["cleanup"] == "quiescent",
            || format!("turn B: {b}"),
        )?;
        sandbox.disarm(handoff)?;
        sandbox.disarm(send)
    })
}

/// Design §6.8, §7.4 [r4.3, r5.7]: Host's early stop is independent of
/// Core, the dispatchers and Store. Turn B's dispatcher is parked at
/// `store.commit.step` before sending its step row (Task 4: model text is
/// no event), so the writer stays free; the daemon latches through turn A's receipt, whose
/// reply is lost (`store.commit.reply_lost`). Before B is released,
/// `host.early_stop.sent` is acknowledged for B's group (the only live
/// one) and the group is gone. After release B ends by the force row, and
/// the exit is 4. B's vendor holds after its model output, so its group is
/// live until the early stop.
#[test]
fn s1_f12_host_early_stop_independent_of_store() -> TestResult {
    evidenced(|| {
        let held = script(
            "b",
            1,
            vec![
                json!({"action":"report_pids"}),
                accepted(1),
                text("first"),
                tool_round()[0].clone(),
                tool_round()[1].clone(),
                gate("b"),
                text("observed"),
                json!({"action":"hang"}),
            ],
        );
        let sandbox = Sandbox::new(&scripts(&[held, completes("a", 1)]))?;
        sandbox.count("store.commit.step")?;
        sandbox.count("store.commit.reply_lost")?;
        let daemon = sandbox.start()?;
        let (b, _) = sandbox.spawn("b")?;
        sandbox.await_file("b.entered")?;
        sandbox.await_accepted(&b, 1)?;
        let vendor = vendor_pid(&sandbox)?;
        let send = sandbox.next_hit("store.commit.step")?;
        sandbox.arm("store.commit.step", send, "pause")?;
        sandbox.release("b")?;
        sandbox.ack(&daemon, "store.commit.step", send, "pause")?;
        sandbox.arm("host.early_stop.sent", 1, "fail_io")?;
        let lost = sandbox.next_hit("store.commit.reply_lost")?;
        sandbox.arm("store.commit.reply_lost", lost, "fail_io")?;
        sandbox.refused(&spawn_args("a", &[]), "store_error")?;
        sandbox.ack(&daemon, "store.commit.reply_lost", lost, "fail_io")?;
        sandbox.ack(&daemon, "host.early_stop.sent", 1, "fail_io")?;
        wait_exited("B's group is gone", Duration::from_secs(3), vendor)?;
        sandbox.resume_point("store.commit.step", send)?;
        daemon.latched_exit()?;
        let envelope: String = sandbox.query(&format!(
            "SELECT envelope FROM turns WHERE session_id='{b}' AND number=1"
        ))?;
        let envelope: Value = serde_json::from_str(&envelope)?;
        check(envelope["cancel"]["outcome"] == "forced", || {
            format!("B did not end by the force row: {envelope}")
        })
    })
}

/// A turn that is accepted, emits its terminal and exits 1. Host records a
/// failing exit, which Route would report as the vendor's own.
fn exits_failing(prompt: &str) -> Value {
    script(
        prompt,
        1,
        vec![
            accepted(1),
            text("observed"),
            terminal(1),
            json!({"action":"exit","code":1}),
        ],
    )
}

/// Design §6.8 pipeline step 5 [S3]: a vendor exit Route observes under the
/// daemon force is the force row (`ForceStopped`), never `process_exited`.
/// With Host's early stop wired the vendor's end may come from the force
/// itself, and Route may consume the recorded exit after the force is set.
/// This is the window behind the intermittent failure of
/// `s1_f12_host_early_stop_independent_of_store`, made deterministic: Wire is
/// paused at `wire.exit.observed`, the exit recorded and not yet returned to
/// Route, while the force is raised. The receipt of `daemon stop --force` is
/// sent after the force is set, so Route reads it once released. B then ends
/// by Core's forced terminal: no failure and no exit, a settled cancel. The
/// vendor is gone before the force, so Host's early stop finds nothing live
/// and the outcome is `requested`, not `forced`; the F12 test pins `forced`.
#[test]
fn s1_f12_exit_observed_under_force_is_the_force_row() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(&[exits_failing("b")]))?;
        sandbox.count("wire.exit.observed")?;
        let mut daemon = sandbox.start()?;
        let observed = sandbox.next_hit("wire.exit.observed")?;
        sandbox.arm("wire.exit.observed", observed, "pause")?;
        let (b, _) = sandbox.spawn("b")?;
        sandbox.ack(&daemon, "wire.exit.observed", observed, "pause")?;
        let stop = sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
        check(stop["stopping"] == true, || format!("force stop: {stop}"))?;
        sandbox.resume_point("wire.exit.observed", observed)?;
        let status = daemon.exit(Duration::from_secs(20))?;
        let envelope: String = sandbox.query(&format!(
            "SELECT envelope FROM turns WHERE session_id='{b}' AND number=1"
        ))?;
        let envelope: Value = serde_json::from_str(&envelope)?;
        check(
            envelope["failure"].is_null()
                && envelope["exit"].is_null()
                && envelope["cancel"]["settled_at"].is_string(),
            || format!("B did not end by the force row (daemon {status}): {envelope}"),
        )
    })
}

/// S1-evidence2 fix round 3 (Sol r3 finding 1): the scenario's final clean
/// stop runs inside its one teardown deadline, begun before the stop
/// command and the exit wait. The reviewer's probe: a "daemon" that never
/// exits by itself (`sleep 12`) behind a mock CLI that accepts the stop;
/// the final stop succeeded after 12 s. It must fail as a timeout by the
/// deadline instead.
#[test]
fn s1_store_harness_final_stop_is_within_the_teardown_deadline() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new(&json!({"scripts": []}))?;
        // No collection: this checks the stop helper alone.
        drop(sandbox.evidence.take());
        let mock = sandbox.root.path().join("mock-via");
        fs::write(
            &mock,
            b"#!/bin/sh\nprintf '%s\\n' '{\"sessions\":{\"active\":0},\"stopping\":true}'\n",
        )?;
        fs::set_permissions(&mock, fs::Permissions::from_mode(0o700))?;
        sandbox.via = mock;
        rusqlite::Connection::open(sandbox.state.join("store.sqlite3"))?.execute_batch(
            "CREATE TABLE anchors(anchor_id TEXT, generation TEXT, marker TEXT, socket_path TEXT,
                 phase TEXT, pid INTEGER, pgid INTEGER, uid INTEGER, boot_id TEXT,
                 pid_namespace TEXT, start_ticks INTEGER, absence_time TEXT)",
        )?;
        // The idle check's `hello` and `daemon/status`, answered once.
        let listener = std::os::unix::net::UnixListener::bind(sandbox.runtime.join("via.sock"))?;
        let peer = thread::spawn(move || -> std::io::Result<()> {
            let (mut stream, _) = listener.accept()?;
            let mut reader = BufReader::new(stream.try_clone()?);
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line)?;
                let request: Value = serde_json::from_str(&line)?;
                writeln!(
                    stream,
                    "{}",
                    json!({"jsonrpc":"2.0","id":request["id"],"result":{"sessions":{"active":0}}})
                )?;
            }
            Ok(())
        });
        let daemon = Daemon {
            child: Command::new("sleep").arg("12").spawn()?,
            sandbox: &sandbox,
            trace: sandbox.root.path().join("daemon.trace"),
        };
        let started = Instant::now();
        let result = daemon.stop_clean();
        let elapsed = started.elapsed();
        let _ = peer.join();
        let error = result.expect_err("a final stop past the teardown deadline succeeded");
        assert!(
            matches!(
                error.downcast_ref::<ScenarioError>(),
                Some(ScenarioError::Timeout(_))
            ),
            "{error}"
        );
        // The one deadline, plus a scheduling tolerance (a contract upper
        // bound, design T4-A50).
        assert!(
            elapsed <= outer_cleanup::TEARDOWN + Duration::from_secs(1),
            "the final stop returned after {elapsed:?}"
        );
        Ok(())
    })
}

/// S1-evidence2 fix round 3 (Sol r3, `via-t76`): a background call's typed
/// timeout keeps its type through the thread's result and its join, so the
/// scenario records `timeout`, not `fail`.
#[test]
fn s1_store_harness_background_timeout_stays_typed() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new(&json!({"scripts": []}))?;
        drop(sandbox.evidence.take());
        let mock = sandbox.root.path().join("hanging-via");
        fs::write(&mock, b"#!/bin/sh\nexec sleep 30\n")?;
        fs::set_permissions(&mock, fs::Permissions::from_mode(0o700))?;
        sandbox.via = mock;
        let error = joined(sandbox.background_within(&["ignored"], Duration::from_millis(300)))
            .expect_err("a hanging background call returned");
        assert!(
            matches!(
                error.downcast_ref::<ScenarioError>(),
                Some(ScenarioError::Timeout(_))
            ),
            "the background timeout lost its type: {error}"
        );
        Ok(())
    })
}
