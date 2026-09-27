//! `daemon/stop` through the real `via` binary and daemon (C1 §3.14, runtime
//! §6 final shutdown): force enters final shutdown at once, drain finishes
//! accepted work first, the `{"stopping":true}` receipt is acceptance only,
//! and cleanup after daemon-first death is proved by the outer harness seam.

#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
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
    trace: PathBuf,
    cleanup: PathBuf,
    crash_snapshot: Option<Vec<outer_cleanup::AnchorRow>>,
}

impl<'a> Daemon<'a> {
    fn start(paths: &'a Paths, evidence: &Evidence) -> Result<Self, ScenarioError> {
        let trace = evidence.dir.join("daemon.trace");
        let mut command = paths.command();
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&trace).map_err(infra)?);
        let mut daemon = Self {
            child: command.spawn().map_err(infra)?,
            paths,
            trace,
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

    /// The daemon's final bounded shutdown summary line.
    fn summary(&self) -> Result<Value, ScenarioError> {
        let trace = fs::read_to_string(&self.trace).map_err(infra)?;
        trace
            .lines()
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

/// A turn that is accepted, starts a grandchild in the owned group and then
/// holds at gate `hold` until released; after release it completes.
fn held_fixture() -> Value {
    json!({
        "expected_request":{"type":"start","id":1,"turn":1,"prompt":"hold"},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
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
            let ended = events.last().ok_or_else(|| fail("no events"))?;
            check(
                ended["type"] == "turn.ended"
                    && ended["state"] == "cancelled"
                    && ended["cancel"] == envelope["cancel"],
                || format!("turn.ended {ended}"),
            )?;
            check(envelope["events"]["last_seq"] == ended["seq"], || {
                "event range".to_owned()
            })?;
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
