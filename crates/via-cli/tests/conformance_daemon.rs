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

/// The default fake capabilities with each `(pointer, value)` replaced.
fn capabilities(changes: &[(&str, Value)]) -> Value {
    let mut capabilities = json!({
        "verbs": {"spawn":{"support":"native"},"resume":{"support":"native"},
                  "steer":{"support":"unsupported","reason":"no steer input"},
                  "cancel":{"support":"native"},"close":{"support":"native"}},
        "params": {"instructions":{"support":"unsupported","reason":"no instructions input"},
                   "output_schema":{"support":"unsupported","reason":"no schema input"},
                   "effort":{"support":"unsupported","reason":"no effort setting"},
                   "max_steps":{"support":"unsupported","reason":"no step limit"}},
        "bounds": [], "network_control": false,
        "recover": {"support":"unsupported","reason":"no recovery"},
        "usage": {"tokens":"turn","cost":"unavailable"}
    });
    for (pointer, value) in changes {
        if let Some(slot) = capabilities.pointer_mut(pointer) {
            *slot = value.clone();
        }
    }
    capabilities
}

fn native() -> Value {
    json!({"support":"native"})
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
        "profile": {"capabilities": capabilities(&[("/params/instructions", native())])},
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

fn vendor_turn(turn: u32) -> String {
    format!("fake-turn-{turn}")
}

fn accepted(turn: u32) -> Value {
    emit(&json!({"type":"accepted","id":1,"vendor_turn_id":vendor_turn(turn)}))
}

fn terminal(turn: u32) -> Value {
    emit(
        &json!({"type":"terminal","vendor_turn_id":vendor_turn(turn),"status":"completed",
                 "final_text":"done","stop_reason":"end_turn"}),
    )
}

/// A completed terminal of `turn` carrying `output` as its structured output.
fn structured(turn: u32, output: &Value) -> Value {
    emit(
        &json!({"type":"terminal","vendor_turn_id":vendor_turn(turn),"status":"completed",
                 "final_text":"done","stop_reason":"end_turn","structured_output":output}),
    )
}

fn gate(name: &str) -> Value {
    json!({"action":"gate","name":name})
}

/// The script run by the start whose prompt is `prompt`.
fn script(prompt: &str, steps: &[Value]) -> Value {
    json!({"expected_request":{"type":"start","prompt":prompt},"steps":steps})
}

/// A script run by a start that carries at least `expected`.
fn script_expecting(expected: &Value, steps: &[Value]) -> Value {
    let mut request = json!({"type":"start"});
    for (member, value) in expected.as_object().into_iter().flatten() {
        request[member] = value.clone();
    }
    json!({"expected_request":request,"steps":steps})
}

/// `params` plus the test handle.
fn with_handle(params: &Value) -> Value {
    let mut params = params.clone();
    params["handle"] = json!(HANDLE);
    params
}

/// A raw spawn that must succeed: its session and receipt.
fn spawned(raw: &mut Raw, id: u64, params: &Value) -> Result<(String, Value), ScenarioError> {
    let receipt = call(raw, id, "spawn", &with_handle(params))?;
    let session = receipt["session_id"]
        .as_str()
        .ok_or_else(|| failure(format!("no session in {receipt}")))?
        .to_owned();
    Ok((session, receipt))
}

/// A raw call that must be refused: its `error.data`, with its code.
fn refusal(
    raw: &mut Raw,
    id: u64,
    method: &str,
    params: &Value,
) -> Result<(i64, Value), ScenarioError> {
    let reply = raw.exchange(&request(id, method, params))?;
    let error = reply
        .get("error")
        .ok_or_else(|| failure(format!("{method} was not refused: {reply}")))?;
    Ok((
        error["code"].as_i64().unwrap_or_default(),
        error["data"].clone(),
    ))
}

/// The value's warnings with `code`.
fn with_code<'a>(value: &'a Value, code: &str) -> Vec<&'a Value> {
    value["warnings"]
        .as_array()
        .map(|warnings| {
            warnings
                .iter()
                .filter(|warning| warning["code"] == code)
                .collect()
        })
        .unwrap_or_default()
}

/// Runs `body` against a daemon on a sandbox with `fixture`, recording its
/// evidence as `name`.
fn scenario_case(
    name: &str,
    fixture: &Value,
    body: impl FnOnce(&Sandbox, &Evidence) -> Result<(), ScenarioError>,
) -> TestResult {
    let sandbox = Sandbox::new(fixture)?;
    let evidence = Evidence::new(name, &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| body(&sandbox, evidence),
        |evidence| collect_available(evidence, &sandbox.state, &sandbox.teardown),
    );
    report.require_pass()
}

/// Sol r1 #17 (4) through the daemon: `require` on a profile whose
/// `cancel` is partial. `describe` lists `missing_capability` for
/// `cancel`, spawn refuses it naming the member, harness and route, and
/// `cancel:partial` is met. A route whose spawn verb is unsupported refuses
/// spawn `unsupported_verb` with `data.verb`.
#[test]
fn conformance_daemon_partial_requirements() -> TestResult {
    let partial = json!({"support":"partial","semantics":"stops_after_tool"});
    let fixture = json!({
        "profile": {"capabilities": capabilities(&[("/verbs/cancel", partial)])},
        "scripts": [script("p", &[accepted(1), terminal(1)])],
    });
    scenario_case(
        "conformance_daemon_partial_requirements",
        &fixture,
        |sandbox, evidence| {
            let daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            let strict = call(
                &mut raw,
                1,
                "describe",
                &json!({"harness":"fake","model":"fake","require":["cancel"]}),
            )?;
            check(
                strict["refusals"]
                    == json!([{"field":"cancel","kind":"missing_capability","route":"fake",
                           "message":strict["refusals"][0]["message"]}]),
                || format!("describe require cancel: {strict}"),
            )?;
            let (_, data) = refusal(
                &mut raw,
                2,
                "spawn",
                &with_handle(&json!({"harness":"fake","model":"fake","prompt":"p",
                                 "require":["cancel"]})),
            )?;
            check(
                data["kind"] == "missing_capability"
                    && data["field"] == "cancel"
                    && data["harness"] == "fake"
                    && data["route"] == "fake",
                || format!("spawn require cancel: {data}"),
            )?;
            let (session, _) = spawned(
                &mut raw,
                3,
                &json!({"harness":"fake","model":"fake","prompt":"p","require":["cancel:partial"]}),
            )?;
            let envelope = wait(&mut raw, 4, &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || {
                format!("cancel:partial: {envelope}")
            })?;
            record(evidence, &mut raw, &session, &[&envelope])?;
            drop(raw);
            daemon.shutdown()?;
            // The same sandbox, now on a route that cannot spawn.
            let unsupported = json!({"support":"unsupported","reason":"no spawn"});
            let fixture = json!({
                "profile": {"capabilities": capabilities(&[("/verbs/spawn", unsupported)])},
                "scripts": [],
            });
            std::fs::write(&sandbox.fixture, fixture.to_string()).map_err(infra)?;
            let _daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            let (code, data) = refusal(
                &mut raw,
                5,
                "spawn",
                &with_handle(&json!({"harness":"fake","model":"fake","prompt":"p"})),
            )?;
            check(
                code == -32006
                    && data["kind"] == "unsupported_verb"
                    && data["verb"] == "spawn"
                    && data["harness"] == "fake"
                    && data["route"] == "fake",
                || format!("unsupported spawn: {data}"),
            )
        },
    )
}

/// Sol r1 #17 (6) AD12 through daemon restarts: v1 → v2 → v3, each
/// declaring only its predecessor compatible. Receipt, envelope and status
/// report the adapter version that ran; a compatible resume advances the
/// session's; a session last run by v1 is refused by v3
/// `harness_unavailable` with `data.reason: "adapter_version"`, naming the
/// harness and route.
#[test]
fn conformance_daemon_adapter_version_chain() -> TestResult {
    let scripts = json!([
        script("a", &[accepted(1), terminal(1)]),
        script("b", &[accepted(1), terminal(1)]),
        script("c", &[accepted(2), terminal(2)]),
        script("d", &[accepted(3), terminal(3)]),
    ]);
    let version = |profile: Value| json!({"profile": profile, "scripts": scripts});
    let v1 = version(json!({"adapter_version":"1"}));
    scenario_case(
        "conformance_daemon_adapter_version_chain",
        &v1,
        |sandbox, evidence| {
            let daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            let (chained, receipt) = spawned(
                &mut raw,
                1,
                &json!({"harness":"fake","model":"fake","prompt":"a"}),
            )?;
            check(receipt["adapter_version"] == "1", || format!("{receipt}"))?;
            let envelope = wait(&mut raw, 2, &format!("{chained}/1"))?;
            check(envelope["adapter_version"] == "1", || format!("{envelope}"))?;
            let (left, _) = spawned(
                &mut raw,
                3,
                &json!({"harness":"fake","model":"fake","prompt":"b"}),
            )?;
            wait(&mut raw, 4, &format!("{left}/1"))?;
            drop(raw);
            daemon.shutdown()?;

            let fixture = version(json!({"adapter_version":"2","compatible":["1"]}));
            std::fs::write(&sandbox.fixture, fixture.to_string()).map_err(infra)?;
            let daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            let status = call(&mut raw, 5, "status", &json!({"session":chained}))?;
            check(status["adapter_version"] == "1", || format!("{status}"))?;
            call(
                &mut raw,
                6,
                "resume",
                &with_handle(&json!({"session":chained,"prompt":"c"})),
            )?;
            let envelope = wait(&mut raw, 7, &format!("{chained}/2"))?;
            check(
                envelope["state"] == "completed" && envelope["adapter_version"] == "2",
                || format!("{envelope}"),
            )?;
            let status = call(&mut raw, 8, "status", &json!({"session":chained}))?;
            check(status["adapter_version"] == "2", || format!("{status}"))?;
            drop(raw);
            daemon.shutdown()?;

            let fixture = version(json!({"adapter_version":"3","compatible":["2"]}));
            std::fs::write(&sandbox.fixture, fixture.to_string()).map_err(infra)?;
            let _daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            call(
                &mut raw,
                9,
                "resume",
                &with_handle(&json!({"session":chained,"prompt":"d"})),
            )?;
            let envelope = wait(&mut raw, 10, &format!("{chained}/3"))?;
            check(envelope["adapter_version"] == "3", || format!("{envelope}"))?;
            let (_, data) = refusal(
                &mut raw,
                11,
                "resume",
                &with_handle(&json!({"session":left,"prompt":"d"})),
            )?;
            check(
                data["kind"] == "harness_unavailable"
                    && data["reason"] == "adapter_version"
                    && data["harness"] == "fake"
                    && data["route"] == "fake",
                || format!("v1 session on v3: {data}"),
            )?;
            let status = call(&mut raw, 12, "status", &json!({"session":left}))?;
            check(status["adapter_version"] == "1", || format!("{status}"))?;
            record(evidence, &mut raw, &chained, &[&envelope])
        },
    )
}

/// Sol r1 #17 (12) C1 P5 through the daemon: every omitted per-turn value
/// inherits from the latest accepted turn, a queued one included; a null
/// `output_schema` or `max_steps` clears; a null `effort` or `bound` is
/// `invalid_params` naming it; each turn starts with its frozen values.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one session's turns, member by member"
)]
fn conformance_daemon_inheritance_and_clearing() -> TestResult {
    let bound = json!({"mode":"full","extra_write_dirs":[],"network":true});
    let schema = json!({"type":"object","properties":{"a":{"type":"integer"}},"required":["a"]});
    let fixture = json!({
        "profile": {
            "capabilities": capabilities(&[
                ("/params/effort", native()),
                ("/params/output_schema", native()),
                ("/params/max_steps", native()),
                ("/bounds", json!(["full"])),
            ]),
            "efforts": ["low", "high"],
        },
        "scripts": [
            script_expecting(
                &json!({"prompt":"one","effort":"low","bound":bound,"output_schema":schema,
                        "max_steps":5}),
                &[accepted(1), gate("one"), structured(1, &json!({"a":1}))],
            ),
            script_expecting(
                &json!({"prompt":"two","effort":"high","bound":bound,"output_schema":schema,
                        "max_steps":5}),
                &[accepted(2), structured(2, &json!({"a":2}))],
            ),
            script_expecting(
                &json!({"prompt":"three","effort":"high","bound":bound}),
                &[accepted(3), terminal(3)],
            ),
        ],
    });
    scenario_case(
        "conformance_daemon_inheritance_and_clearing",
        &fixture,
        |sandbox, evidence| {
            let _daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            let (session, receipt) = spawned(
                &mut raw,
                1,
                &json!({"harness":"fake","model":"fake","prompt":"one","effort":"low",
                    "bound":bound,"output_schema":schema,"max_steps":5,
                    "deadlines":{"wall_ms":100_000}}),
            )?;
            check(
                receipt["effective"]
                    == json!({"model":"fake","effort":"low","bound":bound,
                          "deadlines":{"wall_ms":100_000,"idle_ms":600_000},"max_steps":5}),
                || format!("spawn receipt: {receipt}"),
            )?;
            sandbox.await_gate("one")?;
            for (id, (member, field)) in [(2, ("effort", "effort")), (3, ("bound", "bound"))] {
                let mut params = json!({"session":session,"prompt":"x"});
                params[member] = Value::Null;
                let (_, data) = refusal(&mut raw, id, "resume", &with_handle(&params))?;
                check(
                    data["kind"] == "invalid_params" && data["field"] == field,
                    || format!("null {member}: {data}"),
                )?;
            }
            let two = call(
                &mut raw,
                4,
                "resume",
                &with_handle(&json!({"session":session,"prompt":"two","effort":"high"})),
            )?;
            check(
                two["effective"]
                    == json!({"model":"fake","effort":"high","bound":bound,
                          "deadlines":{"wall_ms":100_000,"idle_ms":600_000},"max_steps":5}),
                || format!("turn 2 receipt: {two}"),
            )?;
            let three = call(
                &mut raw,
                5,
                "resume",
                &with_handle(&json!({"session":session,"prompt":"three",
                                 "output_schema":null,"max_steps":null})),
            )?;
            check(
                three["effective"]
                    == json!({"model":"fake","effort":"high","bound":bound,
                          "deadlines":{"wall_ms":100_000,"idle_ms":600_000},"max_steps":null}),
                || format!("turn 3 receipt: {three}"),
            )?;
            sandbox.release_gate("one")?;
            let first = wait(&mut raw, 6, &format!("{session}/1"))?;
            check(
                first["state"] == "completed"
                    && first["bound"]
                        == json!({"requested":bound,"effective":bound,"inherited":false}),
                || format!("turn 1: {first}"),
            )?;
            let second = wait(&mut raw, 7, &format!("{session}/2"))?;
            check(
                second["state"] == "completed"
                    && second["effort"] == json!({"requested":"high","resolved":"high"})
                    && second["bound"]
                        == json!({"requested":bound,"effective":bound,"inherited":true}),
                || format!("turn 2: {second}"),
            )?;
            let third = wait(&mut raw, 8, &format!("{session}/3"))?;
            check(
                third["state"] == "completed"
                    && with_code(&third, "structured_output_missing").is_empty(),
                || format!("turn 3: {third}"),
            )?;
            record(evidence, &mut raw, &session, &[&first, &second, &third])
        },
    )
}

/// Sol r1 #17 (13) through the daemon: the envelope fields come from the
/// session's plan and the turn's frozen values, and `status` reports the
/// described turn's version with no false warning. After a daemon restart,
/// `result` and `status` read the same back from the Store.
#[test]
fn conformance_daemon_envelope_fields() -> TestResult {
    let bound = json!({"mode":"full","extra_write_dirs":[],"network":true});
    let fixture = json!({
        "profile": {
            "adapter_version": "7.7.7",
            "models": [{"model":"fake-pro","aliases":["pro"]}],
            "capabilities": capabilities(&[
                ("/params/effort", native()),
                ("/bounds", json!(["full"])),
                ("/usage/tokens", json!("session_cumulative")),
            ]),
            "efforts": ["high"],
            "handshake": {"checked": ["1.0"], "requires": []},
        },
        "scripts": [script("p", &[
            json!({"action":"hello","message":{"type":"hello","vendor_version":"1.0",
                   "features":[]}}),
            accepted(1),
            emit(&json!({"type":"usage","vendor_turn_id":vendor_turn(1),
                         "total_tokens":10,"input":9,"output":1,"reasoning_output":0})),
            terminal(1),
        ])],
    });
    let expected = json!({
        "harness":"fake",
        "model":{"requested":"pro","resolved":"fake-pro"},
        "effort":{"requested":"high","resolved":"high"},
        "bound":{"requested":bound,"effective":bound,"inherited":false},
        "route":"fake","adapter_version":"7.7.7",
        "vendor_version":"1.0","version_status":"tested",
        "vendor_options":{"fake":{}},
    });
    let fields = |envelope: &Value| -> Result<(), ScenarioError> {
        for (member, value) in expected.as_object().into_iter().flatten() {
            check(&envelope[member] == value, || {
                format!("{member}: {envelope}")
            })?;
        }
        check(
            envelope["usage"]["scope"] == "session_cumulative"
                && envelope["usage"]["total_tokens"] == 10
                && with_code(envelope, "vendor_version_untested").is_empty(),
            || format!("usage and warnings: {envelope}"),
        )
    };
    let status_fields = |status: &Value| {
        check(
            status["adapter_version"] == "7.7.7"
                && status["vendor_version"] == "1.0"
                && status["version_status"] == "tested"
                && status["model"] == "pro"
                && with_code(status, "vendor_version_untested").is_empty(),
            || format!("status: {status}"),
        )
    };
    scenario_case(
        "conformance_daemon_envelope_fields",
        &fixture,
        |sandbox, evidence| {
            let daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            let (session, _) = spawned(
                &mut raw,
                1,
                &json!({"harness":"fake","model":"pro","prompt":"p","effort":"high",
                    "bound":bound,"vendor":{"fake":{}}}),
            )?;
            let envelope = wait(&mut raw, 2, &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || format!("{envelope}"))?;
            fields(&envelope)?;
            status_fields(&call(&mut raw, 3, "status", &json!({"session":session}))?)?;
            record(evidence, &mut raw, &session, &[&envelope])?;
            drop(raw);
            daemon.shutdown()?;
            let _daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            let read = call(
                &mut raw,
                4,
                "result",
                &json!({"address":format!("{session}/1")}),
            )?;
            fields(&read)?;
            status_fields(&call(&mut raw, 5, "status", &json!({"session":session}))?)
        },
    )
}

/// Sol r1 #17 (15), #16, #1, #2 through the daemon: a schema that does not
/// compile, or declares another draft, is `invalid_params` naming
/// `output_schema`; an invalid value fails a completed turn
/// `structured_output_invalid`, kept; on a failed turn the class stands
/// and the envelope warns `structured_output_invalid` (`reason:
/// "invalid"`); a value whose validation reaches the bound fails
/// `structured_output_invalid` with `reason: "validation_limit"`.
#[test]
fn conformance_daemon_schema_validation() -> TestResult {
    let schema = json!({"type":"object","properties":{"a":{"type":"integer"}},"required":["a"]});
    let failed = emit(&json!({"type":"terminal","vendor_turn_id":vendor_turn(2),
        "status":"failed","final_text":"","stop_reason":"error","vendor_code":"E1",
        "structured_output":{"b":1}}));
    let fixture = json!({
        "profile": {"capabilities": capabilities(&[("/params/output_schema", native())])},
        "scripts": [
            script("bad", &[accepted(1), structured(1, &json!({"b":1}))]),
            script("failed", &[accepted(2), failed]),
            script("limit", &[accepted(3), structured(3, &json!(1))]),
        ],
    });
    let mut defs = serde_json::Map::new();
    defs.insert("d0".into(), json!({"type":"string"}));
    for i in 1..=40 {
        let previous = json!({"$ref": format!("#/$defs/d{}", i - 1)});
        defs.insert(
            format!("d{i}"),
            json!({"anyOf":[previous.clone(), previous]}),
        );
    }
    let doubling = json!({"$ref":"#/$defs/d40","$defs":defs});
    scenario_case(
        "conformance_daemon_schema_validation",
        &fixture,
        |sandbox, evidence| {
            let _daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            let draft07 = json!({"$schema":"http://json-schema.org/draft-07/schema#",
                             "dependentRequired":{"a":["b"]}});
            for (id, refused_schema) in [(1, json!({"type":5})), (2, draft07)] {
                let (_, data) = refusal(
                    &mut raw,
                    id,
                    "spawn",
                    &with_handle(&json!({"harness":"fake","model":"fake","prompt":"bad",
                                     "output_schema":refused_schema})),
                )?;
                check(
                    data["kind"] == "invalid_params" && data["field"] == "output_schema",
                    || format!("{refused_schema}: {data}"),
                )?;
            }
            let (session, _) = spawned(
                &mut raw,
                3,
                &json!({"harness":"fake","model":"fake","prompt":"bad","output_schema":schema}),
            )?;
            let bad = wait(&mut raw, 4, &format!("{session}/1"))?;
            check(
                bad["state"] == "failed"
                    && bad["failure"]["class"] == "structured_output_invalid"
                    && bad["structured_output"] == json!({"b":1}),
                || format!("invalid on completed: {bad}"),
            )?;
            call(
                &mut raw,
                5,
                "resume",
                &with_handle(&json!({"session":session,"prompt":"failed"})),
            )?;
            let failed = wait(&mut raw, 6, &format!("{session}/2"))?;
            let warned = with_code(&failed, "structured_output_invalid");
            check(
                failed["state"] == "failed"
                    && failed["failure"]["class"] != "structured_output_invalid"
                    && failed["structured_output"] == json!({"b":1})
                    && warned.len() == 1
                    && warned[0]["data"] == json!({"reason":"invalid"}),
                || format!("invalid on failed: {failed}"),
            )?;
            call(
                &mut raw,
                7,
                "resume",
                &with_handle(&json!({"session":session,"prompt":"limit","output_schema":doubling})),
            )?;
            let limited = wait(&mut raw, 8, &format!("{session}/3"))?;
            check(
                limited["state"] == "failed"
                    && limited["failure"]["class"] == "structured_output_invalid"
                    && limited["failure"]["data"] == json!({"reason":"validation_limit"})
                    && limited["structured_output"] == json!(1),
                || format!("validation limit: {limited}"),
            )?;
            record(evidence, &mut raw, &session, &[&bad, &failed, &limited])
        },
    )
}

/// Sol r1 #17 (19), (21) AD13, AC7 through the daemon: every category
/// whose effective state is not the requested one is listed in one
/// `config_switch_unverified` warning on the receipts, in `status` and on
/// every envelope; `status` shows the frozen effective states.
#[test]
fn conformance_daemon_config_switch_unverified() -> TestResult {
    let fixture = json!({
        "profile": {"categories": {
            "hooks": {"off": "unverified"},
            "mcp_servers": {"off": "verified"},
            "plugins": {"on": "none"},
            "skills": {"on": "verified"},
            "agents": {"on": "none", "observed": "off"},
        }},
        "scripts": [
            script("one", &[accepted(1), terminal(1)]),
            script("two", &[accepted(2), terminal(2)]),
        ],
    });
    let categories = json!([
        {"category":"hooks","requested":"off","effective":"unknown"},
        {"category":"plugins","requested":"on","effective":"unknown"},
        {"category":"agents","requested":"on","effective":"off"},
    ]);
    let one_warning = |value: &Value| {
        let found = with_code(value, "config_switch_unverified");
        check(
            found.len() == 1 && found[0]["data"]["categories"] == categories,
            || format!("config_switch_unverified: {value}"),
        )
    };
    scenario_case(
        "conformance_daemon_config_switch_unverified",
        &fixture,
        |sandbox, evidence| {
            let _daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            let (session, receipt) = spawned(
                &mut raw,
                1,
                &json!({"harness":"fake","model":"fake","prompt":"one"}),
            )?;
            one_warning(&receipt)?;
            let first = wait(&mut raw, 2, &format!("{session}/1"))?;
            one_warning(&first)?;
            let status = call(&mut raw, 3, "status", &json!({"session":session}))?;
            check(
                status["inherit"]
                    == json!({"hooks":"unknown","mcp_servers":"off","plugins":"unknown",
                          "skills":"on","agents":"off","instruction_files":"on"}),
                || format!("status inherit: {status}"),
            )?;
            one_warning(&status)?;
            let receipt = call(
                &mut raw,
                4,
                "resume",
                &with_handle(&json!({"session":session,"prompt":"two"})),
            )?;
            one_warning(&receipt)?;
            let second = wait(&mut raw, 5, &format!("{session}/2"))?;
            one_warning(&second)?;
            record(evidence, &mut raw, &session, &[&first, &second])
        },
    )
}

/// Sol r1 #17, #10, #11 through the daemon: steer on a route without it is
/// `unsupported_verb` naming the verb, harness and route; the driver's
/// refusals of an admitted steer map to `admission_refused`
/// (`control_lane_full`) and `steer_failed` (-32021) with `reason` and
/// `delivery`; none commits `steer.delivered`.
#[test]
fn conformance_daemon_steer_mappings() -> TestResult {
    let cases = [
        (
            None,
            -32006,
            json!({"kind":"unsupported_verb","verb":"steer","harness":"fake",
                              "route":"fake"}),
        ),
        (
            Some("over_capacity"),
            -32012,
            json!({"kind":"admission_refused",
                                                "reason":"control_lane_full"}),
        ),
        (
            Some("not_steerable"),
            -32021,
            json!({"kind":"steer_failed","reason":"not_steerable",
                                                "delivery":"none"}),
        ),
        (
            Some("not_delivered"),
            -32021,
            json!({"kind":"steer_failed","reason":"not_delivered",
                                                "delivery":"uncertain"}),
        ),
    ];
    for (refusal_decl, code, expected) in cases {
        let mut profile = json!({});
        if let Some(declared) = refusal_decl {
            profile = json!({"capabilities": capabilities(&[("/verbs/steer", native())]),
                             "steer_refusal": declared});
        }
        let fixture = json!({
            "profile": profile,
            "scripts": [script("p", &[accepted(1), gate("running"), terminal(1)])],
        });
        let name = format!(
            "conformance_daemon_steer_mappings_{}",
            refusal_decl.unwrap_or("unsupported")
        );
        scenario_case(&name, &fixture, |sandbox, evidence| {
            let _daemon = Daemon::start(sandbox, evidence)?;
            let mut raw = Raw::open(sandbox)?;
            let (session, _) = spawned(
                &mut raw,
                1,
                &json!({"harness":"fake","model":"fake","prompt":"p"}),
            )?;
            sandbox.await_gate("running")?;
            let (got, data) = refusal(
                &mut raw,
                2,
                "steer",
                &with_handle(&json!({"session":session,"text":"x"})),
            )?;
            check(got == code && data == expected, || {
                format!("steer refusal {refusal_decl:?}: {got} {data}")
            })?;
            sandbox.release_gate("running")?;
            let envelope = wait(&mut raw, 3, &format!("{session}/1"))?;
            let events = call(
                &mut raw,
                4,
                "events",
                &json!({"session":session,"limit":100}),
            )?;
            check(
                envelope["state"] == "completed"
                    && events["events"].as_array().is_some_and(|events| {
                        events
                            .iter()
                            .all(|event| event["type"] != "steer.delivered")
                    }),
                || format!("after the refusal: {envelope} {events}"),
            )?;
            record(evidence, &mut raw, &session, &[&envelope])
        })?;
    }
    Ok(())
}
