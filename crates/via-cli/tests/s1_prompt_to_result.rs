//! First S1 process-boundary scenario. Activate when the runtime spine lands.

#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::cell::RefCell;
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use scenario::{Captured, ScenarioError, collect_available, run_command, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn verify_anchors_after_daemon(cx: &Context<'_>) -> Result<Value, ScenarioError> {
    let db = cx.state.join("store.sqlite3");
    let store = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(infra)?;
    let mut query = store.prepare("SELECT anchor_id,generation,phase,socket_path,pid,pgid,uid,boot_id,pid_namespace,start_ticks,absence_time FROM anchors ORDER BY anchor_id").map_err(infra)?;
    let rows = query.query_map([], |row| Ok(json!({
        "anchor_id":row.get::<_, String>(0)?,"generation":row.get::<_, String>(1)?,
        "phase":row.get::<_, String>(2)?,"socket_path":row.get::<_, String>(3)?,
        "pid":row.get::<_, Option<i64>>(4)?,"pgid":row.get::<_, Option<i64>>(5)?,
        "uid":row.get::<_, i64>(6)?,"boot_id":row.get::<_, String>(7)?,
        "pid_namespace":row.get::<_, String>(8)?,"start_ticks":row.get::<_, Option<i64>>(9)?,
        "absence_time":row.get::<_, Option<String>>(10)?,
    }))).map_err(infra)?;
    let snapshot: Vec<Value> = rows.collect::<Result<_, _>>().map_err(infra)?;
    drop(query);
    drop(store);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(infra)?;
    let fake = via_core::FakeConfig::from_environment().map_err(infra)?;
    let engine =
        via_core::Engine::open(cx.state, cx.runtime, fake, cx.via.to_path_buf()).map_err(infra)?;
    let mut recovered = runtime.block_on(engine.verify_cleanup()).map_err(infra)?;
    drop(engine);
    drop(runtime);
    let records = recovered["records"]
        .as_array()
        .ok_or_else(|| infra("missing recovery records"))?;
    let exact = snapshot.len() == records.len()
        && snapshot.iter().all(|stored| {
            stored["phase"] == "arm_intent"
                && stored["pid"].as_i64().is_some()
                && stored["pgid"].as_i64().is_some()
                && stored["start_ticks"].as_i64().is_some()
                && records.iter().any(|record| {
                    record["anchor_id"] == stored["anchor_id"]
                        && record["generation"] == stored["generation"]
                        && record["cleanup"] == "group_absent"
                })
        });
    recovered["store_snapshot"] = json!(snapshot);
    if !exact {
        recovered["status"] = json!("unverified");
        recovered["absence_proven"] = json!(false);
        recovered["reason"] = json!("Store identity snapshot and Host recovery differ");
    }
    Ok(recovered)
}

struct Daemon<'a> {
    child: Child,
    cx: &'a Context<'a>,
    cleanup_path: PathBuf,
    ready: bool,
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        let was_alive = matches!(self.child.try_wait(), Ok(None));
        let mut anchors = json!({"status":"unverified","absence_proven":false,"reason":"verified cleanup unavailable"});
        let mut stop = "not_attempted";
        let mut direct_kill = "not_needed";
        let mut direct_reaped = false;
        if was_alive && self.ready {
            stop = match self.cx.run(
                &["daemon", "stop", "--force", "--json"],
                Duration::from_secs(2),
            ) {
                Ok(capture) if capture.timed_out => "timed_out",
                Ok(capture) if capture.status.success() => "accepted",
                Ok(_) => "refused",
                Err(_) => "unavailable",
            };
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    direct_reaped = true;
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
        if !direct_reaped {
            direct_kill = if self.child.kill().is_ok() {
                "sent_to_retained_child"
            } else {
                "failed"
            };
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                match self.child.try_wait() {
                    Ok(Some(_)) => {
                        direct_reaped = true;
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
        }
        if direct_reaped {
            anchors = verify_anchors_after_daemon(self.cx).unwrap_or_else(|error| {
                json!({
                    "status":"unverified","absence_proven":false,"reason":error.to_string()
                })
            });
        }
        let report = json!({
            "direct_child": {"pid":self.child.id(),"was_alive":was_alive,"stop":stop,"kill":direct_kill,"reaped":direct_reaped},
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

struct Context<'a> {
    via: &'a Path,
    state: &'a Path,
    runtime: &'a Path,
    fake: &'a Path,
    fixture: &'a Path,
    sync: &'a Path,
}

impl Context<'_> {
    fn command(&self) -> Command {
        let mut command = Command::new(self.via);
        command.env_clear();
        command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        command.env("VIA_STATE_DIR", self.state);
        command.env("VIA_RUNTIME_DIR", self.runtime);
        command.env("VIA_FAKE_AGENT_BINARY", self.fake);
        command.env("VIA_FAKE_SCENARIO", self.fixture);
        command.env("VIA_FAKE_SYNC_DIR", self.sync);
        command
    }

    fn run(&self, args: &[&str], timeout: Duration) -> TestResult<Captured> {
        let mut command = self.command();
        command.args(args);
        run_command(&mut command, timeout)
    }
}

fn start_daemon<'a>(
    cx: &'a Context<'a>,
    trace: &Path,
    cleanup_path: PathBuf,
) -> Result<Daemon<'a>, ScenarioError> {
    let mut command = cx.command();
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(trace).map_err(infra)?);
    let mut daemon = Daemon {
        child: command.spawn().map_err(infra)?,
        cx,
        cleanup_path,
        ready: false,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = daemon.child.try_wait().map_err(infra)? {
            return Err(ScenarioError::Failure(format!(
                "daemon exited before readiness: {status}"
            )));
        }
        let capture = cx
            .run(&["daemon", "status", "--json"], Duration::from_secs(1))
            .map_err(infra)?;
        if capture.timed_out {
            return Err(ScenarioError::Timeout("daemon status timed out".to_owned()));
        }
        if capture.status.success()
            && let Ok(value) = serde_json::from_slice::<Value>(&capture.stdout)
            && value["daemon_version"].is_string()
            && value["pid"].as_u64().is_some()
        {
            daemon.ready = true;
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

fn infra(error: impl std::fmt::Display) -> ScenarioError {
    ScenarioError::Infrastructure(error.to_string())
}

fn cli(
    cx: &Context<'_>,
    evidence: &Evidence,
    name: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, ScenarioError> {
    let capture = cx.run(args, timeout).map_err(infra)?;
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
        return Err(ScenarioError::Failure(format!(
            "via {args:?} exited {}",
            capture.status
        )));
    }
    Ok(capture.stdout)
}

fn fake_binary(via: &Path) -> TestResult<PathBuf> {
    let bin_dir = via.parent().ok_or("via binary has no parent directory")?;
    let fake = bin_dir.join("via-fake-agent");
    if !fake.is_file() {
        return Err(format!("missing {}; build -p via-fake-agent first", fake.display()).into());
    }
    Ok(fake)
}

fn write_reply_fixture(path: &Path) -> TestResult {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "expected_request":{"type":"start","id":1,"turn":1,"prompt":"hello"},
            "steps":[
                {"action":"report_pids"},
                {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
                {"action":"emit","message":{"type":"text","vendor_turn_id":"fake-turn-1","text":"reply"}},
                {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"reply","stop_reason":"end_turn"}}
            ]
        }))?,
    )?;
    Ok(())
}

fn write_gated_fixture(path: &Path) -> TestResult {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "expected_request":{"type":"start","id":1,"turn":1,"prompt":"slow"},
            "steps":[
                {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
                {"action":"gate","name":"wait_disconnected"},
                {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"after disconnect","stop_reason":"end_turn"}}
            ]
        }))?,
    )?;
    Ok(())
}

fn spawn_and_validate(
    cx: &Context<'_>,
    evidence: &Evidence,
) -> Result<(String, String), ScenarioError> {
    let output = cli(
        cx,
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
        Duration::from_secs(15),
    )?;
    evidence.write("envelopes.ndjson", &output).map_err(infra)?;
    let lines: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(serde_json::from_slice)
        .collect::<Result<_, _>>()
        .map_err(infra)?;
    assert_eq!(
        lines.len(),
        2,
        "foreground spawn needs receipt and envelope"
    );
    let receipt = &lines[0];
    let envelope = &lines[1];
    assert_eq!(receipt["state"], "queued");
    assert_eq!(envelope["state"], "completed");
    assert_eq!(envelope["final_text"], "reply");
    assert_eq!(envelope["exit"]["code"], 0);
    assert_eq!(receipt["session_id"], envelope["session_id"]);
    let session = receipt["session_id"]
        .as_str()
        .ok_or_else(|| ScenarioError::Failure("receipt has no session id".to_owned()))?
        .to_owned();
    let handle = receipt["handle"]
        .as_str()
        .ok_or_else(|| ScenarioError::Failure("receipt has no generated handle".to_owned()))?
        .to_owned();
    Ok((session, handle))
}

fn expect_request_error(
    cx: &Context<'_>,
    evidence: &Evidence,
    name: &str,
    args: &[&str],
    kind: &str,
) -> Result<(), ScenarioError> {
    let capture = cx.run(args, Duration::from_secs(5)).map_err(infra)?;
    evidence
        .write(&format!("{name}.stdout"), &capture.stdout)
        .map_err(infra)?;
    evidence
        .write(&format!("{name}.stderr"), &capture.stderr)
        .map_err(infra)?;
    if capture.timed_out || capture.status.code() != Some(2) {
        return Err(ScenarioError::Failure(format!(
            "{name}: expected request exit 2"
        )));
    }
    let error: Value = serde_json::from_slice(&capture.stderr).map_err(infra)?;
    if error["data"]["kind"] != kind {
        return Err(ScenarioError::Failure(format!(
            "{name}: expected {kind}, got {error}"
        )));
    }
    Ok(())
}

#[test]
fn s1_prompt_to_result_real_cli() -> TestResult {
    let via = Path::new(env!("CARGO_BIN_EXE_via"));
    let fake = fake_binary(via)?;
    let sandbox = tempfile::tempdir()?;
    let state = sandbox.path().join("state");
    let runtime = sandbox.path().join("runtime");
    let sync = sandbox.path().join("sync");
    fs::DirBuilder::new().mode(0o700).create(&state)?;
    fs::DirBuilder::new().mode(0o700).create(&runtime)?;
    fs::DirBuilder::new().mode(0o700).create(&sync)?;
    let fixture = sandbox.path().join("fixture.json");
    write_reply_fixture(&fixture)?;
    let evidence = Evidence::new("s1_prompt_to_result", &fake, &fixture)?;
    let cx = Context {
        via,
        state: &state,
        runtime: &runtime,
        fake: &fake,
        fixture: &fixture,
        sync: &sync,
    };
    let generated_handle = RefCell::new(None::<String>);
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = start_daemon(
                &cx,
                &evidence.dir.join("daemon.trace"),
                evidence.dir.join("cleanup.json"),
            )?;
            let (session, handle) = spawn_and_validate(&cx, evidence)?;
            *generated_handle.borrow_mut() = Some(handle.clone());
            let wrong_handle = format!("h_{}", "A".repeat(43));
            expect_request_error(
                &cx,
                evidence,
                "wrong_handle",
                &[
                    "steer",
                    &session,
                    "--text",
                    "late",
                    "--handle",
                    &wrong_handle,
                    "--json",
                ],
                "invalid_handle",
            )?;
            expect_request_error(
                &cx,
                evidence,
                "unsupported_steer",
                &[
                    "steer", &session, "--text", "late", "--handle", &handle, "--json",
                ],
                "unsupported_verb",
            )?;
            let events = cli(
                &cx,
                evidence,
                "events",
                &["events", &session, "--json"],
                Duration::from_secs(5),
            )?;
            evidence.write("events.ndjson", &events).map_err(infra)?;
            let logs = cli(
                &cx,
                evidence,
                "logs",
                &["logs", &session, "--json"],
                Duration::from_secs(5),
            )?;
            evidence.write("logs.ndjson", &logs).map_err(infra)?;
            Ok(())
        },
        |evidence| {
            collect_available(evidence, &state)?;
            if let Some(handle) = generated_handle.borrow().as_ref() {
                for name in [
                    "events.ndjson",
                    "logs.ndjson",
                    "daemon.trace",
                    "store.sqlite3",
                ] {
                    let bytes = fs::read(evidence.dir.join(name)).map_err(infra)?;
                    if bytes
                        .windows(handle.len())
                        .any(|window| window == handle.as_bytes())
                    {
                        return Err(ScenarioError::Failure(format!("handle leaked into {name}")));
                    }
                }
            }
            Ok(())
        },
    );
    report.require_pass()
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end disconnect scenario retains setup and teardown together"
)]
fn s1_f30_wait_disconnect_result_survives() -> TestResult {
    let via = Path::new(env!("CARGO_BIN_EXE_via"));
    let fake = fake_binary(via)?;
    let sandbox = tempfile::tempdir()?;
    let state = sandbox.path().join("state");
    let runtime = sandbox.path().join("runtime");
    let sync = sandbox.path().join("sync");
    for path in [&state, &runtime, &sync] {
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    let fixture = sandbox.path().join("fixture.json");
    write_gated_fixture(&fixture)?;
    let evidence = Evidence::new("s1_f30_wait_disconnect", &fake, &fixture)?;
    let cx = Context {
        via,
        state: &state,
        runtime: &runtime,
        fake: &fake,
        fixture: &fixture,
        sync: &sync,
    };
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = start_daemon(
                &cx,
                &evidence.dir.join("daemon.trace"),
                evidence.dir.join("cleanup.json"),
            )?;
            let receipt_bytes = cli(
                &cx,
                evidence,
                "background_spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "slow",
                    "--background",
                    "--json",
                ],
                Duration::from_secs(5),
            )?;
            let receipt: Value = serde_json::from_slice(&receipt_bytes).map_err(infra)?;
            let session = receipt["session_id"]
                .as_str()
                .ok_or_else(|| ScenarioError::Failure("missing session".to_owned()))?;
            let entered = sync.join("wait_disconnected.entered");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !entered.exists() {
                if Instant::now() >= deadline {
                    return Err(ScenarioError::Timeout("fake did not enter gate".to_owned()));
                }
                thread::sleep(Duration::from_millis(5));
            }
            let accepted_deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let accepted_events = cli(
                    &cx,
                    evidence,
                    "accepted_before_terminal",
                    &["events", session, "--json"],
                    Duration::from_secs(1),
                )?;
                let accepted_page: Value =
                    serde_json::from_slice(&accepted_events).map_err(infra)?;
                if accepted_page["events"].as_array().is_some_and(|events| {
                    events.iter().any(|event| event["type"] == "turn.accepted")
                }) {
                    break;
                }
                if Instant::now() >= accepted_deadline {
                    return Err(ScenarioError::Failure(
                        "acceptance was not durable before terminal".to_owned(),
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
            let waited = cx
                .run(&["wait", session, "--json"], Duration::from_millis(150))
                .map_err(infra)?;
            if !waited.timed_out {
                return Err(ScenarioError::Failure(
                    "wait returned before fake terminal".to_owned(),
                ));
            }
            fs::write(sync.join("wait_disconnected.release"), b"").map_err(infra)?;
            let result = cli(
                &cx,
                evidence,
                "result_after_disconnect",
                &["wait", session, "--json"],
                Duration::from_secs(5),
            )?;
            let envelope: Value = serde_json::from_slice(&result).map_err(infra)?;
            if envelope["state"] != "completed"
                || envelope["final_text"] != "after disconnect"
                || envelope["exit"]["code"] != 0
            {
                return Err(ScenarioError::Failure(
                    "turn did not complete after wait disconnect".to_owned(),
                ));
            }
            let mut envelopes = receipt_bytes;
            envelopes.extend_from_slice(&result);
            evidence
                .write("envelopes.ndjson", &envelopes)
                .map_err(infra)?;
            let events = cli(
                &cx,
                evidence,
                "events",
                &["events", session, "--json"],
                Duration::from_secs(5),
            )?;
            evidence.write("events.ndjson", &events).map_err(infra)?;
            let logs = cli(
                &cx,
                evidence,
                "logs",
                &["logs", session, "--json"],
                Duration::from_secs(5),
            )?;
            evidence.write("logs.ndjson", &logs).map_err(infra)?;
            Ok(())
        },
        |evidence| collect_available(evidence, &state),
    );
    report.require_pass()
}

#[test]
fn s1_cli_auto_starts_daemon_and_keeps_result() -> TestResult {
    let via = Path::new(env!("CARGO_BIN_EXE_via"));
    let fake = fake_binary(via)?;
    let sandbox = tempfile::tempdir()?;
    let state = sandbox.path().join("state");
    let runtime = sandbox.path().join("runtime");
    let sync = sandbox.path().join("sync");
    fs::DirBuilder::new().mode(0o700).create(&sync)?;
    let fixture = sandbox.path().join("fixture.json");
    write_reply_fixture(&fixture)?;
    let evidence = Evidence::new("s1_cli_auto_start", &fake, &fixture)?;
    let cx = Context {
        via,
        state: &state,
        runtime: &runtime,
        fake: &fake,
        fixture: &fixture,
        sync: &sync,
    };
    let report = run_scenario(
        evidence,
        |evidence| {
            let (session, _) = spawn_and_validate(&cx, evidence)?;
            let events = cli(
                &cx,
                evidence,
                "events",
                &["events", &session, "--json"],
                Duration::from_secs(5),
            )?;
            evidence.write("events.ndjson", &events).map_err(infra)?;
            let logs = cli(
                &cx,
                evidence,
                "logs",
                &["logs", &session, "--json"],
                Duration::from_secs(5),
            )?;
            evidence.write("logs.ndjson", &logs).map_err(infra)?;
            evidence.write("daemon.trace", b"").map_err(infra)?;
            Ok(())
        },
        |evidence| {
            let stop = cx
                .run(
                    &["daemon", "stop", "--force", "--json"],
                    Duration::from_secs(2),
                )
                .map_err(infra)?;
            let stop_accepted = !stop.timed_out && stop.status.success();
            let deadline = Instant::now() + Duration::from_secs(2);
            while runtime.join("via.sock").exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            let stopped = !runtime.join("via.sock").exists();
            let anchors = if stopped {
                verify_anchors_after_daemon(&cx).unwrap_or_else(|error| {
                    json!({
                        "status":"unverified","absence_proven":false,"reason":error.to_string()
                    })
                })
            } else {
                json!({"status":"unverified","absence_proven":false,"reason":"daemon socket remained open"})
            };
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(evidence.dir.join("cleanup.json"))
                .map_err(infra)?;
            file.write_all(
                json!({"anchors":anchors,"daemon":{"stop_accepted":stop_accepted,"socket_removed":stopped},"cli_child":"reaped_by_runner"})
                    .to_string()
                    .as_bytes(),
            )
            .map_err(infra)?;
            file.sync_all().map_err(infra)?;
            collect_available(evidence, &state)?;
            if !stop_accepted || !stopped {
                return Err(ScenarioError::Infrastructure(
                    "auto-started daemon did not stop cleanly".to_owned(),
                ));
            }
            Ok(())
        },
    );
    report.require_pass()
}
