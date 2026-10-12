//! Claude Code through the real `via` binary, daemon and SQLite Store
//! (x.3.2 C3; vendor packet §9; adapter design §7 S-LAUNCH). Every
//! scenario's `daemon.json` names `harnesses.claude.binary`:
//! `<root>/vendor/claude`, a link to the replaying fake beside
//! `claude.replay.json`. That fixture is a lifetimes file (launch *n* runs
//! lifetime *n*), which the scenario rewrites before each turn, since a
//! `--resume` lifetime pins the UUID the daemon's session derived (ruling
//! Q4). The fake reports its side on `claude.launches` (one pid per start)
//! and `claude.progress` (its gates and every input line it read).
//! Waits are bounded waits on CLI replies, durable rows, the fake's logs or
//! process exit; no sleep orders two events. Each scenario emits the S1
//! evidence artifact (runtime §11.2): with every replaying binary's logs,
//! and `fixtures.json`, every replay and wrapper script as written, which
//! its `fixture_sha256` hashes. A positive turn also requires the fake's
//! exit 0: its replay validation passed.

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[cfg(feature = "test-failpoints")]
#[path = "support/failpoints.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod failpoints;
#[path = "support/outer_cleanup.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod outer_cleanup;
#[path = "support/process.rs"]
mod process;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
#[cfg(feature = "test-failpoints")]
use std::time::SystemTime;
use std::time::{Duration, Instant};

use daemon::collect_available;
use scenario::{Captured, ScenarioError, run_command, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A valid caller handle: `h_` and 43 base64url digits whose last is `A`.
const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
/// The recipe's tool list (packet §4).
const TOOLS: &str = "Read,Write,Edit,Glob,Grep,Bash";
/// The version the fixtures' init reports: in the adapter's `checked` set.
const TESTED: &str = "2.1.285";
/// The bound on every wait for the fake's logs or a durable row.
const WAIT: Duration = Duration::from_secs(20);

// ------------------------------------------------------------- deployment

/// One scenario's private deployment.
struct Deployment {
    root: tempfile::TempDir,
    /// The scenario's one final teardown (runtime §11.2), which every
    /// daemon guard records into.
    teardown: outer_cleanup::Teardown,
    /// Daemon runs started so far, numbering their traces and reports.
    runs: AtomicUsize,
    /// Command outputs that could not be written as evidence.
    lost_outputs: Mutex<Vec<String>>,
    via: PathBuf,
    fake: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    /// The stimuli manifest the evidence's `fixture_sha256` hashes:
    /// written at collection from [`Self::stimuli`].
    fixture: PathBuf,
    /// The Claude binary's directory: the link, its replay and logs.
    vendor: PathBuf,
    /// The sessions' working directory.
    work: PathBuf,
    /// The replaying fake's link: the configured binary, or the link a
    /// wrapper script runs ([`Deployment::wrap`]). Its replay and logs
    /// sit beside it.
    replayed: RefCell<PathBuf>,
    /// Every replaying binary the scenario configured, whose logs are
    /// evidence.
    binaries: RefCell<Vec<PathBuf>>,
    /// Every stimulus the scenario wrote, in order: each replay fixture as
    /// written (a scenario rewrites its replay before later turns) and
    /// each wrapper script. Collected as `fixtures.json`, the file the
    /// evidence's `fixture_sha256` hashes.
    stimuli: RefCell<Vec<Value>>,
    #[cfg(feature = "test-failpoints")]
    failpoints: failpoints::Failpoints,
}

impl Deployment {
    fn new() -> TestResult<Self> {
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
        let [state, runtime, sync, vendor, work, home] =
            ["state", "runtime", "sync", "vendor", "work", "home"]
                .map(|name| root.path().join(name));
        for path in [&state, &runtime, &sync, &vendor, &work, &home] {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        let fixture = root.path().join("fixture.json");
        fs::write(&fixture, br#"{"scripts":[]}"#)?;
        std::os::unix::fs::symlink(&fake, vendor.join("claude"))?;
        #[cfg(feature = "test-failpoints")]
        let failpoints = failpoints::Failpoints::new(root.path())?;
        let deployment = Self {
            root,
            teardown: outer_cleanup::Teardown::new(),
            runs: AtomicUsize::new(0),
            lost_outputs: Mutex::new(Vec::new()),
            via,
            fake,
            state,
            runtime,
            fixture,
            replayed: RefCell::new(vendor.join("claude")),
            binaries: RefCell::new(vec![vendor.join("claude")]),
            stimuli: RefCell::new(Vec::new()),
            vendor,
            work,
            #[cfg(feature = "test-failpoints")]
            failpoints,
        };
        deployment.config(&json!({"harnesses":{"claude":{"binary":deployment.claude()}}}))?;
        Ok(deployment)
    }

    /// The configured Claude binary.
    fn claude(&self) -> PathBuf {
        self.vendor.join("claude")
    }

    /// Writes `<state>/daemon.json` (0600); the daemon reads it at start.
    fn config(&self, config: &Value) -> TestResult {
        let path = self.state.join("daemon.json");
        fs::write(&path, config.to_string())?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// Writes the binary `binary`'s replay: lifetime *n* for launch *n*.
    /// The replay is recorded as a stimulus and `binary` as one whose logs
    /// are evidence.
    fn replay_for(&self, binary: &Path, lifetimes: &[Value]) -> Result<(), ScenarioError> {
        let replay = json!({
            "source": "synthetic (x.3.2 C3): no vendor run; the shapes of the 2026-09-30 \
                       re-probe's recordings, trimmed to what the adapter reads",
            "lifetimes": lifetimes,
        });
        let mut path = binary.as_os_str().to_owned();
        path.push(".replay.json");
        let path = PathBuf::from(path);
        fs::write(&path, serde_json::to_vec_pretty(&replay).map_err(infra)?).map_err(infra)?;
        self.stimulus(&path, "replay", &replay);
        let mut binaries = self.binaries.borrow_mut();
        if !binaries.iter().any(|known| known == binary) {
            binaries.push(binary.to_owned());
        }
        Ok(())
    }

    /// Records one stimulus written at `path`.
    fn stimulus(&self, path: &Path, kind: &str, content: &Value) {
        self.stimuli.borrow_mut().push(json!({
            "file": self.relative(path),
            kind: content,
        }));
    }

    /// `path` relative to the deployment's root.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(self.root.path())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    /// [`Self::replay_for`] the replaying fake.
    fn replay(&self, lifetimes: &[Value]) -> Result<(), ScenarioError> {
        let binary = self.replayed.borrow().clone();
        self.replay_for(&binary, lifetimes)
    }

    /// Makes the configured binary a `/bin/sh` script that runs `prelude`,
    /// then replaces itself with the replaying fake, linked as
    /// `<vendor>/claude-real` (its replay and logs beside that link).
    fn wrap(&self, prelude: &str) -> TestResult {
        let real = self.vendor.join("claude-real");
        std::os::unix::fs::symlink(&self.fake, &real)?;
        let script = self.claude();
        fs::remove_file(&script)?;
        let text = format!("#!/bin/sh\n{prelude}\nexec {} \"$@\"\n", shell_word(&real));
        fs::write(&script, &text)?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        self.stimulus(&script, "script", &json!(text));
        self.binaries.borrow_mut().push(real.clone());
        *self.replayed.borrow_mut() = real;
        Ok(())
    }

    /// `binary`'s log with `suffix` (`launches` or `progress`), or empty.
    fn log_of(binary: &Path, suffix: &str) -> Result<String, ScenarioError> {
        let mut path = binary.as_os_str().to_owned();
        path.push(format!(".{suffix}"));
        match fs::read_to_string(PathBuf::from(path)) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(infra(error)),
        }
    }

    /// The pids of the configured binary's launches, in order.
    fn launches(&self) -> Result<Vec<u32>, ScenarioError> {
        Self::log_of(&self.replayed.borrow(), "launches")?
            .lines()
            .map(|line| line.trim().parse().map_err(infra))
            .collect()
    }

    /// Waits until the configured binary's progress log has `line`.
    fn await_progress(&self, line: &str) -> Result<(), ScenarioError> {
        let deadline = Instant::now() + WAIT;
        loop {
            if Self::log_of(&self.replayed.borrow(), "progress")?
                .lines()
                .any(|seen| seen == line)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(format!(
                    "the fake never logged {line:?}"
                )));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// The configured binary's progress lines of launch `n`.
    fn progress_of(&self, n: usize) -> Result<Vec<String>, ScenarioError> {
        let suffix = format!(" launch {n}");
        Ok(Self::log_of(&self.replayed.borrow(), "progress")?
            .lines()
            .filter(|line| line.ends_with(&suffix))
            .map(str::to_owned)
            .collect())
    }

    /// Releases launch `n`'s `await_signal` step with `SIGUSR1`.
    fn release(&self, n: usize) -> Result<(), ScenarioError> {
        let pid = *self
            .launches()?
            .get(n - 1)
            .ok_or_else(|| fail(&format!("no launch {n}")))?;
        let pid = rustix::process::Pid::from_raw(i32::try_from(pid).map_err(infra)?)
            .ok_or_else(|| fail("launch pid 0"))?;
        rustix::process::kill_process(pid, rustix::process::Signal::USR1).map_err(infra)
    }

    /// A launch's `/proc/<pid>/` file `name`, NUL-separated entries.
    fn proc_entries(&self, n: usize, name: &str) -> Result<Vec<String>, ScenarioError> {
        let pid = *self
            .launches()?
            .get(n - 1)
            .ok_or_else(|| fail(&format!("no launch {n}")))?;
        let bytes = fs::read(format!("/proc/{pid}/{name}")).map_err(infra)?;
        Ok(bytes
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| String::from_utf8_lossy(entry).into_owned())
            .collect())
    }

    /// The `--session-id` or `--resume` value launch `n` was started with,
    /// read while it waits at a gate.
    fn launch_session(&self, n: usize) -> Result<String, ScenarioError> {
        let argv = self.proc_entries(n, "cmdline")?;
        argv.iter()
            .position(|arg| arg == "--session-id" || arg == "--resume")
            .and_then(|at| argv.get(at + 1))
            .cloned()
            .ok_or_else(|| fail(&format!("launch {n} argv names no session: {argv:?}")))
    }

    /// A client command: it never carries the failpoint activation inputs.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.via);
        command.env_clear();
        command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        command.env("VIA_STATE_DIR", &self.state);
        command.env("VIA_RUNTIME_DIR", &self.runtime);
        command
    }

    /// One CLI call, recorded as evidence.
    fn run(
        &self,
        evidence: &Evidence,
        name: &str,
        args: &[&str],
    ) -> Result<Captured, ScenarioError> {
        let mut command = self.command();
        command.args(args);
        let mut capture = run_command(&mut command, WAIT).map_err(infra)?;
        if let Err(error) = daemon::write_output(evidence, name, &capture) {
            capture.attached.push(error.detail().to_owned());
            self.lost_outputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(error.detail().to_owned());
        }
        if capture.timed_out {
            return Err(ScenarioError::Timeout(format!(
                "via {args:?} timed out{}",
                capture.notes()
            )));
        }
        Ok(capture)
    }

    /// One successful CLI call's JSON output.
    fn ok(&self, evidence: &Evidence, name: &str, args: &[&str]) -> Result<Value, ScenarioError> {
        let capture = self.run(evidence, name, args)?;
        let decoded = serde_json::from_slice::<Value>(&capture.stdout);
        check(
            daemon::cli_exit_matches(args, capture.status, decoded.as_ref().ok()),
            || {
                format!(
                    "via {args:?} exited {}: {}{}",
                    capture.status,
                    String::from_utf8_lossy(&capture.stderr),
                    capture.notes()
                )
            },
        )?;
        decoded.map_err(infra)
    }

    /// One CLI call refused with request error `kind`: the error object.
    fn refused(
        &self,
        evidence: &Evidence,
        name: &str,
        args: &[&str],
        kind: &str,
    ) -> Result<Value, ScenarioError> {
        let capture = self.run(evidence, name, args)?;
        let error: Value = serde_json::from_slice(&capture.stderr).unwrap_or(Value::Null);
        check(
            capture.status.code() == Some(2) && error["data"]["kind"] == kind,
            || {
                format!(
                    "via {args:?}: expected {kind}, got exit {} {}",
                    capture.status,
                    String::from_utf8_lossy(&capture.stderr)
                )
            },
        )?;
        Ok(error)
    }

    /// `via spawn` of a Claude session in the work directory, in the
    /// background; returns the receipt.
    fn spawn(
        &self,
        evidence: &Evidence,
        name: &str,
        prompt: &str,
        extra: &[&str],
    ) -> Result<Value, ScenarioError> {
        let work = self.work.to_string_lossy().into_owned();
        let mut args = vec![
            "spawn",
            "--harness",
            "claude",
            "--model",
            "haiku",
            "--prompt",
            prompt,
            "--cwd",
            &work,
            "--handle",
            HANDLE,
            "--background",
            "--json",
        ];
        args.extend_from_slice(extra);
        self.ok(evidence, name, &args)
    }

    /// `via resume`; returns the receipt.
    fn resume(
        &self,
        evidence: &Evidence,
        name: &str,
        session: &str,
        prompt: &str,
    ) -> Result<Value, ScenarioError> {
        self.ok(
            evidence,
            name,
            &[
                "resume", session, "--prompt", prompt, "--handle", HANDLE, "--json",
            ],
        )
    }

    /// A turn's terminal envelope through `via wait`.
    fn wait(&self, evidence: &Evidence, address: &str) -> Result<Value, ScenarioError> {
        let name = format!("wait-{}", address.replace('/', "-"));
        self.ok(
            evidence,
            &name,
            &["wait", address, "--timeout-ms", "15000", "--json"],
        )
    }

    /// `via status` of a session.
    fn status(
        &self,
        evidence: &Evidence,
        name: &str,
        session: &str,
    ) -> Result<Value, ScenarioError> {
        self.ok(evidence, name, &["status", session, "--json"])
    }

    /// A read-only view of the committed rows the scenario asserts.
    fn store(&self) -> Result<rusqlite::Connection, ScenarioError> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(infra)?;
        store.busy_timeout(Duration::from_secs(1)).map_err(infra)?;
        Ok(store)
    }

    /// A session's committed events, in sequence order.
    fn events(&self, session: &str) -> Result<Vec<Value>, ScenarioError> {
        let store = self.store()?;
        let mut query = store
            .prepare("SELECT event FROM events WHERE session_id=?1 ORDER BY seq")
            .map_err(infra)?;
        let rows = query
            .query_map([session], |row| row.get::<_, String>(0))
            .map_err(infra)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(infra)?;
        rows.iter()
            .map(|event| serde_json::from_str(event).map_err(infra))
            .collect()
    }

    /// How many of a session's events are of `kind`.
    fn count(&self, session: &str, kind: &str) -> Result<usize, ScenarioError> {
        Ok(self
            .events(session)?
            .iter()
            .filter(|event| event["type"] == kind)
            .count())
    }

    /// How many rows `table` (`sessions` or `turns`) holds.
    fn rows(&self, table: &str) -> Result<usize, ScenarioError> {
        let count: i64 = self
            .store()?
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(infra)?;
        usize::try_from(count).map_err(infra)
    }

    /// Waits for turn `n`'s first durable event of `kind`.
    fn await_event(&self, session: &str, n: u32, kind: &str) -> Result<Value, ScenarioError> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(event) = self
                .events(session)?
                .into_iter()
                .find(|event| event["turn"] == n && event["type"] == kind)
            {
                return Ok(event);
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(format!(
                    "turn {n} never committed {kind}"
                )));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// A session's turn `n`: state and envelope.
    fn turn(&self, session: &str, n: u32) -> Result<(String, Value), ScenarioError> {
        let (state, envelope): (String, Option<String>) = self
            .store()?
            .query_row(
                "SELECT state,envelope FROM turns WHERE session_id=?1 AND number=?2",
                rusqlite::params![session, n],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(infra)?;
        let envelope =
            serde_json::from_str(envelope.as_deref().unwrap_or("null")).map_err(infra)?;
        Ok((state, envelope))
    }

    /// The session's anchors: `(pid, pgid, absence proved)`, in order.
    fn anchors(&self, session: &str) -> Result<Vec<(u32, u32, bool)>, ScenarioError> {
        let store = self.store()?;
        let mut query = store
            .prepare(
                "SELECT pid,pgid,absence_time IS NOT NULL FROM anchors \
                 WHERE owner_session=?1 ORDER BY rowid",
            )
            .map_err(infra)?;
        query
            .query_map([session], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(infra)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(infra)
    }

    /// Writes every committed envelope and event, read-only, as evidence.
    fn write_store_evidence(&self, evidence: &Evidence) -> Result<(), ScenarioError> {
        if !self.state.join("store.sqlite3").is_file() {
            return Ok(());
        }
        let store = self.store()?;
        for (name, sql) in [
            (
                "envelopes.ndjson",
                "SELECT envelope FROM turns WHERE envelope IS NOT NULL ORDER BY session_id, number",
            ),
            (
                "events.ndjson",
                "SELECT event FROM events ORDER BY session_id, seq",
            ),
        ] {
            let mut query = store.prepare(sql).map_err(infra)?;
            let mut lines = Vec::new();
            for line in query
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(infra)?
            {
                lines.extend_from_slice(line.map_err(infra)?.as_bytes());
                lines.push(b'\n');
            }
            evidence.write(name, &lines).map_err(infra)?;
        }
        // The fake's own side: every replaying binary's launches and
        // progress, named by its path under the root.
        for binary in self.binaries.borrow().iter() {
            let name = self.relative(binary).replace('/', "-");
            for suffix in ["launches", "progress"] {
                let text = Self::log_of(binary, suffix)?;
                evidence
                    .write(&format!("{name}.{suffix}"), text.as_bytes())
                    .map_err(infra)?;
            }
        }
        // Every stimulus as written, in order; the evidence's
        // `fixture_sha256` hashes the same bytes.
        let manifest = serde_json::to_vec_pretty(&json!({
            "stimuli": *self.stimuli.borrow(),
        }))
        .map_err(infra)?;
        fs::write(&self.fixture, &manifest).map_err(infra)?;
        evidence.write("fixtures.json", &manifest).map_err(infra)
    }

    /// Fails if any command output could not be written as evidence.
    fn outputs_written(&self) -> Result<(), ScenarioError> {
        let lost = self
            .lost_outputs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lost.is_empty() {
            Ok(())
        } else {
            Err(infra(format!(
                "command output not written: {}",
                lost.join("; ")
            )))
        }
    }
}

// ----------------------------------------------------------------- daemon

/// Directly owned daemon child of one run. Its drop records the run's
/// teardown; the scenario's `cleanup.json` covers every run, crashed ones
/// included ([`daemon::collect_available`]).
struct Daemon<'a> {
    child: Child,
    deployment: &'a Deployment,
    /// `<n>-<run>`: the run's name in the teardown.
    run: String,
    /// The run's own cleanup report, `cleanup-<n>-<run>.json`.
    report: PathBuf,
    crash_snapshot: Option<Vec<outer_cleanup::AnchorRow>>,
    /// Set once the run was torn down: its drop then does nothing.
    torn_down: bool,
}

impl<'a> Daemon<'a> {
    /// Starts a daemon and waits until it admits requests; `run` names its
    /// trace and cleanup evidence.
    fn start(
        deployment: &'a Deployment,
        evidence: &Evidence,
        run: &str,
    ) -> Result<Self, ScenarioError> {
        Self::start_with(deployment, evidence, run, &[])
    }

    /// [`Self::start`] with extra daemon-only environment.
    fn start_with(
        deployment: &'a Deployment,
        evidence: &Evidence,
        run: &str,
        env: &[(&str, &str)],
    ) -> Result<Self, ScenarioError> {
        if deployment.teardown.begun() {
            return Err(infra("a daemon started after the final teardown began"));
        }
        let number = deployment.runs.fetch_add(1, Ordering::Relaxed) + 1;
        let trace_name = |run: &str| {
            if run == "final" {
                "daemon.trace".to_owned()
            } else {
                format!("daemon-{run}.trace")
            }
        };
        let trace_file = trace_name(run);
        let run = format!("{number}-{run}");
        let mut trace = File::options()
            .create(true)
            .append(true)
            .open(evidence.dir.join(trace_file))
            .map_err(infra)?;
        writeln!(trace, "=== daemon run {run} ===").map_err(infra)?;
        let mut command = deployment.command();
        #[cfg(feature = "test-failpoints")]
        deployment.failpoints.activate(&mut command);
        for (key, value) in env {
            command.env(key, value);
        }
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(trace);
        let mut daemon = Self {
            child: command.spawn().map_err(infra)?,
            deployment,
            report: evidence.dir.join(format!("cleanup-{run}.json")),
            run,
            crash_snapshot: None,
            torn_down: false,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = daemon.child.try_wait().map_err(infra)? {
                return Err(fail(&format!("daemon exited before readiness: {status}")));
            }
            // A direct probe: never auto-starts a second daemon.
            let ready =
                daemon::serving_pid_by(&deployment.runtime, deadline) == Some(daemon.child.id());
            if Instant::now() > deadline {
                return Err(ScenarioError::Timeout("daemon readiness".to_owned()));
            }
            if ready {
                return Ok(daemon);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// The daemon's pid.
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Kills the daemon with SIGKILL, as a crash would, and reaps it. Keep
    /// the value alive until the scenario ends: its drop runs the outer
    /// cleanup, which would otherwise stop a surviving anchor before the
    /// restart.
    fn kill(&mut self) -> Result<(), ScenarioError> {
        self.crash_snapshot = Some(
            outer_cleanup::snapshot(
                &self.deployment.state.join("store.sqlite3"),
                Instant::now() + outer_cleanup::TEARDOWN,
            )
            .map_err(infra)?,
        );
        self.child.kill().map_err(infra)?;
        if !outer_cleanup::reap_by(&mut self.child, Instant::now() + outer_cleanup::REAP) {
            return Err(ScenarioError::Timeout(
                "the killed daemon was not reaped in 1 s".to_owned(),
            ));
        }
        Ok(())
    }

    /// A deliberate intermediate shutdown before a restart, with its own
    /// runtime §11.2 bound.
    fn shutdown(mut self) -> Result<(), ScenarioError> {
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

    /// Tears this run down by `deadline` and records it.
    fn tear_down(&mut self, deadline: Instant) -> Value {
        self.torn_down = true;
        let deployment = self.deployment;
        let rows = self.crash_snapshot.take();
        deployment.teardown.daemon_generation(
            &self.run,
            deadline,
            &mut self.child,
            Some((&deployment.state.join("store.sqlite3"), rows)),
            Some(&self.report),
            |by| {
                outer_cleanup::run_within(
                    deployment
                        .command()
                        .args(["daemon", "stop", "--force", "--json"]),
                    by,
                )
            },
        )
    }
}

impl Drop for Daemon<'_> {
    /// The scenario's final teardown of this run (runtime §11.2): it
    /// begins, or joins, the scenario's one teardown deadline.
    fn drop(&mut self) {
        if self.torn_down {
            return;
        }
        let deadline = self.deployment.teardown.begin();
        self.tear_down(deadline);
    }
}

// -------------------------------------------------------------- scenarios

fn infra(error: impl std::fmt::Display) -> ScenarioError {
    ScenarioError::Infrastructure(error.to_string())
}

fn fail(detail: &str) -> ScenarioError {
    ScenarioError::Failure(detail.to_owned())
}

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(ScenarioError::Failure(detail()))
    }
}

/// Runtime §11.2: a positive turn is `completed` and its replaying fake
/// ran its whole replay ([`replay_ran`]).
fn completed(envelope: &Value) -> bool {
    envelope["state"] == "completed" && replay_ran(envelope, 0)
}

/// The turn's process exited `code`, its lifetime's own: the fake
/// validated its whole replay (every expected input, nothing extra, EOF
/// where awaited). A failed check exits it 3 instead, whatever the turn's
/// own outcome.
fn replay_ran(envelope: &Value, code: i32) -> bool {
    envelope["exit"] == json!({"code": code, "signal": null})
}

/// Runs `action` on a fresh deployment, then collects its evidence.
fn scenario(
    name: &str,
    action: impl FnOnce(&Deployment, &Evidence) -> Result<(), ScenarioError>,
) -> TestResult {
    let deployment = Deployment::new()?;
    let evidence = Evidence::new(name, &deployment.fake, &deployment.fixture)?;
    run_scenario(
        evidence,
        |evidence| action(&deployment, evidence),
        |evidence| {
            let failures: Vec<String> = [
                deployment.write_store_evidence(evidence),
                collect_available(evidence, &deployment.state, &deployment.teardown),
                deployment.outputs_written(),
            ]
            .into_iter()
            .filter_map(Result::err)
            .map(|error| error.detail().to_owned())
            .collect();
            if failures.is_empty() {
                Ok(())
            } else {
                Err(infra(failures.join("; ")))
            }
        },
    )
    .require_pass()
}

fn session_of(receipt: &Value) -> Result<String, ScenarioError> {
    receipt["session_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| fail(&format!("receipt has no session: {receipt}")))
}

// --------------------------------------------------------------- fixtures

/// How a launch names its vendor session.
#[derive(Clone, Copy)]
enum Launch<'a> {
    /// `--session-id`, captured as `${sid}`.
    New,
    /// `--session-id` with this exact UUID.
    NewAs(&'a str),
    /// `--resume` this exact UUID.
    Resume(&'a str),
}

/// The recipe's argv (packet §4) in the default mode, without
/// `--restricted` (owner, 2026-10-05), with `--strict-mcp-config` while MCP
/// servers are requested off (Claude's default requests them on).
fn argv(launch: Launch<'_>, mcp_off: bool) -> Value {
    let mut argv = vec![
        json!("-p"),
        json!("--input-format"),
        json!("stream-json"),
        json!("--output-format"),
        json!("stream-json"),
        json!("--verbose"),
        json!("--model"),
        json!("haiku"),
    ];
    argv.extend(match launch {
        Launch::New => [json!("--session-id"), json!({"capture": "sid"})],
        Launch::NewAs(id) => [json!("--session-id"), json!(id)],
        Launch::Resume(id) => [json!("--resume"), json!(id)],
    });
    if mcp_off {
        argv.push(json!("--strict-mcp-config"));
    }
    for arg in [
        "--permission-mode",
        "dontAsk",
        "--permission-prompts",
        "none",
        "--tools",
        TOOLS,
        "--allowedTools",
        TOOLS,
    ] {
        argv.push(json!(arg));
    }
    json!(argv)
}

/// `argv` with `--restricted` after the session, as `harnesses.claude.restricted`
/// launches.
fn restricted(recipe: &Value) -> Value {
    let mut flags = recipe.as_array().cloned().unwrap_or_default();
    flags.insert(10, json!("--restricted"));
    Value::Array(flags)
}

/// A lifetime whose argv passes `--restricted`.
fn restricted_launch(mut lifetime: Value) -> Value {
    lifetime["argv"] = restricted(&lifetime["argv"]);
    lifetime
}

/// The session ID a launch's lines carry: the capture for a new one.
fn sid(launch: Launch<'_>) -> &str {
    match launch {
        Launch::New => "${sid}",
        Launch::NewAs(id) | Launch::Resume(id) => id,
    }
}

/// One lifetime: the fake pauses 100 ms after the prompt, as recorded.
fn lifetime(argv: &Value, steps: Vec<Value>) -> Value {
    let mut all = vec![steps[0].clone(), json!({"delay": {"ms": 100}})];
    all.extend(steps.into_iter().skip(1));
    json!({"argv": argv, "version": format!("{TESTED} (Claude Code)"),
           "deadline_ms": 20000, "steps": all})
}

fn emit(line: &Value) -> Value {
    json!({"emit": {"line": line.to_string()}})
}

/// The one user line a launch must read.
fn prompt(text: &str) -> Value {
    json!({"expect": {"line": {"type": "user", "message": {"role": "user",
        "content": [{"type": "text", "text": text}]}}}})
}

/// An init for `sid` reporting `version` and `capabilities`.
fn init_as(sid: &str, version: &str, capabilities: &[&str]) -> Value {
    emit(
        &json!({"type": "system", "subtype": "init", "cwd": "/work/project",
        "session_id": sid, "tools": ["Bash", "Edit", "Glob", "Grep", "Read", "Write"],
        "mcp_servers": [], "model": "claude-haiku-4-5-20251001", "permissionMode": "dontAsk",
        "apiKeySource": "none", "claude_code_version": version,
        "uuid": "00000000-0000-4000-8000-000000000001", "capabilities": capabilities}),
    )
}

fn init(sid: &str) -> Value {
    init_as(
        sid,
        TESTED,
        &[
            "interrupt_receipt_v1",
            "interrupt_cancel_queued_v1",
            "msg_lifecycle_v1",
        ],
    )
}

/// An assistant text block: the turn's acceptance.
fn reply(sid: &str, text: &str) -> Value {
    emit(
        &json!({"type": "assistant", "message": {"model": "claude-haiku-4-5-20251001",
        "id": "msg_01SYNTH000001", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": 7, "output_tokens": 3}},
        "parent_tool_use_id": null, "session_id": sid}),
    )
}

fn result(sid: &str, text: &str, cost: f64) -> Value {
    emit(
        &json!({"type": "result", "subtype": "success", "is_error": false,
        "session_id": sid, "stop_reason": "end_turn", "terminal_reason": "completed",
        "num_turns": 1, "total_cost_usd": cost,
        "usage": {"input_tokens": 10, "cache_creation_input_tokens": 100,
            "cache_read_input_tokens": 1000, "output_tokens": 20},
        "permission_denials": [], "result": text}),
    )
}

fn await_eof() -> Value {
    json!({"await_eof": {}})
}

/// A gate the test releases with `SIGUSR1`.
fn gate() -> Value {
    json!({"await_signal": {"signal": "SIGUSR1"}})
}

/// The end of a vendor Host stops: its group's `SIGTERM`, then exit 143.
fn stopped_by_host() -> [Value; 2] {
    [
        json!({"await_signal": {"signal": "SIGTERM"}}),
        json!({"exit": {"code": 143, "stderr": ""}}),
    ]
}

/// A launch that answers `text` and completes.
fn completing(launch: Launch<'_>, mcp_off: bool, text: &str, cost: f64) -> Value {
    let id = sid(launch);
    lifetime(
        &argv(launch, mcp_off),
        vec![
            prompt(&format!("Reply {text}.")),
            init(id),
            reply(id, text),
            result(id, text, cost),
            await_eof(),
        ],
    )
}

/// The prompt [`completing`] expects for `text`.
fn ask(text: &str) -> String {
    format!("Reply {text}.")
}

// ---------------------------------------------------------- F10 identity

/// F10 (packet §9 `claude_identity_resume`), the Core half through the
/// daemon: a session's first launch confirms its UUID (one
/// `session.opened`, `vendor_identity_verified:true`); while the second
/// launch, a new process resuming that UUID, waits after its prompt and
/// before its init, `status` keeps the historical ID with
/// `vendor_identity_verified:false`; its own init commits exactly one
/// `session.reopened` before its acceptance (`turn.started`), and the
/// flag returns to true; the third launch reopens once more. A fourth
/// launch whose init names another session fails `resume_mismatch`: no
/// `session.reopened`, the historical ID unchanged and not verified.
#[test]
fn claude_identity_resume_through_daemon() -> TestResult {
    scenario("claude_identity_resume_through_daemon", |d, evidence| {
        let mut lives = vec![completing(Launch::New, false, "ONE", 0.001)];
        d.replay(&lives)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        let first = d.wait(evidence, &format!("{session}/1"))?;
        let uuid = first["vendor_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        check(completed(&first) && !uuid.is_empty(), || {
            format!("turn 1: {first}")
        })?;
        let opened = d.status(evidence, "status-1", &session)?;
        check(
            opened["vendor_session_id"] == uuid.as_str()
                && opened["vendor_identity_verified"] == true
                && d.count(&session, "session.opened")? == 1,
            || format!("after the first launch: {opened}"),
        )?;
        let resumed = Launch::Resume(&uuid);
        lives.push(lifetime(
            &argv(resumed, false),
            vec![
                prompt(&ask("TWO")),
                gate(),
                init(&uuid),
                reply(&uuid, "TWO"),
                result(&uuid, "TWO", 0.002),
                await_eof(),
            ],
        ));
        lives.push(completing(resumed, false, "THREE", 0.003));
        d.replay(&lives)?;
        d.resume(evidence, "resume-2", &session, &ask("TWO"))?;
        d.await_progress("at 3 launch 2")?;
        let reopening = d.status(evidence, "status-2-reopening", &session)?;
        check(
            reopening["vendor_session_id"] == uuid.as_str()
                && reopening["vendor_identity_verified"] == false
                && d.count(&session, "session.reopened")? == 0,
            || format!("while reopening: {reopening}"),
        )?;
        d.release(2)?;
        for (n, text) in [(2, "TWO"), (3, "THREE")] {
            if n == 3 {
                d.resume(evidence, "resume-3", &session, &ask(text))?;
            }
            let envelope = d.wait(evidence, &format!("{session}/{n}"))?;
            check(
                completed(&envelope) && envelope["vendor_session_id"] == uuid.as_str(),
                || format!("turn {n}: {envelope}"),
            )?;
        }
        reopened_once_per_launch(d, &session, &[2, 3])?;
        let verified = d.status(evidence, "status-3", &session)?;
        check(verified["vendor_identity_verified"] == true, || {
            format!("after the third launch: {verified}")
        })?;
        mismatch_never_reopens(d, evidence, (&session, &uuid), &mut lives)
    })
}

/// One `session.opened` and one `session.reopened` per later launch in
/// `turns`, each committed before that turn's acceptance (`turn.started`).
fn reopened_once_per_launch(
    d: &Deployment,
    session: &str,
    turns: &[u32],
) -> Result<(), ScenarioError> {
    let events = d.events(session)?;
    let seq = |kind: &str, turn: u32| {
        events
            .iter()
            .find(|event| event["type"] == kind && event["turn"] == turn)
            .and_then(|event| event["seq"].as_u64())
    };
    let reopened: Vec<u64> = events
        .iter()
        .filter(|event| event["type"] == "session.reopened")
        .filter_map(|event| event["seq"].as_u64())
        .collect();
    check(
        d.count(session, "session.opened")? == 1 && reopened.len() == turns.len(),
        || format!("opened/reopened events: {events:?}"),
    )?;
    for (at, turn) in reopened.iter().zip(turns) {
        let started = seq("turn.started", *turn);
        let submitted = seq("turn.submitted", *turn);
        let between = submitted
            .zip(started)
            .is_some_and(|(submitted, started)| submitted < *at && *at < started);
        check(between, || {
            format!(
                "turn {turn}'s session.reopened is not between its submission and acceptance: {events:?}"
            )
        })?;
    }
    Ok(())
}

/// The fourth launch's init names another session: `resume_mismatch`, no
/// `session.reopened`, and the historical ID kept, unverified.
fn mismatch_never_reopens(
    d: &Deployment,
    evidence: &Evidence,
    (session, uuid): (&str, &str),
    lives: &mut Vec<Value>,
) -> Result<(), ScenarioError> {
    let other = "99999999-9999-4999-8999-999999999999";
    let mut steps = vec![prompt(&ask("FOUR")), init(other)];
    steps.push(json!({"expect": {"line": {"type": "control_request",
        "request": {"subtype": "interrupt"}}, "capture": {"rid": "/request_id"}}}));
    steps.push(json!({"emit": {"line": "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":${rid},\"response\":{\"still_queued\":[]}}}"}}));
    steps.push(emit(
        &json!({"type": "result", "subtype": "error_during_execution",
        "is_error": true, "session_id": other, "stop_reason": "end_turn",
        "terminal_reason": "aborted_tools"}),
    ));
    steps.push(await_eof());
    lives.push(lifetime(&argv(Launch::Resume(uuid), false), steps));
    d.replay(lives)?;
    let reopened = d.count(session, "session.reopened")?;
    d.resume(evidence, "resume-4", session, &ask("FOUR"))?;
    let envelope = d.wait(evidence, &format!("{session}/4"))?;
    check(
        envelope["state"] == "failed"
            && envelope["failure"]["class"] == "resume_mismatch"
            && replay_ran(&envelope, 0),
        || format!("turn 4: {envelope}"),
    )?;
    let status = d.status(evidence, "status-4", session)?;
    check(
        status["vendor_session_id"] == uuid
            && status["vendor_identity_verified"] == false
            && d.count(session, "session.reopened")? == reopened,
        || format!("after the mismatch: {status}"),
    )
}

/// F10 through the daemon: a missing vendor session (`--resume` answered
/// by a pre-init rejection that echoes the UUID) fails the turn
/// `submit_failed` and keeps the VIA session; it commits no
/// `session.reopened` and never creates a fresh vendor session: the next
/// launch resumes the same UUID and its own init reopens it.
#[test]
fn claude_identity_missing_session_through_daemon() -> TestResult {
    scenario("claude_identity_missing_session", |d, evidence| {
        let mut lives = vec![completing(Launch::New, false, "ONE", 0.001)];
        d.replay(&lives)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        let first = d.wait(evidence, &format!("{session}/1"))?;
        let uuid = first["vendor_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        check(completed(&first) && !uuid.is_empty(), || {
            format!("turn 1: {first}")
        })?;
        let gone = emit(
            &json!({"type": "result", "subtype": "error_during_execution",
            "is_error": true, "num_turns": 0, "stop_reason": null, "session_id": uuid,
            "total_cost_usd": 0, "errors": [format!("No conversation found with session ID: {uuid}")]}),
        );
        lives.push(lifetime(
            &argv(Launch::Resume(&uuid), false),
            vec![
                prompt(&ask("TWO")),
                gone,
                await_eof(),
                json!({"exit": {"code": 1, "stderr": "No conversation found\n"}}),
            ],
        ));
        lives.push(completing(Launch::Resume(&uuid), false, "THREE", 0.002));
        d.replay(&lives)?;
        d.resume(evidence, "resume-2", &session, &ask("TWO"))?;
        let second = d.wait(evidence, &format!("{session}/2"))?;
        check(
            second["state"] == "failed"
                && second["failure"]["class"] == "submit_failed"
                && replay_ran(&second, 1)
                && d.count(&session, "session.reopened")? == 0,
            || format!("turn 2: {second}"),
        )?;
        let status = d.status(evidence, "status-2", &session)?;
        check(
            status["vendor_session_id"] == uuid.as_str()
                && status["vendor_identity_verified"] == false,
            || format!("after the missing session: {status}"),
        )?;
        d.resume(evidence, "resume-3", &session, &ask("THREE"))?;
        let third = d.wait(evidence, &format!("{session}/3"))?;
        check(
            completed(&third) && third["vendor_session_id"] == uuid.as_str(),
            || format!("turn 3: {third}"),
        )?;
        reopened_once_per_launch(d, &session, &[3])
    })
}

/// F10 through the daemon: a lost submission (the process exits after
/// reading the prompt, before any output) is never resent. The next
/// launch reads exactly its own prompt (the replay's strict input fails a
/// second line) under the same expected UUID (`--session-id` again: no
/// identity was ever confirmed), whose init then opens the session once.
#[test]
fn claude_identity_lost_input_through_daemon() -> TestResult {
    scenario("claude_identity_lost_input", |d, evidence| {
        let mut lives = vec![lifetime(
            &argv(Launch::New, false),
            vec![
                prompt(&ask("ONE")),
                gate(),
                json!({"exit": {"code": 1, "stderr": ""}}),
            ],
        )];
        d.replay(&lives)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        d.await_progress("at 3 launch 1")?;
        let expected = d.launch_session(1)?;
        d.release(1)?;
        let first = d.wait(evidence, &format!("{session}/1"))?;
        check(
            first["timestamps"]["accepted_at"].is_null()
                && first["state"] != "completed"
                && replay_ran(&first, 1),
            || format!("turn 1: {first}"),
        )?;
        lives.push(completing(Launch::NewAs(&expected), false, "TWO", 0.001));
        d.replay(&lives)?;
        d.resume(evidence, "resume-2", &session, &ask("TWO"))?;
        let second = d.wait(evidence, &format!("{session}/2"))?;
        check(
            completed(&second) && second["vendor_session_id"] == expected.as_str(),
            || format!("turn 2: {second}"),
        )?;
        let reads = |n| {
            d.progress_of(n).map(|lines| {
                lines
                    .iter()
                    .filter(|line| line.starts_with("read "))
                    .count()
            })
        };
        check(reads(1)? == 1 && reads(2)? == 1, || {
            format!("input lines read: {:?}", d.progress_of(2))
        })?;
        check(
            d.count(&session, "session.opened")? == 1
                && d.count(&session, "session.reopened")? == 0,
            || "the lost launch opened the session".to_owned(),
        )
    })
}

// ------------------------------------------------- Q5 generation barrier

/// The late message launch 1 emits after its stdin EOF.
const LATE: &str = "LATE-ONE";

/// Launch 1 of a barrier scenario: it answers, reads its stdin EOF, then
/// holds at a gate (step 7) before it emits a late assistant message and
/// exits 0: the old generation still owes an observation.
fn held_past_eof() -> Value {
    let id = "${sid}";
    lifetime(
        &argv(Launch::New, false),
        vec![
            prompt(&ask("ONE")),
            init(id),
            reply(id, "ONE"),
            result(id, "ONE", 0.001),
            await_eof(),
            gate(),
            emit(
                &json!({"type": "assistant", "message": {"model": "claude-haiku-4-5-20251001",
                "id": "msg_01SYNTH000009", "type": "message", "role": "assistant",
                "content": [{"type": "text", "text": LATE}],
                "usage": {"input_tokens": 1, "output_tokens": 1}},
                "parent_tool_use_id": null, "session_id": id}),
            ),
        ],
    )
}

/// Spawns turn 1 on [`held_past_eof`], waits until its process holds past
/// EOF, then queues turn 2 behind it: the barrier is contended. Launch 2
/// (if any) resumes turn 1's UUID, read from launch 1's argv, and waits at
/// a gate (step 3) before its init. Returns the session and that UUID.
fn contend(d: &Deployment, evidence: &Evidence) -> Result<(String, String), ScenarioError> {
    d.replay(&[held_past_eof()])?;
    let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
    d.await_progress("at 7 launch 1")?;
    let uuid = d.launch_session(1)?;
    d.replay(&[
        held_past_eof(),
        lifetime(
            &argv(Launch::Resume(&uuid), false),
            vec![
                prompt(&ask("TWO")),
                gate(),
                init(&uuid),
                reply(&uuid, "TWO"),
                result(&uuid, "TWO", 0.002),
                await_eof(),
            ],
        ),
    ])?;
    d.resume(evidence, "resume-2", &session, &ask("TWO"))?;
    let status = d.status(evidence, "status-contended", &session)?;
    let (turn_1, _) = d.turn(&session, 1)?;
    check(
        d.launches()?.len() == 1
            && turn_1 != "completed"
            && status["queue"]
                .as_array()
                .is_some_and(|queue| !queue.is_empty()),
        || format!("turn 2 is not waiting behind launch 1: turn 1 {turn_1}, {status}"),
    )?;
    Ok((session, uuid))
}

/// Wire's message dequeue (one hit per stdout message and one for the end
/// of stdout), Route's observed exit (after its read to EOF), and Host's
/// committed ARM intent, the last step before a vendor spawns.
#[cfg(feature = "test-failpoints")]
const RECEIVED: &str = "wire.messages.received";
#[cfg(feature = "test-failpoints")]
const EXITED: &str = "wire.exit.observed";
#[cfg(feature = "test-failpoints")]
const ARMING: &str = "host.anchor.after_arm_intent_commit";

/// Ruling Q5, the generation barrier under contention: launch 1 has read
/// its stdin EOF but still owes a late message (it holds at a gate) while
/// turn 2 waits behind it. Turn 2 launches only after launch 1's reader
/// finished: at launch 2's ARM, held before its vendor spawns, Wire has
/// handed Route all five of launch 1's stdout reads (init, reply, result,
/// the late message, end of stdout) and Route has observed launch 1's
/// exit, which it waits for only after reading stdout to EOF; no sixth
/// read exists yet. A post-result assistant message is no observation by
/// contract, so its delivery is read from Wire's dequeue, not from an
/// event. It verifies nothing: while launch 2 waits before its own init,
/// `status` keeps the historical ID unverified and no `session.reopened`
/// exists. Launch 2's own init reopens the session once. Both launches ran
/// their replays through (exit 0).
#[cfg(feature = "test-failpoints")]
#[test]
fn claude_generation_barrier_holds_late_messages() -> TestResult {
    scenario("claude_generation_barrier_late", |d, evidence| {
        for (point, action) in [(RECEIVED, "value_persist:1"), (EXITED, "value_persist:1")] {
            d.failpoints.arm(point, 1, action).map_err(infra)?;
        }
        d.failpoints.arm(ARMING, 2, "pause").map_err(infra)?;
        let daemon = Daemon::start(d, evidence, "final")?;
        let (session, uuid) = contend(d, evidence)?;
        d.release(1)?;
        d.failpoints
            .wait_ack(ARMING, 2, "pause", daemon.pid(), WAIT)
            .map_err(infra)?;
        let acked = |point: &str, n: u64| {
            d.root
                .path()
                .join("failpoints")
                .join(format!("{point}.{n}.ack"))
                .exists()
        };
        let reads = (1..=6).filter(|n| acked(RECEIVED, *n)).count();
        let exited = acked(EXITED, 1);
        let spawned = d.launches()?.len();
        d.failpoints.release(ARMING, 2).map_err(infra)?;
        check(reads == 5 && exited && spawned == 1, || {
            format!(
                "at launch 2's ARM: {reads} stdout reads of launch 1, its exit observed \
                 {exited}, {spawned} launches"
            )
        })?;
        d.await_progress("at 3 launch 2")?;
        let reopening = d.status(evidence, "status-reopening", &session)?;
        check(
            reopening["vendor_session_id"] == uuid.as_str()
                && reopening["vendor_identity_verified"] == false
                && d.count(&session, "session.reopened")? == 0,
            || format!("launch 1's late message verified the reopening: {reopening}"),
        )?;
        late_before_next(d, &session)?;
        d.release(2)?;
        let first = d.wait(evidence, &format!("{session}/1"))?;
        let second = d.wait(evidence, &format!("{session}/2"))?;
        check(completed(&first) && completed(&second), || {
            format!("turn 1 {first}; turn 2 {second}")
        })?;
        reopened_once_per_launch(d, &session, &[2])
    })
}

/// The durable side of the barrier's order: turn 1's `turn.ended`
/// precedes turn 2's `turn.submitted`, and in the fake's log launch 1 was
/// released before launch 2 read its prompt. (The late message's delivery
/// itself is read at launch 2's ARM.)
#[cfg(feature = "test-failpoints")]
fn late_before_next(d: &Deployment, session: &str) -> Result<(), ScenarioError> {
    let events = d.events(session)?;
    let seq = |kind: &str, turn: u32| {
        events
            .iter()
            .find(|event| event["type"] == kind && event["turn"] == turn)
            .and_then(|event| event["seq"].as_u64())
    };
    let (ended, submitted) = (seq("turn.ended", 1), seq("turn.submitted", 2));
    let progress = Deployment::log_of(&d.replayed.borrow(), "progress")?;
    let at = |line: &str| progress.lines().position(|seen| seen == line);
    let released = at("signalled 7 launch 1")
        .zip(at("read 1 launch 2"))
        .is_some_and(|(released, read)| released < read);
    check(
        ended
            .zip(submitted)
            .is_some_and(|(ended, submitted)| ended < submitted)
            && released,
        || {
            format!(
                "turn 1 ended at {ended:?}, turn 2 submitted at {submitted:?}; progress:\n{progress}"
            )
        },
    )
}

/// Ruling Q5's stop check under contention: turn 2, queued behind launch 1
/// that holds past its EOF, is cancelled before the barrier opens. It
/// never launches (one launch in all, no `session.reopened`) and ends
/// `cancelled` with nothing accepted; launch 1, released, still emits its
/// late message and completes turn 1 (exit 0).
#[test]
fn claude_generation_barrier_stop_while_contended() -> TestResult {
    scenario("claude_generation_barrier_stop", |d, evidence| {
        let _daemon = Daemon::start(d, evidence, "final")?;
        let (session, _uuid) = contend(d, evidence)?;
        d.ok(
            evidence,
            "cancel-2",
            &[
                "cancel", &session, "--turn", "2", "--handle", HANDLE, "--json",
            ],
        )?;
        let second = d.wait(evidence, &format!("{session}/2"))?;
        d.release(1)?;
        let first = d.wait(evidence, &format!("{session}/1"))?;
        check(
            completed(&first)
                && second["state"] == "cancelled"
                && second["timestamps"]["accepted_at"].is_null()
                && d.launches()?.len() == 1
                && d.count(&session, "session.reopened")? == 0,
            || format!("turn 1 {first}; turn 2 {second}"),
        )
    })
}

// ------------------------------------------------------------- recovery

/// Waits until launch `n`'s process and its group are gone.
fn await_gone(d: &Deployment, n: usize, pgid: u32) -> Result<(), ScenarioError> {
    let vendor = *d
        .launches()?
        .get(n - 1)
        .ok_or_else(|| fail(&format!("no launch {n}")))?;
    let deadline = Instant::now() + WAIT;
    loop {
        let absent = i32::try_from(pgid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .is_some_and(|group| {
                rustix::process::test_kill_process_group(group) == Err(rustix::io::Errno::SRCH)
            });
        if process::exited(vendor).map_err(infra)? && absent {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(fail(&format!("launch {n} or group {pgid} survived")));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// C1 §7.5 for Claude: after the restart the crashed turn 1 is `unknown`
/// (`daemon_restart`), its cleanup `quiescent` on Host's group-absence
/// proof, with `outcome` (`requested` or `forced`); exactly one launch,
/// which read exactly one input line: nothing was resent.
fn recovered_unknown(d: &Deployment, session: &str, outcome: &str) -> Result<(), ScenarioError> {
    let (state, envelope) = d.turn(session, 1)?;
    check(
        state == "unknown"
            && envelope["failure"]["class"] == "daemon_restart"
            && envelope["cancel"]["outcome"] == outcome
            && envelope["cancel"]["cleanup"] == "quiescent",
        || format!("turn 1 after restart: {state} {envelope}"),
    )?;
    let anchors = d.anchors(session)?;
    let reads: Vec<String> = d
        .progress_of(1)?
        .into_iter()
        .filter(|line| line.starts_with("read "))
        .collect();
    check(
        d.launches()?.len() == 1 && anchors.len() == 1 && anchors[0].2 && reads.len() == 1,
        || format!("after restart: anchors {anchors:?}, launch 1 reads {reads:?}"),
    )
}

/// Packet §9 `claude_recovery_no_submit`, a crash after the submission
/// intent and the prompt, before any acceptance: the daemon is killed
/// while the vendor holds after reading the prompt. The anchor's own
/// cleanup stops the group (the vendor exits at its `SIGTERM`); restart
/// proves absence and the turn is `unknown`, never resent (no
/// `session.opened`: nothing confirmed). C1 §7.3 cancels any turn queued
/// behind an `unknown` one, so no later launch follows.
#[test]
fn claude_recovery_no_submit_before_acceptance() -> TestResult {
    scenario("claude_recovery_before_acceptance", |d, evidence| {
        let [term, exit] = stopped_by_host();
        let lives = vec![lifetime(
            &argv(Launch::New, false),
            vec![prompt(&ask("ONE")), term, exit],
        )];
        d.replay(&lives)?;
        let mut crashed = Daemon::start(d, evidence, "crashed")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        d.await_progress("at 3 launch 1")?;
        d.await_event(&session, 1, "turn.submitted")?;
        let (_, pgid, _) = d.anchors(&session)?[0];
        crashed.kill()?;
        await_gone(d, 1, pgid)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        recovered_unknown(d, &session, "requested")?;
        check(
            d.count(&session, "session.opened")? == 0 && d.count(&session, "turn.started")? == 0,
            || "an unconfirmed launch opened or accepted".to_owned(),
        )
    })
}

/// Packet §9 `claude_recovery_no_submit`, a crash after acceptance: the
/// vendor confirmed the session and accepted the turn, then holds. After
/// the restart the turn is `unknown` with cleanup proved and nothing was
/// resent; the confirmation committed before the crash stands.
#[test]
fn claude_recovery_no_submit_after_acceptance() -> TestResult {
    scenario("claude_recovery_after_acceptance", |d, evidence| {
        let [term, exit] = stopped_by_host();
        let lives = vec![lifetime(
            &argv(Launch::New, false),
            vec![
                prompt(&ask("ONE")),
                init("${sid}"),
                reply("${sid}", "partial"),
                term,
                exit,
            ],
        )];
        d.replay(&lives)?;
        let mut crashed = Daemon::start(d, evidence, "crashed")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        d.await_event(&session, 1, "turn.started")?;
        d.await_progress("at 5 launch 1")?;
        let (_, pgid, _) = d.anchors(&session)?[0];
        crashed.kill()?;
        await_gone(d, 1, pgid)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        recovered_unknown(d, &session, "requested")?;
        check(d.count(&session, "session.opened")? == 1, || {
            "the crashed launch's confirmation was lost".to_owned()
        })
    })
}

/// Packet §9 `claude_recovery_no_submit`, a surviving vendor: the
/// anchor's cleanup is held past the crash (`host.anchor.defer_cleanup`),
/// so the vendor and a child it left in its group live on. The restarted
/// daemon verifies the persisted anchor and stops the group through it
/// (`forced`), then proves absence (`quiescent`); the turn is `unknown`
/// and nothing is resent. (An unverified anchor is never signalled:
/// S1's recovery suite, Host's own.)
#[cfg(feature = "test-failpoints")]
#[test]
fn claude_recovery_no_submit_survivor() -> TestResult {
    scenario("claude_recovery_survivor", |d, evidence| {
        let point = "host.anchor.defer_cleanup";
        d.failpoints.arm(point, 1, "fail_io").map_err(infra)?;
        let [term, exit] = stopped_by_host();
        let lives = vec![lifetime(
            &argv(Launch::New, false),
            vec![
                prompt(&ask("ONE")),
                init("${sid}"),
                reply("${sid}", "partial"),
                json!({"spawn_survivor": {"ms": 15000}}),
                term,
                exit,
            ],
        )];
        d.replay(&lives)?;
        let mut crashed = Daemon::start(d, evidence, "crashed")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        d.await_event(&session, 1, "turn.started")?;
        d.await_progress("at 6 launch 1")?;
        let (anchor, pgid, _) = d.anchors(&session)?[0];
        crashed.kill()?;
        d.failpoints
            .wait_ack(point, 1, "fail_io", anchor, Duration::from_secs(10))
            .map_err(|error| fail(&format!("failpoint {point}: {error}")))?;
        let vendor = d.launches()?[0];
        check(
            !process::exited(vendor).map_err(infra)? && !process::exited(anchor).map_err(infra)?,
            || "the held vendor did not survive the crash".to_owned(),
        )?;
        d.failpoints.disarm(point).map_err(infra)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        recovered_unknown(d, &session, "forced")?;
        await_gone(d, 1, pgid)
    })
}

// -------------------------------------------------------------- S-LAUNCH

/// S-LAUNCH end to end (adapter design §5.4, runtime §8): `daemon.json`'s
/// `harnesses.claude` is read once, at daemon start. A change written
/// while the daemon runs (another binary, MCP servers off, the restricted
/// mode) does not reach a session spawned on that daemon: it still runs
/// the old binary without `--strict-mcp-config` or `--restricted`. After
/// the restart a new session runs the new binary with both; a session
/// spawned before it runs the new binary too (the binary is the daemon's,
/// not frozen) but keeps its frozen inheritance and mode, so neither flag
/// is passed (via-umz). Each replay pins its argv, so the wrong binary or
/// recipe fails its launch. Status reports each session's frozen
/// effective states.
#[test]
fn claude_s_launch_config_applies_after_restart() -> TestResult {
    scenario("claude_s_launch_config_after_restart", |d, evidence| {
        let other = d.vendor.join("other");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&other)
            .map_err(infra)?;
        let next = other.join("claude");
        std::os::unix::fs::symlink(&d.fake, &next).map_err(infra)?;
        d.replay(&[
            completing(Launch::New, false, "ONE", 0.001),
            completing(Launch::New, false, "TWO", 0.001),
        ])?;
        let daemon = Daemon::start(d, evidence, "before")?;
        let old = session_of(&d.spawn(evidence, "spawn-old", &ask("ONE"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{old}/1"))?;
        let uuid = envelope["vendor_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        check(completed(&envelope) && !uuid.is_empty(), || {
            format!("the old session's first turn: {envelope}")
        })?;
        d.config(&json!({"harnesses":{"claude":{"binary":next,
            "inherit":{"mcp_servers":false},"restricted":true}}}))
            .map_err(infra)?;
        let unchanged = session_of(&d.spawn(evidence, "spawn-unchanged", &ask("TWO"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{unchanged}/1"))?;
        check(completed(&envelope), || {
            format!("a session spawned before the restart: {envelope}")
        })?;
        daemon.shutdown()?;
        d.replay_for(
            &next,
            &[
                restricted_launch(completing(Launch::New, true, "THREE", 0.001)),
                completing(Launch::Resume(&uuid), false, "FOUR", 0.002),
            ],
        )?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let new = session_of(&d.spawn(evidence, "spawn-new", &ask("THREE"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{new}/1"))?;
        check(completed(&envelope), || {
            format!("a session spawned after the restart: {envelope}")
        })?;
        d.resume(evidence, "resume-old", &old, &ask("FOUR"))?;
        let envelope = d.wait(evidence, &format!("{old}/2"))?;
        check(completed(&envelope), || {
            format!("the old session after the restart: {envelope}")
        })?;
        let counts = (
            d.launches()?.len(),
            Deployment::log_of(&next, "launches")?.lines().count(),
        );
        let inherit = |session: &str| -> Result<Value, ScenarioError> {
            Ok(d.status(evidence, &format!("status-{session}"), session)?["inherit"].clone())
        };
        // Status reports the effective settings: the default mode loads
        // the user's configuration, MCP servers included; the restricted
        // mode none of it, and MCP `off` is verified by
        // `--strict-mcp-config`.
        let (old, new) = (inherit(&old)?, inherit(&new)?);
        check(
            counts == (2, 2)
                && old
                    == json!({"hooks":"on","mcp_servers":"on","plugins":"on","skills":"on",
                        "agents":"on","instruction_files":"on"})
                && new
                    == json!({"hooks":"off","mcp_servers":"off","plugins":"off",
                        "skills":"off","agents":"off","instruction_files":"off"}),
            || format!("launches per binary {counts:?}; inherit {old} and {new}"),
        )
    })
}

/// Raw vendor arguments a passthrough session passes (owner, 2026-10-06):
/// an option with a separate value, a short option with an attached
/// value and a long one with `=value` and a space, none reserved.
const PASSED: [&str; 4] = ["--max-budget-usd", "5", "-dapi", "--name=via passthrough"];

/// A lifetime whose argv ends with [`PASSED`], as a passthrough session
/// launches (C2 §6.3: appended after VIA's own arguments).
fn passing(mut lifetime: Value) -> Value {
    if let Some(argv) = lifetime["argv"].as_array_mut() {
        argv.extend(PASSED.iter().map(|arg| json!(arg)));
    }
    lifetime
}

/// The number of `vendor_passthrough` warnings a receipt, envelope or
/// status carries.
fn passthrough_warnings(value: &Value) -> usize {
    value["warnings"].as_array().map_or(0, |warnings| {
        warnings
            .iter()
            .filter(|warning| warning["code"] == "vendor_passthrough")
            .count()
    })
}

/// Owner, 2026-10-06 (C1 §3.2, §4 `vendor_args`; C2 §6.3): `via spawn …
/// -- ARGS` appends ARGS unchanged to every launch of the session (the
/// fake checks each argv exactly), frozen at spawn and stored with the
/// session: a resume reuses them, and so does a resume after a daemon
/// restart. Each receipt, envelope and status of the session carries one
/// `vendor_passthrough` warning; a plain session's carry none. `--` on
/// `via resume` is `invalid_params` kind2 `session_scope_on_resume`
/// naming `vendor_args`, with no launch; a bare `--` sends nothing and the
/// resume proceeds.
#[test]
fn claude_passthrough_args_frozen_across_resume_and_restart() -> TestResult {
    scenario("claude_passthrough_frozen", |d, evidence| {
        let mut lives = vec![
            passing(completing(Launch::New, false, "ONE", 0.001)),
            completing(Launch::New, false, "PLAIN", 0.001),
        ];
        d.replay(&lives)?;
        let daemon = Daemon::start(d, evidence, "before")?;
        let mut extra = vec!["--"];
        extra.extend(PASSED);
        let receipt = d.spawn(evidence, "spawn-passing", &ask("ONE"), &extra)?;
        let session = session_of(&receipt)?;
        let first = d.wait(evidence, &format!("{session}/1"))?;
        let status = d.status(evidence, "status-passing", &session)?;
        let uuid = first["vendor_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        check(
            completed(&first)
                && !uuid.is_empty()
                && [&receipt, &first, &status]
                    .iter()
                    .all(|value| passthrough_warnings(value) == 1),
            || format!("the passthrough session: {receipt} {first} {status}"),
        )?;
        let plain_receipt = d.spawn(evidence, "spawn-plain", &ask("PLAIN"), &[])?;
        let plain = session_of(&plain_receipt)?;
        let plain_first = d.wait(evidence, &format!("{plain}/1"))?;
        let plain_status = d.status(evidence, "status-plain", &plain)?;
        check(
            completed(&plain_first)
                && [&plain_receipt, &plain_first, &plain_status]
                    .iter()
                    .all(|value| passthrough_warnings(value) == 0),
            || format!("the plain session: {plain_receipt} {plain_first} {plain_status}"),
        )?;
        let error = d.refused(
            evidence,
            "resume-with-args",
            &[
                "resume", &session, "--prompt", "p", "--handle", HANDLE, "--json", "--", "--model",
                "opus",
            ],
            "invalid_params",
        )?;
        check(
            error["data"]["kind2"] == "session_scope_on_resume"
                && error["data"]["field"] == "vendor_args"
                && d.launches()?.len() == 2,
            || format!("resume with vendor_args: {error}"),
        )?;
        lives.push(passing(completing(
            Launch::Resume(&uuid),
            false,
            "TWO",
            0.002,
        )));
        d.replay(&lives)?;
        // C1 §1, §3.3: a bare `--` on resume sends nothing and is ignored.
        let resumed = d.ok(
            evidence,
            "resume-2",
            &[
                "resume",
                &session,
                "--prompt",
                &ask("TWO"),
                "--handle",
                HANDLE,
                "--json",
                "--",
            ],
        )?;
        let second = d.wait(evidence, &format!("{session}/2"))?;
        check(
            completed(&second)
                && passthrough_warnings(&resumed) == 1
                && passthrough_warnings(&second) == 1,
            || format!("the resumed turn: {resumed} {second}"),
        )?;
        daemon.shutdown()?;
        lives.push(passing(completing(
            Launch::Resume(&uuid),
            false,
            "THREE",
            0.003,
        )));
        d.replay(&lives)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let resumed = d.resume(evidence, "resume-3", &session, &ask("THREE"))?;
        let third = d.wait(evidence, &format!("{session}/3"))?;
        let status = d.status(evidence, "status-after-restart", &session)?;
        check(
            completed(&third)
                && d.launches()?.len() == 4
                && [&resumed, &third, &status]
                    .iter()
                    .all(|value| passthrough_warnings(value) == 1),
            || format!("after the restart: {resumed} {third} {status}"),
        )
    })
}

/// Owner, 2026-10-06 (C1 §4 `vendor_args`; C2 §6.3; packet §4): a
/// reserved flag in any long, `=value`, normalized or short form, a short
/// cluster holding a reserved letter, an operand and `--` itself are
/// `invalid_params` kind2 `vendor_option_conflict` naming `vendor_args`;
/// past 64 arguments or 16 KiB `invalid_params` naming `vendor_args`
/// (NUL cannot cross an argv: C1 protocol tests cover it). None commits a
/// session or launches; then unreserved arguments pass and launch.
#[test]
fn claude_passthrough_reserved_and_bounds_refused() -> TestResult {
    scenario("claude_passthrough_refused", |d, evidence| {
        d.replay(&[passing(completing(Launch::New, false, "ONE", 0.001))])?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let work = d.work.to_string_lossy().into_owned();
        let spawn = |name: &str, passed: &[&str]| {
            let mut args = vec![
                "spawn",
                "--harness",
                "claude",
                "--model",
                "haiku",
                "--prompt",
                "p",
                "--cwd",
                &work,
                "--handle",
                HANDLE,
                "--background",
                "--json",
                "--",
            ];
            args.extend_from_slice(passed);
            d.refused(evidence, name, &args, "invalid_params")
        };
        for (name, passed) in [
            ("long", &["--permission-mode", "bypassPermissions"][..]),
            ("attached", &["--permission-mode=bypassPermissions"]),
            ("normalized", &["--Permission_Mode=default"]),
            ("alias", &["--allowed-tools", "Bash"]),
            ("short", &["-p"]),
            ("cluster", &["-xp"]),
            ("prefix", &["--resume-session-at", "x"]),
            ("operand", &["hello"]),
            ("value-then-operand", &["--max-budget-usd", "5", "hello"]),
            ("double-dash", &["--", "--model"]),
        ] {
            let error = spawn(&format!("spawn-{name}"), passed)?;
            check(
                error["data"]["kind2"] == "vendor_option_conflict"
                    && error["data"]["field"] == "vendor_args",
                || format!("{name}: {error}"),
            )?;
        }
        let many: Vec<String> = (0..65).map(|n| format!("--x{n}")).collect();
        let long = format!("--name={}", "z".repeat(16 * 1024));
        for (name, passed) in [
            (
                "too-many",
                many.iter().map(String::as_str).collect::<Vec<_>>(),
            ),
            ("too-long", vec![long.as_str()]),
        ] {
            let error = spawn(&format!("spawn-{name}"), &passed)?;
            check(
                error["data"]["kind2"].is_null() && error["data"]["field"] == "vendor_args",
                || format!("{name}: {error}"),
            )?;
        }
        check(d.rows("sessions")? == 0 && d.launches()?.is_empty(), || {
            "a refused spawn committed or launched".to_owned()
        })?;
        let mut extra = vec!["--"];
        extra.extend(PASSED);
        let session = session_of(&d.spawn(evidence, "spawn-passing", &ask("ONE"), &extra)?)?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        check(completed(&envelope) && d.launches()?.len() == 1, || {
            format!("the unreserved arguments: {envelope}")
        })
    })
}

/// C2 §5 through the daemon: `describe` and receipts report the last
/// version an init reported for this harness and program path (before
/// any launch `null`/`untested`, after one its version, here untested and
/// warned). A handshake missing `interrupt_receipt_v1` fails its turn
/// `protocol` with no resend and caches a refusal: the next spawn of the
/// same recipe is refused `harness_unavailable` with
/// `data.reason:"handshake_refused"` before any receipt or launch, and
/// `describe` reports `refused`. A daemon restart clears the cache: the
/// next spawn launches and re-checks.
#[test]
fn claude_s_launch_last_version_and_cached_refusal() -> TestResult {
    scenario("claude_s_launch_version_refusal", |d, evidence| {
        let id = "${sid}";
        let untested = lifetime(
            &argv(Launch::New, false),
            vec![
                prompt(&ask("ONE")),
                init_as(id, "2.1.291", &["interrupt_receipt_v1"]),
                reply(id, "ONE"),
                result(id, "ONE", 0.001),
                await_eof(),
            ],
        );
        let no_receipt = refused_handshake("TWO");
        let after = completing(Launch::New, false, "THREE", 0.001);
        d.replay(&[untested, no_receipt, after])?;
        let describe = [
            "describe",
            "--harness",
            "claude",
            "--model",
            "haiku",
            "--json",
        ];
        let daemon = Daemon::start(d, evidence, "before")?;
        let plan = d.ok(evidence, "describe-0", &describe)?;
        check(
            plan["vendor_version"].is_null() && plan["version_status"] == "untested",
            || format!("describe before any launch: {plan}"),
        )?;
        let session = session_of(&d.spawn(evidence, "spawn-1", &ask("ONE"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        check(
            completed(&envelope)
                && envelope["vendor_version"] == "2.1.291"
                && envelope["version_status"] == "untested",
            || format!("turn on an untested version: {envelope}"),
        )?;
        let plan = d.ok(evidence, "describe-1", &describe)?;
        let receipt = d.spawn(evidence, "spawn-2", &ask("TWO"), &[])?;
        check(
            plan["vendor_version"] == "2.1.291"
                && plan["version_status"] == "untested"
                && receipt["vendor_version"] == "2.1.291",
            || format!("describe {plan}, receipt {receipt}"),
        )?;
        let refused = d.wait(evidence, &format!("{}/1", session_of(&receipt)?))?;
        check(
            refused["state"] == "failed"
                && refused["failure"]["class"] == "protocol"
                && replay_ran(&refused, 0),
            || format!("a refused handshake: {refused}"),
        )?;
        let error = d.refused(
            evidence,
            "spawn-3",
            &[
                "spawn",
                "--harness",
                "claude",
                "--model",
                "haiku",
                "--prompt",
                "p",
                "--handle",
                HANDLE,
                "--json",
            ],
            "harness_unavailable",
        )?;
        let plan = d.ok(evidence, "describe-2", &describe)?;
        check(
            error["data"]["reason"] == "handshake_refused"
                && plan["version_status"] == "refused"
                && plan["vendor_version"] == TESTED
                && d.launches()?.len() == 2,
            || format!("cached refusal: {error}, describe {plan}"),
        )?;
        daemon.shutdown()?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let plan = d.ok(evidence, "describe-3", &describe)?;
        let session = session_of(&d.spawn(evidence, "spawn-4", &ask("THREE"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        check(
            plan["version_status"] == "untested"
                && plan["vendor_version"].is_null()
                && completed(&envelope),
            || format!("after the restart: describe {plan}, turn {envelope}"),
        )
    })
}

/// A launch whose init lacks `interrupt_receipt_v1`: the adapter refuses
/// the handshake, interrupts, and the vendor's abort result ends it.
fn refused_handshake(text: &str) -> Value {
    let id = "${sid}";
    lifetime(
        &argv(Launch::New, false),
        vec![
            prompt(&ask(text)),
            init_as(id, TESTED, &["msg_lifecycle_v1"]),
            json!({"expect": {"line": {"type": "control_request",
                "request": {"subtype": "interrupt"}}, "capture": {"rid": "/request_id"}}}),
            emit(
                &json!({"type": "result", "subtype": "error_during_execution",
                "is_error": true, "session_id": id, "stop_reason": "tool_use",
                "terminal_reason": "aborted_tools"}),
            ),
            await_eof(),
        ],
    )
}

/// The refusal-cache clock seam (test builds): a frozen origin plus milliseconds.
#[cfg(feature = "test-failpoints")]
const PLAN_CLOCK: &str = "adapter.claude.plan_clock_ms";

/// C2 §5's refusal expiry under controlled time through the daemon. The
/// Claude adapter's refusal-cache clock (`adapter.claude.plan_clock_ms`)
/// is frozen at one origin before anything runs, so the refused
/// handshake's entry is written at the origin and real elapsed time never
/// moves it. 590 s and 599.999 s on, the entry still refuses `describe`
/// and a spawn of the same recipe (`harness_unavailable`,
/// `handshake_refused`, nothing launched); 600 s on it has expired: the
/// plan is admitted (`tested`, the version its init reported), the spawn
/// launches, and that launch's handshake succeeds through the fixture.
#[cfg(feature = "test-failpoints")]
#[test]
fn claude_refusal_expires_through_daemon() -> TestResult {
    scenario("claude_refusal_expires", |d, evidence| {
        d.replay(&[
            refused_handshake("ONE"),
            completing(Launch::New, false, "TWO", 0.001),
        ])?;
        let describe = [
            "describe",
            "--harness",
            "claude",
            "--model",
            "haiku",
            "--json",
        ];
        let at = |ms: u64| {
            d.failpoints
                .arm(PLAN_CLOCK, 1, &format!("value_persist:{ms}"))
                .map_err(infra)
        };
        at(0)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn-1", &ask("ONE"), &[])?)?;
        let refused = d.wait(evidence, &format!("{session}/1"))?;
        check(
            refused["failure"]["class"] == "protocol" && replay_ran(&refused, 0),
            || format!("the refused handshake: {refused}"),
        )?;
        for ms in [590_000, 599_999] {
            at(ms)?;
            let live = d.ok(evidence, &format!("describe-{ms}"), &describe)?;
            let error = d.refused(
                evidence,
                &format!("spawn-{ms}"),
                &[
                    "spawn",
                    "--harness",
                    "claude",
                    "--model",
                    "haiku",
                    "--prompt",
                    "p",
                    "--handle",
                    HANDLE,
                    "--json",
                ],
                "harness_unavailable",
            )?;
            check(
                live["version_status"] == "refused"
                    && error["data"]["reason"] == "handshake_refused"
                    && d.launches()?.len() == 1,
                || format!("{ms} ms on: describe {live}, spawn {error}"),
            )?;
        }
        at(600_000)?;
        let expired = d.ok(evidence, "describe-600000", &describe)?;
        let next = session_of(&d.spawn(evidence, "spawn-600000", &ask("TWO"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{next}/1"))?;
        check(
            expired["version_status"] == "tested"
                && expired["vendor_version"] == TESTED
                && expired["refusals"].as_array().is_none_or(Vec::is_empty)
                && completed(&envelope)
                && envelope["version_status"] == "tested"
                && d.launches()?.len() == 2,
            || format!("600 s on: describe {expired}, turn {envelope}"),
        )
    })
}

/// Critical r1 #2 through the daemon: a resume's admission reads the
/// refusal cache. Session A completes its first turn; session B's launch
/// of the same recipe on the same binary fails its handshake and caches a
/// refusal; A's resume is then refused `harness_unavailable` with
/// `data.reason:"handshake_refused"` before any receipt: no turn 2, no
/// third launch.
#[test]
fn claude_s_launch_cached_refusal_refuses_resume() -> TestResult {
    scenario("claude_s_launch_refusal_resume", |d, evidence| {
        d.replay(&[
            completing(Launch::New, false, "ONE", 0.001),
            refused_handshake("TWO"),
        ])?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let healthy = session_of(&d.spawn(evidence, "spawn-a", &ask("ONE"), &[])?)?;
        let first = d.wait(evidence, &format!("{healthy}/1"))?;
        check(completed(&first), || format!("session A's turn: {first}"))?;
        let other = session_of(&d.spawn(evidence, "spawn-b", &ask("TWO"), &[])?)?;
        let refused = d.wait(evidence, &format!("{other}/1"))?;
        check(
            refused["failure"]["class"] == "protocol" && replay_ran(&refused, 0),
            || format!("session B's refused handshake: {refused}"),
        )?;
        let error = d.refused(
            evidence,
            "resume-a",
            &[
                "resume", &healthy, "--prompt", "p", "--handle", HANDLE, "--json",
            ],
            "harness_unavailable",
        )?;
        check(
            error["data"]["reason"] == "handshake_refused"
                && d.rows("turns")? == 2
                && d.launches()?.len() == 2,
            || format!("A's resume: {error}"),
        )
    })
}

/// Critical r1 #1 through the daemon: a launch request (Host's
/// `Configure` frame) past the 64 KiB cap the anchor reads under is
/// refused `invalid_params` before any receipt, naming the value that
/// carries it: 16,500 bytes of instructions alone, and 8,300 bytes of
/// instructions with a schema of about 8,450, each of which fits alone,
/// naming the larger. Nothing is committed or launched. Instructions of
/// 15,000 bytes are admitted and launch through Host with their whole
/// argv (the replay pins it). The daemon's `PATH` is fixed so the
/// launch environment's size is known.
#[test]
fn claude_s_launch_request_past_host_cap_refused() -> TestResult {
    scenario("claude_s_launch_request_cap", |d, evidence| {
        let file = |name: &str, content: &str| -> Result<String, ScenarioError> {
            let path = d.root.path().join(name);
            fs::write(&path, content).map_err(infra)?;
            d.stimulus(&path, "input", &json!(content.len()));
            Ok(path.to_string_lossy().into_owned())
        };
        let admitted = "z".repeat(15_000);
        let mut lifetime_argv = argv(Launch::New, false);
        if let Some(args) = lifetime_argv.as_array_mut() {
            args.extend([json!("--append-system-prompt"), json!(admitted)]);
        }
        let id = sid(Launch::New);
        d.replay(&[lifetime(
            &lifetime_argv,
            vec![
                prompt(&ask("ONE")),
                init(id),
                reply(id, "ONE"),
                result(id, "ONE", 0.001),
                await_eof(),
            ],
        )])?;
        let _daemon = Daemon::start_with(d, evidence, "final", &[("PATH", "/usr/bin:/bin")])?;
        let over = file("over.txt", &"z".repeat(16_500))?;
        let half = file("half.txt", &"z".repeat(8_300))?;
        let schema = file(
            "schema.json",
            &json!({"type": "object", "description": "z".repeat(8_400)}).to_string(),
        )?;
        let spawn = |name: &str, extra: &[&str]| {
            let work = d.work.to_string_lossy().into_owned();
            let mut args = vec![
                "spawn",
                "--harness",
                "claude",
                "--model",
                "haiku",
                "--prompt",
                "p",
                "--cwd",
                &work,
                "--handle",
                HANDLE,
                "--background",
                "--json",
            ];
            args.extend_from_slice(extra);
            d.refused(evidence, name, &args, "invalid_params")
        };
        for (name, extra, field) in [
            ("spawn-over", vec!["--instructions", &over], "instructions"),
            (
                "spawn-combined",
                vec!["--instructions", &half, "--output-schema", &schema],
                "output_schema",
            ),
        ] {
            let error = spawn(name, &extra)?;
            check(
                error["data"]["field"] == field
                    && d.rows("sessions")? == 0
                    && d.launches()?.is_empty(),
                || format!("{name}: {error}"),
            )?;
        }
        let fits = file("fits.txt", &admitted)?;
        let session = session_of(&d.spawn(
            evidence,
            "spawn-fits",
            &ask("ONE"),
            &["--instructions", &fits],
        )?)?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        check(
            completed(&envelope) && d.rows("sessions")? == 1 && d.launches()?.len() == 1,
            || format!("the admitted launch: {envelope}"),
        )
    })
}

/// Each item the session channel took (test builds).
#[cfg(feature = "test-failpoints")]
const ADMITTED: &str = "adapter.observation.admitted";

/// When Wire handed Route its `n`th stdout message: the modification time
/// of `wire.messages.received`'s `n`th acknowledgement, once it exists
/// (armed `value_persist`, which acknowledges every hit).
#[cfg(feature = "test-failpoints")]
fn received_at(d: &Deployment, n: u64) -> Result<SystemTime, ScenarioError> {
    let path = d
        .root
        .path()
        .join("failpoints")
        .join(format!("{RECEIVED}.{n}.ack"));
    let deadline = Instant::now() + WAIT;
    loop {
        match fs::metadata(&path) {
            Ok(metadata) => return metadata.modified().map_err(infra),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(infra(error)),
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!("no read {n} acknowledged")));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Critical r1 #3 (bead via-mnx, C2 §4, runtime §8): an observation's
/// instant is when Route read its message, not when the Adapter dequeued
/// it, so a delayed dequeue under backpressure neither extends nor expires
/// the idle deadline wrongly. The turn's idle budget is 2 s. The Adapter's
/// delivery is held after the channel took its first item (the init's
/// identity), so the two assistant messages that follow wait in Route's
/// read-ahead and on the hop; the fake then emits nothing more. Released
/// 1 s after Route read the second message (Wire's third read, its
/// `wire.messages.received` acknowledgement, critical r2 #3), the
/// delivery hands both on. The idle deadline counts from that read: the
/// interrupt of the idle order reaches the vendor 2 s after it (1.9 s to
/// 2.5 s), not 2 s after the dequeue (about 3 s), and not before. The turn
/// ends `deadline_idle` and the fake answered the interrupt and saw its
/// EOF (exit 0).
#[cfg(feature = "test-failpoints")]
#[test]
fn claude_progress_keeps_its_read_instant_under_backpressure() -> TestResult {
    scenario("claude_progress_read_instant", |d, evidence| {
        let id = "${sid}";
        let second = emit(
            &json!({"type": "assistant", "message": {"model": "claude-haiku-4-5-20251001",
            "id": "msg_01SYNTH000002", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "still working"}],
            "usage": {"input_tokens": 7, "output_tokens": 3}},
            "parent_tool_use_id": null, "session_id": id}),
        );
        d.replay(&[lifetime(
            &argv(Launch::New, false),
            vec![
                prompt(&ask("ONE")),
                init(id),
                reply(id, "ONE"),
                second,
                gate(),
                json!({"expect": {"line": {"type": "control_request",
                    "request": {"subtype": "interrupt"}}, "capture": {"rid": "/request_id"}}}),
                json!({"emit": {"line": "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":${rid},\"response\":{\"still_queued\":[]}}}"}}),
                emit(
                    &json!({"type": "result", "subtype": "error_during_execution",
                    "is_error": true, "session_id": id, "stop_reason": "tool_use",
                    "terminal_reason": "aborted_tools"}),
                ),
                await_eof(),
            ],
        )])?;
        d.failpoints.arm(ADMITTED, 1, "pause").map_err(infra)?;
        d.failpoints
            .arm(RECEIVED, 1, "value_persist:1")
            .map_err(infra)?;
        let daemon = Daemon::start(d, evidence, "final")?;
        let session =
            session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &["--idle-ms", "2000"])?)?;
        d.failpoints
            .wait_ack(ADMITTED, 1, "pause", daemon.pid(), WAIT)
            .map_err(infra)?;
        let read = received_at(d, 3)?;
        d.await_progress("at 6 launch 1")?;
        d.release(1)?;
        let held = SystemTime::now()
            .duration_since(read)
            .unwrap_or(Duration::ZERO);
        thread::sleep(Duration::from_secs(1).saturating_sub(held));
        d.failpoints.release(ADMITTED, 1).map_err(infra)?;
        d.await_progress("read 2 launch 1")?;
        let interrupted = SystemTime::now()
            .duration_since(read)
            .unwrap_or(Duration::ZERO);
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        check(
            envelope["state"] == "failed"
                && envelope["failure"]["class"] == "deadline_idle"
                && replay_ran(&envelope, 0)
                && interrupted >= Duration::from_millis(1_900)
                && interrupted <= Duration::from_millis(2_500),
            || format!("the idle order's interrupt {interrupted:?} after Route's read: {envelope}"),
        )
    })
}

/// Critical r2 #2 (runtime §8): Core's idle deadline waits for its decode
/// fence. The turn's idle budget is 2 s. The Adapter's delivery is held
/// after the channel took its first item (the init's identity). Route
/// then reads three unknown messages (non-progress noise) and, 1.5 s
/// after the spawn call began and so before the deadline, the turn's
/// first assistant message (its acceptance and progress), all held behind
/// the delivery in its read-ahead. The hold outlasts the deadline by at
/// least 500 ms, and ends before that progress's own deadline (its read
/// plus 2 s). When the deadline fires,
/// Route had read that progress, so Core reconciles everything read by
/// then before it decides: released, the delivery hands it on, its read
/// instant moves the deadline, and the turn completes with no cancel (the
/// fake's replay, strict about input, saw no interrupt, and its EOF:
/// exit 0). Bounds are wall-clock: the submission is after the spawn call
/// began and before the fake read its prompt.
#[cfg(feature = "test-failpoints")]
#[test]
fn claude_idle_waits_for_the_decode_fence() -> TestResult {
    scenario("claude_idle_decode_fence", |d, evidence| {
        let id = "${sid}";
        let noise = |n: u64| emit(&json!({"type": "via_test_noise", "n": n}));
        d.replay(&[lifetime(
            &argv(Launch::New, false),
            vec![
                prompt(&ask("ONE")),
                init(id),
                noise(1),
                noise(2),
                noise(3),
                gate(),
                reply(id, "ONE"),
                gate(),
                result(id, "ONE", 0.001),
                await_eof(),
            ],
        )])?;
        d.failpoints.arm(ADMITTED, 1, "pause").map_err(infra)?;
        d.failpoints
            .arm(RECEIVED, 1, "value_persist:1")
            .map_err(infra)?;
        let daemon = Daemon::start(d, evidence, "final")?;
        let asked = SystemTime::now();
        let session =
            session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &["--idle-ms", "2000"])?)?;
        d.await_progress("read 1 launch 1")?;
        // The submission was by now: the deadline is by now plus 2 s.
        let deadline_by = SystemTime::now() + Duration::from_secs(2);
        d.failpoints
            .wait_ack(ADMITTED, 1, "pause", daemon.pid(), WAIT)
            .map_err(infra)?;
        d.await_progress("at 7 launch 1")?;
        // The progress comes late enough that its own deadline, its read
        // plus 2 s, is still ahead when the fence opens.
        let late = asked + Duration::from_millis(1_500);
        if let Ok(left) = late.duration_since(SystemTime::now()) {
            thread::sleep(left);
        }
        d.release(1)?;
        // Route read the progress (Wire's fifth read) before the deadline,
        // which is no earlier than the spawn call's start plus 2 s.
        let progress = received_at(d, 5)?;
        let read_after = progress.duration_since(asked).ok();
        d.await_progress("at 9 launch 1")?;
        let hold = deadline_by + Duration::from_millis(500);
        if let Ok(left) = hold.duration_since(SystemTime::now()) {
            thread::sleep(left);
        }
        d.failpoints.release(ADMITTED, 1).map_err(infra)?;
        d.release(1)?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        check(
            read_after.is_some_and(|after| after < Duration::from_millis(1_800))
                && completed(&envelope)
                && envelope["cancel"].is_null(),
            || format!("progress read {read_after:?} after the spawn began: {envelope}"),
        )
    })
}

/// Critical r1 #5 (packet §6, `claude_fifo_busy_input` through the
/// daemon): a resume admitted while turn 1's Bash tool runs is queued, not
/// written to the busy process. Turn 1's process is gated inside its tool
/// (`running_tools` reported); the resume's receipt is `queued`, and while
/// the tool still runs that process has read one input line and nothing
/// else launched. Released, turn 1 completes and its process exits 0
/// (strict input: a second user line before its EOF would have failed it);
/// turn 2 then runs as a second process resuming the confirmed UUID. The
/// first process read exactly one line in all.
#[test]
fn claude_fifo_busy_input_through_daemon() -> TestResult {
    scenario("claude_fifo_busy_input", |d, evidence| {
        let id = "${sid}";
        let tool = emit(
            &json!({"type": "assistant", "message": {"model": "claude-haiku-4-5-20251001",
            "id": "msg_01SYNTH000001", "type": "message", "role": "assistant",
            "content": [{"type": "tool_use", "id": "toolu_01SYNTH000001", "name": "Bash",
                "input": {"command": "sleep 1"}}],
            "usage": {"input_tokens": 7, "output_tokens": 3}},
            "parent_tool_use_id": null, "session_id": id}),
        );
        let ended = emit(&json!({"type": "user", "message": {"role": "user",
            "content": [{"tool_use_id": "toolu_01SYNTH000001", "type": "tool_result",
                "content": "ok", "is_error": false}]},
            "parent_tool_use_id": null, "session_id": id}));
        let mut lives = vec![lifetime(
            &argv(Launch::New, false),
            vec![
                prompt(&ask("FIRST")),
                init(id),
                tool,
                gate(),
                ended,
                reply(id, "FIRST"),
                result(id, "FIRST", 0.001),
                await_eof(),
            ],
        )];
        d.replay(&lives)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("FIRST"), &[])?)?;
        d.await_progress("at 5 launch 1")?;
        await_running_tool(d, evidence, &session)?;
        let receipt = d.resume(evidence, "resume-busy", &session, &ask("SECOND"))?;
        let uuid = d.status(evidence, "status-busy", &session)?["vendor_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let reads = |d: &Deployment| -> Result<usize, ScenarioError> {
            Ok(Deployment::log_of(&d.replayed.borrow(), "progress")?
                .lines()
                .filter(|line| line.starts_with("read ") && line.ends_with(" launch 1"))
                .count())
        };
        // Room for a wrongly dispatched prompt to arrive before the check.
        thread::sleep(Duration::from_millis(300));
        let (busy_reads, busy_launches) = (reads(d)?, d.launches()?.len());
        check(
            receipt["state"] == "queued"
                && !uuid.is_empty()
                && busy_reads == 1
                && busy_launches == 1,
            || {
                format!(
                    "during the tool: receipt {receipt}, {busy_reads} lines read, \
                     {busy_launches} launches"
                )
            },
        )?;
        lives.push(completing(Launch::Resume(&uuid), false, "SECOND", 0.002));
        d.replay(&lives)?;
        d.release(1)?;
        let first = d.wait(evidence, &format!("{session}/1"))?;
        let second = d.wait(evidence, &format!("{session}/2"))?;
        check(
            completed(&first) && completed(&second) && reads(d)? == 1 && d.launches()?.len() == 2,
            || format!("turn 1 {first}; turn 2 {second}"),
        )
    })
}

/// One schema launch (packet §9 `claude_schema_replace_clear` shapes): the
/// `StructuredOutput` tool in the init, and a `success` result carrying
/// `output`, or no `structured_output` member at all when `None`.
fn schema_launch(launch: Launch<'_>, schema: &str, text: &str, output: Option<&Value>) -> Value {
    let id = sid(launch);
    let mut argv = argv(launch, false);
    if let Some(args) = argv.as_array_mut() {
        args.extend([json!("--json-schema"), json!(schema)]);
    }
    let init = json!({"type": "system", "subtype": "init", "cwd": "/work/project",
        "session_id": id, "tools": ["Bash", "Edit", "Glob", "Grep", "Read", "Write",
        "StructuredOutput"], "mcp_servers": [], "model": "claude-haiku-4-5-20251001",
        "permissionMode": "dontAsk", "apiKeySource": "none", "claude_code_version": TESTED,
        "uuid": "00000000-0000-4000-8000-000000000001",
        "capabilities": ["interrupt_receipt_v1", "interrupt_cancel_queued_v1",
            "msg_lifecycle_v1"]});
    let mut result = json!({"type": "result", "subtype": "success", "is_error": false,
        "session_id": id, "stop_reason": "end_turn", "terminal_reason": "completed",
        "num_turns": 1, "total_cost_usd": 0.001,
        "usage": {"input_tokens": 10, "cache_creation_input_tokens": 100,
            "cache_read_input_tokens": 1000, "output_tokens": 20},
        "permission_denials": [], "result": text});
    if let Some(output) = output {
        result["structured_output"] = output.clone();
    }
    lifetime(
        &argv,
        vec![
            prompt(&ask(text)),
            emit(&init),
            reply(id, text),
            emit(&result),
            await_eof(),
        ],
    )
}

/// Critical r1 #6 (packet §9, C1 §5): Claude's structured output through
/// Core's validation into the public envelope. Turn 1's result carries a
/// value its schema refuses (`a` must be a string): the turn fails
/// `structured_output_invalid` with `data.reason:"invalid"`, the value
/// kept. Turn 2 inherits the schema and its result carries none: it
/// completes with the `structured_output_missing` warning and a null
/// output. Both launches pass the compact schema and ran their replays
/// through (exit 0).
#[test]
fn claude_structured_output_invalid_and_missing() -> TestResult {
    scenario("claude_structured_output_envelope", |d, evidence| {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}},
            "required": ["a"]});
        let compact = schema.to_string();
        let path = d.root.path().join("schema.json");
        fs::write(&path, &compact).map_err(infra)?;
        d.stimulus(&path, "input", &schema);
        let path = path.to_string_lossy().into_owned();
        let invalid = json!({"a": 5});
        let mut lives = vec![schema_launch(Launch::New, &compact, "ONE", Some(&invalid))];
        d.replay(&lives)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session =
            session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &["--output-schema", &path])?)?;
        let first = d.wait(evidence, &format!("{session}/1"))?;
        let uuid = first["vendor_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        check(
            first["state"] == "failed"
                && first["failure"]["class"] == "structured_output_invalid"
                && first["failure"]["data"]["reason"] == "invalid"
                && first["structured_output"] == invalid
                && replay_ran(&first, 0)
                && !uuid.is_empty(),
            || format!("an invalid structured output: {first}"),
        )?;
        lives.push(schema_launch(Launch::Resume(&uuid), &compact, "TWO", None));
        d.replay(&lives)?;
        d.resume(evidence, "resume", &session, &ask("TWO"))?;
        let second = d.wait(evidence, &format!("{session}/2"))?;
        let warned = second["warnings"].as_array().is_some_and(|warnings| {
            warnings
                .iter()
                .any(|warning| warning["code"] == "structured_output_missing")
        });
        check(
            completed(&second) && warned && second["structured_output"].is_null(),
            || format!("a missing structured output: {second}"),
        )
    })
}

/// S-LAUNCH, packet §4 B7: the vendor's environment comes only from the
/// adapter's allow-list (`HOME`, `PATH`, `LANG`, from the daemon's start)
/// plus Host's `VIA_PROCESS_MARKER`; the daemon's other names, credentials
/// among them, never reach it. Read from the vendor's `/proc` while it
/// waits at a gate; values are compared, never shown.
#[test]
fn claude_s_launch_env_is_the_allow_list() -> TestResult {
    scenario("claude_s_launch_env", |d, evidence| {
        let id = "${sid}";
        d.replay(&[lifetime(
            &argv(Launch::New, false),
            vec![
                prompt(&ask("ONE")),
                gate(),
                init(id),
                reply(id, "ONE"),
                result(id, "ONE", 0.001),
                await_eof(),
            ],
        )])?;
        let home = d.root.path().join("home");
        let home = home.to_string_lossy();
        let secret = "c3-daemon-only-value";
        let _daemon = Daemon::start_with(
            d,
            evidence,
            "final",
            &[
                ("HOME", &home),
                ("LANG", "C.UTF-8"),
                ("USER", "via-test"),
                ("LOGNAME", "via-test"),
                ("ANTHROPIC_API_KEY", secret),
                ("CLAUDE_CONFIG_DIR", secret),
            ],
        )?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        d.await_progress("at 3 launch 1")?;
        let environ = d.proc_entries(1, "environ")?;
        d.release(1)?;
        let names: BTreeSet<&str> = environ
            .iter()
            .filter_map(|entry| entry.split_once('=').map(|(name, _)| name))
            .collect();
        let value = |name: &str| {
            environ
                .iter()
                .find_map(|entry| entry.strip_prefix(&format!("{name}=")))
                .map(str::to_owned)
        };
        let path = std::env::var("PATH").unwrap_or_default();
        check(
            names == BTreeSet::from(["HOME", "LANG", "PATH", "VIA_PROCESS_MARKER"])
                && value("HOME").as_deref() == Some(&*home)
                && value("LANG").as_deref() == Some("C.UTF-8")
                && value("PATH").as_deref() == Some(path.as_str())
                && !environ.iter().any(|entry| entry.contains(secret)),
            || format!("the vendor's environment names: {names:?}"),
        )?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        check(completed(&envelope), || envelope.to_string())
    })
}

// --------------------------------------------------------- stream limits

/// Packet §9 `claude_stream_limits`, a large final payload: a 300 KiB
/// final text is past C1 §5's inline 256 KiB, so the envelope names
/// `final_text.txt` in the turn's evidence folder with its exact bytes,
/// not truncated, and `final_text` is null; the file holds the text.
#[test]
fn claude_stream_limits_final_text_file() -> TestResult {
    scenario("claude_stream_limits_final_text", |d, evidence| {
        let id = "${sid}";
        let text = "x".repeat(300 * 1024);
        d.replay(&[lifetime(
            &argv(Launch::New, false),
            vec![
                prompt(&ask("LARGE")),
                init(id),
                reply(id, &text),
                result(id, &text, 0.001),
                await_eof(),
            ],
        )])?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("LARGE"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        let file = &envelope["final_text_file"];
        let written = file["path"]
            .as_str()
            .map(fs::read_to_string)
            .transpose()
            .map_err(infra)?
            .unwrap_or_default();
        check(
            completed(&envelope)
                && envelope["final_text"].is_null()
                && file["bytes"] == text.len()
                && file["truncated"] == false
                && written == text,
            || {
                format!(
                    "the large final text: {file} ({} bytes read)",
                    written.len()
                )
            },
        )
    })
}

/// How a caller stops a turn under stream pressure.
#[derive(Clone, Copy)]
enum CallerStop {
    /// `via cancel` of the turn.
    Cancel,
    /// `via close` of its session.
    Close,
}

/// `path` as one single-quoted shell word: each `'` closes the quote, adds
/// an escaped quote and reopens it.
fn shell_word(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// The lifetime of a turn whose Bash tool a caller stops: the interrupt
/// is answered with the nested receipt, then the abort result.
fn interrupted_tool() -> Value {
    let id = "${sid}";
    lifetime(
        &argv(Launch::New, false),
        vec![
            prompt("Run sleep 30 with Bash."),
            init(id),
            emit(
                &json!({"type": "assistant", "message": {"model": "claude-haiku-4-5-20251001",
                "id": "msg_01SYNTH000001", "type": "message", "role": "assistant",
                "content": [{"type": "tool_use", "id": "toolu_01SYNTH000001", "name": "Bash",
                    "input": {"command": "sleep 30"}}],
                "usage": {"input_tokens": 7, "output_tokens": 3}},
                "parent_tool_use_id": null, "session_id": id}),
            ),
            json!({"expect": {"line": {"type": "control_request",
                "request": {"subtype": "interrupt"}}, "capture": {"rid": "/request_id"}}}),
            json!({"emit": {"line": "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":${rid},\"response\":{\"still_queued\":[]}}}"}}),
            emit(
                &json!({"type": "result", "subtype": "error_during_execution",
                "is_error": true, "session_id": id, "stop_reason": "tool_use",
                "terminal_reason": "aborted_tools"}),
            ),
            await_eof(),
        ],
    )
}

/// A file in the deployment's `sync` directory, once it exists.
fn await_sync(d: &Deployment, name: &str) -> Result<String, ScenarioError> {
    let path = d.root.path().join("sync").join(name);
    let deadline = Instant::now() + WAIT;
    loop {
        match fs::read_to_string(&path) {
            Ok(text) => return Ok(text.trim().to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(infra(error)),
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!("no {name} in sync")));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Whether process `pid` exists (signal 0).
fn alive(pid: u32) -> Result<bool, ScenarioError> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| infra(format!("bad pid {pid}")))?;
    match rustix::process::test_kill_process(pid) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(infra(error)),
    }
}

/// Packet §9 `claude_stream_limits`, control service under a stderr flood:
/// beside the vendor a writer in its group floods stderr (the turn's
/// `stderr.log`, which the anchor drains under its cap and no daemon task
/// reads, runtime §4) without end, 256 KiB at a time. It records its pid and a
/// readiness marker, then a heartbeat after every chunk. The caller stops
/// the turn during its tool only once the writer is ready, alive and its
/// heartbeat still advancing, so the stop lands mid-flood. The interrupt
/// is serviced (the nested receipt and the abort terminal acknowledge it)
/// within the stop's bound, and the group's cleanup is proved with the
/// writer dead. Memory under a flood is
/// `claude_stream_limits_stderr_flood_memory`'s.
fn stderr_flood_under(stop: CallerStop, name: &str) -> TestResult {
    scenario(name, |d, evidence| {
        let sync = shell_word(&d.root.path().join("sync"));
        d.wrap(&format!(
            "sh -c 'echo $$ > \"$1/flood.pid\"; : > \"$1/flood.ready\"; i=0; \
             while :; do head -c 262144 /dev/zero | tr \"\\000\" e >&2; sleep 0.01; \
             i=$((i+1)); echo $i > \"$1/flood.beat.tmp\"; \
             mv \"$1/flood.beat.tmp\" \"$1/flood.beat\"; done' flood {sync} >/dev/null &"
        ))
        .map_err(infra)?;
        d.replay(&[interrupted_tool()])?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn", "Run sleep 30 with Bash.", &[])?)?;
        d.await_event(&session, 1, "turn.started")?;
        await_running_tool(d, evidence, &session)?;
        await_sync(d, "flood.ready")?;
        let writer: u32 = await_sync(d, "flood.pid")?.parse().map_err(infra)?;
        // The heartbeat advances past a value read now: the writer is
        // still flooding just before the stop.
        let beat = |d: &Deployment| -> Result<u64, ScenarioError> {
            await_sync(d, "flood.beat")?.parse().map_err(infra)
        };
        let first = beat(d)?;
        let deadline = Instant::now() + WAIT;
        let before = loop {
            let now = beat(d)?;
            if now > first {
                break now;
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(
                    "the flood's heartbeat stalled".to_owned(),
                ));
            }
            thread::sleep(Duration::from_millis(5));
        };
        check(alive(writer)?, || {
            "the flood writer exited before the stop".to_owned()
        })?;
        let stopped = Instant::now();
        let reply = match stop {
            CallerStop::Cancel => d.ok(
                evidence,
                "cancel",
                &[
                    "cancel", &session, "--turn", "1", "--handle", HANDLE, "--json",
                ],
            )?,
            CallerStop::Close => d.ok(
                evidence,
                "close",
                &["close", &session, "--handle", HANDLE, "--json"],
            )?,
        };
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        let took = stopped.elapsed();
        let dead = !alive(writer)?;
        let closed = match stop {
            CallerStop::Cancel => true,
            CallerStop::Close => reply["state"] == "closed" && reply["cleanup"] == "quiescent",
        };
        let measured = json!({"beat_at_stop": before, "ended_after_stop_ms": took.as_millis(),
            "writer_dead_after": dead});
        evidence
            .write("measured.json", measured.to_string().as_bytes())
            .map_err(infra)?;
        check(
            envelope["state"] == "cancelled"
                && envelope["cancel"]["outcome"] == "acknowledged"
                && envelope["cancel"]["cleanup"] == "quiescent"
                && replay_ran(&envelope, 0)
                && closed
                && took < STOP_BOUND
                && dead,
            || {
                format!(
                    "stop {reply}; envelope {envelope}; ended {took:?} after the stop; \
                     writer dead after cleanup {dead}"
                )
            },
        )
    })
}

/// The bound on a caller's stop under the flood: C1 §3.5's default force
/// after 10 s is never needed here (the interrupt is answered), so the
/// turn ends within the close's cleanup allowance; 10 s is generous.
const STOP_BOUND: Duration = Duration::from_secs(10);

#[test]
fn claude_stream_limits_stderr_flood_keeps_cancel() -> TestResult {
    stderr_flood_under(CallerStop::Cancel, "claude_stream_limits_stderr_flood")
}

#[test]
fn claude_stream_limits_stderr_flood_keeps_close() -> TestResult {
    stderr_flood_under(CallerStop::Close, "claude_stream_limits_stderr_flood_close")
}

/// The sustained flood's size: twice [`RSS_BOUND_KIB`].
const FLOOD: u64 = 64 * 1024 * 1024;

/// What runtime §4's per-turn cap keeps of a flood besides its marker line:
/// the first 4 MiB and the last 1 MiB.
const KEPT: u64 = 5 * 1024 * 1024;

/// Packet §9 `claude_stream_limits`, memory under a sustained stderr
/// flood: before the vendor runs, its stderr receives the whole 64 MiB
/// (`stderr.log` holds exactly its capped first 4 MiB, the dropped-bytes
/// marker line and its last 1 MiB when the turn ends, bead via-c2r), then
/// the turn
/// completes. The daemon's peak RSS stays within [`RSS_BOUND_KIB`] of its
/// baseline, half the flood, so a daemon that held the flood in memory
/// fails. Accepted limitation: Claude's observation-stall path (C2 A1) is
/// outside every RSS measurement here; it is bounded by count and bytes
/// (runtime §8) and exercised by `claude_observation_stall_interrupts`.
#[test]
fn claude_stream_limits_stderr_flood_memory() -> TestResult {
    scenario("claude_stream_limits_stderr_memory", |d, evidence| {
        d.wrap(&format!("head -c {FLOOD} /dev/zero | tr '\\000' e >&2"))
            .map_err(infra)?;
        d.replay(&[completing(Launch::New, false, "ONE", 0.001)])?;
        let daemon = Daemon::start_with(d, evidence, "final", MEASURED)?;
        let rss = Rss::watch(daemon.pid())?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        let (baseline, peak) = rss.finish()?;
        let written = envelope["evidence"]["folder"]
            .as_str()
            .map(|folder| fs::metadata(Path::new(folder).join("stderr.log")))
            .transpose()
            .map_err(infra)?
            .map_or(0, |meta| meta.len());
        let measured = json!({"baseline_kib": baseline, "peak_kib": peak,
            "bound_kib": RSS_BOUND_KIB, "stderr": written, "flood": FLOOD});
        evidence
            .write("measured.json", measured.to_string().as_bytes())
            .map_err(infra)?;
        let marker = format!("\n[via: {} bytes of vendor stderr dropped]\n", FLOOD - KEPT);
        let capped = KEPT + marker.len() as u64;
        check(
            completed(&envelope)
                && written == capped
                && peak.saturating_sub(baseline) < RSS_BOUND_KIB,
            || {
                format!(
                    "stderr flood: {envelope}; stderr.log {written} bytes; \
                     RSS {baseline} KiB to {peak} KiB"
                )
            },
        )
    })
}

/// Waits until `status` shows the turn's tool running.
fn await_running_tool(
    d: &Deployment,
    evidence: &Evidence,
    session: &str,
) -> Result<(), ScenarioError> {
    let deadline = Instant::now() + WAIT;
    loop {
        let status = d.status(evidence, "status-tool", session)?;
        if status["progress"]["running_tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!("no running tool: {status}")));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// The oversize line's size: 256 times Wire's 1 MiB message bound.
const OVERSIZE: u64 = 256 * 1024 * 1024;

/// Packet §9 `claude_stream_limits`, oversize stdout: before the vendor
/// runs, its stdout carries one 256 MiB line, far past Wire's 1 MiB
/// message bound (runtime §3). The turn fails `overflow`, never a
/// successful truncated envelope; the message's first 64 KiB is kept as
/// `undecoded.bin` in the turn's folder, and cleanup is proved. Memory is
/// bounded: the daemon's peak RSS stays within [`RSS_BOUND_KIB`] of its
/// baseline, a small fraction of the line, so a reader that buffered the
/// stream before rejecting it fails.
#[test]
fn claude_stream_limits_oversize_stdout() -> TestResult {
    scenario("claude_stream_limits_oversize", |d, evidence| {
        d.wrap(&format!(
            "head -c {OVERSIZE} /dev/zero | tr '\\000' o; echo"
        ))
        .map_err(infra)?;
        let [term, exit] = stopped_by_host();
        d.replay(&[lifetime(
            &argv(Launch::New, false),
            vec![prompt(&ask("ONE")), term, exit],
        )])?;
        let daemon = Daemon::start_with(d, evidence, "final", MEASURED)?;
        let rss = Rss::watch(daemon.pid())?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        let (baseline, peak) = rss.finish()?;
        let undecoded = envelope["evidence"]["folder"]
            .as_str()
            .map(|folder| fs::metadata(Path::new(folder).join("undecoded.bin")))
            .transpose()
            .map_err(infra)?
            .map_or(0, |meta| meta.len());
        let status = d.status(evidence, "status", &session)?;
        let measured = json!({"baseline_kib": baseline, "peak_kib": peak,
            "bound_kib": RSS_BOUND_KIB, "line": OVERSIZE, "undecoded": undecoded});
        evidence
            .write("measured.json", measured.to_string().as_bytes())
            .map_err(infra)?;
        check(
            envelope["state"] == "failed"
                && envelope["failure"]["class"] == "overflow"
                && envelope["final_text"] == ""
                && status["process"]["cleanup"] == "quiescent"
                && undecoded == 64 * 1024
                && peak.saturating_sub(baseline) < RSS_BOUND_KIB,
            || {
                format!(
                    "oversize stdout: {envelope}; undecoded.bin {undecoded} bytes; \
                     RSS {baseline} KiB to {peak} KiB"
                )
            },
        )
    })
}

// ------------------------------------------------------------ memory

/// The bound on the daemon's RSS growth during one stream-limit scenario:
/// one session's holders (the 1 MiB message bound, its 64 KiB undecoded
/// prefix, the 4 MiB observation budget, runtime §8) with room for the
/// allocator, far under the streams these scenarios push (64 MiB of
/// stderr, a 256 MiB stdout line).
const RSS_BOUND_KIB: u64 = 32 * 1024;

/// The daemon environment of a measured run: F24's malloc arenas
/// (`s1_f24_memory.rs`, runtime §8), two on glibc, none set on musl.
const MEASURED: &[(&str, &str)] = if cfg!(target_env = "gnu") {
    &[("MALLOC_ARENA_MAX", "2")]
} else {
    &[]
};

/// F24's measurement (`s1_f24_memory.rs`): the daemon's `VmRSS` sampled
/// every 10 ms from `/proc/<pid>/status` after a settled baseline, and
/// its peak the higher of the samples and `VmHWM`. Every reading must
/// succeed: a missing sample is an infrastructure failure, never a pass.
/// Dropped without [`Rss::finish`], the sampler stops and is joined.
struct Rss {
    pid: u32,
    baseline: u64,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    sampler: Option<thread::JoinHandle<Result<u64, String>>>,
}

impl Rss {
    /// Settles (elapsed time only, as F24), reads the baseline and starts
    /// sampling.
    fn watch(pid: u32) -> Result<Self, ScenarioError> {
        thread::sleep(Duration::from_millis(300));
        let baseline = status_kib(pid, "VmRSS:").ok_or_else(|| infra("no daemon VmRSS"))?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = std::sync::Arc::clone(&stop);
        let sampler = thread::spawn(move || {
            let mut peak = None;
            while !stopping.load(Ordering::Acquire) {
                let rss = status_kib(pid, "VmRSS:")
                    .ok_or_else(|| format!("no VmRSS sample of daemon {pid}"))?;
                peak = Some(peak.map_or(rss, |peak: u64| peak.max(rss)));
                thread::sleep(Duration::from_millis(10));
            }
            peak.ok_or_else(|| format!("no RSS sample of daemon {pid}"))
        });
        Ok(Self {
            pid,
            baseline,
            stop,
            sampler: Some(sampler),
        })
    }

    /// Stops sampling: `(baseline, peak)` in KiB, or an infrastructure
    /// failure when any reading is missing.
    fn finish(mut self) -> Result<(u64, u64), ScenarioError> {
        self.stop.store(true, Ordering::Release);
        let sampled = self
            .sampler
            .take()
            .ok_or_else(|| infra("the RSS sampler is gone"))?
            .join()
            .map_err(|_| infra("the RSS sampler panicked"))?
            .map_err(infra)?;
        let hwm = status_kib(self.pid, "VmHWM:")
            .ok_or_else(|| infra(format!("no VmHWM of daemon {}", self.pid)))?;
        Ok((self.baseline, hwm.max(sampled)))
    }
}

impl Drop for Rss {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(sampler) = self.sampler.take() {
            let _ = sampler.join();
        }
    }
}

/// A `/proc/<pid>/status` field in KiB (F24's reader).
fn status_kib(pid: u32, field: &str) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
}
