//! The daemon lifecycle through the real `via` binary (design T3 §6, §8):
//! startup and lock contention (F1–F3, F11), version mismatch (F4), idle
//! exit (F6), the `daemon stop` modes (F7), Ctrl-C on a foreground spawn
//! (F29), the re-probe loop, Host's early stop under a plain stop and a
//! drain (r5.1), and a force-path read stalled past the cutoff (§6.7,
//! r5.10). Waits are bounded waits on failpoint
//! acknowledgements, durable rows, sockets or process exit; a sleep only
//! lets time pass, never orders two events.

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
#[path = "support/process.rs"]
mod process;
#[path = "support/scenario.rs"]
#[expect(dead_code, reason = "shared support; evidenced uses its typed errors")]
mod scenario;
mod support;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
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
#[cfg(feature = "test-failpoints")]
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

/// Waits for `child` to exit; `None` when it is still running at `within`.
/// Each observation is timestamped after it returns: an exit observed only
/// after the deadline is `None`.
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

/// Waits until `ready` holds, observed by the deadline; a typed timeout
/// otherwise.
fn wait_until(what: &str, within: Duration, mut ready: impl FnMut() -> bool) -> TestResult {
    let deadline = Instant::now() + within;
    loop {
        let reached = ready();
        if Instant::now() > deadline {
            return Err(timeout(format!("never reached: {what}")));
        }
        if reached {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn check(condition: bool, message: impl FnOnce() -> String) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message().into())
    }
}

/// Whether `pid` has exited: gone, or a zombie its parent has not reaped.
/// Unreadable process state is an error, never absence.
fn gone(pid: u32) -> TestResult<bool> {
    Ok(process::exited(pid)?)
}

/// Waits until `pid` exited, observed by `deadline`; a typed timeout
/// otherwise.
fn wait_gone_by(pid: u32, deadline: Instant) -> TestResult {
    loop {
        let exited = gone(pid)?;
        if Instant::now() > deadline {
            return Err(timeout(format!(
                "process {pid} did not exit by its deadline"
            )));
        }
        if exited {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10).min(outer_cleanup::left(deadline)));
    }
}

#[cfg(feature = "test-failpoints")]
fn wait_gone(pid: u32, within: Duration) -> TestResult {
    wait_gone_by(pid, Instant::now() + within)
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
    runs: Cell<u32>,
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
    /// Declares a scenario with no Store by design: its evidence then
    /// requires none of the Store's artifacts.
    fn no_store(&self) {
        self.store_expected
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

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
        #[cfg(feature = "test-failpoints")]
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
            runs: Cell::new(0),
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

    /// A command whose daemon, started directly or by the CLI, runs the
    /// failpoint controller.
    fn command_fp(&self) -> Command {
        #[cfg(feature = "test-failpoints")]
        {
            let mut command = self.command();
            self.failpoints.activate(&mut command);
            command
        }
        #[cfg(not(feature = "test-failpoints"))]
        self.command()
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

    #[cfg(feature = "test-failpoints")]
    /// One CLI call refused with request error `kind`: the error object.
    fn refused(&self, args: &[&str], kind: &str) -> TestResult<Value> {
        let captured = self.run(args)?;
        let error: Value = serde_json::from_slice(&captured.stderr).map_err(|_| {
            format!(
                "via {args:?}: expected {kind}, got exit {} stderr {}",
                captured.status,
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
        if self.teardown.begun() {
            return Err("a daemon started after the final teardown began".into());
        }
        let run = self.runs.get() + 1;
        self.runs.set(run);
        let trace = self.root.path().join(format!("daemon-{run}.trace"));
        let mut command = self.command_fp();
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&trace)?);
        let mut daemon = Daemon {
            child: command.spawn()?,
            sandbox: self,
            trace,
            torn_down: false,
        };
        daemon.ready()?;
        Ok(daemon)
    }

    /// `daemon/status` over a direct connection: never auto-starts.
    fn status(&self) -> TestResult<Value> {
        let (mut raw, _) = Raw::hello(&self.runtime, VERSION)?;
        let reply = raw.call("daemon/status", &json!({}))?;
        reply
            .get("result")
            .cloned()
            .ok_or_else(|| format!("daemon/status refused: {reply}").into())
    }

    /// Spawns turn 1 in the background: `(session, handle)`.
    fn spawn(&self, prompt: &str) -> TestResult<(String, String)> {
        let receipt = self.ok(&[
            "spawn",
            "--harness",
            "fake",
            "--model",
            "fake",
            "--prompt",
            prompt,
            "--background",
            "--json",
        ])?;
        let session = receipt["session_id"]
            .as_str()
            .ok_or("receipt has no session")?;
        let handle = receipt["handle"].as_str().ok_or("receipt has no handle")?;
        Ok((session.to_owned(), handle.to_owned()))
    }

    fn wait(&self, address: &str) -> TestResult<Value> {
        self.ok(&["wait", address, "--timeout-ms", "30000", "--json"])
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

    #[cfg(feature = "test-failpoints")]
    /// One value read from the Store through a read-only connection.
    fn query<T: rusqlite::types::FromSql>(&self, sql: &str) -> TestResult<T> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        Ok(store.query_row(sql, [], |row| row.get(0))?)
    }

    #[cfg(feature = "test-failpoints")]
    /// The `session.closed` reason of `session`, if it closed.
    fn closed_reason(&self, session: &str) -> TestResult<Option<String>> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let mut query = store.prepare(
            "SELECT json_extract(event,'$.reason') FROM events
             WHERE session_id=?1 AND json_extract(event,'$.type')='session.closed'",
        )?;
        let mut rows = query.query([session])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    /// The scenario's final outer cleanup: proves every committed anchor's
    /// group absent, as the runtime §11.2 outer harness does, within the
    /// scenario's one teardown deadline, which this begins or joins.
    #[cfg(feature = "test-failpoints")]
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

    /// The scenario's final teardown of a daemon the CLI started (pid
    /// `pid`), within its one teardown deadline, which this begins or
    /// joins: a force-stop through `stop` for at most 2 s, recorded; the
    /// daemon's exit; the outer cleanup.
    fn stop_auto(&self, mut stop: Command, pid: u32) -> TestResult {
        let deadline = self.teardown.begin();
        stop.args(["daemon", "stop", "--force", "--json"]);
        let (record, failure) = outer_cleanup::run_within(
            &mut stop,
            deadline.min(Instant::now() + outer_cleanup::ORDINARY_STOP),
        );
        self.teardown.record(
            json!({"generation":format!("auto-{pid}"),"stop":record}),
            failure,
        );
        wait_gone_by(pid, deadline)?;
        self.verify_anchors_by(deadline)
    }
}

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

    /// Waits for the acknowledgement of a process the harness did not start
    /// (an anchor, or a daemon the CLI started) and returns its pid, after
    /// checking the acknowledgement in full.
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

/// A direct C1 connection, which never auto-starts a daemon.
struct Raw {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
}

impl Raw {
    /// Connects and says `hello` as `version`: the connection and the reply.
    fn hello(runtime: &Path, version: &str) -> TestResult<(Self, Value)> {
        let stream = UnixStream::connect(runtime.join("via.sock"))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut raw = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next: 0,
        };
        let hello = raw.call(
            "hello",
            &json!({"api_version":1,"client_version":version,"client":"s1-lifecycle"}),
        )?;
        Ok((raw, hello))
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
    /// Set once [`Daemon::finish`] or [`Daemon::shutdown`] tore it down.
    torn_down: bool,
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

    #[cfg(feature = "test-failpoints")]
    /// The bounded negative: the daemon is still running after `span`.
    fn alive_for(&mut self, span: Duration) -> TestResult {
        match wait_child(&mut self.child, span)? {
            None => Ok(()),
            Some(status) => Err(format!("the daemon exited early: {status}").into()),
        }
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

    /// The scenario's final teardown of this daemon, within its one
    /// teardown deadline, which this begins or joins (runtime §11.2): a
    /// force-stop for at most 2 s, the exit (a kill is incomplete), the
    /// outer cleanup.
    fn finish(mut self) -> TestResult {
        let deadline = self.sandbox.teardown.begin();
        self.stop_by(deadline)
    }

    /// A deliberate intermediate shutdown before a restart, with its own
    /// runtime §11.2 bound: the final teardown has not begun, and the next
    /// daemon may start.
    fn shutdown(mut self) -> TestResult {
        self.stop_by(Instant::now() + outer_cleanup::TEARDOWN)
    }

    /// Force-stops, reaps and records this daemon, then proves outer
    /// cleanup, all by `deadline`.
    fn stop_by(&mut self, deadline: Instant) -> TestResult {
        let sandbox = self.sandbox;
        let generation = format!("finish-{}", self.child.id());
        let record = sandbox.teardown.daemon_generation(
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
        self.torn_down = true;
        if record["direct_child"]["kill"] != "not_needed" {
            return Err(timeout(format!(
                "the daemon did not exit after its force-stop: {record}"
            )));
        }
        sandbox.verify_anchors_by(deadline)
    }
}

impl Drop for Daemon<'_> {
    /// Final teardown (runtime §11.2): a live daemon's drop begins, or
    /// joins, the scenario's one teardown deadline, which bounds the
    /// force-stop (at most 2 s), the exit wait and the kill's 1 s reap
    /// ([`outer_cleanup::teardown_child`]); an exited child's drop begins
    /// nothing. Either records its direct child's reap status in the
    /// teardown, for `cleanup.json`. A deliberate mid-test stop exits the
    /// daemon first ([`Daemon::exit`], [`Daemon::shutdown`]).
    fn drop(&mut self) {
        if self.torn_down {
            return;
        }
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

/// Every file under `dir` with its bytes.
fn snapshot(dir: &Path) -> TestResult<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(snapshot(&path)?);
        } else {
            files.insert(path.clone(), fs::read(&path)?);
        }
    }
    Ok(files)
}

/// `files` without the daemon's own `via.log`, which a refused start
/// appends to (bead via-23b): F11 keeps the Store's bytes untouched.
fn without_log(
    sandbox: &Sandbox,
    mut files: BTreeMap<PathBuf, Vec<u8>>,
) -> BTreeMap<PathBuf, Vec<u8>> {
    files.remove(&sandbox.state.join("via.log"));
    files
}

/// The refused start's cause is in `via.log`, as an ERROR line holding one
/// of `reasons` (bead via-23b).
fn refusal_logged(sandbox: &Sandbox, reasons: &[&str]) -> TestResult {
    let log = fs::read_to_string(sandbox.state.join("via.log"))?;
    check(
        log.lines().any(|line| {
            line.contains("ERROR")
                && line.contains("daemon startup failed")
                && reasons.iter().any(|reason| line.contains(reason))
        }),
        || format!("no startup failure in via.log:\n{log}"),
    )
}

/// F2 (design §6.1), characterization: a killed daemon leaves its socket;
/// the next daemon takes both locks, then replaces the stale socket.
#[test]
fn s1_f02_stale_socket_replaced_after_lock() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&completes("after", 1))?;
        let mut first = sandbox.start()?;
        first.child.kill()?;
        if !outer_cleanup::reap_by(&mut first.child, Instant::now() + outer_cleanup::REAP) {
            return Err(timeout("the killed daemon was not reaped in 1 s"));
        }
        check(sandbox.runtime.join("via.sock").exists(), || {
            "the killed daemon left no socket".to_owned()
        })?;
        let second = sandbox.start()?;
        check(sandbox.status()?["pid"] == second.pid(), || {
            "another daemon answered".to_owned()
        })?;
        let (session, _) = sandbox.spawn("after")?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(envelope["state"] == "completed", || envelope.to_string())?;
        second.finish()
    })
}

/// The socket's identity: inode and device.
fn socket_identity(runtime: &Path) -> TestResult<(u64, u64)> {
    let metadata = fs::symlink_metadata(runtime.join("via.sock"))?;
    Ok((metadata.dev(), metadata.ino()))
}

/// F2 (design §6.1): both locks precede any replacement of the socket. A
/// daemon that loses `daemon.lock` (exit 75), or `store.lock` (exit 4), leaves
/// a live socket exactly as it was: the same inode, still answering its owner.
/// A stale socket is replaced only under both locks (the test above).
#[test]
fn s1_f02_losing_daemon_leaves_live_socket_untouched() -> TestResult {
    evidenced(|| {
        // `daemon.lock` held by a live daemon.
        let sandbox = Sandbox::new(&json!({"scripts": []}))?;
        // An idle owner: no turn by design.
        sandbox.no_launch();
        let owner = sandbox.start()?;
        let before = socket_identity(&sandbox.runtime)?;
        let mut direct = sandbox.command();
        direct.arg("daemon");
        let loser = run_command(&mut direct, Duration::from_secs(10))?;
        check(loser.status.code() == Some(75), || {
            format!("the losing daemon: {}", loser.status)
        })?;
        check(
            socket_identity(&sandbox.runtime).ok() == Some(before),
            || "the losing daemon removed or replaced the socket".to_owned(),
        )?;
        check(sandbox.status()?["pid"] == owner.pid(), || {
            "the owner no longer answers on its socket".to_owned()
        })?;
        owner.finish()?;
        // `store.lock` held by another process, no `daemon.lock` holder: the
        // socket is one the harness listens on.
        let sandbox = Sandbox::new(&json!({"scripts": []}))?;
        // No daemon ever opens this Store.
        sandbox.no_store();
        let listener = UnixListener::bind(sandbox.runtime.join("via.sock"))?;
        listener.set_nonblocking(true)?;
        let before = socket_identity(&sandbox.runtime)?;
        let store_lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(sandbox.state.join("store.lock"))?;
        store_lock.try_lock()?;
        let mut direct = sandbox.command();
        direct.arg("daemon");
        let loser = run_command(&mut direct, Duration::from_secs(10))?;
        let stderr = String::from_utf8_lossy(&loser.stderr);
        check(
            loser.status.code() == Some(4) && stderr.contains("store.lock is held"),
            || format!("the store-lock loser: {} {stderr}", loser.status),
        )?;
        check(
            socket_identity(&sandbox.runtime).ok() == Some(before),
            || "the store-lock loser removed or replaced the socket".to_owned(),
        )?;
        let _client = UnixStream::connect(sandbox.runtime.join("via.sock"))?;
        listener
            .accept()
            .map_err(|error| format!("the harness's socket has no connection: {error}"))?;
        Ok(())
    })
}

/// F3 (design §6.1): an unsafe runtime root (a symlink, mode 0755, another
/// owner) is refused by the CLI's own check before it connects or spawns:
/// the daemon's message, exit 4, and nothing created.
#[test]
fn s1_f03_unsafe_runtime_dir_refused() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&json!({"scripts": []}))?;
        // The CLI refuses before any daemon or Store.
        sandbox.no_store();
        let target = sandbox.root.path().join("target");
        fs::DirBuilder::new().mode(0o700).create(&target)?;
        let link = sandbox.root.path().join("link");
        std::os::unix::fs::symlink(&target, &link)?;
        let open = sandbox.root.path().join("open");
        fs::DirBuilder::new().create(&open)?;
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755))?;
        let mut variants = vec![
            ("symlink", link, Some(target)),
            ("mode", open.clone(), Some(open)),
        ];
        // Another owner: a root-owned directory, when this user is not root.
        let foreign = Path::new("/root");
        let uid = rustix::process::geteuid();
        if !uid.is_root() {
            let owner = fs::symlink_metadata(foreign)?.uid();
            check(owner != uid.as_raw(), || "/root is ours".to_owned())?;
            variants.push(("owner", foreign.to_path_buf(), None));
        }
        for (name, runtime, inspect) in variants {
            let mut command = sandbox.command();
            command
                .env("VIA_RUNTIME_DIR", &runtime)
                .args(["daemon", "status", "--json"]);
            let captured = run_command(&mut command, Duration::from_secs(30))?;
            let error: Value = serde_json::from_slice(&captured.stderr).unwrap_or_default();
            check(
                captured.status.code() == Some(4)
                    && error["message"]
                        .as_str()
                        .is_some_and(|message| message.starts_with("unsafe VIA managed directory")),
                || {
                    format!(
                        "{name}: exit {} stderr {}",
                        captured.status,
                        String::from_utf8_lossy(&captured.stderr)
                    )
                },
            )?;
            if let Some(dir) = inspect {
                check(fs::read_dir(&dir)?.next().is_none(), || {
                    format!("{name}: something was created in {}", dir.display())
                })?;
            }
            check(fs::read_dir(&sandbox.state)?.next().is_none(), || {
                format!("{name}: the State directory was touched")
            })?;
        }
        Ok(())
    })
}

/// F11 (design §6.1): a newer Store schema, and a Store that fails
/// `quick_check`, are refused before any mutation: exit 4 with the reason,
/// the Store's bytes and sidecars unchanged, and no socket left behind;
/// only `via.log` gains the refusal's cause.
#[test]
fn s1_f11_newer_or_corrupt_store_refused_untouched() -> TestResult {
    // Bead via-23b: neither ever reaches `via.log`.
    const PROMPT: &str = "seed-prompt-sentinel-5d7e";
    const OUTPUT: &str = "seed-output-sentinel-a913";
    evidenced(|| {
        for variant in ["newer", "corrupt"] {
            let sandbox = Sandbox::new(&script(
                PROMPT,
                1,
                vec![
                    accepted(1),
                    json!({"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1",
                        "status":"completed","final_text":OUTPUT,"stop_reason":"end_turn"}}),
                ],
            ))?;
            let daemon = sandbox.start()?;
            let (session, _) = sandbox.spawn(PROMPT)?;
            sandbox.wait(&format!("{session}/1"))?;
            daemon.shutdown()?;
            let store = sandbox.state.join("store.sqlite3");
            let reasons: &[&str] = if variant == "newer" {
                rusqlite::Connection::open(&store)?.pragma_update(None, "user_version", 99)?;
                &["newer Store schema"]
            } else {
                // Page 2 overwritten: the file still opens, and `quick_check` fails.
                let mut bytes = fs::read(&store)?;
                let page = usize::from(u16::from_be_bytes([bytes[16], bytes[17]]));
                check(bytes.len() >= 3 * page, || {
                    "the Store is too small".to_owned()
                })?;
                bytes[page..2 * page].fill(0xA5);
                fs::write(&store, &bytes)?;
                &["corrupt Store", "malformed"]
            };
            let before = snapshot(&sandbox.state)?;
            let mut command = sandbox.command();
            command.arg("daemon");
            let captured = run_command(&mut command, Duration::from_secs(30))?;
            let stderr = String::from_utf8_lossy(&captured.stderr);
            check(
                captured.status.code() == Some(4)
                    && reasons.iter().any(|reason| stderr.contains(reason)),
                || format!("{variant}: exit {} stderr {stderr}", captured.status),
            )?;
            let (before, after) = (before, snapshot(&sandbox.state)?);
            let (before, after) = (without_log(&sandbox, before), without_log(&sandbox, after));
            refusal_logged(&sandbox, reasons)?;
            let log = fs::read_to_string(sandbox.state.join("via.log"))?;
            check(!log.contains(PROMPT) && !log.contains(OUTPUT), || {
                format!("{variant}: a prompt or output in via.log: {log}")
            })?;
            check(after == before, || {
                let changed: Vec<_> = before
                    .keys()
                    .chain(after.keys())
                    .filter(|path| before.get(*path) != after.get(*path))
                    .collect();
                format!("{variant}: the State directory changed: {changed:?}")
            })?;
            check(!sandbox.runtime.join("via.sock").exists(), || {
                format!("{variant}: the socket was left behind")
            })?;
        }
        Ok(())
    })
}

/// F11 with a WAL left beside the Store and no `-shm` (T3-S3 round 1,
/// decision 5): the newer schema version is committed only in the WAL, so
/// the refusal is correct only if the probe reads the WAL. The daemon exits
/// 4 as for a newer Store, and the Store and WAL bytes are unchanged.
/// SQLite reads a WAL only through a wal-index: the probe's documented
/// limit is the `-shm` it creates, the one file allowed to appear.
#[test]
fn s1_f11_newer_store_in_a_wal_without_shm_refused() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&completes("seed", 1))?;
        let daemon = sandbox.start()?;
        let (session, _) = sandbox.spawn("seed")?;
        sandbox.wait(&format!("{session}/1"))?;
        daemon.shutdown()?;
        let store = sandbox.state.join("store.sqlite3");
        let wal = sandbox.state.join("store.sqlite3-wal");
        let shm = sandbox.state.join("store.sqlite3-shm");
        // Everything in the main file, and no sidecar: the last connection's
        // close removes both.
        rusqlite::Connection::open(&store)?.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        check(!wal.exists() && !shm.exists(), || {
            "sidecars left after a checkpoint".to_owned()
        })?;
        // The newer version is written to a copy, whose WAL is taken while its
        // connection is still open, so nothing is checkpointed into the file.
        let scratch = sandbox.root.path().join("wal-copy");
        fs::DirBuilder::new().mode(0o700).create(&scratch)?;
        let copy = scratch.join("store.sqlite3");
        fs::copy(&store, &copy)?;
        let writer = rusqlite::Connection::open(&copy)?;
        writer.pragma_update(None, "wal_autocheckpoint", 0)?;
        writer.pragma_update(None, "user_version", 99)?;
        fs::copy(scratch.join("store.sqlite3-wal"), &wal)?;
        fs::set_permissions(&wal, fs::Permissions::from_mode(0o600))?;
        drop(writer);
        let main: i64 = rusqlite::Connection::open_with_flags(
            format!("file:{}?immutable=1", store.display()),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?
        .pragma_query_value(None, "user_version", |row| row.get(0))?;
        check(main == 10, || format!("the main file says v{main}"))?;
        let before = snapshot(&sandbox.state)?;
        let mut command = sandbox.command();
        command.arg("daemon");
        let captured = run_command(&mut command, Duration::from_secs(30))?;
        let stderr = String::from_utf8_lossy(&captured.stderr);
        check(
            captured.status.code() == Some(4) && stderr.contains("newer Store schema"),
            || format!("exit {} stderr {stderr}", captured.status),
        )?;
        let mut after = without_log(&sandbox, snapshot(&sandbox.state)?);
        let before = without_log(&sandbox, before);
        after.remove(&shm);
        refusal_logged(&sandbox, &["newer Store schema"])?;
        check(after == before, || {
            let changed: Vec<_> = before
                .keys()
                .chain(after.keys())
                .filter(|path| before.get(*path) != after.get(*path))
                .collect();
            format!("the State directory changed: {changed:?}")
        })?;
        check(!sandbox.runtime.join("via.sock").exists(), || {
            "the socket was left behind".to_owned()
        })
    })
}

/// F29 (design §6.5): Ctrl-C on a foreground `spawn` that auto-started its
/// daemon. The receipt line is printed; SIGINT to the CLI's process group
/// exits it 130 with nothing more written; the daemon, in its own process
/// group, and the turn continue, and the result is read later.
#[test]
fn s1_f29_ctrl_c_foreground_spawn_exits_130() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&held("hold", 1))?;
        let out = sandbox.root.path().join("spawn.stdout");
        let mut command = sandbox.command();
        command
            .args([
                "spawn",
                "--harness",
                "fake",
                "--model",
                "fake",
                "--prompt",
                "hold",
                "--json",
            ])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(File::create(&out)?)
            .stderr(File::create(sandbox.root.path().join("spawn.stderr"))?);
        let mut cli = command.spawn()?;
        sandbox.await_file("hold.entered")?;
        wait_until("the receipt line", Duration::from_secs(20), || {
            fs::read(&out).is_ok_and(|bytes| bytes.ends_with(b"\n"))
        })?;
        let receipt: Value = serde_json::from_slice(&fs::read(&out)?)?;
        let group = rustix::process::Pid::from_raw(i32::try_from(cli.id())?).ok_or("no pid")?;
        rustix::process::kill_process_group(group, rustix::process::Signal::INT)?;
        let status =
            wait_child(&mut cli, Duration::from_secs(10))?.ok_or("the CLI kept waiting")?;
        let lines = fs::read(&out)?
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count();
        check(status.code() == Some(130) && lines == 1, || {
            format!("interrupted CLI: {status}, {lines} lines")
        })?;
        let daemon = sandbox.status()?;
        let pid = u32::try_from(daemon["pid"].as_u64().ok_or("status has no pid")?)?;
        check(daemon["sessions"]["active"] == 1, || {
            format!("the turn did not continue: {daemon}")
        })?;
        sandbox.release("hold")?;
        let session = receipt["session_id"]
            .as_str()
            .ok_or("receipt has no session")?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(envelope["state"] == "completed", || envelope.to_string())?;
        let result = sandbox.ok(&["result", &format!("{session}/1"), "--json"])?;
        check(result["state"] == "completed", || result.to_string())?;
        sandbox.stop_auto(sandbox.command(), pid)
    })
}

/// F1 (design §6.1): two auto-starts at once make one daemon. The first
/// CLI's daemon holds `daemon.lock` at `daemon.startup.after_lock`; a
/// daemon started meanwhile exits 75 after one line; the second CLI's own
/// daemons lose the same way while it retries; once released, both CLIs
/// are answered by the first daemon.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f01_concurrent_auto_start_one_daemon() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&json!({"scripts": []}))?;
        // Daemon startup only: no turn by design.
        sandbox.no_launch();
        sandbox.arm("daemon.startup.after_lock", 1, "pause")?;
        let status_cli = |name: &str| -> TestResult<Child> {
            let mut command = sandbox.command_fp();
            command
                .args(["daemon", "status", "--json"])
                .stdin(Stdio::null())
                .stdout(File::create(
                    sandbox.root.path().join(format!("{name}.stdout")),
                )?)
                .stderr(File::create(
                    sandbox.root.path().join(format!("{name}.stderr")),
                )?);
            Ok(command.spawn()?)
        };
        let mut first = status_cli("first")?;
        let owner = sandbox.process_ack("daemon.startup.after_lock", 1, "pause")?;
        let mut direct = sandbox.command();
        direct.arg("daemon");
        let loser = run_command(&mut direct, Duration::from_secs(10))?;
        let stderr = String::from_utf8_lossy(&loser.stderr);
        check(
            loser.status.code() == Some(75)
                && stderr.lines().count() == 1
                && stderr.contains("daemon.lock"),
            || format!("the losing daemon: {} {stderr}", loser.status),
        )?;
        let mut second = status_cli("second")?;
        sandbox.resume_point("daemon.startup.after_lock", 1)?;
        for (name, cli) in [("first", &mut first), ("second", &mut second)] {
            let status = wait_child(cli, Duration::from_secs(30))?
                .ok_or_else(|| timeout(format!("the {name} CLI never returned")))?;
            let stdout = fs::read(sandbox.root.path().join(format!("{name}.stdout")))?;
            let reply: Value = serde_json::from_slice(&stdout).unwrap_or_default();
            check(status.success() && reply["pid"] == owner, || {
                format!(
                    "{name}: {status} {reply} {}",
                    fs::read_to_string(sandbox.root.path().join(format!("{name}.stderr")))
                        .unwrap_or_default()
                )
            })?;
        }
        sandbox.disarm("daemon.startup.after_lock")?;
        sandbox.stop_auto(sandbox.command(), owner)
    })
}

/// F4 (design §6.2) [r1.14]: `VIA_TEST_CLIENT_VERSION` makes the CLI (and
/// the daemon it starts) another version. A Store mismatch exits 4 and
/// stops nothing; with another client connected the plain stop is
/// `admission_refused`; idle and Store-matched, the daemon stops (`idle`,
/// exit 0) and the CLI's own version answers from a new daemon.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f04_version_mismatch_stops_only_matching_idle_daemon() -> TestResult {
    evidenced(|| {
        const OTHER: &str = "0.0.0-f04";
        let sandbox = Sandbox::new(&json!({"scripts": []}))?;
        // Idle daemons only: no turn by design.
        sandbox.no_launch();
        let mut daemon = sandbox.start()?;
        let other_version = |state: &Path| -> TestResult<Captured> {
            let mut command = sandbox.command();
            command
                .env("VIA_TEST_CLIENT_VERSION", OTHER)
                .env("VIA_STATE_DIR", state)
                .args(["daemon", "status", "--json"]);
            run_command(&mut command, Duration::from_secs(40))
        };
        // A Store mismatch: exit 4, and nothing is stopped.
        let elsewhere = sandbox.root.path().join("elsewhere");
        fs::DirBuilder::new().mode(0o700).create(&elsewhere)?;
        let captured = other_version(&elsewhere)?;
        check(
            captured.status.code() == Some(4)
                && String::from_utf8_lossy(&captured.stderr).contains("different Store path"),
            || {
                format!(
                    "store mismatch: {}",
                    String::from_utf8_lossy(&captured.stderr)
                )
            },
        )?;
        check(sandbox.status()?["pid"] == daemon.pid(), || {
            "the daemon was replaced".to_owned()
        })?;
        // Another client connected: the idle-only stop is refused.
        let (connected, _) = Raw::hello(&sandbox.runtime, VERSION)?;
        let captured = other_version(&sandbox.state)?;
        let error: Value = serde_json::from_slice(&captured.stderr).unwrap_or_default();
        check(
            captured.status.code() == Some(2)
                && error["data"]["kind"] == "admission_refused"
                && error["message"] == "daemon not idle",
            || format!("busy daemon: {} {error}", captured.status),
        )?;
        check(sandbox.status()?["pid"] == daemon.pid(), || {
            "a busy daemon was stopped".to_owned()
        })?;
        drop(connected);
        // Idle and Store-matched. A closed connection's task ends shortly after
        // the close; a refusal while one is still counted is retried in bound.
        let deadline = Instant::now() + Duration::from_secs(5);
        let captured = loop {
            let captured = other_version(&sandbox.state)?;
            let error: Value = serde_json::from_slice(&captured.stderr).unwrap_or_default();
            if error["message"] != "daemon not idle" || Instant::now() >= deadline {
                break captured;
            }
            thread::sleep(Duration::from_millis(50));
        };
        let status: Value = serde_json::from_slice(&captured.stdout).unwrap_or_default();
        check(
            captured.status.success() && status["daemon_version"] == OTHER,
            || {
                format!(
                    "idle daemon: {} {status} {}",
                    captured.status,
                    String::from_utf8_lossy(&captured.stderr)
                )
            },
        )?;
        let exit = daemon.exit(Duration::from_secs(15))?;
        let summary = daemon.summary()?;
        check(
            exit.code() == Some(0) && summary["mode"] == "idle" && status["pid"] != daemon.pid(),
            || format!("stopped daemon: {exit} {summary}"),
        )?;
        let pid = u32::try_from(status["pid"].as_u64().ok_or("status has no pid")?)?;
        let mut stop = sandbox.command();
        stop.env("VIA_TEST_CLIENT_VERSION", OTHER);
        sandbox.stop_auto(stop, pid)
    })
}

/// F4, explicit stop (design §6.2 items 3 and 5, review Y item 6): `via
/// daemon stop` from another version's CLI against an idle, Store-matched
/// daemon sends the permitted plain stop on the mismatched connection, and
/// never starts a replacement. A Store mismatch exits 4 and a connected
/// client gets `admission_refused`; `--force` is not the permitted stop, so
/// the daemon is untouched. Any replacement would hit
/// `daemon.startup.after_lock`, armed to pause and acknowledge.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f04_explicit_stop_from_mismatched_version_stops_idle_daemon_only() -> TestResult {
    evidenced(|| {
        const OTHER: &str = "0.0.0-f04-stop";
        let sandbox = Sandbox::new(&json!({"scripts": []}))?;
        // Idle daemons only: no turn by design.
        sandbox.no_launch();
        let mut daemon = sandbox.start()?;
        sandbox.arm("daemon.startup.after_lock", 1, "pause")?;
        let stop = |state: &Path, extra: &[&str]| -> TestResult<Captured> {
            let mut command = sandbox.command_fp();
            command
                .env("VIA_TEST_CLIENT_VERSION", OTHER)
                .env("VIA_STATE_DIR", state)
                .args(["daemon", "stop"])
                .args(extra)
                .arg("--json");
            run_command(&mut command, Duration::from_secs(40))
        };
        let untouched = |what: &str| -> TestResult {
            check(sandbox.status()?["pid"] == daemon.pid(), || {
                format!("{what}: the daemon was stopped or replaced")
            })
        };
        // A Store mismatch: exit 4, and nothing is sent.
        let elsewhere = sandbox.root.path().join("elsewhere");
        fs::DirBuilder::new().mode(0o700).create(&elsewhere)?;
        let captured = stop(&elsewhere, &[])?;
        check(
            captured.status.code() == Some(4)
                && String::from_utf8_lossy(&captured.stderr).contains("different Store path"),
            || {
                format!(
                    "store mismatch: {}",
                    String::from_utf8_lossy(&captured.stderr)
                )
            },
        )?;
        untouched("store mismatch")?;
        // `--force` is not the permitted plain stop: reported, daemon untouched.
        let captured = stop(&sandbox.state, &["--force"])?;
        let error: Value = serde_json::from_slice(&captured.stderr).unwrap_or_default();
        check(
            captured.status.code() == Some(2) && error["data"]["kind"] == "version_mismatch",
            || format!("forced stop: {} {error}", captured.status),
        )?;
        untouched("forced stop")?;
        // Another client connected: the idle-only stop is refused.
        let (connected, _) = Raw::hello(&sandbox.runtime, VERSION)?;
        let captured = stop(&sandbox.state, &[])?;
        let error: Value = serde_json::from_slice(&captured.stderr).unwrap_or_default();
        check(
            captured.status.code() == Some(2)
                && error["data"]["kind"] == "admission_refused"
                && error["message"] == "daemon not idle",
            || format!("busy daemon: {} {error}", captured.status),
        )?;
        untouched("busy daemon")?;
        drop(connected);
        // Idle and Store-matched. A closed connection's task ends shortly after
        // the close; a refusal while one is still counted is retried in bound.
        let deadline = Instant::now() + Duration::from_secs(5);
        let captured = loop {
            let captured = stop(&sandbox.state, &[])?;
            let error: Value = serde_json::from_slice(&captured.stderr).unwrap_or_default();
            if error["message"] != "daemon not idle" || Instant::now() >= deadline {
                break captured;
            }
            thread::sleep(Duration::from_millis(50));
        };
        let reply: Value = serde_json::from_slice(&captured.stdout).unwrap_or_default();
        check(
            captured.status.success() && reply["stopping"] == true,
            || {
                format!(
                    "idle daemon: {} {reply} {}",
                    captured.status,
                    String::from_utf8_lossy(&captured.stderr)
                )
            },
        )?;
        let exit = daemon.exit(Duration::from_secs(15))?;
        let summary = daemon.summary()?;
        check(exit.code() == Some(0) && summary["mode"] == "idle", || {
            format!("stopped daemon: {exit} {summary}")
        })?;
        // The CLI has returned and the daemon is gone: no replacement was
        // started (it would have paused and acknowledged at its lock).
        check(
            !sandbox.runtime.join("via.sock").exists()
                && sandbox
                    .failpoints
                    .ack_bytes("daemon.startup.after_lock", 1)
                    .is_err(),
            || "a replacement daemon was started".to_owned(),
        )
    })
}

/// F6 (design §6.4): with a lowered idle interval the daemon never exits
/// while a client is connected or a turn runs, and exits `idle` (0) once
/// neither holds. A client that arrives while an exiting daemon is paused
/// at `daemon.shutdown.idle_final`, its socket already gone, gets a fresh
/// daemon, which then idles out too.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f06_idle_exit_and_late_client() -> TestResult {
    evidenced(|| {
        const IDLE: Duration = Duration::from_millis(300);
        let mut sandbox = Sandbox::new(&held("hold", 1))?;
        sandbox
            .env
            .push(("VIA_TEST_IDLE_EXIT_MS", "300".to_owned()));
        let idle_exit = |daemon: &mut Daemon<'_>| -> TestResult {
            let status = daemon.exit(Duration::from_secs(15))?;
            let summary = daemon.summary()?;
            check(
                status.code() == Some(0) && summary["mode"] == "idle",
                || format!("idle exit: {status} {summary}"),
            )
        };
        // A connected client keeps the daemon.
        let mut daemon = sandbox.start()?;
        let (client, _) = Raw::hello(&sandbox.runtime, VERSION)?;
        daemon.alive_for(IDLE * 3)?;
        drop(client);
        idle_exit(&mut daemon)?;
        // A running turn keeps it; its waiter's connection too.
        let mut daemon = sandbox.start()?;
        let (session, _) = sandbox.spawn("hold")?;
        sandbox.await_file("hold.entered")?;
        daemon.alive_for(IDLE * 3)?;
        let (mut waiter, _) = Raw::hello(&sandbox.runtime, VERSION)?;
        sandbox.release("hold")?;
        let reply = waiter.call("wait", &json!({"address":format!("{session}/1")}))?;
        check(reply["result"]["state"] == "completed", || {
            reply.to_string()
        })?;
        drop(waiter);
        idle_exit(&mut daemon)?;
        // The late client.
        sandbox.arm("daemon.shutdown.idle_final", 1, "pause")?;
        let mut daemon = sandbox.start()?;
        sandbox.ack(&daemon, "daemon.shutdown.idle_final", 1, "pause")?;
        check(!sandbox.runtime.join("via.sock").exists(), || {
            "the socket outlived the listener".to_owned()
        })?;
        sandbox.disarm("daemon.shutdown.idle_final")?;
        let late_out = sandbox.root.path().join("late.stdout");
        let mut late = sandbox.command_fp();
        late.args(["daemon", "status", "--json"])
            .stdin(Stdio::null())
            .stdout(File::create(&late_out)?)
            .stderr(File::create(sandbox.root.path().join("late.stderr"))?);
        let mut late = late.spawn()?;
        sandbox.resume_point("daemon.shutdown.idle_final", 1)?;
        idle_exit(&mut daemon)?;
        let status = wait_child(&mut late, Duration::from_secs(30))?.ok_or("the late CLI hung")?;
        let reply: Value = serde_json::from_slice(&fs::read(&late_out)?).unwrap_or_default();
        check(status.success() && reply["pid"] != daemon.pid(), || {
            format!("late client: {status} {reply}")
        })?;
        let fresh = u32::try_from(reply["pid"].as_u64().ok_or("status has no pid")?)?;
        wait_gone(fresh, Duration::from_secs(15))?;
        sandbox.verify_anchors()
    })
}

/// F7's force: a running turn, a turn claimed at `core.dispatch.before_grant`
/// and a turn queued while daemon main is paused at
/// `daemon.dispatcher.before_start`, each in its own session, then a force
/// sent on a connection accepted before the pause. Both points are counted.
/// Returns the three sessions once the daemon exited 0.
#[cfg(feature = "test-failpoints")]
fn force_with_barriers(sandbox: &Sandbox, mut daemon: Daemon<'_>) -> TestResult<[String; 3]> {
    let grant = "core.dispatch.before_grant";
    let start = "daemon.dispatcher.before_start";
    let (running, _) = sandbox.spawn("run")?;
    sandbox.await_file("run.entered")?;
    let grant_at = sandbox.next_hit(grant)?;
    sandbox.arm(grant, grant_at, "pause")?;
    let (claimed, _) = sandbox.spawn("claim")?;
    sandbox.ack(&daemon, grant, grant_at, "pause")?;
    // Accepted before daemon main pauses: it carries the force.
    let (mut forcer, _) = Raw::hello(&sandbox.runtime, VERSION)?;
    let start_at = sandbox.next_hit(start)?;
    sandbox.arm(start, start_at, "pause")?;
    let (queued, _) = sandbox.spawn("queued")?;
    sandbox.ack(&daemon, start, start_at, "pause")?;
    let stop = forcer.call("daemon/stop", &json!({"force":true}))?;
    check(stop["result"]["stopping"] == true, || stop.to_string())?;
    sandbox.resume_point(start, start_at)?;
    sandbox.resume_point(grant, grant_at)?;
    let status = daemon.exit(Duration::from_secs(15))?;
    let summary = daemon.summary()?;
    check(status.code() == Some(0), || {
        format!("force exit: {status} {summary}")
    })?;
    drop(forcer);
    drop(daemon);
    sandbox.disarm(grant)?;
    sandbox.disarm(start)?;
    Ok([running, claimed, queued])
}

/// F7 (design §6.3) [O2, O3]. A plain stop is refused while a turn runs. A
/// drain finishes the accepted turn, refuses new work and closes no
/// session; after a restart the drained session resumes. A force closes
/// exactly the sessions with a running, claimed or queued turn, each
/// reached through a barrier, and leaves idle sessions open and resumable.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f07_stop_refused_drain_keeps_sessions_force_closes_unfinished() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(&[
            completes("idle", 1),
            held("hold", 1),
            completes("again", 2),
            held("run", 1),
            completes("claim", 1),
            completes("queued", 1),
            completes("later", 2),
        ]))?;
        // Plain refused, then drain.
        let mut daemon = sandbox.start()?;
        let (idle, idle_handle) = sandbox.spawn("idle")?;
        sandbox.wait(&format!("{idle}/1"))?;
        let (drained, drained_handle) = sandbox.spawn("hold")?;
        sandbox.await_file("hold.entered")?;
        let refused = sandbox.refused(&["daemon", "stop", "--json"], "admission_refused")?;
        check(refused["message"] == "sessions are active", || {
            refused.to_string()
        })?;
        let stopping = sandbox.ok(&["daemon", "stop", "--drain", "--json"])?;
        check(stopping["stopping"] == true, || stopping.to_string())?;
        sandbox.refused(
            &[
                "spawn",
                "--harness",
                "fake",
                "--model",
                "fake",
                "--prompt",
                "idle",
                "--background",
                "--json",
            ],
            "daemon_stopping",
        )?;
        sandbox.release("hold")?;
        let status = daemon.exit(Duration::from_secs(15))?;
        let summary = daemon.summary()?;
        check(
            status.code() == Some(0) && summary["mode"] == "drain",
            || format!("drain exit: {status} {summary}"),
        )?;
        drop(daemon);
        for session in [&idle, &drained] {
            check(sandbox.closed_reason(session)?.is_none(), || {
                format!("the drain closed {session}")
            })?;
        }
        // Restart: the drained session resumes. Then a force, with a running,
        // a claimed and a queued turn, each in its own session.
        let grant = "core.dispatch.before_grant";
        let start = "daemon.dispatcher.before_start";
        sandbox.count(grant)?;
        sandbox.count(start)?;
        let daemon = sandbox.start()?;
        sandbox.ok(&[
            "resume",
            &drained,
            "--prompt",
            "again",
            "--handle",
            &drained_handle,
            "--json",
        ])?;
        let again = sandbox.wait(&format!("{drained}/2"))?;
        check(again["state"] == "completed", || again.to_string())?;
        let [running, claimed, queued] = force_with_barriers(&sandbox, daemon)?;
        for session in [&running, &claimed, &queued] {
            let reason = sandbox.closed_reason(session)?;
            check(reason.as_deref() == Some("daemon_stop_force"), || {
                format!("{session} closed as {reason:?}")
            })?;
        }
        for session in [&idle, &drained] {
            check(sandbox.closed_reason(session)?.is_none(), || {
                format!("the force closed idle {session}")
            })?;
        }
        // The idle session is still resumable.
        let daemon = sandbox.start()?;
        sandbox.ok(&[
            "resume",
            &idle,
            "--prompt",
            "later",
            "--handle",
            &idle_handle,
            "--json",
        ])?;
        let later = sandbox.wait(&format!("{idle}/2"))?;
        check(later["state"] == "completed", || later.to_string())?;
        daemon.finish()
    })
}

/// F7's `Cancelling` state (design §6.3, review Y item 8): a session whose
/// only unfinished turn is a queued turn being cancelled is in the force
/// set. The queued turn is held while a `cancel` request owns it
/// (`Cancelling{request}`), its commit paused at `store.commit.cancel`, and
/// a force is accepted on a connection taken before daemon main pauses at
/// `daemon.dispatcher.before_start`. Once the cancel has committed and daemon
/// main resumes, final shutdown closes the session exactly once,
/// `daemon_stop_force`.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f07_force_set_includes_session_in_cancelling_state() -> TestResult {
    evidenced(|| {
        let start = "daemon.dispatcher.before_start";
        let commit = "store.commit.cancel";
        let sandbox = Sandbox::new(&completes("queued", 1))?;
        // The queued turn is cancelled before its launch.
        sandbox.no_launch();
        let mut daemon = sandbox.start()?;
        // Both connections are accepted before daemon main pauses: it accepts
        // no other, and one carries the force, the other the cancel.
        let (mut forcer, _) = Raw::hello(&sandbox.runtime, VERSION)?;
        let (mut canceller, _) = Raw::hello(&sandbox.runtime, VERSION)?;
        sandbox.arm(start, 1, "pause")?;
        let (session, handle) = sandbox.spawn("queued")?;
        sandbox.ack(&daemon, start, 1, "pause")?;
        // The turn is `Waiting` with no dispatcher: the cancel takes it
        // (`Cancelling{request}`) and parks at its commit.
        sandbox.arm(commit, 1, "pause")?;
        let params = json!({"session":session,"handle":handle});
        let cancel = thread::spawn(move || canceller.call("cancel", &params).map_err(sendable));
        sandbox.ack(&daemon, commit, 1, "pause")?;
        let stop = forcer.call("daemon/stop", &json!({"force":true}))?;
        check(stop["result"]["stopping"] == true, || stop.to_string())?;
        // The cancellation commits; only then does daemon main run final shutdown.
        sandbox.resume_point(commit, 1)?;
        let reply = cancel.join().map_err(|_| "the cancel thread panicked")??;
        check(
            reply["result"]["state"] == "cancelled" && reply["result"]["already_terminal"] == false,
            || format!("cancel: {reply}"),
        )?;
        sandbox.resume_point(start, 1)?;
        let status = daemon.exit(Duration::from_secs(15))?;
        let summary = daemon.summary()?;
        check(status.code() == Some(0), || {
            format!("force exit: {status} {summary}")
        })?;
        drop(forcer);
        drop(daemon);
        let closed: i64 = sandbox.query(&format!(
            "SELECT count(*) FROM events WHERE session_id='{session}'
         AND json_extract(event,'$.type')='session.closed'"
        ))?;
        let reason = sandbox.closed_reason(&session)?;
        check(
            closed == 1 && reason.as_deref() == Some("daemon_stop_force"),
            || format!("{closed} closures, reason {reason:?}"),
        )?;
        let turn: String = sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{session}' AND number=1"
        ))?;
        check(turn == "cancelled", || format!("turn state {turn}"))?;
        sandbox.disarm(start)?;
        sandbox.disarm(commit)?;
        sandbox.verify_anchors()
    })
}

/// The pid of the fake vendor, once it reported it.
#[cfg(feature = "test-failpoints")]
fn vendor_pid(sandbox: &Sandbox) -> TestResult<u32> {
    sandbox.await_file("agent.pid")?;
    Ok(fs::read_to_string(sandbox.sync.join("agent.pid"))?
        .trim()
        .parse()?)
}

/// Design §11 `s1_f12_evidence_before_terminal` [r6.3, r4.2, r5.4]: a
/// force ends a running turn whose anchor stops answering usefully, then
/// final shutdown's Host reconciliation supplies the stop evidence and the
/// absence proof, and Core has both **before** the forced terminal commits.
/// Final shutdown is paused at reconciliation's entry
/// (`core.shutdown.reconcile_entry`) and at the terminal's seam
/// (`core.shutdown.before_forced_terminal`). Core acknowledges its receipt
/// of each of the turn's reconciliation facts at
/// `core.shutdown.evidence_stopped_live` and `core.shutdown.evidence_absent`
/// (counted, never paused), both ahead of the terminal seam: at the seam the
/// group is gone, the acks are recorded and the turn has no terminal.
/// `deferred`: the anchor defers `begin_cleanup` and withholds
/// `stopped_live` (`host.anchor.defer_cleanup`, persistent) until the
/// harness disarms it at the first pause, so reconciliation's `Stop` is the
/// only source of `forced`: both acks, then `cancelled` / `forced` /
/// `quiescent`. Otherwise the anchor loses every `Stop` reply
/// (`host.anchor.final_reply_lost`, persistent): no `stopped_live` ack,
/// the absence ack alone, and the turn is `unknown`, `requested`, with
/// cleanup `quiescent` decided independently by the absence proof.
#[cfg(feature = "test-failpoints")]
fn evidence_before_terminal(deferred: bool) -> TestResult {
    let reconcile = "core.shutdown.reconcile_entry";
    let before_terminal = "core.shutdown.before_forced_terminal";
    let absence = "host.recovery.absence_commit";
    let anchor_point = if deferred {
        "host.anchor.defer_cleanup"
    } else {
        "host.anchor.final_reply_lost"
    };
    let sandbox = Sandbox::new(&script(
        "hold",
        1,
        vec![
            json!({"action":"report_pids"}),
            accepted(1),
            gate("hold"),
            terminal(1),
        ],
    ))?;
    let evidence_stopped_live = "core.shutdown.evidence_stopped_live";
    let evidence_absent = "core.shutdown.evidence_absent";
    sandbox.count(absence)?;
    sandbox.count(evidence_stopped_live)?;
    sandbox.count(evidence_absent)?;
    sandbox.arm(anchor_point, 1, "fail_io_persist")?;
    sandbox.arm(reconcile, 1, "pause")?;
    sandbox.arm(before_terminal, 1, "pause")?;
    let daemon = sandbox.start()?;
    let (session, _) = sandbox.spawn("hold")?;
    let vendor = vendor_pid(&sandbox)?;
    let stopping = sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
    check(stopping["stopping"] == true, || stopping.to_string())?;
    sandbox.ack(&daemon, reconcile, 1, "pause")?;
    let turn_state = || -> TestResult<String> {
        sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{session}' AND number=1"
        ))
    };
    // Absence proofs committed so far (Route's close and the early stop's,
    // before reconciliation).
    let proofs = sandbox.next_hit(absence)? - 1;
    if deferred {
        // Route's close found no evidence: the anchor kept the group, and
        // nothing proved its absence.
        check(
            !gone(vendor)? && turn_state()? == "running" && proofs == 0,
            || format!("the deferred anchor did not keep the group: {proofs} proofs"),
        )?;
        sandbox.disarm(anchor_point)?;
    }
    sandbox.resume_point(reconcile, 1)?;
    sandbox.ack(&daemon, before_terminal, 1, "pause")?;
    // The evidence is in and the terminal is not: the group is gone, and
    // Core has acknowledged receipt of this turn's reconciliation facts
    // (both seams precede `before_forced_terminal` in Core), each once.
    check(gone(vendor)?, || {
        "the group was not gone at the terminal seam".to_owned()
    })?;
    let got_stopped_live = sandbox.next_hit(evidence_stopped_live)? - 1;
    let got_absent = sandbox.next_hit(evidence_absent)? - 1;
    if deferred {
        // Only reconciliation's `Stop` can have supplied `stopped_live`, and
        // no absence proof existed before it (`proofs == 0`).
        check(got_stopped_live == 1 && got_absent == 1, || {
            format!("Core received {got_stopped_live} stopped_live and {got_absent} absence facts")
        })?;
    } else {
        // Every stop reply is lost: no `stopped_live` reached Core, yet the
        // absence proof did, independently.
        check(
            sandbox.failpoints.ack_bytes(anchor_point, 1).is_ok(),
            || "no lost stop reply was acknowledged".to_owned(),
        )?;
        check(got_stopped_live == 0 && got_absent == 1, || {
            format!("Core received {got_stopped_live} stopped_live and {got_absent} absence facts")
        })?;
    }
    let state = turn_state()?;
    check(state == "running", || {
        format!("the terminal committed before its seam: {state}")
    })?;
    sandbox.resume_point(before_terminal, 1)?;
    let mut daemon = daemon;
    let status = daemon.exit(Duration::from_secs(20))?;
    check(status.code() == Some(0), || {
        format!(
            "force exit: {status} {}",
            fs::read_to_string(&daemon.trace).unwrap_or_default()
        )
    })?;
    drop(daemon);
    let raw: String = sandbox.query(&format!(
        "SELECT envelope FROM turns WHERE session_id='{session}' AND number=1"
    ))?;
    let envelope: Value = serde_json::from_str(&raw)?;
    let expected = if deferred {
        ("cancelled", "forced")
    } else {
        ("unknown", "requested")
    };
    check(
        envelope["state"] == expected.0
            && envelope["cancel"]["outcome"] == expected.1
            && envelope["cancel"]["cleanup"] == "quiescent",
        || format!("terminal: {envelope}"),
    )?;
    sandbox.disarm(reconcile)?;
    sandbox.disarm(before_terminal)?;
    sandbox.verify_anchors()
}

/// Design §11 `s1_f12_evidence_before_terminal`, positive case. A
/// characterization: the code already ordered this. Mutation RED: skipping
/// the `stopped_live` acknowledgement, or delivering both after the terminal
/// seam, fails the receipt check; making `forced_terminal` ignore
/// reconciliation's `forced` evidence ends the turn `unknown`, failing the
/// envelope check.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f12_evidence_before_terminal() -> TestResult {
    evidenced(|| evidence_before_terminal(true))
}

/// Design §11 `s1_f12_evidence_before_terminal`, lost-evidence variant.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f12_evidence_before_terminal_lost_stop_evidence_is_unknown() -> TestResult {
    evidenced(|| evidence_before_terminal(false))
}

/// Design §8, §6.6: a group whose close was uncertain holds its connection
/// slot, which `daemon/status` reports in `connections.held_unproven`; once
/// the group is gone, the re-probe loop proves it absent and the slot
/// returns, with no request made.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_reprobe_returns_capacity() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new(&scripts(&[
            script(
                "slow",
                1,
                vec![json!({"action":"report_pids"}), accepted(1)],
            ),
            completes("next", 1),
        ]))?;
        sandbox
            .env
            .push(("VIA_TEST_CONNECTION_SLOTS", "1".to_owned()));
        let daemon = sandbox.start()?;
        let arm_intent = "host.anchor.after_arm_intent_commit";
        let eof_cleanup = "host.anchor.before_eof_cleanup";
        sandbox.arm(arm_intent, 1, "pause")?;
        sandbox.arm(eof_cleanup, 1, "pause")?;
        let (session, handle) = sandbox.spawn("slow")?;
        sandbox.ack(&daemon, arm_intent, 1, "pause")?;
        let reply = sandbox.ok(&["cancel", &session, "--handle", &handle, "--json"])?;
        check(reply["cancel"]["outcome"] == "requested", || {
            reply.to_string()
        })?;
        sandbox.resume_point(arm_intent, 1)?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(envelope["cancel"]["cleanup"] == "uncertain", || {
            envelope.to_string()
        })?;
        let held = sandbox.status()?;
        check(
            held["connections"] == json!({"limit":1,"in_use":1,"held_unproven":1}),
            || format!("held slot: {held}"),
        )?;
        sandbox.process_ack(eof_cleanup, 1, "pause")?;
        sandbox.resume_point(eof_cleanup, 1)?;
        sandbox.disarm(eof_cleanup)?;
        wait_until(
            "the re-probe frees the slot",
            Duration::from_secs(30),
            || {
                sandbox.status().is_ok_and(|status| {
                    status["connections"] == json!({"limit":1,"in_use":0,"held_unproven":0})
                })
            },
        )?;
        let (next, _) = sandbox.spawn("next")?;
        let envelope = sandbox.wait(&format!("{next}/1"))?;
        check(envelope["state"] == "completed", || envelope.to_string())?;
        daemon.finish()
    })
}

/// Inserts `count` proven-absent anchors named `<prefix><n>` copying a real
/// anchor's identity, and one unread anchor `unread` whose group (a pid
/// above any `pid_max`) is absent but not yet proved.
#[cfg(feature = "test-failpoints")]
fn insert_anchors(sandbox: &Sandbox, owner: &str, prefix: &str, count: u32) -> TestResult {
    let mut store = rusqlite::Connection::open(sandbox.state.join("store.sqlite3"))?;
    let tx = store.transaction()?;
    let copy = "INSERT INTO anchors(anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,phase,record_version,pid,pgid,start_ticks,absence_time)
         SELECT ?1,'g'||?1,a.marker,'/nonexistent',?2,1,a.uid,a.boot_id,a.pid_namespace,'arm_intent',1,?3,?3,1,?4
         FROM anchors a WHERE a.pid IS NOT NULL LIMIT 1";
    for index in 0..count {
        let changed = tx.execute(
            copy,
            rusqlite::params![
                format!("{prefix}{index:05}"),
                owner,
                4_194_305 + index,
                Some("1")
            ],
        )?;
        check(changed == 1, || "no real anchor to copy".to_owned())?;
    }
    let changed = tx.execute(
        copy,
        rusqlite::params!["1-unread", owner, 4_195_000, None::<String>],
    )?;
    check(changed == 1, || "no real anchor to copy".to_owned())?;
    tx.commit()?;
    Ok(())
}

/// Restarts a stopped daemon over 300 proven synthetic anchors and one
/// unread one (see [`insert_anchors`]), owned by `owner`'s turn 1.
/// Startup reconciliation stops at its deadline at a page boundary before
/// `1-unread`, which it counts unread: that anchor holds one slot.
#[cfg(feature = "test-failpoints")]
fn restart_with_unread<'a>(sandbox: &'a Sandbox, owner: &str) -> TestResult<Daemon<'a>> {
    if sandbox.teardown.begun() {
        return Err("a daemon started after the final teardown began".into());
    }
    insert_anchors(sandbox, owner, "0-synthetic", 300)?;
    let boundary = "core.recovery.page_boundary";
    sandbox.arm(boundary, 1, "pause")?;
    let run = sandbox.runs.get() + 1;
    sandbox.runs.set(run);
    let trace = sandbox.root.path().join(format!("daemon-{run}.trace"));
    let mut command = sandbox.command_fp();
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(&trace)?);
    let mut daemon = Daemon {
        child: command.spawn()?,
        sandbox,
        trace,
        torn_down: false,
    };
    sandbox.ack(&daemon, boundary, 1, "pause")?;
    // Recovery's deadline began before the acknowledgement: 5 s after it
    // has passed, so paging stops with the unread anchor counted.
    thread::sleep(Duration::from_millis(5_200));
    sandbox.resume_point(boundary, 1)?;
    daemon.ready()?;
    sandbox.disarm(boundary)?;
    Ok(daemon)
}

/// Removes the synthetic anchors of [`insert_anchors`] from a stopped
/// daemon's Store, then checks every real one is proved absent.
#[cfg(feature = "test-failpoints")]
fn remove_synthetic_anchors(sandbox: &Sandbox) -> TestResult {
    let store = rusqlite::Connection::open(sandbox.state.join("store.sqlite3"))?;
    store.execute(
        "DELETE FROM anchors WHERE anchor_id LIKE '0-synthetic%' OR anchor_id='1-unread'",
        [],
    )?;
    drop(store);
    sandbox.verify_anchors()
}

/// Design §8 [r1.15]: startup reconciliation stops at its deadline before
/// an anchor it counts as unread, which holds the only slot. A drain is
/// accepted while a turn waits for that slot. The re-probe loop's resumed
/// paging reads the anchor, proves its group absent and frees the slot;
/// the waiting turn runs and the drain finishes cleanly.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_drain_with_recovered_holdings_reprobes() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new(&scripts(&[completes("seed", 1), completes("later", 1)]))?;
        sandbox
            .env
            .push(("VIA_TEST_CONNECTION_SLOTS", "1".to_owned()));
        let daemon = sandbox.start()?;
        let (seed, _) = sandbox.spawn("seed")?;
        sandbox.wait(&format!("{seed}/1"))?;
        daemon.shutdown()?;
        let mut daemon = restart_with_unread(&sandbox, &seed)?;
        let held = sandbox.status()?;
        check(
            held["connections"] == json!({"limit":1,"in_use":1,"held_unproven":1}),
            || format!("unread holding: {held}"),
        )?;
        let waiting = "core.dispatch.awaiting_slot";
        sandbox.arm(waiting, 1, "pause")?;
        let (later, _) = sandbox.spawn("later")?;
        sandbox.ack(&daemon, waiting, 1, "pause")?;
        let stopping = sandbox.ok(&["daemon", "stop", "--drain", "--json"])?;
        check(stopping["stopping"] == true, || stopping.to_string())?;
        sandbox.resume_point(waiting, 1)?;
        let status = daemon.exit(Duration::from_secs(60))?;
        let summary = daemon.summary()?;
        check(
            status.code() == Some(0) && summary["mode"] == "drain",
            || format!("drain exit: {status} {summary}"),
        )?;
        let state: String = sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{later}' AND number=1"
        ))?;
        let proved: i64 = sandbox.query(
            "SELECT count(*) FROM anchors WHERE anchor_id='1-unread' AND absence_time IS NOT NULL",
        )?;
        check(state == "completed" && proved == 1, || {
            format!("waiting turn {state}, unread anchor proved {proved}")
        })?;
        drop(daemon);
        sandbox.disarm(waiting)?;
        remove_synthetic_anchors(&sandbox)
    })
}

/// Design §8, round 1 decision 3: resumed paging progresses while this
/// daemon owns a live group. Of two slots, an earlier daemon's unread
/// anchor holds one and a live turn of this daemon the other; a third turn
/// waits. Paging reads only the startup cohort, so it never challenges the
/// live group: it proves the unread anchor absent while that group runs,
/// and the waiting turn completes before the live one is released.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_resumed_paging_progresses_with_a_live_current_group() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new(&scripts(&[
            completes("seed", 1),
            held("live", 1),
            completes("later", 1),
        ]))?;
        sandbox
            .env
            .push(("VIA_TEST_CONNECTION_SLOTS", "2".to_owned()));
        let daemon = sandbox.start()?;
        let (seed, _) = sandbox.spawn("seed")?;
        sandbox.wait(&format!("{seed}/1"))?;
        daemon.shutdown()?;
        let mut daemon = restart_with_unread(&sandbox, &seed)?;
        let held = sandbox.status()?;
        check(
            held["connections"] == json!({"limit":2,"in_use":1,"held_unproven":1}),
            || format!("unread holding: {held}"),
        )?;
        let (live, _) = sandbox.spawn("live")?;
        sandbox.await_file("live.entered")?;
        let (later, _) = sandbox.spawn("later")?;
        let state = |session: &str| {
            sandbox.query::<String>(&format!(
                "SELECT state FROM turns WHERE session_id='{session}' AND number=1"
            ))
        };
        wait_until(
            "the waiting turn completes while the live group runs",
            Duration::from_secs(20),
            || state(&later).is_ok_and(|state| state == "completed"),
        )?;
        let running = state(&live)?;
        let proved: i64 = sandbox.query(
            "SELECT count(*) FROM anchors WHERE anchor_id='1-unread' AND absence_time IS NOT NULL",
        )?;
        check(running == "running" && proved == 1, || {
            format!("live turn {running}, unread anchor proved {proved}")
        })?;
        sandbox.release("live")?;
        let envelope = sandbox.wait(&format!("{live}/1"))?;
        check(envelope["state"] == "completed", || envelope.to_string())?;
        let stopping = sandbox.ok(&["daemon", "stop", "--json"])?;
        check(stopping["stopping"] == true, || stopping.to_string())?;
        let status = daemon.exit(Duration::from_secs(30))?;
        check(status.code() == Some(0), || format!("stop exit: {status}"))?;
        drop(daemon);
        remove_synthetic_anchors(&sandbox)
    })
}

/// Design §6.8 [r5.1]: Host's early-stop task, wired at serve start, ends
/// on Host's retire signal when no force comes. A plain stop of an idle
/// daemon and a drain that finishes a held turn both exit 0, and the
/// shutdown summary reports no pending or failed task.
#[test]
fn s1_host_early_stop_exits_on_plain_stop_and_drain() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&scripts(&[completes("plain", 1), held("drained", 1)]))?;
        let clean = |daemon: &mut Daemon<'_>, what: &str| -> TestResult {
            let status = daemon.exit(Duration::from_secs(15))?;
            let summary = daemon.summary()?;
            check(
                status.code() == Some(0)
                    && summary["disposition"] == "clean"
                    && summary["pending_joins"] == 0
                    && summary["failed_joins"] == 0,
                || format!("{what}: exit {status}, summary {summary}"),
            )
        };
        let mut daemon = sandbox.start()?;
        let (session, _) = sandbox.spawn("plain")?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(envelope["state"] == "completed", || envelope.to_string())?;
        let stopping = sandbox.ok(&["daemon", "stop", "--json"])?;
        check(stopping["stopping"] == true, || stopping.to_string())?;
        clean(&mut daemon, "plain stop")?;
        drop(daemon);
        let mut daemon = sandbox.start()?;
        let (session, _) = sandbox.spawn("drained")?;
        sandbox.await_file("drained.entered")?;
        let stopping = sandbox.ok(&["daemon", "stop", "--drain", "--json"])?;
        check(stopping["stopping"] == true, || stopping.to_string())?;
        sandbox.release("drained")?;
        clean(&mut daemon, "drain")?;
        drop(daemon);
        let daemon = sandbox.start()?;
        let envelope = sandbox.wait(&format!("{session}/1"))?;
        check(envelope["state"] == "completed", || envelope.to_string())?;
        daemon.finish()
    })
}

/// A force stop whose force-path read stalls in the Store worker.
#[cfg(feature = "test-failpoints")]
struct StalledRead {
    session: String,
    /// From the force request to `core.shutdown.reconcile_entry`.
    reconciled_after: Duration,
    /// From the force request to the daemon's exit.
    exited_after: Duration,
    status: ExitStatus,
    summary: Value,
}

/// Turn 1 runs (`hang`) and turn 2 is queued behind it. A force stop pauses
/// the dispatcher at `core.force.cancel_read`, before turn 2's cancellation
/// read; the next read the Store worker dequeues is then held at
/// `store.read.stall`, and every later read waits behind it. The dispatcher
/// abandons the read at the cutoff. `core.shutdown.reconcile_entry` is
/// paused to time Host reconciliation's start. With `release`, the worker
/// is released there; otherwise never.
#[cfg(feature = "test-failpoints")]
fn force_with_stalled_read(sandbox: &Sandbox, release: bool) -> TestResult<StalledRead> {
    let cancel_read = "core.force.cancel_read";
    let stall = "store.read.stall";
    let reconcile = "core.shutdown.reconcile_entry";
    sandbox.count(stall)?;
    sandbox.arm(cancel_read, 1, "pause")?;
    sandbox.arm(reconcile, 1, "pause")?;
    let mut daemon = sandbox.start()?;
    let (session, handle) = sandbox.spawn("hang")?;
    // Accepted, not only running: a force before the fake's acceptance is
    // settled `requested`, not `forced`, and the checks below assume it.
    let running = format!(
        "SELECT count(*) FROM turns WHERE session_id='{session}' AND state='running' \
         AND accepted_at IS NOT NULL"
    );
    wait_until("turn 1 is accepted", Duration::from_secs(20), || {
        sandbox.query::<i64>(&running).is_ok_and(|count| count == 1)
    })?;
    sandbox.ok(&[
        "resume", &session, "--prompt", "q", "--handle", &handle, "--json",
    ])?;
    let started = Instant::now();
    let stopping = sandbox.ok(&["daemon", "stop", "--force", "--json"])?;
    check(stopping["stopping"] == true, || stopping.to_string())?;
    sandbox.ack(&daemon, cancel_read, 1, "pause")?;
    let next = sandbox.next_hit(stall)?;
    sandbox.arm(stall, next, "pause")?;
    sandbox.resume_point(cancel_read, 1)?;
    sandbox.ack(&daemon, stall, next, "pause")?;
    sandbox.ack(&daemon, reconcile, 1, "pause")?;
    let reconciled_after = started.elapsed();
    if release {
        sandbox.resume_point(stall, next)?;
    }
    sandbox.resume_point(reconcile, 1)?;
    let status = daemon.exit(Duration::from_secs(20))?;
    let exited_after = started.elapsed();
    let summary = daemon.summary()?;
    drop(daemon);
    for point in [cancel_read, stall, reconcile] {
        sandbox.disarm(point)?;
    }
    Ok(StalledRead {
        session,
        reconciled_after,
        exited_after,
        status,
        summary,
    })
}

/// Design §6.8 budget table [r5.10]: a force-path read stalled in the
/// Store worker is abandoned at `deadline − 8 s`, so the dispatchers join
/// and Host reconciliation starts (`core.shutdown.reconcile_entry`) before
/// `deadline − 5 s`: under 5 s from the force request, which precedes the
/// deadline's start. With the old `deadline − 4 s` cutoff it started only
/// when the dispatcher join timed out at `deadline − 5 s`. The forced
/// running turn's terminal carries reconciliation's evidence; the queued
/// turn stays unresolved, so the exit is 4.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_shutdown_budget_read_cutoff_before_reconciliation() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "hang",
            1,
            vec![accepted(1), json!({"action":"hang"})],
        ))?;
        let run = force_with_stalled_read(&sandbox, true)?;
        check(run.reconciled_after < Duration::from_secs(5), || {
            format!(
                "reconciliation began {:?} after the force",
                run.reconciled_after
            )
        })?;
        let summary = &run.summary;
        check(
            run.status.code() == Some(4)
                && summary["unresolved_turns"] == 1
                && summary["uncommitted_turns"] == 0
                && summary["store"] == "joined",
            || format!("exit {}, summary {summary}", run.status),
        )?;
        let session = &run.session;
        let forced: String = sandbox.query(&format!(
            "SELECT state || ' ' || json_extract(envelope,'$.cancel.outcome') || ' '
                || json_extract(envelope,'$.cancel.cleanup')
         FROM turns WHERE session_id='{session}' AND number=1"
        ))?;
        check(forced == "cancelled forced quiescent", || {
            format!("turn 1: {forced}")
        })?;
        let queued: String = sandbox.query(&format!(
            "SELECT state FROM turns WHERE session_id='{session}' AND number=2"
        ))?;
        check(queued == "queued", || format!("turn 2: {queued}"))?;
        sandbox.start()?.finish()
    })
}

/// Design §6.7: a force-path read abandoned at the cutoff leaves its turn
/// unresolved, so the exit is 4 however the worker ends. Released, the
/// Store's drop joins the worker (`store: joined`); held, the join times
/// out at the final deadline (`join_timed_out`), and the daemon still exits
/// within its bound.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_force_cutoff_worker_stalled_read_is_never_clean() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&script(
            "hang",
            1,
            vec![accepted(1), json!({"action":"hang"})],
        ))?;
        for (release, store) in [(true, "joined"), (false, "join_timed_out")] {
            let run = force_with_stalled_read(&sandbox, release)?;
            let summary = &run.summary;
            check(
                run.status.code() == Some(4)
                    && summary["disposition"] == "incomplete"
                    && summary["unresolved_turns"].as_u64() >= Some(1)
                    && summary["store"] == store
                    && run.exited_after < Duration::from_secs(13),
                || {
                    format!(
                        "released {release}: exit {} after {:?}, summary {summary}",
                        run.status, run.exited_after
                    )
                },
            )?;
            // The next daemon recovers the unresolved turns, and its fake
            // re-runs no turn: every session here is forced or queued.
            let daemon = sandbox.start()?;
            daemon.shutdown()?;
        }
        Ok(())
    })
}

/// Design §6.1 step 5 (T3-S3 round 1, decision 4): the one 15 s startup
/// budget bounds `hello`. A peer that accepts the connection and never
/// answers makes the CLI give up within the budget, not after the
/// request's 30 s read timeout, and no daemon is started over it.
#[test]
fn s1_silent_peer_before_hello_is_bounded_by_the_startup_budget() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&completes("unused", 1))?;
        // No daemon starts over the silent peer: no Store.
        sandbox.no_store();
        let listener = UnixListener::bind(sandbox.runtime.join("via.sock"))?;
        let (accepted, held) = std::sync::mpsc::channel();
        let peer = thread::spawn(move || {
            // Accepts every connection and keeps it open, reading nothing.
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if accepted.send(stream).is_err() {
                    break;
                }
            }
        });
        let started = Instant::now();
        let mut command = sandbox.command();
        command.args(["daemon", "status", "--json"]);
        let captured = run_command(&mut command, Duration::from_secs(25))?;
        let elapsed = started.elapsed();
        let stderr = String::from_utf8_lossy(&captured.stderr);
        check(
            !captured.status.success() && elapsed < Duration::from_secs(17),
            || format!("exit {} after {elapsed:?}: {stderr}", captured.status),
        )?;
        check(stderr.contains("startup"), || stderr.to_string())?;
        check(held.try_recv().is_ok(), || {
            "the CLI never connected".to_owned()
        })?;
        drop(held);
        drop(peer);
        Ok(())
    })
}
