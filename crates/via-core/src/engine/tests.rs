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
    AdapterConfig, ApiError, BootstrapEnv, DaemonStopParams, Deadline, ResumeParams, SessionId,
    SpawnParams, TurnNumber,
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
    for part in ["state", "runtime", "runtime/anchors", "sync"] {
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
    fs::write(&scenario, br#"{"scripts":[]}"#).unwrap();
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
        AdapterConfig::load(BootstrapEnv::capture(), None).unwrap(),
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

/// The session's first 1000 durable events as one `events` page.
async fn events_page(engine: &Engine, session: &SessionId) -> Value {
    let params = serde_json::from_value(json!({"session": session, "limit": 1000})).unwrap();
    let page = engine.events(params).await.unwrap();
    serde_json::from_str(page.get()).unwrap()
}

/// The session's durable event types, checking that sequences are dense.
async fn event_types(engine: &Engine, session: &SessionId) -> Vec<String> {
    let page = events_page(engine, session).await;
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
                steps: Vec::new(),
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
    let result: serde_json::Value = serde_json::from_str(result.get()).unwrap();
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
        let page = events_page(&engine, &session).await;
        assert_eq!(page["events"][6]["reason"], "daemon_stop_force");
        for n in 1..=3 {
            let envelope = engine
                .result(&format!("{}/{n}", session.as_str()))
                .await
                .unwrap();
            let envelope: serde_json::Value = serde_json::from_str(envelope.get()).unwrap();
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
        let envelope: serde_json::Value = serde_json::from_str(envelope.get()).unwrap();
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
        let page = events_page(&engine, &session).await;
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
        let envelope: serde_json::Value = serde_json::from_str(envelope.get()).unwrap();
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
        let envelope: serde_json::Value = serde_json::from_str(envelope.get()).unwrap();
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

/// Releases a paused point on drop, so Store's writer can join even when an
/// assertion fails first.
struct Release(PathBuf);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, b"");
    }
}

/// A durably `closing` session an earlier daemon left, with one queued turn
/// and one anchor of that turn that has no absence proof yet: `identified`
/// and, when `provable`, in this boot and namespace with a group id above
/// any `pid_max`, so a re-probe proves it absent. Otherwise its identity is
/// of another boot, which no probe can prove.
async fn closing_with_anchor(root: &Path, provable: bool) -> SessionId {
    closing_with_anchors(root, provable, 1).await
}

/// The id of [`closing_with_anchors`]'s `n`th anchor.
fn anchor_id(n: usize) -> String {
    format!("{n}-anchor")
}

/// [`closing_with_anchor`] with `count` such anchors, [`anchor_id`]`(0..count)`.
async fn closing_with_anchors(root: &Path, provable: bool, count: usize) -> SessionId {
    use std::os::unix::fs::MetadataExt;
    let session = {
        let earlier = open(root);
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
    let store = via_store::Store::open(&root.join("state")).unwrap();
    let (_raw, journal) = store.runtime_resources().into_wire_parts();
    let (boot_id, pid_namespace) = if provable {
        (
            fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .unwrap()
                .trim()
                .to_owned(),
            fs::read_link("/proc/self/ns/pid")
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        ("another-boot".to_owned(), "another-namespace".to_owned())
    };
    let uid = fs::metadata("/proc/self").unwrap().uid();
    for n in 0..count {
        let (id, generation) = (anchor_id(n), format!("g{n}"));
        let intent = via_store::AnchorIntent {
            anchor_id: id.clone(),
            generation: generation.clone(),
            marker: "marker".to_owned(),
            socket_path: root.join(format!("runtime/anchors/{n}.sock")),
            owner_session: session.clone(),
            owner_turn: turn(1),
            uid,
            boot_id: boot_id.clone(),
            pid_namespace: pid_namespace.clone(),
        };
        let via_store::CommitOutcome::Committed(receipt) =
            journal.commit_anchor_intent(intent).await
        else {
            panic!("the anchor intent did not commit");
        };
        // A group id above Linux's `pid_max` (4194304) names no group.
        let identity = via_store::AnchorIdentity {
            pid: 4_194_305,
            pgid: 4_194_305,
            uid,
            boot_id: boot_id.clone(),
            pid_namespace: pid_namespace.clone(),
            start_ticks: 1,
            marker: "marker".to_owned(),
        };
        let identified = journal
            .commit_anchor_identified(&id, &generation, receipt.record_version, identity)
            .await;
        assert!(matches!(identified, via_store::CommitOutcome::Committed(_)));
    }
    session
}

/// Design §4 "Restart", [O1.D9] and §7.2 row 12: the restart close
/// completion's absence check re-probes the session's held group and its
/// proof is not committed. The failed write fails startup before `Closed`;
/// nothing is closed, and the scoped failure is recorded against the session.
#[test]
fn a_failed_proof_write_fails_the_restart_close_before_closed() {
    let Some(root) = child("a_failed_proof_write_fails_the_restart_close_before_closed") else {
        return;
    };
    let points = fail_first(&root, "store.journal.absence");
    run(async {
        let session = closing_with_anchor(&root, true).await;
        let engine = open(&root);
        engine
            .adapter
            .hold_capacity("0-anchor".to_owned(), session.clone(), Box::new(()));
        let refused = engine.hand_off_queued().await.unwrap_err();
        assert!(refused.starts_with("store_error:"), "{refused}");
        assert!(acked(&points, "store.journal.absence", 1));
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended"],
            "no session.closed"
        );
        // Not committed: recorded against the session, not latched.
        assert!(!engine.store_failed());
        let status = engine.store_failure_status().unwrap();
        assert_eq!(status["scope"], "session", "{status}");
        assert_eq!(status["affected"]["addresses"], json!([session.as_str()]));
    });
}

/// As above, but the proof's commit does not answer within the bound: its
/// outcome is uncertain, so startup fails before `Closed` and the daemon
/// latches [O1, §7.2 row 12].
#[test]
fn an_uncertain_proof_write_fails_the_restart_close_before_closed() {
    let Some(root) = child("an_uncertain_proof_write_fails_the_restart_close_before_closed") else {
        return;
    };
    let points = pause_first(&root, "store.journal.absence");
    run(async {
        let session = closing_with_anchor(&root, true).await;
        let engine = open(&root);
        // Declared after the Engine, so it drops first: Store's writer is
        // released before the Engine joins it.
        let release = Release(points.join("store.journal.absence.1.release"));
        engine
            .adapter
            .hold_capacity("0-anchor".to_owned(), session.clone(), Box::new(()));
        // Store's writer stays paused: a `Closed` commit would wait on it.
        let refused = tokio::time::timeout(Duration::from_secs(10), engine.hand_off_queued())
            .await
            .expect("startup fails without committing `Closed`")
            .unwrap_err();
        assert!(refused.starts_with("store_error:"), "{refused}");
        assert!(engine.store_failed(), "the uncertain proof latches");
        // Store reads wait behind the paused writer: let the proof commit
        // finish first. Its reply has no reader left.
        drop(release);
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended"],
            "no session.closed"
        );
    });
}

/// Design §4 "Restart", [O1.D9] and §7.2 row 12, T3 review round 2: a proof
/// write that is not committed is not lost when the same re-probe pass then
/// fails on its next page read. After the pass's first page of anchors,
/// only a page read can fail without an uncertain proof (which latches on its
/// own), so the session holds a page and one anchor: the writer is paused at
/// the first proof, the second proof is armed to fail, and the read of the
/// second page is armed to fail. Startup must still fail before `Closed`.
#[test]
fn a_failed_proof_before_a_page_read_failure_fails_the_restart_close() {
    let Some(root) = child("a_failed_proof_before_a_page_read_failure_fails_the_restart_close")
    else {
        return;
    };
    let (proof, read) = ("store.journal.absence", "store.read.stall");
    let points = count_points(&root, &[read]);
    let arm = |occurrence: u64, action: &str| {
        let command = json!({"token":FAILPOINT_TOKEN,"occurrence":occurrence,"action":action});
        fs::write(points.join(format!("{proof}.json")), command.to_string()).unwrap();
    };
    arm(1, "pause");
    run(async {
        let anchors = via_store::ANCHOR_PAGE_LIMIT as usize + 1;
        let session = closing_with_anchors(&root, true, anchors).await;
        let engine = open(&root);
        // Declared after the Engine, so it drops first: Store's writer is
        // released before the Engine joins it.
        let release = Release(points.join(format!("{proof}.1.release")));
        for n in 0..anchors {
            engine
                .adapter
                .hold_capacity(anchor_id(n), session.clone(), Box::new(()));
        }
        let (refused, read_hit) = tokio::join!(
            async {
                tokio::time::timeout(Duration::from_secs(60), engine.hand_off_queued())
                    .await
                    .expect("startup ends")
                    .unwrap_err()
            },
            async {
                // The pass read its first page and its writer waits at the
                // first proof: the second proof fails, and so does the next
                // read, the second page's.
                while !acked(&points, proof, 1) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                arm(2, "fail_io");
                let read_hit = arm_next(&points, read);
                drop(release);
                read_hit
            }
        );
        assert!(acked(&points, proof, 2), "the second proof did not fail");
        assert!(
            acked(&points, read, read_hit),
            "{read} #{read_hit} was not reached"
        );
        assert!(
            refused.contains("an absence proof was not recorded"),
            "{refused}"
        );
        assert!(!engine.store_failed(), "not committed does not latch");
        assert_eq!(
            event_types(&engine, &session).await,
            ["turn.queued", "turn.ended"],
            "no session.closed"
        );
        let status = engine.store_failure_status().unwrap();
        assert_eq!(status["scope"], "session", "{status}");
    });
}

/// Design §4 "Restart": a held group no probe can prove (an earlier boot's)
/// is ordinary unproved absence, not a Store failure. The completion still
/// commits `Closed`, and its cleanup derives `uncertain` from the missing
/// proof.
#[test]
fn an_unprovable_group_leaves_the_restart_close_cleanup_uncertain() {
    let Some(root) = child("an_unprovable_group_leaves_the_restart_close_cleanup_uncertain") else {
        return;
    };
    run(async {
        let session = closing_with_anchor(&root, false).await;
        let engine = open(&root);
        engine
            .adapter
            .hold_capacity("0-anchor".to_owned(), session.clone(), Box::new(()));
        let handoff = engine.hand_off_queued().await.unwrap();
        assert_eq!((handoff.cancelled, handoff.closed), (1, 1), "{handoff:?}");
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
        assert_eq!(result["cleanup"], "uncertain", "{result}");
        assert!(!engine.store_failed());
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
        let _orders = slot.start_running(
            turn(1),
            tokio::time::Instant::now(),
            super::progress::Progress::starting(1),
        );
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
            first_failure: Some(super::FailureNote {
                site: super::latch::FailureSite::Event,
                outcome: super::latch::WriteOutcome::NotCommitted,
            }),
            uncertain: None,
            steps: super::progress::StepTracker::default(),
            vendor: super::lane::VendorRecord::default(),
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
        let (_route, orders) =
            slot.start_running(turn(1), wall, super::progress::Progress::starting(1));
        let watched = orders.clone();
        // A session Store does not hold: the head read writes nothing.
        let mut record = super::TurnRecord {
            session: SessionId::try_from("s_000000000000").unwrap(),
            turn: turn(1),
            head: super::journal::Head::new(None),
            accepted: None,
            first_failure: None,
            uncertain: None,
            steps: super::progress::StepTracker::default(),
            vendor: super::lane::VendorRecord::default(),
        };
        let effective: crate::api::Effective = serde_json::from_value(json!({
            "model":"fake","effort":null,"bound":null,
            "deadlines":{"wall_ms":30_000,"idle_ms":600_000},"max_steps":null
        }))
        .unwrap();
        // Model output after a tool result ends step 1: its row's commit
        // is the drained write (Task 4 design §3.2).
        let _ = record.steps.accept();
        let marks = |model: bool, ended: &[&str]| via_adapters::ObservationItem {
            at: tokio::time::Instant::now(),
            vendor_turn: None,
            observation: via_adapters::Observation::Progress(via_adapters::ProgressMarks {
                model,
                tools_started: Vec::new(),
                tools_ended: ended.iter().map(|id| (*id).to_owned()).collect(),
                usage: None,
            }),
        };
        let queued = vec![marks(false, &["t"]), marks(true, &[])];
        engine
            .drain_queued((&slot, None), &mut record, &effective, orders, queued)
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

/// In a child, activates Store's failpoints before the Engine opens, with
/// `point` pausing its first hit until its release file exists; returns
/// their directory.
fn pause_first(root: &Path, point: &str) -> PathBuf {
    let dir = root.join("failpoints");
    fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let command = json!({"token":FAILPOINT_TOKEN,"occurrence":1,"action":"pause"});
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
            folder: None,
            cwd: None,
        };
        let record = super::TurnRecord {
            session: session.clone(),
            turn: turn(1),
            head: super::journal::Head::new(None),
            accepted: None,
            first_failure: None,
            uncertain: None,
            steps: super::progress::StepTracker::default(),
            vendor: super::lane::VendorRecord::default(),
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
            final_text: Some(String::new()),
            final_text_file: None,
            exit: None,
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
        // T4-5 review round 1: the history fallback keeps the session's
        // frozen `cwd` for the envelope it builds.
        assert_eq!(
            queueing.cwd.as_deref(),
            engine.cwd.to_str(),
            "the rebuilt queueing lost the frozen cwd"
        );
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
        first_failure: None,
        uncertain: uncertain.then_some(super::journal::UncertainEvent {
            seq: 2,
            event: serde_json::Value::Null,
            accepted: None,
        }),
        steps: super::progress::StepTracker::default(),
        vendor: super::lane::VendorRecord::default(),
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
        folder: None,
        cwd: None,
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
        final_text: Some(String::new()),
        final_text_file: None,
        exit: None,
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
/// batch's result read (`batch.rs` step 1) latches. Task 4 §6.7: that read
/// is now the terminal's facts, not its envelope.
#[test]
fn a_corrupt_result_read_in_the_batch_latches() {
    let Some(root) = child("a_corrupt_result_read_in_the_batch_latches") else {
        return;
    };
    batch_read_corruption(
        &root,
        "store.read.corrupt.terminal_facts",
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
        let (_route, orders) =
            slot.start_running(turn(1), wall, super::progress::Progress::starting(1));
        let mut record = super::TurnRecord {
            head: super::journal::Head::new(None),
            ..turn_one(&session, false)
        };
        let effective: crate::api::Effective = serde_json::from_value(json!({
            "model":"fake","effort":null,"bound":null,
            "deadlines":{"wall_ms":30_000,"idle_ms":600_000},"max_steps":null
        }))
        .unwrap();
        let vendor_turn = via_adapters::VendorTurnId::try_from("v_1".to_owned()).unwrap();
        let accepted = via_adapters::ObservationItem {
            at: tokio::time::Instant::now(),
            vendor_turn: Some(vendor_turn.clone()),
            observation: via_adapters::Observation::Accepted(
                via_adapters::observation::Acceptance {
                    correlation: via_adapters::AcceptanceToken::try_from(1).unwrap(),
                    vendor_turn_id: Some(vendor_turn),
                },
            ),
        };
        let n = arm_next(&points, point);
        engine
            .drain_queued(
                (&slot, None),
                &mut record,
                &effective,
                orders,
                vec![accepted],
            )
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

/// A session whose only turn was cancelled and which Store then closed
/// with the closure pass's `session.closed`, as a forced terminal whose
/// reply was lost leaves it.
async fn durably_closed_session(engine: &Engine) -> SessionId {
    let session = new_session(engine).await;
    cancel(engine, &session, 1).await.unwrap();
    let event = Event {
        seq: 3,
        session_id: &session,
        turn: None,
        late: false,
        at: &rfc3339(std::time::SystemTime::now()),
        body: EventBody::SessionClosed {
            reason: super::drive::FORCE_CLOSE_REASON,
        },
    }
    .to_value()
    .unwrap();
    assert!(
        engine
            .store
            .commit_session_closed(&session, event)
            .await
            .unwrap()
    );
    session
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
        let closed = durably_closed_session(&engine).await;
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

/// T3-S5 round 3, decision 14 (design §6.8, §7.4): after a latch, an
/// unjoined force session is counted by its durable state too. A durably
/// closed one is not counted; an open one is, and so is an open joined
/// session after them. Unjoined sessions get no closure write.
#[test]
fn after_a_latch_unjoined_sessions_count_by_their_durable_state() {
    let Some(root) = child("after_a_latch_unjoined_sessions_count_by_their_durable_state") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let closed = durably_closed_session(&engine).await;
        let open_unjoined = new_session(&engine).await;
        let open_joined = new_session(&engine).await;
        super::lock(&engine.force_sessions).replace(vec![
            closed.clone(),
            open_unjoined.clone(),
            open_joined.clone(),
        ]);
        let _closed_dispatcher = engine.dispatching(&closed);
        let _open_dispatcher = engine.dispatching(&open_unjoined);
        engine.latch().await;
        let report = shutdown(&engine).await;
        assert_eq!(report.unjoined_dispatchers, 2, "{report:?}");
        assert_eq!(report.unclosed_sessions, 2, "{report:?}");
        let types = event_types(&engine, &open_unjoined).await;
        assert!(!types.contains(&"session.closed".to_owned()), "{types:?}");
    });
}

/// T3-S5 round 3, decision 14 (design §6.8, §7.4): after a latch, when
/// every force session is unjoined the pass visits none of them, and each
/// still counts only if Store does not read it closed.
#[test]
fn after_a_latch_only_unjoined_sessions_count_by_their_durable_state() {
    let Some(root) = child("after_a_latch_only_unjoined_sessions_count_by_their_durable_state")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let closed = durably_closed_session(&engine).await;
        let open_unjoined = new_session(&engine).await;
        super::lock(&engine.force_sessions).replace(vec![closed.clone(), open_unjoined.clone()]);
        let _closed_dispatcher = engine.dispatching(&closed);
        let _open_dispatcher = engine.dispatching(&open_unjoined);
        engine.latch().await;
        let report = shutdown(&engine).await;
        assert_eq!(report.unclosed_sessions, 1, "{report:?}");
    });
}

/// T3-S5 round 4, decision 16 (design §6.8): without a latch, an unjoined
/// force session is also counted by its durable state. A durably closed one
/// is not counted; an open one is. Neither gets a closure write.
#[test]
fn without_a_latch_unjoined_sessions_count_by_their_durable_state() {
    let Some(root) = child("without_a_latch_unjoined_sessions_count_by_their_durable_state") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let closed = durably_closed_session(&engine).await;
        let open_unjoined = new_session(&engine).await;
        super::lock(&engine.force_sessions).replace(vec![closed.clone(), open_unjoined.clone()]);
        let _closed_dispatcher = engine.dispatching(&closed);
        let _open_dispatcher = engine.dispatching(&open_unjoined);
        let report = shutdown(&engine).await;
        assert!(!report.store_failed, "{report:?}");
        assert_eq!(report.unjoined_dispatchers, 2, "{report:?}");
        assert_eq!(report.unclosed_sessions, 1, "{report:?}");
        let types = event_types(&engine, &open_unjoined).await;
        assert!(!types.contains(&"session.closed".to_owned()), "{types:?}");
    });
}

/// Turn 2 of a new session is running in `engine` (turn 1 ended at its
/// failed launch), with its lane mapping `fake-turn-1` to turn 1,
/// `fake-turn-2` to turn 2, and `gone` evicted past the mapping bound.
async fn running_turn_2(
    engine: &Engine,
    root: &Path,
) -> (
    SessionId,
    std::sync::Arc<super::Slot>,
    super::lane::LaneClaim,
    super::TurnRecord,
    crate::api::Effective,
    tokio::sync::watch::Receiver<Option<via_adapters::StopOrder>>,
) {
    let session = new_session(engine).await;
    // Turn 1 ends: the absent anchor fails its launch.
    dispatch(engine, &session).await;
    assert!(engine.result(&format!("{session}/1")).await.is_ok());
    resume(engine, &session, None).await;
    // Turn 2 is running.
    let next = events_page(engine, &session).await["events"]
        .as_array()
        .unwrap()
        .len() as u64
        + 1;
    let submitted = Event {
        seq: next,
        session_id: &session,
        turn: Some(2),
        late: false,
        at: &rfc3339(std::time::SystemTime::now()),
        body: EventBody::TurnSubmitted { attempt: 1 },
    }
    .to_value()
    .unwrap();
    engine
        .store
        .commit_submission(SubmissionRecord {
            session_id: session.clone(),
            turn: turn(2),
            event: submitted,
        })
        .await
        .unwrap();
    let slot = engine.slot(&session).unwrap();
    let wall = tokio::time::Instant::now() + Duration::from_secs(3600);
    let (_route, orders) =
        slot.start_running(turn(2), wall, super::progress::Progress::starting(2));
    let route = engine
        .store
        .session_snapshot(&session)
        .await
        .unwrap()
        .unwrap()
        .route;
    let lane = engine
        .open_lane(&session, &route, "fake", root.to_path_buf())
        .await;
    // `gone` is evicted by the bound's worth of later vendor turns.
    lane.map_vendor_turn("gone", turn(1));
    for filler in 1..super::lane::VENDOR_TURNS {
        lane.map_vendor_turn(&format!("filler-{filler}"), turn(1));
    }
    lane.map_vendor_turn("fake-turn-1", turn(1));
    lane.map_vendor_turn("fake-turn-2", turn(2));
    let record = super::TurnRecord {
        session: session.clone(),
        turn: turn(2),
        head: super::journal::Head::new(None),
        accepted: None,
        first_failure: None,
        uncertain: None,
        steps: super::progress::StepTracker::default(),
        vendor: super::lane::VendorRecord::default(),
    };
    let effective: crate::api::Effective = serde_json::from_value(json!({
        "model":"fake","effort":null,"bound":null,
        "deadlines":{"wall_ms":30_000,"idle_ms":600_000},"max_steps":null
    }))
    .unwrap();
    (session, slot, lane, record, effective, orders)
}

/// (14) AD4, C1 §6.1: a denial naming an earlier, ended turn's vendor turn
/// arrives while turn 2 runs. It is committed `action.denied` with that
/// turn's number and `late: true`, under the running turn, and stays out
/// of turn 2's `denied_actions`; turn 2's own denial is kept there. Sol r1
/// F6 (C2 §2): one naming a genuinely unseen vendor turn is session-level,
/// `turn: null`, and one naming a vendor turn evicted past the 64-mapping
/// bound is dropped; neither reaches turn 2's list. The end-to-end case is
/// `conformance_core`'s.
#[test]
fn a_late_denial_is_committed_late_and_kept_out_of_the_running_turn() {
    let Some(root) = child("a_late_denial_is_committed_late_and_kept_out_of_the_running_turn")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, slot, lane, mut record, effective, orders) =
            running_turn_2(&engine, &root).await;
        let denial = |vendor_turn: &str, target: &str| via_adapters::ObservationItem {
            at: tokio::time::Instant::now(),
            vendor_turn: Some(
                via_adapters::VendorTurnId::try_from(vendor_turn.to_owned()).unwrap(),
            ),
            observation: via_adapters::Observation::ActionDenied(via_adapters::Denial {
                kind: via_adapters::DenialKind::Command,
                target: target.to_owned(),
                reason: "policy".to_owned(),
            }),
        };
        let queued = vec![
            denial("fake-turn-1", "late"),
            denial("fake-turn-2", "own"),
            denial("stranger", "session"),
            denial("gone", "expired"),
        ];
        engine
            .drain_queued(
                (&slot, Some(&*lane)),
                &mut record,
                &effective,
                orders,
                queued,
            )
            .await;
        assert!(record.first_failure.is_none());
        let page = events_page(&engine, &session).await;
        let denied: Vec<&Value> = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "action.denied")
            .collect();
        assert_eq!(denied.len(), 3, "{page}");
        assert_eq!(
            (&denied[0]["turn"], &denied[0]["late"], &denied[0]["target"]),
            (&json!(1), &json!(true), &json!("late")),
            "{page}"
        );
        assert_eq!(
            (&denied[1]["turn"], &denied[1]["late"], &denied[1]["target"]),
            (&json!(2), &json!(false), &json!("own")),
            "{page}"
        );
        assert_eq!(
            (&denied[2]["turn"], &denied[2]["late"], &denied[2]["target"]),
            (&Value::Null, &json!(false), &json!("session")),
            "{page}"
        );
        let (kept, total) = record.vendor.denied.into_parts();
        let kept = serde_json::to_value(&kept).unwrap();
        assert_eq!(total, 1, "only turn 2's own denial: {kept}");
        assert_eq!(kept[0]["target"], "own", "{kept}");
    });
}

/// Sol r1 F12 (C2 §2, H3): a session's lane opens its driver from the
/// session's stored route identity: its harness, its receipt's route and
/// its recorded adapter version, never a constant.
#[test]
fn a_lane_opens_from_the_sessions_stored_route_identity() {
    let Some(root) = child("a_lane_opens_from_the_sessions_stored_route_identity") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let session = new_session(&engine).await;
        dispatch(&engine, &session).await;
        let lane = super::lock(&engine.lanes).get(&session).cloned().unwrap();
        let reference = &lane.reference;
        assert_eq!(
            (
                reference.harness.as_str(),
                reference.route.as_str(),
                reference.adapter_version.as_str()
            ),
            ("fake", "fake", env!("CARGO_PKG_VERSION"))
        );
    });
}

/// Sol r1 F12 (C2 §2 Recover, AD9): restart recovery asks the adapter set
/// once per session with unfinished turns, from the session's stored
/// route identity and with Host's reconciled facts for it; the fake never
/// resumes, so without Host evidence its answer is `unknown`, and the turn
/// is recovered `unknown` as before.
#[test]
fn recovery_asks_the_adapter_per_session_with_the_reconciled_facts() {
    let Some(root) = child("recovery_asks_the_adapter_per_session_with_the_reconciled_facts")
    else {
        return;
    };
    run(async {
        let session = {
            let earlier = open(&root);
            let session = new_session(&earlier).await;
            // Turn 1 is submitted, then the daemon is gone.
            let submitted = Event {
                seq: 2,
                session_id: &session,
                turn: Some(1),
                late: false,
                at: &rfc3339(std::time::SystemTime::now()),
                body: EventBody::TurnSubmitted { attempt: 1 },
            }
            .to_value()
            .unwrap();
            earlier
                .store
                .commit_submission(SubmissionRecord {
                    session_id: session.clone(),
                    turn: turn(1),
                    event: submitted,
                })
                .await
                .unwrap();
            session
        };
        let engine = open(&root);
        assert_eq!(engine.recover().await.unwrap(), 1);
        let recoveries = super::lock(&engine.faults.recoveries).clone();
        assert_eq!(recoveries, [(session.clone(), 0, "unknown")]);
        let envelope = engine.result(&format!("{session}/1")).await.unwrap();
        let envelope: Value = serde_json::from_str(envelope.get()).unwrap();
        assert_eq!(envelope["state"], "unknown", "{envelope}");
    });
}

/// Sol r1 F2, F13, Sol r2 F13 (C2 §2 health, `TurnAbandoned`): a turn
/// whose run is dropped while pending, after Core's run loop took the
/// session channel from the monitor, abandons its `run_turn`, which fails
/// the driver's health. Its claim and the channel go back to the lane; the
/// lane's monitor, with no observation and no dispatch, keeps that first
/// cause and retires the failed driver.
#[test]
fn the_lane_monitor_retires_a_driver_whose_turn_was_abandoned() {
    let Some(root) = child("the_lane_monitor_retires_a_driver_whose_turn_was_abandoned") else {
        return;
    };
    run(async {
        use via_adapters::{DriverFailure, DriverHealth, Prepared, TurnActivity, TurnCx, TurnSpec};
        let engine = open(&root);
        let (_session, slot, claim, mut record, effective, orders) =
            running_turn_2(&engine, &root).await;
        let lane = std::sync::Arc::clone(claim.lane());
        let now = tokio::time::Instant::now();
        let (_stop, stop) = tokio::sync::watch::channel(None);
        let (_force, force) = tokio::sync::watch::channel(None);
        let cx = TurnCx {
            turn: turn(2),
            prepared: Prepared::NeedsConnection,
            capacity: None,
            activity: TurnActivity::new(now),
            wall: Deadline::at(now + Duration::from_secs(60)),
            tool_grace: Duration::from_secs(60),
            stop,
            force,
        };
        let spec = TurnSpec {
            prompt: "p".to_owned(),
            ..TurnSpec::default()
        };
        {
            let mut running = Box::pin(engine.execute_turn(
                (&slot, &claim),
                (&mut record, &effective),
                orders,
                (spec, cx),
            ));
            // Poll until the run loop holds the channel: the same poll goes
            // on to start `run_turn`. Dropping the run then abandons it.
            for _ in 0..1_000 {
                tokio::select! {
                    biased;
                    () = &mut running => panic!("the turn ended before it was abandoned"),
                    () = std::future::ready(()) => {}
                }
                if lane.turn_holds_channel() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(lane.turn_holds_channel(), "the run loop took the channel");
        }
        assert!(
            !lane.turn_holds_channel(),
            "the channel went back to the lane"
        );
        drop(claim);
        let mut health = lane.driver.health();
        tokio::time::timeout(
            Duration::from_secs(5),
            health.wait_for(|health| matches!(health, DriverHealth::Closed)),
        )
        .await
        .expect("the monitor retires the failed driver")
        .unwrap();
        assert_eq!(lane.first_cause(), Some(DriverFailure::TurnAbandoned));
        assert!(lane.claim().is_none(), "a retired lane is never claimed");
    });
}

/// Sol r1 F4 (C2 §2 observations before turns): between turns the lane's
/// monitor owns the session channel: non-durable items (progress, a vendor
/// close) are dropped at once, returning their budget, and a durable one
/// is committed as it arrives, session-level, never the next turn's
/// (decision H3 as narrowed).
/// Without the drain, the channel fills and its sender blocks.
#[test]
fn between_turns_the_lane_monitor_drains_the_session_channel() {
    let Some(root) = child("between_turns_the_lane_monitor_drains_the_session_channel") else {
        return;
    };
    run(async {
        use via_adapters::{Observation, ProgressMarks};
        let engine = open(&root);
        let session = new_session(&engine).await;
        let (lane, sender) = adopt_test_lane(&engine, &root, &session).await;
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1_000));
        let send = |observation| send_held(&sender, &budget, None, observation);
        // 32 items through a 4-item channel.
        for _ in 0..30 {
            let marks = ProgressMarks {
                model: true,
                ..ProgressMarks::default()
            };
            send(Observation::Progress(marks)).await;
        }
        send(denied("between")).await;
        send(Observation::VendorClosed("idle".to_owned())).await;
        until(|| budget.available_permits() == 1_000).await;
        assert_eq!(
            denials(&engine, &session).await,
            [(json!("between"), Value::Null, json!(false))],
            "session-level, committed as it arrived"
        );
        drop(lane);
    });
}

/// A lane adopted for `session` on the fake adapter, with the sending end
/// of its 4-item session channel.
async fn adopt_test_lane(
    engine: &Engine,
    root: &Path,
    session: &SessionId,
) -> (
    std::sync::Arc<super::lane::Lane>,
    tokio::sync::mpsc::Sender<via_adapters::Admitted>,
) {
    use via_adapters::{
        Inherit, ObservationBudget, SessionCx, SessionSpec, VendorOptions, observation_channel,
    };
    let route = engine
        .store
        .session_snapshot(session)
        .await
        .unwrap()
        .unwrap()
        .route;
    let reference = super::lane::session_ref(&route);
    let (sink, _unused) = observation_channel();
    let driver = engine.adapter.open_session(
        &reference,
        SessionSpec {
            session_id: session.clone(),
            model: "fake".to_owned(),
            instructions: None,
            initial_bound: None,
            cwd: root.to_path_buf(),
            vendor: VendorOptions::new(),
            inherit: Inherit::OD2_DEFAULT,
            confirmed_vendor_session_id: None,
            allow_untested: false,
        },
        SessionCx {
            observations: sink,
            tracker: engine.tracker.clone(),
            cancel: engine.cancel.child_token(),
        },
    );
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    engine.adopt_lane(
        session,
        (driver, receiver, ObservationBudget::new()),
        (reference, &route),
    );
    let lane = super::lock(&engine.lanes).get(session).cloned().unwrap();
    (lane, sender)
}

/// Sends `observation`, naming `vendor_turn`, into a test lane's channel
/// with 10 permits of `budget`.
async fn send_held(
    sender: &tokio::sync::mpsc::Sender<via_adapters::Admitted>,
    budget: &std::sync::Arc<tokio::sync::Semaphore>,
    vendor_turn: Option<&str>,
    observation: via_adapters::Observation,
) {
    let permit = std::sync::Arc::clone(budget)
        .try_acquire_many_owned(10)
        .unwrap();
    let item = via_adapters::ObservationItem {
        at: tokio::time::Instant::now(),
        vendor_turn: vendor_turn
            .map(|turn| via_adapters::VendorTurnId::try_from(turn.to_owned()).unwrap()),
        observation,
    };
    tokio::time::timeout(
        Duration::from_secs(2),
        sender.send(via_adapters::Admitted { item, permit }),
    )
    .await
    .expect("the monitor drains the channel between turns")
    .unwrap();
}

/// A denial named `target`.
fn denied(target: &str) -> via_adapters::Observation {
    via_adapters::Observation::ActionDenied(via_adapters::Denial {
        kind: via_adapters::DenialKind::Network,
        target: target.to_owned(),
        reason: "r".to_owned(),
    })
}

/// The session's `action.denied` events as `(target, turn, late)`.
async fn denials(engine: &Engine, session: &SessionId) -> Vec<(Value, Value, Value)> {
    events_page(engine, session).await["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "action.denied")
        .map(|event| {
            (
                event["target"].clone(),
                event["turn"].clone(),
                event["late"].clone(),
            )
        })
        .collect()
}

/// Turn 1 of a new session has ended (its launch failed) and its slot is
/// retired; the session has an adopted lane with its earlier vendor turn
/// `vt-1` mapped to turn 1.
async fn idle_session_with_lane(
    engine: &Engine,
    root: &Path,
) -> (
    SessionId,
    std::sync::Arc<super::lane::Lane>,
    tokio::sync::mpsc::Sender<via_adapters::Admitted>,
) {
    let session = new_session(engine).await;
    dispatch(engine, &session).await;
    assert!(engine.slot(&session).is_none(), "retired");
    let (lane, sender) = adopt_test_lane(engine, root, &session).await;
    lane.map_vendor_turn("vt-1", TurnNumber::try_from(1).unwrap());
    (session, lane, sender)
}

/// Waits until the session's `action.denied` events are `expected`.
async fn until_denials(engine: &Engine, session: &SessionId, expected: &[(Value, Value, Value)]) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while denials(engine, session).await != expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the denials are committed");
}

/// S-CORE c4 r2 item 1b (C2 §2 session drain, decision H3 as narrowed):
/// a durable observation received between turns is committed as it
/// arrives, with its own attribution and no next turn: a session-level
/// denial `turn: null`, and a late one naming an earlier vendor turn with
/// that turn and `late: true`. No slot is left behind.
#[test]
fn a_between_turn_denial_is_committed_before_any_next_turn() {
    let Some(root) = child("a_between_turn_denial_is_committed_before_any_next_turn") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, _lane, sender) = idle_session_with_lane(&engine, &root).await;
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1_000));
        send_held(&sender, &budget, None, denied("session")).await;
        send_held(&sender, &budget, Some("vt-1"), denied("late")).await;
        until_denials(
            &engine,
            &session,
            &[
                (json!("session"), Value::Null, json!(false)),
                (json!("late"), json!(1), json!(true)),
            ],
        )
        .await;
        until(|| budget.available_permits() == 1_000).await;
        assert!(engine.slot(&session).is_none(), "the write's slot is gone");
        // No next turn was queued or run.
        let page = events_page(&engine, &session).await;
        assert!(
            page["events"]
                .as_array()
                .unwrap()
                .iter()
                .all(|event| event["turn"] != 2),
            "{page}"
        );
    });
}

/// S-CORE c4 r2 items 1a and 5 (decision H3 as narrowed, C1 §6.1): an
/// identity confirmed between turns commits `session.opened` with C1's
/// members, the handshake's `vendor_version` included, and writes the
/// session's identity columns in the same transaction, from which
/// `status` and the lane's recovery read it.
#[test]
fn a_between_turn_identity_commits_its_open_event_and_the_columns() {
    let Some(root) = child("a_between_turn_identity_commits_its_open_event_and_the_columns") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, lane, sender) = idle_session_with_lane(&engine, &root).await;
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1_000));
        let identity = via_adapters::observation::Identity {
            vendor_session_id: "vs-1".to_owned(),
            connection_id: "c-1".to_owned(),
            transcript: Some(PathBuf::from("/t/vs-1.jsonl")),
            vendor_version: Some("9.9.9".to_owned()),
        };
        let confirmed = via_adapters::Observation::IdentityConfirmed(identity);
        send_held(&sender, &budget, None, confirmed).await;
        until(|| lane.verified()).await;
        let page = events_page(&engine, &session).await;
        let opened: Vec<&Value> = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "session.opened")
            .collect();
        assert_eq!(opened.len(), 1, "{page}");
        let mut members: Vec<&str> = opened[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        members.sort_unstable();
        assert_eq!(
            members,
            [
                "at",
                "late",
                "route",
                "seq",
                "session_id",
                "turn",
                "type",
                "vendor_session_id",
                "vendor_version"
            ],
            "{page}"
        );
        assert_eq!(opened[0]["vendor_version"], "9.9.9");
        assert_eq!(opened[0]["vendor_session_id"], "vs-1");
        assert!(opened[0]["turn"].is_null());
        let route = engine
            .store
            .session_snapshot(&session)
            .await
            .unwrap()
            .unwrap()
            .route;
        assert_eq!(route.vendor_session_id.as_deref(), Some("vs-1"));
        assert_eq!(route.transcript.as_deref(), Some("/t/vs-1.jsonl"));
    });
}

/// S-CORE c4 r2 item 1b: final shutdown joins each lane's monitor and
/// commits what its channel still has before the lanes go; a denial
/// received after the final-shutdown fence is still committed.
#[test]
fn a_between_turn_denial_at_final_shutdown_is_committed() {
    let Some(root) = child("a_between_turn_denial_at_final_shutdown_is_committed") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, lane, sender) = idle_session_with_lane(&engine, &root).await;
        engine.enter_final_shutdown().await;
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1_000));
        send_held(&sender, &budget, None, denied("at shutdown")).await;
        drop(lane);
        let report = shutdown(&engine).await;
        assert_eq!(report.unstarted_dispatchers, 0);
        assert_eq!(
            denials(&engine, &session).await,
            [(json!("at shutdown"), Value::Null, json!(false))]
        );
        assert_eq!(budget.available_permits(), 1_000);
    });
}

/// S-CORE c4 r2 item 1b: a denial received between turns and followed by
/// the session's close is committed before `session.closed`; nothing
/// commits after it.
#[test]
fn a_between_turn_denial_followed_by_close_is_still_in_events() {
    let Some(root) = child("a_between_turn_denial_followed_by_close_is_still_in_events") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, _lane, sender) = idle_session_with_lane(&engine, &root).await;
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1_000));
        send_held(&sender, &budget, None, denied("before close")).await;
        let (closed, ()) = tokio::join!(
            close(&engine, &session, None),
            dispatch_closing(&engine, &session)
        );
        closed.unwrap();
        let page = events_page(&engine, &session).await;
        let types: Vec<&str> = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        let denied_at = types.iter().position(|kind| *kind == "action.denied");
        let closed_at = types.iter().position(|kind| *kind == "session.closed");
        assert!(denied_at.is_some() && denied_at < closed_at, "{types:?}");
        assert_eq!(*types.last().unwrap(), "session.closed", "{types:?}");
        assert_eq!(budget.available_permits(), 1_000);
    });
}

/// Sol r2 #3: a turn dropped while it drains the session channel loses no
/// durable item it did not handle: the receiver goes back to the lane with
/// its claim, and the monitor commits the rest.
#[test]
fn a_dropped_turn_mid_drain_loses_no_durable_item() {
    let Some(root) = child("a_dropped_turn_mid_drain_loses_no_durable_item") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, lane, sender) = idle_session_with_lane(&engine, &root).await;
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1_000));
        let claim = lane.claim().expect("an idle lane is claimed");
        let mut observed = claim.observe().await;
        for target in ["first", "second", "third"] {
            send_held(&sender, &budget, None, denied(target)).await;
        }
        let first = observed.recv().await.unwrap();
        claim.dispose(first).await;
        // The turn is dropped mid-drain.
        drop(observed);
        drop(claim);
        until_denials(
            &engine,
            &session,
            &[
                (json!("first"), Value::Null, json!(false)),
                (json!("second"), Value::Null, json!(false)),
                (json!("third"), Value::Null, json!(false)),
            ],
        )
        .await;
        until(|| budget.available_permits() == 1_000).await;
    });
}

/// Sol r2 #2, #3, #9: replacing a retired lane joins its monitor, commits
/// what its channel still has before the successor exists, and keeps the
/// session's one byte budget for the successor's channel.
#[test]
fn replacing_a_retired_lane_keeps_its_items_and_the_budget() {
    let Some(root) = child("replacing_a_retired_lane_keeps_its_items_and_the_budget") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, lane, sender) = idle_session_with_lane(&engine, &root).await;
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1_000));
        assert!(lane.begin_retire(), "an idle lane retires");
        assert!(lane.claim().is_none(), "a retiring lane is never claimed");
        for target in ["one", "two"] {
            send_held(&sender, &budget, None, denied(target)).await;
        }
        let route = engine
            .store
            .session_snapshot(&session)
            .await
            .unwrap()
            .unwrap()
            .route;
        let successor = engine
            .open_lane(&session, &route, "fake", root.clone())
            .await;
        // Committed before the successor was made.
        assert_eq!(
            denials(&engine, &session).await,
            [
                (json!("one"), Value::Null, json!(false)),
                (json!("two"), Value::Null, json!(false)),
            ]
        );
        assert_eq!(budget.available_permits(), 1_000);
        assert!(
            successor.budget().shares(lane.budget()),
            "one session budget"
        );
        assert!(
            matches!(
                *lane.driver.health().borrow(),
                via_adapters::DriverHealth::Closed
            ),
            "retired before it was replaced"
        );
    });
}

/// S-CORE c4 r2 item 4 (C1 §5: adapter-reported warnings "reach the
/// envelope only as these codes"): the running turn's own adapter warning
/// whose code is in C1's closed list adds that code to the envelope's
/// `warnings` once, with VIA's own message and the data within 4 KiB;
/// another code, and a late one of an earlier turn, stay events only.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one turn's warnings and the envelope built from them"
)]
fn closed_list_adapter_warnings_reach_the_envelope_once_per_code() {
    let Some(root) = child("closed_list_adapter_warnings_reach_the_envelope_once_per_code") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, slot, lane, mut record, effective, orders) =
            running_turn_2(&engine, &root).await;
        let warning = |vendor_turn: &str, code: &'static str, data: Option<Value>| {
            via_adapters::ObservationItem {
                at: tokio::time::Instant::now(),
                vendor_turn: Some(
                    via_adapters::VendorTurnId::try_from(vendor_turn.to_owned()).unwrap(),
                ),
                observation: via_adapters::Observation::Warning(via_adapters::Warning {
                    code,
                    message: "the vendor's own words".to_owned(),
                    data,
                }),
            }
        };
        let categories = json!({"categories":[{"category":"c","requested":"r","effective":"e"}]});
        let queued = vec![
            warning("fake-turn-2", "vendor_specific", None),
            warning(
                "fake-turn-2",
                "config_switch_unverified",
                Some(categories.clone()),
            ),
            warning(
                "fake-turn-2",
                "config_switch_unverified",
                Some(json!({"second":true})),
            ),
            warning("fake-turn-1", "structured_output_missing", None),
            warning(
                "fake-turn-2",
                "deprecated",
                Some(json!({"big":"d".repeat(5000)})),
            ),
        ];
        engine
            .drain_queued(
                (&slot, Some(&*lane)),
                &mut record,
                &effective,
                orders,
                queued,
            )
            .await;
        assert!(record.first_failure.is_none());
        let page = events_page(&engine, &session).await;
        let events = page["events"].as_array().unwrap();
        let codes: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == "warning")
            .map(|event| &event["code"])
            .collect();
        assert_eq!(codes.len(), 5, "every one is an event: {page}");
        assert!(codes.contains(&&json!("vendor_specific")), "{page}");
        let at = "2026-01-01T00:00:00.000Z".to_owned();
        let envelope = super::terminal::turn_envelope(
            (&session, record.turn),
            super::terminal::blank("completed", "end_turn", None),
            None,
            (None, None),
            (
                crate::api::Timestamps {
                    queued_at: at.clone(),
                    submitted_at: None,
                    accepted_at: None,
                    ended_at: at,
                },
                None,
            ),
            (1, 1),
            record.vendor.clone(),
        );
        let envelope = serde_json::to_value(&envelope).unwrap();
        let adapter: Vec<&Value> = envelope["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|warning| warning["code"] != "vendor_version_untested")
            .collect();
        let listed: Vec<&Value> = adapter.iter().map(|warning| &warning["code"]).collect();
        assert_eq!(
            listed,
            [&json!("config_switch_unverified"), &json!("deprecated")],
            "{envelope}"
        );
        assert_eq!(adapter[0]["data"], categories, "the first one's data");
        for warning in &adapter {
            let message = warning["message"].as_str().unwrap();
            assert!(!message.is_empty() && message != "the vendor's own words");
            assert!(message.len() <= 1024);
        }
        assert!(
            adapter[1].get("data").is_none(),
            "over 4 KiB: {}",
            adapter[1]
        );
    });
}

/// Sol r1 F4 (C1 §6.1, §5): adapter warnings while a turn runs commit
/// `warning` events with their own attribution, the running turn's or an
/// earlier turn's `late: true`, within C1 §5's caps: `message` cut to
/// 1 KiB encoded, `data` over 4 KiB encoded left out.
#[test]
fn adapter_warnings_commit_warning_events_within_the_caps() {
    let Some(root) = child("adapter_warnings_commit_warning_events_within_the_caps") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, slot, lane, mut record, effective, orders) =
            running_turn_2(&engine, &root).await;
        let warning = |vendor_turn: &str, message: String, data: Option<Value>| {
            via_adapters::ObservationItem {
                at: tokio::time::Instant::now(),
                vendor_turn: Some(
                    via_adapters::VendorTurnId::try_from(vendor_turn.to_owned()).unwrap(),
                ),
                observation: via_adapters::Observation::Warning(via_adapters::Warning {
                    code: "deprecated",
                    message,
                    data,
                }),
            }
        };
        let queued = vec![
            warning("fake-turn-2", "own".to_owned(), Some(json!({"k":"v"}))),
            warning("fake-turn-1", "late".to_owned(), None),
            warning(
                "fake-turn-2",
                "w".repeat(2048),
                Some(json!({"big":"d".repeat(5000)})),
            ),
        ];
        engine
            .drain_queued(
                (&slot, Some(&*lane)),
                &mut record,
                &effective,
                orders,
                queued,
            )
            .await;
        assert!(record.first_failure.is_none());
        let page = events_page(&engine, &session).await;
        let warnings: Vec<&Value> = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "warning")
            .collect();
        assert_eq!(warnings.len(), 3, "{page}");
        assert_eq!(
            (
                &warnings[0]["turn"],
                &warnings[0]["late"],
                &warnings[0]["message"]
            ),
            (&json!(2), &json!(false), &json!("own")),
        );
        assert_eq!(warnings[0]["code"], "deprecated");
        assert_eq!(warnings[0]["data"], json!({"k":"v"}));
        assert_eq!(
            (
                &warnings[1]["turn"],
                &warnings[1]["late"],
                &warnings[1]["message"]
            ),
            (&json!(1), &json!(true), &json!("late")),
        );
        assert_eq!(warnings[2]["message"], "w".repeat(1022));
        assert!(warnings[2].get("data").is_none(), "{}", warnings[2]);
    });
}
