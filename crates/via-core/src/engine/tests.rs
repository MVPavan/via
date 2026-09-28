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
        None,
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

/// Final shutdown under the daemon's one 10 s budget, from which the
/// design §6.8 table measures its reserves.
async fn shutdown(engine: &Engine) -> super::EngineShutdown {
    engine
        .shutdown(Deadline::at(
            tokio::time::Instant::now() + Duration::from_secs(10),
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

/// C1 P6: while the predecessor is durably `unknown`, each successor is
/// cancelled without submission.
#[test]
fn successors_are_cancelled_behind_an_unknown_predecessor() {
    let Some(root) = child("successors_are_cancelled_behind_an_unknown_predecessor") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        end_turn_one(&engine, &session, Some("unknown")).await;
        assert!(!submitted(&engine, &session, 2).await);
        resume(&engine, &session, None).await;
        assert!(!submitted(&engine, &session, 3).await);
        assert_eq!(engine.active(), 0);
    });
}

/// C1 §7.3 (T2-C review decision 1): a predecessor whose cleanup is still
/// `pending` gives Wait, not Cancel: the successor stays queued, neither
/// submitted nor cancelled. Store can hold such an envelope only as this
/// synthetic terminal row (under P7 a pending-cleanup turn is nonterminal
/// and the unresolved check already waits), so the decision is tested here.
/// T2-B cancelled the successor.
#[test]
fn a_successor_waits_behind_a_cleanup_pending_predecessor() {
    let Some(root) = child("a_successor_waits_behind_a_cleanup_pending_predecessor") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        end_turn_one(&engine, &session, Some("pending")).await;
        let waited =
            tokio::time::timeout(Duration::from_secs(1), engine.dispatcher(session.clone())).await;
        assert!(waited.is_err(), "the dispatcher keeps waiting");
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.queued", "turn.submitted", "turn.ended"],
            "turn 2 is neither submitted nor cancelled"
        );
        let pending = engine
            .result(&format!("{}/2", session.as_str()))
            .await
            .unwrap_err();
        assert_eq!(pending.kind, "turn_not_finished");
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
            engine.faults.grant_paused.notified().await;
            engine.request_stop(&force()).await.unwrap();
            engine.faults.grant_release.notify_one();
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

/// Design §2.3 and §7.2 row 9: a `queued → cancelled` commit under force
/// that fails, and whose one same-sequence retry fails too, latches;
/// nothing more is written, the session is not closed, and the shutdown is
/// unclean (exit 4). Re-pointed in S5: one not-committed failure is now
/// retried, so the fault fails both attempts.
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
        engine.faults.cancel_fails.store(2, Ordering::Release);
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
                (receipted, engine.latch_finalized())
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
                assert!(
                    !latched_at_return,
                    "a receipt succeeded after the latch finalized"
                );
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

/// Round 2, decision 1, with round 3's interleaving: the latch's first
/// phase is synchronous. C's dispatcher decides `Run` and is paused before
/// its grant. A is then paused inside its receipt, holding `admission`. B's
/// submission reply is lost, so B marks the failure pending and waits for
/// `admission` to finalize. Released, C's grant is refused: no
/// `turn.submitted`, no anchor, no vendor launch. Released next, A completes
/// and is counted with its start sent, the latch finalizes, and the shutdown
/// is unclean.
#[test]
fn a_pending_failure_refuses_grants_while_a_receipt_holds_admission() {
    let Some(root) = child("a_pending_failure_refuses_grants_while_a_receipt_holds_admission")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let mut starts = engine.take_starts().unwrap();
        let b = new_session(&engine).await;
        let c = new_session(&engine).await;
        while starts.try_recv().is_ok() {}
        engine
            .faults
            .hold_before_grant
            .store(true, Ordering::Release);
        let ((), a) = tokio::join!(
            // C decides `Run` and pauses before its grant.
            dispatch(&engine, &c),
            async {
                engine.faults.grant_paused.notified().await;
                engine.faults.hold_receipt.store(true, Ordering::Release);
                let (a, ()) = tokio::join!(spawn(&engine, None), async {
                    // A now holds `admission` inside its receipt.
                    engine.faults.granted.notified().await;
                    engine
                        .faults
                        .submission_reply_lost
                        .store(true, Ordering::Release);
                    let ((), ()) = tokio::join!(
                        // B's lost submission reply marks the failure pending,
                        // then waits for `admission`, which A holds.
                        dispatch(&engine, &b),
                        async {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            assert!(engine.store_failed(), "the failure is pending");
                            assert!(!engine.latch_finalized(), "A still holds admission");
                            engine.faults.grant_release.notify_one();
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            assert_eq!(
                                event_types(&engine, &c).await,
                                ["turn.queued"],
                                "C's grant was refused"
                            );
                            engine.faults.release.notify_one();
                        }
                    );
                });
                a
            },
        );
        let (a, _) = a.unwrap().enqueued.unwrap();
        assert_eq!(starts.try_recv().unwrap(), a, "A's start was sent");
        assert!(
            engine.unresolved.turns().contains(&(a, turn(1))),
            "A is counted"
        );
        assert!(engine.latch_finalized());
        assert_eq!(spawn(&engine, None).await.unwrap_err().kind, "store_error");
        let report = shutdown(&engine).await;
        assert_eq!(report.anchors, 0, "C launched nothing: {report:?}");
        assert!(report.store_failed && !report.is_clean(), "{report:?}");
    });
}

/// Round 3, decision 1: a closing cancellation paused after its failure and
/// close checks may still complete once another session's write fails;
/// Store's same-transaction refusal is the closure proof. With no other
/// queued or running turn in Store the close commits; the exit is still 4.
#[test]
fn a_close_past_its_check_commits_when_store_proves_no_other_turn() {
    let Some(root) = child("a_close_past_its_check_commits_when_store_proves_no_other_turn") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine.request_stop(&force()).await.unwrap();
        close_racing_a_failure(&engine, &session).await;
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended", "session.closed"]
        );
        let report = shutdown(&engine).await;
        assert!(report.store_failed && !report.is_clean(), "{report:?}");
    });
}

/// Round 3, decision 1, variant: the session also holds a committed queued
/// turn this daemon never registered (left by an earlier daemon here). The
/// close passed its check before the failure, and Store refuses
/// `session.closed` in the same transaction: no close, exit 4.
#[test]
fn a_close_past_its_check_is_refused_by_store_for_an_unregistered_turn() {
    let Some(root) = child("a_close_past_its_check_is_refused_by_store_for_an_unregistered_turn")
    else {
        return;
    };
    run(async {
        let session = {
            let earlier = open(&root);
            new_session(&earlier).await
        };
        let engine = open(&root);
        resume(&engine, &session, None).await;
        engine.request_stop(&force()).await.unwrap();
        close_racing_a_failure(&engine, &session).await;
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.queued", "turn.ended"]
        );
        let report = shutdown(&engine).await;
        assert!(report.store_failed && !report.is_clean(), "{report:?}");
    });
}

/// Pauses the session's closing cancellation after its checks, has another
/// session's write fail (the latch's first phase runs at once; its second
/// waits for the `admission` the closer holds), then releases the closer.
async fn close_racing_a_failure(engine: &Engine, session: &SessionId) {
    engine
        .faults
        .hold_after_close_check
        .store(true, Ordering::Release);
    let ((), ()) = tokio::join!(dispatch(engine, session), async {
        engine.faults.granted.notified().await;
        // Another session's failed write.
        let finalize = engine.latch();
        assert!(engine.store_failed(), "the failure is pending");
        engine.faults.release.notify_one();
        finalize.await;
    });
}

/// Round 2, decision 2: an older durable queued turn left by an earlier daemon
/// is not in this daemon's memory. A new resume queues turn 2; force cancels
/// it as the session's last known turn, and Store refuses `session.closed`
/// in the same transaction because turn 1 is still queued. The closure pass
/// then counts the session unclosed: no `session.closed`, exit 4.
#[test]
fn force_never_closes_a_session_with_an_older_queued_turn() {
    let Some(root) = child("force_never_closes_a_session_with_an_older_queued_turn") else {
        return;
    };
    run(async {
        let session = {
            let earlier = open(&root);
            new_session(&earlier).await
        };
        let engine = open(&root);
        resume(&engine, &session, None).await;
        engine.request_stop(&force()).await.unwrap();
        dispatch(&engine, &session).await;
        let types = event_types(&engine, &session).await;
        assert_eq!(
            types,
            ["turn.queued", "turn.queued", "turn.ended"],
            "{types:?}"
        );
        assert!(
            !engine.store_failed(),
            "a refused close is not a Store failure"
        );
        let report = shutdown(&engine).await;
        assert_eq!(report.unclosed_sessions, 1, "{report:?}");
        assert!(!report.is_clean(), "{report:?}");
        assert!(
            !event_types(&engine, &session)
                .await
                .contains(&"session.closed".to_owned())
        );
    });
}

/// Round 2, decision 3: a force-path read that stalls past final shutdown's
/// read cutoff is abandoned: the turn stays unresolved, no further read is
/// issued, and the dispatcher returns in time for the shutdown's bound.
#[test]
fn a_stalled_force_path_read_expires_at_the_cutoff() {
    let Some(root) = child("a_stalled_force_path_read_expires_at_the_cutoff") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine.request_stop(&force()).await.unwrap();
        // Never released: the read stalls.
        engine
            .faults
            .hold_cancel_read
            .store(true, Ordering::Release);
        let started = tokio::time::Instant::now();
        // The cutoff is `FINALIZE_RESERVE + 3 s` = 8 s before this
        // deadline (design §6.7 [r5.10]): 500 ms from now.
        engine.begin_final_shutdown(started + Duration::from_millis(8_500));
        dispatch(&engine, &session).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "dispatcher took {elapsed:?}"
        );
        assert_eq!(event_types(&engine, &session).await, ["turn.queued"]);
        let report = shutdown(&engine).await;
        assert!(
            report.unresolved_turns >= 1 && !report.is_clean(),
            "{report:?}"
        );
    });
}

async fn cancel(engine: &Engine, session: &SessionId, n: u32) -> Result<Value, ApiError> {
    let params =
        serde_json::from_value(json!({"session":session.as_str(),"handle":HANDLE,"turn":n}))
            .unwrap();
    engine.cancel(params).await
}

async fn close(engine: &Engine, session: &SessionId, key: Option<&str>) -> Result<Value, ApiError> {
    let mut raw = json!({"session":session.as_str(),"handle":HANDLE});
    if let Some(key) = key {
        raw["op_key"] = json!(key);
    }
    let params = serde_json::from_value(raw.clone()).unwrap();
    engine.close(params, &raw.to_string()).await
}

/// Polls `ready` while the other futures of the test's join make progress.
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the condition is reached");
}

/// Runs the session's dispatcher once its close order is set, so the close
/// pass, not a dispatch decision, meets the queued turn.
async fn dispatch_closing(engine: &Engine, session: &SessionId) {
    until(|| {
        engine
            .slot(session)
            .is_some_and(|slot| slot.close_watch().is_some())
    })
    .await;
    dispatch(engine, session).await;
}

/// Design §3.1 [r1.3]: a cancel orders a `Claimed` turn; its submission read
/// fails, so the claim rolls back to one `Cancelling{dispatcher}`
/// cancellation. The first caller rejoins it, a second cancel subscribes to
/// it, and exactly one terminal is written, with no submission.
#[test]
fn a_claim_rollback_has_one_cancellation_owner() {
    let Some(root) = child("a_claim_rollback_has_one_cancellation_owner") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine
            .faults
            .hold_after_grant
            .store(true, Ordering::Release);
        let ((), (first, second)) = tokio::join!(dispatch(&engine, &session), async {
            // Claimed and granted, before its submission.
            engine.faults.granted.notified().await;
            engine
                .faults
                .hold_after_grant
                .store(false, Ordering::Release);
            let slot = engine.slot(&session).unwrap();
            tokio::join!(cancel(&engine, &session, 1), async {
                // The first cancel waits on the claimed turn's order.
                until(|| slot.watchers(turn(1)).0 == 1).await;
                engine
                    .faults
                    .submission_unread
                    .store(true, Ordering::Release);
                engine
                    .faults
                    .hold_cancel_read
                    .store(true, Ordering::Release);
                engine.faults.release.notify_one();
                // The rollback's dispatcher cancellation, before its read.
                engine.faults.granted.notified().await;
                let (second, ()) = tokio::join!(cancel(&engine, &session, 1), async {
                    // Both callers joined the one cancellation.
                    until(|| slot.watchers(turn(1)).1 == 2).await;
                    engine.faults.release.notify_one();
                });
                second
            })
        });
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(first["state"], "cancelled", "{first}");
        assert_eq!(first["already_terminal"], false, "{first}");
        assert_eq!(first, second);
        assert_eq!(first["cancel"]["outcome"], "acknowledged", "{first}");
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended"]
        );
        let envelope = engine
            .result(&format!("{}/1", session.as_str()))
            .await
            .unwrap();
        assert!(
            envelope["timestamps"]["submitted_at"].is_null(),
            "{envelope}"
        );
    });
}

/// Design §7.3 [r1.13]: callers joined to a dispatcher-owned cancellation
/// get a plain `store_error` when its read fails; the dispatcher keeps the
/// claim, retries on its timer and commits the cancellation.
#[test]
fn a_dispatcher_cancellation_read_failure_replies_store_error_to_joined_callers() {
    let Some(root) =
        child("a_dispatcher_cancellation_read_failure_replies_store_error_to_joined_callers")
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
        let ((), (first, second)) = tokio::join!(dispatch(&engine, &session), async {
            engine.faults.granted.notified().await;
            engine
                .faults
                .hold_after_grant
                .store(false, Ordering::Release);
            let slot = engine.slot(&session).unwrap();
            tokio::join!(cancel(&engine, &session, 1), async {
                until(|| slot.watchers(turn(1)).0 == 1).await;
                engine
                    .faults
                    .submission_unread
                    .store(true, Ordering::Release);
                engine
                    .faults
                    .hold_cancel_read
                    .store(true, Ordering::Release);
                engine.faults.cancel_read_fails.store(1, Ordering::Release);
                engine.faults.release.notify_one();
                // The rollback's dispatcher cancellation, before its read.
                engine.faults.granted.notified().await;
                let (second, ()) = tokio::join!(cancel(&engine, &session, 1), async {
                    until(|| slot.watchers(turn(1)).1 == 2).await;
                    engine.faults.release.notify_one();
                });
                second
            })
        });
        for reply in [first, second] {
            let error = reply.unwrap_err();
            assert_eq!(error.kind, "store_error");
            assert!(error.data().get("commit_outcome").is_none(), "plain");
        }
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended"],
            "the retried cancellation committed"
        );
        let envelope = engine
            .result(&format!("{}/1", session.as_str()))
            .await
            .unwrap();
        assert_eq!(envelope["state"], "cancelled", "{envelope}");
        assert!(!engine.store_failed(), "a read failure never latches");
    });
}

/// Design §4 step 5 [r3.1]: a second close and a keyed replay arriving while
/// the first close is held after its absence check wait without holding
/// `admission` (another session's `resume` commits meanwhile), and all three
/// reply from the first close's one outcome.
#[test]
fn close_callers_wait_without_admission_and_share_one_outcome() {
    let Some(root) = child("close_callers_wait_without_admission_and_share_one_outcome") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let other = new_session(&engine).await;
        engine
            .faults
            .hold_before_closed
            .store(true, Ordering::Release);
        let (first, (), (second, replay, ())) = tokio::join!(
            close(&engine, &session, Some("k")),
            dispatch_closing(&engine, &session),
            async {
                engine.faults.granted.notified().await;
                tokio::join!(
                    close(&engine, &session, None),
                    close(&engine, &session, Some("k")),
                    async {
                        until(|| {
                            engine
                                .slot(&session)
                                .and_then(|slot| slot.close_watch())
                                .is_some_and(|watch| watch.receiver_count() == 3)
                        })
                        .await;
                        tokio::time::timeout(Duration::from_secs(5), resume(&engine, &other, None))
                            .await
                            .expect("no close caller holds admission");
                        engine.faults.release.notify_one();
                    }
                )
            }
        );
        let first = first.unwrap();
        assert_eq!(first["state"], "closed", "{first}");
        assert_eq!(
            first["cancelled_turns"],
            json!([format!("{}/1", session.as_str())])
        );
        assert_eq!(second.unwrap(), first);
        assert_eq!(replay.unwrap(), first);
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended", "session.closed"]
        );
    });
}

/// Design §4 dispatcher step 5 [r5.9, r4.6]: a latch during the close's
/// absence check is re-checked under `admission`: no `Closed` is written,
/// and the latch exit publishes `store_error` to the waiting caller.
#[test]
fn a_latch_during_the_absence_check_refuses_closed() {
    let Some(root) = child("a_latch_during_the_absence_check_refuses_closed") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine
            .faults
            .hold_before_closed
            .store(true, Ordering::Release);
        let (closed, (), ()) = tokio::join!(
            close(&engine, &session, None),
            dispatch_closing(&engine, &session),
            async {
                engine.faults.granted.notified().await;
                // Another session's failed write.
                let finalize = engine.latch();
                engine.faults.release.notify_one();
                finalize.await;
            }
        );
        assert_eq!(closed.unwrap_err().kind, "store_error");
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended"]
        );
    });
}

/// Design §4 dispatcher step 5 [r5.9, r4.6]: force accepted during the
/// close's absence check is re-checked under `admission`: no `Closed` is
/// written, and the force exit publishes `daemon_stopping`.
#[test]
fn a_force_during_the_absence_check_refuses_closed() {
    let Some(root) = child("a_force_during_the_absence_check_refuses_closed") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine
            .faults
            .hold_before_closed
            .store(true, Ordering::Release);
        let (closed, (), ()) = tokio::join!(
            close(&engine, &session, None),
            dispatch_closing(&engine, &session),
            async {
                engine.faults.granted.notified().await;
                engine.request_stop(&force()).await.unwrap();
                engine.faults.release.notify_one();
            }
        );
        assert_eq!(closed.unwrap_err().kind, "daemon_stopping");
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended"]
        );
    });
}

/// Design §4 "Restart": a session left durably `closing` by an earlier
/// daemon is finished by the restart handoff before admission. Its queued
/// turn is cancelled with cause `close`, and `Closed` commits with the
/// derived result; a `resume` is then `session_closed`.
#[test]
fn the_restart_handoff_completes_a_closing_session() {
    let Some(root) = child("the_restart_handoff_completes_a_closing_session") else {
        return;
    };
    run(async {
        let session = {
            let earlier = open(&root);
            let session = new_session(&earlier).await;
            earlier
                .store
                .commit_closing(via_store::ClosingRecord {
                    session_id: session.clone(),
                    operation: None,
                })
                .await
                .unwrap();
            session
        };
        let engine = open(&root);
        engine.recover().await.unwrap();
        let handoff = engine.hand_off_queued().await.unwrap();
        assert_eq!(
            (handoff.enqueued, handoff.cancelled, handoff.closed),
            (0, 1, 1),
            "{handoff:?}"
        );
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended", "session.closed"]
        );
        let result = engine
            .store
            .session_close_result(&session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result["cancelled_turns"],
            json!([format!("{}/1", session.as_str())]),
            "{result}"
        );
        let (params, raw) = resume_raw(&session, None);
        let refused = engine.resume(params, &raw).await.unwrap_err();
        assert_eq!(refused.kind, "session_closed");
        let report = shutdown(&engine).await;
        assert!(report.is_clean(), "{report:?}");
    });
}

/// Design §4 steps 2–4 [r1.5]: after an idle stop is accepted a new close is
/// `daemon_stopping`, and a keyed replay of a committed close still replays
/// its result.
#[test]
fn a_close_after_an_idle_stop_is_fenced_and_a_replay_still_replays() {
    let Some(root) = child("a_close_after_an_idle_stop_is_fenced_and_a_replay_still_replays")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let other = new_session(&engine).await;
        // Launches fail here: each turn ends and leaves no active work.
        dispatch(&engine, &other).await;
        let (first, ()) = tokio::join!(
            close(&engine, &session, Some("k")),
            dispatch_closing(&engine, &session)
        );
        let first = first.unwrap();
        assert_eq!(first["state"], "closed", "{first}");
        let plain = serde_json::from_value(json!({})).unwrap();
        assert_eq!(
            engine.request_stop(&plain).await.unwrap(),
            super::StopMode::Idle
        );
        let fenced = tokio::time::timeout(Duration::from_secs(5), close(&engine, &other, None))
            .await
            .expect("the fence replies at once");
        assert_eq!(fenced.unwrap_err().kind, "daemon_stopping");
        assert_eq!(close(&engine, &session, Some("k")).await.unwrap(), first);
    });
}

fn plain() -> DaemonStopParams {
    serde_json::from_value(json!({})).unwrap()
}

/// Design §6.3 [r3.5, r5.12]: with no active turn, a plain stop is still
/// refused while a session is in the durable closing set, which
/// `daemon/status` reports; once `Closed` commits it is accepted.
#[test]
fn a_plain_stop_is_refused_while_a_session_is_closing() {
    let Some(root) = child("a_plain_stop_is_refused_while_a_session_is_closing") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        engine
            .faults
            .hold_before_closed
            .store(true, Ordering::Release);
        let (closed, (), ()) = tokio::join!(
            close(&engine, &session, None),
            dispatch_closing(&engine, &session),
            async {
                engine.faults.granted.notified().await;
                assert_eq!(engine.active(), 0);
                assert_eq!(engine.counts().closing, 1);
                let refused = engine.request_stop(&plain()).await.unwrap_err();
                assert_eq!(refused.message, "sessions are active");
                assert_eq!(engine.stop_mode(), None);
                engine.faults.release.notify_one();
            }
        );
        assert_eq!(closed.unwrap()["state"], "closed");
        assert_eq!(engine.counts().closing, 0);
        assert_eq!(
            engine.request_stop(&plain()).await.unwrap(),
            super::StopMode::Idle
        );
    });
}

/// Design §6.3 [O3, r5.12]: the force set is the sessions with a queued,
/// claimed, cancelling, running or settling turn. A session whose close
/// is in progress with no turn left is not in it, and stays for its close.
#[test]
fn the_force_set_leaves_out_a_session_with_only_a_close_in_progress() {
    let Some(root) = child("the_force_set_leaves_out_a_session_with_only_a_close_in_progress")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let closing = new_session(&engine).await;
        // A queued turn no dispatcher has claimed.
        let queued = new_session(&engine).await;
        engine
            .faults
            .hold_before_closed
            .store(true, Ordering::Release);
        let (closed, (), ()) = tokio::join!(
            close(&engine, &closing, None),
            dispatch_closing(&engine, &closing),
            async {
                engine.faults.granted.notified().await;
                engine.request_stop(&force()).await.unwrap();
                let set = super::lock(&engine.force_sessions).clone();
                assert_eq!(set, Some(vec![queued.clone()]));
                engine.faults.release.notify_one();
            }
        );
        assert_eq!(closed.unwrap_err().kind, "daemon_stopping");
    });
}

/// T3-S3 round 1, decision 1 (design §6.3 [O3]): each slot is read for
/// the force set in one slot-state section. A session whose only turn left
/// the queue for `running` is in the set; once its run loop is done with
/// the turn, it is not. (Characterization: the move itself is one section.)
#[test]
fn the_force_set_reads_a_slot_in_one_section() {
    let Some(root) = child("the_force_set_reads_a_slot_in_one_section") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let slot = engine.slot(&session).unwrap();
        let _orders = slot.start_running(turn(1), tokio::time::Instant::now());
        assert!(slot.queued().is_empty());
        assert_eq!(engine.unfinished_sessions(), std::slice::from_ref(&session));
        slot.finish_running(turn(1));
        assert!(engine.unfinished_sessions().is_empty());
    });
}

/// T3-S3 round 1, decision 2 (design §6.8 step 3): a dispatcher that has
/// not joined still owns its session. Turn 1 was handed to final shutdown,
/// and the dispatcher is held in turn 2's force-path cancellation read when
/// final shutdown settles: nothing of that session is settled, neither turn
/// 1's forced terminal nor the closure; both stay for restart recovery, and
/// the shutdown is not clean.
#[test]
fn final_settlement_skips_a_session_whose_dispatcher_has_not_joined() {
    let Some(root) = child("final_settlement_skips_a_session_whose_dispatcher_has_not_joined")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        engine
            .faults
            .hold_after_grant
            .store(true, Ordering::Release);
        let ((), ()) = tokio::join!(dispatch(&engine, &session), async {
            engine.faults.granted.notified().await;
            engine.request_stop(&force()).await.unwrap();
            engine
                .faults
                .hold_cancel_read
                .store(true, Ordering::Release);
            engine.faults.release.notify_one();
            // Turn 1 was handed off; the dispatcher holds turn 2's read.
            engine.faults.granted.notified().await;
            assert_eq!(super::lock(&engine.forced).len(), 1, "turn 1 handed off");
            let report = shutdown(&engine).await;
            assert!(!report.is_clean(), "{report:?}");
            assert_eq!(report.unclosed_sessions, 1, "{report:?}");
            assert_eq!(report.unresolved_turns, 2, "{report:?}");
            let types = event_types(&engine, &session).await;
            assert!(
                !types.iter().any(|kind| kind == "turn.ended"),
                "final shutdown settled a turn its dispatcher still owns: {types:?}"
            );
            engine.faults.release.notify_one();
        });
    });
}

/// Design §6.8 entry [r3.2]: under a drain, `close` still works until
/// daemon main enters final shutdown; from entry on, new close work is
/// `daemon_stopping`, and a keyed replay of a committed close replays.
#[test]
fn final_shutdown_entry_fences_new_close_work_under_drain() {
    let Some(root) = child("final_shutdown_entry_fences_new_close_work_under_drain") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let other = new_session(&engine).await;
        // Launches fail here: each turn ends and leaves no active work.
        dispatch(&engine, &other).await;
        let drain = serde_json::from_value(json!({"drain":true})).unwrap();
        assert_eq!(
            engine.request_stop(&drain).await.unwrap(),
            super::StopMode::Drain
        );
        let (first, ()) = tokio::join!(
            close(&engine, &session, Some("k")),
            dispatch_closing(&engine, &session)
        );
        let first = first.unwrap();
        assert_eq!(first["state"], "closed", "{first}");
        let entry = engine.enter_final_shutdown().await;
        assert_eq!(entry.active, 0, "{entry:?}");
        let fenced = tokio::time::timeout(Duration::from_secs(5), close(&engine, &other, None))
            .await
            .expect("the fence replies at once");
        assert_eq!(fenced.unwrap_err().kind, "daemon_stopping");
        assert_eq!(close(&engine, &session, Some("k")).await.unwrap(), first);
    });
}

/// Design §8: the re-probe loop returns at final-shutdown entry and on
/// force, so final shutdown's first step joins it at once.
#[test]
fn the_reprobe_loop_returns_at_entry_and_on_force() {
    let Some(root) = child("the_reprobe_loop_returns_at_entry_and_on_force") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let ((), _) = tokio::join!(
            async {
                tokio::time::timeout(Duration::from_secs(2), engine.reprobe())
                    .await
                    .expect("entry ends the loop");
            },
            engine.enter_final_shutdown()
        );
        drop(engine);
        let forced = open(&root);
        let stop = force();
        let ((), _) = tokio::join!(
            async {
                tokio::time::timeout(Duration::from_secs(2), forced.reprobe())
                    .await
                    .expect("force ends the loop");
            },
            forced.request_stop(&stop)
        );
    });
}

/// T3-S3 round 1, decision 6 (design §8): the re-probe backoff resets to
/// 1 s on every added holding, including one added while the loop waits.
/// A held group with no identity keeps its token at every pass: after the
/// passes at 1, 3 and 7 s the loop waits 8 s. A holding added once the
/// third pass began is re-probed within 1 s (2.5 s allowed), not 8 s later.
#[test]
fn an_added_holding_resets_the_reprobe_backoff() {
    let Some(root) = child("an_added_holding_resets_the_reprobe_backoff") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let passes = || engine.faults.reprobe_passes.load(Ordering::Acquire);
        engine
            .adapter
            .hold_capacity("0-held".to_owned(), session.clone(), Box::new(()));
        let stop = force();
        let ((), ()) = tokio::join!(engine.reprobe(), async {
            // During the third pass or the 8 s wait after it.
            until(|| passes() == 3).await;
            engine
                .adapter
                .hold_capacity("1-held".to_owned(), session.clone(), Box::new(()));
            let added = tokio::time::Instant::now();
            let reset = tokio::time::timeout(Duration::from_millis(2_500), async {
                while passes() < 4 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            assert!(
                reset.is_ok(),
                "no pass within 2.5 s of the addition ({:?})",
                added.elapsed()
            );
            engine.request_stop(&stop).await.unwrap();
        });
    });
}

/// T3-S5 round 1, decision 1 (design §7.1, §7.2 rows 5 and 6 [O1.D2]): a
/// turn whose event already failed cleanly (its first failure, scoped)
/// then reports an uncertain Route Store failure, such as a raw write
/// during cleanup. The uncertain outcome still latches, and the first note
/// stays the turn's, for its resolution write.
#[test]
fn a_later_uncertain_route_failure_latches_after_a_clean_first_failure() {
    let Some(root) = child("a_later_uncertain_route_failure_latches_after_a_clean_first_failure")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let slot = super::queue::Slot::new(super::journal::Head::new(Some(2)));
        let mut record = super::TurnRecord {
            session: session.clone(),
            turn: turn(1),
            head: super::journal::Head::new(Some(2)),
            accepted: None,
            spans: Vec::new(),
            first_failure: Some(super::FailureNote {
                site: super::latch::FailureSite::Event,
                outcome: super::latch::WriteOutcome::NotCommitted,
            }),
            uncertain: None,
        };
        assert!(!engine.store_failed(), "the clean failure is scoped");
        let cause = via_adapters::RouteError::Store {
            turn: turn(1),
            kind: via_adapters::StoreFailure::Uncertain,
        };
        engine
            .route_failed(&slot, &mut record, Some(&cause), false)
            .await;
        assert!(engine.store_failed(), "the uncertain failure latched");
        let first = record.first_failure.expect("the first note is kept");
        assert_eq!(first.site, super::latch::FailureSite::Event);
        assert_eq!(first.outcome, super::latch::WriteOutcome::NotCommitted);
        let status = engine.store_failure_status().unwrap();
        assert_eq!(status["scope"], "daemon");
        assert_eq!(status["kind"], "commit_uncertain");
    });
}

/// T3-S5 round 1, decision 3 (design §7.2 row 5): an observation still
/// queued when `execute` completes is committed by the completion's drain.
/// When that write is not committed (the record's head read fails and
/// writes nothing), the turn's stop order with cause `store` attaches, as
/// in the observation branch, so the disposition carries row 5's `cancel`
/// evidence.
#[test]
fn a_drained_observation_whose_write_fails_attaches_the_store_order() {
    let Some(root) = child("a_drained_observation_whose_write_fails_attaches_the_store_order")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let slot = engine.slot(&session).unwrap();
        let wall = tokio::time::Instant::now() + Duration::from_secs(3600);
        let (_route, orders) = slot.start_running(turn(1), wall);
        let watched = orders.clone();
        // A session Store does not hold: the head read writes nothing.
        let mut record = super::TurnRecord {
            session: SessionId::try_from("s_000000000000").unwrap(),
            turn: turn(1),
            head: super::journal::Head::new(None),
            accepted: None,
            spans: Vec::new(),
            first_failure: None,
            uncertain: None,
        };
        let effective: crate::api::Effective = serde_json::from_value(json!({
            "model":"fake","effort":null,"bound":null,
            "deadlines":{"wall_ms":30_000,"idle_ms":600_000},"max_steps":null
        }))
        .unwrap();
        let raw_ref = crate::RawRef::new(
            crate::ConnectionId::try_from("c_000000000000").unwrap(),
            0,
            1,
        )
        .unwrap();
        let queued = via_adapters::FakeObservation::Data {
            observation: via_adapters::Observation::AssistantText {
                text: "lost".to_owned(),
            },
            raw_ref,
        };
        engine
            .drain_queued(&slot, &mut record, &effective, orders, vec![queued])
            .await;
        let note = record.first_failure.expect("the drained write failed");
        assert_eq!(note.outcome, super::latch::WriteOutcome::NotCommitted);
        let order = watched.borrow().clone();
        let order = order.expect("row 5's stop order attached");
        assert!(matches!(order.cause, via_adapters::StopCause::Store));
        assert!(!engine.store_failed(), "the failure is scoped");
    });
}

/// Failpoint token of a child that arms a Store seam.
const FAILPOINT_TOKEN: &str = "engine-tests-failpoint-token";

/// In a child, activates Store's failpoints before the Engine opens, with
/// `point` failing its first hit (`fail_io`); returns their directory.
fn fail_first(root: &Path, point: &str) -> PathBuf {
    let dir = root.join("failpoints");
    fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let command = json!({"token":FAILPOINT_TOKEN,"occurrence":1,"action":"fail_io"});
    fs::write(dir.join(format!("{point}.json")), command.to_string()).unwrap();
    via_store::failpoint::activate(&dir, FAILPOINT_TOKEN).unwrap();
    dir
}

/// T3-S5 round 1, decision 10 (design §7.1): SQLite corruption on the
/// session-head read before a terminal commit (`store.read.corrupt.next_seq`)
/// reaches the failure hook as `Corrupt`, which latches even at final
/// shutdown's scoped forced-terminal site. While serving, the head is
/// unknown there only after an uncertain write, which has already latched
/// (the restart handoff's terminal fails startup on any error), so the
/// record here starts with an unknown head.
#[test]
fn a_corrupt_head_read_before_a_terminal_latches() {
    let Some(root) = child("a_corrupt_head_read_before_a_terminal_latches") else {
        return;
    };
    let points = fail_first(&root, "store.read.corrupt.next_seq");
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let started = super::Started {
            session: session.clone(),
            turn: turn(1),
            queued_at: rfc3339(std::time::SystemTime::now()),
            first_seq: 1,
            submitted: None,
        };
        let record = super::TurnRecord {
            session: session.clone(),
            turn: turn(1),
            head: super::journal::Head::new(None),
            accepted: None,
            spans: Vec::new(),
            first_failure: None,
            uncertain: None,
        };
        let terminal = super::Terminal {
            state: "failed",
            failure: Some(super::failure(
                crate::api::FailureClass::Store,
                "a turn event could not be recorded".to_owned(),
                None,
            )),
            stop_reason: "error",
            vendor_stop_reason: None,
            final_text: String::new(),
            exit: None,
            raw_ref: None,
            raw_incomplete: false,
            warnings: Vec::new(),
            cancel: None,
        };
        let finished = engine.finish(&started, record, terminal, false, None).await;
        assert_eq!(finished.unwrap_err().kind, "store_error");
        assert!(points.join("store.read.corrupt.next_seq.1.ack").exists());
        assert_corruption_latched(&engine);
    });
}

/// T3-S5 round 1, decision 10 (design §7.1): SQLite corruption on the force
/// closure pass's session-head read latches, and the session counts as
/// unclosed. Since round 2 (decision 11) Store's read reply reports it,
/// once. The pass reads the head from Store only for a session with no
/// slot or an unknown head. End to end every force session keeps its slot,
/// whose head its last write left known (an unknown one follows a latching
/// write, which skips the pass), so the slot is removed here.
#[test]
fn a_corrupt_head_read_in_the_closure_pass_latches() {
    let Some(root) = child("a_corrupt_head_read_in_the_closure_pass_latches") else {
        return;
    };
    let points = fail_first(&root, "store.read.corrupt.next_seq");
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        cancel(&engine, &session, 1).await.unwrap();
        super::lock(&engine.sessions).remove(&session);
        super::lock(&engine.force_sessions).replace(vec![session.clone()]);
        let report = shutdown(&engine).await;
        assert!(points.join("store.read.corrupt.next_seq.1.ack").exists());
        assert!(report.store_failed, "{report:?}");
        assert_eq!(report.unclosed_sessions, 1, "{report:?}");
        let status = engine.store_failure_status().unwrap();
        assert_eq!(status["kind"], "corrupt_store");
        // Store's read reply reported it, once (T3-S5 round 2, decision 11).
        assert_eq!(status["count"], 1, "{status}");
        assert!(
            !event_types(&engine, &session)
                .await
                .contains(&"session.closed".to_owned())
        );
    });
}

/// T3-S5 round 1, decision 10 (design §7.1, §7.2 row 2): SQLite corruption
/// on the session-head read of row 2's resolution write
/// (`commit_submit_failed`) is reported as `corrupt_store`, not as a failed
/// commit. The dispatcher's slot is fresh after a restart, so its head is
/// unknown; the slot here starts that way.
#[test]
fn a_corrupt_head_read_before_a_submit_failed_write_is_corrupt() {
    let Some(root) = child("a_corrupt_head_read_before_a_submit_failed_write_is_corrupt") else {
        return;
    };
    let points = fail_first(&root, "store.read.corrupt.next_seq");
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let queueing = engine.queueing(&session, turn(1)).await.unwrap();
        let slot = super::queue::Slot::new(super::journal::Head::new(None));
        engine
            .submit_failed(&slot, &session, turn(1), queueing, "row unreadable")
            .await;
        assert!(points.join("store.read.corrupt.next_seq.1.ack").exists());
        assert_corruption_latched(&engine);
        let types = event_types(&engine, &session).await;
        assert_eq!(types, ["turn.queued"], "nothing was written");
    });
}

/// In a child, activates Store's failpoints before the Engine opens and
/// counts each of `points`: a command under another token is refused at
/// every hit, leaving `<point>.<n>.refused`. Returns their directory.
fn count_points(root: &Path, points: &[&str]) -> PathBuf {
    let dir = root.join("failpoints");
    fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    for point in points {
        let command = json!({"token":"counting-only-token","occurrence":1,"action":"pause"});
        fs::write(dir.join(format!("{point}.json")), command.to_string()).unwrap();
    }
    via_store::failpoint::activate(&dir, FAILPOINT_TOKEN).unwrap();
    dir
}

/// Arms counted `point` to fail its next hit (`fail_io`); returns that
/// occurrence.
fn arm_next(dir: &Path, point: &str) -> u64 {
    let prefix = format!("{point}.");
    let counted = fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            name.strip_prefix(&prefix)?
                .strip_suffix(".refused")?
                .parse::<u64>()
                .ok()
        })
        .max()
        .unwrap_or(0);
    let next = counted + 1;
    let command = json!({"token":FAILPOINT_TOKEN,"occurrence":next,"action":"fail_io"});
    fs::write(dir.join(format!("{point}.json")), command.to_string()).unwrap();
    next
}

/// Whether `point`'s occurrence `n` acted: its acknowledgement exists.
fn acked(dir: &Path, point: &str, n: u64) -> bool {
    dir.join(format!("{point}.{n}.ack")).exists()
}

/// Asserts the latch's phase one ran with `corrupt_store` (design §7.1),
/// recorded once: Store's read reply reported the one corrupt read, and
/// no aborted write recorded it again (T3-S5 round 3, decision 13).
fn assert_corruption_latched(engine: &Engine) {
    assert!(engine.store_failed(), "the corrupt read latched");
    let status = engine.store_failure_status().unwrap();
    assert_eq!(status["kind"], "corrupt_store", "{status}");
    assert_eq!(status["scope"], "daemon", "{status}");
    assert_eq!(status["count"], 1, "one failure, one record: {status}");
}

/// The force closure pass of one session whose only turn was cancelled,
/// with `point` failing its next hit as SQLite corruption.
fn closure_read_corruption(root: &Path, point: &str) {
    let points = count_points(root, &[point]);
    run(async {
        let engine = open(root);
        let session = new_session(&engine).await;
        cancel(&engine, &session, 1).await.unwrap();
        super::lock(&engine.force_sessions).replace(vec![session.clone()]);
        let n = arm_next(&points, point);
        let report = shutdown(&engine).await;
        assert!(acked(&points, point, n), "{point} #{n} was not reached");
        assert_corruption_latched(&engine);
        assert!(report.store_failed, "{report:?}");
        assert_eq!(report.unclosed_sessions, 1, "{report:?}");
    });
}

/// T3-S5 round 2, decision 11 (design §7.1): SQLite corruption on the
/// closure pass's snapshot read (`stop.rs` `close_forced`) latches.
#[test]
fn a_corrupt_snapshot_read_in_the_closure_pass_latches() {
    let Some(root) = child("a_corrupt_snapshot_read_in_the_closure_pass_latches") else {
        return;
    };
    closure_read_corruption(&root, "store.read.corrupt.snapshot");
}

/// T3-S5 round 2, decision 11 (design §7.1): SQLite corruption on the
/// closure pass's predecessor read (`stop.rs` `close_forced`) latches.
#[test]
fn a_corrupt_predecessors_read_in_the_closure_pass_latches() {
    let Some(root) = child("a_corrupt_predecessors_read_in_the_closure_pass_latches") else {
        return;
    };
    closure_read_corruption(&root, "store.read.corrupt.predecessors");
}

/// A turn record of `session`'s turn 1 at a known head, optionally with
/// an uncertain event at sequence 2 to reconcile.
fn turn_one(session: &SessionId, uncertain: bool) -> super::TurnRecord {
    super::TurnRecord {
        session: session.clone(),
        turn: turn(1),
        head: super::journal::Head::new(Some(2)),
        accepted: None,
        spans: Vec::new(),
        first_failure: None,
        uncertain: uncertain.then_some(super::journal::UncertainEvent {
            seq: 2,
            raw_ref: None,
            accepted: None,
        }),
    }
}

/// Turn 1's `Started`, never submitted.
fn started_one(session: &SessionId) -> super::Started {
    super::Started {
        session: session.clone(),
        turn: turn(1),
        queued_at: rfc3339(std::time::SystemTime::now()),
        first_seq: 1,
        submitted: None,
    }
}

/// A `failed(store)` terminal.
fn store_terminal() -> super::Terminal {
    super::Terminal {
        state: "failed",
        failure: Some(super::failure(
            crate::api::FailureClass::Store,
            "a turn event could not be recorded".to_owned(),
            None,
        )),
        stop_reason: "error",
        vendor_stop_reason: None,
        final_text: String::new(),
        exit: None,
        raw_ref: None,
        raw_incomplete: false,
        warnings: Vec::new(),
        cancel: None,
    }
}

/// T3-S5 round 2, decision 11 (design §7.1): SQLite corruption on the
/// terminal commit's reconcile read (`drive.rs` `commit_turn_ended_with`)
/// latches; the caller's reply is the same `store_error`.
#[test]
fn a_corrupt_reconcile_read_before_a_terminal_latches() {
    let Some(root) = child("a_corrupt_reconcile_read_before_a_terminal_latches") else {
        return;
    };
    let point = "store.read.corrupt.events";
    let points = count_points(&root, &[point]);
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let n = arm_next(&points, point);
        let finished = engine
            .finish(
                &started_one(&session),
                turn_one(&session, true),
                store_terminal(),
                false,
                None,
            )
            .await;
        assert_eq!(finished.unwrap_err().kind, "store_error");
        assert!(acked(&points, point, n), "{point} #{n} was not reached");
        assert_corruption_latched(&engine);
    });
}

/// Final shutdown's batch for turn 1 of a session with turn 2 queued, with
/// `record` as turn 1's and `point` failing its next hit as SQLite
/// corruption: the batch is skipped. A step 1 read skips it before its
/// write, so phase two waits for final shutdown's entry; a read the write
/// needs aborts that write, whose hook call finishes phase two.
fn batch_read_corruption(
    root: &Path,
    point: &str,
    record: fn(&SessionId) -> super::TurnRecord,
    aborts_write: bool,
) {
    let points = count_points(root, &[point]);
    run(async {
        let engine = open(root);
        let session = new_session(&engine).await;
        resume(&engine, &session, None).await;
        let affected = super::batch::AffectedTurn {
            started: started_one(&session),
            record: record(&session),
            terminal: store_terminal(),
            raw_incomplete: false,
        };
        let n = arm_next(&points, point);
        let mut batches = super::batch::FailureBatches::default();
        let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(10));
        engine
            .resolve_affected(affected, deadline, &mut batches)
            .await;
        assert!(acked(&points, point, n), "{point} #{n} was not reached");
        assert_corruption_latched(&engine);
        assert_eq!((batches.committed, batches.skipped), (0, 1));
        // Phase two waits for `admission`: the aborted write's hook call or
        // final shutdown's entry takes it.
        assert_eq!(engine.latch_finalized(), aborts_write);
        engine.enter_final_shutdown().await;
        assert!(
            engine.latch_finalized(),
            "final shutdown's entry finished it"
        );
    });
}

/// T3-S5 round 2, decision 11 (design §7.1): SQLite corruption on the
/// batch's result read (`batch.rs` step 1) latches.
#[test]
fn a_corrupt_result_read_in_the_batch_latches() {
    let Some(root) = child("a_corrupt_result_read_in_the_batch_latches") else {
        return;
    };
    batch_read_corruption(
        &root,
        "store.read.corrupt.result",
        |session| turn_one(session, true),
        false,
    );
}

/// T3-S5 round 2, decision 11 (design §7.1): SQLite corruption on the
/// batch's reconcile read (`batch.rs` step 1) latches.
#[test]
fn a_corrupt_reconcile_read_in_the_batch_latches() {
    let Some(root) = child("a_corrupt_reconcile_read_in_the_batch_latches") else {
        return;
    };
    batch_read_corruption(
        &root,
        "store.read.corrupt.events",
        |session| turn_one(session, true),
        false,
    );
}

/// T3-S5 round 2, decision 11 (design §7.1): SQLite corruption on the
/// batch's queued-row read (`batch.rs` step 1) latches.
#[test]
fn a_corrupt_queued_row_read_in_the_batch_latches() {
    let Some(root) = child("a_corrupt_queued_row_read_in_the_batch_latches") else {
        return;
    };
    batch_read_corruption(
        &root,
        "store.read.corrupt.queued_turn",
        |session| turn_one(session, true),
        false,
    );
}

/// T3-S5 round 3, decision 13 (design §7.1, §7.5): SQLite corruption on
/// the batch's session-head read (`store.read.corrupt.next_seq`) aborts the
/// batch's write. Store's read reply recorded it, so the batch records no
/// second failure; the batch is still skipped and the latch stands. The
/// record's head is unknown, as after the uncertain write that makes the
/// batch read it.
#[test]
fn a_corrupt_head_read_in_the_batch_records_one_failure() {
    let Some(root) = child("a_corrupt_head_read_in_the_batch_records_one_failure") else {
        return;
    };
    batch_read_corruption(
        &root,
        "store.read.corrupt.next_seq",
        |session| super::TurnRecord {
            head: super::journal::Head::new(None),
            ..turn_one(session, false)
        },
        true,
    );
}

/// T3-S5 round 3, decision 13 (design §7.1, §7.5): SQLite corruption on
/// the acceptance's session-head read (`drive.rs` `observe`,
/// `store.read.corrupt.next_seq`) aborts the acceptance write. Store's read
/// reply recorded it, so `store_failure.count` is 1; the turn keeps its
/// first failure (row 5) and the latch stands. The record's head is
/// unknown, as after an uncertain write.
#[test]
fn a_corrupt_head_read_before_an_acceptance_records_one_failure() {
    let Some(root) = child("a_corrupt_head_read_before_an_acceptance_records_one_failure") else {
        return;
    };
    let point = "store.read.corrupt.next_seq";
    let points = count_points(&root, &[point]);
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let slot = engine.slot(&session).unwrap();
        let wall = tokio::time::Instant::now() + Duration::from_secs(3600);
        let (_route, orders) = slot.start_running(turn(1), wall);
        let mut record = super::TurnRecord {
            head: super::journal::Head::new(None),
            ..turn_one(&session, false)
        };
        let effective: crate::api::Effective = serde_json::from_value(json!({
            "model":"fake","effort":null,"bound":null,
            "deadlines":{"wall_ms":30_000,"idle_ms":600_000},"max_steps":null
        }))
        .unwrap();
        let raw_ref = crate::RawRef::new(
            crate::ConnectionId::try_from("c_000000000000").unwrap(),
            0,
            1,
        )
        .unwrap();
        let accepted =
            via_adapters::FakeObservation::Accepted(via_adapters::FakeAcceptanceObservation {
                correlation: via_adapters::AcceptanceToken::try_from(1).unwrap(),
                vendor_turn_id: via_adapters::VendorTurnId::try_from("v_1".to_owned()).unwrap(),
                raw_ref,
            });
        let n = arm_next(&points, point);
        engine
            .drain_queued(&slot, &mut record, &effective, orders, vec![accepted])
            .await;
        assert!(acked(&points, point, n), "{point} #{n} was not reached");
        let note = record.first_failure.expect("the acceptance write failed");
        assert_eq!(note.site, super::latch::FailureSite::Event);
        assert!(record.accepted.is_none(), "nothing was accepted");
        assert_corruption_latched(&engine);
    });
}

/// T3-S5 round 3, decision 13 (design §7.1, §7.3, §7.5): when a read
/// streak expires, SQLite corruption on the queueing read that row 2's
/// resolution write needs (`store.read.corrupt.events`) aborts that write.
/// The expired streak records its own failure; Store's read reply records
/// the corruption, which stays the latest, and the aborted resolution
/// write records nothing more: two failures, two records.
#[test]
fn a_corrupt_queueing_read_after_a_read_streak_records_one_failure() {
    let Some(root) = child("a_corrupt_queueing_read_after_a_read_streak_records_one_failure")
    else {
        return;
    };
    let point = "store.read.corrupt.events";
    let points = count_points(&root, &[point]);
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        let slot = engine.slot(&session).unwrap();
        let n = arm_next(&points, point);
        engine.read_expired(&slot, &session, turn(1)).await;
        assert!(acked(&points, point, n), "{point} #{n} was not reached");
        assert!(engine.store_failed(), "the corrupt read latched");
        let status = engine.store_failure_status().unwrap();
        assert_eq!(status["kind"], "corrupt_store", "{status}");
        assert_eq!(status["scope"], "daemon", "{status}");
        assert_eq!(status["count"], 2, "the streak and the read: {status}");
        let types = event_types(&engine, &session).await;
        assert_eq!(types, ["turn.queued"], "nothing was written");
    });
}

/// T3-S5 round 2, decision 12 (design §6.8, §7.4): after a latch the
/// closure pass does not run; it counts each force session Store does not
/// read as closed. A durably closed session is not counted; an open one and
/// one whose snapshot read is corrupt (`store.read.corrupt.snapshot`) are.
#[test]
fn after_a_latch_the_closure_pass_counts_durably_open_sessions() {
    let Some(root) = child("after_a_latch_the_closure_pass_counts_durably_open_sessions") else {
        return;
    };
    let point = "store.read.corrupt.snapshot";
    let points = count_points(&root, &[point]);
    run(async {
        let engine = open(&root);
        let closed = new_session(&engine).await;
        cancel(&engine, &closed, 1).await.unwrap();
        let event = Event {
            seq: 3,
            session_id: &closed,
            turn: None,
            late: false,
            at: &rfc3339(std::time::SystemTime::now()),
            raw_ref: None,
            body: EventBody::SessionClosed {
                reason: super::drive::FORCE_CLOSE_REASON,
            },
        }
        .to_value()
        .unwrap();
        assert!(
            engine
                .store
                .commit_session_closed(&closed, event)
                .await
                .unwrap()
        );
        let open_session = new_session(&engine).await;
        let unreadable = new_session(&engine).await;
        super::lock(&engine.force_sessions).replace(vec![
            closed.clone(),
            open_session.clone(),
            unreadable.clone(),
        ]);
        engine.latch().await;
        // The pass reads the three snapshots in order: the third fails.
        let n = arm_next(&points, point) + 2;
        let command = json!({"token":FAILPOINT_TOKEN,"occurrence":n,"action":"fail_io"});
        fs::write(points.join(format!("{point}.json")), command.to_string()).unwrap();
        let report = shutdown(&engine).await;
        assert!(acked(&points, point, n), "{point} #{n} was not reached");
        assert_eq!(report.unclosed_sessions, 2, "{report:?}");
        assert_corruption_latched(&engine);
    });
}
