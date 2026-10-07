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
        until(|| {
            live.routing()
                .state(SES)
                .is_some_and(|state| state.pending_requests > 0)
        })
        .await
            && !active.is_finished()
            && requests_to(&rig.requests(), &setup_target) == before
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

/// §5, §7.2, §8: the kept switch changes the vendor's variant only at release.
fn kept_variant_fixture(cwd: &str, release: &std::path::Path) -> Value {
    let mut next = fixture(cwd, success());
    next["session_variants"] = json!({SES:"low"});
    for route in next["routes"].as_array_mut().unwrap() {
        if route["path"] == "/api/model" {
            route["responses"][0]["json"]["data"][0]["variants"] =
                json!([{"id":"low"},{"id":"high"}]);
        }
        if route["path"] == "/api/session" {
            route["responses"][0]["json"]["data"]["model"]["variant"] = json!("low");
        }
        if route["path"] == format!("/api/session/{SES}") {
            route["responses"][0]["json"]["data"]["model"]["variant"] = json!("$VARIANT");
        }
    }
    replace(
        &mut next,
        route(
            "POST",
            &format!("/api/session/{SES}/model"),
            &json!([
                {"status":204,"wait_for_file":release,"apply_variant":true},
                {"status":204,"apply_variant":true}
            ]),
        ),
    );
    next
}

async fn successor_after_kept_variant(reopened: bool) {
    let rig = Rig::new(&json!({}));
    let release = rig.root().join("release-model-switch");
    rig.fixture(&kept_variant_fixture(
        rig.root().to_str().unwrap(),
        &release,
    ));
    row(&rig, 1, "running");
    let mut lane = Lane::open(&rig, false);
    let (warm, _) = lane.turn(1, Some("low"), Duration::from_secs(4)).await;
    row(&rig, 1, "completed");
    let live = server(&lane).unwrap();
    let model_target = format!("/api/session/{SES}/model");
    let (context, stop) = controls(&lane, 2);
    let active = tokio::spawn(async move {
        let (end, _) = lane.turn_context(Some("high"), context).await;
        (lane, end)
    });
    let sent = until(|| requests_to(&rig.requests(), &model_target) == 1).await;
    cancel(&stop, Duration::from_millis(20));
    let (lane, stopped) = active.await.unwrap();
    let retained = live
        .routing()
        .state(SES)
        .is_some_and(|state| state.pending_requests == 1);
    let mut successor = if reopened {
        let successor = Lane::open(&rig, true);
        // Pin before closing the old driver: its current exchange still owns this generation.
        let _pin = successor.driver.prepare();
        lane.close().await;
        successor
    } else {
        lane
    };
    row(&rig, 3, "running");
    let instruction_target = format!("/api/experimental/session/{SES}/instructions/entries");
    let before = requests_to(&rig.requests(), &instruction_target);
    let (context, successor_stop) = controls(&successor, 3);
    let (end, entered, waited, prompts_before_release) = {
        let future = successor.turn_context(Some("low"), context);
        tokio::pin!(future);
        // Poll through setup; keep the successor future pinned while checking the held exchange.
        let mut early = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(match future.as_mut().poll(cx) {
                std::task::Poll::Ready(end) => Some(end),
                std::task::Poll::Pending => None,
            })
        })
        .await;
        let entered = if reopened && early.is_none() {
            // Losing the monitor consumes nothing; the same turn future remains pinned.
            let reopen_setup = until(|| requests_to(&rig.requests(), &instruction_target) > before);
            tokio::select! {
                end = &mut future => { early = Some(end); false },
                entered = reopen_setup => entered,
            }
        } else {
            early.is_none()
        };
        // Any pre-admission read has answered before release; the earlier switch remains pending.
        let waited = until(|| {
            live.routing()
                .state(SES)
                .is_some_and(|state| state.pending_requests == 1)
        })
        .await;
        let prompts_before_release = prompts(&rig.requests()).len();
        std::fs::write(&release, b"").unwrap();
        let (end, _) = if let Some(end) = early {
            end
        } else {
            future.await
        };
        (end, entered, waited, prompts_before_release)
    };
    drop(successor_stop);
    successor.close().await;
    drop(stop);
    let requests = rig.requests();
    rig.finish().await;
    assert!(warm.terminal.is_some(), "warmup: {warm:?}");
    assert!(
        sent && retained && entered && waited,
        "held setup: {stopped:?}"
    );
    assert_eq!(
        prompts_before_release, 1,
        "successor must wait for the kept switch"
    );
    assert!(end.terminal.is_some(), "successor: {end:?}");
    assert_own_variant(&requests, &model_target);
}

fn assert_own_variant(requests: &[Value], model_target: &str) {
    let switches: Vec<_> = requests
        .iter()
        .filter(|request| request["target"] == model_target)
        .map(|request| request["body"]["model"]["variant"].clone())
        .collect();
    assert_eq!(switches, vec![json!("high"), json!("low")]);
    let prompts = prompts(requests);
    assert_eq!(prompts.len(), 2, "never resend the stopped turn's prompt");
    assert_eq!(prompts[1]["variant"], "low", "effort at prompt dispatch");
    let prompt_index = requests
        .iter()
        .rposition(|request| {
            request["target"]
                .as_str()
                .is_some_and(|target| target.ends_with("/prompt"))
        })
        .unwrap();
    assert_eq!(
        requests[prompt_index - 1]["target"],
        format!("/api/session/{SES}")
    );
    assert_eq!(
        requests[prompt_index - 1]["variant"],
        "low",
        "own switch was read back"
    );
}

#[test]
fn oc05_c2_kept_model_switch_is_read_after_successor_admission() {
    run(successor_after_kept_variant(false));
}

#[test]
fn oc05_c2_kept_model_switch_is_read_after_reopened_admission() {
    run(successor_after_kept_variant(true));
}

/// OC05 (§7.2, §8): an abandoned cleanup pipeline can restart after its kept GET ends.
#[test]
fn oc05_cancelled_reopen_cleanup_is_retried_by_successor() {
    run(async {
        let rig = Rig::new(&json!({}));
        let release = rig.root().join("release-reopen-inbox");
        let mut next = fixture(rig.root().to_str().unwrap(), success());
        replace(
            &mut next,
            route(
                "GET",
                &format!("/api/session/{SES}/inbox"),
                &json!([
                    {"status":200,"json":{"data":[]},"wait_for_file":release},
                    {"status":200,"json":{"data":[]}}
                ]),
            ),
        );
        rig.fixture(&next);
        let warm = bootstrap(&rig).await;
        warm.close().await;
        row(&rig, 2, "running");
        let mut lane = Lane::open(&rig, true);
        let (context, stop) = controls(&lane, 2);
        let active = tokio::spawn(async move {
            let (end, _) = lane.turn_context(None, context).await;
            (lane, end)
        });
        let target = format!("/api/session/{SES}/inbox");
        let entered = until(|| requests_to(&rig.requests(), &target) == 1).await;
        cancel(&stop, Duration::from_millis(20));
        let (mut lane, stopped) = active.await.unwrap();
        row(&rig, 2, "unknown");
        let live = server(&lane).unwrap();
        std::fs::write(release, b"release").unwrap();
        let completed = until(|| {
            live.routing()
                .state(SES)
                .is_some_and(|state| state.pending_requests == 0)
        })
        .await;
        row(&rig, 3, "running");
        let (end, _) = lane.turn(3, None, Duration::from_secs(2)).await;
        let inbox_reads = requests_to(&rig.requests(), &target);
        lane.close().await;
        rig.finish().await;
        assert!(
            entered && completed,
            "fixture did not reach/release the kept inbox GET"
        );
        assert!(stopped.terminal.is_none());
        assert_eq!(
            end.terminal.as_ref().map(|terminal| terminal.status),
            Some(VendorTerminalStatus::Completed),
            "{end:?}"
        );
        assert_eq!(inbox_reads, 2, "successor reruns abandoned cleanup");
    });
}

async fn malformed_setup_drains(model_switch: bool) {
    let rig = Rig::new(&json!({}));
    let mut next = fixture(rig.root().to_str().unwrap(), success());
    next["session_variants"] = json!({SES:"default"});
    if model_switch {
        replace(
            &mut next,
            route(
                "POST",
                &format!("/api/session/{SES}/model"),
                &json!([
                    {"status":200,"raw":"<html>untrusted</html>","apply_variant":true}
                ]),
            ),
        );
        for route in next["routes"].as_array_mut().unwrap() {
            if route["path"] == format!("/api/session/{SES}") {
                route["responses"][0]["json"]["data"]["model"]["variant"] = json!("$VARIANT");
            }
        }
    } else {
        let info = next["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|route| route["path"] == format!("/api/session/{SES}"))
            .unwrap()["responses"][0]
            .clone();
        replace(
            &mut next,
            route(
                "GET",
                &format!("/api/session/{SES}"),
                &json!([
                    info, {"status":500,"raw":"{ broken-json"}
                ]),
            ),
        );
    }
    rig.fixture(&next);
    let mut lane = bootstrap(&rig).await;
    let live = server(&lane).unwrap();
    row(&rig, 2, "running");
    let (end, _) = lane
        .turn(2, model_switch.then_some("high"), Duration::from_secs(2))
        .await;
    let drained = live.is_draining();
    let prompts = prompts(&rig.requests()).len();
    lane.close().await;
    rig.finish().await;
    assert!(drained, "undecodable setup response must drain: {end:?}");
    assert!(
        end.terminal.is_none(),
        "no prompt after malformed setup: {end:?}"
    );
    assert_eq!(prompts, 1, "only the warmup prompt was sent");
}

#[test]
fn oc09_model_switch_200_html_drains_without_prompt() {
    run(malformed_setup_drains(true));
}

#[test]
fn oc09_setup_500_malformed_json_drains() {
    run(malformed_setup_drains(false));
}
