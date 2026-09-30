//! Real-daemon scenario harness: private directories, a daemon child whose
//! drop records verified cleanup evidence, CLI calls and a raw C1 connection.

use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::outer_cleanup;
use crate::scenario::{Captured, ScenarioError, run_command};
use crate::support::evidence::Evidence;

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error>>;

pub(crate) fn infra(error: impl std::fmt::Display) -> ScenarioError {
    ScenarioError::Infrastructure(error.to_string())
}

pub(crate) fn failure(detail: impl Into<String>) -> ScenarioError {
    ScenarioError::Failure(detail.into())
}

/// An I/O error: a typed timeout when a socket timeout elapsed, else an
/// infrastructure failure.
pub(crate) fn io(error: std::io::Error) -> ScenarioError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        ScenarioError::Timeout(format!("C1 exchange timed out: {error}"))
    } else {
        infra(error)
    }
}

/// Creates a directory and proves it is private (0700) before any use.
pub(crate) fn private_dir(path: &Path) -> TestResult {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    assert_private(path)
}

/// Fails unless `path` is a directory with mode exactly 0700.
pub(crate) fn assert_private(path: &Path) -> TestResult {
    let mode = fs::symlink_metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Err(format!("{} has mode {mode:o}, expected 700", path.display()).into());
    }
    Ok(())
}

/// One isolated daemon deployment: private state, runtime and fake sync
/// dirs, and the scenario's one final teardown (runtime §11.2), which every
/// daemon guard of the sandbox records into.
pub(crate) struct Sandbox {
    _root: tempfile::TempDir,
    pub(crate) teardown: outer_cleanup::Teardown,
    /// Daemon generations started so far, naming each one's trace header
    /// and cleanup report.
    generations: AtomicUsize,
    pub(crate) via: PathBuf,
    pub(crate) fake: PathBuf,
    pub(crate) fixture: PathBuf,
    pub(crate) state: PathBuf,
    pub(crate) runtime: PathBuf,
    pub(crate) sync: PathBuf,
}

impl Sandbox {
    /// Writes `fixture` as the fake scenario and creates the private directories.
    pub(crate) fn new(fixture: &Value) -> TestResult<Self> {
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
        assert_private(root.path())?;
        let state = root.path().join("state");
        let runtime = root.path().join("runtime");
        let sync = root.path().join("sync");
        for dir in [&state, &runtime, &sync] {
            private_dir(dir)?;
        }
        let fixture_path = root.path().join("fixture.json");
        fs::write(&fixture_path, serde_json::to_vec(fixture)?)?;
        Ok(Self {
            _root: root,
            teardown: outer_cleanup::Teardown::new(),
            generations: AtomicUsize::new(0),
            via,
            fake,
            fixture: fixture_path,
            state,
            runtime,
            sync,
        })
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(&self.via);
        command.env_clear();
        command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        command.env("VIA_STATE_DIR", &self.state);
        command.env("VIA_RUNTIME_DIR", &self.runtime);
        command.env("VIA_FAKE_AGENT_BINARY", &self.fake);
        command.env("VIA_FAKE_SCENARIO", &self.fixture);
        command.env("VIA_FAKE_SYNC_DIR", &self.sync);
        command
    }

    pub(crate) fn run(&self, args: &[&str], timeout: Duration) -> TestResult<Captured> {
        let mut command = self.command();
        command.args(args);
        run_command(&mut command, timeout)
    }

    /// Waits until the fake agent entered gate `name`.
    pub(crate) fn await_gate(&self, name: &str) -> Result<(), ScenarioError> {
        let entered = self.sync.join(format!("{name}.entered"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !entered.exists() {
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(format!("fake did not enter {name}")));
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    pub(crate) fn release_gate(&self, name: &str) -> Result<(), ScenarioError> {
        fs::write(self.sync.join(format!("{name}.release")), b"").map_err(infra)
    }

    /// Counts rows in a live Store table through a read-only connection.
    pub(crate) fn count(&self, sql: &str) -> Result<i64, ScenarioError> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(infra)?;
        store.query_row(sql, [], |row| row.get(0)).map_err(infra)
    }
}

/// A deterministic dump of the live Store's session, turn, event, operation
/// and anchor rows, for before/after comparisons around a refused request.
pub(crate) fn store_dump(state: &Path) -> Result<String, ScenarioError> {
    use std::fmt::Write as _;
    let store = rusqlite::Connection::open_with_flags(
        state.join("store.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(infra)?;
    let mut dump = String::new();
    for table in ["sessions", "turns", "events", "operations", "anchors"] {
        let mut statement = store
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .map_err(infra)?;
        let columns = statement.column_count();
        let mut rows = statement.query([]).map_err(infra)?;
        while let Some(row) = rows.next().map_err(infra)? {
            dump.push_str(table);
            for index in 0..columns {
                let value: rusqlite::types::Value = row.get(index).map_err(infra)?;
                write!(dump, " {value:?}").map_err(infra)?;
            }
            dump.push('\n');
        }
    }
    Ok(dump)
}

/// The daemon child of one generation. Dropping it is the scenario's final
/// teardown of it; [`Daemon::shutdown`] is a deliberate intermediate one
/// before a restart. Either force-stops, reaps and records the generation
/// (runtime §11.2) in `cleanup-<generation>.json` and the sandbox's
/// teardown, which [`collect_available`] validates as a whole.
pub(crate) struct Daemon<'a> {
    child: Child,
    sandbox: &'a Sandbox,
    generation: usize,
    report: PathBuf,
    /// Set once [`Daemon::shutdown`] tore it down.
    torn_down: bool,
}

impl<'a> Daemon<'a> {
    pub(crate) fn start(sandbox: &'a Sandbox, evidence: &Evidence) -> Result<Self, ScenarioError> {
        Self::start_with(sandbox, evidence, |_| {})
    }

    /// [`Self::start`] with extra daemon environment, such as failpoint
    /// activation or a lowered test bound. No daemon starts once the
    /// scenario's final teardown began: a live daemon dropped mid-test
    /// fails loudly instead of shortening that teardown. Each generation
    /// appends to `daemon.trace` under its own header.
    pub(crate) fn start_with(
        sandbox: &'a Sandbox,
        evidence: &Evidence,
        configure: impl FnOnce(&mut Command),
    ) -> Result<Self, ScenarioError> {
        if sandbox.teardown.begun() {
            return Err(infra("a daemon started after the final teardown began"));
        }
        let generation = sandbox.generations.fetch_add(1, Ordering::Relaxed) + 1;
        let mut trace = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(evidence.dir.join("daemon.trace"))
            .map_err(infra)?;
        writeln!(trace, "=== daemon generation {generation} ===").map_err(infra)?;
        let mut command = sandbox.command();
        configure(&mut command);
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(trace);
        let mut daemon = Self {
            child: command.spawn().map_err(infra)?,
            sandbox,
            generation,
            report: evidence.dir.join(format!("cleanup-{generation}.json")),
            torn_down: false,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = daemon.child.try_wait().map_err(infra)? {
                return Err(failure(format!("daemon exited before readiness: {status}")));
            }
            // A direct connection: an auto-starting `via daemon status`
            // would start a second daemon, without the child's failpoints,
            // that can win `daemon.lock` over the child.
            let ready = serving_pid(&sandbox.runtime) == Some(daemon.pid());
            if Instant::now() > deadline {
                return Err(ScenarioError::Timeout(
                    "daemon readiness deadline elapsed".to_owned(),
                ));
            }
            if ready {
                return Ok(daemon);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// The pid of the daemon this harness started: readiness confirmed it
    /// serves the sandbox's socket.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// A deliberate intermediate shutdown before a restart, with its own
    /// runtime §11.2 bound: the final teardown has not begun, so the next
    /// generation may start. Recorded like the final one; a daemon that
    /// needed a kill is a timeout, any other cleanup failure an
    /// infrastructure failure.
    pub(crate) fn shutdown(mut self) -> Result<(), ScenarioError> {
        let record = self.tear_down(Instant::now() + outer_cleanup::TEARDOWN);
        if record["direct_child"]["kill"] != "not_needed" {
            return Err(ScenarioError::Timeout(format!(
                "the daemon did not exit after its force-stop: {record}"
            )));
        }
        if record["failures"]
            .as_array()
            .is_some_and(|failures| !failures.is_empty())
        {
            return Err(infra(format!("intermediate shutdown incomplete: {record}")));
        }
        Ok(())
    }

    /// Tears this generation down by `deadline` and records it.
    fn tear_down(&mut self, deadline: Instant) -> Value {
        self.torn_down = true;
        let sandbox = self.sandbox;
        sandbox.teardown.daemon_generation(
            &self.generation.to_string(),
            deadline,
            &mut self.child,
            Some((&sandbox.state.join("store.sqlite3"), None)),
            Some(&self.report),
            |by| {
                outer_cleanup::run_within(
                    sandbox
                        .command()
                        .args(["daemon", "stop", "--force", "--json"]),
                    by,
                )
            },
        )
    }
}

impl Drop for Daemon<'_> {
    /// The scenario's final teardown of this generation (runtime §11.2):
    /// the drop, of a live or an exited daemon, begins or joins the one
    /// teardown deadline, which bounds the force-stop (at most 2 s), the
    /// exit wait, the kill's 1 s reap and the anchor cleanup. A deliberate
    /// stop before a restart is [`Daemon::shutdown`].
    fn drop(&mut self) {
        if self.torn_down {
            return;
        }
        // Final, live or exited: it begins or joins the scenario's one
        // deadline (S1-evidence2 fix round 3, Sol r3 finding 1). Only an
        // explicit `shutdown()` has its own bound.
        let deadline = self.sandbox.teardown.begin();
        self.tear_down(deadline);
    }
}

/// The scenario's evidence after every daemon generation was torn down:
/// the Store's backup, `via.log` and the evidence folders, then the whole
/// teardown's report, `cleanup.json` ([`outer_cleanup::Teardown::summary`]),
/// which must be complete: at least one generation, every direct child
/// reaped, every anchor cleanup proved and no recorded failure. The report
/// is written before any evidence copy can fail, and a copy failure is
/// returned only after it.
pub(crate) fn collect_available(
    evidence: &Evidence,
    state: &Path,
    teardown: &outer_cleanup::Teardown,
) -> Result<(), ScenarioError> {
    let summary = teardown.summary();
    let written = outer_cleanup::write_report(&evidence.dir.join("cleanup.json"), &summary);
    let mut copies = Vec::new();
    let store = state.join("store.sqlite3");
    if store.is_file()
        && let Err(error) = evidence.backup_store(&store)
    {
        copies.push(format!("store backup: {error}"));
    }
    // The daemon's own trace after startup (Task 4 design §7.6).
    for name in ["via.log", "via.log.1"] {
        let log = state.join(name);
        if log.is_file()
            && let Err(error) = fs::read(&log)
                .map_err(Into::into)
                .and_then(|bytes| evidence.write(name, &bytes))
        {
            copies.push(format!("{name}: {error}"));
        }
    }
    let folders = state.join("evidence");
    if folders.is_dir()
        && let Err(error) = evidence.copy_evidence(&folders)
    {
        copies.push(format!("evidence folders: {error}"));
    }
    written.map_err(infra)?;
    if summary["complete"] != true {
        return Err(infra(format!(
            "outer cleanup is incomplete: {}",
            summary["failures"]
        )));
    }
    if copies.is_empty() {
        Ok(())
    } else {
        Err(infra(copies.join("; ")))
    }
}

/// Runs one successful CLI call and records its output as evidence.
pub(crate) fn cli(
    sandbox: &Sandbox,
    evidence: &Evidence,
    name: &str,
    args: &[&str],
) -> Result<Value, ScenarioError> {
    let capture = sandbox.run(args, Duration::from_secs(20)).map_err(infra)?;
    // The captured outcome first; a lost output write is attached to it,
    // never in its place (S1-evidence2 fix round 2, finding 5).
    let written = write_output(evidence, name, &capture);
    if capture.timed_out {
        return Err(ScenarioError::Timeout(format!(
            "via {args:?} timed out{}{}",
            capture.notes(),
            note(written.as_ref().err())
        )));
    }
    if !capture.status.success() {
        return Err(failure(format!(
            "via {args:?} exited {}: {}{}",
            capture.status,
            String::from_utf8_lossy(&capture.stderr),
            note(written.as_ref().err())
        )));
    }
    written?;
    serde_json::from_slice(&capture.stdout).map_err(infra)
}

/// Runs one CLI call that must be refused with request error `kind`; returns
/// the error object.
pub(crate) fn refused(
    sandbox: &Sandbox,
    evidence: &Evidence,
    name: &str,
    args: &[&str],
    kind: &str,
) -> Result<Value, ScenarioError> {
    let capture = sandbox.run(args, Duration::from_secs(20)).map_err(infra)?;
    let written = write_output(evidence, name, &capture);
    // A timeout is classified before the refusal is interpreted
    // (S1-evidence2 fix round 2, finding 4).
    if capture.timed_out {
        return Err(ScenarioError::Timeout(format!(
            "{name}: via {args:?} timed out{}{}",
            capture.notes(),
            note(written.as_ref().err())
        )));
    }
    let error: Value = serde_json::from_slice(&capture.stderr).map_err(|_| {
        failure(format!(
            "{name}: expected a {kind} request error, got exit {} stderr {}{}",
            capture.status,
            String::from_utf8_lossy(&capture.stderr),
            note(written.as_ref().err())
        ))
    })?;
    if capture.status.code() != Some(2) || error["data"]["kind"] != kind {
        return Err(failure(format!(
            "{name}: expected {kind}, got {error}{}",
            note(written.as_ref().err())
        )));
    }
    written?;
    Ok(error)
}

/// Writes a call's stdout and stderr as `<name>.stdout` and `<name>.stderr`.
pub(crate) fn write_output(
    evidence: &Evidence,
    name: &str,
    capture: &Captured,
) -> Result<(), ScenarioError> {
    evidence
        .write(&format!("{name}.stdout"), &capture.stdout)
        .and_then(|()| evidence.write(&format!("{name}.stderr"), &capture.stderr))
        .map_err(|error| infra(format!("{name} output not written: {error}")))
}

/// An attached failure, as a suffix for an error detail.
pub(crate) fn note(error: Option<&ScenarioError>) -> String {
    error.map_or_else(String::new, |error| format!(" ({})", error.detail()))
}

/// Reads a session's durable events through `via events`.
pub(crate) fn events(
    sandbox: &Sandbox,
    evidence: &Evidence,
    name: &str,
    session: &str,
) -> Result<Vec<Value>, ScenarioError> {
    let page = cli(sandbox, evidence, name, &["events", session, "--json"])?;
    page["events"]
        .as_array()
        .cloned()
        .ok_or_else(|| failure(format!("events page has no events: {page}")))
}

/// A raw C1 connection after `hello`, for lost-reply and byte-level requests.
pub(crate) struct Raw {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Raw {
    pub(crate) fn open(sandbox: &Sandbox) -> Result<Self, ScenarioError> {
        Self::open_at(&sandbox.runtime)
    }

    /// [`Self::open`] on the socket in `runtime`.
    pub(crate) fn open_at(runtime: &Path) -> Result<Self, ScenarioError> {
        let stream = UnixStream::connect(runtime.join("via.sock")).map_err(infra)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(infra)?;
        let mut raw = Self {
            reader: BufReader::new(stream.try_clone().map_err(infra)?),
            writer: stream,
        };
        let params =
            json!({"api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"s1-test"});
        let hello = raw.exchange(
            &json!({"jsonrpc":"2.0","id":0,"method":"hello","params":params}).to_string(),
        )?;
        if hello["result"]["api_version"] != 1 {
            return Err(infra(format!("hello refused: {hello}")));
        }
        Ok(raw)
    }

    /// Writes one request line.
    pub(crate) fn send(&mut self, line: &str) -> Result<(), ScenarioError> {
        self.writer.write_all(line.as_bytes()).map_err(io)?;
        self.writer.write_all(b"\n").map_err(io)
    }

    pub(crate) fn exchange(&mut self, line: &str) -> Result<Value, ScenarioError> {
        self.send(line)?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply).map_err(io)? == 0 {
            return Err(failure("daemon closed the connection"));
        }
        serde_json::from_str(&reply).map_err(infra)
    }

    /// Sends one request and disconnects without reading its reply: a lost reply.
    pub(crate) fn send_and_drop(mut self, line: &str) -> Result<(), ScenarioError> {
        self.send(line)?;
        // Give the daemon the request before the peer closes.
        thread::sleep(Duration::from_millis(200));
        Ok(())
    }
}

/// `daemon/status` over a direct connection to the socket in `runtime`:
/// unlike `via daemon status`, it never auto-starts a daemon, and a stale
/// socket file refuses it.
pub(crate) fn direct_status(runtime: &Path) -> Result<Value, ScenarioError> {
    let reply = Raw::open_at(runtime)?.exchange(&request(1, "daemon/status", &json!({})))?;
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| failure(format!("daemon/status refused: {reply}")))
}

/// The pid of the daemon serving `runtime`'s socket, if one answers:
/// readiness compares it with the child the harness started.
pub(crate) fn serving_pid(runtime: &Path) -> Option<u32> {
    direct_status(runtime)
        .ok()
        .and_then(|status| status["pid"].as_u64())
        .and_then(|pid| u32::try_from(pid).ok())
}

/// One JSON-RPC request line with `params` serialized exactly as the CLI does.
pub(crate) fn request(id: u64, method: &str, params: &Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string()
}
