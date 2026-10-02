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
//! evidence artifact (runtime §11.2).

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
    /// The fake harness's scenario: present so the evidence names it; no
    /// scenario here runs the fake harness.
    fixture: PathBuf,
    /// The Claude binary's directory: the link, its replay and logs.
    vendor: PathBuf,
    /// The sessions' working directory.
    work: PathBuf,
    /// The replaying fake's link: the configured binary, or the link a
    /// wrapper script runs ([`Deployment::wrap`]). Its replay and logs
    /// sit beside it.
    replayed: RefCell<PathBuf>,
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
    fn replay_for(binary: &Path, lifetimes: &[Value]) -> Result<(), ScenarioError> {
        let replay = json!({
            "source": "synthetic (x.3.2 C3): no vendor run; the shapes of the 2026-09-30 \
                       re-probe's recordings, trimmed to what the adapter reads",
            "lifetimes": lifetimes,
        });
        let mut path = binary.as_os_str().to_owned();
        path.push(".replay.json");
        fs::write(path, serde_json::to_vec_pretty(&replay).map_err(infra)?).map_err(infra)
    }

    /// [`Self::replay_for`] the replaying fake.
    fn replay(&self, lifetimes: &[Value]) -> Result<(), ScenarioError> {
        Self::replay_for(&self.replayed.borrow(), lifetimes)
    }

    /// Makes the configured binary a `/bin/sh` script that runs `prelude`,
    /// then replaces itself with the replaying fake, linked as
    /// `<vendor>/claude-real` (its replay and logs beside that link).
    fn wrap(&self, prelude: &str) -> TestResult {
        let real = self.vendor.join("claude-real");
        std::os::unix::fs::symlink(&self.fake, &real)?;
        let script = self.claude();
        fs::remove_file(&script)?;
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n{prelude}\nexec '{}' \"$@\"\n",
                real.to_string_lossy()
            ),
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
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
        check(capture.status.success(), || {
            format!(
                "via {args:?} exited {}: {}{}",
                capture.status,
                String::from_utf8_lossy(&capture.stderr),
                capture.notes()
            )
        })?;
        serde_json::from_slice(&capture.stdout).map_err(infra)
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
        // The fake's own side: its replay, launches and progress.
        for suffix in ["replay.json", "launches", "progress"] {
            let text = Self::log_of(&self.replayed.borrow(), suffix)?;
            evidence
                .write(&format!("claude.{suffix}"), text.as_bytes())
                .map_err(infra)?;
        }
        Ok(())
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
            let ready = daemon::serving_pid(&deployment.runtime) == Some(daemon.child.id());
            if Instant::now() > deadline {
                return Err(ScenarioError::Timeout("daemon readiness".to_owned()));
            }
            if ready {
                return Ok(daemon);
            }
            thread::sleep(Duration::from_millis(5));
        }
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

/// The recipe's argv (packet §4), with `--strict-mcp-config` while MCP
/// servers are requested off (OD2's default).
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
    argv.push(json!("--restricted"));
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
        let mut lives = vec![completing(Launch::New, true, "ONE", 0.001)];
        d.replay(&lives)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        let first = d.wait(evidence, &format!("{session}/1"))?;
        let uuid = first["vendor_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        check(first["state"] == "completed" && !uuid.is_empty(), || {
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
            &argv(resumed, true),
            vec![
                prompt(&ask("TWO")),
                gate(),
                init(&uuid),
                reply(&uuid, "TWO"),
                result(&uuid, "TWO", 0.002),
                await_eof(),
            ],
        ));
        lives.push(completing(resumed, true, "THREE", 0.003));
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
                envelope["state"] == "completed" && envelope["vendor_session_id"] == uuid.as_str(),
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
        check(submitted < Some(*at) && Some(*at) < started, || {
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
    lives.push(lifetime(&argv(Launch::Resume(uuid), true), steps));
    d.replay(lives)?;
    let reopened = d.count(session, "session.reopened")?;
    d.resume(evidence, "resume-4", session, &ask("FOUR"))?;
    let envelope = d.wait(evidence, &format!("{session}/4"))?;
    check(
        envelope["state"] == "failed" && envelope["failure"]["class"] == "resume_mismatch",
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
        let mut lives = vec![completing(Launch::New, true, "ONE", 0.001)];
        d.replay(&lives)?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        let first = d.wait(evidence, &format!("{session}/1"))?;
        let uuid = first["vendor_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let gone = emit(
            &json!({"type": "result", "subtype": "error_during_execution",
            "is_error": true, "num_turns": 0, "stop_reason": null, "session_id": uuid,
            "total_cost_usd": 0, "errors": [format!("No conversation found with session ID: {uuid}")]}),
        );
        lives.push(lifetime(
            &argv(Launch::Resume(&uuid), true),
            vec![
                prompt(&ask("TWO")),
                gone,
                await_eof(),
                json!({"exit": {"code": 1, "stderr": "No conversation found\n"}}),
            ],
        ));
        lives.push(completing(Launch::Resume(&uuid), true, "THREE", 0.002));
        d.replay(&lives)?;
        d.resume(evidence, "resume-2", &session, &ask("TWO"))?;
        let second = d.wait(evidence, &format!("{session}/2"))?;
        check(
            second["state"] == "failed"
                && second["failure"]["class"] == "submit_failed"
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
            third["state"] == "completed" && third["vendor_session_id"] == uuid.as_str(),
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
            &argv(Launch::New, true),
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
            first["timestamps"]["accepted_at"].is_null() && first["state"] != "completed",
            || format!("turn 1: {first}"),
        )?;
        lives.push(completing(Launch::NewAs(&expected), true, "TWO", 0.001));
        d.replay(&lives)?;
        d.resume(evidence, "resume-2", &session, &ask("TWO"))?;
        let second = d.wait(evidence, &format!("{session}/2"))?;
        check(
            second["state"] == "completed" && second["vendor_session_id"] == expected.as_str(),
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
            &argv(Launch::New, true),
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
            &argv(Launch::New, true),
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
            &argv(Launch::New, true),
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
/// while the daemon runs (another binary, MCP servers inherited) does not
/// reach a session spawned on that daemon: it still runs the old binary
/// with `--strict-mcp-config`. After the restart a new session runs the
/// new binary with MCP servers on; a session spawned before it runs the
/// new binary too (the binary is the daemon's, not frozen) but keeps its
/// frozen inheritance, so `--strict-mcp-config` stays. Each replay pins
/// its argv, so the wrong binary or recipe fails its launch.
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
            completing(Launch::New, true, "ONE", 0.001),
            completing(Launch::New, true, "TWO", 0.001),
        ])?;
        let daemon = Daemon::start(d, evidence, "before")?;
        let old = session_of(&d.spawn(evidence, "spawn-old", &ask("ONE"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{old}/1"))?;
        let uuid = envelope["vendor_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        d.config(&json!({"harnesses":{"claude":{"binary":next,
            "inherit":{"mcp_servers":true}}}}))
            .map_err(infra)?;
        let unchanged = session_of(&d.spawn(evidence, "spawn-unchanged", &ask("TWO"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{unchanged}/1"))?;
        check(envelope["state"] == "completed", || {
            format!("a session spawned before the restart: {envelope}")
        })?;
        daemon.shutdown()?;
        Deployment::replay_for(
            &next,
            &[
                completing(Launch::New, false, "THREE", 0.001),
                completing(Launch::Resume(&uuid), true, "FOUR", 0.002),
            ],
        )?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let new = session_of(&d.spawn(evidence, "spawn-new", &ask("THREE"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{new}/1"))?;
        check(envelope["state"] == "completed", || {
            format!("a session spawned after the restart: {envelope}")
        })?;
        d.resume(evidence, "resume-old", &old, &ask("FOUR"))?;
        let envelope = d.wait(evidence, &format!("{old}/2"))?;
        check(envelope["state"] == "completed", || {
            format!("the old session after the restart: {envelope}")
        })?;
        let counts = (
            d.launches()?.len(),
            Deployment::log_of(&next, "launches")?.lines().count(),
        );
        let inherit = |session: &str| -> Result<Value, ScenarioError> {
            Ok(d.status(evidence, &format!("status-{session}"), session)?["inherit"]
                ["mcp_servers"]
                .clone())
        };
        // Status reports the effective setting: `off` is verified by
        // `--strict-mcp-config`; inherited servers cannot be verified.
        check(
            counts == (2, 2) && inherit(&old)? == "off" && inherit(&new)? == "unknown",
            || format!("launches per binary {counts:?}"),
        )
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
            &argv(Launch::New, true),
            vec![
                prompt(&ask("ONE")),
                init_as(id, "2.1.290", &["interrupt_receipt_v1"]),
                reply(id, "ONE"),
                result(id, "ONE", 0.001),
                await_eof(),
            ],
        );
        let no_receipt = lifetime(
            &argv(Launch::New, true),
            vec![
                prompt(&ask("TWO")),
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
        );
        let after = completing(Launch::New, true, "THREE", 0.001);
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
            envelope["vendor_version"] == "2.1.290" && envelope["version_status"] == "untested",
            || format!("turn on an untested version: {envelope}"),
        )?;
        let plan = d.ok(evidence, "describe-1", &describe)?;
        let receipt = d.spawn(evidence, "spawn-2", &ask("TWO"), &[])?;
        check(
            plan["vendor_version"] == "2.1.290"
                && plan["version_status"] == "untested"
                && receipt["vendor_version"] == "2.1.290",
            || format!("describe {plan}, receipt {receipt}"),
        )?;
        let refused = d.wait(evidence, &format!("{}/1", session_of(&receipt)?))?;
        check(
            refused["state"] == "failed" && refused["failure"]["class"] == "protocol",
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
                && envelope["state"] == "completed",
            || format!("after the restart: describe {plan}, turn {envelope}"),
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
            &argv(Launch::New, true),
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
        check(envelope["state"] == "completed", || envelope.to_string())
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
            &argv(Launch::New, true),
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
            envelope["state"] == "completed"
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

/// Packet §9 `claude_stream_limits`, a stderr flood: the vendor writes
/// 16 MiB to stderr (the turn's `stderr.log`, which the operating system
/// writes and no VIA task reads, runtime §4) before it runs; the turn
/// still runs, and a cancel during its tool is serviced: the nested
/// receipt and the abort terminal acknowledge it, with cleanup proved.
#[test]
fn claude_stream_limits_stderr_flood_keeps_cancel() -> TestResult {
    scenario("claude_stream_limits_stderr_flood", |d, evidence| {
        const FLOOD: u64 = 16 * 1024 * 1024;
        d.wrap(&format!("head -c {FLOOD} /dev/zero | tr '\\000' e >&2"))
            .map_err(infra)?;
        let id = "${sid}";
        d.replay(&[lifetime(
            &argv(Launch::New, true),
            vec![
                prompt("Run sleep 30 with Bash."),
                init(id),
                emit(&json!({"type": "assistant", "message": {"model": "claude-haiku-4-5-20251001",
                    "id": "msg_01SYNTH000001", "type": "message", "role": "assistant",
                    "content": [{"type": "tool_use", "id": "toolu_01SYNTH000001", "name": "Bash",
                        "input": {"command": "sleep 30"}}],
                    "usage": {"input_tokens": 7, "output_tokens": 3}},
                    "parent_tool_use_id": null, "session_id": id})),
                json!({"expect": {"line": {"type": "control_request",
                    "request": {"subtype": "interrupt"}}, "capture": {"rid": "/request_id"}}}),
                json!({"emit": {"line": "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":${rid},\"response\":{\"still_queued\":[]}}}"}}),
                emit(&json!({"type": "result", "subtype": "error_during_execution",
                    "is_error": true, "session_id": id, "stop_reason": "tool_use",
                    "terminal_reason": "aborted_tools"})),
                await_eof(),
            ],
        )])?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn", "Run sleep 30 with Bash.", &[])?)?;
        d.await_event(&session, 1, "turn.started")?;
        await_running_tool(d, evidence, &session)?;
        let reply = d.ok(
            evidence,
            "cancel",
            &[
                "cancel", &session, "--turn", "1", "--handle", HANDLE, "--json",
            ],
        )?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        let stderr = envelope["evidence"]["folder"]
            .as_str()
            .map(|folder| fs::metadata(Path::new(folder).join("stderr.log")))
            .transpose()
            .map_err(infra)?
            .map_or(0, |meta| meta.len());
        check(
            envelope["state"] == "cancelled"
                && envelope["cancel"]["outcome"] == "acknowledged"
                && envelope["cancel"]["cleanup"] == "quiescent"
                && stderr == FLOOD,
            || format!("cancel {reply}; envelope {envelope}; stderr.log {stderr} bytes"),
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

/// Packet §9 `claude_stream_limits`, oversize stdout: before the vendor
/// runs, its stdout carries a 2 MiB line, past Wire's 1 MiB message
/// bound (runtime §3). The turn fails `overflow`, never a successful
/// truncated envelope; the message's first 64 KiB is kept as
/// `undecoded.bin` in the turn's folder, and cleanup is proved.
#[test]
fn claude_stream_limits_oversize_stdout() -> TestResult {
    scenario("claude_stream_limits_oversize", |d, evidence| {
        d.wrap("head -c 2097152 /dev/zero | tr '\\000' o; echo")
            .map_err(infra)?;
        let [term, exit] = stopped_by_host();
        d.replay(&[lifetime(
            &argv(Launch::New, true),
            vec![prompt(&ask("ONE")), term, exit],
        )])?;
        let _daemon = Daemon::start(d, evidence, "final")?;
        let session = session_of(&d.spawn(evidence, "spawn", &ask("ONE"), &[])?)?;
        let envelope = d.wait(evidence, &format!("{session}/1"))?;
        let undecoded = envelope["evidence"]["folder"]
            .as_str()
            .map(|folder| fs::metadata(Path::new(folder).join("undecoded.bin")))
            .transpose()
            .map_err(infra)?
            .map_or(0, |meta| meta.len());
        let status = d.status(evidence, "status", &session)?;
        check(
            envelope["state"] == "failed"
                && envelope["failure"]["class"] == "overflow"
                && envelope["final_text"] == ""
                && status["process"]["cleanup"] == "quiescent"
                && undecoded == 64 * 1024,
            || format!("oversize stdout: {envelope}; undecoded.bin {undecoded} bytes"),
        )
    })
}
