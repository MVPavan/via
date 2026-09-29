//! S1 Task 2 scenarios through the real `via` binary and daemon: multi-turn
//! sessions, `resume` with `op_key`, spawn retries, the per-session queue and
//! independent sessions (F13, F14, F17, F28), plus C1 `wait.timeout_ms`.

#[path = "support/daemon.rs"]
mod daemon;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::thread;
use std::time::{Duration, Instant};

use daemon::{Daemon, Raw, Sandbox, TestResult, cli, events, failure, infra, refused, request};
use scenario::{ScenarioError, collect_available, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const OTHER_HANDLE: &str = "h_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA";

/// A fake script for one turn, selected by its start request's turn and prompt.
/// With `gate`, the agent holds after acceptance until the gate is released.
fn turn_script(turn: u32, prompt: &str, gate: Option<&str>) -> Value {
    let vendor_turn = format!("fake-turn-{turn}");
    let mut steps = vec![
        json!({"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":vendor_turn}}),
    ];
    if let Some(name) = gate {
        steps.push(json!({"action":"gate","name":name}));
    }
    steps.push(json!({"action":"emit","message":{"type":"text","vendor_turn_id":vendor_turn,"text":format!("{prompt} reply")}}));
    steps.push(json!({"action":"emit","message":{"type":"terminal","vendor_turn_id":vendor_turn,"status":"completed","final_text":format!("{prompt} reply"),"stop_reason":"end_turn"}}));
    json!({"expected_request":{"type":"start","id":1,"turn":turn,"prompt":prompt},"steps":steps})
}

fn fixture(scripts: &[Value]) -> Value {
    json!({"scripts":scripts})
}

fn spawn_params(prompt: &str, handle: &str, key: &str) -> Value {
    json!({"harness":"fake","model":"fake","prompt":prompt,"handle":handle,"idempotency_key":key})
}

fn session_of(receipt: &Value) -> Result<String, ScenarioError> {
    receipt["session_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| failure(format!("receipt has no session: {receipt}")))
}

/// Proves one session's durable history: dense `seq` from 1, every event its
/// own, and per turn exactly one queued, submitted and ended event in order
/// and exactly one agent launch (one committed Host anchor per turn, none
/// for any other). Returns each turn's `(submitted_seq, ended_seq)`.
fn check_history(
    sandbox: &Sandbox,
    session: &str,
    events: &[Value],
    turns: u32,
) -> Result<Vec<(u64, u64)>, ScenarioError> {
    // Session ids are `s_` plus base32 digits: safe to inline in SQL.
    let launches = sandbox.count(&format!(
        "SELECT count(*) FROM anchors WHERE owner_session='{session}'"
    ))?;
    if launches != i64::from(turns) {
        return Err(failure(format!(
            "{session}: {launches} agent launches for {turns} turns"
        )));
    }
    for turn in 1..=turns {
        let per_turn = sandbox.count(&format!(
            "SELECT count(*) FROM anchors WHERE owner_session='{session}' AND owner_turn={turn}"
        ))?;
        if per_turn != 1 {
            return Err(failure(format!(
                "{session}/{turn}: {per_turn} agent launches"
            )));
        }
    }
    for (index, event) in events.iter().enumerate() {
        if event["seq"] != json!(index + 1) || event["session_id"] != session {
            return Err(failure(format!(
                "event {index} is not dense or not {session}'s: {event}"
            )));
        }
    }
    let mut spans = Vec::new();
    for turn in 1..=turns {
        let of = |kind: &str| -> Vec<u64> {
            events
                .iter()
                .filter(|event| event["turn"] == json!(turn) && event["type"] == kind)
                .filter_map(|event| event["seq"].as_u64())
                .collect()
        };
        let (queued, submitted, ended) =
            (of("turn.queued"), of("turn.submitted"), of("turn.ended"));
        if queued.len() != 1 || submitted.len() != 1 || ended.len() != 1 {
            return Err(failure(format!(
                "turn {turn}: queued {queued:?} submitted {submitted:?} ended {ended:?}"
            )));
        }
        if !(queued[0] < submitted[0] && submitted[0] < ended[0]) {
            return Err(failure(format!("turn {turn} lifecycle out of order")));
        }
        spans.push((submitted[0], ended[0]));
    }
    if events.iter().any(|event| {
        event["turn"]
            .as_u64()
            .is_some_and(|turn| turn > u64::from(turns))
    }) {
        return Err(failure(format!("history has more than {turns} turns")));
    }
    Ok(spans)
}

fn wait_completed(
    sandbox: &Sandbox,
    evidence: &Evidence,
    name: &str,
    address: &str,
    text: &str,
) -> Result<Value, ScenarioError> {
    let envelope = cli(sandbox, evidence, name, &["wait", address, "--json"])?;
    if envelope["state"] != "completed" || envelope["final_text"] != text {
        return Err(failure(format!(
            "{address} did not complete with {text}: {envelope}"
        )));
    }
    Ok(envelope)
}

/// Polls a live Store count until it reaches `expected`.
fn await_count(sandbox: &Sandbox, sql: &str, expected: i64) -> Result<(), ScenarioError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if sandbox.count(sql)? == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(failure(format!("{sql} never reached {expected}")));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// F13: a keyed spawn whose reply was lost is retried: same key, handle and
/// params replay the stored receipt and create no second session; any other
/// handle or byte of params is `idempotency_conflict`.
#[test]
fn s1_f13_spawn_retry_after_lost_reply_replays_one_session() -> TestResult {
    let sandbox = Sandbox::new(&fixture(&[turn_script(1, "p1", None)]))?;
    let evidence = Evidence::new("s1_f13_spawn_retry", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let params = spawn_params("p1", HANDLE, "k-13");
            Raw::open(&sandbox)?.send_and_drop(&request(7, "spawn", &params))?;
            await_count(&sandbox, "SELECT count(*) FROM sessions", 1)?;
            let retried = cli(
                &sandbox,
                evidence,
                "spawn_retry",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "p1",
                    "--idempotency-key",
                    "k-13",
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let session = session_of(&retried)?;
            let mut raw = Raw::open(&sandbox)?;
            let replay = raw.exchange(&request(8, "spawn", &params))?;
            let mut expected = retried.clone();
            // The CLI adds the caller's handle to its printed receipt only.
            expected
                .as_object_mut()
                .ok_or_else(|| failure("receipt is not an object"))?
                .remove("handle");
            if replay["result"] != expected {
                return Err(failure(format!("replay differs: {replay} vs {expected}")));
            }
            for (name, line) in [
                (
                    "changed_prompt",
                    request(9, "spawn", &spawn_params("p2", HANDLE, "k-13")),
                ),
                (
                    "other_handle",
                    request(10, "spawn", &spawn_params("p1", OTHER_HANDLE, "k-13")),
                ),
                // Byte-identical params (C1 P4): whitespace alone is another request.
                (
                    "whitespace",
                    request(11, "spawn", &params)
                        .replace("\"prompt\":\"p1\"", "\"prompt\": \"p1\""),
                ),
            ] {
                let refusal = raw.exchange(&line)?;
                evidence
                    .write(&format!("{name}.reply"), refusal.to_string().as_bytes())
                    .map_err(infra)?;
                if refusal["error"]["data"]["kind"] != "invalid_params"
                    || refusal["error"]["data"]["kind2"] != "idempotency_conflict"
                {
                    return Err(failure(format!(
                        "{name}: expected idempotency_conflict: {refusal}"
                    )));
                }
            }
            let envelope = wait_completed(
                &sandbox,
                evidence,
                "wait",
                &format!("{session}/1"),
                "p1 reply",
            )?;
            evidence
                .write(
                    "envelopes.ndjson",
                    format!("{retried}\n{envelope}\n").as_bytes(),
                )
                .map_err(infra)?;
            let history = events(&sandbox, evidence, "events", &session)?;
            evidence
                .write(
                    "events.ndjson",
                    serde_json::to_vec(&history).map_err(infra)?.as_slice(),
                )
                .map_err(infra)?;
            check_history(&sandbox, &session, &history, 1)?;
            if sandbox.count("SELECT count(*) FROM sessions")? != 1
                || sandbox.count("SELECT count(*) FROM turns")? != 1
            {
                return Err(failure("a retried spawn created a second session or turn"));
            }
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// F14: a `resume` whose reply was lost, retried with the same `op_key`, adds
/// exactly one turn; changed params under the key conflict; without a key
/// every retry is a new turn (C1 §3 `op_key`).
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end retry scenario keeps its steps and checks together"
)]
fn s1_f14_resume_retry_with_op_key_adds_one_turn() -> TestResult {
    let sandbox = Sandbox::new(&fixture(&[
        turn_script(1, "p1", None),
        turn_script(2, "p2", None),
        turn_script(3, "p3", None),
        turn_script(4, "p3", None),
    ]))?;
    let evidence = Evidence::new("s1_f14_resume_retry", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let spawn = cli(
                &sandbox,
                evidence,
                "spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "p1",
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let session = session_of(&spawn)?;
            let mut envelopes = vec![wait_completed(
                &sandbox,
                evidence,
                "wait_1",
                &format!("{session}/1"),
                "p1 reply",
            )?];
            let params = json!({"session":session,"handle":HANDLE,"prompt":"p2","op_key":"r-14"});
            Raw::open(&sandbox)?.send_and_drop(&request(7, "resume", &params))?;
            await_count(&sandbox, "SELECT count(*) FROM turns", 2)?;
            let resume = [
                "resume", &session, "--prompt", "p2", "--op-key", "r-14", "--handle", HANDLE,
                "--json",
            ];
            let retried = cli(&sandbox, evidence, "resume_retry", &resume)?;
            if retried["turn"] != format!("{session}/2") {
                return Err(failure(format!("keyed retry is not turn 2: {retried}")));
            }
            envelopes.push(wait_completed(
                &sandbox,
                evidence,
                "wait_2",
                &format!("{session}/2"),
                "p2 reply",
            )?);
            // A retry after the turn ended still replays the original receipt.
            let again = cli(&sandbox, evidence, "resume_again", &resume)?;
            if again != retried {
                return Err(failure(format!(
                    "op_key replay differs: {again} vs {retried}"
                )));
            }
            refused(
                &sandbox,
                evidence,
                "resume_conflict",
                &[
                    "resume", &session, "--prompt", "changed", "--op-key", "r-14", "--handle",
                    HANDLE, "--json",
                ],
                "invalid_params",
            )
            .and_then(|error| {
                if error["data"]["kind2"] == "idempotency_conflict" {
                    Ok(())
                } else {
                    Err(failure(format!("expected idempotency_conflict: {error}")))
                }
            })?;
            refused(
                &sandbox,
                evidence,
                "resume_wrong_handle",
                &[
                    "resume",
                    &session,
                    "--prompt",
                    "p9",
                    "--handle",
                    OTHER_HANDLE,
                    "--json",
                ],
                "invalid_handle",
            )?;
            refused(
                &sandbox,
                evidence,
                "resume_missing_session",
                &[
                    "resume",
                    "s_0000000000zz",
                    "--prompt",
                    "p9",
                    "--handle",
                    HANDLE,
                    "--json",
                ],
                "session_not_found",
            )?;
            // Without a key a retry is a new turn.
            for turn in [3_u32, 4] {
                let unkeyed = cli(
                    &sandbox,
                    evidence,
                    &format!("resume_unkeyed_{turn}"),
                    &[
                        "resume", &session, "--prompt", "p3", "--handle", HANDLE, "--json",
                    ],
                )?;
                if unkeyed["turn"] != format!("{session}/{turn}") || unkeyed["state"] != "queued" {
                    return Err(failure(format!(
                        "unkeyed resume is not turn {turn}: {unkeyed}"
                    )));
                }
                envelopes.push(wait_completed(
                    &sandbox,
                    evidence,
                    &format!("wait_{turn}"),
                    &format!("{session}/{turn}"),
                    "p3 reply",
                )?);
            }
            // A session address waits on the latest turn.
            let latest = cli(
                &sandbox,
                evidence,
                "result_latest",
                &["result", &session, "--json"],
            )?;
            if latest["turn"] != 4 {
                return Err(failure(format!(
                    "session address is not the latest turn: {latest}"
                )));
            }
            let mut lines = String::new();
            for envelope in &envelopes {
                lines.push_str(&envelope.to_string());
                lines.push('\n');
            }
            evidence
                .write("envelopes.ndjson", lines.as_bytes())
                .map_err(infra)?;
            let history = events(&sandbox, evidence, "events", &session)?;
            evidence
                .write(
                    "events.ndjson",
                    serde_json::to_vec(&history).map_err(infra)?.as_slice(),
                )
                .map_err(infra)?;
            check_history(&sandbox, &session, &history, 4)?;
            if sandbox.count("SELECT count(*) FROM turns")? != 4 {
                return Err(failure("a keyed retry created a duplicate turn"));
            }
            for (index, envelope) in envelopes.iter().enumerate() {
                let range = &envelope["events"];
                let first = range["first_seq"].as_u64().unwrap_or(0);
                let last = range["last_seq"].as_u64().unwrap_or(0);
                let own = |seq: u64| history.get(usize::try_from(seq).unwrap_or(0) - 1).cloned();
                if own(first).is_none_or(|event| {
                    event["type"] != "turn.queued" || event["turn"] != json!(index + 1)
                }) || own(last).is_none_or(|event| {
                    event["type"] != "turn.ended" || event["turn"] != json!(index + 1)
                }) {
                    return Err(failure(format!(
                        "turn {} event range is wrong: {range}",
                        index + 1
                    )));
                }
            }
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// F17: one turn runs at a time; eight turns queue behind it in order and the
/// ninth queued turn is `queue_full` (C1 §3.3, §8.1; runtime §8).
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end queue scenario keeps its steps and checks together"
)]
fn s1_f17_ninth_queued_turn_is_queue_full_and_order_kept() -> TestResult {
    let mut scripts = vec![turn_script(1, "q1", Some("hold_1"))];
    for turn in 2..=10 {
        scripts.push(turn_script(turn, &format!("q{turn}"), None));
    }
    let sandbox = Sandbox::new(&fixture(&scripts))?;
    let evidence = Evidence::new("s1_f17_queue_full", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let spawn = cli(
                &sandbox,
                evidence,
                "spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "q1",
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let session = session_of(&spawn)?;
            sandbox.await_gate("hold_1")?;
            let mut receipts = vec![spawn];
            for turn in 2..=9_u32 {
                let prompt = format!("q{turn}");
                let receipt = cli(
                    &sandbox,
                    evidence,
                    &format!("resume_{turn}"),
                    &[
                        "resume", &session, "--prompt", &prompt, "--handle", HANDLE, "--json",
                    ],
                )?;
                if receipt["turn"] != format!("{session}/{turn}")
                    || receipt["state"] != "queued"
                    || receipt["queue_position"] != json!(turn - 2)
                {
                    return Err(failure(format!("turn {turn} receipt: {receipt}")));
                }
                receipts.push(receipt);
            }
            refused(
                &sandbox,
                evidence,
                "resume_ninth_queued",
                &[
                    "resume", &session, "--prompt", "q10", "--handle", HANDLE, "--json",
                ],
                "queue_full",
            )?;
            // Nothing past the running turn was submitted while it held.
            let held = events(&sandbox, evidence, "events_held", &session)?;
            if held
                .iter()
                .any(|event| event["type"] == "turn.submitted" && event["turn"] != 1)
            {
                return Err(failure("a queued turn was submitted while turn 1 ran"));
            }
            sandbox.release_gate("hold_1")?;
            let mut envelopes = Vec::new();
            for turn in 1..=9_u32 {
                envelopes.push(wait_completed(
                    &sandbox,
                    evidence,
                    &format!("wait_{turn}"),
                    &format!("{session}/{turn}"),
                    &format!("q{turn} reply"),
                )?);
            }
            // The refused request took no turn number.
            let tenth = cli(
                &sandbox,
                evidence,
                "resume_after_drain",
                &[
                    "resume", &session, "--prompt", "q10", "--handle", HANDLE, "--json",
                ],
            )?;
            if tenth["turn"] != format!("{session}/10") {
                return Err(failure(format!("turn after queue_full: {tenth}")));
            }
            envelopes.push(wait_completed(
                &sandbox,
                evidence,
                "wait_10",
                &format!("{session}/10"),
                "q10 reply",
            )?);
            let mut lines = String::new();
            for value in receipts.iter().chain(&envelopes) {
                lines.push_str(&value.to_string());
                lines.push('\n');
            }
            evidence
                .write("envelopes.ndjson", lines.as_bytes())
                .map_err(infra)?;
            let history = events(&sandbox, evidence, "events", &session)?;
            evidence
                .write(
                    "events.ndjson",
                    serde_json::to_vec(&history).map_err(infra)?.as_slice(),
                )
                .map_err(infra)?;
            let spans = check_history(&sandbox, &session, &history, 10)?;
            // One at a time, in order: each turn submits after its predecessor ended.
            for pair in spans.windows(2) {
                if pair[1].0 < pair[0].1 {
                    return Err(failure(format!("turns overlapped or reordered: {spans:?}")));
                }
            }
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// F28: two callers drive two sessions at once; neither sees the other's
/// events and each session's events stay dense and ordered.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end concurrency scenario keeps both callers together"
)]
fn s1_f28_two_callers_drive_two_sessions_without_crosstalk() -> TestResult {
    let sandbox = Sandbox::new(&fixture(&[
        turn_script(1, "a1", Some("hold_a")),
        turn_script(2, "a2", None),
        turn_script(1, "b1", Some("hold_b")),
        turn_script(2, "b2", None),
    ]))?;
    let evidence = Evidence::new(
        "s1_f28_independent_sessions",
        &sandbox.fake,
        &sandbox.fixture,
    )?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let caller = |name: &'static str, handle: &'static str| {
                let sandbox = &sandbox;
                move || -> Result<(String, Vec<Value>), ScenarioError> {
                    let spawn = cli(
                        sandbox,
                        evidence,
                        &format!("spawn_{name}"),
                        &[
                            "spawn",
                            "--harness",
                            "fake",
                            "--model",
                            "fake",
                            "--prompt",
                            &format!("{name}1"),
                            "--handle",
                            handle,
                            "--background",
                            "--json",
                        ],
                    )?;
                    let session = session_of(&spawn)?;
                    sandbox.await_gate(&format!("hold_{name}"))?;
                    let resumed = cli(
                        sandbox,
                        evidence,
                        &format!("resume_{name}"),
                        &[
                            "resume",
                            &session,
                            "--prompt",
                            &format!("{name}2"),
                            "--handle",
                            handle,
                            "--json",
                        ],
                    )?;
                    Ok((session, vec![spawn, resumed]))
                }
            };
            let (a, b) = thread::scope(|scope| {
                let a = scope.spawn(caller("a", HANDLE));
                let b = scope.spawn(caller("b", OTHER_HANDLE));
                (a.join(), b.join())
            });
            let (a, a_receipts) = a.map_err(|_| failure("caller a panicked"))??;
            let (b, b_receipts) = b.map_err(|_| failure("caller b panicked"))??;
            if a == b {
                return Err(failure("two spawns share one session"));
            }
            // Both first turns are running at once before either is released.
            // The fake is at its gate once it emitted acceptance; VIA commits
            // `turn.started` from that stdout message a moment later, so poll
            // (bounded) while both gates still hold.
            let running = |session: &str| -> Result<bool, ScenarioError> {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let history = events(
                        &sandbox,
                        evidence,
                        &format!("events_running_{session}"),
                        session,
                    )?;
                    if history.iter().any(|event| event["type"] == "turn.ended") {
                        return Ok(false);
                    }
                    if history.iter().any(|event| event["type"] == "turn.started") {
                        return Ok(true);
                    }
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            };
            if !running(&a)? || !running(&b)? {
                return Err(failure("the two sessions did not run concurrently"));
            }
            // Another session's handle cannot add a turn here.
            refused(
                &sandbox,
                evidence,
                "cross_handle",
                &[
                    "resume",
                    &a,
                    "--prompt",
                    "b9",
                    "--handle",
                    OTHER_HANDLE,
                    "--json",
                ],
                "invalid_handle",
            )?;
            let released = thread::scope(|scope| {
                let waits: Vec<_> = [("a", &a), ("b", &b)]
                    .into_iter()
                    .map(|(name, session)| {
                        let sandbox = &sandbox;
                        scope.spawn(move || -> Result<Vec<Value>, ScenarioError> {
                            sandbox.release_gate(&format!("hold_{name}"))?;
                            let mut envelopes = Vec::new();
                            for turn in 1..=2 {
                                envelopes.push(wait_completed(
                                    sandbox,
                                    evidence,
                                    &format!("wait_{name}_{turn}"),
                                    &format!("{session}/{turn}"),
                                    &format!("{name}{turn} reply"),
                                )?);
                            }
                            Ok(envelopes)
                        })
                    })
                    .collect();
                waits
                    .into_iter()
                    .map(|wait| wait.join().map_err(|_| failure("waiter panicked"))?)
                    .collect::<Result<Vec<_>, _>>()
            })?;
            let mut lines = String::new();
            for value in a_receipts
                .iter()
                .chain(&b_receipts)
                .chain(released.iter().flatten())
            {
                lines.push_str(&value.to_string());
                lines.push('\n');
            }
            evidence
                .write("envelopes.ndjson", lines.as_bytes())
                .map_err(infra)?;
            let mut all = Vec::new();
            for session in [&a, &b] {
                let history = events(&sandbox, evidence, &format!("events_{session}"), session)?;
                check_history(&sandbox, session, &history, 2)?;
                // Each turn's own reply is its envelope's `final_text`
                // (`wait_completed`); model text is not an event (Task 4
                // design §2.1), so each turn is attributed by its one
                // `turn.ended`, and no event carries text of either session.
                for turn in 1..=2_u32 {
                    let ended = history
                        .iter()
                        .filter(|event| {
                            event["type"] == "turn.ended"
                                && event["turn"] == json!(turn)
                                && event["state"] == "completed"
                        })
                        .count();
                    if ended != 1 {
                        return Err(failure(format!(
                            "{session}/{turn} has {ended} completed turn.ended events"
                        )));
                    }
                }
                if history.iter().any(|event| event.get("text").is_some()) {
                    return Err(failure(format!("{session} holds model text in events")));
                }
                all.extend(history);
            }
            evidence
                .write(
                    "events.ndjson",
                    serde_json::to_vec(&all).map_err(infra)?.as_slice(),
                )
                .map_err(infra)?;
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// C1 §3.8: `wait` accepts `timeout_ms` and ends `wait_timeout` when it
/// expires while the turn is still running.
#[test]
fn c1_wait_timeout_ms_bounds_the_wait() -> TestResult {
    let sandbox = Sandbox::new(&fixture(&[turn_script(1, "w1", Some("hold_w"))]))?;
    let evidence = Evidence::new("c1_wait_timeout_ms", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let spawn = cli(
                &sandbox,
                evidence,
                "spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "w1",
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let session = session_of(&spawn)?;
            sandbox.await_gate("hold_w")?;
            let started = Instant::now();
            refused(
                &sandbox,
                evidence,
                "wait_timeout",
                &["wait", &session, "--timeout-ms", "150", "--json"],
                "wait_timeout",
            )?;
            if started.elapsed() > Duration::from_secs(3) {
                return Err(failure(format!(
                    "wait outlived timeout_ms: {:?}",
                    started.elapsed()
                )));
            }
            let mut raw = Raw::open(&sandbox)?;
            let reply = raw.exchange(&request(
                7,
                "wait",
                &json!({"address":session,"timeout_ms":100}),
            ))?;
            if reply["error"]["data"]["kind"] != "wait_timeout" {
                return Err(failure(format!("raw wait with timeout_ms: {reply}")));
            }
            sandbox.release_gate("hold_w")?;
            let envelope = wait_completed(&sandbox, evidence, "wait", &session, "w1 reply")?;
            evidence
                .write(
                    "envelopes.ndjson",
                    format!("{spawn}\n{envelope}\n").as_bytes(),
                )
                .map_err(infra)?;
            let history = events(&sandbox, evidence, "events", &session)?;
            evidence
                .write(
                    "events.ndjson",
                    serde_json::to_vec(&history).map_err(infra)?.as_slice(),
                )
                .map_err(infra)?;
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// The `result` of a raw C1 reply that must have succeeded.
fn succeeded(name: &str, reply: &Value) -> Result<Value, ScenarioError> {
    if reply["result"].is_object() {
        Ok(reply["result"].clone())
    } else {
        Err(failure(format!("{name}: expected a result: {reply}")))
    }
}

/// Checks a raw C1 refusal's canonical `kind`, its `kind2` and the named field.
fn refused_raw(
    name: &str,
    reply: &Value,
    kind: &str,
    kind2: Option<&str>,
    field: &str,
) -> Result<(), ScenarioError> {
    let data = &reply["error"]["data"];
    let message = reply["error"]["message"].as_str().unwrap_or_default();
    if data["kind"] != kind
        || data["kind2"] != json!(kind2)
        || data["field"] != field
        || !message.contains(field)
    {
        return Err(failure(format!(
            "{name}: expected {kind}/{kind2:?} naming {field}: {reply}"
        )));
    }
    // A route refusal also names the route; a session-scope one does not.
    if kind2.is_none() && data["route"] != "fake" {
        return Err(failure(format!("{name}: refusal names no route: {reply}")));
    }
    Ok(())
}

/// Every turn's frozen per-turn values as its Store row holds them, in order.
fn stored_effective(sandbox: &Sandbox, session: &str) -> Result<Vec<Value>, ScenarioError> {
    let store = rusqlite::Connection::open_with_flags(
        sandbox.state.join("store.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(infra)?;
    let mut query = store
        .prepare("SELECT effective FROM turns WHERE session_id=?1 ORDER BY number")
        .map_err(|error| failure(format!("turns have no frozen values: {error}")))?;
    let rows = query
        .query_map([session], |row| row.get::<_, String>(0))
        .map_err(infra)?;
    let mut values = Vec::new();
    for row in rows {
        values.push(serde_json::from_str(&row.map_err(infra)?).map_err(infra)?);
    }
    Ok(values)
}

/// Each turn's `turn.started` effective values, in turn order.
fn started_effective(history: &[Value], turns: u32) -> Vec<Value> {
    (1..=turns)
        .map(|turn| {
            history
                .iter()
                .find(|event| event["type"] == "turn.started" && event["turn"] == json!(turn))
                .map_or(Value::Null, |event| event["effective"].clone())
        })
        .collect()
}

/// The fake route's frozen values with `wall_ms` and the default `idle_ms`
/// (design T3 §5): nothing else is set here.
fn fake_effective(wall_ms: u64) -> Value {
    json!({"model":"fake","effort":null,"bound":null,
        "deadlines":{"wall_ms":wall_ms,"idle_ms":600_000},"max_steps":null})
}

/// C1 §3.3/§4 P5: per-turn values freeze at receipt. Turn 1 sets `wall_ms`
/// A; turn 2, queued behind it, sets B through the CLI flag; turn 3, queued,
/// sets nothing and inherits B from the latest accepted turn. Receipts,
/// Store rows and each turn's `turn.started` carry the frozen values.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end inheritance scenario keeps its steps and checks together"
)]
fn s1_params_queued_turns_inherit_frozen_per_turn_values() -> TestResult {
    const A: u64 = 20_000;
    const B: u64 = 25_000;
    let sandbox = Sandbox::new(&fixture(&[
        turn_script(1, "i1", Some("hold_i")),
        turn_script(2, "i2", None),
        turn_script(3, "i3", None),
    ]))?;
    let evidence = Evidence::new("s1_params_inheritance", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let mut raw = Raw::open(&sandbox)?;
            let spawn = succeeded(
                "spawn",
                &raw.exchange(&request(
                    7,
                    "spawn",
                    &json!({"harness":"fake","model":"fake","prompt":"i1","handle":HANDLE,
                        "deadlines":{"wall_ms":A}}),
                ))?,
            )?;
            let session = session_of(&spawn)?;
            sandbox.await_gate("hold_i")?;
            let second = cli(
                &sandbox,
                evidence,
                "resume_2",
                &[
                    "resume",
                    &session,
                    "--prompt",
                    "i2",
                    "--wall-ms",
                    &B.to_string(),
                    "--handle",
                    HANDLE,
                    "--json",
                ],
            )?;
            let third = succeeded(
                "resume_3",
                &raw.exchange(&request(
                    8,
                    "resume",
                    &json!({"session":session,"handle":HANDLE,"prompt":"i3"}),
                ))?,
            )?;
            let receipts = [&spawn, &second, &third];
            let expected = [fake_effective(A), fake_effective(B), fake_effective(B)];
            for (index, receipt) in receipts.iter().enumerate() {
                if receipt["state"] != "queued" || receipt["effective"] != expected[index] {
                    return Err(failure(format!(
                        "turn {} receipt is not frozen at {}: {receipt}",
                        index + 1,
                        expected[index]
                    )));
                }
            }
            // Frozen at receipt: durable before any of turns 2 and 3 ran.
            let stored = stored_effective(&sandbox, &session)?;
            if stored != expected {
                return Err(failure(format!("Store rows hold {stored:?}")));
            }
            sandbox.release_gate("hold_i")?;
            let mut envelopes = Vec::new();
            for turn in 1..=3_u32 {
                envelopes.push(wait_completed(
                    &sandbox,
                    evidence,
                    &format!("wait_{turn}"),
                    &format!("{session}/{turn}"),
                    &format!("i{turn} reply"),
                )?);
            }
            let mut lines = String::new();
            for value in receipts.into_iter().chain(&envelopes) {
                lines.push_str(&value.to_string());
                lines.push('\n');
            }
            evidence
                .write("envelopes.ndjson", lines.as_bytes())
                .map_err(infra)?;
            let history = events(&sandbox, evidence, "events", &session)?;
            evidence
                .write(
                    "events.ndjson",
                    serde_json::to_vec(&history).map_err(infra)?.as_slice(),
                )
                .map_err(infra)?;
            check_history(&sandbox, &session, &history, 3)?;
            let started = started_effective(&history, 3);
            if started != expected {
                return Err(failure(format!("turn.started effective: {started:?}")));
            }
            // C1 §5 has no deadlines field; its frozen effort and bound show.
            for envelope in &envelopes {
                if envelope["effort"] != json!({"requested":null,"resolved":null})
                    || envelope["bound"]["effective"] != Value::Null
                {
                    return Err(failure(format!("envelope differs from frozen: {envelope}")));
                }
            }
            if stored_effective(&sandbox, &session)? != expected {
                return Err(failure("frozen Store rows changed after the turns ran"));
            }
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// C1 §4 / §8.2: Core drives each turn from its own frozen `wall_ms`. Turn 1
/// hangs under a short one and ends `deadline_wall` well before the fake's
/// default; turn 2 sets a longer one, is still live (`wait_timeout`) past
/// turn 1's budget, and completes once released.
#[test]
fn s1_params_frozen_wall_deadline_applies_to_its_turn_only() -> TestResult {
    let hang = json!({"expected_request":{"type":"start","id":1,"turn":1,"prompt":"d1"},
        "steps":[{"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"hang"}]});
    let sandbox = Sandbox::new(&fixture(&[hang, turn_script(2, "d2", Some("hold_d2"))]))?;
    let evidence = Evidence::new("s1_params_wall_deadline", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let mut raw = Raw::open(&sandbox)?;
            let spawn = succeeded(
                "spawn",
                &raw.exchange(&request(
                    7,
                    "spawn",
                    &json!({"harness":"fake","model":"fake","prompt":"d1","handle":HANDLE,
                        "deadlines":{"wall_ms":1500}}),
                ))?,
            )?;
            let session = session_of(&spawn)?;
            let second = cli(
                &sandbox,
                evidence,
                "resume_2",
                &[
                    "resume",
                    &session,
                    "--prompt",
                    "d2",
                    "--wall-ms",
                    "60000",
                    "--handle",
                    HANDLE,
                    "--json",
                ],
            )?;
            let first = cli(
                &sandbox,
                evidence,
                "wait_1",
                &["wait", &format!("{session}/1"), "--json"],
            )?;
            if first["state"] != "failed"
                || first["failure"]["class"] != "deadline_wall"
                || first["duration_ms"].as_u64().is_none_or(|ms| ms >= 10_000)
            {
                return Err(failure(format!(
                    "turn 1 did not end at its own 1500 ms deadline: {first}"
                )));
            }
            // Turn 2 is still live 2 s after reaching its gate, past turn 1's
            // whole 1500 ms budget: its own 60 s deadline applies.
            sandbox.await_gate("hold_d2")?;
            refused(
                &sandbox,
                evidence,
                "wait_2_held",
                &[
                    "wait",
                    &format!("{session}/2"),
                    "--timeout-ms",
                    "2000",
                    "--json",
                ],
                "wait_timeout",
            )?;
            sandbox.release_gate("hold_d2")?;
            let last = wait_completed(
                &sandbox,
                evidence,
                "wait_2",
                &format!("{session}/2"),
                "d2 reply",
            )?;
            evidence
                .write(
                    "envelopes.ndjson",
                    format!("{spawn}\n{second}\n{first}\n{last}\n").as_bytes(),
                )
                .map_err(infra)?;
            let history = events(&sandbox, evidence, "events", &session)?;
            evidence
                .write(
                    "events.ndjson",
                    serde_json::to_vec(&history).map_err(infra)?.as_slice(),
                )
                .map_err(infra)?;
            check_history(&sandbox, &session, &history, 2)?;
            let started = started_effective(&history, 2);
            if started != [fake_effective(1500), fake_effective(60_000)] {
                return Err(failure(format!("turn.started effective: {started:?}")));
            }
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// C1 §4/§4.2/§8.1 against the fake route's capabilities: each unsupported
/// per-turn value is refused with its canonical kind, naming field and
/// route; session-scope parameters on `resume` are `session_scope_on_resume`.
/// Nullable and empty values are accepted and follow inheritance. A refusal
/// commits nothing.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end refusal table keeps every case together"
)]
fn s1_params_unsupported_values_are_refused_by_name() -> TestResult {
    let sandbox = Sandbox::new(&fixture(&[
        turn_script(1, "r1", None),
        turn_script(1, "n1", None),
        turn_script(2, "n2", None),
    ]))?;
    let evidence = Evidence::new("s1_params_refusals", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let mut raw = Raw::open(&sandbox)?;
            let spawn = succeeded(
                "spawn",
                &raw.exchange(&request(
                    7,
                    "spawn",
                    &json!({"harness":"fake","model":"fake","prompt":"r1","handle":HANDLE}),
                ))?,
            )?;
            let session = session_of(&spawn)?;
            let first = wait_completed(
                &sandbox,
                evidence,
                "wait_r1",
                &format!("{session}/1"),
                "r1 reply",
            )?;
            let per_turn = [
                ("effort", json!("high"), "invalid_params", "effort"),
                (
                    "output_schema",
                    json!({"type":"object"}),
                    "invalid_params",
                    "output_schema",
                ),
                ("max_steps", json!(5), "invalid_params", "max_steps"),
                (
                    "bound",
                    json!({"mode":"full","extra_write_dirs":[],"network":true}),
                    "bound_unsupported",
                    "bound",
                ),
                (
                    "vendor",
                    json!({"fake":{"k":"v"}}),
                    "invalid_params",
                    "vendor",
                ),
            ];
            let spawn_base = json!({"harness":"fake","model":"fake","prompt":"x","handle":HANDLE});
            let resume_base = json!({"session":session,"handle":HANDLE,"prompt":"x"});
            let mut id = 10;
            let mut replies = String::new();
            for (method, base) in [("spawn", &spawn_base), ("resume", &resume_base)] {
                for (member, value, kind, field) in &per_turn {
                    let mut params = base.clone();
                    params[*member] = value.clone();
                    id += 1;
                    let reply = raw.exchange(&request(id, method, &params))?;
                    replies.push_str(&reply.to_string());
                    replies.push('\n');
                    refused_raw(&format!("{method} {member}"), &reply, kind, None, field)?;
                }
            }
            for (member, value) in [
                ("harness", json!("fake")),
                ("model", json!("fake")),
                ("allow_untested", json!(false)),
                ("instructions", json!({"text":"x"})),
                ("cwd", json!("/")),
                ("require", json!([])),
                ("label", json!("l")),
            ] {
                let mut params = resume_base.clone();
                params[member] = value;
                id += 1;
                let reply = raw.exchange(&request(id, "resume", &params))?;
                replies.push_str(&reply.to_string());
                replies.push('\n');
                refused_raw(
                    &format!("resume {member}"),
                    &reply,
                    "invalid_params",
                    Some("session_scope_on_resume"),
                    member,
                )?;
            }
            evidence
                .write("refusals.ndjson", replies.as_bytes())
                .map_err(infra)?;
            if sandbox.count("SELECT count(*) FROM turns")? != 1 {
                return Err(failure("a refused request committed a turn"));
            }
            // Nullable members accept null (`output_schema: null` clears);
            // empty values pass.
            let nulls = json!({"output_schema":null,"max_steps":null,
                "vendor":{},"deadlines":{"wall_ms":null,"idle_ms":null}});
            let mut params = spawn_base.clone();
            params["prompt"] = json!("n1");
            let mut resumed = resume_base.clone();
            resumed["prompt"] = json!("n2");
            for (target, extra) in [(&mut params, &nulls), (&mut resumed, &nulls)] {
                for (member, value) in extra.as_object().into_iter().flatten() {
                    target[member] = value.clone();
                }
            }
            let null_spawn = succeeded(
                "spawn_nulls",
                &raw.exchange(&request(90, "spawn", &params))?,
            )?;
            let null_resume = succeeded(
                "resume_nulls",
                &raw.exchange(&request(91, "resume", &resumed))?,
            )?;
            // Nothing given or inherited: the fake route's default wall deadline.
            for receipt in [&null_spawn, &null_resume] {
                if receipt["effective"] != fake_effective(30_000) {
                    return Err(failure(format!("null values changed effective: {receipt}")));
                }
            }
            let other = session_of(&null_spawn)?;
            let other_first = wait_completed(
                &sandbox,
                evidence,
                "wait_n1",
                &format!("{other}/1"),
                "n1 reply",
            )?;
            let second = wait_completed(
                &sandbox,
                evidence,
                "wait_n2",
                &format!("{session}/2"),
                "n2 reply",
            )?;
            let mut lines = String::new();
            for value in [
                &spawn,
                &first,
                &null_spawn,
                &null_resume,
                &other_first,
                &second,
            ] {
                lines.push_str(&value.to_string());
                lines.push('\n');
            }
            evidence
                .write("envelopes.ndjson", lines.as_bytes())
                .map_err(infra)?;
            let history = events(&sandbox, evidence, "events", &session)?;
            evidence
                .write(
                    "events.ndjson",
                    serde_json::to_vec(&history).map_err(infra)?.as_slice(),
                )
                .map_err(infra)?;
            check_history(&sandbox, &session, &history, 2)?;
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// C1 P4 and §3 `op_key` with per-turn values: a keyed retry replays the
/// identical receipt, `effective` included; changed per-turn values under
/// the key are `idempotency_conflict`. An unkeyed turn after them inherits
/// the keyed turn's frozen value.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end retry scenario keeps its steps and checks together"
)]
fn s1_params_keyed_retry_replays_identical_effective() -> TestResult {
    let sandbox = Sandbox::new(&fixture(&[
        turn_script(1, "k1", None),
        turn_script(2, "k2", None),
        turn_script(3, "k3", None),
    ]))?;
    let evidence = Evidence::new("s1_params_keyed_retry", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let mut raw = Raw::open(&sandbox)?;
            let spawn = json!({"harness":"fake","model":"fake","prompt":"k1","handle":HANDLE,
                "idempotency_key":"k-e","deadlines":{"wall_ms":21_000}});
            let original = succeeded("spawn", &raw.exchange(&request(7, "spawn", &spawn))?)?;
            let session = session_of(&original)?;
            let repeated = succeeded("spawn_retry", &raw.exchange(&request(8, "spawn", &spawn))?)?;
            if repeated != original || original["effective"] != fake_effective(21_000) {
                return Err(failure(format!(
                    "spawn replay differs: {repeated} vs {original}"
                )));
            }
            let mut changed = spawn.clone();
            changed["deadlines"]["wall_ms"] = json!(22_000);
            let reply = raw.exchange(&request(9, "spawn", &changed))?;
            if reply["error"]["data"]["kind2"] != "idempotency_conflict" {
                return Err(failure(format!("changed spawn wall_ms: {reply}")));
            }
            wait_completed(
                &sandbox,
                evidence,
                "wait_1",
                &format!("{session}/1"),
                "k1 reply",
            )?;
            let resume = json!({"session":session,"handle":HANDLE,"prompt":"k2","op_key":"o-e",
                "deadlines":{"wall_ms":23_000}});
            let turn = succeeded("resume", &raw.exchange(&request(10, "resume", &resume))?)?;
            let again = succeeded(
                "resume_retry",
                &raw.exchange(&request(11, "resume", &resume))?,
            )?;
            if again != turn || turn["effective"] != fake_effective(23_000) {
                return Err(failure(format!("resume replay differs: {again} vs {turn}")));
            }
            for (id, name, value) in [
                (12, "wall_ms", json!({"wall_ms":24_000})),
                (13, "omitted", json!({})),
            ] {
                let mut changed = resume.clone();
                changed["deadlines"] = value;
                let reply = raw.exchange(&request(id, "resume", &changed))?;
                if reply["error"]["data"]["kind2"] != "idempotency_conflict" {
                    return Err(failure(format!("changed resume {name}: {reply}")));
                }
            }
            wait_completed(
                &sandbox,
                evidence,
                "wait_2",
                &format!("{session}/2"),
                "k2 reply",
            )?;
            // A replay after the turn ended is still the original receipt.
            let late = succeeded(
                "resume_late_retry",
                &raw.exchange(&request(14, "resume", &resume))?,
            )?;
            let third = succeeded(
                "resume_3",
                &raw.exchange(&request(
                    15,
                    "resume",
                    &json!({"session":session,"handle":HANDLE,"prompt":"k3"}),
                ))?,
            )?;
            if late != turn || third["effective"] != fake_effective(23_000) {
                return Err(failure(format!("late replay {late}; turn 3 {third}")));
            }
            let last = wait_completed(
                &sandbox,
                evidence,
                "wait_3",
                &format!("{session}/3"),
                "k3 reply",
            )?;
            evidence
                .write(
                    "envelopes.ndjson",
                    format!("{original}\n{turn}\n{third}\n{last}\n").as_bytes(),
                )
                .map_err(infra)?;
            let history = events(&sandbox, evidence, "events", &session)?;
            evidence
                .write(
                    "events.ndjson",
                    serde_json::to_vec(&history).map_err(infra)?.as_slice(),
                )
                .map_err(infra)?;
            check_history(&sandbox, &session, &history, 3)?;
            let expected = [
                fake_effective(21_000),
                fake_effective(23_000),
                fake_effective(23_000),
            ];
            if stored_effective(&sandbox, &session)? != expected
                || started_effective(&history, 3) != expected
            {
                return Err(failure("frozen values differ from the receipts"));
            }
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}
