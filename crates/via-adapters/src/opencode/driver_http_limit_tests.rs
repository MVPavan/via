//! C2 regressions for `opencode.md` §8–§9: HTTP caps fail only the affected turn.

use super::driver_tests::{Lane, SES, fixture, prompts, replace, route, row, run, success};
use super::serve_tests::Rig;
use crate::{AdapterError, RouteError};
use serde_json::{Value, json};
use std::time::Duration;

// §9: an ordinary HTTP body may retain at most 1 MiB.
const OVER_BODY_LIMIT: usize = 1024 * 1024 + 1;

fn assert_protocol(end: &crate::TurnEnd) {
    assert!(
        matches!(&end.outcome, Err(AdapterError::Route(error))
            if matches!(error.cause, RouteError::Protocol { .. })),
        "affected turn: {end:?}"
    );
}

async fn prompt_limit(status: u16) {
    let rig = Rig::new(&json!({}));
    let cwd = rig.root().to_str().unwrap().to_owned();
    let mut scenario = fixture(&cwd, Vec::new());
    let mut responses = vec![json!({
        "status":status,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},
        "pad_to":OVER_BODY_LIMIT,"emit":success(),"emit_before_response":true,"sleep_ms":100
    })];
    if status == 401 {
        responses.insert(
            0,
            json!({
                "status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},
                "emit":success()
            }),
        );
    }
    replace(
        &mut scenario,
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            &json!(responses),
        ),
    );
    rig.fixture(&scenario);
    row(&rig, 1, "running");
    let mut lane = Lane::open(&rig, false);
    let number = if status == 401 {
        let (warm, _) = lane.turn(1, None, Duration::from_secs(5)).await;
        // Assertions still follow owned process cleanup below.
        row(
            &rig,
            1,
            if warm.outcome.is_ok() {
                "completed"
            } else {
                "unknown"
            },
        );
        row(&rig, 2, "running");
        2
    } else {
        1
    };
    let prepared = lane.driver.prepare();
    let server = match &prepared {
        crate::Prepared::Pinned(pin) => pin
            .opencode
            .as_ref()
            .and_then(via_routes::opencode::ServerPin::live)
            .map(|(server, _)| server),
        crate::Prepared::NeedsConnection => None,
    };
    let (end, _) = lane
        .turn_prepared(number, None, Duration::from_secs(5), prepared)
        .await;
    let generation_protocol = server
        .as_ref()
        .is_some_and(|server| server.failure() == Some(via_routes::codex::LossCause::Protocol));
    let draining = matches!(lane.driver.prepare(), crate::Prepared::NeedsConnection);
    lane.close().await;
    let requests = rig.requests();
    rig.finish().await;
    if status == 401 {
        assert!(
            end.terminal.is_some(),
            "the terminal preceded generation failure"
        );
        assert!(
            generation_protocol,
            "401 still fails the entire generation protocol"
        );
    } else {
        assert_protocol(&end);
    }
    assert!(draining, "the out-of-bounds answer fences this generation");
    assert_eq!(
        prompts(&requests).len(),
        usize::try_from(number).unwrap(),
        "a prompt is never resent"
    );
}

#[test]
fn oc09_c2_prompt_body_limit_overrides_earlier_terminal_and_drains() {
    run(prompt_limit(200));
}

#[test]
fn oc09_c2_unauthorized_body_limit_keeps_generation_protocol_priority() {
    run(prompt_limit(401));
}

#[test]
fn oc09_c2_setup_body_limit_fails_protocol_without_prompt() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        let mut scenario = fixture(&cwd, success());
        replace(
            &mut scenario,
            route(
                "POST",
                "/api/session",
                &json!([{
                    "status":200,"json":{},"pad_to":OVER_BODY_LIMIT
                }]),
            ),
        );
        rig.fixture(&scenario);
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let (end, _) = lane.turn(1, None, Duration::from_secs(5)).await;
        let draining = matches!(lane.driver.prepare(), crate::Prepared::NeedsConnection);
        lane.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert_protocol(&end);
        assert!(draining, "setup cap failure drains its generation");
        assert!(
            prompts(&requests).is_empty(),
            "setup failed before submission"
        );
    });
}

/// §7.4 fixture setup: stop only the input whose delivered execution is observed.
async fn execution_ready(lane: &Lane, by: tokio::time::Instant) -> bool {
    loop {
        if let crate::Prepared::Pinned(pin) = lane.driver.prepare()
            && pin
                .opencode
                .and_then(|pin| pin.live())
                .is_some_and(|(server, _)| {
                    server.routing().state(SES).is_some_and(|state| {
                        state.execution_owner.is_some() && state.pending_requests == 0
                    })
                })
        {
            return true;
        }
        if tokio::time::Instant::now() >= by {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// §8–§9: a native acknowledgement cannot settle an out-of-bounds stop reply.
#[test]
fn oc09_c2_interrupt_body_limit_overrides_native_ack_and_drains() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        let mut running = success();
        running.pop();
        let mut scenario = fixture(&cwd, running);
        replace(
            &mut scenario,
            route(
                "POST",
                &format!("/api/session/{SES}/interrupt"),
                &json!([{
                    "status":200,"json":{"interrupted":true},"pad_to":OVER_BODY_LIMIT,
                    "emit_before_response":true,"sleep_ms":100,"emit":[
                        super::driver_tests::event("session.execution.interrupted",
                            &json!({"sessionID":SES,"reason":"user"}))
                    ]
                }]),
            ),
        );
        rig.fixture(&scenario);
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let inspector = Lane::open(&rig, true);
        let prepared = lane.driver.prepare();
        let capacity = matches!(prepared, crate::Prepared::NeedsConnection)
            .then(|| Box::new(()) as crate::CapacityToken);
        let now = tokio::time::Instant::now();
        let (stop, stop_rx) = tokio::sync::watch::channel(None);
        let (force, force_rx) = tokio::sync::watch::channel(None);
        let context = crate::TurnCx {
            turn: crate::TurnNumber::try_from(1).unwrap(),
            prepared,
            capacity,
            activity: crate::TurnActivity::new(now),
            wall: crate::Deadline::at(now + Duration::from_secs(5)),
            tool_grace: Duration::from_secs(1),
            stop: stop_rx,
            force: force_rx,
            stop_ack: crate::StopAck::new(),
        };
        let active = tokio::spawn(async move {
            let result = lane.turn_context(None, context).await;
            (lane, result)
        });
        let by = now + Duration::from_secs(2);
        let delivered = execution_ready(&inspector, by).await;
        let attached = tokio::time::Instant::now();
        stop.send_replace(Some(crate::StopOrder {
            cause: crate::StopCause::Cancel,
            requested_at: "2026-10-06T00:00:00Z".into(),
            attached,
            force_at: crate::Deadline::at(attached + Duration::from_secs(2)),
            close_by: crate::Deadline::at(attached + Duration::from_secs(2)),
        }));
        let (lane, (end, _)) = active.await.unwrap();
        let draining = matches!(lane.driver.prepare(), crate::Prepared::NeedsConnection);
        lane.close().await;
        inspector.close().await;
        drop((stop, force));
        let requests = rig.requests();
        rig.finish().await;
        assert!(
            delivered,
            "the current input owns its execution before cancellation"
        );
        assert_protocol(&end);
        assert!(
            matches!(&end.outcome, Err(AdapterError::Route(error)) if error.acknowledged),
            "native acknowledgement is independent evidence: {end:?}"
        );
        assert!(
            matches!(
                end.terminal.as_ref().map(|terminal| &terminal.status),
                Some(crate::VendorTerminalStatus::Interrupted)
            ),
            "the raw native acknowledgement remains retained: {end:?}"
        );
        assert!(draining, "the stop's unknown effect drains the generation");
        assert_eq!(
            prompts(&requests).len(),
            1,
            "native stop never resends the prompt"
        );
    });
}

fn late_limit_fixture(cwd: &str) -> Value {
    let mut scenario = fixture(cwd, Vec::new());
    let mut first = success();
    let terminal = first.pop().unwrap();
    first.extend([
        super::driver_tests::event(
            "session.step.started",
            &json!({
                "sessionID":"$SESSION","assistantMessageID":"old_message"
            }),
        ),
        super::driver_tests::event(
            "session.tool.called",
            &json!({
                "sessionID":"$SESSION","id":"old_call",
                "assistantMessageID":"old_message","name":"bash"
            }),
        ),
        super::driver_tests::event(
            "session.tool.success",
            &json!({
                "sessionID":"$SESSION","id":"old_call"
            }),
        ),
        terminal,
    ]);
    let mut second = success();
    let terminal = second.pop().unwrap();
    second.extend([
        super::driver_tests::event(
            "permission.asked",
            &json!({
                "sessionID":"$SESSION","id":"perm_late","action":"bash","resources":["*"],
                "source":{"messageID":"old_message","id":"old_call"}
            }),
        ),
        json!({"pause_ms":300}),
        terminal,
    ]);
    replace(
        &mut scenario,
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            &json!([
                {"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},"emit":first},
                {"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},"emit":second}
            ]),
        ),
    );
    replace(
        &mut scenario,
        route(
            "POST",
            &format!("/api/session/{SES}/permission/perm_late/reply"),
            &json!([{
                "status":204,"json":{},"pad_to":OVER_BODY_LIMIT,
                "emit_before_response":true,"sleep_ms":100,"emit":[
                    super::driver_tests::event("permission.replied",
                        &json!({"sessionID":SES,"requestID":"perm_late"}))
                ]
            }]),
        ),
    );
    scenario
}

/// §9–§11: late HTTP bounds evidence remains an observation of its retained owner.
#[test]
fn oc09_c2_native_settled_tombstone_decline_limit_never_fails_successor() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        rig.fixture(&late_limit_fixture(&cwd));
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let (first, _) = lane.turn(1, None, Duration::from_secs(5)).await;
        row(&rig, 1, "completed");
        row(&rig, 2, "running");
        let (second, observations) = lane.turn(2, None, Duration::from_secs(5)).await;
        let draining = matches!(lane.driver.prepare(), crate::Prepared::NeedsConnection);
        lane.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(first.terminal.is_some(), "predecessor completed");
        assert!(
            second.outcome.is_ok() && second.terminal.is_some(),
            "{second:?}"
        );
        let sent = prompts(&requests);
        assert_eq!(
            sent.len(),
            2,
            "a limit-breaching decline never resends input"
        );
        let old = sent[0]["body"]["id"].as_str().unwrap();
        assert!(
            observations.iter().any(|item| {
                matches!(&item.observation, crate::Observation::Warning(warning)
                if warning.code == "http_response_limit")
                    && item.vendor_turn.as_ref().map(crate::VendorTurnId::as_str) == Some(old)
            }),
            "late bounds warning must identify A: {observations:?}"
        );
        assert!(
            !requests
                .iter()
                .any(|request| { request["target"] == format!("/api/session/{SES}/interrupt") }),
            "a tombstone decline cannot interrupt B"
        );
        assert!(draining, "the unknown decline effect drains the generation");
    });
}
