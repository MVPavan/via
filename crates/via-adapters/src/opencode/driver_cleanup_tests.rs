//! OC05/OC09: sent cleanup claims and complete setup responses (§7.2, §8, E48).

use super::driver_request_tests::{bootstrap, cancel, controls, requests_to, server, until};
use super::driver_tests::{Lane, SES, event, fixture, prompts, replace, route, row, run, success};
use super::serve_tests::Rig;
use crate::{AdapterError, Deadline, StartRejected, TurnEnd, VendorTerminalStatus};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use via_routes::opencode::{Server, ServerPin, session, turn};

/// §7.2: wall-limited tests exercise min(remaining wall, 30 s), without changing the rule.
const RETRY_WALL: Duration = Duration::from_secs(2);
/// §7.2: allow the real 30 s admission bound to beat the caller's wall.
const MISSING_PROOF_WALL: Duration = Duration::from_secs(35);
/// §8: prove a successor remains pending while the fixture withholds stream evidence.
const PROOF_HOLD: Duration = Duration::from_millis(100);

async fn old_input(rig: &Rig) -> String {
    rig.fixture(&fixture(rig.root().to_str().unwrap(), success()));
    let warm = bootstrap(rig).await;
    warm.close().await;
    row(rig, 1, "unknown");
    prompts(&rig.requests())[0]["body"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn generation(
    rig: &Rig,
    old: &str,
    omitted: bool,
    early_proof: Vec<Value>,
) -> (ServerPin, Arc<Server>) {
    let mut next = fixture(rig.root().to_str().unwrap(), success());
    let gate = (!early_proof.is_empty()).then(|| rig.root().join("release-inbox-listing"));
    replace(
        &mut next,
        route(
            "GET",
            &format!("/api/session/{SES}/inbox"),
            &json!([
                {"status":200,"json":{"data":[{"id":old}]},"emit":early_proof,
                 "emit_before_response":true,"wait_for_file":gate},
                {"status":200,"json":{"data":if omitted {json!([])} else {json!([{"id":old}])}}}
            ]),
        ),
    );
    let proof = event(
        "session.inbox.cancelled",
        &json!({"sessionID":SES,"inboxID":old}),
    );
    replace(
        &mut next,
        route(
            "DELETE",
            &format!("/api/session/{SES}/inbox/{old}"),
            &json!([{"status":204,"emit":if omitted {vec![]} else {vec![proof.clone()]}}]),
        ),
    );
    for (id, events) in [
        ("ses_proof_cancel", vec![proof]),
        (
            "ses_proof_delivered",
            vec![
                event("session.execution.started", &json!({"sessionID":SES})),
                event(
                    "session.inbox.delivered",
                    &json!({"sessionID":SES,"inboxID":old}),
                ),
            ],
        ),
        (
            "ses_proof_terminal",
            vec![event(
                "session.execution.succeeded",
                &json!({"sessionID":SES}),
            )],
        ),
    ] {
        replace(
            &mut next,
            route(
                "GET",
                &format!("/api/session/{id}"),
                &json!([
                    {"status":404,"json":{"_tag":"SessionNotFoundError"},"emit":events}
                ]),
            ),
        );
    }
    replace(
        &mut next,
        route(
            "POST",
            "/api/session/ses_stop_hold*",
            &json!([
                {"status":200,"json":{"interrupted":false},
                 "wait_for_file":rig.root().join("release-stop-pool")}
            ]),
        ),
    );
    rig.fixture(&next);
    let pin = rig.launch().await.unwrap();
    let (live, _) = pin.live().unwrap();
    (pin, live)
}

async fn emit(live: &Server, id: &str) {
    let result = session::get(
        live.http(),
        id,
        Deadline::at(tokio::time::Instant::now() + Duration::from_secs(2)),
    )
    .await;
    assert_eq!(result.unwrap(), None);
}

fn completed(end: &TurnEnd) -> bool {
    end.terminal.as_ref().map(|terminal| terminal.status) == Some(VendorTerminalStatus::Completed)
}

/// §8: withdrawing an unsent stop-pool DELETE must not consume its generation claim.
#[test]
fn oc05_reopen_withdrawn_unsent_delete_is_sent_on_retry() {
    run(async {
        let rig = Rig::new(&json!({}));
        let old = old_input(&rig).await;
        let (pin, live) = generation(&rig, &old, false, vec![]).await;
        let mut holds = JoinSet::new();
        for index in 0..2 {
            let live = Arc::clone(&live);
            holds.spawn(async move {
                turn::interrupt(
                    live.http(),
                    &format!("ses_stop_hold{index}"),
                    Deadline::at(tokio::time::Instant::now() + Duration::from_secs(4)),
                )
                .await
            });
        }
        let full = until(|| {
            rig.requests()
                .iter()
                .filter(|request| {
                    request["target"]
                        .as_str()
                        .is_some_and(|target| target.contains("ses_stop_hold"))
                })
                .count()
                == 2
        })
        .await;
        row(&rig, 2, "running");
        let mut lane = Lane::open(&rig, true);
        let (context, stop) = controls(&lane, 2);
        let active = tokio::spawn(async move {
            let (end, _) = lane.turn_context(None, context).await;
            (lane, end)
        });
        let inbox = format!("/api/session/{SES}/inbox");
        let delete = format!("{inbox}/{old}");
        let waiting = until(|| {
            requests_to(&rig.requests(), &inbox) == 1
                && live
                    .routing()
                    .state(SES)
                    .is_some_and(|state| state.pending_requests > 0)
        })
        .await;
        let unsent = requests_to(&rig.requests(), &delete) == 0;
        cancel(&stop, Duration::from_millis(20));
        let (mut lane, _) = active.await.unwrap();
        row(&rig, 2, "unknown");
        let withdrawn = until(|| {
            live.routing()
                .state(SES)
                .is_some_and(|state| state.pending_requests == 0)
        })
        .await;
        std::fs::write(rig.root().join("release-stop-pool"), b"release").unwrap();
        while let Some(result) = holds.join_next().await {
            result.unwrap().unwrap();
        }
        row(&rig, 3, "running");
        let (end, _) = lane.turn(3, None, RETRY_WALL).await;
        let deletes = requests_to(&rig.requests(), &delete);
        lane.close().await;
        drop(pin);
        rig.finish().await;
        assert!(
            full && waiting && unsent && withdrawn,
            "fixture did not withdraw a pool waiter: full={full}, waiting={waiting}, unsent={unsent}, withdrawn={withdrawn}"
        );
        assert!(completed(&end), "retry failed: {end:?}");
        assert_eq!(
            deletes, 1,
            "one vendor-visible DELETE per input per generation"
        );
    });
}

#[derive(Clone, Copy)]
enum Proof {
    Cancelled,
    Missing,
    Delivered,
}

async fn pending_proof(proof: Proof) {
    let rig = Rig::new(&json!({}));
    let old = old_input(&rig).await;
    let (pin, live) = generation(&rig, &old, true, vec![]).await;
    row(&rig, 2, "running");
    let mut lane = Lane::open(&rig, true);
    let (context, stop) = controls(&lane, 2);
    let active = tokio::spawn(async move {
        let (end, _) = lane.turn_context(None, context).await;
        (lane, end)
    });
    let inbox = format!("/api/session/{SES}/inbox");
    let delete = format!("{inbox}/{old}");
    let settled = until(|| {
        requests_to(&rig.requests(), &delete) == 1
            && live
                .routing()
                .state(SES)
                .is_some_and(|state| state.pending_requests == 0)
    })
    .await;
    cancel(&stop, Duration::from_millis(20));
    let (mut lane, _) = active.await.unwrap();
    row(&rig, 2, "unknown");
    row(&rig, 3, "running");
    let mut retry = tokio::spawn(async move {
        let wall = if matches!(proof, Proof::Missing) {
            MISSING_PROOF_WALL
        } else {
            RETRY_WALL
        };
        let (end, _) = lane.turn(3, None, wall).await;
        (lane, end)
    });
    let omitted = until(|| requests_to(&rig.requests(), &inbox) == 2).await;
    // Timeout consumes no result: the spawned turn remains owned and is joined below.
    let mut early = tokio::time::timeout(PROOF_HOLD, &mut retry).await.ok();
    let held = early.is_none();
    let mut delivered_held = true;
    if held && !matches!(proof, Proof::Missing) {
        emit(
            &live,
            if matches!(proof, Proof::Delivered) {
                "ses_proof_delivered"
            } else {
                "ses_proof_cancel"
            },
        )
        .await;
        if matches!(proof, Proof::Delivered) {
            let running =
                until(|| live.routing().state(SES).is_some_and(|state| state.running)).await;
            // Read-only completion wait; stream execution evidence remains Route-owned.
            early = tokio::time::timeout(PROOF_HOLD, &mut retry).await.ok();
            delivered_held = running && early.is_none();
            emit(&live, "ses_proof_terminal").await;
        }
    }
    let (lane, end) = if let Some(result) = early {
        result.unwrap()
    } else {
        retry.await.unwrap()
    };
    let deletes = requests_to(&rig.requests(), &delete);
    let sent_prompts = prompts(&rig.requests()).len();
    lane.close().await;
    drop(pin);
    rig.finish().await;
    assert!(
        settled && omitted,
        "fixture did not settle DELETE and omit its input on retry"
    );
    assert!(held, "omitted listing bypassed stream proof: {end:?}");
    assert!(
        delivered_held,
        "delivered leftover bypassed the execution rule"
    );
    assert_eq!(deletes, 1, "204 never authorizes another DELETE");
    if matches!(proof, Proof::Missing) {
        assert!(
            matches!(end.outcome, Err(AdapterError::Rejected {
                reason: StartRejected::VendorError(Some(ref code), _), ..
            }) if code.as_str() == "session_busy"),
            "{end:?}"
        );
        assert_eq!(sent_prompts, 1, "only the old generation's warmup prompt");
    } else {
        assert!(
            completed(&end),
            "stream proof did not admit successor: {end:?}"
        );
        assert_eq!(sent_prompts, 2, "exactly one successor, never a resend");
    }
}

#[test]
fn oc05_reopen_omitted_input_waits_for_cancelled_stream_proof() {
    run(pending_proof(Proof::Cancelled));
}

#[test]
fn oc05_reopen_omitted_input_without_proof_is_session_busy() {
    run(pending_proof(Proof::Missing));
}

#[test]
fn oc05_reopen_delivered_leftover_holds_successor_until_execution_end() {
    run(pending_proof(Proof::Delivered));
}

async fn early_stream_proof(delivered: bool) {
    let rig = Rig::new(&json!({}));
    let old = old_input(&rig).await;
    let kind = if delivered {
        "session.inbox.delivered"
    } else {
        "session.inbox.cancelled"
    };
    let mut proof = event(kind, &json!({"sessionID":SES,"inboxID":old}));
    proof["durable"] = json!({"seq":1});
    let (pin, live) = generation(&rig, &old, false, vec![proof]).await;
    row(&rig, 2, "running");
    let mut lane = Lane::open(&rig, true);
    let mut active = tokio::spawn(async move {
        let (end, _) = lane.turn(2, None, RETRY_WALL).await;
        (lane, end)
    });
    let observed = until(|| {
        live.routing()
            .state(SES)
            .is_some_and(|state| state.last_seq == Some(1))
    })
    .await;
    std::fs::write(rig.root().join("release-inbox-listing"), b"release").unwrap();
    // Completion cannot consume Route's earlier stream proof; join the same task below.
    let early = if delivered {
        tokio::time::timeout(PROOF_HOLD, &mut active).await.ok()
    } else {
        None
    };
    let held = early.is_none();
    if delivered {
        emit(&live, "ses_proof_terminal").await;
    }
    let (lane, end) = if let Some(result) = early {
        result.unwrap()
    } else {
        active.await.unwrap()
    };
    let deletes = requests_to(&rig.requests(), &format!("/api/session/{SES}/inbox/{old}"));
    lane.close().await;
    drop(pin);
    rig.finish().await;
    assert!(observed, "stream proof must precede the listing response");
    assert!(held, "a delivered leftover needs its execution terminal");
    assert!(completed(&end), "{end:?}");
    assert_eq!(
        deletes, 0,
        "an already observed cancellation/delivery needs no DELETE"
    );
}

#[test]
fn oc05_reopen_keeps_cancellation_observed_during_listing() {
    run(early_stream_proof(false));
}

#[test]
fn oc05_reopen_keeps_delivery_observed_during_listing() {
    run(early_stream_proof(true));
}

async fn unexpected_success(model_switch: bool) {
    let rig = Rig::new(&json!({}));
    let mut next = fixture(rig.root().to_str().unwrap(), success());
    next["session_variants"] = json!({SES:"default"});
    if model_switch {
        replace(
            &mut next,
            route(
                "POST",
                &format!("/api/session/{SES}/model"),
                &json!([{"status":201,"raw":"<html>untrusted</html>"}]),
            ),
        );
    } else {
        let created = next["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|route| route["path"] == "/api/session")
            .unwrap()["responses"][0]
            .clone();
        replace(
            &mut next,
            route(
                "POST",
                "/api/session",
                &json!([
                    created, {"status":201,"raw":"{ malformed"}
                ]),
            ),
        );
    }
    rig.fixture(&next);
    let mut warm = Some(bootstrap(&rig).await);
    let live = server(warm.as_ref().unwrap()).unwrap();
    row(&rig, 2, "running");
    let mut lane = if model_switch {
        warm.take().unwrap()
    } else {
        Lane::open(&rig, false)
    };
    let (end, _) = lane
        .turn(2, model_switch.then_some("high"), Duration::from_secs(2))
        .await;
    let drained = live.is_draining();
    let sent_prompts = prompts(&rig.requests()).len();
    lane.close().await;
    if let Some(warm) = warm {
        warm.close().await;
    }
    rig.finish().await;
    assert!(
        drained,
        "unexpected 201 must be malformed and drain: {end:?}"
    );
    assert!(end.terminal.is_none());
    assert_eq!(sent_prompts, 1, "no prompt after malformed setup");
}

#[test]
fn oc09_model_switch_201_html_drains_without_prompt() {
    run(unexpected_success(true));
}

#[test]
fn oc09_session_create_201_malformed_drains_without_prompt() {
    run(unexpected_success(false));
}

/// §7.4, §8, §9: both control producers share first-byte claims and typed fate proof.
async fn rejected_prompt_cancel_claim(proof_first: bool, overflow: bool) {
    let rig = Rig::new(&json!({}));
    let mut next = fixture(rig.root().to_str().unwrap(), success());
    let prompt_gate = rig.root().join("release-rejected-prompt");
    let pool_gate = rig.root().join("release-cancel-pool");
    let delete_gate = rig.root().join("release-cancel-response");
    replace(
        &mut next,
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            &json!([
                {"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},
                 "emit":success()},
                {"status":400,"json":{"_tag":"InvalidRequestError"},
                 "wait_for_file":prompt_gate},
                {"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},
                 "emit":success()}
            ]),
        ),
    );
    replace(
        &mut next,
        route(
            "DELETE",
            &format!("/api/session/{SES}/inbox/*"),
            &json!([{"status":204,"wait_for_file":delete_gate}]),
        ),
    );
    replace(
        &mut next,
        route(
            "POST",
            "/api/session/ses_cancel_hold*",
            &json!([{"status":200,"json":{"interrupted":false},"wait_for_file":pool_gate}]),
        ),
    );
    rig.fixture(&next);
    let mut lane = bootstrap(&rig).await;
    let live = server(&lane).unwrap();
    let mut holds = JoinSet::new();
    if proof_first {
        for index in 0..2 {
            let live = Arc::clone(&live);
            holds.spawn(async move {
                turn::interrupt(
                    live.http(),
                    &format!("ses_cancel_hold{index}"),
                    Deadline::at(tokio::time::Instant::now() + Duration::from_secs(4)),
                )
                .await
            });
        }
        assert!(
            until(|| rig
                .requests()
                .iter()
                .filter(|request| {
                    request["target"]
                        .as_str()
                        .is_some_and(|target| target.contains("ses_cancel_hold"))
                })
                .count()
                == 2)
            .await,
            "stop pool did not fill"
        );
    }
    row(&rig, 2, "running");
    let (context, stop) = controls(&lane, 2);
    let active = tokio::spawn(async move {
        let (end, _) = lane.turn_context(None, context).await;
        (lane, end)
    });
    assert!(
        until(|| prompts(&rig.requests()).len() == 2).await,
        "prompt was not sent"
    );
    let input = prompts(&rig.requests())[1]["body"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    if overflow {
        // Exercise the generation-owned overflow pump without coupling this race to
        // observation saturation; the lane-overflow fixtures cover its producer.
        live.routing().enqueue_cleanup(
            SES,
            crate::TurnNumber::try_from(2).unwrap(),
            Deadline::at(tokio::time::Instant::now() + Duration::from_secs(3)),
        );
    } else {
        cancel(&stop, Duration::from_secs(1));
    }
    let delete = format!("/api/session/{SES}/inbox/{input}");
    if proof_first {
        assert!(
            until(|| live
                .routing()
                .state(SES)
                .is_some_and(|state| { state.pending_requests == 2 }))
            .await,
            "DELETE intent did not reach the full stop pool"
        );
        assert_eq!(
            requests_to(&rig.requests(), &delete),
            0,
            "DELETE must still be unsent"
        );
        std::fs::write(&prompt_gate, b"release").unwrap();
        assert!(
            until(|| live.routing().state(SES).is_some_and(|state| {
                state.last.as_ref().is_some_and(|last| {
                    last.phase == via_routes::opencode::state::InputPhase::NotAccepted
                })
            }))
            .await,
            "typed 400 was not classified before DELETE first byte"
        );
        std::fs::write(&pool_gate, b"release").unwrap();
        while let Some(result) = holds.join_next().await {
            result.unwrap().unwrap();
        }
    }
    assert!(
        until(|| requests_to(&rig.requests(), &delete) == 1).await,
        "DELETE was not sent"
    );
    if !proof_first {
        assert!(
            live.routing().state(SES).unwrap().cleanup_pending,
            "claim was not created"
        );
        std::fs::write(&prompt_gate, b"release").unwrap();
    }
    std::fs::write(&delete_gate, b"release").unwrap();
    let (mut lane, end) = active.await.unwrap();
    row(&rig, 2, "unknown");
    let released = until(|| {
        live.routing()
            .state(SES)
            .is_some_and(|state| state.pending_requests == 0)
    })
    .await;
    let unfenced = !live.routing().state(SES).unwrap().cleanup_pending;
    row(&rig, 3, "running");
    let (successor, _) = lane.turn(3, None, RETRY_WALL).await;
    let healthy = !live.is_draining();
    let prompt_count = prompts(&rig.requests()).len();
    let delete_count = requests_to(&rig.requests(), &delete);
    lane.close().await;
    drop(live);
    rig.finish().await;
    assert!(
        released && healthy,
        "requests did not settle normally: {end:?}"
    );
    assert!(
        unfenced,
        "typed non-acceptance left a cancel claim unresolved"
    );
    assert!(completed(&successor), "successor blocked: {successor:?}");
    assert_eq!(prompt_count, 3, "one prompt per turn; never resend");
    assert_eq!(delete_count, 1, "at most one sent DELETE per input");
}

#[test]
fn oc08_live_cancel_claim_before_typed_rejection_releases_successor() {
    run(rejected_prompt_cancel_claim(false, false));
}

#[test]
fn oc08_live_cancel_claim_after_typed_rejection_releases_successor() {
    run(rejected_prompt_cancel_claim(true, false));
}

#[test]
fn oc09_overflow_cancel_claim_before_typed_rejection_releases_successor() {
    run(rejected_prompt_cancel_claim(false, true));
}

#[test]
fn oc09_overflow_cancel_claim_after_typed_rejection_releases_successor() {
    run(rejected_prompt_cancel_claim(true, true));
}
