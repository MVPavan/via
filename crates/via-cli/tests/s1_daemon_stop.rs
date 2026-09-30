//! `daemon/stop` through the real `via` binary and daemon (C1 §3.14, runtime
//! §6 final shutdown): force enters final shutdown at once, drain finishes
//! accepted work first, the `{"stopping":true}` receipt is acceptance only,
//! final shutdown delivers committed results and never claims a false clean
//! exit, and cleanup after daemon-first death is proved by the outer harness
//! seam.

#[cfg(feature = "test-failpoints")]
#[path = "support/failpoints.rs"]
mod failpoints;
#[cfg(feature = "test-failpoints")]
#[path = "support/hits.rs"]
mod hits;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use scenario::{Captured, ScenarioError, collect_available, run_command, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// C1/runtime final shutdown budget; force must finish inside it.
const FINAL_SHUTDOWN: Duration = Duration::from_secs(10);

struct Paths {
    _root: tempfile::TempDir,
    via: PathBuf,
    fake: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    sync: PathBuf,
    fixture: PathBuf,
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
        Ok(Self {
            _root: root,
            via,
            fake,
            state,
            runtime,
            sync,
            fixture: fixture_path,
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

    /// Writes every committed envelope and event, read-only, as scenario evidence.
    fn write_store_evidence(&self, evidence: &Evidence) -> Result<(), ScenarioError> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(infra)?;
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

    /// Terminal envelope and ordered events, read-only after the daemon exited.
    /// Turn 1's committed step rows (Task 4 design §3).
    fn step_rows(&self, session: &str) -> Result<i64, ScenarioError> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(infra)?;
        store
            .query_row(
                "SELECT count(*) FROM steps WHERE session_id=?1 AND turn=1",
                [session],
                |row| row.get(0),
            )
            .map_err(infra)
    }

    fn committed(&self, session: &str) -> Result<(Value, Vec<Value>), ScenarioError> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(infra)?;
        let envelope: Option<String> = store
            .query_row(
                "SELECT envelope FROM turns WHERE session_id=?1 AND number=1",
                [session],
                |row| row.get(0),
            )
            .map_err(infra)?;
        let envelope = serde_json::from_str(&envelope.ok_or_else(|| fail("no terminal envelope"))?)
            .map_err(infra)?;
        let mut query = store
            .prepare("SELECT event FROM events WHERE session_id=?1 ORDER BY seq")
            .map_err(infra)?;
        let events = query
            .query_map([session], |row| row.get::<_, String>(0))
            .map_err(infra)?
            .map(|event| serde_json::from_str(&event.map_err(infra)?).map_err(infra))
            .collect::<Result<Vec<Value>, _>>()?;
        Ok((envelope, events))
    }
}

/// Directly owned daemon child; teardown writes `cleanup.json` on every path.
struct Daemon<'a> {
    child: Child,
    paths: &'a Paths,
    cleanup: PathBuf,
    crash_snapshot: Option<Vec<outer_cleanup::AnchorRow>>,
}

impl<'a> Daemon<'a> {
    fn start(paths: &'a Paths, evidence: &Evidence) -> Result<Self, ScenarioError> {
        Self::start_with(paths, evidence, |_| {})
    }

    /// Starts the daemon with `configure` applied to its command.
    fn start_with(
        paths: &'a Paths,
        evidence: &Evidence,
        configure: impl FnOnce(&mut Command),
    ) -> Result<Self, ScenarioError> {
        // Startup lines only: later ones go to `via.log` (Task 4 design §7.6).
        let trace = evidence.dir.join("daemon.trace");
        let mut command = paths.command();
        configure(&mut command);
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&trace).map_err(infra)?);
        let mut daemon = Self {
            child: command.spawn().map_err(infra)?,
            paths,
            cleanup: evidence.dir.join("cleanup.json"),
            crash_snapshot: None,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = daemon.child.try_wait().map_err(infra)? {
                return Err(fail(&format!("daemon exited before readiness: {status}")));
            }
            // Never let the readiness probe auto-start a second daemon.
            if !paths.runtime.join("via.sock").exists() {
                if Instant::now() >= deadline {
                    return Err(ScenarioError::Timeout("daemon socket".to_owned()));
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let mut status = paths.command();
            status.args(["daemon", "status", "--json"]);
            let capture = run_command(&mut status, Duration::from_secs(1)).map_err(infra)?;
            if capture.status.success() {
                return Ok(daemon);
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout("daemon readiness".to_owned()));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_exit(&mut self, within: Duration) -> Result<Option<ExitStatus>, ScenarioError> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().map_err(infra)? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// The daemon's final bounded shutdown summary line: the last in
    /// `via.log` (Task 4 design §7.6).
    fn summary(&self) -> Result<Value, ScenarioError> {
        let log = fs::read_to_string(self.paths.state.join("via.log")).map_err(infra)?;
        log.lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|line| line.get("daemon_shutdown").cloned())
            .ok_or_else(|| fail("daemon wrote no final shutdown summary"))
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
        let mut reaped = matches!(self.wait_exit(Duration::from_secs(2)), Ok(Some(_)));
        if !reaped {
            kill = if self.child.kill().is_ok() {
                "sent_to_retained_child"
            } else {
                "failed"
            };
            reaped = matches!(self.wait_exit(Duration::from_secs(1)), Ok(Some(_)));
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
        if let Ok(mut file) = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.cleanup)
        {
            let _ = file.write_all(report.to_string().as_bytes());
            let _ = file.sync_all();
        }
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

fn wait_file(path: &Path) -> Result<(), ScenarioError> {
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

fn read_pid(path: &Path) -> Result<u32, ScenarioError> {
    fs::read_to_string(path)
        .map_err(infra)?
        .trim()
        .parse()
        .map_err(infra)
}

/// Whether `pid` names a live (non-zombie) process; zombies await reaping.
fn process_live(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|text| {
            let state = text.get(text.rfind(')')? + 2..)?.chars().next()?;
            Some(state != 'Z' && state != 'X')
        })
        .unwrap_or(false)
}

/// Waits until `pid` is gone or a zombie awaiting its (possibly slow) reaper.
fn wait_not_live(pid: u32) -> Result<(), ScenarioError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_live(pid) {
        if Instant::now() >= deadline {
            return Err(fail(&format!("process {pid} is still live")));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn json_line(bytes: &[u8]) -> Result<Value, ScenarioError> {
    serde_json::from_slice(bytes).map_err(infra)
}

/// A turn that is accepted, reports partial text, starts a grandchild in the
/// owned group and then holds at gate `hold` until released; after release it
/// completes.
fn held_fixture() -> Value {
    json!({
        "expected_request":{"type":"start","id":1,"turn":1,"prompt":"hold"},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"text","vendor_turn_id":"fake-turn-1","text":"partial"}},
            {"action":"spawn_grandchild","name":"gc"},
            {"action":"report_pids"},
            {"action":"gate","name":"hold"},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"drained","stop_reason":"end_turn"}}
        ]
    })
}

/// A held turn without a grandchild: the fake waits for its grandchildren
/// before exiting, so a drained turn must not own a gated one.
fn drain_fixture() -> Value {
    let mut fixture = held_fixture();
    if let Some(steps) = fixture["steps"].as_array_mut() {
        steps.retain(|step| step["action"] != "spawn_grandchild");
    }
    fixture
}

/// Starts the held turn in the background and waits until it is live.
fn start_held_turn(
    paths: &Paths,
    evidence: &Evidence,
) -> Result<(String, u32, Option<u32>), ScenarioError> {
    let spawn = paths.run(
        evidence,
        "spawn",
        &[
            "spawn",
            "--harness",
            "fake",
            "--model",
            "fake",
            "--prompt",
            "hold",
            "--background",
            "--json",
        ],
    )?;
    check(spawn.status.success(), || {
        format!("spawn exited {}", spawn.status)
    })?;
    let receipt = json_line(&spawn.stdout)?;
    let session = receipt["session_id"]
        .as_str()
        .ok_or_else(|| fail("receipt has no session id"))?
        .to_owned();
    wait_file(&paths.sync.join("hold.entered"))?;
    let agent = read_pid(&paths.sync.join("agent.pid"))?;
    let grandchild = if paths.sync.join("gc.pid").exists() {
        wait_file(&paths.sync.join("gc.entered"))?;
        Some(read_pid(&paths.sync.join("gc.pid"))?)
    } else {
        None
    };
    Ok((session, agent, grandchild))
}

/// C1 §6 force lifecycle: the turn's `prefix` events, then `cancel.requested`,
/// `cancel.settled`, `turn.ended` (the last event of the turn, ending its
/// range) and the session-level `session.closed`, all densely sequenced.
fn check_forced_lifecycle(
    envelope: &Value,
    events: &[Value],
    prefix: &[&str],
) -> Result<(), ScenarioError> {
    let types: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default())
        .collect();
    let mut expected = prefix.to_vec();
    expected.extend([
        "cancel.requested",
        "cancel.settled",
        "turn.ended",
        "session.closed",
    ]);
    check(types == expected, || format!("event types {types:?}"))?;
    for (index, event) in events.iter().enumerate() {
        check(event["seq"] == json!(index + 1), || {
            format!("seq not dense at {index}: {event}")
        })?;
    }
    let [settled, ended, closed] = [
        &events[prefix.len() + 1],
        &events[prefix.len() + 2],
        &events[prefix.len() + 3],
    ];
    check(
        settled["outcome"] == envelope["cancel"]["outcome"]
            && settled["cleanup"] == envelope["cancel"]["cleanup"]
            && settled["turn"] == 1,
        || format!("cancel.settled {settled}"),
    )?;
    check(
        ended["state"] == envelope["state"]
            && ended["failure"] == envelope["failure"]
            && ended["cancel"] == envelope["cancel"],
        || format!("turn.ended {ended}"),
    )?;
    check(
        closed["turn"].is_null() && closed["reason"] == "daemon_stop_force",
        || format!("session.closed {closed}"),
    )?;
    let last = prefix.len() + 3;
    check(
        envelope["events"] == json!({"first_seq":1,"last_seq":last,"count":last}),
        || format!("event range {}", envelope["events"]),
    )
}

/// Waits until the Store has committed an event of `kind` for `session`.
fn wait_event(paths: &Paths, session: &str, kind: &str) -> Result<(), ScenarioError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let store = rusqlite::Connection::open_with_flags(
            paths.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(infra)?;
        let found: bool = store
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE session_id=?1 AND json_extract(event,'$.type')=?2)",
                [session, kind],
                |row| row.get(0),
            )
            .map_err(infra)?;
        if found {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!("{kind} never committed")));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Holds the Store's SQLite write lock from outside the daemon, so its next
/// commits fail after the busy timeout: a real Store write failure.
fn hold_store_write_lock(paths: &Paths) -> Result<rusqlite::Connection, ScenarioError> {
    let store = rusqlite::Connection::open_with_flags(
        paths.state.join("store.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(infra)?;
    store.execute_batch("BEGIN IMMEDIATE").map_err(infra)?;
    Ok(store)
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

/// T1-I7: force must enter final shutdown at once, not wait for the held
/// turn's 30 s wall deadline; the turn ends `cancelled` with Host evidence.
#[test]
fn s1_daemon_stop_force_ends_active_turn_immediately() -> TestResult {
    scenario(
        "s1_daemon_stop_force",
        &held_fixture(),
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence)?;
            let (session, agent, grandchild) = start_held_turn(paths, evidence)?;
            // `hold.entered` proves only that the vendor emitted its text; force
            // once the daemon has made the acceptance durable. Model text is
            // not an event (Task 4 design §2.1).
            wait_event(paths, &session, "turn.started")?;
            let started = Instant::now();
            let stop = paths.run(
                evidence,
                "stop_force",
                &["daemon", "stop", "--force", "--json"],
            )?;
            check(stop.status.success(), || {
                format!("force stop exited {}", stop.status)
            })?;
            check(json_line(&stop.stdout)? == json!({"stopping":true}), || {
                "force stop receipt".to_owned()
            })?;
            let status = daemon.wait_exit(FINAL_SHUTDOWN)?;
            let elapsed = started.elapsed();
            let status = status
                .ok_or_else(|| fail("force stop did not exit within the 10 s final shutdown"))?;
            check(status.code() == Some(0), || {
                format!("force stop exit {status}")
            })?;
            wait_not_live(agent)?;
            wait_not_live(grandchild.ok_or_else(|| fail("no grandchild"))?)?;
            let summary = daemon.summary()?;
            evidence
                .write("daemon_shutdown.json", summary.to_string().as_bytes())
                .map_err(infra)?;
            check(
                summary["mode"] == "force" && summary["disposition"] == "clean",
                || format!("summary {summary}"),
            )?;
            let (envelope, events) = paths.committed(&session)?;
            evidence
                .write("envelope.json", envelope.to_string().as_bytes())
                .map_err(infra)?;
            check(envelope["state"] == "cancelled", || {
                format!("envelope {envelope}")
            })?;
            check(
                envelope["failure"].is_null() && envelope["stop_reason"] == "interrupted",
                || format!("envelope {envelope}"),
            )?;
            check(
                envelope["cancel"]["outcome"] == "forced"
                    && envelope["cancel"]["cleanup"] == "quiescent",
                || format!("cancel {}", envelope["cancel"]),
            )?;
            // Events committed before the force stay in the turn (W3-F 8),
            // and the forced terminal carries the open step's row (Task 4
            // design §3.2).
            check_forced_lifecycle(
                &envelope,
                &events,
                &["turn.queued", "turn.submitted", "turn.started"],
            )?;
            let rows = paths.step_rows(&session)?;
            check(rows == 1, || format!("{rows} step rows"))?;
            check(elapsed < FINAL_SHUTDOWN, || {
                format!("force took {elapsed:?}")
            })
        },
    )
}

/// C1 `drain`: accepted work finishes under its own deadline, new work is
/// refused `daemon_stopping`, then final shutdown exits cleanly.
#[test]
fn s1_daemon_stop_drain_finishes_accepted_turn_then_exits() -> TestResult {
    scenario(
        "s1_daemon_stop_drain",
        &drain_fixture(),
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence)?;
            let (session, _, _) = start_held_turn(paths, evidence)?;
            let refused = paths.run(evidence, "stop_plain", &["daemon", "stop", "--json"])?;
            check(
                refused.status.code() == Some(2)
                    && json_line(&refused.stderr)?["data"]["kind"] == "admission_refused",
                || "plain stop must refuse active work".to_owned(),
            )?;
            let both = paths.run(
                evidence,
                "stop_both",
                &["daemon", "stop", "--drain", "--force", "--json"],
            )?;
            check(
                both.status.code() == Some(2)
                    && json_line(&both.stderr)?["data"]["kind"] == "invalid_params",
                || "drain with force must be invalid".to_owned(),
            )?;
            let stop = paths.run(
                evidence,
                "stop_drain",
                &["daemon", "stop", "--drain", "--json"],
            )?;
            check(stop.status.success(), || {
                format!("drain stop exited {}", stop.status)
            })?;
            check(json_line(&stop.stdout)? == json!({"stopping":true}), || {
                "drain stop receipt".to_owned()
            })?;
            let late = paths.run(
                evidence,
                "spawn_late",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "late",
                    "--json",
                ],
            )?;
            check(
                late.status.code() == Some(2)
                    && json_line(&late.stderr)?["data"]["kind"] == "daemon_stopping",
                || "spawn during drain must be daemon_stopping".to_owned(),
            )?;
            check(
                daemon.wait_exit(Duration::from_millis(300))?.is_none(),
                || "drain must not stop before accepted work ends".to_owned(),
            )?;
            fs::write(paths.sync.join("hold.release"), b"").map_err(infra)?;
            let status = daemon
                .wait_exit(FINAL_SHUTDOWN)?
                .ok_or_else(|| fail("drained daemon did not exit"))?;
            check(status.code() == Some(0), || format!("drain exit {status}"))?;
            let summary = daemon.summary()?;
            evidence
                .write("daemon_shutdown.json", summary.to_string().as_bytes())
                .map_err(infra)?;
            check(
                summary["mode"] == "drain" && summary["disposition"] == "clean",
                || format!("summary {summary}"),
            )?;
            let (envelope, _) = paths.committed(&session)?;
            check(
                envelope["state"] == "completed" && envelope["final_text"] == "drained",
                || format!("envelope {envelope}"),
            )
        },
    )
}

/// Seam §4.2: the receipt is only acceptance; clean completion is the
/// daemon's own exit 0 with a clean summary (joins, Store, cleanup).
#[test]
fn s1_daemon_stop_receipt_precedes_clean_exit() -> TestResult {
    let fixture = json!({
        "expected_request":{"type":"start","id":1,"turn":1,"prompt":"hello"},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"reply","stop_reason":"end_turn"}}
        ]
    });
    scenario("s1_daemon_stop_clean", &fixture, |paths, evidence| {
        let mut daemon = Daemon::start(paths, evidence)?;
        let spawn = paths.run(
            evidence,
            "spawn",
            &[
                "spawn",
                "--harness",
                "fake",
                "--model",
                "fake",
                "--prompt",
                "hello",
                "--json",
            ],
        )?;
        check(spawn.status.success(), || {
            format!("spawn exited {}", spawn.status)
        })?;
        let stop = paths.run(evidence, "stop", &["daemon", "stop", "--json"])?;
        check(
            stop.status.success() && json_line(&stop.stdout)? == json!({"stopping":true}),
            || "idle stop receipt".to_owned(),
        )?;
        let status = daemon
            .wait_exit(FINAL_SHUTDOWN)?
            .ok_or_else(|| fail("idle stop did not exit"))?;
        check(status.code() == Some(0), || {
            format!("idle stop exit {status}")
        })?;
        let summary = daemon.summary()?;
        evidence
            .write("daemon_shutdown.json", summary.to_string().as_bytes())
            .map_err(infra)?;
        check(
            summary["mode"] == "idle"
                && summary["disposition"] == "clean"
                && summary["pending_joins"] == 0
                && summary["failed_joins"] == 0
                && summary["uncertain_owners"] == 0
                && summary["store"] == "joined",
            || format!("summary {summary}"),
        )?;
        check(!paths.runtime.join("via.sock").exists(), || {
            "clean exit leaves no socket".to_owned()
        })
    })
}

/// Seam §4.5 / runtime §11.2: after the daemon dies first, the anchor's
/// EOF cleanup is proved by the outer snapshot/control/ESRCH path alone.
#[test]
fn s1_daemon_first_death_outer_cleanup_proves_absence() -> TestResult {
    scenario(
        "s1_daemon_first_death",
        &held_fixture(),
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence)?;
            let (_, agent, grandchild) = start_held_turn(paths, evidence)?;
            let rows =
                outer_cleanup::snapshot(&paths.state.join("store.sqlite3")).map_err(infra)?;
            check(rows.len() == 1, || format!("{} anchor rows", rows.len()))?;
            daemon.child.kill().map_err(infra)?;
            daemon.child.wait().map_err(infra)?;
            let anchors = outer_cleanup::verify(&rows, Instant::now() + FINAL_SHUTDOWN);
            evidence
                .write("outer_cleanup.json", anchors.to_string().as_bytes())
                .map_err(infra)?;
            daemon.crash_snapshot = Some(rows);
            check(
                anchors["status"] == "quiescent" && anchors["absence_proven"] == true,
                || format!("outer cleanup {anchors}"),
            )?;
            wait_not_live(agent)?;
            wait_not_live(grandchild.ok_or_else(|| fail("no grandchild"))?)
        },
    )
}

/// Spawns a foreground `via spawn` that waits for its result, with its output
/// kept as evidence; the caller waits for it.
fn spawn_foreground(paths: &Paths, evidence: &Evidence) -> Result<Child, ScenarioError> {
    let mut command = paths.command();
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
        .stdin(Stdio::null())
        .stdout(File::create(evidence.dir.join("foreground.stdout")).map_err(infra)?)
        .stderr(File::create(evidence.dir.join("foreground.stderr")).map_err(infra)?);
    command.spawn().map_err(infra)
}

fn wait_child(child: &mut Child, within: Duration) -> Result<Option<ExitStatus>, ScenarioError> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().map_err(infra)? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// W1-D Sol finding 1: an accepted stop reaches daemon main before its reply
/// is written. The caller never reads the reply and keeps the connection
/// open; the daemon must still run final shutdown and exit. A request id is
/// now at most 256 bytes (C1 A31), so the reply can no longer fill the
/// socket buffer; a reply write that never completes is covered by
/// `s1_c1_reply_not_read_closes_the_socket` (design §10.1, A32).
#[test]
fn s1_daemon_stop_unread_reply_still_stops() -> TestResult {
    let fixture = json!({
        "expected_request":{"type":"start","id":1,"turn":1,"prompt":"hello"},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"reply","stop_reason":"end_turn"}}
        ]
    });
    scenario(
        "s1_daemon_stop_unread_reply",
        &fixture,
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence)?;
            // One completed turn first, so the run keeps turn evidence.
            let spawn = paths.run(
                evidence,
                "spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "hello",
                    "--json",
                ],
            )?;
            check(spawn.status.success(), || {
                format!("spawn exited {}", spawn.status)
            })?;
            let mut stream = UnixStream::connect(paths.runtime.join("via.sock")).map_err(infra)?;
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .map_err(infra)?;
            let hello = json!({"jsonrpc":"2.0","id":1,"method":"hello","params":{
                "api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"s1-test"}});
            writeln!(stream, "{hello}").map_err(infra)?;
            let mut reply = String::new();
            BufReader::new(stream.try_clone().map_err(infra)?)
                .read_line(&mut reply)
                .map_err(infra)?;
            check(
                json_line(reply.as_bytes())?["result"]["api_version"] == 1,
                || format!("hello reply {reply}"),
            )?;
            // The longest id C1 accepts (A31), echoed in the unread reply.
            let id = "x".repeat(254);
            let stop = json!({"jsonrpc":"2.0","id":id,"method":"daemon/stop","params":{}});
            writeln!(stream, "{stop}").map_err(infra)?;
            // The reply is never read while the daemon stops.
            let status = daemon.wait_exit(FINAL_SHUTDOWN + Duration::from_secs(5))?;
            drop(stream);
            let status = status
                .ok_or_else(|| fail("an unread stop reply kept the daemon from final shutdown"))?;
            check(status.code() == Some(0), || format!("exit {status}"))?;
            let summary = daemon.summary()?;
            check(
                summary["mode"] == "idle" && summary["disposition"] == "clean",
                || format!("summary {summary}"),
            )
        },
    )
}

/// W1-D Sol finding 2 under runtime §7: a receipted turn whose terminal
/// commit failed, and whose one same-sequence retry failed too, latches
/// Store failure (design §7.2 row 7, escalation), so the daemon stops
/// admission and dispatch and shuts itself down in force mode; the exit is
/// 4, never clean. An outside SQLite writer lock makes both attempts fail
/// for real; it is released once the daemon reports the latch, so the
/// turn's failure-resolution batch commits it `failed(store)` (§7.4).
/// Re-pointed in S5: the turn no longer stays unresolved, and the latch is
/// read from `daemon/status` in the diagnostic window, not from the
/// socket's removal.
#[test]
fn s1_daemon_stop_store_failure_is_not_a_clean_exit() -> TestResult {
    scenario(
        "s1_daemon_stop_unresolved",
        &drain_fixture(),
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence)?;
            let (session, _, _) = start_held_turn(paths, evidence)?;
            // The acceptance is the turn's last write before its terminal;
            // model text is not an event (Task 4 design §2.1).
            wait_event(paths, &session, "turn.started")?;
            let lock = hold_store_write_lock(paths)?;
            fs::write(paths.sync.join("hold.release"), b"").map_err(infra)?;
            let latched = wait_latched(paths);
            drop(lock);
            latched?;
            let status = daemon
                .wait_exit(FINAL_SHUTDOWN)?
                .ok_or_else(|| fail("the latched daemon did not exit"))?;
            let summary = daemon.summary()?;
            evidence
                .write("daemon_shutdown.json", summary.to_string().as_bytes())
                .map_err(infra)?;
            check(status.code() == Some(4), || {
                format!("unresolved turn must exit 4, got {status}; summary {summary}")
            })?;
            check(
                summary["disposition"] == "incomplete"
                    && summary["mode"] == "force"
                    && summary["store_failed"] == true
                    && summary["unresolved_turns"] == 0
                    && summary["failure_batches"] == json!({"committed":1,"skipped":0}),
                || format!("summary {summary}"),
            )?;
            let (envelope, _) = paths.committed(&session)?;
            check(
                envelope["state"] == "failed" && envelope["failure"]["class"] == "store",
                || format!("the batch did not resolve the turn: {envelope}"),
            )
        },
    )
}

/// Waits until the daemon reports the latch (`health: store_failed`,
/// design §7.5), which it serves through the diagnostic window (§7.4); the
/// socket stays until the window ends. Never auto-starts a second daemon:
/// each probe needs the socket.
fn wait_latched(paths: &Paths) -> Result<(), ScenarioError> {
    let socket = paths.runtime.join("via.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if socket.exists() {
            let mut status = paths.command();
            status.args(["daemon", "status", "--json"]);
            let capture = run_command(&mut status, Duration::from_secs(1)).map_err(infra)?;
            let latched = capture.status.success()
                && json_line(&capture.stdout)
                    .is_ok_and(|status| status["health"] == "store_failed");
            if latched {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(
                "the daemon never latched".to_owned(),
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// W1-D Sol finding 4: final shutdown lets a foreground `via spawn` already
/// waiting on its turn receive the result committed during shutdown.
#[test]
fn s1_daemon_stop_force_delivers_waiting_foreground_result() -> TestResult {
    scenario(
        "s1_daemon_stop_force_foreground",
        &held_fixture(),
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence)?;
            let mut foreground = spawn_foreground(paths, evidence)?;
            wait_file(&paths.sync.join("hold.entered"))?;
            wait_file(&paths.sync.join("gc.entered"))?;
            let stop = paths.run(
                evidence,
                "stop_force",
                &["daemon", "stop", "--force", "--json"],
            )?;
            check(stop.status.success(), || {
                format!("force stop exited {}", stop.status)
            })?;
            let waited = wait_child(&mut foreground, FINAL_SHUTDOWN)?;
            let status = daemon
                .wait_exit(FINAL_SHUTDOWN)?
                .ok_or_else(|| fail("force stop did not exit"))?;
            let waited = waited.ok_or_else(|| fail("foreground spawn never returned"))?;
            let stdout = fs::read(evidence.dir.join("foreground.stdout")).map_err(infra)?;
            let lines: Vec<&[u8]> = stdout
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .collect();
            check(waited.code() == Some(3) && lines.len() == 2, || {
                format!(
                    "foreground exit {waited}, stdout {}",
                    String::from_utf8_lossy(&stdout)
                )
            })?;
            let envelope = json_line(lines[1])?;
            check(
                envelope["state"] == "cancelled" && envelope["cancel"]["outcome"] == "forced",
                || format!("envelope {envelope}"),
            )?;
            check(status.code() == Some(0), || format!("daemon exit {status}"))
        },
    )
}

/// W1-D Sol finding 3: a force while the vendor is launched but has not
/// accepted is `forced` only from Host evidence that it stopped the live
/// group, never `acknowledged`, and the lifecycle events commit.
#[test]
fn s1_daemon_stop_force_before_acceptance_is_forced() -> TestResult {
    let fixture = json!({
        "expected_request":{"type":"start","id":1,"turn":1,"prompt":"hold"},
        "steps":[
            {"action":"report_pids"},
            {"action":"gate","name":"hold"},
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"late","stop_reason":"end_turn"}}
        ]
    });
    scenario(
        "s1_daemon_stop_force_before_acceptance",
        &fixture,
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence)?;
            let (session, agent, _) = start_held_turn(paths, evidence)?;
            let stop = paths.run(
                evidence,
                "stop_force",
                &["daemon", "stop", "--force", "--json"],
            )?;
            check(stop.status.success(), || {
                format!("force stop exited {}", stop.status)
            })?;
            let status = daemon
                .wait_exit(FINAL_SHUTDOWN)?
                .ok_or_else(|| fail("force stop did not exit"))?;
            wait_not_live(agent)?;
            let summary = daemon.summary()?;
            check(
                status.code() == Some(0) && summary["disposition"] == "clean",
                || format!("exit {status}, summary {summary}"),
            )?;
            let (envelope, events) = paths.committed(&session)?;
            evidence
                .write("envelope.json", envelope.to_string().as_bytes())
                .map_err(infra)?;
            check(
                envelope["state"] == "cancelled"
                    && envelope["timestamps"]["accepted_at"].is_null()
                    && envelope["cancel"]["outcome"] == "forced"
                    && envelope["cancel"]["cleanup"] == "quiescent",
                || format!("envelope {envelope}"),
            )?;
            check_forced_lifecycle(&envelope, &events, &["turn.queued", "turn.submitted"])
        },
    )
}

/// W3-F 8 under runtime §7: a turn whose write failed (Task 4: a step
/// row's, since model text is no event) latches Store
/// failure; the daemon force-stops itself and the turn ends `failed(store)`,
/// not `cancelled`: C1 §8.2 `store` records that the durable stream lost an
/// event, and hiding it behind a cancellation would claim a complete record.
/// The cancel evidence is still reported. The failure was observed before
/// the forced terminal's close check, so no close-bearing commit starts
/// (T2-B2 design §3.2): no `session.closed`, and the exit is 4.
#[test]
fn s1_daemon_stop_force_after_store_failure_ends_failed_store() -> TestResult {
    let fixture = json!({
        "expected_request":{"type":"start","id":1,"turn":1,"prompt":"hold"},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"text","vendor_turn_id":"fake-turn-1","text":"first"}},
            {"action":"emit","message":{"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t","name":"shell"}},
            {"action":"emit","message":{"type":"tool_ended","vendor_turn_id":"fake-turn-1","tool_id":"t"}},
            {"action":"gate","name":"text"},
            {"action":"emit","message":{"type":"text","vendor_turn_id":"fake-turn-1","text":"lost"}},
            {"action":"report_pids"},
            {"action":"gate","name":"hold"},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"late","stop_reason":"end_turn"}}
        ]
    });
    scenario(
        "s1_daemon_stop_force_after_store_failure",
        &fixture,
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence)?;
            let spawn = paths.run(
                evidence,
                "spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "hold",
                    "--background",
                    "--json",
                ],
            )?;
            let session = json_line(&spawn.stdout)?["session_id"]
                .as_str()
                .ok_or_else(|| fail("receipt has no session id"))?
                .to_owned();
            wait_file(&paths.sync.join("text.entered"))?;
            wait_event(paths, &session, "turn.started")?;
            let lock = hold_store_write_lock(paths)?;
            fs::write(paths.sync.join("text.release"), b"").map_err(infra)?;
            // Past the Store's 250 ms busy timeout the commit of step 1's row,
            // which the text after the tool result ends, fails and latches
            // (Task 4 design §3.2); the lock is released once the daemon reports the
            // latch, so the failure-resolution batch can commit (re-pointed
            // in S5: the socket now stays through the diagnostic window).
            let latched = wait_latched(paths);
            drop(lock);
            latched?;
            let status = daemon
                .wait_exit(FINAL_SHUTDOWN)?
                .ok_or_else(|| fail("the latched daemon did not exit"))?;
            check(status.code() == Some(4), || format!("exit {status}"))?;
            let summary = daemon.summary()?;
            check(summary["store_failed"] == true, || {
                format!("summary {summary}")
            })?;
            let (envelope, events) = paths.committed(&session)?;
            evidence
                .write("envelope.json", envelope.to_string().as_bytes())
                .map_err(infra)?;
            check(
                envelope["state"] == "failed"
                    && envelope["failure"]["class"] == "store"
                    && envelope["stop_reason"] == "error"
                    && envelope["cancel"]["outcome"] == "forced",
                || format!("envelope {envelope}"),
            )?;
            let types: Vec<&str> = events
                .iter()
                .map(|event| event["type"].as_str().unwrap_or_default())
                .collect();
            check(
                types
                    == [
                        "turn.queued",
                        "turn.submitted",
                        "turn.started",
                        "turn.ended",
                    ],
                || format!("event types {types:?}"),
            )?;
            check(
                events
                    .iter()
                    .enumerate()
                    .all(|(index, event)| event["seq"] == json!(index + 1)),
                || "seq not dense".to_owned(),
            )?;
            check(
                envelope["events"] == json!({"first_seq":1,"last_seq":4,"count":4}),
                || format!("event range {}", envelope["events"]),
            )
        },
    )
}

/// W3-F Sol 1, made deterministic with `daemon.dispatcher.before_start`
/// (design §10): a force accepted right after spawn receipts, while a
/// receipted turn is still queued for daemon main and outside its drive set,
/// still ends every receipted turn `cancelled` with truthful cancel fields
/// and a clean exit. Daemon main is paused about to start turn A's
/// dispatcher; turn B is receipted on a connection already served, so its
/// start waits in the channel, and a force lands on a third. Released, main
/// starts A's dispatcher and enters final shutdown, which takes B from the
/// queue: the summary counts one queued drive. A first turn completes
/// beforehand, so the run has turn evidence.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_daemon_stop_force_right_after_receipt_cancels_queued_turn() -> TestResult {
    scenario(
        "s1_daemon_stop_force_after_receipt",
        &drain_fixture(),
        |paths, evidence| {
            let root = paths
                .state
                .parent()
                .ok_or_else(|| infra("the state directory has no parent"))?;
            let failpoints = failpoints::Failpoints::new(root).map_err(infra)?;
            let dir = root.join("failpoints");
            let point = "daemon.dispatcher.before_start";
            hits::count(&dir, point).map_err(infra)?;
            let mut daemon =
                Daemon::start_with(paths, evidence, |command| failpoints.activate(command))?;
            let (first, _, _) = start_held_turn(paths, evidence)?;
            fs::write(paths.sync.join("hold.release"), b"").map_err(infra)?;
            wait_event(paths, &first, "turn.ended")?;
            // Re-arm the gate: a launched raced turn holds until forced.
            for name in ["hold.entered", "hold.release", "hold.released", "agent.pid"] {
                let _ = fs::remove_file(paths.sync.join(name));
            }
            let next = hits::hits(&dir, point).map_err(infra)? + 1;
            failpoints.arm(point, next, "pause").map_err(infra)?;
            // Daemon main accepts no connection while paused: open all three first.
            let socket = paths.runtime.join("via.sock");
            let mut a = Raw::connect(&socket)?;
            let mut b = Raw::connect(&socket)?;
            let mut forcer = Raw::connect(&socket)?;
            let first_raced = a.spawn()?;
            failpoints
                .wait_ack(point, next, "pause", daemon.child.id(), FINAL_SHUTDOWN)
                .map_err(infra)?;
            evidence
                .write(
                    "before_start.ack",
                    &failpoints.ack_bytes(point, next).map_err(infra)?,
                )
                .map_err(infra)?;
            // Main is paused: B's start waits in the channel.
            let raced = [first_raced, b.spawn()?];
            let stopping = forcer.request("daemon/stop", &json!({"force":true}))?;
            check(stopping["result"]["stopping"] == true, || {
                stopping.to_string()
            })?;
            failpoints.release(point, next).map_err(infra)?;
            let status = daemon
                .wait_exit(FINAL_SHUTDOWN)?
                .ok_or_else(|| fail("force stop did not exit"))?;
            let summary = daemon.summary()?;
            evidence
                .write("daemon_shutdown.json", summary.to_string().as_bytes())
                .map_err(infra)?;
            check(
                status.code() == Some(0)
                    && summary["disposition"] == "clean"
                    && summary["queued_drives"] == 1,
                || format!("exit {status}, summary {summary}"),
            )?;
            failpoints.disarm(point).map_err(infra)?;
            for session in raced {
                check_raced_turn(paths, &session)?;
            }
            Ok(())
        },
    )
}

/// A raw connection to the daemon that has had its `hello` served.
#[cfg(feature = "test-failpoints")]
struct Raw {
    reader: BufReader<UnixStream>,
    stream: UnixStream,
    next: u64,
}

#[cfg(feature = "test-failpoints")]
impl Raw {
    fn connect(socket: &Path) -> Result<Self, ScenarioError> {
        let stream = UnixStream::connect(socket).map_err(infra)?;
        stream
            .set_read_timeout(Some(FINAL_SHUTDOWN))
            .map_err(infra)?;
        let reader = BufReader::new(stream.try_clone().map_err(infra)?);
        let mut raw = Self {
            reader,
            stream,
            next: 0,
        };
        let hello = json!({"api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"force-race"});
        let reply = raw.request("hello", &hello)?;
        check(reply.get("result").is_some(), || reply.to_string())?;
        Ok(raw)
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, ScenarioError> {
        let id = self.next;
        self.next += 1;
        let mut line = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
        line.push('\n');
        self.stream.write_all(line.as_bytes()).map_err(infra)?;
        let mut reply = String::new();
        self.reader.read_line(&mut reply).map_err(infra)?;
        serde_json::from_str(&reply).map_err(|error| fail(&format!("{error}: {reply:?}")))
    }

    /// A held spawn's receipt; its session id.
    fn spawn(&mut self) -> Result<String, ScenarioError> {
        let spawn = json!({"harness":"fake","model":"fake","prompt":"hold","handle":format!("h_{}", "A".repeat(43))});
        let reply = self.request("spawn", &spawn)?;
        reply["result"]["session_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| fail(&format!("spawn not receipted: {reply}")))
    }
}

/// A receipted raced turn ends `cancelled`. One the force found still queued
/// (T2-B2 design §4) was never submitted: `turn.queued`, `turn.ended`,
/// `session.closed`, no cancel object. Otherwise it ends with the C1 force
/// lifecycle. Host
/// committed vendor facts only for a launched vendor, which holds at its gate,
/// so its force must be `forced`; a turn forced before launch was `requested`.
#[cfg(feature = "test-failpoints")]
fn check_raced_turn(paths: &Paths, session: &str) -> Result<(), ScenarioError> {
    let (envelope, events) = paths.committed(session)?;
    if events.iter().all(|event| event["type"] != "turn.submitted") {
        let types: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap_or_default())
            .collect();
        return check(
            envelope["state"] == "cancelled"
                && envelope["cancel"].is_null()
                && envelope["timestamps"]["submitted_at"].is_null()
                && types == ["turn.queued", "turn.ended", "session.closed"]
                && events[2]["reason"] == "daemon_stop_force",
            || format!("queued cancellation {envelope} {types:?}"),
        );
    }
    let outcome = match vendor_launched(paths, session)? {
        Some(true) => "forced",
        Some(false) => {
            return Err(fail(&format!("{session}: anchor without vendor facts")));
        }
        None => "requested",
    };
    check(
        envelope["state"] == "cancelled"
            && envelope["failure"].is_null()
            && envelope["stop_reason"] == "interrupted"
            && envelope["timestamps"]["accepted_at"].is_null()
            && envelope["cancel"]["outcome"] == outcome
            && envelope["cancel"]["cleanup"] == "quiescent",
        || format!("envelope {envelope}"),
    )?;
    check_forced_lifecycle(&envelope, &events, &["turn.queued", "turn.submitted"])
}

/// Host's launch evidence for `session`: `None` without an anchor intent,
/// otherwise whether Host committed the launched vendor's facts.
#[cfg(feature = "test-failpoints")]
fn vendor_launched(paths: &Paths, session: &str) -> Result<Option<bool>, ScenarioError> {
    let store = rusqlite::Connection::open_with_flags(
        paths.state.join("store.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(infra)?;
    let mut query = store
        .prepare("SELECT vendor_pid IS NOT NULL FROM anchors WHERE owner_session=?1")
        .map_err(infra)?;
    let launched: Vec<bool> = query
        .query_map([session], |row| row.get(0))
        .map_err(infra)?
        .collect::<Result<_, _>>()
        .map_err(infra)?;
    Ok(match launched.as_slice() {
        [] => None,
        [launched] => Some(*launched),
        _ => return Err(fail(&format!("{session}: more than one anchor"))),
    })
}

/// T4-2 review round 2 (coding-style §5, design §6.5): a blob step still
/// running after the Store's bounded drain is pending work, so final
/// shutdown is `incomplete` (exit 4) and its summary counts the step. A
/// prompt over 256 KiB stalls its first blob step (`blob.step.stall`);
/// the spawn fails `not_committed` at the 2 s bound; an idle stop then
/// drops the Store with the step still held. A first small turn completes,
/// so the run has turn evidence.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_daemon_stop_stalled_blob_step_is_not_a_clean_exit() -> TestResult {
    let fixture = json!({
        "expected_request":{"type":"start","id":1,"turn":1,"prompt":"hello"},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"reply","stop_reason":"end_turn"}}
        ]
    });
    scenario(
        "s1_daemon_stop_stalled_blob",
        &fixture,
        |paths, evidence| {
            let root = paths
                .state
                .parent()
                .ok_or_else(|| infra("the state directory has no parent"))?;
            let failpoints = failpoints::Failpoints::new(root).map_err(infra)?;
            let point = "blob.step.stall";
            failpoints.arm(point, 1, "pause").map_err(infra)?;
            let mut daemon =
                Daemon::start_with(paths, evidence, |command| failpoints.activate(command))?;
            let receipted = paths.run(
                evidence,
                "spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "hello",
                    "--json",
                ],
            )?;
            check(receipted.status.success(), || {
                format!("the small spawn exited {}", receipted.status)
            })?;
            let mut raw = Raw::connect(&paths.runtime.join("via.sock"))?;
            let spawn = json!({"harness":"fake","model":"fake","prompt":"p".repeat(256 * 1024 + 1),"handle":format!("h_{}", "A".repeat(43))});
            let refused = raw.request("spawn", &spawn)?;
            failpoints
                .wait_ack(point, 1, "pause", daemon.child.id(), FINAL_SHUTDOWN)
                .map_err(infra)?;
            check(
                refused["error"]["data"]["kind"] == "store_error"
                    && refused["error"]["data"]["commit_outcome"] == "not_committed",
                || format!("the stalled blob spawn was not refused: {refused}"),
            )?;
            let stopping = raw.request("daemon/stop", &json!({}))?;
            check(stopping["result"]["stopping"] == true, || {
                stopping.to_string()
            })?;
            drop(raw);
            // The summary is written with the step still held; the process
            // itself exits once the step's thread ends.
            let deadline = Instant::now() + FINAL_SHUTDOWN;
            let summary = loop {
                if let Ok(summary) = daemon.summary() {
                    break summary;
                }
                if Instant::now() >= deadline {
                    failpoints.release(point, 1).map_err(infra)?;
                    return Err(fail("daemon wrote no final shutdown summary"));
                }
                thread::sleep(Duration::from_millis(5));
            };
            failpoints.release(point, 1).map_err(infra)?;
            let status = daemon
                .wait_exit(FINAL_SHUTDOWN)?
                .ok_or_else(|| fail("the daemon did not exit"))?;
            evidence
                .write("daemon_shutdown.json", summary.to_string().as_bytes())
                .map_err(infra)?;
            check(status.code() == Some(4), || {
                format!("a stalled blob step must exit 4, got {status}; summary {summary}")
            })?;
            check(
                summary["disposition"] == "incomplete"
                    && summary["blob_tasks"] == 1
                    && summary["pending_joins"] == 1
                    && summary["store"] == "joined",
                || format!("summary {summary}"),
            )
        },
    )
}
