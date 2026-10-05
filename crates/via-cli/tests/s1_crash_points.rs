//! Crash points around spawn and submission (S1 F8, F10; runtime-contracts §11
//! failpoint table), driven through the real `via` binary, daemon and SQLite
//! Store with the test-only failpoint controller.
//!
//! F8: a crash inside `spawn`'s write leaves session, turn 1, handle hash and
//! queued event together or not at all, and a lost reply leaves one whole
//! session, whose turn the restarted daemon hands off and runs exactly once
//! (T2-C). F10: a crash after submission intent,
//! after the prompt reached the agent or before acceptance was recorded
//! restarts as `unknown` and is never sent again; the submission record
//! precedes any agent I/O.
#![cfg(feature = "test-failpoints")]

#[path = "support/anchors.rs"]
mod anchors;
#[path = "support/daemon.rs"]
#[expect(
    dead_code,
    reason = "shared support; this file uses the direct status probe"
)]
mod daemon;
#[path = "support/failpoints.rs"]
mod failpoints;
#[path = "support/hits.rs"]
mod hits;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use daemon::collect_available;
use failpoints::Failpoints;
use scenario::{Captured, ScenarioError, run_command, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const ACK_WAIT: Duration = Duration::from_secs(10);
const FINAL_SHUTDOWN: Duration = Duration::from_secs(10);
/// A valid caller handle: `h_` and 43 base64url digits whose last is `A`.
const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const OTHER_HANDLE: &str = "h_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA";

struct Paths {
    root: tempfile::TempDir,
    /// The scenario's one final teardown (runtime §11.2), which every
    /// daemon guard records into.
    teardown: outer_cleanup::Teardown,
    /// Daemon runs started so far, numbering their traces and reports.
    runs: std::sync::atomic::AtomicUsize,
    /// Command outputs that could not be written as evidence: reported at
    /// collection, beside the scenario's outcome, never in its place.
    lost_outputs: std::sync::Mutex<Vec<String>>,
    via: PathBuf,
    fake: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    sync: PathBuf,
    fixture: PathBuf,
    failpoints: Failpoints,
}

impl Paths {
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
        let root = tempfile::tempdir()?;
        let [state, runtime, sync] =
            ["state", "runtime", "sync"].map(|name| root.path().join(name));
        for path in [&state, &runtime, &sync] {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        let fixture_path = root.path().join("fixture.json");
        fs::write(&fixture_path, serde_json::to_vec(fixture)?)?;
        let failpoints = Failpoints::new(root.path())?;
        Ok(Self {
            root,
            teardown: outer_cleanup::Teardown::new(),
            runs: std::sync::atomic::AtomicUsize::new(0),
            lost_outputs: std::sync::Mutex::new(Vec::new()),
            via,
            fake,
            state,
            runtime,
            sync,
            fixture: fixture_path,
            failpoints,
        })
    }

    /// The failpoint directory `Failpoints::new` created under the root.
    fn failpoint_dir(&self) -> PathBuf {
        self.root.path().join("failpoints")
    }

    /// A client command: it never carries the failpoint activation inputs.
    fn command(&self) -> Command {
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

    fn run(
        &self,
        evidence: &Evidence,
        name: &str,
        args: &[&str],
    ) -> Result<Captured, ScenarioError> {
        let mut command = self.command();
        command.args(args);
        let mut capture = run_command(&mut command, Duration::from_secs(15)).map_err(infra)?;
        // The captured outcome first: a lost output write is attached to
        // the capture and reported at collection, never in its place; the
        // caller classifies the exit (S1-evidence2 fix round 2 finding 5,
        // fix round 3).
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

    /// Starts `via spawn --background` without waiting: its reply may never
    /// come. `extra` arguments, such as an idempotency key, follow.
    fn spawn_pending(
        &self,
        evidence: &Evidence,
        name: &str,
        prompt: &str,
        extra: &[&str],
    ) -> PendingClient<'_> {
        let stdout = evidence.dir.join(format!("{name}.stdout"));
        let stderr = evidence.dir.join(format!("{name}.stderr"));
        let mut command = self.command();
        command
            .args(spawn_args(prompt))
            .args(extra)
            .env("VIA_HANDLE", HANDLE)
            .stdin(Stdio::null());
        let child = File::create(&stdout)
            .and_then(|out| Ok((out, File::create(&stderr)?)))
            .and_then(|(out, err)| command.stdout(out).stderr(err).spawn());
        PendingClient {
            child: Some(child),
            stdout,
            stderr,
            teardown: &self.teardown,
        }
    }

    /// Starts the client command `args` with the test handle without
    /// waiting: its reply may never come.
    fn client_pending(&self, evidence: &Evidence, name: &str, args: &[&str]) -> PendingClient<'_> {
        let stdout = evidence.dir.join(format!("{name}.stdout"));
        let stderr = evidence.dir.join(format!("{name}.stderr"));
        let mut command = self.command();
        command
            .args(args)
            .env("VIA_HANDLE", HANDLE)
            .stdin(Stdio::null());
        let child = File::create(&stdout)
            .and_then(|out| Ok((out, File::create(&stderr)?)))
            .and_then(|(out, err)| command.stdout(out).stderr(err).spawn());
        PendingClient {
            child: Some(child),
            stdout,
            stderr,
            teardown: &self.teardown,
        }
    }

    /// A consistent read-only view of the committed rows the scenario asserts.
    fn store(&self) -> Result<rusqlite::Connection, ScenarioError> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(infra)?;
        store.busy_timeout(Duration::from_secs(1)).map_err(infra)?;
        Ok(store)
    }

    /// Row counts of every table a spawn writes, read in one transaction:
    /// sessions, turns, events, anchors and spawn keys.
    fn counts(&self) -> Result<[i64; 5], ScenarioError> {
        let mut store = self.store()?;
        let tx = store.transaction().map_err(infra)?;
        let mut counts = [0; 5];
        for (count, table) in
            counts
                .iter_mut()
                .zip(["sessions", "turns", "events", "anchors", "spawn_keys"])
        {
            *count = tx
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .map_err(infra)?;
        }
        Ok(counts)
    }

    /// The only session's id; fails unless exactly one exists.
    fn only_session(&self) -> Result<String, ScenarioError> {
        let store = self.store()?;
        let mut query = store.prepare("SELECT id FROM sessions").map_err(infra)?;
        let ids = query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(infra)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(infra)?;
        match ids.as_slice() {
            [id] => Ok(id.clone()),
            _ => Err(fail(&format!("expected one session, found {ids:?}"))),
        }
    }

    /// Turn 1's state, submission and acceptance timestamps and correlation.
    fn turn(&self, session: &str) -> Result<TurnRow, ScenarioError> {
        self.store()?
            .query_row(
                "SELECT state,prompt,submitted_at,accepted_at,correlation,envelope FROM turns WHERE session_id=?1 AND number=1",
                [session],
                |row| {
                    Ok(TurnRow {
                        state: row.get(0)?,
                        prompt: row.get(1)?,
                        submitted_at: row.get(2)?,
                        accepted_at: row.get(3)?,
                        correlation: row.get(4)?,
                        envelope: row.get(5)?,
                    })
                },
            )
            .map_err(infra)
    }

    /// Committed event types of a session in sequence order, with the check
    /// that sequences are dense from one.
    fn event_types(&self, session: &str) -> Result<Vec<String>, ScenarioError> {
        let store = self.store()?;
        let mut query = store
            .prepare("SELECT seq,event FROM events WHERE session_id=?1 ORDER BY seq")
            .map_err(infra)?;
        let rows = query
            .query_map([session], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(infra)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(infra)?;
        let mut types = Vec::new();
        for (index, (seq, event)) in rows.into_iter().enumerate() {
            let event: Value = serde_json::from_str(&event).map_err(infra)?;
            check(
                i64::try_from(index + 1) == Ok(seq) && event["seq"] == seq,
                || format!("event sequence is not dense at {seq}: {event}"),
            )?;
            types.push(event["type"].as_str().unwrap_or_default().to_owned());
        }
        Ok(types)
    }

    fn anchors_for(&self, session: &str) -> Result<i64, ScenarioError> {
        self.store()?
            .query_row(
                "SELECT count(*) FROM anchors WHERE owner_session=?1",
                [session],
                |row| row.get(0),
            )
            .map_err(infra)
    }

    /// Writes every committed envelope and event, read-only, as scenario evidence.
    fn write_store_evidence(&self, evidence: &Evidence) -> Result<(), ScenarioError> {
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
        Ok(())
    }
}

struct TurnRow {
    state: String,
    prompt: String,
    submitted_at: Option<String>,
    accepted_at: Option<String>,
    correlation: Option<String>,
    envelope: Option<String>,
}

/// A client whose reply the scenario may deliberately lose.
struct PendingClient<'a> {
    child: Option<std::io::Result<Child>>,
    stdout: PathBuf,
    stderr: PathBuf,
    /// The scenario's teardown, which records an unreaped client.
    teardown: &'a outer_cleanup::Teardown,
}

impl PendingClient<'_> {
    /// Waits for the client and returns its status, stdout and stderr.
    fn finish(mut self) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), ScenarioError> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| infra("pending client already finished"))?
            .map_err(infra)?;
        let status = wait_child(&mut child, Duration::from_secs(10));
        let Ok(Some(status)) = status else {
            let reaped =
                outer_cleanup::kill_and_reap(&mut child, Instant::now() + outer_cleanup::REAP);
            return Err(ScenarioError::Timeout(format!(
                "pending client never returned (killed, reaped in 1 s: {reaped})"
            )));
        };
        Ok((
            status,
            fs::read(&self.stdout).map_err(infra)?,
            fs::read(&self.stderr).map_err(infra)?,
        ))
    }
}

/// A scenario that fails before `finish` kills its client and reaps it,
/// polled for at most the 1 s reap allowance, never a blocking wait. A
/// client still unreaped then is recorded in the scenario's teardown as
/// incomplete cleanup (S1-evidence2 fix round 2, finding 7).
impl Drop for PendingClient<'_> {
    fn drop(&mut self) {
        if let Some(Ok(mut child)) = self.child.take() {
            let pid = child.id();
            let reaped =
                outer_cleanup::kill_and_reap(&mut child, Instant::now() + outer_cleanup::REAP);
            self.teardown.record(
                json!({"generation":format!("pending-client-{pid}"),"killed":true,"reaped":reaped}),
                (!reaped).then(|| format!("pending client {pid} was not reaped in 1 s")),
            );
        }
    }
}

/// Directly owned daemon child of one run. Its drop records the run's
/// teardown; the scenario's `cleanup.json` covers every run, crashed ones
/// included ([`daemon::collect_available`]).
struct Daemon<'a> {
    child: Child,
    paths: &'a Paths,
    /// `<n>-<run>`: the run's name in the teardown.
    run: String,
    /// The run's own cleanup report, `cleanup-<n>-<run>.json`.
    report: PathBuf,
    crash_snapshot: Option<Vec<outer_cleanup::AnchorRow>>,
    /// Set once the run was torn down: its drop then does nothing.
    torn_down: bool,
}

impl<'a> Daemon<'a> {
    /// Starts a daemon with the failpoint controller active; `run` names its
    /// trace and cleanup evidence.
    fn start(paths: &'a Paths, evidence: &Evidence, run: &str) -> Result<Self, ScenarioError> {
        let mut daemon = Self::spawn(paths, evidence, run)?;
        daemon.wait_ready()?;
        Ok(daemon)
    }

    /// `start` with the connection-slot pool lowered to `slots` (design §11).
    /// `fake` replaces the fake vendor binary path when given.
    fn start_slots(
        paths: &'a Paths,
        evidence: &Evidence,
        run: &str,
        slots: usize,
        fake: Option<&std::path::Path>,
    ) -> Result<Self, ScenarioError> {
        let mut daemon = Self::spawn_with(paths, evidence, run, Some(slots), fake)?;
        daemon.wait_ready()?;
        Ok(daemon)
    }

    /// Starts a daemon without waiting for it to admit requests.
    fn spawn(paths: &'a Paths, evidence: &Evidence, run: &str) -> Result<Self, ScenarioError> {
        Self::spawn_with(paths, evidence, run, None, None)
    }

    fn spawn_with(
        paths: &'a Paths,
        evidence: &Evidence,
        run: &str,
        slots: Option<usize>,
        fake: Option<&std::path::Path>,
    ) -> Result<Self, ScenarioError> {
        if paths.teardown.begun() {
            return Err(infra("a daemon started after the final teardown began"));
        }
        // Every run appends to its trace under its own header, never
        // truncating an earlier run's (S1-evidence2 fix round 2, finding 1).
        let trace = if run == "final" {
            evidence.dir.join("daemon.trace")
        } else {
            evidence.dir.join(format!("daemon-{run}.trace"))
        };
        let number = paths
            .runs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let run = format!("{number}-{run}");
        let mut trace = File::options()
            .create(true)
            .append(true)
            .open(&trace)
            .map_err(infra)?;
        writeln!(trace, "=== daemon run {run} ===").map_err(infra)?;
        let mut command = paths.command();
        paths.failpoints.activate(&mut command);
        if let Some(slots) = slots {
            command.env("VIA_TEST_CONNECTION_SLOTS", slots.to_string());
        }
        if let Some(fake) = fake {
            command.env("VIA_FAKE_AGENT_BINARY", fake);
        }
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(trace);
        Ok(Self {
            child: command.spawn().map_err(infra)?,
            paths,
            report: evidence.dir.join(format!("cleanup-{run}.json")),
            run,
            crash_snapshot: None,
            torn_down: false,
        })
    }

    /// Waits until the daemon answers `daemon status`, i.e. admits requests.
    fn wait_ready(&mut self) -> Result<(), ScenarioError> {
        let paths = self.paths;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().map_err(infra)? {
                return Err(fail(&format!("daemon exited before readiness: {status}")));
            }
            // A direct probe: never auto-starts a second daemon, even over
            // the stale socket file a killed daemon left.
            let ready = daemon::serving_pid(&paths.runtime) == Some(self.child.id());
            if Instant::now() > deadline {
                return Err(ScenarioError::Timeout("daemon readiness".to_owned()));
            }
            if ready {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Kills the daemon without any shutdown, as a crash would, and reaps it.
    fn kill(&mut self) -> Result<(), ScenarioError> {
        self.crash_snapshot = Some(self.capture()?);
        self.child.kill().map_err(infra)?;
        if !outer_cleanup::reap_by(&mut self.child, Instant::now() + outer_cleanup::REAP) {
            return Err(ScenarioError::Timeout(
                "the killed daemon was not reaped in 1 s".to_owned(),
            ));
        }
        Ok(())
    }

    /// The committed anchor rows before a deliberate crash, from a bounded
    /// read-only snapshot (runtime §11.2).
    fn capture(&self) -> Result<Vec<outer_cleanup::AnchorRow>, ScenarioError> {
        outer_cleanup::snapshot(
            &self.paths.state.join("store.sqlite3"),
            Instant::now() + outer_cleanup::TEARDOWN,
        )
        .map_err(infra)
    }

    /// Waits for a failpoint `crash`: the daemon aborts (SIGABRT) by itself.
    fn wait_crash(&mut self) -> Result<(), ScenarioError> {
        let status = wait_child(&mut self.child, Duration::from_secs(10))?
            .ok_or_else(|| ScenarioError::Timeout("daemon never crashed".to_owned()))?;
        check(status.signal() == Some(6), || {
            format!("daemon ended {status}, not by the crash point's abort")
        })?;
        self.crash_snapshot = Some(self.capture()?);
        Ok(())
    }
}

impl Drop for Daemon<'_> {
    /// The scenario's final teardown of this run (runtime §11.2): a
    /// daemon's drop begins, or joins, the scenario's one teardown deadline
    /// (`Paths::teardown`), which bounds the force-stop (at most 2 s), the
    /// exit wait, the kill's 1 s reap and the anchor cleanup. An exited
    /// daemon's drop begins or joins it too; a deliberate stop before a
    /// restart is `Daemon::shutdown`, with its own bound. The run is recorded in
    /// `cleanup-<n>-<run>.json` and the teardown, which
    /// [`daemon::collect_available`] validates as a whole.
    fn drop(&mut self) {
        if self.torn_down {
            return;
        }
        // Final, live or exited: it begins or joins the scenario's one
        // deadline (S1-evidence2 fix round 3, Sol r3 finding 1). Only an
        // explicit `shutdown()` has its own bound.
        let deadline = self.paths.teardown.begin();
        self.tear_down(deadline);
    }
}

impl Daemon<'_> {
    /// A deliberate intermediate shutdown of a live run before a restart,
    /// with its own runtime §11.2 bound: the final teardown has not begun,
    /// so the next run may start (S1-evidence2 fix round 2, finding 8).
    /// Recorded like the final one; a run that needed a kill is a timeout,
    /// any other cleanup failure an infrastructure failure.
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
        let paths = self.paths;
        let rows = self.crash_snapshot.take();
        paths.teardown.daemon_generation(
            &self.run,
            deadline,
            &mut self.child,
            Some((&paths.state.join("store.sqlite3"), rows)),
            Some(&self.report),
            |by| {
                outer_cleanup::run_within(
                    paths
                        .command()
                        .args(["daemon", "stop", "--force", "--json"]),
                    by,
                )
            },
        )
    }
}

/// Waits for `child` to exit; `None` when it is still running at `within`.
/// Each observation is timestamped after it returns: an exit observed only
/// after the deadline is `None`.
fn wait_child(child: &mut Child, within: Duration) -> Result<Option<ExitStatus>, ScenarioError> {
    let deadline = Instant::now() + within;
    loop {
        let status = child.try_wait().map_err(infra)?;
        if Instant::now() > deadline {
            return Ok(None);
        }
        if status.is_some() {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

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

/// Waits until `point` was hit at least `at_least` times; counting began
/// with [`hits::count`] before the first hit.
fn wait_hits(paths: &Paths, point: &str, at_least: u64) -> Result<(), ScenarioError> {
    let dir = paths.failpoint_dir();
    let deadline = Instant::now() + ACK_WAIT;
    while hits::hits(&dir, point).map_err(infra)? < at_least {
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!(
                "{point} was not hit {at_least} times"
            )));
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// A turn reached a pending, registered connection-slot reservation (§10).
const AWAITING_SLOT: &str = "core.dispatch.awaiting_slot";

/// Starts counting reservations that wait for a slot; call before the spawns.
fn count_waiters(paths: &Paths) -> Result<(), ScenarioError> {
    hits::count(&paths.failpoint_dir(), AWAITING_SLOT).map_err(infra)
}

fn wait_file(path: &std::path::Path) -> Result<(), ScenarioError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!(
                "{} never appeared",
                path.display()
            )));
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn spawn_args(prompt: &str) -> [&str; 9] {
    [
        "spawn",
        "--harness",
        "fake",
        "--model",
        "fake",
        "--prompt",
        prompt,
        "--background",
        "--json",
    ]
}

fn json_line(bytes: &[u8]) -> Result<Value, ScenarioError> {
    serde_json::from_slice(bytes).map_err(infra)
}

/// Arms one point through the harness controller.
fn arm(paths: &Paths, point: &str, action: &str) -> Result<(), ScenarioError> {
    paths.failpoints.arm(point, 1, action).map_err(infra)
}

/// Waits for the daemon's entry acknowledgement and keeps it as evidence.
fn acknowledged(
    paths: &Paths,
    evidence: &Evidence,
    point: &str,
    action: &str,
    daemon: &Daemon<'_>,
) -> Result<(), ScenarioError> {
    paths
        .failpoints
        .wait_ack(point, 1, action, daemon.child.id(), ACK_WAIT)
        .map_err(|error| fail(&format!("failpoint {point}: {error}")))?;
    let bytes = paths.failpoints.ack_bytes(point, 1).map_err(infra)?;
    evidence
        .write(&format!("{point}.ack.json"), &bytes)
        .map_err(infra)
}

/// A turn that completes at once, used for post-recovery liveness and turn evidence.
fn reply_steps(prompt: &str) -> Value {
    json!({
        "expected_request":{"type":"start","id":1,"turn":1,"prompt":prompt},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"done","stop_reason":"end_turn"}}
        ]
    })
}

/// The fake reads the start (proving the prompt reached it) and holds at
/// gate `prompted` before accepting.
fn prompted_fixture() -> Value {
    json!({
        "expected_request":{"type":"start","id":1,"turn":1,"prompt":"f10"},
        "steps":[
            {"action":"gate","name":"prompted"},
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"done","stop_reason":"end_turn"}}
        ]
    })
}

/// The fake accepts at once, then holds at gate `accepted` before its terminal.
fn accepting_fixture() -> Value {
    json!({
        "expected_request":{"type":"start","id":1,"turn":1,"prompt":"f10"},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"gate","name":"accepted"},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"done","stop_reason":"end_turn"}}
        ]
    })
}

fn scenario(
    name: &str,
    fixture: &Value,
    action: impl FnOnce(&Paths, &Evidence) -> Result<(), ScenarioError>,
) -> TestResult {
    let paths = Paths::new(fixture)?;
    let evidence = Evidence::new(name, &paths.fake, &paths.fixture)?;
    run_scenario(
        evidence,
        |evidence| action(&paths, evidence),
        // Every collection step runs; their failures are reported together.
        |evidence| {
            let failures: Vec<String> = [
                paths.write_store_evidence(evidence),
                collect_available(evidence, &paths.state, &paths.teardown),
                paths.outputs_written(),
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

/// After recovery the Store is usable: a normal turn runs to `completed`.
fn completes_normally(paths: &Paths, evidence: &Evidence) -> Result<String, ScenarioError> {
    fs::write(
        &paths.fixture,
        serde_json::to_vec(&reply_steps("after")).map_err(infra)?,
    )
    .map_err(infra)?;
    let spawn = paths.run(evidence, "spawn-after", &spawn_args("after"))?;
    check(spawn.status.success(), || {
        format!("post-recovery spawn exited {}", spawn.status)
    })?;
    let receipt = json_line(&spawn.stdout)?;
    let address = receipt["turn"]
        .as_str()
        .ok_or_else(|| fail("post-recovery receipt has no turn"))?
        .to_owned();
    let wait = paths.run(evidence, "wait-after", &["wait", &address, "--json"])?;
    let envelope = json_line(&wait.stdout)?;
    check(envelope["state"] == "completed", || {
        format!("post-recovery turn ended {envelope}")
    })?;
    Ok(receipt["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

/// Store error kind of a refused request on stderr.
fn error_kind(stderr: &[u8]) -> Option<String> {
    let error: Value = serde_json::from_slice(stderr).ok()?;
    error["data"]["kind"].as_str().map(str::to_owned)
}

/// F8: a crash while a keyed `spawn`'s transaction holds every row but has
/// not committed leaves no session, turn, handle hash, event or spawn key,
/// before and after restart; the caller never received a receipt. After
/// restart the same key creates exactly one whole session, turn and launch.
#[test]
fn s1_f08_crash_inside_spawn_write_leaves_nothing() -> TestResult {
    scenario(
        "s1_f08_crash_inside_spawn_write",
        &f08_and_after(),
        |paths, evidence| {
            let point = "store.spawn.before_commit";
            let key = ["--idempotency-key", "f08-key"];
            arm(paths, point, "pause")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let client = paths.spawn_pending(evidence, "spawn-crashed", "f08", &key);
            acknowledged(paths, evidence, point, "pause", &daemon)?;
            let paused = paths.counts()?;
            check(paused == [0; 5], || {
                format!("uncommitted spawn visible while paused: {paused:?}")
            })?;
            daemon.kill()?;
            let (status, stdout, _) = client.finish()?;
            check(!status.success() && stdout.is_empty(), || {
                format!("crashed spawn client exited {status} with a receipt")
            })?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let recovered = paths.counts()?;
            check(recovered == [0; 5], || {
                format!("partial spawn survived the crash: {recovered:?}")
            })?;
            let mut retry = spawn_args("f08").to_vec();
            retry.extend(key);
            let mut command = paths.command();
            command.args(&retry).env("VIA_HANDLE", HANDLE);
            let capture = run_command(&mut command, Duration::from_secs(15)).map_err(infra)?;
            evidence
                .write("spawn-retried.stdout", &capture.stdout)
                .map_err(infra)?;
            let receipt = json_line(&capture.stdout)?;
            let session = paths.only_session()?;
            check(receipt["session_id"] == session.as_str(), || {
                format!("the keyed retry's receipt is not the session: {receipt}")
            })?;
            let envelope = t2c_wait(paths, evidence, &format!("{session}/1"))?;
            let counts = paths.counts()?;
            check(
                envelope["state"] == "completed"
                    && counts[..2] == [1, 1]
                    && counts[3..] == [1, 1]
                    && paths.anchors_for(&session)? == 1,
                || format!("the keyed retry is not one whole run: {envelope} {counts:?}"),
            )?;
            completes_normally(paths, evidence)?;
            check(paths.counts()?[0] == 2, || {
                "post-recovery Store holds more than the two new sessions".to_owned()
            })
        },
    )
}

/// F8: a crash right after `spawn`'s commit, before any reply, leaves session,
/// turn 1, handle hash and the queued event all durable together; after
/// restart the handle authenticates, and the unacknowledged turn is handed off
/// (T2-C, design §10) and runs exactly once.
#[test]
fn s1_f08_crash_after_spawn_commit_keeps_the_whole_session() -> TestResult {
    scenario(
        "s1_f08_crash_after_spawn_commit",
        &f08_and_after(),
        |paths, evidence| {
            let point = "store.spawn.after_commit";
            arm(paths, point, "crash")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let client = paths.spawn_pending(evidence, "spawn-crashed", "f08", &[]);
            acknowledged(paths, evidence, point, "crash", &daemon)?;
            daemon.wait_crash()?;
            let (status, stdout, _) = client.finish()?;
            check(!status.success() && stdout.is_empty(), || {
                format!("crashed spawn client exited {status} with a receipt")
            })?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            // Durable state as the crash left it, before any restart.
            check_whole_queued_session(paths)?;
            let session = paths.only_session()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let steer = |name: &str, handle: &str| {
                let mut command = paths.command();
                command
                    .args(["steer", &session, "--text", "x", "--json"])
                    .env("VIA_HANDLE", handle);
                let capture = run_command(&mut command, Duration::from_secs(15)).map_err(infra)?;
                evidence.write(name, &capture.stderr).map_err(infra)?;
                Ok::<_, ScenarioError>(error_kind(&capture.stderr))
            };
            // The committed hash authenticates the handle; fake then refuses steer.
            let right = steer("steer-right.stderr", HANDLE)?;
            let wrong = steer("steer-wrong.stderr", OTHER_HANDLE)?;
            check(
                right.as_deref() == Some("unsupported_verb")
                    && wrong.as_deref() == Some("invalid_handle"),
                || format!("handle hash not committed with the session: {right:?} {wrong:?}"),
            )?;
            runs_once_after_restart(paths, evidence, &session)?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// Fake scripts for the scenario's `f08` turn and the later `after` turn.
fn f08_and_after() -> Value {
    json!({"scripts":[t2c_script(1, "f08", None), t2c_script(1, "after", None)]})
}

/// T2-C (design §10): a committed turn whose receipt was never acknowledged
/// is handed off by the restarted daemon and runs exactly once.
fn runs_once_after_restart(
    paths: &Paths,
    evidence: &Evidence,
    session: &str,
) -> Result<(), ScenarioError> {
    let envelope = t2c_wait(paths, evidence, &format!("{session}/1"))?;
    let types = paths.event_types(session)?;
    let submissions = types
        .iter()
        .filter(|kind| *kind == "turn.submitted")
        .count();
    check(
        envelope["state"] == "completed" && submissions == 1 && paths.anchors_for(session)? == 1,
        || format!("the handed-off turn did not run exactly once: {envelope} {types:?}"),
    )
}

/// Exactly one session exists with its receipt, handle hash, queued turn 1
/// (prompt kept) and only `turn.queued` at seq 1, and no process was started.
fn check_whole_queued_session(paths: &Paths) -> Result<(), ScenarioError> {
    let counts = paths.counts()?;
    check(counts == [1, 1, 1, 0, 0], || {
        format!("spawn rows are not whole: {counts:?}")
    })?;
    let session = paths.only_session()?;
    let (hash, receipt, next_seq): (Vec<u8>, String, i64) = paths
        .store()?
        .query_row(
            "SELECT handle_hash,receipt,next_seq FROM sessions WHERE id=?1",
            [&session],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(infra)?;
    let receipt: Value = serde_json::from_str(&receipt).map_err(infra)?;
    check(
        hash.len() == 32 && receipt["session_id"] == session.as_str() && next_seq == 2,
        || format!("session row incomplete: receipt {receipt}, next_seq {next_seq}"),
    )?;
    let turn = paths.turn(&session)?;
    check(
        turn.state == "queued" && turn.prompt == "f08" && turn.submitted_at.is_none(),
        || format!("turn 1 is {} with prompt {:?}", turn.state, turn.prompt),
    )?;
    let types = paths.event_types(&session)?;
    check(types == ["turn.queued"], || format!("events {types:?}"))
}

/// F8 / `store.commit.reply_lost`: the spawn commits but its reply is lost.
/// The caller gets `store_error` with `commit_outcome: unknown` and
/// `retry: same_key_only`, never a receipt; one whole session exists. The
/// uncertain commit latches Store failure (runtime §7): the daemon shuts
/// itself down in force mode and exits 4 without dispatching the
/// unacknowledged turn. The restarted daemon hands it off (T2-C, design §10)
/// and it runs exactly once.
#[test]
fn s1_f08_lost_spawn_reply_leaves_one_whole_undispatched_session() -> TestResult {
    scenario(
        "s1_f08_lost_spawn_reply",
        &f08_and_after(),
        |paths, evidence| {
            let point = "store.commit.reply_lost";
            arm(paths, point, "fail_io")?;
            let mut daemon = Daemon::start(paths, evidence, "latched")?;
            let client = paths.spawn_pending(evidence, "spawn-lost", "f08", &[]);
            acknowledged(paths, evidence, point, "fail_io", &daemon)?;
            let (status, stdout, stderr) = client.finish()?;
            let error: Value = serde_json::from_slice(&stderr).unwrap_or_default();
            check(
                !status.success()
                    && stdout.is_empty()
                    && error["data"]
                        == json!({"kind":"store_error","commit_outcome":"unknown","retry":"same_key_only"}),
                || {
                    format!(
                        "lost reply returned {status}: {}",
                        String::from_utf8_lossy(&stderr)
                    )
                },
            )?;
            latched_exit(&mut daemon)?;
            check_whole_queued_session(paths)?;
            let session = paths.only_session()?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            runs_once_after_restart(paths, evidence, &session)?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// Waits for the restarted daemon's `result` and checks it is `unknown`.
fn restarted_unknown(
    paths: &Paths,
    evidence: &Evidence,
    session: &str,
) -> Result<Value, ScenarioError> {
    restarted_unknown_as(paths, evidence, session, true)
}

/// `restarted_unknown`, with cleanup expected `quiescent` or, when Host
/// could not prove absence for every anchor of the turn, `uncertain`.
fn restarted_unknown_as(
    paths: &Paths,
    evidence: &Evidence,
    session: &str,
    quiescent: bool,
) -> Result<Value, ScenarioError> {
    let address = format!("{session}/1");
    let result = paths.run(
        evidence,
        "result-restarted",
        &["result", &address, "--json"],
    )?;
    let envelope = json_line(&result.stdout).map_err(|_| {
        fail(&format!(
            "restarted result is not an envelope: {}",
            String::from_utf8_lossy(&result.stderr)
        ))
    })?;
    check(envelope["state"] == "unknown", || {
        format!("restarted turn is not unknown: {envelope}")
    })?;
    let types = paths.event_types(session)?;
    check(
        types.last().map(String::as_str) == Some("turn.ended"),
        || format!("recovery did not end the turn: {types:?}"),
    )?;
    check(
        types
            .iter()
            .filter(|kind| *kind == "turn.submitted")
            .count()
            == 1,
        || format!("turn was submitted again: {types:?}"),
    )?;
    restart_cleanup(paths, session, &envelope, &types, quiescent)?;
    let ended: String = paths
        .store()?
        .query_row(
            "SELECT event FROM events WHERE session_id=?1 ORDER BY seq DESC LIMIT 1",
            [session],
            |row| row.get(0),
        )
        .map_err(infra)?;
    let ended: Value = serde_json::from_str(&ended).map_err(infra)?;
    check(
        envelope["failure"]["class"] == "daemon_restart"
            && ended["failure"]["class"] == "daemon_restart"
            && ended["state"] == "unknown",
        || {
            format!(
                "recovered turn lacks daemon_restart: {} / {ended}",
                envelope["failure"]
            )
        },
    )?;
    Ok(envelope)
}

/// Starts a daemon that must refuse to come up. Its accept loop starts only
/// after recovery succeeds, so its exit proves no request was admitted.
fn refused_start(
    paths: &Paths,
    evidence: &Evidence,
    run: &str,
) -> Result<(ExitStatus, String), ScenarioError> {
    let trace = evidence.dir.join(format!("daemon-{run}.trace"));
    let mut command = paths.command();
    paths.failpoints.activate(&mut command);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(&trace).map_err(infra)?);
    let mut child = command.spawn().map_err(infra)?;
    let Some(status) = wait_child(&mut child, Duration::from_secs(15))? else {
        let reaped = outer_cleanup::kill_and_reap(&mut child, Instant::now() + outer_cleanup::REAP);
        return Err(fail(&format!(
            "daemon admitted requests after a failed reconciliation (killed, reaped in 1 s: {reaped})"
        )));
    };
    Ok((status, fs::read_to_string(&trace).map_err(infra)?))
}

/// C1 §7.5: before admission Host reconciled the crashed daemon's anchors and
/// the recovered turn keeps that result. Every anchor of the session carries
/// committed group-absence proof by the time `result` is served, so cleanup is
/// `quiescent`; with no anchor nothing could launch and the stop was only
/// `requested`.
fn restart_cleanup(
    paths: &Paths,
    session: &str,
    envelope: &Value,
    types: &[String],
    quiescent: bool,
) -> Result<(), ScenarioError> {
    check(
        types.ends_with(&[
            "cancel.requested".to_owned(),
            "cancel.settled".to_owned(),
            "turn.ended".to_owned(),
        ]),
        || format!("recovery recorded no cleanup settlement: {types:?}"),
    )?;
    let (anchors, unproven): (i64, i64) = paths
        .store()?
        .query_row(
            "SELECT count(*), count(*) - count(absence_time) FROM anchors WHERE owner_session=?1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(infra)?;
    let cancel = &envelope["cancel"];
    if !quiescent {
        let warned = envelope["warnings"].as_array().is_some_and(|warnings| {
            warnings
                .iter()
                .any(|w| w["code"] == "cancel_cleanup_uncertain")
        });
        return check(cancel["cleanup"] == "uncertain" && warned, || {
            format!("unproven anchors did not leave cleanup uncertain: {cancel}")
        });
    }
    let outcome_ok = if anchors == 0 {
        cancel["outcome"] == "requested"
    } else {
        cancel["outcome"] == "requested" || cancel["outcome"] == "forced"
    };
    check(
        unproven == 0 && cancel["cleanup"] == "quiescent" && outcome_ok,
        || {
            format!(
                "restart cleanup not reconciled ({anchors} anchors, {unproven} unproven): {cancel}"
            )
        },
    )
}

/// F10: the submission record commits before any agent I/O: paused right
/// after it, `turn.submitted` is durable and no anchor or agent exists. A
/// crash there restarts as `unknown` with nothing ever launched.
#[test]
fn s1_f10_submission_precedes_agent_io_and_restarts_unknown() -> TestResult {
    scenario(
        "s1_f10_submission_precedes_agent_io",
        &prompted_fixture(),
        |paths, evidence| {
            let point = "core.intent.after_commit";
            arm(paths, point, "pause")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            acknowledged(paths, evidence, point, "pause", &daemon)?;
            let turn = paths.turn(&session)?;
            let types = paths.event_types(&session)?;
            check(
                turn.state == "running"
                    && turn.submitted_at.is_some()
                    && types == ["turn.queued", "turn.submitted"],
                || format!("submission not durable at intent: {} {types:?}", turn.state),
            )?;
            check(
                paths.anchors_for(&session)? == 0 && !paths.sync.join("prompted.entered").exists(),
                || "agent I/O began before the submission record".to_owned(),
            )?;
            daemon.kill()?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            restarted_unknown(paths, evidence, &session)?;
            check(
                paths.anchors_for(&session)? == 0 && !paths.sync.join("prompted.entered").exists(),
                || "restart dispatched the unknown turn".to_owned(),
            )?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// F10: the prompt reached the agent (its gate proves it read the start) and
/// the daemon dies before acceptance is recorded. After restart the turn is
/// `unknown`, not re-dispatched: one anchor, one submission, no second start.
#[test]
fn s1_f10_crash_after_prompt_write_restarts_unknown_without_resend() -> TestResult {
    scenario(
        "s1_f10_crash_after_prompt_write",
        &prompted_fixture(),
        |paths, evidence| {
            let point = "wire.prompt.after_write";
            arm(paths, point, "pause")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            acknowledged(paths, evidence, point, "pause", &daemon)?;
            wait_file(&paths.sync.join("prompted.entered"))?;
            let turn = paths.turn(&session)?;
            check(
                turn.state == "running"
                    && turn.submitted_at.is_some()
                    && turn.accepted_at.is_none(),
                || format!("prompt written without a submission record: {}", turn.state),
            )?;
            daemon.kill()?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let envelope = restarted_unknown(paths, evidence, &session)?;
            check(envelope["vendor"]["turn_id"].is_null(), || {
                format!("unrecorded acceptance appeared: {envelope}")
            })?;
            let anchors = paths.anchors_for(&session)?;
            check(anchors == 1, || format!("{anchors} launches for one turn"))?;
            completes_normally(paths, evidence)?;
            check(paths.anchors_for(&session)? == 1, || {
                "restart dispatched the unknown turn".to_owned()
            })
        },
    )
}

/// F10: the agent accepted, and the daemon crashes before recording it.
/// After restart the turn is `unknown` with no acceptance, never re-sent.
#[test]
fn s1_f10_crash_before_acceptance_commit_restarts_unknown() -> TestResult {
    scenario(
        "s1_f10_crash_before_acceptance_commit",
        &accepting_fixture(),
        |paths, evidence| {
            let point = "core.accept.before_commit";
            arm(paths, point, "crash")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            acknowledged(paths, evidence, point, "crash", &daemon)?;
            daemon.wait_crash()?;
            let turn = paths.turn(&session)?;
            check(
                turn.state == "running"
                    && turn.accepted_at.is_none()
                    && turn.correlation.is_none()
                    && turn.envelope.is_none(),
                || format!("acceptance recorded despite the crash: {}", turn.state),
            )?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let envelope = restarted_unknown(paths, evidence, &session)?;
            check(envelope["timestamps"]["accepted_at"].is_null(), || {
                format!("unrecorded acceptance appeared: {envelope}")
            })?;
            completes_normally(paths, evidence)?;
            check(paths.anchors_for(&session)? == 1, || {
                "restart dispatched the unknown turn".to_owned()
            })
        },
    )
}

/// F10 ordering on the normal path: paused right after submission intent, no
/// agent I/O has happened; once released the same turn launches once and
/// completes.
#[test]
fn s1_f10_released_intent_pause_launches_once_and_completes() -> TestResult {
    scenario(
        "s1_f10_released_intent_pause",
        &reply_steps("f10"),
        |paths, evidence| {
            let point = "core.intent.after_commit";
            arm(paths, point, "pause")?;
            let daemon = Daemon::start(paths, evidence, "final")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let receipt = json_line(&spawn.stdout)?;
            let session = receipt["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            acknowledged(paths, evidence, point, "pause", &daemon)?;
            check(paths.anchors_for(&session)? == 0, || {
                "agent I/O began before the submission record".to_owned()
            })?;
            paths.failpoints.release(point, 1).map_err(infra)?;
            let address = format!("{session}/1");
            let wait = paths.run(evidence, "wait", &["wait", &address, "--json"])?;
            let envelope = json_line(&wait.stdout)?;
            check(envelope["state"] == "completed", || {
                format!("released turn ended {envelope}")
            })?;
            check(paths.anchors_for(&session)? == 1, || {
                "released turn did not launch exactly once".to_owned()
            })
        },
    )
}

/// A marker left by an earlier arming or another daemon never satisfies a
/// wait: arming clears it, and an acknowledgement from another pid is refused.
#[test]
fn failpoint_harness_rejects_stale_acknowledgements() -> TestResult {
    let root = tempfile::tempdir()?;
    let failpoints = Failpoints::new(root.path())?;
    let point = "store.spawn.before_commit";
    let dir = root.path().join("failpoints");
    let stale = json!({"action":"pause","occurrence":1,"pid":1,"point":point});
    fs::write(dir.join(format!("{point}.1.ack")), stale.to_string())?;
    fs::write(dir.join(format!("{point}.1.release")), b"")?;
    failpoints.arm(point, 1, "pause")?;
    if dir.join(format!("{point}.1.ack")).exists()
        || dir.join(format!("{point}.1.release")).exists()
    {
        return Err("arming left a stale marker".into());
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(format!("{point}.1.ack")))?;
    file.write_all(stale.to_string().as_bytes())?;
    match failpoints.wait_ack(
        point,
        1,
        "pause",
        std::process::id(),
        Duration::from_millis(50),
    ) {
        Err(error) if error.contains("unexpected acknowledgement") => Ok(()),
        other => Err(format!("another daemon's acknowledgement was accepted: {other:?}").into()),
    }
}

/// F10 / C1 §7.5: when Host cannot reconcile the committed anchors (here its
/// anchor inventory is unreadable), startup fails with a named error before
/// admission and commits no recovery; once the inventory is readable again
/// the turn recovers as `unknown`.
#[test]
fn s1_f10_failed_host_reconciliation_refuses_admission() -> TestResult {
    scenario(
        "s1_f10_failed_host_reconciliation",
        &prompted_fixture(),
        |paths, evidence| {
            let point = "wire.prompt.after_write";
            arm(paths, point, "pause")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            acknowledged(paths, evidence, point, "pause", &daemon)?;
            wait_file(&paths.sync.join("prompted.entered"))?;
            daemon.kill()?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let tamper = |phase: &str| {
                let store =
                    rusqlite::Connection::open(paths.state.join("store.sqlite3")).map_err(infra)?;
                store
                    .execute(
                        "UPDATE anchors SET phase=?2 WHERE owner_session=?1",
                        [session.as_str(), phase],
                    )
                    .map_err(infra)
            };
            let phase: String = paths
                .store()?
                .query_row(
                    "SELECT phase FROM anchors WHERE owner_session=?1",
                    [&session],
                    |row| row.get(0),
                )
                .map_err(infra)?;
            check(tamper("unreadable")? == 1, || {
                "no anchor to tamper".to_owned()
            })?;
            let (status, trace) = refused_start(paths, evidence, "refused")?;
            check(
                !status.success() && trace.contains("store_error: host reconciliation"),
                || format!("startup did not fail by name ({status}): {trace}"),
            )?;
            let turn = paths.turn(&session)?;
            let types = paths.event_types(&session)?;
            check(
                turn.state == "running"
                    && turn.envelope.is_none()
                    && types == ["turn.queued", "turn.submitted"],
                || {
                    format!(
                        "recovery committed without reconciliation: {} {types:?}",
                        turn.state
                    )
                },
            )?;
            tamper(&phase)?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            restarted_unknown(paths, evidence, &session)?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// [`anchors::insert_proven_absent`] into the scenario's Store.
fn insert_proven_absent(
    paths: &Paths,
    owner: &str,
    prefix: &str,
    count: u32,
) -> Result<(), ScenarioError> {
    anchors::insert_proven_absent(&paths.state.join("store.sqlite3"), owner, prefix, count)
        .map_err(infra)
}

/// [`anchors::delete_synthetic`] from the scenario's Store.
fn delete_synthetic(paths: &Paths, prefix: &str) -> Result<(), ScenarioError> {
    anchors::delete_synthetic(&paths.state.join("store.sqlite3"), prefix).map_err(infra)
}

/// Runtime §7 availability and bounded memory: more than 10,000 committed
/// anchors (here 10,001 synthetic proven-absent rows) never strand the
/// daemon. Startup recovery pages through the whole inventory, recovers the
/// crashed turn, admits requests, and a normal `daemon stop` then shuts down
/// cleanly (exit 0) with Host consuming the same inventory page by page.
#[test]
fn s1_f10_recovery_pages_past_ten_thousand_anchors_and_admits() -> TestResult {
    scenario(
        "s1_f10_recovery_pages_anchors",
        &prompted_fixture(),
        |paths, evidence| {
            let point = "wire.prompt.after_write";
            arm(paths, point, "pause")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            acknowledged(paths, evidence, point, "pause", &daemon)?;
            wait_file(&paths.sync.join("prompted.entered"))?;
            daemon.kill()?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            insert_proven_absent(paths, &session, "synthetic", 10_001)?;
            let mut daemon = Daemon::start(paths, evidence, "final")?;
            restarted_unknown(paths, evidence, &session)?;
            completes_normally(paths, evidence)?;
            let stop = paths.run(evidence, "stop", &["daemon", "stop", "--json"])?;
            check(stop.status.success(), || {
                format!("stop exited {}", stop.status)
            })?;
            let exit = wait_child(&mut daemon.child, FINAL_SHUTDOWN + Duration::from_secs(2))?;
            check(exit.is_some_and(|status| status.success()), || {
                format!("final shutdown after the load was not clean: {exit:?}")
            })?;
            delete_synthetic(paths, "synthetic")
        },
    )
}

/// Decision 1 (runtime §7, C1 §7.5): the 5 s reconciliation deadline bounds
/// startup. Two pages of anchors belong to an ended turn; the crashed turn,
/// which never launched, has none. Reconciliation is held at the page
/// boundary past the deadline; on release Core stops paging, so it can no
/// longer know the crashed turn has no anchor: it recovers `unknown` with
/// cleanup `uncertain`, commits, and the daemon admits requests.
#[test]
fn s1_f10_reconciliation_deadline_settles_uncertain_and_admits() -> TestResult {
    scenario(
        "s1_f10_reconciliation_deadline",
        &reply_steps("after"),
        |paths, evidence| {
            let intent = "core.intent.after_commit";
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let ended = completes_normally(paths, evidence)?;
            // The completed turn was hit 1; the crashed turn is hit 2.
            paths.failpoints.arm(intent, 2, "pause").map_err(infra)?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("after"))?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            paths
                .failpoints
                .wait_ack(intent, 2, "pause", daemon.child.id(), ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {intent}: {error}")))?;
            daemon.kill()?;
            paths.failpoints.disarm(intent).map_err(infra)?;
            daemon.shutdown()?;
            check(paths.anchors_for(&session)? == 0, || {
                "the crashed turn launched".to_owned()
            })?;
            // Ids sort before the real anchor's hex id: two full pages.
            insert_proven_absent(paths, &ended, "0-synthetic", 300)?;
            let boundary = "core.recovery.page_boundary";
            arm(paths, boundary, "pause")?;
            let mut daemon = Daemon::spawn(paths, evidence, "final")?;
            acknowledged(paths, evidence, boundary, "pause", &daemon)?;
            // The deadline began before the acknowledgement: 5 s after it has passed.
            thread::sleep(Duration::from_millis(5_200));
            paths.failpoints.release(boundary, 1).map_err(infra)?;
            daemon.wait_ready()?;
            let envelope = restarted_unknown_as(paths, evidence, &session, false)?;
            check(envelope["cancel"]["outcome"] == "requested", || {
                format!("unexpected outcome: {}", envelope["cancel"])
            })?;
            completes_normally(paths, evidence)?;
            let stop = paths.run(evidence, "stop", &["daemon", "stop", "--json"])?;
            check(stop.status.success(), || {
                format!("stop exited {}", stop.status)
            })?;
            wait_child(&mut daemon.child, FINAL_SHUTDOWN + Duration::from_secs(2))?;
            delete_synthetic(paths, "0-synthetic")
        },
    )
}

/// Decision 2 (runtime §7): a failed absence-proof commit during startup
/// recovery is a Store failure. The daemon exits with the Store startup
/// failure before admission and commits no recovery; without the fault a
/// restart recovers normally.
#[test]
fn s1_f10_failed_absence_commit_fails_startup() -> TestResult {
    scenario(
        "s1_f10_failed_absence_commit",
        &prompted_fixture(),
        |paths, evidence| {
            let point = "wire.prompt.after_write";
            arm(paths, point, "pause")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            acknowledged(paths, evidence, point, "pause", &daemon)?;
            wait_file(&paths.sync.join("prompted.entered"))?;
            daemon.kill()?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let commit = "host.recovery.absence_commit";
            arm(paths, commit, "fail_io")?;
            let (status, trace) = refused_start(paths, evidence, "refused")?;
            check(
                !status.success()
                    && trace.contains("store_error: host reconciliation")
                    && trace.contains("group absence commit failed"),
                || format!("startup did not fail with the Store failure ({status}): {trace}"),
            )?;
            let turn = paths.turn(&session)?;
            check(turn.state == "running" && turn.envelope.is_none(), || {
                format!("recovery committed after a Store failure: {}", turn.state)
            })?;
            paths.failpoints.disarm(commit).map_err(infra)?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            restarted_unknown(paths, evidence, &session)?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// Waits for a daemon that latched Store failure to end its own final
/// shutdown with exit 4, and returns its shutdown summary.
fn latched_exit(daemon: &mut Daemon<'_>) -> Result<Value, ScenarioError> {
    let status = wait_child(&mut daemon.child, FINAL_SHUTDOWN + Duration::from_secs(2))?
        .ok_or_else(|| ScenarioError::Timeout("the latched daemon never exited".to_owned()))?;
    // Task 4 design §7.6: the summary is in `via.log`.
    let log = fs::read_to_string(daemon.paths.state.join("via.log")).map_err(infra)?;
    let summary = log
        .lines()
        .rev()
        .find_map(|line| {
            let line: Value = serde_json::from_str(line).ok()?;
            line.get("daemon_shutdown").cloned()
        })
        .ok_or_else(|| fail("no daemon_shutdown summary"))?;
    check(
        status.code() == Some(4)
            && summary["store_failed"] == true
            && summary["mode"] == "force"
            && summary["disposition"] == "incomplete",
        || format!("latched daemon ended {status}; summary {summary}"),
    )?;
    Ok(summary)
}

/// Runtime §7 (T2-B2): a submission commit whose reply is lost after the
/// grant latches Store failure. No vendor launches, the turn is not retried,
/// and the daemon exits 4; the restarted daemon recovers it `unknown`.
#[test]
fn s1_f10_uncertain_submission_latches_and_launches_nothing() -> TestResult {
    scenario(
        "s1_f10_uncertain_submission_latches",
        &prompted_fixture(),
        |paths, evidence| {
            // The spawn's receipt is the first Core lifecycle commit; the
            // submission is the second.
            let point = "store.commit.reply_lost";
            paths.failpoints.arm(point, 2, "fail_io").map_err(infra)?;
            let mut daemon = Daemon::start(paths, evidence, "latched")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            paths
                .failpoints
                .wait_ack(point, 2, "fail_io", daemon.child.id(), ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {point}: {error}")))?;
            latched_exit(&mut daemon)?;
            let turn = paths.turn(&session)?;
            let types = paths.event_types(&session)?;
            check(
                turn.state == "running"
                    && turn.submitted_at.is_some()
                    && types == ["turn.queued", "turn.submitted"],
                || format!("turn {} with events {types:?}", turn.state),
            )?;
            check(
                paths.anchors_for(&session)? == 0 && !paths.sync.join("prompted.entered").exists(),
                || "a vendor launched after the uncertain submission".to_owned(),
            )?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            restarted_unknown(paths, evidence, &session)?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// Committed anchors of a session whose vendor was spawned (vendor facts).
fn vendor_launches(paths: &Paths, session: &str) -> Result<i64, ScenarioError> {
    paths
        .store()?
        .query_row(
            "SELECT count(*) FROM anchors WHERE owner_session=?1 AND vendor_pid IS NOT NULL",
            [session],
            |row| row.get(0),
        )
        .map_err(infra)
}

/// T2-B2 design §3.1: a force accepted while the acquisition waits at the
/// pre-ARM gate (after `ArmIntent` committed) wins: no ARM is sent, no vendor
/// launches, and the turn ends as a force before launch (`cancelled`,
/// `requested`); the session closes and the shutdown is clean.
#[test]
fn s1_f10_force_at_the_pre_arm_gate_launches_nothing() -> TestResult {
    scenario(
        "s1_f10_force_at_pre_arm_gate",
        &prompted_fixture(),
        |paths, evidence| {
            let point = "host.anchor.after_arm_intent_commit";
            arm(paths, point, "pause")?;
            let mut daemon = Daemon::start(paths, evidence, "forced")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            acknowledged(paths, evidence, point, "pause", &daemon)?;
            let stop = paths.run(evidence, "stop", &["daemon", "stop", "--force", "--json"])?;
            check(stop.status.success(), || {
                format!("force stop exited {}", stop.status)
            })?;
            paths.failpoints.release(point, 1).map_err(infra)?;
            let status = wait_child(&mut daemon.child, FINAL_SHUTDOWN + Duration::from_secs(2))?
                .ok_or_else(|| ScenarioError::Timeout("force stop never exited".to_owned()))?;
            check_pre_launch_force(paths, &session, true)?;
            check(status.code() == Some(0), || format!("daemon exit {status}"))?;
            // A later daemon runs a normal turn, which also leaves turn evidence.
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// T2-B2 design §3.1 with the Store-failed latch: while one turn waits at the
/// pre-ARM gate, another session's lost receipt reply latches; on release the
/// gate refuses ARM, nothing launches, no `session.closed` commits, and the
/// daemon exits 4.
#[test]
fn s1_f10_latch_at_the_pre_arm_gate_launches_nothing() -> TestResult {
    scenario(
        "s1_f10_latch_at_pre_arm_gate",
        &prompted_fixture(),
        |paths, evidence| {
            let gate = "host.anchor.after_arm_intent_commit";
            arm(paths, gate, "pause")?;
            // Spawn, then its submission; the second spawn's receipt is third.
            let lost = "store.commit.reply_lost";
            paths.failpoints.arm(lost, 3, "fail_io").map_err(infra)?;
            let mut daemon = Daemon::start(paths, evidence, "latched")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            acknowledged(paths, evidence, gate, "pause", &daemon)?;
            let second = paths.run(evidence, "spawn-lost", &spawn_args("f10"))?;
            check(
                !second.status.success()
                    && error_kind(&second.stderr).as_deref() == Some("store_error"),
                || "the lost receipt was not store_error".to_owned(),
            )?;
            paths.failpoints.release(gate, 1).map_err(infra)?;
            latched_exit(&mut daemon)?;
            check_pre_launch_force(paths, &session, false)?;
            paths.failpoints.disarm(gate).map_err(infra)?;
            paths.failpoints.disarm(lost).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// The turn ended as a force before launch: submitted, `cancelled` with
/// `requested`, no vendor spawned and no prompt read; `session.closed`
/// follows only when `closed`.
fn check_pre_launch_force(paths: &Paths, session: &str, closed: bool) -> Result<(), ScenarioError> {
    check(
        vendor_launches(paths, session)? == 0 && !paths.sync.join("prompted.entered").exists(),
        || "a vendor launched past the pre-ARM gate".to_owned(),
    )?;
    let turn = paths.turn(session)?;
    let envelope: Value =
        serde_json::from_str(turn.envelope.as_deref().unwrap_or("null")).map_err(infra)?;
    check(
        envelope["state"] == "cancelled"
            && envelope["cancel"]["outcome"] == "requested"
            && envelope["timestamps"]["accepted_at"].is_null(),
        || format!("envelope {envelope}"),
    )?;
    let mut expected = vec![
        "turn.queued",
        "turn.submitted",
        "cancel.requested",
        "cancel.settled",
        "turn.ended",
    ];
    if closed {
        expected.push("session.closed");
    }
    let types = paths.event_types(session)?;
    check(types == expected, || format!("events {types:?}"))
}

/// Round 1, decision 1 (runtime §7): the terminal commit succeeds but its
/// reply is lost. The read-back finds the terminal, so a waiter gets the
/// committed envelope; the uncertain commit itself still latches Store
/// failure, and the daemon exits 4.
#[test]
fn s1_f12_lost_terminal_reply_returns_the_envelope_and_latches() -> TestResult {
    scenario(
        "s1_f12_lost_terminal_reply",
        &accepting_fixture(),
        |paths, evidence| {
            // Spawn, submission and acceptance are the first three Core
            // lifecycle commits; the terminal is the fourth.
            let point = "store.commit.reply_lost";
            paths.failpoints.arm(point, 4, "fail_io").map_err(infra)?;
            let mut daemon = Daemon::start(paths, evidence, "latched")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            let receipt = json_line(&spawn.stdout)?;
            let address = receipt["turn"]
                .as_str()
                .ok_or_else(|| fail("receipt has no turn"))?
                .to_owned();
            wait_file(&paths.sync.join("accepted.entered"))?;
            let registered = "core.wait.registered";
            hits::count(&paths.failpoint_dir(), registered).map_err(infra)?;
            let out = evidence.dir.join("wait.stdout");
            let mut waiter = paths.command();
            waiter
                .args(["wait", &address, "--json"])
                .stdin(Stdio::null())
                .stdout(File::create(&out).map_err(infra)?)
                .stderr(File::create(evidence.dir.join("wait.stderr")).map_err(infra)?);
            let mut waiter = waiter.spawn().map_err(infra)?;
            // The waiter is attached before the terminal commits.
            wait_hits(paths, registered, 1)?;
            fs::write(paths.sync.join("accepted.release"), b"").map_err(infra)?;
            paths
                .failpoints
                .wait_ack(point, 4, "fail_io", daemon.child.id(), ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {point}: {error}")))?;
            let exit = wait_child(&mut waiter, FINAL_SHUTDOWN)?
                .ok_or_else(|| ScenarioError::Timeout("the waiter never returned".to_owned()))?;
            let envelope = json_line(&fs::read(&out).map_err(infra)?)?;
            check(exit.success() && envelope["state"] == "completed", || {
                format!("waiter ended {exit} with {envelope}")
            })?;
            latched_exit(&mut daemon)?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// Round 2, decision 3: under force, the queued turn's cancellation read is
/// paused at `core.force.cancel_read` and never released. The read expires at
/// final shutdown's read cutoff (4 s before its 10 s deadline), the turn stays
/// unresolved, and the daemon finishes shutdown within its bound with exit 4.
#[test]
fn s1_f12_stalled_force_path_read_expires_within_the_shutdown_bound() -> TestResult {
    scenario(
        "s1_f12_stalled_force_read",
        &accepting_fixture(),
        |paths, evidence| {
            let point = "core.force.cancel_read";
            arm(paths, point, "pause")?;
            let mut daemon = Daemon::start(paths, evidence, "stalled")?;
            let mut args = spawn_args("f10").to_vec();
            args.extend(["--handle", HANDLE]);
            let spawn = paths.run(evidence, "spawn", &args)?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            wait_file(&paths.sync.join("accepted.entered"))?;
            let resume = paths.run(
                evidence,
                "resume",
                &[
                    "resume", &session, "--prompt", "q", "--handle", HANDLE, "--json",
                ],
            )?;
            check(resume.status.success(), || {
                format!("resume exited {}", resume.status)
            })?;
            let stop = paths.run(evidence, "stop", &["daemon", "stop", "--force", "--json"])?;
            check(stop.status.success(), || {
                format!("force stop exited {}", stop.status)
            })?;
            let stopped = Instant::now();
            acknowledged(paths, evidence, point, "pause", &daemon)?;
            let status = wait_child(&mut daemon.child, FINAL_SHUTDOWN + Duration::from_secs(2))?
                .ok_or_else(|| ScenarioError::Timeout("shutdown outlived its bound".to_owned()))?;
            let elapsed = stopped.elapsed();
            // Task 4 design §7.6: the summary is in `via.log`.
            let trace = fs::read_to_string(paths.state.join("via.log")).map_err(infra)?;
            check(
                status.code() == Some(4)
                    && elapsed < FINAL_SHUTDOWN + Duration::from_secs(1)
                    && trace.contains("\"unresolved_turns\":1"),
                || format!("shutdown ended {status} after {elapsed:?}; trace {trace}"),
            )?;
            let types = paths.event_types(&session)?;
            check(!types.iter().any(|kind| kind == "session.closed"), || {
                format!("events {types:?}")
            })?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// Round 2, item 4: startup recovery's terminal commit loses its reply. The
/// commit was uncertain, so startup fails before admission even though the
/// terminal is durable; the next restart reads that durable `unknown` result,
/// recovers nothing twice and admits.
#[test]
fn s1_f10_lost_recovery_terminal_reply_fails_startup_then_admits() -> TestResult {
    scenario(
        "s1_f10_lost_recovery_terminal_reply",
        &prompted_fixture(),
        |paths, evidence| {
            let intent = "core.intent.after_commit";
            arm(paths, intent, "pause")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let spawn = paths.run(evidence, "spawn", &spawn_args("f10"))?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            acknowledged(paths, evidence, intent, "pause", &daemon)?;
            daemon.kill()?;
            paths.failpoints.disarm(intent).map_err(infra)?;
            daemon.shutdown()?;
            // Recovery commits `cancel.requested` and `cancel.settled`, then the
            // terminal: the third Core lifecycle commit of the new daemon.
            let lost = "store.commit.reply_lost";
            paths.failpoints.arm(lost, 3, "fail_io").map_err(infra)?;
            let (status, trace) = refused_start(paths, evidence, "refused")?;
            check(
                !status.success() && trace.contains("crash recovery failed"),
                || format!("startup ended {status}: {trace}"),
            )?;
            let turn = paths.turn(&session)?;
            check(turn.state == "unknown" && turn.envelope.is_some(), || {
                format!("the recovered terminal is not durable: {}", turn.state)
            })?;
            paths.failpoints.disarm(lost).map_err(infra)?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            restarted_unknown(paths, evidence, &session)?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// One turn's fake script: acceptance, an optional gate, then completion.
fn t2c_script(turn: u32, prompt: &str, gate: Option<&str>) -> Value {
    let vendor = format!("fake-turn-{turn}");
    let mut steps =
        vec![json!({"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":vendor}})];
    if let Some(name) = gate {
        steps.push(json!({"action":"gate","name":name}));
    }
    steps.push(json!({"action":"emit","message":{"type":"terminal","vendor_turn_id":vendor,"status":"completed","final_text":"done","stop_reason":"end_turn"}}));
    json!({"expected_request":{"type":"start","id":1,"turn":turn,"prompt":prompt},"steps":steps})
}

/// A session's turn `n`: state and envelope.
fn turn_n(paths: &Paths, session: &str, n: u32) -> Result<(String, Value), ScenarioError> {
    let (state, envelope): (String, Option<String>) = paths
        .store()?
        .query_row(
            "SELECT state,envelope FROM turns WHERE session_id=?1 AND number=?2",
            rusqlite::params![session, n],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(infra)?;
    let envelope = serde_json::from_str(envelope.as_deref().unwrap_or("null")).map_err(infra)?;
    Ok((state, envelope))
}

/// Committed anchors (launch attempts) of a session's turn `n`.
fn anchors_of_turn(paths: &Paths, session: &str, n: u32) -> Result<i64, ScenarioError> {
    paths
        .store()?
        .query_row(
            "SELECT count(*) FROM anchors WHERE owner_session=?1 AND owner_turn=?2",
            rusqlite::params![session, n],
            |row| row.get(0),
        )
        .map_err(infra)
}

/// `via spawn` with an explicit handle, and optionally an idempotency key.
fn t2c_spawn(
    paths: &Paths,
    evidence: &Evidence,
    name: &str,
    prompt: &str,
    key: Option<&str>,
) -> Result<Captured, ScenarioError> {
    let mut args = spawn_args(prompt).to_vec();
    args.extend(["--handle", HANDLE]);
    if let Some(key) = key {
        args.extend(["--idempotency-key", key]);
    }
    paths.run(evidence, name, &args)
}

/// `via resume` with the scenario handle.
fn t2c_resume(
    paths: &Paths,
    evidence: &Evidence,
    name: &str,
    session: &str,
    prompt: &str,
) -> Result<Captured, ScenarioError> {
    paths.run(
        evidence,
        name,
        &[
            "resume", session, "--prompt", prompt, "--handle", HANDLE, "--json",
        ],
    )
}

/// Polls `result` every 20 ms until `address` is terminal, so the test
/// looks before the re-probe loop's first pass, one second after the turn
/// ends. (Written when `wait` checked once per second; `wait` now returns as
/// soon as the terminal commits, C1 §3.8, and the poll is kept as is.)
fn result_when_done(
    paths: &Paths,
    evidence: &Evidence,
    address: &str,
) -> Result<Value, ScenarioError> {
    let name = format!("result-{}", address.replace('/', "-"));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let result = paths.run(evidence, &name, &["result", address, "--json"])?;
        if result.status.success() {
            return json_line(&result.stdout);
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!("{address} never ended")));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Waits for a turn's durable envelope through `via wait`.
fn t2c_wait(paths: &Paths, evidence: &Evidence, address: &str) -> Result<Value, ScenarioError> {
    let name = format!("wait-{}", address.replace('/', "-"));
    let wait = paths.run(evidence, &name, &["wait", address, "--json"])?;
    json_line(&wait.stdout)
}

/// T2-C 1 (design §10): a crash with turn 1 running and turn 2 queued behind
/// it. On restart turn 1 is `unknown` with `daemon_restart`, and the handoff
/// cancels turn 2 before admission (C1 §7.5, P6). Neither is launched again.
/// On `rust-foundation` before T2-C, turn 2 stayed `queued` with no owner.
#[test]
fn s1_t2c_crash_with_a_queued_successor_cancels_it_on_restart() -> TestResult {
    scenario(
        "s1_t2c_queued_successor_cancelled",
        &json!({"scripts":[t2c_script(1, "c1", Some("hold")), t2c_script(2, "c2", None)]}),
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let spawn = t2c_spawn(paths, evidence, "spawn", "c1", None)?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            wait_file(&paths.sync.join("hold.entered"))?;
            let resume = t2c_resume(paths, evidence, "resume", &session, "c2")?;
            check(resume.status.success(), || "resume refused".to_owned())?;
            daemon.kill()?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let (state, first) = turn_n(paths, &session, 1)?;
            check(
                state == "unknown" && first["failure"]["class"] == "daemon_restart",
                || format!("turn 1: {state} {first}"),
            )?;
            // T4-5 review round 1: the recovered envelope reports the
            // session's frozen `cwd`, as `status` does.
            let frozen: Option<String> = paths
                .store()?
                .query_row(
                    "SELECT json_extract(params,'$.cwd') FROM sessions WHERE id=?1",
                    [session.as_str()],
                    |row| row.get(0),
                )
                .map_err(infra)?;
            check(
                frozen.is_some() && first["cwd"].as_str() == frozen.as_deref(),
                || format!("turn 1 cwd {} against the frozen {frozen:?}", first["cwd"]),
            )?;
            let (state, second) = turn_n(paths, &session, 2)?;
            check(
                state == "cancelled"
                    && second["state"] == "cancelled"
                    && second["timestamps"]["submitted_at"].is_null(),
                || format!("turn 2 was not cancelled before admission: {state} {second}"),
            )?;
            check(
                anchors_of_turn(paths, &session, 1)? == 1
                    && anchors_of_turn(paths, &session, 2)? == 0,
                || "a turn launched after the restart".to_owned(),
            )?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// T2-C 2 (design §10): turn 1's terminal committed, and the daemon crashed
/// while turn 2 was decided `Run` but not yet granted
/// (`core.dispatch.before_grant`), leaving it durably `queued`. On restart
/// the handoff enqueues turn 2, which is dispatched and completes. On
/// `rust-foundation` before T2-C, turn 2 stayed `queued` forever.
#[test]
fn s1_t2c_queued_successor_after_a_committed_terminal_runs_on_restart() -> TestResult {
    scenario(
        "s1_t2c_queued_successor_runs",
        &json!({"scripts":[t2c_script(1, "r1", Some("hold")), t2c_script(2, "r2", None)]}),
        |paths, evidence| {
            let point = "core.dispatch.before_grant";
            paths.failpoints.arm(point, 2, "pause").map_err(infra)?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let spawn = t2c_spawn(paths, evidence, "spawn", "r1", None)?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            wait_file(&paths.sync.join("hold.entered"))?;
            let resume = t2c_resume(paths, evidence, "resume", &session, "r2")?;
            check(resume.status.success(), || "resume refused".to_owned())?;
            fs::write(paths.sync.join("hold.release"), b"").map_err(infra)?;
            paths
                .failpoints
                .wait_ack(point, 2, "pause", daemon.child.id(), ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {point}: {error}")))?;
            let (first, _) = turn_n(paths, &session, 1)?;
            let (second, _) = turn_n(paths, &session, 2)?;
            check(first == "completed" && second == "queued", || {
                format!("before the crash: turn 1 {first}, turn 2 {second}")
            })?;
            daemon.kill()?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let envelope = t2c_wait(paths, evidence, &format!("{session}/2"))?;
            check(envelope["state"] == "completed", || {
                format!("turn 2 after restart: {envelope}")
            })?;
            check(anchors_of_turn(paths, &session, 2)? == 1, || {
                "turn 2 did not launch exactly once".to_owned()
            })
        },
    )
}

/// T2-C 3 (design §10): a keyed spawn's receipt reply is lost. The caller gets
/// `store_error` with `commit_outcome: unknown` and `retry: same_key_only`,
/// and the daemon exits 4. After a restart the same keyed request returns
/// the same session and turn, and the turn runs exactly once. On
/// `rust-foundation` before T2-C the replay succeeded but the turn never ran.
#[test]
fn s1_t2c_keyed_receipt_replay_after_restart_runs_once() -> TestResult {
    scenario(
        "s1_t2c_keyed_replay_after_restart",
        &json!({"scripts":[t2c_script(1, "k1", None)]}),
        |paths, evidence| {
            let point = "store.commit.reply_lost";
            arm(paths, point, "fail_io")?;
            let mut daemon = Daemon::start(paths, evidence, "latched")?;
            let lost = t2c_spawn(paths, evidence, "spawn-lost", "k1", Some("key-1"))?;
            let error: Value = serde_json::from_slice(&lost.stderr).unwrap_or_default();
            check(
                !lost.status.success()
                    && error["data"]
                        == json!({"kind":"store_error","commit_outcome":"unknown","retry":"same_key_only"}),
                || format!("lost receipt: {error}"),
            )?;
            latched_exit(&mut daemon)?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let session = paths.only_session()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let replay = t2c_spawn(paths, evidence, "spawn-replay", "k1", Some("key-1"))?;
            let receipt = json_line(&replay.stdout)?;
            check(
                receipt["session_id"] == session.as_str()
                    && receipt["turn"] == format!("{session}/1"),
                || format!("replay returned {receipt}"),
            )?;
            let envelope = t2c_wait(paths, evidence, &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || {
                format!("the replayed turn: {envelope}")
            })?;
            let again = t2c_spawn(paths, evidence, "spawn-replay-2", "k1", Some("key-1"))?;
            check(json_line(&again.stdout)? == receipt, || {
                "a second replay differs".to_owned()
            })?;
            check(
                paths.anchors_for(&session)? == 1 && paths.counts()?[1] == 1,
                || "the keyed turn did not run exactly once".to_owned(),
            )
        },
    )
}

/// T2-C 4 (design §10): an unkeyed resume's receipt reply is lost (the fifth
/// Core lifecycle commit, after turn 1's receipt, submission, acceptance
/// and terminal); the daemon exits 4. After a restart the committed queued
/// turn 2 runs exactly once. On `rust-foundation` before T2-C it stayed
/// `queued`.
#[test]
fn s1_t2c_unkeyed_lost_resume_receipt_runs_once_after_restart() -> TestResult {
    scenario(
        "s1_t2c_unkeyed_lost_resume",
        &json!({"scripts":[t2c_script(1, "u1", None), t2c_script(2, "u2", None)]}),
        |paths, evidence| {
            let point = "store.commit.reply_lost";
            paths.failpoints.arm(point, 5, "fail_io").map_err(infra)?;
            let mut daemon = Daemon::start(paths, evidence, "latched")?;
            let spawn = t2c_spawn(paths, evidence, "spawn", "u1", None)?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            let first = t2c_wait(paths, evidence, &format!("{session}/1"))?;
            check(first["state"] == "completed", || format!("turn 1: {first}"))?;
            let lost = t2c_resume(paths, evidence, "resume-lost", &session, "u2")?;
            let error: Value = serde_json::from_slice(&lost.stderr).unwrap_or_default();
            check(
                !lost.status.success() && error["data"]["commit_outcome"] == "unknown",
                || format!("lost resume receipt: {error}"),
            )?;
            latched_exit(&mut daemon)?;
            let (state, _) = turn_n(paths, &session, 2)?;
            check(state == "queued", || {
                format!("turn 2 before restart: {state}")
            })?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let second = t2c_wait(paths, evidence, &format!("{session}/2"))?;
            check(second["state"] == "completed", || {
                format!("turn 2: {second}")
            })?;
            check(anchors_of_turn(paths, &session, 2)? == 1, || {
                "turn 2 did not launch exactly once".to_owned()
            })
        },
    )
}

/// T2-C round 1 (design §10.3): the handoff's `queued → cancelled` commit
/// loses its reply. The commit is uncertain although its terminal is
/// durable, so the handoff fails startup. The next restart finds that turn
/// `cancelled` and admits.
#[test]
fn s1_t2c_lost_handoff_cancellation_reply_fails_startup_then_admits() -> TestResult {
    scenario(
        "s1_t2c_lost_handoff_cancellation",
        &json!({"scripts":[t2c_script(1, "c1", Some("hold")), t2c_script(2, "c2", None)]}),
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let spawn = t2c_spawn(paths, evidence, "spawn", "c1", None)?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            wait_file(&paths.sync.join("hold.entered"))?;
            let resume = t2c_resume(paths, evidence, "resume", &session, "c2")?;
            check(resume.status.success(), || "resume refused".to_owned())?;
            daemon.kill()?;
            daemon.shutdown()?;
            // Recovery commits turn 1's `cancel.requested`,
            // `cancel.settled` and terminal; the handoff's cancellation of
            // turn 2 is fourth.
            let lost = "store.commit.reply_lost";
            paths.failpoints.arm(lost, 4, "fail_io").map_err(infra)?;
            let (status, trace) = refused_start(paths, evidence, "refused")?;
            check(
                !status.success() && trace.contains("restart handoff failed"),
                || format!("startup ended {status}: {trace}"),
            )?;
            let (state, _) = turn_n(paths, &session, 2)?;
            check(state == "cancelled", || {
                format!("the uncertain cancellation is not durable: {state}")
            })?;
            paths.failpoints.disarm(lost).map_err(infra)?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let (first, _) = turn_n(paths, &session, 1)?;
            let (second, _) = turn_n(paths, &session, 2)?;
            check(first == "unknown" && second == "cancelled", || {
                format!("after restart: turn 1 {first}, turn 2 {second}")
            })?;
            check(anchors_of_turn(paths, &session, 2)? == 0, || {
                "turn 2 launched".to_owned()
            })?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// Six sessions whose turn 1 (prompt `s<i>`) holds at its own gate
/// `hold<i>`, for T2-D: a fake gate admits one agent.
fn six_held() -> Value {
    let scripts: Vec<Value> = (0..6)
        .map(|index| t2c_script(1, &format!("s{index}"), Some(&format!("hold{index}"))))
        .collect();
    json!({"scripts":scripts})
}

/// Releases every T2-D gate.
fn release_six(paths: &Paths) -> Result<(), ScenarioError> {
    for index in 0..6 {
        fs::write(paths.sync.join(format!("hold{index}.release")), b"").map_err(infra)?;
    }
    Ok(())
}

/// Spawns six sessions and waits until four turns are accepted and holding
/// and the other two wait, registered, for a connection slot.
fn spawn_six_held(paths: &Paths, evidence: &Evidence) -> Result<Vec<String>, ScenarioError> {
    count_waiters(paths)?;
    let mut sessions = Vec::new();
    for index in 0..6 {
        let prompt = format!("s{index}");
        let spawn = t2c_spawn(paths, evidence, &format!("spawn-{index}"), &prompt, None)?;
        sessions.push(
            json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while store_count(
        paths,
        "SELECT count(*) FROM turns WHERE accepted_at IS NOT NULL",
    )? < 4
    {
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(
                "four turns never started".to_owned(),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Both other reservations are pending in the slot queue: neither
    // launches until a slot frees.
    wait_hits(paths, AWAITING_SLOT, 2)?;
    Ok(sessions)
}

fn store_count(paths: &Paths, query: &str) -> Result<i64, ScenarioError> {
    paths
        .store()?
        .query_row(query, [], |row| row.get(0))
        .map_err(infra)
}

/// Four slots in use, two turns waiting for one: exactly four anchors, and
/// the other two turns `queued` with no `submitted_at` and no anchor.
fn check_four_running_two_waiting(paths: &Paths) -> Result<(), ScenarioError> {
    let anchors = store_count(paths, "SELECT count(*) FROM anchors")?;
    let waiting = store_count(
        paths,
        "SELECT count(*) FROM turns WHERE state='queued' AND submitted_at IS NULL",
    )?;
    let running = store_count(paths, "SELECT count(*) FROM turns WHERE state='running'")?;
    check(anchors == 4 && waiting == 2 && running == 4, || {
        format!("anchors {anchors}, waiting {waiting}, running {running}")
    })
}

/// T2-D 1 (design §11, runtime §8): six held turns and four connection slots.
/// Exactly four anchors exist at once; the other two turns stay `queued` with
/// no `submitted_at` and no anchor until a slot frees, and then all six
/// complete. Before T2-D all six launched at once.
#[test]
fn s1_t2d_six_turns_share_four_connection_slots() -> TestResult {
    scenario("s1_t2d_four_slots", &six_held(), |paths, evidence| {
        let _daemon = Daemon::start(paths, evidence, "final")?;
        let sessions = spawn_six_held(paths, evidence)?;
        check_four_running_two_waiting(paths)?;
        release_six(paths)?;
        for session in &sessions {
            let envelope = t2c_wait(paths, evidence, &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || {
                format!("{session}: {envelope}")
            })?;
        }
        let anchors = store_count(paths, "SELECT count(*) FROM anchors")?;
        check(anchors == 6, || format!("{anchors} launches for six turns"))
    })
}

/// T2-D 2 (design §11): a force stop while two turns wait for a slot. The
/// waiting turns are `cancelled` without submission, the four running ones
/// end under the force row, every session closes, and the exit is clean.
#[test]
fn s1_t2d_force_while_turns_wait_for_a_slot() -> TestResult {
    scenario("s1_t2d_force_waiting", &six_held(), |paths, evidence| {
        let mut daemon = Daemon::start(paths, evidence, "forced")?;
        let sessions = spawn_six_held(paths, evidence)?;
        check_four_running_two_waiting(paths)?;
        let stop = paths.run(evidence, "stop", &["daemon", "stop", "--force", "--json"])?;
        check(stop.status.success(), || "force stop refused".to_owned())?;
        let status = wait_child(&mut daemon.child, FINAL_SHUTDOWN + Duration::from_secs(2))?
            .ok_or_else(|| ScenarioError::Timeout("force stop never exited".to_owned()))?;
        check(status.code() == Some(0), || format!("daemon exit {status}"))?;
        let (mut forced, mut never_submitted) = (0, 0);
        for session in &sessions {
            let (state, envelope) = turn_n(paths, session, 1)?;
            check(state == "cancelled", || {
                format!("{session}: {state} {envelope}")
            })?;
            if envelope["timestamps"]["submitted_at"].is_null() {
                check(
                    envelope["cancel"].is_null() && anchors_of_turn(paths, session, 1)? == 0,
                    || format!("a waiting turn launched: {envelope}"),
                )?;
                never_submitted += 1;
            } else {
                check(envelope["cancel"]["outcome"] == "forced", || {
                    format!("a running turn: {envelope}")
                })?;
                forced += 1;
            }
            let types = paths.event_types(session)?;
            check(
                types.last().map(String::as_str) == Some("session.closed"),
                || format!("{session} not closed: {types:?}"),
            )?;
        }
        check((forced, never_submitted) == (4, 2), || {
            format!("forced {forced}, cancelled while waiting {never_submitted}")
        })?;
        daemon.shutdown()?;
        let _daemon = Daemon::start(paths, evidence, "final")?;
        completes_normally(paths, evidence).map(drop)
    })
}

/// T2-D 3 (design §11, §3.2): the Store-failed latch while two turns wait for
/// a slot. A seventh spawn's receipt reply is lost (the 15th lifecycle
/// commit, after six receipts, four submissions and four acceptances). The
/// waiting turns get no grant and no launch, and the daemon exits 4.
#[test]
fn s1_t2d_latch_while_turns_wait_for_a_slot() -> TestResult {
    scenario("s1_t2d_latch_waiting", &six_held(), |paths, evidence| {
        let lost = "store.commit.reply_lost";
        paths.failpoints.arm(lost, 15, "fail_io").map_err(infra)?;
        let mut daemon = Daemon::start(paths, evidence, "latched")?;
        let sessions = spawn_six_held(paths, evidence)?;
        check_four_running_two_waiting(paths)?;
        let seventh = t2c_spawn(paths, evidence, "spawn-lost", "s0", None)?;
        check(
            !seventh.status.success()
                && error_kind(&seventh.stderr).as_deref() == Some("store_error"),
            || "the seventh receipt was not store_error".to_owned(),
        )?;
        latched_exit(&mut daemon)?;
        let mut waiting = 0;
        for session in &sessions {
            let (state, envelope) = turn_n(paths, session, 1)?;
            if state == "queued" {
                check(
                    envelope.is_null() && anchors_of_turn(paths, session, 1)? == 0,
                    || format!("a waiting turn was granted: {envelope}"),
                )?;
                waiting += 1;
            }
        }
        let anchors = store_count(paths, "SELECT count(*) FROM anchors")?;
        let submitted = store_count(
            paths,
            "SELECT count(*) FROM turns WHERE submitted_at IS NOT NULL",
        )?;
        check(waiting == 2 && anchors == 4 && submitted == 4, || {
            format!("waiting {waiting}, anchors {anchors}, submitted {submitted}")
        })?;
        paths.failpoints.disarm(lost).map_err(infra)?;
        daemon.shutdown()?;
        release_six(paths)?;
        let _daemon = Daemon::start(paths, evidence, "final")?;
        completes_normally(paths, evidence).map(drop)
    })
}

/// Two quick sessions for the lowered-pool T2-D tests: prompts `s0` and `s1`.
fn two_quick() -> Value {
    json!({"scripts":[t2c_script(1, "s0", None), t2c_script(1, "s1", None)]})
}

/// Spawns one session with `prompt` and returns its id.
fn spawn_session(
    paths: &Paths,
    evidence: &Evidence,
    name: &str,
    prompt: &str,
) -> Result<String, ScenarioError> {
    let spawn = t2c_spawn(paths, evidence, name, prompt, None)?;
    Ok(json_line(&spawn.stdout)?["session_id"]
        .as_str()
        .ok_or_else(|| fail("receipt has no session"))?
        .to_owned())
}

/// A turn that waits for a connection slot: its reservation is pending and
/// registered, and it is still `queued`, never submitted and never
/// launched. Counting began with [`count_waiters`] before its spawn.
fn still_waiting(paths: &Paths, session: &str) -> Result<(), ScenarioError> {
    wait_hits(paths, AWAITING_SLOT, 1)?;
    let (state, envelope) = turn_n(paths, session, 1)?;
    let submitted = store_count(
        paths,
        "SELECT count(*) FROM turns WHERE submitted_at IS NOT NULL",
    )?;
    let anchors = anchors_of_turn(paths, session, 1)?;
    check(
        state == "queued" && envelope.is_null() && anchors == 0,
        || {
            format!(
                "the waiting turn launched: {state} {envelope}, {anchors} anchors, {submitted} submitted"
            )
        },
    )
}

/// A force stop, exiting `code`, that ends the waiting turn `cancelled`
/// without submission.
fn force_cancels_waiting(
    paths: &Paths,
    evidence: &Evidence,
    daemon: &mut Daemon<'_>,
    session: &str,
    code: i32,
) -> Result<(), ScenarioError> {
    let stop = paths.run(evidence, "stop", &["daemon", "stop", "--force", "--json"])?;
    check(stop.status.success(), || "force stop refused".to_owned())?;
    let status = wait_child(&mut daemon.child, FINAL_SHUTDOWN + Duration::from_secs(2))?
        .ok_or_else(|| ScenarioError::Timeout("force stop never exited".to_owned()))?;
    check(status.code() == Some(code), || {
        format!("daemon exit {status}")
    })?;
    let (state, envelope) = turn_n(paths, session, 1)?;
    check(
        state == "cancelled"
            && envelope["timestamps"]["submitted_at"].is_null()
            && anchors_of_turn(paths, session, 1)? == 0,
        || format!("the waiting turn: {state} {envelope}"),
    )
}

/// T2-D Sol decision 1 (design §11, runtime §5): a slot is capacity for a
/// live process group. With one slot, turn A completes but its absence-proof
/// commit fails, so its cleanup stays uncertain and Host keeps the slot:
/// turn B in another session is not submitted or launched. Only the force
/// stop's reconciliation proves A absent, and the force cancels B unsent.
/// Released when the run returned, B launched at once.
#[test]
fn s1_t2d_uncertain_cleanup_keeps_its_connection_slot() -> TestResult {
    scenario(
        "s1_t2d_uncertain_keeps_slot",
        &two_quick(),
        |paths, evidence| {
            let commit = "host.recovery.absence_commit";
            paths.failpoints.arm(commit, 1, "fail_io").map_err(infra)?;
            let mut daemon = Daemon::start_slots(paths, evidence, "uncertain", 1, None)?;
            let first = spawn_session(paths, evidence, "spawn-a", "s0")?;
            let envelope = result_when_done(paths, evidence, &format!("{first}/1"))?;
            let unproven = "SELECT count(*) FROM anchors WHERE absence_time IS NULL";
            check(store_count(paths, unproven)? == 1, || {
                format!("turn A's group was proved absent: {envelope}")
            })?;
            count_waiters(paths)?;
            let second = spawn_session(paths, evidence, "spawn-b", "s1")?;
            still_waiting(paths, &second)?;
            force_cancels_waiting(paths, evidence, &mut daemon, &second, 0)?;
            check(store_count(paths, unproven)? == 0, || {
                "shutdown never proved turn A's group absent".to_owned()
            })?;
            paths.failpoints.disarm(commit).map_err(infra)?;
            daemon.shutdown()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            completes_normally(paths, evidence).map(drop)
        },
    )
}

/// T2-D Sol decision 2 (design §11): a group an earlier daemon left, whose
/// absence recovery cannot prove, counts. An ended turn's synthetic anchor
/// with no identity recovers `UnverifiedAnchor`; after a restart with one
/// slot, a new turn is neither submitted nor launched, and the force stop
/// cancels it unsent (exit 4: the group is still unproven). Without the reservation it launched at once.
#[test]
fn s1_t2d_unproven_recovered_group_reduces_capacity() -> TestResult {
    scenario("s1_t2d_recovered_group", &two_quick(), |paths, evidence| {
        let daemon = Daemon::start(paths, evidence, "first")?;
        let ended = spawn_session(paths, evidence, "spawn-a", "s0")?;
        let envelope = t2c_wait(paths, evidence, &format!("{ended}/1"))?;
        check(envelope["state"] == "completed", || envelope.to_string())?;
        daemon.shutdown()?;
        let changed = rusqlite::Connection::open(paths.state.join("store.sqlite3"))
            .map_err(infra)?
            .execute(
                "INSERT INTO anchors(anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,phase,record_version)
                 SELECT 'unverified-0','g-unverified-0',a.marker,'/nonexistent',a.owner_session,1,a.uid,a.boot_id,a.pid_namespace,'intent',1
                 FROM anchors a WHERE a.owner_session=?1 LIMIT 1",
                [&ended],
            )
            .map_err(infra)?;
        check(changed == 1, || "no anchor to copy".to_owned())?;
        let mut daemon = Daemon::start_slots(paths, evidence, "restarted", 1, None)?;
        count_waiters(paths)?;
        let waiting = spawn_session(paths, evidence, "spawn-b", "s1")?;
        still_waiting(paths, &waiting)?;
        // The group is still unproven at shutdown: `incomplete`, exit 4.
        force_cancels_waiting(paths, evidence, &mut daemon, &waiting, 4)?;
        // The run exited and was reaped: the fabricated row, which no
        // process ever had, is removed before its teardown checks the real
        // anchors (S1-evidence2 fix round 2, finding 1: every run is
        // validated).
        delete_synthetic(paths, "unverified-")?;
        daemon.shutdown()?;
        let _daemon = Daemon::start(paths, evidence, "final")?;
        completes_normally(paths, evidence).map(drop)
    })
}

/// T2-D round 1, decision 1 (design §11): a failed acquisition proves its
/// group absent before it returns. With one slot, turn A's vendor binary is
/// missing, so the anchor refuses ARM (`VendorSpawnFailed`) and A ends
/// `unknown` (or `failed`).
/// Host then proves A's group absent and releases the slot, so turn B in
/// another session launches and completes. Before, A's slot stayed held
/// until shutdown and B never launched.
#[test]
fn s1_t2d_failed_acquisition_releases_its_proved_absent_slot() -> TestResult {
    scenario(
        "s1_t2d_failed_acquisition",
        &two_quick(),
        |paths, evidence| {
            let vendor = paths.state.with_file_name("vendor");
            let hidden = paths.state.with_file_name("vendor.hidden");
            fs::copy(&paths.fake, &vendor).map_err(infra)?;
            let _daemon = Daemon::start_slots(paths, evidence, "final", 1, Some(&vendor))?;
            fs::rename(&vendor, &hidden).map_err(infra)?;
            let failed = spawn_session(paths, evidence, "spawn-a", "s0")?;
            let envelope = t2c_wait(paths, evidence, &format!("{failed}/1"))?;
            check(
                envelope["state"] == "unknown" || envelope["state"] == "failed",
                || format!("turn A with no vendor: {envelope}"),
            )?;
            fs::rename(&hidden, &vendor).map_err(infra)?;
            let next = spawn_session(paths, evidence, "spawn-b", "s1")?;
            let envelope = t2c_wait(paths, evidence, &format!("{next}/1"))?;
            check(envelope["state"] == "completed", || {
                format!("turn B after A's failed launch: {envelope}")
            })?;
            let unproven = store_count(
                paths,
                "SELECT count(*) FROM anchors WHERE absence_time IS NULL",
            )?;
            check(unproven == 0, || {
                format!("{unproven} anchors left unproven")
            })
        },
    )
}

/// T2-D round 1, decision 2 (design §11): a recovery deadline counts the
/// anchors it left unread. 300 proven-absent synthetic anchors sort first;
/// one with no identity and no absence proof sorts after the first page.
/// Reconciliation is held at the page boundary past its deadline, so paging
/// stops before that anchor. Startup still admits, and with one slot a new
/// turn stays `queued`, unsent, with no anchor. The force stop cancels it;
/// exit 4, since the anchor is still unproven. Before, the unread anchor
/// held nothing and the turn launched at once.
#[test]
fn s1_t2d_recovery_deadline_counts_unread_unproven_anchors() -> TestResult {
    scenario("s1_t2d_unread_anchors", &two_quick(), |paths, evidence| {
        let daemon = Daemon::start(paths, evidence, "first")?;
        let ended = spawn_session(paths, evidence, "spawn-a", "s0")?;
        let envelope = t2c_wait(paths, evidence, &format!("{ended}/1"))?;
        check(envelope["state"] == "completed", || envelope.to_string())?;
        daemon.shutdown()?;
        insert_proven_absent(paths, &ended, "0-synthetic", 300)?;
        let changed = rusqlite::Connection::open(paths.state.join("store.sqlite3"))
            .map_err(infra)?
            .execute(
                "INSERT INTO anchors(anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,phase,record_version)
                 SELECT '1-unproven','g-1-unproven',a.marker,'/nonexistent',a.owner_session,1,a.uid,a.boot_id,a.pid_namespace,'intent',1
                 FROM anchors a WHERE a.owner_session=?1 LIMIT 1",
                [&ended],
            )
            .map_err(infra)?;
        check(changed == 1, || "no anchor to copy".to_owned())?;
        let boundary = "core.recovery.page_boundary";
        arm(paths, boundary, "pause")?;
        let mut daemon = Daemon::spawn_with(paths, evidence, "restarted", Some(1), None)?;
        acknowledged(paths, evidence, boundary, "pause", &daemon)?;
        // The deadline began before the acknowledgement: 5 s after it has passed.
        thread::sleep(Duration::from_millis(5_200));
        paths.failpoints.release(boundary, 1).map_err(infra)?;
        daemon.wait_ready()?;
        paths.failpoints.disarm(boundary).map_err(infra)?;
        count_waiters(paths)?;
        let waiting = spawn_session(paths, evidence, "spawn-b", "s1")?;
        still_waiting(paths, &waiting)?;
        force_cancels_waiting(paths, evidence, &mut daemon, &waiting, 4)?;
        // As above: the fabricated rows go before the run's teardown.
        delete_synthetic(paths, "0-synthetic")?;
        delete_synthetic(paths, "1-unproven")?;
        daemon.shutdown()?;
        let _daemon = Daemon::start(paths, evidence, "final")?;
        completes_normally(paths, evidence).map(drop)
    })
}

/// The fake profile with native steer (C1 §3.4).
fn steer_profile() -> Value {
    json!({"capabilities": {
        "verbs": {"spawn":{"support":"native"},"resume":{"support":"native"},
                  "steer":{"support":"native"},"cancel":{"support":"native"},
                  "close":{"support":"native"}},
        "params": {"instructions":{"support":"unsupported","reason":"no instructions input"},
                   "output_schema":{"support":"unsupported","reason":"no schema input"},
                   "effort":{"support":"unsupported","reason":"no effort setting"},
                   "max_steps":{"support":"unsupported","reason":"no step limit"}},
        "bounds": [], "network_control": false,
        "recover": {"support":"unsupported","reason":"no recovery"},
        "usage": {"tokens":"turn","cost":"unavailable"}
    }})
}

/// Turn 1 (`k2`) takes a steer and reports its delivery.
fn keyed_steer_fixture() -> Value {
    let emit = |message: Value| json!({"action":"emit","message":message});
    let terminal = |turn: &str| {
        emit(
            json!({"type":"terminal","vendor_turn_id":turn,"status":"completed",
                    "final_text":"done","stop_reason":"end_turn"}),
        )
    };
    json!({"profile": steer_profile(), "scripts": [
        {"expected_request":{"type":"start","prompt":"k2"},"steps":[
            emit(json!({"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"})),
            {"action":"expect_request","expected":{"type":"steer","id":3}},
            emit(json!({"type":"steer_delivered","id":3,"vendor_turn_id":"fake-turn-1"})),
            {"action":"gate","name":"delivered"},
            terminal("fake-turn-1"),
        ]},
    ]})
}

/// The keyed steer's operation row: its verb and stored result, if any.
fn steer_row(paths: &Paths, session: &str) -> Result<(String, Option<String>), ScenarioError> {
    paths
        .store()?
        .query_row(
            "SELECT verb,result FROM operations WHERE session_id=?1 AND op_key='k2-key'",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(infra)
}

/// K2 (via-jm4.36, C1 §3.4): the vendor took a keyed steer's input and
/// reported it, and the daemon crashes before the report's commit, which
/// would record the outcome (`core.steer.before_outcome`): the intent row
/// is durable with no result and no `steer.delivered` exists. Restart
/// recovery stores the uncertain outcome. A repeat under the key is
/// answered with it, `steer_failed` with `data.reason: "not_delivered"`
/// and `data.delivery: "uncertain"`, its message saying the outcome was not
/// durably recorded (K2 r1 #5), where a new steer is
/// `no_active_turn` (the turn was recovered `unknown`, and C1 §7.3 cancels
/// the queue behind it): the input is never sent again, and no
/// `steer.delivered` exists.
/// The message of a keyed steer's uncertain outcome (K2 r1 #5).
const UNRECORDED: &str = "the steer's delivery outcome was not durably recorded; whether its input was applied is unknown";

#[test]
fn s1_crash_keyed_steer_before_its_outcome_is_never_resent() -> TestResult {
    scenario(
        "s1_crash_keyed_steer_before_its_outcome",
        &keyed_steer_fixture(),
        |paths, evidence| {
            let point = "core.steer.before_outcome";
            arm(paths, point, "crash")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let mut args = spawn_args("k2").to_vec();
            args.extend(["--handle", HANDLE]);
            let spawn = paths.run(evidence, "spawn", &args)?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session"))?
                .to_owned();
            let steer = [
                "steer", &session, "--text", "also", "--op-key", "k2-key", "--json",
            ];
            let client = paths.client_pending(evidence, "steer-crashed", &steer);
            acknowledged(paths, evidence, point, "crash", &daemon)?;
            daemon.wait_crash()?;
            let (status, stdout, _) = client.finish()?;
            check(!status.success() && stdout.is_empty(), || {
                format!("crashed steer client exited {status} with a reply")
            })?;
            let (verb, result) = steer_row(paths, &session)?;
            check(verb == "steer" && result.is_none(), || {
                format!("the intent row is not open: {verb} {result:?}")
            })?;
            let types = paths.event_types(&session)?;
            check(!types.iter().any(|kind| kind == "steer.delivered"), || {
                format!("the report committed before the crash: {types:?}")
            })?;
            paths.failpoints.disarm(point).map_err(infra)?;
            daemon.shutdown()?;

            let _daemon = Daemon::start(paths, evidence, "final")?;
            let uncertain = json!({"refused":"steer_failed","reason":"not_delivered",
                                   "delivery":"uncertain","recorded":false});
            let (_, result) = steer_row(paths, &session)?;
            let stored: Option<Value> = result
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(infra)?;
            check(stored.as_ref() == Some(&uncertain), || {
                format!("recovery did not store the uncertain outcome: {result:?}")
            })?;
            check(paths.turn(&session)?.state == "unknown", || {
                "turn 1 was not recovered unknown".to_owned()
            })?;
            let mut repeat = steer.to_vec();
            repeat.extend(["--handle", HANDLE]);
            let replayed = paths.run(evidence, "steer-repeat", &repeat)?;
            let error: Value = serde_json::from_slice(&replayed.stderr).map_err(infra)?;
            check(
                !replayed.status.success()
                    && error["data"]["kind"] == "steer_failed"
                    && error["data"]["reason"] == "not_delivered"
                    && error["data"]["delivery"] == "uncertain"
                    && error["message"] == UNRECORDED,
                || format!("the repeat is not the stored outcome: {error}"),
            )?;
            let fresh = [
                "steer", &session, "--text", "also", "--handle", HANDLE, "--json",
            ];
            let fresh = paths.run(evidence, "steer-unkeyed", &fresh)?;
            check(
                error_kind(&fresh.stderr).as_deref() == Some("no_active_turn"),
                || {
                    format!(
                        "an unkeyed steer is not no_active_turn: {}",
                        String::from_utf8_lossy(&fresh.stderr)
                    )
                },
            )?;
            let types = paths.event_types(&session)?;
            check(!types.iter().any(|kind| kind == "steer.delivered"), || {
                format!("input was delivered again: {types:?}")
            })
        },
    )
}
