//! S-CORE chunk 5 through the real `via` binary and daemon (adapter design
//! §7 S-CORE row): the generic intake over C1 JSON-RPC, on fake scenario
//! profiles (decision H2). The Core halves are via-core's
//! `conformance_intake.rs` and `conformance_core.rs`.

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::time::{Duration, Instant};

use daemon::{Daemon, Raw, Sandbox, TestResult, collect_available, failure, infra, request};
use scenario::{ScenarioError, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn emit(message: &Value) -> Value {
    json!({"action":"emit","message":message})
}

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

/// One raw C1 call's `result`, or a failure carrying the reply.
fn call(raw: &mut Raw, id: u64, method: &str, params: &Value) -> Result<Value, ScenarioError> {
    let reply = raw.exchange(&request(id, method, params))?;
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| failure(format!("{method} refused: {reply}")))
}

/// The terminal envelope of `address`, waited for over `raw`.
fn wait(raw: &mut Raw, id: u64, address: &str) -> Result<Value, ScenarioError> {
    call(
        raw,
        id,
        "wait",
        &json!({"address":address,"timeout_ms":30_000}),
    )
}

/// Writes the run's envelopes and the session's events as evidence.
fn record(
    evidence: &Evidence,
    raw: &mut Raw,
    session: &str,
    envelopes: &[&Value],
) -> Result<(), ScenarioError> {
    let mut lines = String::new();
    for envelope in envelopes {
        lines.push_str(&envelope.to_string());
        lines.push('\n');
    }
    evidence
        .write("envelopes.ndjson", lines.as_bytes())
        .map_err(infra)?;
    let events = call(raw, 90, "events", &json!({"session":session,"limit":1000}))?;
    evidence
        .write("events.ndjson", events["events"].to_string().as_bytes())
        .map_err(infra)
}

/// (18) AD7, end to end through the daemon: a handshake missing a feature
/// VIA relies on fails the turn `submit_failed` with
/// `failure.data.reason: "handshake_refused"` before any start reaches the
/// vendor. The instance's reported version and the evidence folder are
/// kept, and the session's cleanup is proved `quiescent`.
#[test]
fn conformance_daemon_handshake_refused_is_submit_failed() -> TestResult {
    let hello = json!({"action":"hello","message":{"type":"hello","vendor_version":"2.0","features":["turns"]}});
    let sandbox = Sandbox::new(&json!({
        "profile": {"handshake": {"requires": ["turns", "steer"]}},
        "scripts": [{"expected_request":{"type":"start","prompt":"p"},
                     "steps":[hello, {"action":"report_pids"}]}],
    }))?;
    let evidence = Evidence::new(
        "conformance_daemon_handshake_refused",
        &sandbox.fake,
        &sandbox.fixture,
    )?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let mut raw = Raw::open(&sandbox)?;
            let receipt = call(
                &mut raw,
                1,
                "spawn",
                &json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE}),
            )?;
            let session = receipt["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("no session in {receipt}")))?
                .to_owned();
            let envelope = wait(&mut raw, 2, &format!("{session}/1"))?;
            check(
                envelope["state"] == "failed"
                    && envelope["failure"]["class"] == "submit_failed"
                    && envelope["failure"]["data"] == json!({"reason":"handshake_refused"}),
                || format!("not a handshake submit_failed: {envelope}"),
            )?;
            check(envelope["vendor_version"] == "2.0", || {
                format!("the instance's version was not kept: {envelope}")
            })?;
            let folder = envelope["evidence"]["folder"].as_str().unwrap_or_default();
            check(std::path::Path::new(folder).is_dir(), || {
                format!("no evidence folder: {envelope}")
            })?;
            check(!sandbox.sync.join("agent.pid").exists(), || {
                "a start reached the vendor".to_owned()
            })?;
            record(evidence, &mut raw, &session, &[&envelope])?;
            // The agent's group is proved absent once its retirement ends.
            let by = Instant::now() + Duration::from_secs(10);
            loop {
                let status = call(&mut raw, 3, "status", &json!({"session":session}))?;
                if status["process"]["cleanup"] == "quiescent" {
                    break;
                }
                check(Instant::now() < by, || {
                    format!("cleanup never proved: {status}")
                })?;
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state, &sandbox.teardown),
    );
    report.require_pass()
}

/// (10), (11) through the daemon: a spawn naming only a model resolves its
/// harness and the catalogued model; steer on a native profile waits for
/// acceptance, is delivered and commits `steer.delivered`; a steer naming
/// another turn is `turn_mismatch`, and one on an idle session
/// `no_active_turn`.
#[test]
#[expect(clippy::too_many_lines, reason = "one session's steer cases in order")]
fn conformance_daemon_model_only_spawn_and_steer() -> TestResult {
    let mut capabilities = json!({
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
    });
    capabilities["verbs"]["steer"] = json!({"support":"native"});
    let turn = "fake-turn-1";
    let sandbox = Sandbox::new(&json!({
        "profile": {"capabilities": capabilities,
                    "models": [{"model":"fake-pro","aliases":["pro"]}]},
        "scripts": [{"expected_request":{"type":"start","prompt":"p"},
                     "steps":[
                        {"action":"gate","name":"submitting"},
                        emit(&json!({"type":"accepted","id":1,"vendor_turn_id":turn})),
                        {"action":"expect_request","expected":{"type":"steer","id":3}},
                        emit(&json!({"type":"steer_delivered","id":3,"vendor_turn_id":turn})),
                        emit(&json!({"type":"terminal","vendor_turn_id":turn,"status":"completed",
                                     "final_text":"done","stop_reason":"end_turn"})),
                     ]}],
    }))?;
    let evidence = Evidence::new(
        "conformance_daemon_model_only_spawn_and_steer",
        &sandbox.fake,
        &sandbox.fixture,
    )?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let mut raw = Raw::open(&sandbox)?;
            let receipt = call(
                &mut raw,
                1,
                "spawn",
                &json!({"model":"pro","prompt":"p","handle":HANDLE}),
            )?;
            check(
                receipt["route"] == "fake" && receipt["effective"]["model"] == "fake-pro",
                || format!("model-only spawn: {receipt}"),
            )?;
            let session = receipt["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            sandbox.await_gate("submitting")?;
            let mismatch = raw.exchange(&request(
                2,
                "steer",
                &json!({"session":session,"handle":HANDLE,"text":"x","expect_turn":2}),
            ))?;
            check(mismatch["error"]["data"]["kind"] == "turn_mismatch", || {
                format!("steer naming another turn: {mismatch}")
            })?;
            // The steer waits for acceptance on its own connection.
            let reply = std::thread::scope(|scope| {
                let steering = scope.spawn(|| {
                    Raw::open(&sandbox)?.exchange(&request(
                        3,
                        "steer",
                        &json!({"session":session,"handle":HANDLE,"text":"also"}),
                    ))
                });
                std::thread::sleep(Duration::from_millis(200));
                check(!steering.is_finished(), || {
                    "steer did not wait for acceptance".to_owned()
                })?;
                sandbox.release_gate("submitting")?;
                steering
                    .join()
                    .map_err(|_| failure("the steer thread panicked"))?
            })?;
            check(
                reply["result"] == json!({"turn":format!("{session}/1"),"delivery":"injected"}),
                || format!("steer reply: {reply}"),
            )?;
            let envelope = wait(&mut raw, 4, &format!("{session}/1"))?;
            check(
                envelope["state"] == "completed"
                    && envelope["model"] == json!({"requested":"pro","resolved":"fake-pro"}),
                || format!("envelope: {envelope}"),
            )?;
            let events = call(
                &mut raw,
                5,
                "events",
                &json!({"session":session,"limit":100}),
            )?;
            let delivered = events["events"]
                .as_array()
                .map(|events| {
                    events
                        .iter()
                        .filter(|event| event["type"] == "steer.delivered")
                        .count()
                })
                .unwrap_or_default();
            check(delivered == 1, || format!("steer.delivered: {events}"))?;
            record(evidence, &mut raw, &session, &[&envelope])?;
            let idle = raw.exchange(&request(
                6,
                "steer",
                &json!({"session":session,"handle":HANDLE,"text":"idle"}),
            ))?;
            check(idle["error"]["data"]["kind"] == "no_active_turn", || {
                format!("steer on an idle session: {idle}")
            })
        },
        |evidence| collect_available(evidence, &sandbox.state, &sandbox.teardown),
    );
    report.require_pass()
}

/// The default fake capabilities as JSON, with `params.instructions` native.
fn instructions_capabilities() -> Value {
    json!({
        "verbs": {"spawn":{"support":"native"},"resume":{"support":"native"},
                  "steer":{"support":"unsupported","reason":"no steer input"},
                  "cancel":{"support":"native"},"close":{"support":"native"}},
        "params": {"instructions":{"support":"native"},
                   "output_schema":{"support":"unsupported","reason":"no schema input"},
                   "effort":{"support":"unsupported","reason":"no effort setting"},
                   "max_steps":{"support":"unsupported","reason":"no step limit"}},
        "bounds": [], "network_control": false,
        "recover": {"support":"unsupported","reason":"no recovery"},
        "usage": {"tokens":"turn","cost":"unavailable"}
    })
}

/// Sol r1 #4 through the CLI and the daemon: `via spawn --instructions
/// FILE` sends the file's absolute `{path}`, which the daemon reads at the
/// receipt and the route receives as the session's instructions text; a
/// raw `{path}` does the same, and a relative one is `invalid_params`
/// naming `instructions`.
#[test]
fn conformance_daemon_instructions_path() -> TestResult {
    let turn = |n: u32| format!("fake-turn-{n}");
    let script = |prompt: &str, n: u32| {
        json!({"expected_request":{"type":"start","prompt":prompt,"instructions":"be brief"},
               "steps":[emit(&json!({"type":"accepted","id":1,"vendor_turn_id":turn(n)})),
                        emit(&json!({"type":"terminal","vendor_turn_id":turn(n),
                                     "status":"completed","final_text":"done",
                                     "stop_reason":"end_turn"}))]})
    };
    let sandbox = Sandbox::new(&json!({
        "profile": {"capabilities": instructions_capabilities()},
        "scripts": [script("cli", 1), script("raw", 1)],
    }))?;
    let file = sandbox.sync.join("instructions.txt");
    std::fs::write(&file, "be brief")?;
    let file = file.to_str().ok_or("non-UTF-8 sandbox")?.to_owned();
    let evidence = Evidence::new(
        "conformance_daemon_instructions_path",
        &sandbox.fake,
        &sandbox.fixture,
    )?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let receipt = daemon::cli(
                &sandbox,
                evidence,
                "cli_spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "cli",
                    "--instructions",
                    &file,
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let mut raw = Raw::open(&sandbox)?;
            let address = receipt["turn"].as_str().unwrap_or_default().to_owned();
            let envelope = wait(&mut raw, 1, &address)?;
            check(envelope["state"] == "completed", || {
                format!("CLI instructions: {envelope}")
            })?;
            let receipt = call(
                &mut raw,
                2,
                "spawn",
                &json!({"harness":"fake","model":"fake","prompt":"raw","handle":HANDLE,
                        "instructions":{"path":file}}),
            )?;
            let session = receipt["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let envelope = wait(&mut raw, 3, &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || {
                format!("raw instructions: {envelope}")
            })?;
            record(evidence, &mut raw, &session, &[&envelope])?;
            let relative = raw.exchange(&request(
                4,
                "spawn",
                &json!({"harness":"fake","model":"fake","prompt":"x","handle":HANDLE,
                        "instructions":{"path":"instructions.txt"}}),
            ))?;
            check(
                relative["error"]["data"]["kind"] == "invalid_params"
                    && relative["error"]["data"]["field"] == "instructions",
                || format!("relative instructions path: {relative}"),
            )
        },
        |evidence| collect_available(evidence, &sandbox.state, &sandbox.teardown),
    );
    report.require_pass()
}
