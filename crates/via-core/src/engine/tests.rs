//! Per-session dispatch, the Store-failed latch and force stop through the
//! Engine over a real Store, with the in-process fault backend. Each case
//! re-executes this binary with fake settings, since Core reads them once
//! from the environment.

use std::{
    env, fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::Ordering,
    time::Duration,
};

use serde_json::{Value, json};
use via_store::{SubmissionRecord, TerminalRecord};

use super::{Engine, Receipted};
use crate::api::{Event, EventBody, rfc3339};
use crate::{
    ApiError, DaemonStopParams, Deadline, FakeConfig, ResumeParams, SessionId, SpawnParams,
    TurnNumber,
};

const CHILD: &str = "VIA_ENGINE_TEST_CHILD";
const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// In the parent, re-runs `name` in a child with a fresh private root and fake
/// settings and returns `None`; in that child, returns the root.
fn child(name: &str) -> Option<PathBuf> {
    if let Some(root) = env::var_os(CHILD) {
        return Some(PathBuf::from(root));
    }
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    for part in ["state", "state/raw", "runtime", "runtime/anchors", "sync"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.path().join(part))
            .unwrap();
    }
    // Launches fail: the anchor binary is absent, so no vendor ever runs.
    let vendor = root.path().join("vendor.sh");
    fs::write(&vendor, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&vendor, fs::Permissions::from_mode(0o700)).unwrap();
    let scenario = root.path().join("scenario.json");
    fs::write(&scenario, b"{}").unwrap();
    let status = Command::new(env::current_exe().unwrap())
        .args(["--exact", &format!("engine::tests::{name}"), "--nocapture"])
        .env(CHILD, root.path())
        .env("VIA_FAKE_AGENT_BINARY", &vendor)
        .env("VIA_FAKE_SCENARIO", &scenario)
        .env("VIA_FAKE_SYNC_DIR", root.path().join("sync"))
        .status()
        .unwrap();
    assert!(status.success(), "{name} child failed: {status}");
    None
}

fn run(body: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body);
}

fn open(root: &Path) -> Engine {
    open_with(root, super::DAEMON_QUEUE_LIMIT)
}

fn open_with(root: &Path, start_capacity: usize) -> Engine {
    Engine::open_with(
        &root.join("state"),
        &root.join("runtime"),
        FakeConfig::from_environment().unwrap(),
        root.join("absent-anchor"),
        start_capacity,
    )
    .unwrap()
}

fn spawn_raw(key: Option<&str>) -> (SpawnParams, String) {
    let mut raw = json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE});
    if let Some(key) = key {
        raw["idempotency_key"] = json!(key);
    }
    (
        serde_json::from_value(raw.clone()).unwrap(),
        raw.to_string(),
    )
}

fn resume_raw(session: &SessionId, key: Option<&str>) -> (ResumeParams, String) {
    let mut raw = json!({"session":session.as_str(),"handle":HANDLE,"prompt":"q"});
    if let Some(key) = key {
        raw["op_key"] = json!(key);
    }
    (
        serde_json::from_value(raw.clone()).unwrap(),
        raw.to_string(),
    )
}

async fn spawn(engine: &Engine, key: Option<&str>) -> Result<Receipted, ApiError> {
    let (params, raw) = spawn_raw(key);
    engine.spawn(params, &raw).await
}

async fn resume(engine: &Engine, session: &SessionId, key: Option<&str>) -> Receipted {
    let (params, raw) = resume_raw(session, key);
    engine.resume(params, &raw).await.unwrap()
}

async fn new_session(engine: &Engine) -> SessionId {
    spawn(engine, None).await.unwrap().enqueued.unwrap().0
}

fn unknown_outcome() -> Value {
    json!({"kind":"store_error","commit_outcome":"unknown","retry":"same_key_only"})
}

fn force() -> DaemonStopParams {
    serde_json::from_value(json!({"force":true})).unwrap()
}

async fn shutdown(engine: &Engine) -> super::EngineShutdown {
    engine
        .shutdown(Deadline::at(
            tokio::time::Instant::now() + Duration::from_secs(5),
        ))
        .await
}

/// Runs the session's dispatcher until it returns.
async fn dispatch(engine: &Engine, session: &SessionId) {
    tokio::time::timeout(Duration::from_secs(20), engine.dispatcher(session.clone()))
        .await
        .expect("the dispatcher returns")
        .unwrap();
}

/// The session's durable event types, checking that sequences are dense.
async fn event_types(engine: &Engine, session: &SessionId) -> Vec<String> {
    let page = engine.events(session.as_str()).await.unwrap();
    let events = page["events"].as_array().unwrap();
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["seq"], json!(index + 1), "dense seq: {page}");
    }
    events
        .iter()
        .map(|event| event["type"].as_str().unwrap().to_owned())
        .collect()
}

fn turn(n: u32) -> TurnNumber {
    TurnNumber::try_from(n).unwrap()
}

/// Runtime §7, C1 §8.1: a receipt commit whose outcome is unknown is
/// `store_error` with `commit_outcome: unknown` and `retry: same_key_only`
/// and latches Store failure: no new spawn, keyed retry or resume is
/// admitted, and final shutdown is unclean (exit 4). T2-B instead read the
/// Store back and returned the receipt.
#[test]
fn an_uncertain_receipt_commit_latches_store_failure() {
    let Some(root) = child("an_uncertain_receipt_commit_latches_store_failure") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine
            .faults
            .receipt_reply_lost
            .store(true, Ordering::Release);
        let error = spawn(&engine, Some("k-1")).await.unwrap_err();
        assert_eq!((error.code, error.data()), (-32018, unknown_outcome()));
        assert!(engine.store_failed(), "the uncertain commit latched");
        for refused in [
            spawn(&engine, None).await.unwrap_err(),
            spawn(&engine, Some("k-1")).await.unwrap_err(),
            {
                let (params, raw) = resume_raw(&session, None);
                engine.resume(params, &raw).await.unwrap_err()
            },
        ] {
            assert_eq!(refused.kind, "store_error");
            assert_eq!(refused.data(), json!({"kind":"store_error"}));
        }
        // Nothing is written after a failed write: turn 1 stays queued and
        // unresolved rather than being cancelled.
        dispatch(&engine, &session).await;
        assert_eq!(event_types(&engine, &session).await, ["turn.queued"]);
        let report = shutdown(&engine).await;
        assert!(report.store_failed && !report.is_clean(), "{report:?}");
        assert!(report.unresolved_turns >= 1, "{report:?}");
    });
}

/// Runtime §7 for `resume`: a lost reply is `store_error` with
/// `commit_outcome: unknown` and latches; T2-B read it back and succeeded.
#[test]
fn an_uncertain_resume_commit_latches_store_failure() {
    let Some(root) = child("an_uncertain_resume_commit_latches_store_failure") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine
            .faults
            .receipt_reply_lost
            .store(true, Ordering::Release);
        let (params, raw) = resume_raw(&session, Some("r-1"));
        let error = engine.resume(params, &raw).await.unwrap_err();
        assert_eq!(error.data(), unknown_outcome());
        assert!(engine.store_failed());
        let (params, raw) = resume_raw(&session, Some("r-1"));
        assert_eq!(
            engine.resume(params, &raw).await.unwrap_err().kind,
            "store_error",
            "a keyed retry in the same daemon gets store_error"
        );
        assert!(!shutdown(&engine).await.is_clean());
    });
}

/// Commits turn `n`'s submission and, with `state`, its terminal directly,
/// and takes it out of the dispatcher's queue as a finished drive would;
/// `pending` is a cancelled terminal whose cleanup is still pending. Without
/// `state` the turn stays running in Store with no owner in this daemon.
async fn end_turn(engine: &Engine, session: &SessionId, n: u32, state: Option<&str>) {
    let at = rfc3339(std::time::SystemTime::now());
    let slot = engine.slot(session).unwrap();
    let head = slot.head.lock(&engine.store, session).await.unwrap();
    let event = |seq, body| {
        Event {
            seq,
            session_id: session,
            turn: Some(n),
            late: false,
            at: &at,
            raw_ref: None,
            body,
        }
        .to_value()
        .unwrap()
    };
    let seq = head.next();
    engine
        .store
        .commit_submission(SubmissionRecord {
            session_id: session.clone(),
            turn: turn(n),
            event: event(seq, EventBody::TurnSubmitted { attempt: 1 }),
        })
        .await
        .unwrap();
    let mut count = 1;
    if let Some(state) = state {
        engine
            .store
            .commit_terminal(TerminalRecord {
                session_id: session.clone(),
                turn: turn(n),
                envelope: match state {
                    "pending" => json!({"state":"cancelled","cancel":{"cleanup":"pending"}}),
                    state => json!({"state":state,"cancel":null}),
                },
                event: event(
                    seq + 1,
                    EventBody::TurnEnded {
                        state: match state {
                            "unknown" => "unknown",
                            "pending" => "cancelled",
                            _ => "failed",
                        },
                        failure: None,
                        stop_reason: "error",
                        cancel: None,
                    },
                ),
                raw_ref: None,
            })
            .await
            .unwrap();
        count = 2;
        engine.unresolved.resolve(session, turn(n));
    }
    head.committed(count);
    slot.pop(turn(n));
    engine.queued.fetch_sub(1, Ordering::AcqRel);
    engine.active.fetch_sub(1, Ordering::AcqRel);
}

async fn end_turn_one(engine: &Engine, session: &SessionId, state: Option<&str>) {
    end_turn(engine, session, 1, state).await;
}

/// Runs the dispatcher and reports whether turn `n` was submitted.
async fn submitted(engine: &Engine, session: &SessionId, n: u32) -> bool {
    dispatch(engine, session).await;
    let result = engine
        .result(&format!("{}/{n}", session.as_str()))
        .await
        .unwrap();
    !result["timestamps"]["submitted_at"].is_null()
}

/// Dispatch follows the durable predecessor, not how its drive ended.
/// Behind a durable, settled terminal whose drive ended uncleanly, both a turn
/// queued before it ended and a later resume run. (A dispatched turn here
/// ends `unknown`, since no anchor exists, so each case uses turn 2.)
#[test]
fn a_turn_behind_a_settled_terminal_runs_however_its_drive_ended() {
    let Some(root) = child("a_turn_behind_a_settled_terminal_runs_however_its_drive_ended") else {
        return;
    };
    run(async {
        let engine = open(&root);
        // Turn 2 queued while turn 1 ran.
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        end_turn_one(&engine, &session, Some("failed")).await;
        assert!(
            submitted(&engine, &session, 2).await,
            "the queued turn runs"
        );
        // A later resume, accepted after turn 1's unclean drive ended.
        let session = new_session(&engine).await;
        end_turn_one(&engine, &session, Some("failed")).await;
        resume(&engine, &session, None).await;
        assert!(submitted(&engine, &session, 2).await, "a later resume runs");
        assert!(!engine.store_failed());
    });
}

/// C1 §7.3: while the predecessor is durably `unknown`, or its cleanup is
/// pending, each successor is cancelled without submission.
#[test]
fn successors_are_cancelled_behind_an_unknown_or_cleanup_pending_predecessor() {
    let Some(root) =
        child("successors_are_cancelled_behind_an_unknown_or_cleanup_pending_predecessor")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        for state in [Some("unknown"), Some("pending")] {
            let session = new_session(&engine).await;
            resume(&engine, &session, None).await;
            end_turn_one(&engine, &session, state).await;
            assert!(!submitted(&engine, &session, 2).await, "{state:?}");
            resume(&engine, &session, None).await;
            assert!(!submitted(&engine, &session, 3).await, "{state:?}");
        }
        assert_eq!(engine.active(), 0);
    });
}

/// Round 3 blocker 2: a failed predecessor read leaves the turn queued; the
/// decision is retried on the read timer and the turn later runs.
#[test]
fn a_failed_predecessor_read_waits_and_the_turn_later_runs() {
    let Some(root) = child("a_failed_predecessor_read_waits_and_the_turn_later_runs") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        end_turn_one(&engine, &session, Some("failed")).await;
        engine
            .faults
            .predecessors_unreadable
            .store(2, Ordering::Release);
        assert!(
            submitted(&engine, &session, 2).await,
            "the turn runs once the read succeeds"
        );
        assert_eq!(
            engine
                .faults
                .predecessors_unreadable
                .load(Ordering::Acquire),
            0
        );
        assert!(!engine.store_failed(), "a read failure never latches");
    });
}

/// Runtime §7: a submission commit whose outcome is unknown after the grant
/// latches Store failure; the turn is never retried and no vendor launches.
/// T2-B recorded the turn failed and kept admitting new work.
#[test]
fn a_failed_submission_commit_latches_and_launches_nothing() {
    let Some(root) = child("a_failed_submission_commit_latches_and_launches_nothing") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine
            .faults
            .submission_reply_lost
            .store(true, Ordering::Release);
        dispatch(&engine, &session).await;
        assert!(engine.store_failed(), "the uncertain submission latched");
        assert_eq!(engine.stop_mode(), Some(super::StopMode::Force));
        assert_eq!(
            spawn(&engine, None).await.unwrap_err().kind,
            "store_error",
            "admission stopped"
        );
        // The commit did land, but nothing followed it.
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.submitted"]
        );
        let read = engine
            .result(&format!("{}/1", session.as_str()))
            .await
            .unwrap_err();
        assert_eq!(read.kind, "store_error");
        let report = shutdown(&engine).await;
        assert_eq!(report.anchors, 0, "no vendor launched: {report:?}");
        assert!(report.store_failed && !report.is_clean(), "{report:?}");
    });
}

/// C1 §3.14 force before the grant, on a queued-only session: every queued
/// turn is cancelled without submission and `session.closed` commits with
/// the last one; the shutdown is clean. T2-B submitted turn 1 after force.
#[test]
fn force_on_a_queued_only_session_cancels_its_turns_and_closes_it() {
    let Some(root) = child("force_on_a_queued_only_session_cancels_its_turns_and_closes_it") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        resume(&engine, &session, None).await;
        assert_eq!(
            engine.request_stop(&force()).await.unwrap(),
            super::StopMode::Force
        );
        dispatch(&engine, &session).await;
        assert_eq!(
            event_types(&engine, &session).await,
            [
                "turn.queued",
                "turn.queued",
                "turn.queued",
                "turn.ended",
                "turn.ended",
                "turn.ended",
                "session.closed",
            ]
        );
        let page = engine.events(session.as_str()).await.unwrap();
        assert_eq!(page["events"][6]["reason"], "daemon_stop_force");
        for n in 1..=3 {
            let envelope = engine
                .result(&format!("{}/{n}", session.as_str()))
                .await
                .unwrap();
            assert_eq!(envelope["state"], "cancelled", "{envelope}");
            assert!(envelope["timestamps"]["submitted_at"].is_null());
        }
        let report = shutdown(&engine).await;
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.anchors, 0);
    });
}

/// Design §4, force accepted after the dispatch decision but before the
/// grant: the grant is refused, so the turn is cancelled without submission
/// and its queued-only session closes. T2-B submitted a turn decided `Run`
/// without checking force.
#[test]
fn a_force_between_the_decision_and_the_grant_refuses_the_grant() {
    let Some(root) = child("a_force_between_the_decision_and_the_grant_refuses_the_grant") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine
            .faults
            .hold_before_grant
            .store(true, Ordering::Release);
        let ((), ()) = tokio::join!(dispatch(&engine, &session), async {
            engine.faults.granted.notified().await;
            engine.request_stop(&force()).await.unwrap();
            engine.faults.release.notify_one();
        });
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended", "session.closed"]
        );
        let report = shutdown(&engine).await;
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.anchors, 0);
    });
}

/// Design §4, grant before force: the granted turn is submitted, then the
/// already-latched force reaches Route before any launch; the turn ends under
/// the C1 §7.6 force row (`cancelled`, `requested`) and the session closes.
#[test]
fn a_turn_granted_before_force_submits_then_ends_forced_without_launch() {
    let Some(root) = child("a_turn_granted_before_force_submits_then_ends_forced_without_launch")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine
            .faults
            .hold_after_grant
            .store(true, Ordering::Release);
        let ((), ()) = tokio::join!(dispatch(&engine, &session), async {
            engine.faults.granted.notified().await;
            engine.request_stop(&force()).await.unwrap();
            engine.faults.release.notify_one();
        });
        let report = shutdown(&engine).await;
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.anchors, 0, "nothing launched: {report:?}");
        let envelope = engine
            .result(&format!("{}/1", session.as_str()))
            .await
            .unwrap();
        assert_eq!(envelope["state"], "cancelled", "{envelope}");
        assert_eq!(envelope["cancel"]["outcome"], "requested", "{envelope}");
        assert_eq!(envelope["cancel"]["cleanup"], "quiescent", "{envelope}");
        assert!(envelope["timestamps"]["accepted_at"].is_null());
        assert_eq!(
            event_types(&engine, &session).await,
            [
                "turn.queued",
                "turn.submitted",
                "cancel.requested",
                "cancel.settled",
                "turn.ended",
                "session.closed",
            ]
        );
    });
}

/// Design §2.3: a `queued → cancelled` commit that fails under force latches;
/// nothing more is written, the session is not closed, and the shutdown is
/// unclean (exit 4).
#[test]
fn a_failed_cancellation_under_force_is_unclean_and_leaves_the_session_open() {
    let Some(root) =
        child("a_failed_cancellation_under_force_is_unclean_and_leaves_the_session_open")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        engine.request_stop(&force()).await.unwrap();
        engine.faults.cancel_fails.store(1, Ordering::Release);
        dispatch(&engine, &session).await;
        assert!(engine.store_failed());
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.queued"],
            "no cancellation after the failed one, and no session.closed"
        );
        let report = shutdown(&engine).await;
        assert!(!report.is_clean(), "{report:?}");
        assert_eq!(report.unresolved_turns, 2, "{report:?}");
    });
}

/// Design §5: a start that finds the channel full waits in the pending set
/// and enters the channel once daemon main takes a start.
#[test]
fn a_start_that_finds_the_channel_full_is_retried_when_capacity_returns() {
    let Some(root) = child("a_start_that_finds_the_channel_full_is_retried_when_capacity_returns")
    else {
        return;
    };
    run(async {
        let engine = open_with(&root, 1);
        let mut starts = engine.take_starts().unwrap();
        let first = new_session(&engine).await;
        let second = new_session(&engine).await;
        assert!(engine.starts_pending(), "the second start found it full");
        assert_eq!(starts.try_recv().unwrap(), first);
        engine.retry_starts();
        assert!(!engine.starts_pending());
        assert_eq!(starts.try_recv().unwrap(), second);
        let report = shutdown(&engine).await;
        assert_eq!(report.unstarted_dispatchers, 2, "{report:?}");
    });
}

/// Design §2: dispatcher exits, which retire their slot, race receipt commits
/// on the same session. Retirement runs under admission and only unleased, so
/// the session keeps one event head: every event is dense and every turn ends.
#[test]
fn slot_retirement_racing_receipts_keeps_one_event_head() {
    let Some(root) = child("slot_retirement_racing_receipts_keeps_one_event_head") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        end_turn_one(&engine, &session, Some("failed")).await;
        for _ in 0..12 {
            // Daemon main starts a dispatcher only for a slot that exists.
            let run = async {
                if engine.slot(&session).is_some() {
                    dispatch(&engine, &session).await;
                }
            };
            let ((), receipted) = tokio::join!(run, async {
                tokio::task::yield_now().await;
                resume(&engine, &session, None).await
            });
            assert!(receipted.enqueued.is_some());
        }
        if engine.slot(&session).is_some() {
            dispatch(&engine, &session).await;
        }
        assert!(!engine.store_failed(), "no event collided");
        let types = event_types(&engine, &session).await;
        let ended = types.iter().filter(|kind| *kind == "turn.ended").count();
        assert_eq!(ended, 13, "{types:?}");
        assert!(engine.slot(&session).is_none(), "the idle slot retired");
        assert_eq!(engine.active(), 0);
    });
}

/// Design §7: waiting turns cost no reads of their own. Eight sessions each
/// hold eight queued turns whose predecessor reads keep failing; over three
/// seconds each dispatcher reads only on its backoff timer (250 ms doubling),
/// not on T2-B's fixed 250 ms recheck (96 reads here at T2-B's head).
#[test]
fn waiting_turns_are_read_per_session_on_backoff() {
    const SESSIONS: usize = 8;
    let Some(root) = child("waiting_turns_are_read_per_session_on_backoff") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let mut sessions = Vec::new();
        for _ in 0..SESSIONS {
            let session = new_session(&engine).await;
            for _ in 0..7 {
                resume(&engine, &session, None).await;
            }
            sessions.push(session);
        }
        engine
            .faults
            .predecessors_unreadable
            .store(usize::MAX, Ordering::Release);
        engine.faults.reads.store(0, Ordering::Release);
        let dispatchers = sessions
            .iter()
            .map(|session| engine.dispatcher(session.clone()));
        let all = futures_join_all(dispatchers);
        assert!(
            tokio::time::timeout(Duration::from_secs(3), all)
                .await
                .is_err(),
            "the turns keep waiting"
        );
        let reads = engine.faults.reads.load(Ordering::Acquire);
        // Reads at 0, 0.25, 0.75, 1.75 s per session, plus scheduling slack.
        assert!(
            reads <= SESSIONS * 5,
            "{reads} reads for {SESSIONS} sessions"
        );
        assert!(reads >= SESSIONS, "each dispatcher read at least once");
        assert_eq!(engine.queued.load(Ordering::Acquire), SESSIONS * 8);
    });
}

/// Polls every future to completion together on the current task.
async fn futures_join_all<F: Future>(futures: impl IntoIterator<Item = F>) {
    let mut set: Vec<_> = futures.into_iter().map(Box::pin).collect();
    std::future::poll_fn(|context| {
        set.retain_mut(|future| future.as_mut().poll(context).is_pending());
        if set.is_empty() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
}

/// Round 1, decision 2: the latch takes `admission`, which a receipt holds
/// through its commit, enqueue and start request. A spawn paused inside its
/// receipt while another turn's failed submission latches either completes
/// first, fully enqueued with a start, or is refused; it never succeeds
/// after the latch.
#[test]
fn a_receipt_in_flight_when_another_turn_latches_completes_before_the_latch() {
    let Some(root) =
        child("a_receipt_in_flight_when_another_turn_latches_completes_before_the_latch")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let mut starts = engine.take_starts().unwrap();
        let first = new_session(&engine).await;
        assert_eq!(starts.try_recv().unwrap(), first);
        engine
            .faults
            .submission_reply_lost
            .store(true, Ordering::Release);
        engine.faults.hold_receipt.store(true, Ordering::Release);
        let (receipted, ()) = tokio::join!(
            async {
                let receipted = spawn(&engine, None).await;
                (receipted, engine.store_failed())
            },
            async {
                engine.faults.granted.notified().await;
                let ((), ()) = tokio::join!(
                    // The other turn's submission reply is lost: it latches.
                    dispatch(&engine, &first),
                    async {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        engine.faults.release.notify_one();
                    }
                );
            },
        );
        let (receipted, latched_at_return) = receipted;
        assert!(engine.store_failed(), "the other turn latched");
        match receipted {
            Ok(receipted) => {
                assert!(!latched_at_return, "a receipt succeeded after the latch");
                let (session, _) = receipted.enqueued.unwrap();
                assert_eq!(starts.try_recv().unwrap(), session, "its start was sent");
                assert!(engine.unresolved.turns().contains(&(session, turn(1))));
            }
            Err(refused) => assert_eq!(refused.kind, "store_error"),
        }
    });
}

/// Round 1, decision 2: a closing cancellation decides `session.closed`
/// under `admission`, after the latch check. Paused before that point while
/// another turn latches, it writes nothing more and closes nothing.
#[test]
fn a_closing_cancellation_after_another_turn_latched_commits_no_close() {
    let Some(root) = child("a_closing_cancellation_after_another_turn_latched_commits_no_close")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        engine.request_stop(&force()).await.unwrap();
        engine
            .faults
            .hold_before_close
            .store(true, Ordering::Release);
        let ((), ()) = tokio::join!(dispatch(&engine, &session), async {
            engine.faults.granted.notified().await;
            // Another turn's failed write.
            engine.latch().await;
            engine.faults.release.notify_one();
        });
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.queued", "turn.ended"],
            "turn 1 cancelled before the latch; nothing after it"
        );
        let report = shutdown(&engine).await;
        assert!(!report.is_clean(), "{report:?}");
    });
}

/// Round 1, decision 3: force accepted while the last queued turn's
/// cancellation (behind an `unknown` predecessor) reads; that cancellation
/// commits without `session.closed` and the dispatcher exits. Final
/// shutdown's closure pass then closes the session durably, and the
/// shutdown is clean.
#[test]
fn force_during_the_last_cancellation_is_closed_by_the_closure_pass() {
    let Some(root) = child("force_during_the_last_cancellation_is_closed_by_the_closure_pass")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        end_turn_one(&engine, &session, Some("unknown")).await;
        engine
            .faults
            .hold_cancel_read
            .store(true, Ordering::Release);
        let ((), ()) = tokio::join!(dispatch(&engine, &session), async {
            engine.faults.granted.notified().await;
            engine.request_stop(&force()).await.unwrap();
            engine.faults.release.notify_one();
        });
        assert!(
            !event_types(&engine, &session)
                .await
                .contains(&"session.closed".to_owned()),
            "the in-path cancellation did not close"
        );
        let report = shutdown(&engine).await;
        assert!(report.is_clean(), "{report:?}");
        let types = event_types(&engine, &session).await;
        assert_eq!(types.last().map(String::as_str), Some("session.closed"));
        let page = engine.events(session.as_str()).await.unwrap();
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.last().unwrap()["reason"], "daemon_stop_force");
    });
}

/// Round 1, decision 4: under force a failed cancellation read retries with
/// backoff; the queued turns are then cancelled and the session closed.
#[test]
fn a_failed_read_under_force_retries_then_cancels_and_closes() {
    let Some(root) = child("a_failed_read_under_force_retries_then_cancels_and_closes") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        engine.request_stop(&force()).await.unwrap();
        engine.faults.cancel_read_fails.store(1, Ordering::Release);
        dispatch(&engine, &session).await;
        assert_eq!(
            event_types(&engine, &session).await,
            [
                "turn.queued",
                "turn.queued",
                "turn.ended",
                "turn.ended",
                "session.closed"
            ]
        );
        assert!(!engine.store_failed(), "a read failure never latches");
        let report = shutdown(&engine).await;
        assert!(report.is_clean(), "{report:?}");
    });
}
