//! Bead via-oq3 (owner, 2026-10-04) through the real `via` binary and
//! daemon: the harness-process pool is `daemon.json`'s `harness_processes.limit`
//! (runtime §8). With 32 slots, 32 turns run their fake agents at once,
//! end to end; the daemon's RSS stays within the memory gate re-sized for
//! 32 slots (Task 4 design §5.1), and every turn's cleanup is proved.

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/rss.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod rss;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use daemon::{Daemon, Raw, Sandbox, TestResult, collect_available, failure, infra, request};
use rss::{GLIBC_ARENAS, limit_kib, processes_of, sampler, status_kib};
use scenario::{ScenarioError, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
/// The configured harness-process slots, and the turns run at once.
const SLOTS: u64 = 32;
/// Model steps each turn reports: a tool round ended by model output.
const STEPS: u64 = 4;
/// The bound on each phase of the scenario.
const PHASE: Duration = Duration::from_secs(60);

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

/// Turn `index`'s script, chosen by its prompt `p<index>`: accepted, then
/// held at gate `g<index>` until every turn runs, then [`STEPS`] tool
/// rounds and a completed terminal.
fn script(index: u64) -> Value {
    let turn = "fake-turn-1";
    let mut round = String::new();
    for message in [
        json!({"type":"tool_started","vendor_turn_id":turn,"tool_id":"t","name":"shell",
            "input_summary":"input"}),
        json!({"type":"tool_ended","vendor_turn_id":turn,"tool_id":"t","status":"completed",
            "output_summary":"output"}),
        json!({"type":"text","vendor_turn_id":turn,"text":"model output"}),
    ] {
        round.push_str(&message.to_string());
        round.push('\n');
    }
    json!({"expected_request":{"type":"start","id":1,"turn":1,"prompt":format!("p{index}")},
    "steps":[
        {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":turn}},
        {"action":"gate","name":format!("g{index}")},
        {"action":"flood","text":round,"count":STEPS},
        {"action":"emit","message":{"type":"terminal","vendor_turn_id":turn,
            "status":"completed","final_text":format!("done {index}"),"stop_reason":"end_turn"}},
    ]})
}

/// Polls `probe` every 20 ms until it returns `Some`, within [`PHASE`].
fn until<T>(
    what: &str,
    mut probe: impl FnMut() -> Result<Option<T>, ScenarioError>,
) -> Result<T, ScenarioError> {
    let deadline = Instant::now() + PHASE;
    loop {
        if let Some(value) = probe()? {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!("{what} within {PHASE:?}")));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Bead via-oq3, owner release requirement: with `harness_processes.limit` 32,
/// 32 per-turn agents run at once (every gate entered while `daemon/status`
/// reports 32 of 32 slots in use); every turn completes; the daemon's peak
/// RSS less its idle baseline stays within 1.25 × the §5.1 sum for 32
/// slots; and every turn's cleanup is certain: `process.cleanup`
/// `quiescent` with no live process, no leftover reported, every slot
/// released with none held unproven, and no fake agent or anchor left.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario: launch, run, complete and clean up 32 turns"
)]
fn s1_slots_32_parallel_turns_complete_within_rss_and_clean_up() -> TestResult {
    let sandbox = Sandbox::new(&json!({"scripts": (0..SLOTS).map(script).collect::<Vec<_>>()}))?;
    let config = sandbox.state.join("daemon.json");
    fs::write(
        &config,
        json!({"harness_processes":{"limit":SLOTS}}).to_string(),
    )?;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600))?;
    let fake = fs::canonicalize(&sandbox.fake)?;
    let via = fs::canonicalize(&sandbox.via)?;
    let evidence = Evidence::new("s1_slots_32_parallel", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let daemon = Daemon::start_with(&sandbox, evidence, |command| {
                if let Some(arenas) = GLIBC_ARENAS {
                    command.env("MALLOC_ARENA_MAX", arenas);
                }
            })?;
            let pid = daemon.pid();
            // Elapsed time only: the daemon settles before its baseline.
            thread::sleep(Duration::from_millis(300));
            let baseline = status_kib(pid, "VmRSS:").ok_or_else(|| infra("no daemon VmRSS"))?;
            let stop = Arc::new(AtomicBool::new(false));
            let sampling = sampler(pid, fake.clone(), via.clone(), Arc::clone(&stop));
            let mut raw = Raw::open(&sandbox)?;
            let mut id = 0;
            let mut next = || {
                id += 1;
                id
            };

            let started = Instant::now();
            let mut sessions = Vec::new();
            for index in 0..SLOTS {
                let receipt = call(
                    &mut raw,
                    next(),
                    "spawn",
                    &json!({"harness":"fake","model":"fake","prompt":format!("p{index}"),
                        "handle":HANDLE}),
                )?;
                let session = receipt["session_id"]
                    .as_str()
                    .ok_or_else(|| failure(format!("no session in {receipt}")))?;
                sessions.push(session.to_owned());
            }
            // Every agent is live and accepted at once, each in its own slot.
            until("every agent entered its gate", || {
                Ok((0..SLOTS)
                    .all(|index| sandbox.sync.join(format!("g{index}.entered")).exists())
                    .then_some(()))
            })?;
            let all_running = started.elapsed();
            let status = call(&mut raw, next(), "daemon/status", &json!({}))?;
            check(
                status["harness_processes"]["limit"] == SLOTS
                    && status["harness_processes"]["in_use"] == SLOTS,
                || {
                    format!(
                        "32 agents without 32 slots in use: {}",
                        status["harness_processes"]
                    )
                },
            )?;
            let agents = processes_of(&fake).len();
            check(agents == 32, || {
                format!("{agents} fake agents alive, not 32")
            })?;
            // Every agent's anchor is live while the agents wait: counted
            // here, not left to the sampler's periodic discovery scans.
            let anchors: Vec<u32> = until("32 live anchors", || {
                let anchors: Vec<u32> = processes_of(&via)
                    .into_iter()
                    .filter(|other| *other != pid && rss::is_anchor(*other))
                    .collect();
                Ok((anchors.len() == 32).then_some(anchors))
            })?;
            let anchor_hwm = anchors
                .iter()
                .filter_map(|anchor| status_kib(*anchor, "VmHWM:"))
                .max()
                .unwrap_or(0);
            for index in 0..SLOTS {
                sandbox.release_gate(&format!("g{index}"))?;
            }

            let mut envelopes = Vec::new();
            for session in &sessions {
                let envelope = until(&format!("{session}/1 ended"), || {
                    let params = json!({"address":format!("{session}/1"),"timeout_ms":5_000});
                    let reply = raw.exchange(&request(next(), "wait", &params))?;
                    if reply["error"]["data"]["kind"] == "wait_timeout" {
                        return Ok(None);
                    }
                    reply
                        .get("result")
                        .cloned()
                        .map(Some)
                        .ok_or_else(|| failure(format!("wait refused: {reply}")))
                })?;
                envelopes.push(envelope);
            }
            let all_ended = started.elapsed();
            // Cleanup is certain for every turn: its group proved absent.
            for session in &sessions {
                until(&format!("{session}'s cleanup proved"), || {
                    let status = call(&mut raw, next(), "status", &json!({"session":session}))?;
                    let process = &status["process"];
                    Ok(
                        (process["cleanup"] == "quiescent" && process["alive"] == false)
                            .then_some(()),
                    )
                })?;
            }
            let released = until("every slot released", || {
                let status = call(&mut raw, next(), "daemon/status", &json!({}))?;
                let processes = status["harness_processes"].clone();
                Ok(
                    (processes["in_use"] == 0 && processes["held_unproven"] == 0)
                        .then_some(processes),
                )
            })?;
            let cleaned = started.elapsed();
            let left = until("no agent or anchor left", || {
                let anchors = processes_of(&via)
                    .into_iter()
                    .filter(|other| *other != pid && rss::is_anchor(*other))
                    .count();
                let agents = processes_of(&fake).len();
                Ok((anchors == 0 && agents == 0).then_some((anchors, agents)))
            })?;
            stop.store(true, Ordering::Release);
            let samples = sampling.join().map_err(|_| infra("the sampler panicked"))?;

            let peak_hwm = status_kib(pid, "VmHWM:").unwrap_or(0);
            let peak_sampled = samples
                .daemon
                .iter()
                .map(|sample| sample.rss_kib)
                .max()
                .unwrap_or(0);
            let peak = peak_hwm.max(peak_sampled);
            let anchor_peak = samples
                .anchors
                .values()
                .copied()
                .max()
                .unwrap_or(0)
                .max(anchor_hwm);
            let metrics = json!({
                "slots": SLOTS, "baseline_kib": baseline, "peak_kib": peak,
                "peak_hwm_kib": peak_hwm, "peak_sampled_kib": peak_sampled,
                "limit_kib": limit_kib(SLOTS), "samples": samples.daemon.len(),
                "anchors": anchors.len(), "anchors_sampled": samples.anchors.len(),
                "anchor_peak_kib": anchor_peak,
                "all_running_ms": all_running.as_millis(), "all_ended_ms": all_ended.as_millis(),
                "cleaned_ms": cleaned.as_millis(), "harness_processes": released,
                "left": {"anchors": left.0, "agents": left.1},
                "malloc_arena_max": GLIBC_ARENAS,
            });
            evidence
                .write("rss.json", metrics.to_string().as_bytes())
                .map_err(infra)?;
            let mut lines = String::new();
            for envelope in &envelopes {
                lines.push_str(&envelope.to_string());
                lines.push('\n');
            }
            evidence
                .write("envelopes.ndjson", lines.as_bytes())
                .map_err(infra)?;
            let mut events = String::new();
            for session in &sessions {
                let page = call(
                    &mut raw,
                    next(),
                    "events",
                    &json!({"session":session,"limit":1000}),
                )?;
                for event in page["events"].as_array().into_iter().flatten() {
                    events.push_str(&event.to_string());
                    events.push('\n');
                }
            }
            evidence
                .write("events.ndjson", events.as_bytes())
                .map_err(infra)?;

            for (index, envelope) in envelopes.iter().enumerate() {
                check(
                    envelope["state"] == "completed"
                        && envelope["final_text"] == format!("done {index}"),
                    || format!("turn {index} did not complete: {envelope}"),
                )?;
                // Any report-only scan found nothing the agent left behind
                // (`null`: no scan ran, C1 §5).
                let leftovers = &envelope["leftovers"];
                check(leftovers.is_null() || leftovers["total"] == 0, || {
                    format!("turn {index} reported leftovers: {}", envelope["leftovers"])
                })?;
            }
            check(peak.saturating_sub(baseline) <= limit_kib(SLOTS), || {
                format!("peak RSS less baseline is over 1.25 × the §5.1 sum: {metrics}")
            })?;
            check(anchor_peak <= 32 * 1024, || {
                format!("anchor RSS: {metrics}")
            })
        },
        |evidence| collect_available(evidence, &sandbox.state, &sandbox.teardown),
    );
    report.require_pass()
}
