//! Real-daemon scenario harness: private directories, a daemon child whose
//! drop records verified cleanup evidence, CLI calls and a raw C1 connection.

use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

/// One isolated daemon deployment: private state, runtime and fake sync dirs.
pub(crate) struct Sandbox {
    _root: tempfile::TempDir,
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

/// The daemon child; dropping it force-stops, reaps and records cleanup.
pub(crate) struct Daemon<'a> {
    child: Child,
    sandbox: &'a Sandbox,
    cleanup_path: PathBuf,
}

impl<'a> Daemon<'a> {
    pub(crate) fn start(sandbox: &'a Sandbox, evidence: &Evidence) -> Result<Self, ScenarioError> {
        Self::start_with(sandbox, evidence, |_| {})
    }

    /// [`Self::start`] with extra daemon environment, such as failpoint
    /// activation or a lowered test bound.
    pub(crate) fn start_with(
        sandbox: &'a Sandbox,
        evidence: &Evidence,
        configure: impl FnOnce(&mut Command),
    ) -> Result<Self, ScenarioError> {
        let mut command = sandbox.command();
        configure(&mut command);
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(evidence.dir.join("daemon.trace")).map_err(infra)?);
        let daemon = Self {
            child: command.spawn().map_err(infra)?,
            sandbox,
            cleanup_path: evidence.dir.join("cleanup.json"),
        };
        let mut daemon = daemon;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = daemon.child.try_wait().map_err(infra)? {
                return Err(failure(format!("daemon exited before readiness: {status}")));
            }
            // A direct connection: an auto-starting `via daemon status`
            // would start a second daemon, without the child's failpoints,
            // that can win `daemon.lock` over the child.
            if serving_pid(&sandbox.runtime) == Some(daemon.pid()) {
                return Ok(daemon);
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(
                    "daemon readiness deadline elapsed".to_owned(),
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// The pid of the daemon this harness started: readiness confirmed it
    /// serves the sandbox's socket.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    fn reap(&mut self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => return false,
            }
        }
        false
    }
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        let was_alive = matches!(self.child.try_wait(), Ok(None));
        let mut stop = "not_attempted";
        if was_alive {
            stop = match self.sandbox.run(
                &["daemon", "stop", "--force", "--json"],
                Duration::from_secs(2),
            ) {
                Ok(capture) if capture.timed_out => "timed_out",
                Ok(capture) if capture.status.success() => "accepted",
                Ok(_) => "refused",
                Err(_) => "unavailable",
            };
        }
        let mut kill = "not_needed";
        let mut reaped = self.reap(Duration::from_secs(12));
        if !reaped {
            kill = if self.child.kill().is_ok() {
                "sent_to_retained_child"
            } else {
                "failed"
            };
            reaped = self.reap(Duration::from_secs(1));
        }
        // Runtime §11.2 outer cleanup: a read-only anchor snapshot, verified
        // anchor control and `ESRCH` absence, never a Core reopen.
        let anchors = if reaped {
            match outer_cleanup::snapshot(&self.sandbox.state.join("store.sqlite3")) {
                Ok(rows) => outer_cleanup::verify(&rows, Instant::now() + Duration::from_secs(10)),
                Err(error) => {
                    json!({"status":"unverified","absence_proven":false,"reason":error})
                }
            }
        } else {
            json!({"status":"unverified","absence_proven":false,"reason":"daemon not reaped"})
        };
        let report = json!({
            "direct_child": {"pid":self.child.id(),"was_alive":was_alive,"stop":stop,"kill":kill,"reaped":reaped},
            "anchors": anchors,
        });
        if let Ok(mut file) = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.cleanup_path)
        {
            let _ = file.write_all(report.to_string().as_bytes());
            let _ = file.sync_all();
        }
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
    evidence
        .write(&format!("{name}.stdout"), &capture.stdout)
        .map_err(infra)?;
    evidence
        .write(&format!("{name}.stderr"), &capture.stderr)
        .map_err(infra)?;
    if capture.timed_out {
        return Err(ScenarioError::Timeout(format!("via {args:?} timed out")));
    }
    if !capture.status.success() {
        return Err(failure(format!(
            "via {args:?} exited {}: {}",
            capture.status,
            String::from_utf8_lossy(&capture.stderr)
        )));
    }
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
    evidence
        .write(&format!("{name}.stdout"), &capture.stdout)
        .map_err(infra)?;
    evidence
        .write(&format!("{name}.stderr"), &capture.stderr)
        .map_err(infra)?;
    let error: Value = serde_json::from_slice(&capture.stderr).map_err(|_| {
        failure(format!(
            "{name}: expected a {kind} request error, got exit {} stderr {}",
            capture.status,
            String::from_utf8_lossy(&capture.stderr)
        ))
    })?;
    if capture.timed_out || capture.status.code() != Some(2) || error["data"]["kind"] != kind {
        return Err(failure(format!("{name}: expected {kind}, got {error}")));
    }
    Ok(error)
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
        self.writer.write_all(line.as_bytes()).map_err(infra)?;
        self.writer.write_all(b"\n").map_err(infra)
    }

    pub(crate) fn exchange(&mut self, line: &str) -> Result<Value, ScenarioError> {
        self.send(line)?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply).map_err(infra)? == 0 {
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
