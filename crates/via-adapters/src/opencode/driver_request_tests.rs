//! C2 regressions for retained first-byte request ownership (`opencode.md` §8).

use super::driver_tests::{Lane, SES, event, fixture, prompts, replace, route, row, run, success};
use super::serve_tests::Rig;
use crate::{
    Deadline, Prepared, StopAck, StopCause, StopOrder, TurnActivity, TurnCx, TurnNumber,
    VendorTerminalStatus,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinSet;
use via_routes::opencode::Server;

async fn until(mut predicate: impl FnMut() -> bool) -> bool {
    let by = tokio::time::Instant::now() + Duration::from_secs(2);
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

fn server(lane: &Lane) -> Option<Arc<Server>> {
    match lane.driver.prepare() {
        Prepared::Pinned(pin) => pin
            .opencode
            .and_then(|pin| pin.live())
            .map(|(server, _)| server),
        Prepared::NeedsConnection => None,
    }
}

fn controls(lane: &Lane, number: u32) -> (TurnCx, watch::Sender<Option<StopOrder>>) {
    let prepared = lane.driver.prepare();
    let capacity =
        matches!(prepared, Prepared::NeedsConnection).then(|| Box::new(()) as crate::CapacityToken);
    let now = tokio::time::Instant::now();
    let (stop, stop_rx) = watch::channel(None);
    let (_force, force) = watch::channel(None);
    (
        TurnCx {
            turn: TurnNumber::try_from(number).unwrap(),
            prepared,
            capacity,
            activity: TurnActivity::new(now),
            wall: Deadline::at(now + Duration::from_secs(4)),
            tool_grace: Duration::from_millis(50),
            stop: stop_rx,
            force,
            stop_ack: StopAck::new(),
        },
        stop,
    )
}

fn cancel(stop: &watch::Sender<Option<StopOrder>>, grace: Duration) {
    let attached = tokio::time::Instant::now();
    stop.send_replace(Some(StopOrder {
        cause: StopCause::Cancel,
        requested_at: "2026-10-06T00:00:00Z".to_owned(),
        attached,
        force_at: Deadline::at(attached + grace),
        close_by: Deadline::at(attached + Duration::from_secs(3)),
    }));
}

async fn bootstrap(rig: &Rig) -> Lane {
    row(rig, 1, "running");
    let mut lane = Lane::open(rig, false);
    let (end, _) = lane.turn(1, None, Duration::from_secs(4)).await;
    row(
        rig,
        1,
        if end.terminal.is_some() {
            "completed"
        } else {
            "unknown"
        },
    );
    lane
}

#[derive(Clone, Copy)]
enum Setup {
    UnsentReadback,
    SentReadback,
    SentModel,
}

/// §8: the original generation serves both warmup and the held setup response.
fn setup_fixture(cwd: &str, setup: Setup) -> Value {
    let mut next = fixture(cwd, success());
    if matches!(setup, Setup::SentModel) {
        replace(
            &mut next,
            route(
                "POST",
                &format!("/api/session/{SES}/model"),
                &json!([
                    {"status":204,"sleep_ms":1000}
                ]),
            ),
        );
    }
    if matches!(setup, Setup::SentReadback) {
        let mut response = next["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|route| {
                route["method"] == "GET" && route["path"] == format!("/api/session/{SES}")
            })
            .unwrap()["responses"][0]
            .clone();
        response["sleep_ms"] = json!(1000);
        replace(
            &mut next,
            route("GET", &format!("/api/session/{SES}"), &json!([response])),
        );
    }
    replace(
        &mut next,
        route(
            "GET",
            "/api/session/ses_pool_hold*",
            &json!([{"status":404,"sleep_ms":1500}]),
        ),
    );
    next
}

/// §8: hold all four General permits without borrowing a reserved control pool.
async fn setup_pool(rig: &Rig, live: &Arc<Server>, setup: Setup) -> (JoinSet<()>, bool) {
    let held_pool = matches!(setup, Setup::UnsentReadback);
    let mut held = JoinSet::new();
    if held_pool {
        for index in 0..4 {
            let live = Arc::clone(live);
            held.spawn(async move {
                let _response = via_routes::opencode::session::get(
                    live.http(),
                    &format!("ses_pool_hold{index}"),
                    Deadline::at(tokio::time::Instant::now() + Duration::from_secs(2)),
                )
                .await;
            });
        }
    }
    let full = !held_pool
        || until(|| {
            rig.requests()
                .iter()
                .filter(|request| {
                    request["target"]
                        .as_str()
                        .is_some_and(|target| target.starts_with("/api/session/ses_pool_hold"))
                })
                .count()
                == 4
        })
        .await;
    (held, full)
}

fn requests_to(requests: &[Value], target: &str) -> usize {
    requests
        .iter()
        .filter(|request| request["target"] == target)
        .count()
}

async fn cancelled_setup(setup: Setup) {
    let held_pool = matches!(setup, Setup::UnsentReadback);
    let model = matches!(setup, Setup::SentModel);
    let rig = Rig::new(&json!({}));
    let cwd = rig.root().to_str().unwrap().to_owned();
    rig.fixture(&setup_fixture(&cwd, setup));
    let warm = bootstrap(&rig).await;
    let live = server(&warm).unwrap();
    let (mut held, full) = setup_pool(&rig, &live, setup).await;
    let setup_target = if model {
        format!("/api/session/{SES}/model")
    } else {
        format!("/api/session/{SES}")
    };
    let before = requests_to(&rig.requests(), &setup_target);
    let mut cancelled = warm;
    let owner = Lane::open(&rig, true);
    let (context, stop) = controls(&cancelled, 2);
    let pin = owner.driver.prepare();
    let active = tokio::spawn(async move {
        let (end, _) = cancelled
            .turn_context(model.then_some("high"), context)
            .await;
        (cancelled, end)
    });
    let entered = if held_pool {
        tokio::time::sleep(Duration::from_millis(30)).await;
        !active.is_finished() && requests_to(&rig.requests(), &setup_target) == before
    } else {
        until(|| requests_to(&rig.requests(), &setup_target) > before).await
    };
    cancel(&stop, Duration::from_millis(20));
    let (cancelled, end) = active.await.unwrap();
    let drain_on_stop = live.is_draining();
    let retained = !model
        || live
            .routing()
            .state(SES)
            .is_some_and(|state| state.pending_requests > 0);
    if !held_pool {
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    let released = until(|| {
        live.routing()
            .state(SES)
            .is_some_and(|state| state.pending_requests == 0)
    })
    .await;
    while held.join_next().await.is_some() {}
    let still_live = !live.is_draining() && matches!(owner.driver.prepare(), Prepared::Pinned(_));
    let pin_alive = match pin {
        Prepared::Pinned(pin) => pin.opencode.and_then(|pin| pin.live()).is_some(),
        Prepared::NeedsConnection => false,
    };
    cancelled.close().await;
    owner.close().await;
    drop(stop);
    let requests = rig.requests();
    rig.finish().await;
    assert!(
        full && entered,
        "setup boundary: full={full}, entered={entered}, end={end:?}, requests={requests:?}"
    );
    assert!(
        !drain_on_stop && still_live && pin_alive,
        "caller stop cannot drain: {end:?}"
    );
    assert!(
        retained,
        "a sent setup socket stays counted after the caller settles"
    );
    assert!(
        released,
        "cancelled setup must release request accounting after its response"
    );
    assert_eq!(
        prompts(&requests).len(),
        1,
        "setup cancellation never submits or resends"
    );
    if held_pool {
        assert_eq!(
            requests_to(&requests, &setup_target),
            before,
            "unsent setup was withdrawn"
        );
    }
}

#[test]
fn oc09_c2_cancelled_setup_waiting_for_pool_withdraws_without_drain() {
    run(cancelled_setup(Setup::UnsentReadback));
}

#[test]
fn oc09_c2_cancelled_sent_setup_keeps_response_without_drain() {
    run(cancelled_setup(Setup::SentReadback));
}

#[test]
fn oc09_c2_cancelled_sent_model_keeps_socket_and_releases_accounting() {
    run(cancelled_setup(Setup::SentModel));
}

async fn cancelled_pending_prompt(delivered: bool) {
    let rig = Rig::new(&json!({}));
    let cwd = rig.root().to_str().unwrap().to_owned();
    let mut next = fixture(&cwd, Vec::new());
    let mut emit = vec![event(
        "session.inbox.enqueued",
        &json!({
            "sessionID":SES,"inboxID":"$INPUT"
        }),
    )];
    if delivered {
        emit.extend([
            event("session.execution.started", &json!({"sessionID":SES})),
            event(
                "session.inbox.delivered",
                &json!({"sessionID":SES,"inboxID":"$INPUT"}),
            ),
        ]);
    }
    replace(
        &mut next,
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            &json!([{
                "status":200,"sleep_ms":1000,"emit_before_response":true,"emit":emit,
                "json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}}
            }]),
        ),
    );
    replace(
        &mut next,
        route(
            "DELETE",
            &format!("/api/session/{SES}/inbox/*"),
            &json!([{
                "status":204,"emit":[event("session.inbox.cancelled", &json!({
                    "sessionID":SES,"inboxID":"$INPUT"
                }))]
            }]),
        ),
    );
    replace(
        &mut next,
        route(
            "POST",
            &format!("/api/session/{SES}/interrupt"),
            &json!([{
                "status":200,"json":{"interrupted":true},"emit":[event(
                    "session.execution.interrupted", &json!({"sessionID":SES,"reason":"user"})
                )]
            }]),
        ),
    );
    rig.fixture(&next);
    row(&rig, 1, "running");
    let mut lane = Lane::open(&rig, false);
    let inspector = Lane::open(&rig, true);
    let (context, stop) = controls(&lane, 1);
    let acknowledgement = context.stop_ack.subscribe();
    let active = tokio::spawn(async move {
        let (end, _) = lane.turn_context(None, context).await;
        (lane, end)
    });
    let sent = until(|| {
        server(&inspector).is_some_and(|server| {
            server.routing().state(SES).is_some_and(|state| {
                state.last.as_ref().is_some_and(|last| {
                    last.phase
                        == if delivered {
                            via_routes::opencode::state::InputPhase::Delivered
                        } else {
                            via_routes::opencode::state::InputPhase::Accepted
                        }
                }) && state.pending_requests == 1
            })
        })
    })
    .await;
    let live = server(&inspector);
    cancel(&stop, Duration::from_secs(1));
    let (lane, end) = active.await.unwrap();
    let released = until(|| {
        live.as_ref().is_some_and(|server| {
            server
                .routing()
                .state(SES)
                .is_some_and(|state| state.pending_requests == 0)
        })
    })
    .await;
    let healthy = live.as_ref().is_some_and(|server| !server.is_draining())
        && matches!(inspector.driver.prepare(), Prepared::Pinned(_));
    lane.close().await;
    inspector.close().await;
    drop(stop);
    let requests = rig.requests();
    rig.finish().await;
    assert!(sent, "stop raced a sent prompt awaiting its 200");
    assert!(
        *acknowledgement.borrow(),
        "the native stop was acknowledged"
    );
    assert!(
        healthy && released,
        "complete prompt response must not drain: {end:?}"
    );
    assert_eq!(
        end.terminal.as_ref().map(|terminal| &terminal.status),
        Some(&VendorTerminalStatus::Interrupted),
        "native stop: {end:?}"
    );
    assert_eq!(prompts(&requests).len(), 1, "never resend the sent prompt");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "DELETE")
            .count(),
        usize::from(!delivered)
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| { request["target"] == format!("/api/session/{SES}/interrupt") })
            .count(),
        usize::from(delivered)
    );
}

#[test]
fn oc08_c2_cancelled_pending_prompt_keeps_200_and_cancels_queued_input() {
    run(cancelled_pending_prompt(false));
}

#[test]
fn oc08_c2_cancelled_pending_prompt_keeps_200_and_interrupts_delivered_input() {
    run(cancelled_pending_prompt(true));
}
