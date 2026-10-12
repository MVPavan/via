//! x.3.2 X4 D7 (C2 §2 Interrupt, C1 §3.5 P7): a driver's report that
//! vendor evidence acknowledged its turn's stop reaches `cancel` and
//! `status` as `acknowledged` with cleanup `pending` while the turn still
//! runs, and only over the order's committed `cancel.requested`.
//!
//! Each case runs a real dispatch whose fake driver is held as its run
//! begins (`adapter.fake.turn_started`), so the turn's `run_turn` is
//! pending. The test makes or drops
//! the turn's `StopAck` in the driver's stead: the fake driver drops its
//! own at once (it returns at acknowledgement), and Core keeps a clone for
//! tests (`Faults::stop_ack`).

use std::{path::Path, time::Duration};

use serde_json::{Value, json};

use super::{
    FAILPOINT_TOKEN, acked, arm_next_with, child, dispatch, new_session, open, release_point, run,
    until,
};
use crate::engine::Engine;
use crate::engine::queue::Ack;
use crate::{SessionId, TurnNumber};

/// The fake driver's run began: paused, the turn's `run_turn` holds
/// there, holding nothing of the Store.
const LAUNCH: &str = "adapter.fake.turn_started";
/// Core's cancel, past its order's attach.
const ORDERED: &str = "core.cancel.ordered";

/// Counts `points` (none acts until armed); returns the directory.
fn counted(root: &Path, points: &[&str]) -> std::path::PathBuf {
    super::count_points(root, points)
}

/// The turn's `StopAck` Core handed the driver.
fn stop_ack(engine: &Engine) -> via_adapters::StopAck {
    super::super::lock(&engine.faults.stop_ack)
        .clone()
        .expect("the turn's context was built")
}

/// A `cancel` of the session's turn 1.
async fn cancel(engine: &Engine, session: &SessionId, wait: bool) -> Value {
    let params = serde_json::from_value(json!({"session": session.as_str(),
        "handle": super::HANDLE, "turn": 1, "wait": wait}))
    .unwrap();
    engine.cancel(params).await.unwrap()
}

/// The session's `status`.
async fn status(engine: &Engine, session: &SessionId) -> Value {
    let params = serde_json::from_value(json!({"session": session})).unwrap();
    engine.status(params).await.unwrap()
}

/// Polls `status` until `ready` holds, within 10 s.
async fn status_until(
    engine: &Engine,
    session: &SessionId,
    ready: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = status(engine, session).await;
            if ready(&status) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("status reached the condition")
}

/// The running turn's `cancel` object while its cleanup is pending.
fn pending(outcome: &str, requested_at: &Value) -> Value {
    json!({"outcome": outcome, "cleanup": "pending", "requested_at": requested_at,
        "settled_at": null})
}

/// `core_p7_ack_visible` (also F16c's generic half): the driver reports
/// acknowledgement after the order and holds `run_turn`; a non-wait
/// `cancel` and `status` show `{acknowledged, pending, settled_at: null}`
/// while the turn runs, and a waiting `cancel` resolves only once the turn
/// ended.
#[test]
fn core_p7_ack_visible() {
    let Some(root) = child("stop_ack::core_p7_ack_visible") else {
        return;
    };
    let points = counted(&root, &[LAUNCH]);
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let launch = arm_next_with(&points, LAUNCH, &json!({"action":"pause"}));
        let ((), ()) = tokio::join!(dispatch(&engine, &session), async {
            until(|| acked(&points, LAUNCH, launch)).await;
            let first = cancel(&engine, &session, false).await;
            let requested_at = first["cancel"]["requested_at"].clone();
            assert_eq!(
                first["cancel"],
                pending("requested", &requested_at),
                "{first}"
            );
            stop_ack(&engine).acknowledged();
            let shown = status_until(&engine, &session, |status| {
                status["active_turn"]["cancel"]["outcome"] == "acknowledged"
            })
            .await;
            assert_eq!(shown["active_turn"]["state"], "running", "{shown}");
            assert_eq!(
                shown["active_turn"]["cancel"],
                pending("acknowledged", &requested_at),
                "{shown}"
            );
            let second = cancel(&engine, &session, false).await;
            assert_eq!(
                second,
                json!({"turn": format!("{}/1", session.as_str()), "state": "running",
                    "already_terminal": false,
                    "cancel": pending("acknowledged", &requested_at)})
            );
            let waiting = cancel(&engine, &session, true);
            tokio::pin!(waiting);
            assert!(
                tokio::time::timeout(Duration::from_millis(200), &mut waiting)
                    .await
                    .is_err(),
                "a waiting cancel waits for the turn's end"
            );
            release_point(&points, LAUNCH, launch);
            let settled = tokio::time::timeout(Duration::from_secs(10), waiting)
                .await
                .expect("the waiting cancel resolves once the turn ended");
            assert_ne!(settled["state"], "running", "{settled}");
            assert!(!settled["cancel"]["settled_at"].is_null(), "{settled}");
        });
    });
}

/// `core_ack_before_order_publish`: the driver's report comes before any
/// order; it is shown only once the order's `cancel.requested` committed
/// (`Requested`), never before. The cancel caller, held past its attach,
/// then reads `acknowledged` with the committed `requested_at`.
#[test]
fn core_ack_before_order_publish() {
    let Some(root) = child("stop_ack::core_ack_before_order_publish") else {
        return;
    };
    let points = counted(&root, &[LAUNCH, ORDERED]);
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let launch = arm_next_with(&points, LAUNCH, &json!({"action":"pause"}));
        let ((), ()) = tokio::join!(dispatch(&engine, &session), async {
            until(|| acked(&points, LAUNCH, launch)).await;
            stop_ack(&engine).acknowledged();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let before = status(&engine, &session).await;
            assert_eq!(before["active_turn"]["cancel"], Value::Null, "{before}");
            let ordered = arm_next_with(&points, ORDERED, &json!({"action":"pause"}));
            let (reply, ()) = tokio::join!(cancel(&engine, &session, false), async {
                until(|| acked(&points, ORDERED, ordered)).await;
                let shown = status_until(&engine, &session, |status| {
                    status["active_turn"]["cancel"]["outcome"] == "acknowledged"
                })
                .await;
                assert!(
                    shown["active_turn"]["cancel"]["requested_at"].is_string(),
                    "shown over the committed order only: {shown}"
                );
                release_point(&points, ORDERED, ordered);
            });
            let requested_at = reply["cancel"]["requested_at"].clone();
            assert_eq!(
                reply["cancel"],
                pending("acknowledged", &requested_at),
                "{reply}"
            );
            release_point(&points, LAUNCH, launch);
        });
    });
}

/// `core_wall_ack_ignored`: a report with no order (a wall's soft stop,
/// which Core never ordered) shows nothing: `status` keeps `cancel: null`.
#[test]
fn core_wall_ack_ignored() {
    let Some(root) = child("stop_ack::core_wall_ack_ignored") else {
        return;
    };
    let points = counted(&root, &[LAUNCH]);
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let launch = arm_next_with(&points, LAUNCH, &json!({"action":"pause"}));
        let ((), ()) = tokio::join!(dispatch(&engine, &session), async {
            until(|| acked(&points, LAUNCH, launch)).await;
            stop_ack(&engine).acknowledged();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let shown = status(&engine, &session).await;
            assert_eq!(shown["active_turn"]["state"], "running", "{shown}");
            assert_eq!(shown["active_turn"]["cancel"], Value::Null, "{shown}");
            release_point(&points, LAUNCH, launch);
        });
    });
}

/// `core_unused_stop_ack_dropped` (d2 #3): the report's sender drops
/// unused once Core observed a cancel; the closed channel is no
/// acknowledgement (status and a non-wait cancel stay `requested`), and
/// its arm goes quiet: the run loop still takes the driver's return, and
/// the turn settles normally.
#[test]
fn core_unused_stop_ack_dropped() {
    let Some(root) = child("stop_ack::core_unused_stop_ack_dropped") else {
        return;
    };
    let points = counted(&root, &[LAUNCH]);
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let launch = arm_next_with(&points, LAUNCH, &json!({"action":"pause"}));
        let ((), ()) = tokio::join!(dispatch(&engine, &session), async {
            until(|| acked(&points, LAUNCH, launch)).await;
            let first = cancel(&engine, &session, false).await;
            let requested_at = first["cancel"]["requested_at"].clone();
            drop(super::super::lock(&engine.faults.stop_ack).take());
            tokio::time::sleep(Duration::from_millis(100)).await;
            let shown = status(&engine, &session).await;
            assert_eq!(
                shown["active_turn"]["cancel"],
                pending("requested", &requested_at),
                "{shown}"
            );
            let again = cancel(&engine, &session, false).await;
            assert_eq!(
                again["cancel"],
                pending("requested", &requested_at),
                "{again}"
            );
            release_point(&points, LAUNCH, launch);
        });
        let ended = status(&engine, &session).await;
        assert_eq!(ended["active_turn"], Value::Null, "{ended}");
    });
}

/// The slot's rule (D7): `acknowledged` replaces only a committed order's
/// `Requested`; with no order, or a failed `cancel.requested`, it shows
/// nothing new.
#[test]
fn ack_shown_only_over_requested() {
    let Some(root) = child("stop_ack::ack_shown_only_over_requested") else {
        return;
    };
    let _ = FAILPOINT_TOKEN;
    run(async {
        let engine = open(&root);
        let (_session, slot, _claim, _record, _effective, _orders) =
            super::running_turn_2(&engine, &root).await;
        let turn = TurnNumber::try_from(2).unwrap();
        slot.acknowledged(turn);
        assert_eq!(slot.cancel_shown(turn), None, "no order: nothing shown");
        slot.acknowledge(turn, Ack::Failed);
        slot.acknowledged(turn);
        assert_eq!(
            slot.cancel_shown(turn),
            Some(Ack::Failed),
            "a failure stands"
        );
        slot.acknowledge(turn, Ack::Requested("t0".to_owned()));
        slot.acknowledged(turn);
        assert_eq!(
            slot.cancel_shown(turn),
            Some(Ack::Acknowledged("t0".to_owned()))
        );
    });
}
