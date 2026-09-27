//! Crash points around spawn and submission (S1 F8, F10; runtime-contracts §11
//! failpoint table), driven through the real `via` binary, daemon and SQLite
//! Store with the test-only failpoint controller.
//!
//! F8: a crash inside `spawn`'s write leaves session, turn 1, handle hash and
//! queued event together or not at all, and a lost reply leaves one whole
//! session that is never dispatched. F10: a crash after submission intent,
//! after the prompt reached the agent or before acceptance was recorded
//! restarts as `unknown` and is never sent again; the submission record
//! precedes any agent I/O.
#![cfg(feature = "test-failpoints")]

#[path = "support/failpoints.rs"]
mod failpoints;
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

use failpoints::Failpoints;
use scenario::{Captured, ScenarioError, collect_available, run_command, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const ACK_WAIT: Duration = Duration::from_secs(10);
const FINAL_SHUTDOWN: Duration = Duration::from_secs(10);
/// A valid caller handle: `h_` and 43 base64url digits whose last is `A`.
const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const OTHER_HANDLE: &str = "h_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA";

struct Paths {
    _root: tempfile::TempDir,
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
            _root: root,
            via,
            fake,
            state,
            runtime,
            sync,
            fixture: fixture_path,
            failpoints,
        })
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
        let capture = run_command(&mut command, Duration::from_secs(15)).map_err(infra)?;
        evidence
            .write(&format!("{name}.stdout"), &capture.stdout)
            .map_err(infra)?;
        evidence
            .write(&format!("{name}.stderr"), &capture.stderr)
            .map_err(infra)?;
        if capture.timed_out {
            return Err(ScenarioError::Timeout(format!("via {args:?} timed out")));
        }
        Ok(capture)
    }

    /// Starts `via spawn --background` without waiting: its reply may never come.
    fn spawn_pending(&self, evidence: &Evidence, name: &str, prompt: &str) -> PendingClient {
        let stdout = evidence.dir.join(format!("{name}.stdout"));
        let stderr = evidence.dir.join(format!("{name}.stderr"));
        let mut command = self.command();
        command
            .args(spawn_args(prompt))
            .env("VIA_HANDLE", HANDLE)
            .stdin(Stdio::null());
        let child = File::create(&stdout)
            .and_then(|out| Ok((out, File::create(&stderr)?)))
            .and_then(|(out, err)| command.stdout(out).stderr(err).spawn());
        PendingClient {
            child: Some(child),
            stdout,
            stderr,
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

    /// Row counts of every table a spawn writes, read in one transaction.
    fn counts(&self) -> Result<[i64; 4], ScenarioError> {
        let mut store = self.store()?;
        let tx = store.transaction().map_err(infra)?;
        let mut counts = [0; 4];
        for (count, table) in counts
            .iter_mut()
            .zip(["sessions", "turns", "events", "anchors"])
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
struct PendingClient {
    child: Option<std::io::Result<Child>>,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl PendingClient {
    /// Waits for the client and returns its status, stdout and stderr.
    fn finish(mut self) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), ScenarioError> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| infra("pending client already finished"))?
            .map_err(infra)?;
        let status = wait_child(&mut child, Duration::from_secs(10));
        let Ok(Some(status)) = status else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ScenarioError::Timeout(
                "pending client never returned".to_owned(),
            ));
        };
        Ok((
            status,
            fs::read(&self.stdout).map_err(infra)?,
            fs::read(&self.stderr).map_err(infra)?,
        ))
    }
}

/// A scenario that fails before `finish` never leaves its client unreaped.
impl Drop for PendingClient {
    fn drop(&mut self) {
        if let Some(Ok(mut child)) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Directly owned daemon child. The last one of a scenario writes
/// `cleanup.json` over every anchor the Store committed, crashed runs included.
struct Daemon<'a> {
    child: Child,
    paths: &'a Paths,
    cleanup: PathBuf,
    evidence_dir: PathBuf,
    crash_snapshot: Option<Vec<outer_cleanup::AnchorRow>>,
}

impl<'a> Daemon<'a> {
    /// Starts a daemon with the failpoint controller active; `run` names its
    /// trace and cleanup evidence.
    fn start(paths: &'a Paths, evidence: &Evidence, run: &str) -> Result<Self, ScenarioError> {
        let mut daemon = Self::spawn(paths, evidence, run)?;
        daemon.wait_ready()?;
        Ok(daemon)
    }

    /// Starts a daemon without waiting for it to admit requests.
    fn spawn(paths: &'a Paths, evidence: &Evidence, run: &str) -> Result<Self, ScenarioError> {
        let trace = if run == "final" {
            evidence.dir.join("daemon.trace")
        } else {
            evidence.dir.join(format!("daemon-{run}.trace"))
        };
        let mut command = paths.command();
        paths.failpoints.activate(&mut command);
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&trace).map_err(infra)?);
        let cleanup = if run == "final" {
            evidence.dir.join("cleanup.json")
        } else {
            evidence.dir.join(format!("cleanup-{run}.json"))
        };
        Ok(Self {
            child: command.spawn().map_err(infra)?,
            paths,
            cleanup,
            evidence_dir: evidence.dir.clone(),
            crash_snapshot: None,
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
            // Never let the readiness probe auto-start a second daemon.
            if paths.runtime.join("via.sock").exists() {
                let mut status = paths.command();
                status.args(["daemon", "status", "--json"]);
                let capture = run_command(&mut status, Duration::from_secs(1)).map_err(infra)?;
                if capture.status.success() {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout("daemon readiness".to_owned()));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Kills the daemon without any shutdown, as a crash would, and reaps it.
    fn kill(&mut self) -> Result<(), ScenarioError> {
        self.crash_snapshot =
            Some(outer_cleanup::snapshot(&self.paths.state.join("store.sqlite3")).map_err(infra)?);
        self.child.kill().map_err(infra)?;
        self.child.wait().map_err(infra)?;
        Ok(())
    }

    /// Waits for a failpoint `crash`: the daemon aborts (SIGABRT) by itself.
    fn wait_crash(&mut self) -> Result<(), ScenarioError> {
        let status = wait_child(&mut self.child, Duration::from_secs(10))?
            .ok_or_else(|| ScenarioError::Timeout("daemon never crashed".to_owned()))?;
        check(status.signal() == Some(6), || {
            format!("daemon ended {status}, not by the crash point's abort")
        })?;
        self.crash_snapshot =
            Some(outer_cleanup::snapshot(&self.paths.state.join("store.sqlite3")).map_err(infra)?);
        Ok(())
    }
}

impl Daemon<'_> {
    fn paths_final_cleanup(&self) -> PathBuf {
        self.evidence_dir.join("cleanup.json")
    }
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        let was_alive = matches!(self.child.try_wait(), Ok(None));
        let outer = Instant::now() + FINAL_SHUTDOWN;
        let mut stop = "not_needed";
        let mut kill = "not_needed";
        if was_alive {
            let mut command = self.paths.command();
            command.args(["daemon", "stop", "--force", "--json"]);
            stop = match run_command(&mut command, Duration::from_secs(2)) {
                Ok(capture) if capture.status.success() && !capture.timed_out => "accepted",
                Ok(_) => "refused",
                Err(_) => "unavailable",
            };
        }
        let mut reaped = matches!(
            wait_child(&mut self.child, Duration::from_secs(2)),
            Ok(Some(_))
        );
        if !reaped {
            kill = if self.child.kill().is_ok() {
                "sent_to_retained_child"
            } else {
                "failed"
            };
            reaped = matches!(
                wait_child(&mut self.child, Duration::from_secs(1)),
                Ok(Some(_))
            );
        }
        let rows = match self.crash_snapshot.take() {
            Some(rows) => Ok(rows),
            None => outer_cleanup::snapshot(&self.paths.state.join("store.sqlite3")),
        };
        let anchors = match rows {
            Ok(rows) => outer_cleanup::verify(&rows, outer),
            Err(error) => json!({"status":"unverified","absence_proven":false,"reason":error}),
        };
        let report = json!({
            "direct_child":{"pid":self.child.id(),"was_alive":was_alive,"stop":stop,"kill":kill,"reaped":reaped},
            "anchors":anchors,
        });
        let final_report = self.paths_final_cleanup();
        for path in [&self.cleanup, &final_report] {
            // A crashed run's report stands in until the final daemon's
            // teardown, which covers every committed anchor, replaces it.
            if path == &final_report && path != &self.cleanup && path.exists() {
                continue;
            }
            if let Ok(mut file) = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
            {
                let _ = file.write_all(report.to_string().as_bytes());
                let _ = file.sync_all();
            }
        }
    }
}

fn wait_child(child: &mut Child, within: Duration) -> Result<Option<ExitStatus>, ScenarioError> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().map_err(infra)? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
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

/// A turn that completes at once, used for post-recovery liveness and raw evidence.
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
        |evidence| {
            paths.write_store_evidence(evidence)?;
            collect_available(evidence, &paths.state)
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

/// F8: a crash while `spawn`'s transaction holds every row but has not
/// committed leaves no session, turn, handle hash or event, before and after
/// restart; the caller never received a receipt.
#[test]
fn s1_f08_crash_inside_spawn_write_leaves_nothing() -> TestResult {
    scenario(
        "s1_f08_crash_inside_spawn_write",
        &reply_steps("after"),
        |paths, evidence| {
            let point = "store.spawn.before_commit";
            arm(paths, point, "pause")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let client = paths.spawn_pending(evidence, "spawn-crashed", "f08");
            acknowledged(paths, evidence, point, "pause", &daemon)?;
            let paused = paths.counts()?;
            check(paused == [0; 4], || {
                format!("uncommitted spawn visible while paused: {paused:?}")
            })?;
            daemon.kill()?;
            let (status, stdout, _) = client.finish()?;
            check(!status.success() && stdout.is_empty(), || {
                format!("crashed spawn client exited {status} with a receipt")
            })?;
            paths.failpoints.disarm(point).map_err(infra)?;
            drop(daemon);
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let recovered = paths.counts()?;
            check(recovered == [0; 4], || {
                format!("partial spawn survived the crash: {recovered:?}")
            })?;
            completes_normally(paths, evidence)?;
            check(paths.counts()?[0] == 1, || {
                "post-recovery Store holds more than the new session".to_owned()
            })
        },
    )
}

/// F8: a crash right after `spawn`'s commit, before any reply, leaves session,
/// turn 1, handle hash and the queued event all durable together; after
/// restart the handle authenticates and the unacknowledged turn is never
/// dispatched.
#[test]
fn s1_f08_crash_after_spawn_commit_keeps_the_whole_session() -> TestResult {
    scenario(
        "s1_f08_crash_after_spawn_commit",
        &reply_steps("after"),
        |paths, evidence| {
            let point = "store.spawn.after_commit";
            arm(paths, point, "crash")?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let client = paths.spawn_pending(evidence, "spawn-crashed", "f08");
            acknowledged(paths, evidence, point, "crash", &daemon)?;
            daemon.wait_crash()?;
            let (status, stdout, _) = client.finish()?;
            check(!status.success() && stdout.is_empty(), || {
                format!("crashed spawn client exited {status} with a receipt")
            })?;
            paths.failpoints.disarm(point).map_err(infra)?;
            drop(daemon);
            let _daemon = Daemon::start(paths, evidence, "final")?;
            check_whole_queued_session(paths)?;
            let session = paths.only_session()?;
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
            completes_normally(paths, evidence)?;
            let turn = paths.turn(&session)?;
            check(
                turn.state == "queued" && turn.submitted_at.is_none(),
                || format!("unacknowledged turn was dispatched: {}", turn.state),
            )
        },
    )
}

/// Exactly one session exists with its receipt, handle hash, queued turn 1
/// (prompt kept) and only `turn.queued` at seq 1, and no process was started.
fn check_whole_queued_session(paths: &Paths) -> Result<(), ScenarioError> {
    let counts = paths.counts()?;
    check(counts == [1, 1, 1, 0], || {
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
/// The caller gets `store_error`, never a receipt; one whole session exists
/// and the unacknowledged turn is never dispatched. (Keyed replay of that
/// receipt needs spawn idempotency keys, which T2-B adds.)
#[test]
fn s1_f08_lost_spawn_reply_leaves_one_whole_undispatched_session() -> TestResult {
    scenario(
        "s1_f08_lost_spawn_reply",
        &reply_steps("after"),
        |paths, evidence| {
            let point = "store.commit.reply_lost";
            arm(paths, point, "fail_io")?;
            let daemon = Daemon::start(paths, evidence, "final")?;
            let client = paths.spawn_pending(evidence, "spawn-lost", "f08");
            acknowledged(paths, evidence, point, "fail_io", &daemon)?;
            let (status, stdout, stderr) = client.finish()?;
            check(
                !status.success()
                    && stdout.is_empty()
                    && error_kind(&stderr).as_deref() == Some("store_error"),
                || {
                    format!(
                        "lost reply returned {status}: {}",
                        String::from_utf8_lossy(&stderr)
                    )
                },
            )?;
            check_whole_queued_session(paths)?;
            let session = paths.only_session()?;
            completes_normally(paths, evidence)?;
            let turn = paths.turn(&session)?;
            check(
                turn.state == "queued" && paths.anchors_for(&session)? == 0,
                || format!("unacknowledged turn was dispatched: {}", turn.state),
            )
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
        let _ = child.kill();
        let _ = child.wait();
        return Err(fail(
            "daemon admitted requests after a failed reconciliation",
        ));
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
            drop(daemon);
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
            drop(daemon);
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
            drop(daemon);
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
            drop(daemon);
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

/// Commits `count` synthetic anchors owned by turn 1 of `owner`, each with a
/// valid-looking identity copied from an existing real anchor and a committed
/// absence proof, so Host accepts them without probing. Their groups
/// (`pgid` beyond Linux's `pid_max`) cannot exist. Ids start with `prefix`.
fn insert_proven_absent(
    paths: &Paths,
    owner: &str,
    prefix: &str,
    count: u32,
) -> Result<(), ScenarioError> {
    let mut store = rusqlite::Connection::open(paths.state.join("store.sqlite3")).map_err(infra)?;
    let tx = store.transaction().map_err(infra)?;
    for index in 0..count {
        let changed = tx
            .execute(
                "INSERT INTO anchors(anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,phase,record_version,pid,pgid,start_ticks,absence_time)
                 SELECT ?1,'g'||?1,a.marker,'/nonexistent',?2,1,a.uid,a.boot_id,a.pid_namespace,'arm_intent',1,?3,?3,1,'1'
                 FROM anchors a WHERE a.pid IS NOT NULL LIMIT 1",
                rusqlite::params![format!("{prefix}{index:05}"), owner, 4_194_305 + index],
            )
            .map_err(infra)?;
        check(changed == 1, || "no real anchor to copy".to_owned())?;
    }
    tx.commit().map_err(infra)
}

/// Removes synthetic anchors after the daemon exited, so teardown verifies
/// only anchors that ever ran.
fn delete_synthetic(paths: &Paths, prefix: &str) -> Result<(), ScenarioError> {
    let store = rusqlite::Connection::open(paths.state.join("store.sqlite3")).map_err(infra)?;
    store.busy_timeout(Duration::from_secs(5)).map_err(infra)?;
    store
        .execute(
            "DELETE FROM anchors WHERE anchor_id LIKE ?1",
            [format!("{prefix}%")],
        )
        .map(drop)
        .map_err(infra)
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
            drop(daemon);
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
            drop(daemon);
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
            drop(daemon);
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
