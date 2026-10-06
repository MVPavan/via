//! C2 fixtures for `opencode.md` §8: drain readiness and captured pins.

use super::driver_tests::{
    Lane, SES, event, fixture, full, prompts, replace, route, row, run, success,
};
use super::serve_tests::Rig;
use crate::{
    AdapterError, Deadline, Prepared, StartRejected, StopAck, StopCause, StopOrder, TurnActivity,
    TurnCx, TurnEnd, TurnNumber, TurnSpec, VendorTerminalStatus,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinSet;

async fn until(mut predicate: impl FnMut() -> bool, bound: Duration) -> bool {
    let by = tokio::time::Instant::now() + bound;
    loop {
        if predicate() {
            return true;
        }
        if tokio::time::Instant::now() >= by {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[derive(Clone, Copy)]
enum PromptFault {
    Conflict,
    Truncated,
    Timeout,
}

fn drain_fixture(cwd: &str, accepted: bool, fault: PromptFault) -> Value {
    let mut output = fixture(cwd, Vec::new());
    let mut events = Vec::new();
    if accepted {
        events = success();
        let terminal = events.pop().unwrap();
        let pause_ms = if matches!(fault, PromptFault::Timeout) {
            31_000
        } else {
            2000
        };
        events.extend([json!({"pause_ms":pause_ms}), terminal]);
    }
    let mut response = json!({
        "status":409,"json":{},"emit_before_response":true,
        "sleep_ms":200,"emit":events
    });
    match fault {
        PromptFault::Conflict => {}
        PromptFault::Truncated => {
            response["status"] = json!(200);
            response["declared_length"] = json!(1000);
        }
        PromptFault::Timeout => {
            response["status"] = json!(200);
            response["json"] = json!({"data":{"id":"$INPUT","sessionID":"$SESSION"}});
            // §8's 30-second response timeout expires before the 40-second wall.
            response["sleep_ms"] = json!(31_000);
        }
    }
    replace(
        &mut output,
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            &json!([response]),
        ),
    );
    output
}

/// Keep the first driver's idle lease and a previously captured pin alive:
/// §8 retirement must depend on sent turns, not those holders.
async fn collect_drain(accepted: bool, fault: PromptFault) -> DrainResult {
    let rig = Rig::new(&json!({}));
    let cwd = rig.root().to_str().unwrap().to_owned();
    rig.fixture(&drain_fixture(&cwd, accepted, fault));
    row(&rig, 1, "running");
    let mut first = Lane::open(&rig, false);
    let timeout = matches!(fault, PromptFault::Timeout);
    let wall = Duration::from_secs(if timeout { 40 } else { 5 });
    let active = tokio::spawn(async move {
        let (end, observations) = first.turn(1, None, wall).await;
        (first, end, observations)
    });
    let prompt_seen = until(
        || !prompts(&rig.requests()).is_empty(),
        Duration::from_secs(3),
    )
    .await;
    let mut unsent = Lane::open(&rig, true);
    let captured_pin = unsent.driver.prepare();
    let held_pin = unsent.driver.prepare();
    let was_pinned = matches!(captured_pin, Prepared::Pinned(_));
    let readiness = unsent.driver.readiness().unwrap();
    let epoch = *readiness.borrow();
    let draining = until(
        || matches!(unsent.driver.prepare(), Prepared::NeedsConnection),
        Duration::from_secs(if timeout { 32 } else { 1 }),
    )
    .await;
    let readiness_changed = *readiness.borrow() != epoch;
    // A stale pin must reject before linking any turn; no second running
    // Store row can coexist with A on this synthetic VIA session.
    let (pinned_end, _) = unsent
        .turn_prepared(2, None, Duration::from_millis(200), captured_pin)
        .await;
    let (mut first, end, _) = active.await.unwrap();
    row(
        &rig,
        1,
        if end.terminal.is_some() {
            "completed"
        } else {
            "unknown"
        },
    );
    let vendor_pid = prompts(&rig.requests())[0]["pid"].as_u64().unwrap();
    let retired = until(
        || !std::path::Path::new(&format!("/proc/{vendor_pid}")).exists(),
        Duration::from_secs(3),
    )
    .await;
    drop(held_pin);
    rig.fixture(&fixture(&cwd, success()));
    row(&rig, 3, "running");
    let (fresh, _) = first.turn(3, None, Duration::from_secs(5)).await;
    first.close().await;
    unsent.close().await;
    let requests = rig.requests();
    rig.finish().await;
    DrainResult {
        prompt_seen,
        was_pinned,
        draining,
        readiness_changed,
        pinned_end,
        end,
        retired,
        fresh,
        requests,
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "Each bool records an independent observed fixture fact, not a state transition."
)]
struct DrainResult {
    prompt_seen: bool,
    was_pinned: bool,
    draining: bool,
    readiness_changed: bool,
    pinned_end: TurnEnd,
    end: TurnEnd,
    retired: bool,
    fresh: TurnEnd,
    requests: Vec<Value>,
}

async fn ambiguous_prompt_drains(accepted: bool, fault: PromptFault) {
    let DrainResult {
        prompt_seen,
        was_pinned,
        draining,
        readiness_changed,
        pinned_end,
        end,
        retired,
        fresh,
        requests,
    } = collect_drain(accepted, fault).await;
    assert!(
        prompt_seen && was_pinned,
        "the old generation was pinned before the reply"
    );
    assert!(
        draining,
        "an ambiguous sent prompt publishes NeedsConnection"
    );
    assert!(
        readiness_changed,
        "drain publishes a readiness epoch change"
    );
    assert!(
        matches!(
            pinned_end.outcome,
            Err(AdapterError::Rejected {
                reason: StartRejected::SessionGone,
                ..
            })
        ),
        "pinned unsent: {pinned_end:?}"
    );
    assert_eq!(
        end.terminal.is_some(),
        accepted,
        "ambiguous prompt: {end:?}"
    );
    if accepted {
        assert!(
            matches!(
                end.terminal.as_ref().map(|t| &t.status),
                Some(VendorTerminalStatus::Completed)
            ),
            "accepted prompt keeps running: {end:?}"
        );
    } else {
        assert!(
            end.outcome.is_err(),
            "no acceptance remains unknown: {end:?}"
        );
    }
    assert!(
        retired,
        "drain retires even with the original idle lease held"
    );
    assert!(fresh.terminal.is_some(), "fresh generation: {fresh:?}");
    assert_eq!(
        prompts(&requests).len(),
        2,
        "never resend or submit the stale pin"
    );
    assert_eq!(
        requests
            .iter()
            .map(|request| request["pid"].as_u64().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        2,
        "successor uses a fresh vendor generation"
    );
}

#[test]
fn oc09_c2_409_after_acceptance_drains_without_ending_owned_execution() {
    run(ambiguous_prompt_drains(true, PromptFault::Conflict));
}

#[test]
fn oc09_c2_socket_failure_after_acceptance_drains_without_ending_owned_execution() {
    run(ambiguous_prompt_drains(true, PromptFault::Truncated));
}

#[test]
fn oc09_c2_409_without_acceptance_is_unknown_and_never_resent() {
    run(ambiguous_prompt_drains(false, PromptFault::Conflict));
}

#[test]
fn oc09_c2_prompt_response_timeout_drains_before_wall_and_keeps_owned_terminal() {
    run(ambiguous_prompt_drains(true, PromptFault::Timeout));
}

#[test]
fn oc04_c2_idle_retirement_reopens_identity_once_without_resending() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        rig.fixture(&fixture(&cwd, success()));
        row(&rig, 1, "running");
        let mut first = Lane::open(&rig, false);
        let (initial, _) = first.turn(1, None, Duration::from_secs(5)).await;
        first.close().await;
        row(&rig, 1, "completed");
        let old_pid = prompts(&rig.requests())[0]["pid"].as_u64().unwrap();
        let retired = until(
            || !std::path::Path::new(&format!("/proc/{old_pid}")).exists(),
            Duration::from_secs(3),
        )
        .await;
        row(&rig, 2, "running");
        let mut reopened = Lane::open(&rig, true);
        let (next, _) = reopened.turn(2, None, Duration::from_secs(5)).await;
        reopened.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(initial.terminal.is_some() && next.terminal.is_some());
        assert!(
            retired,
            "closing the last idle driver retires its generation"
        );
        assert_eq!(prompts(&requests).len(), 2, "each input is submitted once");
        assert_eq!(
            requests
                .iter()
                .map(|request| request["pid"].as_u64().unwrap())
                .collect::<BTreeSet<_>>()
                .len(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "GET"
                    && request["target"] == format!("/api/session/{SES}"))
                .count(),
            1,
            "new generation reopens identity exactly once"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "GET"
                    && request["target"] == format!("/api/session/{SES}/inbox"))
                .count(),
            1,
            "new generation performs leftover cleanup once"
        );
    });
}

fn pending_interrupt_fixture(cwd: &str) -> Value {
    let mut output = fixture(cwd, Vec::new());
    let mut running = success();
    running.pop();
    replace(
        &mut output,
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            &json!([
                {"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},"emit":running},
                {"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},"emit":success()}
            ]),
        ),
    );
    replace(
        &mut output,
        route(
            "POST",
            &format!("/api/session/{SES}/interrupt"),
            &json!([
                {"status":200,"json":{"interrupted":true},"sleep_ms":800,"emit_before_response":true,
                    "emit":[{"pause_ms":350},event("session.execution.interrupted",
                        &json!({"sessionID":SES,"reason":"user"}))]}
            ]),
        ),
    );
    output
}

fn owns_execution(lane: &Lane) -> bool {
    let Prepared::Pinned(pin) = lane.driver.prepare() else {
        return false;
    };
    pin.opencode
        .and_then(|pin| pin.live())
        .is_some_and(|(server, _)| {
            server
                .routing()
                .state(SES)
                .is_some_and(|state| state.execution_owner.is_some() && state.pending_requests == 0)
        })
}

/// §7.4's stream acknowledgement cannot clear §7.2's pending stop request.
#[test]
fn oc05_c2_interrupt_reply_in_flight_fences_successor() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        rig.fixture(&pending_interrupt_fixture(&cwd));
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let inspector = Lane::open(&rig, true);
        let prepared = lane.driver.prepare();
        let capacity = matches!(prepared, Prepared::NeedsConnection)
            .then(|| Box::new(()) as crate::CapacityToken);
        let now = tokio::time::Instant::now();
        let (stop, stop_rx) = watch::channel(None);
        let (force, force_rx) = watch::channel(None);
        let context = TurnCx {
            turn: TurnNumber::try_from(1).unwrap(),
            prepared,
            capacity,
            activity: TurnActivity::new(now),
            wall: Deadline::at(now + Duration::from_secs(5)),
            tool_grace: Duration::from_secs(1),
            stop: stop_rx,
            force: force_rx,
            stop_ack: StopAck::new(),
        };
        let active = tokio::spawn(async move {
            let result = lane.turn_context(None, context).await;
            (lane, result)
        });
        let delivered = until(|| owns_execution(&inspector), Duration::from_secs(2)).await;
        let attached = tokio::time::Instant::now();
        stop.send_replace(Some(StopOrder {
            cause: StopCause::Cancel,
            requested_at: "2026-10-06T00:00:00Z".into(),
            attached,
            force_at: Deadline::at(attached + Duration::from_secs(2)),
            close_by: Deadline::at(attached + Duration::from_secs(2)),
        }));
        let (mut lane, (acknowledged, _)) = active.await.unwrap();
        row(&rig, 1, "cancelled");
        row(&rig, 2, "running");
        let (next, seen) = lane.turn(2, None, Duration::from_secs(2)).await;
        drop((stop, force));
        lane.close().await;
        inspector.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(delivered, "A owns an execution before the stop");
        assert!(
            matches!(
                acknowledged
                    .terminal
                    .as_ref()
                    .map(|terminal| &terminal.status),
                Some(VendorTerminalStatus::Interrupted)
            ),
            "A: {acknowledged:?}"
        );
        assert!(
            next.terminal.is_some(),
            "B: {next:?}; requests: {requests:?}"
        );
        let interrupts: Vec<_> = requests
            .iter()
            .filter(|request| {
                request["method"] == "POST"
                    && request["target"] == format!("/api/session/{SES}/interrupt")
            })
            .collect();
        assert_eq!(interrupts.len(), 1, "one stop request per turn");
        let sent = prompts(&requests);
        assert_eq!(sent.len(), 2, "stopped predecessor is never resent");
        assert!(
            sent[1]["received_ms"].as_u64().unwrap()
                >= interrupts[0]["received_ms"].as_u64().unwrap() + 750,
            "B must wait for the complete interrupt reply: {requests:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|item| matches!(item.observation, crate::Observation::LateTerminal(_))),
            "A's retained acknowledgement never becomes B's late terminal"
        );
    });
}

fn late_decline_fixture(cwd: &str) -> Value {
    let mut output = fixture(cwd, Vec::new());
    let mut first = success();
    let terminal = first.pop().unwrap();
    first.extend([
        event(
            "session.step.started",
            &json!({"sessionID":"$SESSION",
            "assistantMessageID":"old_message"}),
        ),
        event(
            "session.tool.called",
            &json!({"sessionID":"$SESSION","id":"old_call",
            "assistantMessageID":"old_message","name":"bash"}),
        ),
        event(
            "session.tool.success",
            &json!({"sessionID":"$SESSION","id":"old_call"}),
        ),
        terminal,
    ]);
    let mut second = success();
    let terminal = second.pop().unwrap();
    second.extend([
        event(
            "permission.asked",
            &json!({"sessionID":"$SESSION","id":"perm_late",
            "action":"bash","resources":["*"],
            "source":{"messageID":"old_message","id":"old_call"}}),
        ),
        json!({"pause_ms":400}),
        terminal,
    ]);
    replace(
        &mut output,
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
        &mut output,
        route(
            "POST",
            &format!("/api/session/{SES}/permission/perm_late/reply"),
            &json!([{"status":500,"json":{}}]),
        ),
    );
    output
}

/// §11: a failed tombstone decline drains without interrupting its successor.
#[test]
fn oc07_c2_failed_late_decline_keeps_tombstone_owner_and_successor_terminal() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        rig.fixture(&late_decline_fixture(&cwd));
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let (first, _) = lane.turn(1, None, Duration::from_secs(5)).await;
        row(&rig, 1, "completed");
        row(&rig, 2, "running");
        let (second, observations) = lane.turn(2, None, Duration::from_secs(5)).await;
        let retired = until(
            || matches!(lane.driver.prepare(), Prepared::NeedsConnection),
            Duration::from_secs(1),
        )
        .await;
        lane.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(
            matches!(
                first.terminal.as_ref().map(|terminal| &terminal.status),
                Some(VendorTerminalStatus::Completed)
            ),
            "first: {first:?}"
        );
        assert!(
            matches!(
                second.terminal.as_ref().map(|terminal| &terminal.status),
                Some(VendorTerminalStatus::Completed)
            ),
            "successor: {second:?}"
        );
        let declines: Vec<_> = requests
            .iter()
            .filter(|request| {
                request["target"] == format!("/api/session/{SES}/permission/perm_late/reply")
            })
            .collect();
        assert_eq!(
            declines.len(),
            1,
            "failed late request is still declined once"
        );
        assert_eq!(declines[0]["body"], json!({"decision":"reject"}));
        assert!(
            !requests
                .iter()
                .any(|request| request["target"] == format!("/api/session/{SES}/interrupt")),
            "never interrupt a tombstone"
        );
        let sent = prompts(&requests);
        assert_eq!(sent.len(), 2, "no input is resent");
        let old = sent[0]["body"]["id"].as_str().unwrap();
        let declined_observations: Vec<_> = observations
            .iter()
            .filter(|item| matches!(item.observation, crate::Observation::RequestDeclined(_)))
            .collect();
        assert_eq!(
            declined_observations.len(),
            1,
            "one late declined observation: {observations:?}"
        );
        assert_eq!(
            declined_observations[0]
                .vendor_turn
                .as_ref()
                .map(crate::VendorTurnId::as_str),
            Some(old)
        );
        let crate::Observation::RequestDeclined(decline) = &declined_observations[0].observation
        else {
            unreachable!();
        };
        assert_eq!(decline.vendor_method, "permission.asked:bash");
        assert!(decline.blocking && decline.summary.contains("did not settle"));
        assert!(retired, "failed late decline drains the generation");
    });
}

/// §10 retains server-loss facts when the prompt socket fails after its bytes.
#[test]
fn oc09_c2_vendor_death_after_prompt_bytes_reports_server_loss_and_leftovers() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        let mut output = fixture(&cwd, Vec::new());
        replace(
            &mut output,
            route(
                "POST",
                &format!("/api/session/{SES}/prompt"),
                &json!([
                    {"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},
                        "sleep_ms":500,"emit_before_response":true,"emit":[
                            event("session.inbox.enqueued", &json!({"sessionID":"$SESSION","inboxID":"$INPUT"})),
                            {"pause_ms":50},{"exit":true}
                        ]}
                ]),
            ),
        );
        rig.fixture(&output);
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let (end, _) = lane.turn(1, None, Duration::from_secs(5)).await;
        lane.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(end.terminal.is_none());
        assert_eq!(
            prompts(&requests).len(),
            1,
            "the fake received prompt bytes exactly once"
        );
        assert!(
            matches!(end.outcome, Err(AdapterError::Route(ref failure))
            if matches!(failure.cause, crate::RouteError::ServerLost { .. })),
            "vendor death must retain server-loss facts: {end:?}"
        );
        let leftovers = end
            .leftovers
            .as_ref()
            .expect("shared server-loss leftover report");
        assert_eq!(leftovers.scope, crate::observation::LeftoverScope::Server);
        assert_eq!(leftovers.total, 0);
    });
}

struct ControlSources {
    context: TurnCx,
    stop: watch::Sender<Option<StopOrder>>,
    force: watch::Sender<Option<tokio::time::Instant>>,
}

fn control_sources(lane: &Lane) -> ControlSources {
    let prepared = lane.driver.prepare();
    let capacity =
        matches!(prepared, Prepared::NeedsConnection).then(|| Box::new(()) as crate::CapacityToken);
    let now = tokio::time::Instant::now();
    let (stop, stop_rx) = watch::channel(None);
    let (force, force_rx) = watch::channel(None);
    ControlSources {
        context: TurnCx {
            turn: TurnNumber::try_from(1).unwrap(),
            prepared,
            capacity,
            activity: TurnActivity::new(now),
            wall: Deadline::at(now + Duration::from_secs(5)),
            tool_grace: Duration::from_secs(1),
            stop: stop_rx,
            force: force_rx,
            stop_ack: StopAck::new(),
        },
        stop,
        force,
    }
}

fn cancel_order(after: Duration) -> StopOrder {
    let attached = tokio::time::Instant::now();
    StopOrder {
        cause: StopCause::Cancel,
        requested_at: "2026-10-06T00:00:00Z".into(),
        attached,
        force_at: Deadline::at(attached + after),
        close_by: Deadline::at(attached + Duration::from_secs(3)),
    }
}

async fn drive_until<F: Future>(
    mut turn: Pin<&mut F>,
    mut ready: impl FnMut() -> bool,
    bound: Duration,
) -> Option<F::Output> {
    let by = tokio::time::Instant::now() + bound;
    while !ready() && tokio::time::Instant::now() < by {
        // The pinned turn remains alive; losing the short timer consumes no event.
        tokio::select! {
            end = turn.as_mut() => return Some(end),
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    None
}

fn interrupt_seen(rig: &Rig) -> bool {
    rig.requests().iter().any(|request| {
        request["method"] == "POST" && request["target"] == format!("/api/session/{SES}/interrupt")
    })
}

fn control_race_fixture(cwd: &str, terminal: bool) -> Value {
    let mut output = pending_interrupt_fixture(cwd);
    let events = if terminal {
        vec![
            json!({"pause_ms":350}),
            event(
                "session.execution.interrupted",
                &json!({"sessionID":SES,"reason":"user"}),
            ),
        ]
    } else {
        Vec::new()
    };
    replace(
        &mut output,
        route(
            "POST",
            &format!("/api/session/{SES}/interrupt"),
            &json!([
                {"status":200,"json":{"interrupted":true},"sleep_ms":1500,
                    "emit_before_response":true,"emit":events}
            ]),
        ),
    );
    output
}

/// §7.4 reuses Core's earliest `force_at` even while an HTTP stop is pending.
#[test]
fn oc08_c2_shorter_order_cuts_pending_interrupt_at_new_force_at() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        rig.fixture(&control_race_fixture(&cwd, false));
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let inspector = Lane::open(&rig, true);
        let ControlSources {
            context,
            stop,
            force,
        } = control_sources(&lane);
        let (end, elapsed, delivered, sent) = {
            let turn = lane.turn_context(None, context);
            tokio::pin!(turn);
            let mut early = drive_until(
                turn.as_mut(),
                || owns_execution(&inspector),
                Duration::from_secs(2),
            )
            .await;
            let delivered = owns_execution(&inspector);
            stop.send_replace(Some(cancel_order(Duration::from_secs(2))));
            if early.is_none() {
                early = drive_until(
                    turn.as_mut(),
                    || interrupt_seen(&rig),
                    Duration::from_secs(1),
                )
                .await;
            }
            let sent = interrupt_seen(&rig);
            let changed = tokio::time::Instant::now();
            stop.send_replace(Some(cancel_order(Duration::from_millis(200))));
            let (end, _) = match early {
                Some(end) => end,
                None => turn.await,
            };
            (end, changed.elapsed(), delivered, sent)
        };
        drop((stop, force));
        lane.close().await;
        inspector.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(
            delivered && sent,
            "the shortened order reaches an in-flight interrupt"
        );
        assert!(
            end.terminal.is_none() && end.outcome.is_err(),
            "unknown: {end:?}"
        );
        assert!(
            elapsed < Duration::from_millis(800),
            "a shorter order must replace the original two-second cutoff: {elapsed:?}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["target"] == format!("/api/session/{SES}/interrupt"))
                .count(),
            1
        );
    });
}

/// §7.4 compares the terminal's decode instant with `force_at` when a task resumes late.
#[test]
fn oc08_c2_postcutoff_terminal_is_late_when_control_polling_resumes() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        rig.fixture(&control_race_fixture(&cwd, true));
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let inspector = Lane::open(&rig, true);
        let ControlSources {
            context,
            stop,
            force,
        } = control_sources(&lane);
        let ack = context.stop_ack.clone();
        let (end, mut observations, delivered, sent) = {
            let turn = lane.turn_context(None, context);
            tokio::pin!(turn);
            let mut early = drive_until(
                turn.as_mut(),
                || owns_execution(&inspector),
                Duration::from_secs(2),
            )
            .await;
            let delivered = owns_execution(&inspector);
            stop.send_replace(Some(cancel_order(Duration::from_millis(200))));
            if early.is_none() {
                early = drive_until(
                    turn.as_mut(),
                    || interrupt_seen(&rig),
                    Duration::from_secs(1),
                )
                .await;
            }
            let sent = interrupt_seen(&rig);
            // Pause only this turn's polling. The independent event pump and
            // delivery consume the terminal after force_at, before this resumes.
            tokio::time::sleep(Duration::from_millis(600)).await;
            let (end, observations) = match early {
                Some(end) => end,
                None => turn.await,
            };
            (end, observations, delivered, sent)
        };
        // Sealing wakes the ordered consumer to publish withheld late evidence.
        // Keep the driver open through that delivery, before its close cutoff.
        let by = tokio::time::Instant::now() + Duration::from_secs(1);
        while !observations
            .iter()
            .any(|item| matches!(item.observation, crate::Observation::LateTerminal(_)))
        {
            match tokio::time::timeout_at(by, lane.receiver.recv()).await {
                Ok(Some(item)) => observations.push(item.item),
                Ok(None) | Err(_) => break,
            }
        }
        drop((stop, force));
        lane.close().await;
        inspector.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(
            delivered && sent,
            "a stop was pending during the delayed poll"
        );
        assert!(
            end.terminal.is_none() && end.outcome.is_err(),
            "a postcutoff terminal cannot acknowledge the initial result: {end:?}"
        );
        let old = prompts(&requests)[0]["body"]["id"].as_str().unwrap();
        assert!(
            observations.iter().any(|item| matches!(
                item.observation,
                crate::Observation::LateTerminal(_)
            ) && item
                .vendor_turn
                .as_ref()
                .is_some_and(|id| id.as_str() == old)),
            "accepted A retains its late terminal: {observations:?}"
        );
        assert!(
            !*ack.subscribe().borrow(),
            "postcutoff evidence is not a timely acknowledgement"
        );
    });
}

fn dropped_stop_fixture(cwd: &str) -> Value {
    let mut output = control_race_fixture(cwd, false);
    for session in ["ses_stop_hold0", "ses_stop_hold1"] {
        replace(
            &mut output,
            route(
                "POST",
                &format!("/api/session/{session}/interrupt"),
                &json!([{"status":200,"json":{"interrupted":true},"sleep_ms":5000}]),
            ),
        );
    }
    output
}

async fn fill_stop_pool(
    server: &Arc<via_routes::opencode::Server>,
    rig: &Rig,
) -> (JoinSet<()>, bool) {
    let mut held = JoinSet::new();
    for session in ["ses_stop_hold0", "ses_stop_hold1"] {
        let server = Arc::clone(server);
        held.spawn(async move {
            let _outcome = via_routes::opencode::turn::interrupt(
                server.http(),
                session,
                Deadline::at(tokio::time::Instant::now() + Duration::from_secs(6)),
            )
            .await;
        });
    }
    let full = until(
        || {
            rig.requests()
                .iter()
                .filter(|request| {
                    request["target"] == "/api/session/ses_stop_hold0/interrupt"
                        || request["target"] == "/api/session/ses_stop_hold1/interrupt"
                })
                .count()
                == 2
        },
        Duration::from_secs(1),
    )
    .await;
    (held, full)
}

/// §8: dropping a stop's socket must retain its actual first-byte boundary.
async fn dropped_stop(blocked: bool) {
    let rig = Rig::new(&json!({}));
    let cwd = rig.root().to_str().unwrap().to_owned();
    rig.fixture(&dropped_stop_fixture(&cwd));
    row(&rig, 1, "running");
    let mut lane = Lane::open(&rig, false);
    let inspector = Lane::open(&rig, true);
    let ControlSources {
        context,
        stop,
        force,
    } = control_sources(&lane);
    let mut held = JoinSet::new();
    let (delivered, reached_stop, pool_full, server) = {
        let turn = lane.turn_context(None, context);
        tokio::pin!(turn);
        let mut early = drive_until(
            turn.as_mut(),
            || owns_execution(&inspector),
            Duration::from_secs(2),
        )
        .await;
        let delivered = owns_execution(&inspector);
        let server = match inspector.driver.prepare() {
            Prepared::Pinned(pin) => pin
                .opencode
                .and_then(|pin| pin.live())
                .map(|(server, _)| server),
            Prepared::NeedsConnection => None,
        };
        let mut pool_full = !blocked;
        if blocked && let Some(server) = &server {
            (held, pool_full) = fill_stop_pool(server, &rig).await;
        }
        stop.send_replace(Some(cancel_order(Duration::from_secs(2))));
        let ready = || {
            if blocked {
                server.as_ref().is_some_and(|server| {
                    server
                        .routing()
                        .state(SES)
                        .is_some_and(|state| state.pending_requests == 1)
                })
            } else {
                interrupt_seen(&rig)
            }
        };
        if early.is_none() {
            early = drive_until(turn.as_mut(), ready, Duration::from_secs(1)).await;
        }
        let reached_stop = ready() && early.is_none();
        // The entire run_turn future drops here, including its pending stop HTTP
        // future. The two filler sockets remain held until the boundary is read.
        (delivered, reached_stop, pool_full, server)
    };
    let pending = server.as_ref().and_then(|server| {
        server
            .routing()
            .state(SES)
            .map(|state| state.pending_requests)
    });
    let ready = matches!(inspector.driver.prepare(), Prepared::Pinned(_));
    held.abort_all();
    while held.join_next().await.is_some() {}
    drop((stop, force));
    lane.close().await;
    inspector.close().await;
    let requests = rig.requests();
    rig.finish().await;
    assert!(
        delivered && reached_stop && pool_full,
        "reach the intended stop boundary"
    );
    assert_eq!(interrupt_seen_requests(&requests), usize::from(!blocked));
    assert_eq!(
        prompts(&requests).len(),
        1,
        "a dropped stop never resends its prompt"
    );
    if blocked {
        assert!(
            ready,
            "an unsent stop withdrawal does not drain the generation"
        );
        assert_eq!(
            pending,
            Some(0),
            "an unsent stop releases request accounting"
        );
    } else {
        assert!(
            !ready,
            "a sent stop's dropped response drains the generation"
        );
    }
}

fn interrupt_seen_requests(requests: &[Value]) -> usize {
    requests
        .iter()
        .filter(|request| {
            request["method"] == "POST"
                && request["target"] == format!("/api/session/{SES}/interrupt")
        })
        .count()
}

#[test]
fn oc09_c2_dropped_sent_stop_drains_without_resend() {
    run(dropped_stop(false));
}

#[test]
fn oc09_c2_dropped_unsent_stop_releases_accounting_without_drain() {
    run(dropped_stop(true));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OverflowInput {
    Delivered,
    Queued,
    DeliveredDuringCancel,
}

fn overflow_fixture(cwd: &str, input: OverflowInput) -> Value {
    let mut output = fixture(cwd, Vec::new());
    let mut events = vec![event(
        "session.inbox.enqueued",
        &json!({"sessionID":SES,"inboxID":"$INPUT"}),
    )];
    if input == OverflowInput::Delivered {
        events.extend([
            event("session.execution.started", &json!({"sessionID":SES})),
            event(
                "session.inbox.delivered",
                &json!({"sessionID":SES,"inboxID":"$INPUT"}),
            ),
            event(
                "session.step.started",
                &json!({"sessionID":SES,"assistantMessageID":"overflow"}),
            ),
        ]);
    }
    events.push(json!({"pause_ms":500}));
    for _ in 0..20 {
        events.push(if input == OverflowInput::Delivered {
            event(
                "session.text.delta",
                &json!({"sessionID":SES,"assistantMessageID":"overflow","delta":"x"}),
            )
        } else {
            event(
                "session.inbox.enqueued",
                &json!({"sessionID":SES,"inboxID":"$INPUT"}),
            )
        });
    }
    replace(
        &mut output,
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            &json!([
            {"status":200,"sleep_ms":if input == OverflowInput::Delivered {0} else {200},
                "json":{"data":{"id":"$INPUT","sessionID":SES}},"emit":events},
                    {"status":200,"json":{"data":{"id":"$INPUT","sessionID":SES}},"emit":success()}
                ]),
        ),
    );
    let cancellation = if input == OverflowInput::DeliveredDuringCancel {
        vec![
            event("session.execution.started", &json!({"sessionID":SES})),
            event(
                "session.inbox.delivered",
                &json!({"sessionID":SES,"inboxID":"$INPUT"}),
            ),
        ]
    } else {
        vec![event(
            "session.inbox.cancelled",
            &json!({"sessionID":SES,"inboxID":"$INPUT"}),
        )]
    };
    replace(
        &mut output,
        route(
            "DELETE",
            &format!("/api/session/{SES}/inbox/*"),
            &json!([
                {"status":204,"emit_before_response":true,"sleep_ms":800,"emit":cancellation}
            ]),
        ),
    );
    replace(
        &mut output,
        route(
            "POST",
            &format!("/api/session/{SES}/interrupt"),
            &json!([
                {"status":200,"json":{"interrupted":true},"emit_before_response":true,"sleep_ms":200,
                    "emit":[event("session.execution.interrupted",&json!({"sessionID":SES,"reason":"user"}))]}
            ]),
        ),
    );
    output
}

async fn saturate_observations(lane: &Lane) -> bool {
    while lane.receiver.len() < crate::runtime::OBSERVATION_ITEMS {
        if lane
            .driver
            .observations
            .send(
                crate::ObservationItem {
                    at: tokio::time::Instant::now(),
                    vendor_turn: None,
                    observation: crate::Observation::Progress(crate::ProgressMarks::default()),
                },
                Duration::from_millis(5),
            )
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

async fn overflowing_turn(
    lane: &Lane,
    inspector: &Lane,
    rig: &Rig,
    input: OverflowInput,
) -> (Option<TurnEnd>, bool) {
    let ControlSources {
        mut context,
        stop,
        force,
    } = control_sources(lane);
    context.wall = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(20));
    let future = lane.driver.run_turn(
        TurnSpec {
            prompt: "fixture".into(),
            bound: Some(full()),
            ..TurnSpec::default()
        },
        context,
    );
    tokio::pin!(future);
    let early = drive_until(
        future.as_mut(),
        || {
            if input == OverflowInput::Delivered {
                owns_execution(inspector)
            } else {
                !prompts(&rig.requests()).is_empty()
            }
        },
        Duration::from_secs(2),
    )
    .await;
    let filled = saturate_observations(lane).await;
    let end = match early {
        Some(end) => Some(end),
        None => tokio::time::timeout(Duration::from_secs(12), future)
            .await
            .ok(),
    };
    drop((stop, force));
    (end, filled)
}

struct OverflowResult {
    end: Option<TurnEnd>,
    filled: bool,
    ready: bool,
    released: bool,
    next: TurnEnd,
    requests: Vec<Value>,
}

async fn collect_overflow(input: OverflowInput) -> OverflowResult {
    let rig = Rig::new(&json!({}));
    let cwd = rig.root().to_str().unwrap().to_owned();
    rig.fixture(&overflow_fixture(&cwd, input));
    row(&rig, 1, "running");
    let lane = Lane::open(&rig, false);
    let inspector = Lane::open(&rig, true);
    let (end, filled) = overflowing_turn(&lane, &inspector, &rig, input).await;
    let released = until(
        || {
            let Prepared::Pinned(pin) = inspector.driver.prepare() else {
                return false;
            };
            pin.opencode
                .and_then(|pin| pin.live())
                .is_some_and(|(server, _)| server.routing().eligible(SES))
        },
        Duration::from_secs(2),
    )
    .await;
    let ready = matches!(inspector.driver.prepare(), Prepared::Pinned(_));
    row(&rig, 1, "failed");
    row(&rig, 2, "running");
    let mut reopened = Lane::open(&rig, true);
    let (next, _) = reopened.turn(2, None, Duration::from_secs(2)).await;
    reopened.close().await;
    lane.close().await;
    inspector.close().await;
    let requests = rig.requests();
    rig.finish().await;
    OverflowResult {
        end,
        filled,
        ready,
        released,
        next,
        requests,
    }
}

async fn overflow_posts_cleanup(input: OverflowInput) {
    let result = collect_overflow(input).await;
    let end = result
        .end
        .as_ref()
        .expect("observation stall ends as overflow before wall");
    assert!(
        result.filled && end.terminal.is_none(),
        "nonterminal saturated turn: {end:?}"
    );
    let Err(AdapterError::Route(failure)) = &end.outcome else {
        panic!("overflow: {end:?}");
    };
    assert!(matches!(failure.cause, crate::RouteError::Overflow { .. }));
    let cancels: Vec<_> = result
        .requests
        .iter()
        .filter(|request| request["method"] == "DELETE")
        .collect();
    assert_eq!(
        cancels.len(),
        usize::from(input != OverflowInput::Delivered),
        "once-only inbox cleanup: {:?}",
        result.requests
    );
    assert_eq!(
        interrupt_seen_requests(&result.requests),
        usize::from(input != OverflowInput::Queued),
        "once-only execution cleanup: {:?}",
        result.requests
    );
    assert_eq!(
        failure.cleanup,
        Some(crate::WireCleanup::Uncertain),
        "nonterminal overflow cannot claim quiescence: {end:?}"
    );
    assert!(
        result.ready && result.released,
        "cleanup releases server-scoped execution without draining"
    );
    assert!(
        result.next.terminal.is_some(),
        "replacement dispatches: {:?}",
        result.next
    );
    assert_eq!(
        prompts(&result.requests).len(),
        2,
        "never resend the overflowed input"
    );
    if input == OverflowInput::DeliveredDuringCancel {
        let interrupt = result
            .requests
            .iter()
            .find(|request| request["target"] == format!("/api/session/{SES}/interrupt"))
            .unwrap();
        assert!(
            interrupt["received_ms"].as_u64().unwrap()
                < cancels[0]["received_ms"].as_u64().unwrap() + 700,
            "delivery requires an interrupt while DELETE remains pending"
        );
    }
}

#[test]
fn oc09_c2_nonterminal_delivered_overflow_posts_interrupt_cleanup() {
    run(overflow_posts_cleanup(OverflowInput::Delivered));
}

#[test]
fn oc09_c2_nonterminal_queued_overflow_posts_input_cleanup() {
    run(overflow_posts_cleanup(OverflowInput::Queued));
}

#[test]
fn oc09_c2_nonterminal_overflow_interrupts_delivery_during_pending_cancel() {
    run(overflow_posts_cleanup(OverflowInput::DeliveredDuringCancel));
}

fn held_cancel_fixture(cwd: &str) -> Value {
    let mut output = fixture(
        cwd,
        vec![event(
            "session.inbox.enqueued",
            &json!({"sessionID":SES,"inboxID":"$INPUT"}),
        )],
    );
    replace(
        &mut output,
        route(
            "DELETE",
            &format!("/api/session/{SES}/inbox/*"),
            &json!([
                {"status":204,"sleep_ms":1500,"emit_before_response":true,"emit":[
                    {"pause_ms":100},
                    event("session.execution.started", &json!({"sessionID":SES})),
                    event("session.inbox.delivered", &json!({"sessionID":SES,"inboxID":"$INPUT"}))
                ]}
            ]),
        ),
    );
    replace(
        &mut output,
        route(
            "POST",
            &format!("/api/session/{SES}/interrupt"),
            &json!([
                {"status":200,"json":{"interrupted":true},"sleep_ms":200,"emit_before_response":true,"emit":[
                    {"pause_ms":50},
                    event("session.execution.interrupted", &json!({"sessionID":SES,"reason":"user"}))
                ]}
            ]),
        ),
    );
    output
}

fn live_server(lane: &Lane) -> Option<Arc<via_routes::opencode::Server>> {
    match lane.driver.prepare() {
        Prepared::Pinned(pin) => pin
            .opencode
            .and_then(|pin| pin.live())
            .map(|(server, _)| server),
        Prepared::NeedsConnection => None,
    }
}

async fn held_stop_fences(server: Option<&Arc<via_routes::opencode::Server>>) -> (bool, bool) {
    let simultaneous = until(
        || {
            server.is_some_and(|server| {
                server
                    .routing()
                    .state(SES)
                    .is_some_and(|state| state.pending_requests == 2)
            })
        },
        Duration::from_secs(1),
    )
    .await;
    let fenced = until(
        || {
            server.is_some_and(|server| {
                server.routing().state(SES).is_some_and(|state| {
                    state.last.as_ref().is_some_and(|last| {
                        last.phase == via_routes::opencode::state::InputPhase::Ended
                    }) && state.pending_requests == 1
                        && !state.eligible()
                })
            })
        },
        Duration::from_secs(1),
    )
    .await;
    (simultaneous, fenced)
}

/// §7.4 requires a second reserved stop while the original DELETE response is held.
#[test]
fn oc08_c2_delivery_during_pending_cancel_interrupts_before_force() {
    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        rig.fixture(&held_cancel_fixture(&cwd));
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let inspector = Lane::open(&rig, true);
        let ControlSources {
            context,
            stop,
            force,
        } = control_sources(&lane);
        let ack = context.stop_ack.clone();
        let active = tokio::spawn(async move {
            let (end, _) = lane.turn_context(None, context).await;
            (lane, end)
        });
        let admitted = until(
            || {
                live_server(&inspector).is_some_and(|server| {
                    server
                        .routing()
                        .state(SES)
                        .is_some_and(|state| state.last.is_some() && state.pending_requests == 0)
                })
            },
            Duration::from_secs(2),
        )
        .await;
        let server = live_server(&inspector);
        stop.send_replace(Some(cancel_order(Duration::from_millis(900))));
        let (simultaneous, fenced) = held_stop_fences(server.as_ref()).await;
        let (lane, end) = active.await.unwrap();
        drop((stop, force));
        lane.close().await;
        inspector.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(admitted, "prompt settled before queued-input cancellation");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "DELETE")
                .count(),
            1
        );
        assert_eq!(
            interrupt_seen_requests(&requests),
            1,
            "delivery requires interrupt before the held DELETE settles: {requests:?}"
        );
        assert!(
            simultaneous && fenced,
            "the original cancellation remains counted after the execution ends"
        );
        assert!(
            matches!(
                end.terminal.as_ref().map(|terminal| &terminal.status),
                Some(VendorTerminalStatus::Interrupted)
            ),
            "earlier native acknowledgement stands: {end:?}"
        );
        assert!(
            *ack.subscribe().borrow(),
            "native stop acknowledgement arrived before force"
        );
        assert_eq!(
            prompts(&requests).len(),
            1,
            "never resend the cancelled input"
        );
    });
}

fn tool_grace_fixture(cwd: &str, tool_end_after: Duration) -> Value {
    let mut output = pending_interrupt_fixture(cwd);
    let mut running = success();
    running.pop();
    running.extend([
        event(
            "session.step.started",
            &json!({"sessionID":SES,"assistantMessageID":"grace_message"}),
        ),
        event(
            "session.tool.called",
            &json!({"sessionID":SES,"id":"grace_call",
                "assistantMessageID":"grace_message","name":"bash"}),
        ),
    ]);
    replace(
        &mut output,
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            &json!([{"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},
                "emit":running}]),
        ),
    );
    replace(
        &mut output,
        route(
            "POST",
            &format!("/api/session/{SES}/interrupt"),
            &json!([{"status":200,"json":{"interrupted":true},"sleep_ms":1500,
            "emit_before_response":true,"emit":[
                {"pause_ms":100},
                event("session.execution.interrupted", &json!({"sessionID":SES,"reason":"user"})),
                {"pause_ms":tool_end_after.as_millis()},
                event("session.tool.success", &json!({"sessionID":SES,"id":"grace_call"}))
            ]}]),
        ),
    );
    output
}

/// §7.4: tool grace starts at native acknowledgement while the stop socket stays owned.
async fn held_stop_tool_grace(tool_end_after: Duration, cleanup: crate::Cleanup) {
    let rig = Rig::new(&json!({}));
    let cwd = rig.root().to_str().unwrap().to_owned();
    rig.fixture(&tool_grace_fixture(&cwd, tool_end_after));
    row(&rig, 1, "running");
    let mut lane = Lane::open(&rig, false);
    let inspector = Lane::open(&rig, true);
    let ControlSources {
        mut context,
        stop,
        force,
    } = control_sources(&lane);
    context.tool_grace = Duration::from_millis(200);
    let ack = context.stop_ack.clone();
    let active = tokio::spawn(async move {
        let (end, observations) = lane.turn_context(None, context).await;
        (lane, end, observations)
    });
    let ready = until(|| owns_execution(&inspector), Duration::from_secs(2)).await;
    stop.send_replace(Some(cancel_order(Duration::from_millis(900))));
    let (lane, end, observations) = active.await.unwrap();
    drop((stop, force));
    lane.close().await;
    inspector.close().await;
    let requests = rig.requests();
    rig.finish().await;
    assert!(ready, "execution accepted before stop");
    assert!(
        observations.iter().any(|item| {
            let crate::Observation::Progress(marks) = &item.observation else {
                return false;
            };
            marks.tools_started.iter().any(|(id, _)| id == "grace_call")
        }),
        "the tool was reported before the stop: {observations:?}"
    );
    assert!(
        matches!(
            end.terminal.as_ref().map(|terminal| &terminal.status),
            Some(VendorTerminalStatus::Interrupted)
        ),
        "native terminal stands: {end:?}"
    );
    assert!(
        *ack.subscribe().borrow(),
        "native acknowledgement reaches Core before force"
    );
    assert_eq!(
        end.outcome.as_ref().unwrap().cleanup,
        cleanup,
        "tool grace is independent of HTTP response: {end:?}"
    );
    assert_eq!(
        interrupt_seen_requests(&requests),
        1,
        "never resend interrupt"
    );
    assert_eq!(prompts(&requests).len(), 1, "never resend prompt");
}

#[test]
fn oc08_c2_tool_end_within_grace_during_held_stop_response_is_quiescent() {
    run(held_stop_tool_grace(
        Duration::from_millis(50),
        crate::Cleanup::Quiescent,
    ));
}

#[test]
fn oc08_c2_tool_end_after_grace_during_held_stop_response_stays_uncertain() {
    run(held_stop_tool_grace(
        Duration::from_millis(350),
        crate::Cleanup::Uncertain,
    ));
}
