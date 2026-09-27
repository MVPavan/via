//! Receipt reconciliation and dispatch eligibility through the Engine over a
//! real Store, with the in-process fault backend. Each case re-executes this
//! binary with fake settings, since Core reads them once from the environment.

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
use crate::{ApiError, FakeConfig, ResumeParams, SessionId, SpawnParams, TurnNumber};

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
    Engine::open(
        &root.join("state"),
        &root.join("runtime"),
        FakeConfig::from_environment().unwrap(),
        root.join("absent-anchor"),
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

fn unknown_outcome() -> Value {
    json!({"kind":"store_error","commit_outcome":"unknown","retry":"same_key_only"})
}

fn turn(n: u32) -> TurnNumber {
    TurnNumber::try_from(n).unwrap()
}

/// Blocker 1: SQLite committed the spawn but its reply was lost. Core
/// reconciles against Store, registers the turn and hands it off once; a keyed
/// retry replays the receipt and creates nothing more to drive.
#[test]
fn a_committed_spawn_whose_reply_is_lost_is_handed_off_once() {
    let Some(root) = child("a_committed_spawn_whose_reply_is_lost_is_handed_off_once") else {
        return;
    };
    run(async {
        let engine = open(&root);
        engine
            .faults
            .receipt_reply_lost
            .store(true, Ordering::Release);
        let first = spawn(&engine, Some("k-1")).await.unwrap();
        let (session, number) = first.drive.clone().expect("the committed turn is driven");
        assert_eq!(number, turn(1));
        assert_eq!(engine.active(), 1, "registered exactly once");
        let retry = spawn(&engine, Some("k-1")).await.unwrap();
        assert!(retry.drive.is_none(), "a replay creates no second drive");
        assert_eq!(retry.receipt, first.receipt);
        assert_eq!(engine.active(), 1);
        assert_eq!(first.receipt["session_id"], session.as_str());
    });
}

/// Blocker 1 for `resume`: the committed turn is registered and handed off
/// once, and the session's event head stays dense for the next writer.
#[test]
fn a_committed_resume_whose_reply_is_lost_is_handed_off_once() {
    let Some(root) = child("a_committed_resume_whose_reply_is_lost_is_handed_off_once") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, _) = spawn(&engine, None).await.unwrap().drive.unwrap();
        engine
            .faults
            .receipt_reply_lost
            .store(true, Ordering::Release);
        let first = resume(&engine, &session, Some("r-1")).await;
        assert_eq!(first.drive, Some((session.clone(), turn(2))));
        assert_eq!(engine.active(), 2);
        let retry = resume(&engine, &session, Some("r-1")).await;
        assert!(retry.drive.is_none());
        assert_eq!(retry.receipt, first.receipt);
        let third = resume(&engine, &session, None).await;
        assert_eq!(third.drive, Some((session.clone(), turn(3))));
        let events = engine.events(session.as_str()).await.unwrap();
        let seqs: Vec<_> = events["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, [1, 2, 3]);
    });
}

/// Blocker 1, outcome unknown: the reply was lost and Store cannot be read.
/// The request is C1 §8.1 `store_error` with `commit_outcome: unknown` and
/// `retry: same_key_only`; the keyed retry then adopts the committed turn and
/// hands it off exactly once.
#[test]
fn an_unknown_receipt_outcome_is_store_error_and_its_keyed_retry_adopts_it_once() {
    let Some(root) =
        child("an_unknown_receipt_outcome_is_store_error_and_its_keyed_retry_adopts_it_once")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        engine
            .faults
            .receipt_reply_lost
            .store(true, Ordering::Release);
        engine
            .faults
            .reconcile_unreadable
            .store(true, Ordering::Release);
        let error = spawn(&engine, Some("k-1")).await.unwrap_err();
        assert_eq!((error.code, error.data()), (-32018, unknown_outcome()));
        engine
            .faults
            .reconcile_unreadable
            .store(false, Ordering::Release);
        let adopted = spawn(&engine, Some("k-1")).await.unwrap();
        let (session, _) = adopted
            .drive
            .expect("the retry hands off the committed turn");
        assert!(spawn(&engine, Some("k-1")).await.unwrap().drive.is_none());

        engine
            .faults
            .receipt_reply_lost
            .store(true, Ordering::Release);
        engine
            .faults
            .reconcile_unreadable
            .store(true, Ordering::Release);
        let (params, raw) = resume_raw(&session, Some("r-1"));
        let error = engine.resume(params, &raw).await.unwrap_err();
        assert_eq!(error.data(), unknown_outcome());
        engine
            .faults
            .reconcile_unreadable
            .store(false, Ordering::Release);
        let adopted = resume(&engine, &session, Some("r-1")).await;
        assert_eq!(adopted.drive, Some((session.clone(), turn(2))));
        assert!(resume(&engine, &session, Some("r-1")).await.drive.is_none());
        assert_eq!(engine.active(), 2, "each committed turn is registered once");
    });
}

/// Commits turn 1's submission and `state` terminal directly, then ends its
/// drive without a clean disposition, as a drive whose terminal commit could
/// not be confirmed does.
async fn end_turn_one(engine: &Engine, session: &SessionId, state: Option<&str>) {
    end_turn(engine, session, 1, state).await;
}

/// `end_turn_one` for turn `n`; `pending` is a cancelled terminal whose
/// cleanup is still pending.
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
    }
    head.committed(count);
    engine.queued.fetch_sub(1, Ordering::AcqRel);
    engine.active.fetch_sub(1, Ordering::AcqRel);
    drop(super::queue::Finish::new(&slot, turn(n)));
}

async fn submitted(engine: &Engine, session: &SessionId, n: u32) -> bool {
    let envelope = tokio::time::timeout(
        Duration::from_secs(20),
        engine.drive(session.clone(), turn(n)),
    )
    .await
    .expect("a drive ends");
    let _ = envelope;
    let result = engine
        .result(&format!("{}/{n}", session.as_str()))
        .await
        .unwrap();
    !result["timestamps"]["submitted_at"].is_null()
}

/// Item 4: dispatch follows the durable predecessor, not how its drive ended.
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
        let (session, _) = spawn(&engine, None).await.unwrap().drive.unwrap();
        resume(&engine, &session, None).await;
        end_turn_one(&engine, &session, Some("failed")).await;
        assert!(
            submitted(&engine, &session, 2).await,
            "the queued turn runs"
        );
        // A later resume, accepted after turn 1's unclean drive ended.
        let (session, _) = spawn(&engine, None).await.unwrap().drive.unwrap();
        end_turn_one(&engine, &session, Some("failed")).await;
        resume(&engine, &session, None).await;
        assert!(submitted(&engine, &session, 2).await, "a later resume runs");
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
            let (session, _) = spawn(&engine, None).await.unwrap().drive.unwrap();
            resume(&engine, &session, None).await;
            end_turn_one(&engine, &session, state).await;
            assert!(!submitted(&engine, &session, 2).await, "{state:?}");
            resume(&engine, &session, None).await;
            assert!(!submitted(&engine, &session, 3).await, "{state:?}");
        }
    });
}

/// Round 3 blocker 1: a successor that starts while its predecessor is an
/// orphan (committed, reply lost, outcome unknown) waits rather than cancel;
/// once the keyed retry adopts and runs the predecessor, the successor runs.
#[test]
fn a_successor_waits_for_an_orphan_predecessor_to_be_adopted_then_runs() {
    let Some(root) = child("a_successor_waits_for_an_orphan_predecessor_to_be_adopted_then_runs")
    else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, _) = spawn(&engine, None).await.unwrap().drive.unwrap();
        end_turn_one(&engine, &session, Some("failed")).await;
        engine
            .faults
            .receipt_reply_lost
            .store(true, Ordering::Release);
        engine
            .faults
            .reconcile_unreadable
            .store(true, Ordering::Release);
        let (params, raw) = resume_raw(&session, Some("r-2"));
        assert_eq!(
            engine.resume(params, &raw).await.unwrap_err().data(),
            unknown_outcome()
        );
        let third = resume(&engine, &session, None).await;
        assert_eq!(third.drive, Some((session.clone(), turn(3))));
        let address = format!("{}/3", session.as_str());
        let (ran, ()) = tokio::join!(submitted(&engine, &session, 3), async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let pending = engine.result(&address).await.unwrap_err();
            assert_eq!(pending.kind, "turn_not_finished", "turn 3 stays queued");
            // Reconciliation still fails, so only this keyed retry can adopt
            // turn 2, which then runs and settles.
            let adopted = resume(&engine, &session, Some("r-2")).await;
            assert_eq!(adopted.drive, Some((session.clone(), turn(2))));
            end_turn(&engine, &session, 2, Some("failed")).await;
        });
        assert!(ran, "turn 3 submits after its adopted predecessor");
    });
}

/// Round 3 blocker 2: a failed predecessor read leaves the turn queued; the
/// decision is retried and the turn later runs.
#[test]
fn a_failed_predecessor_read_waits_and_the_turn_later_runs() {
    let Some(root) = child("a_failed_predecessor_read_waits_and_the_turn_later_runs") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let (session, _) = spawn(&engine, None).await.unwrap().drive.unwrap();
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
    });
}

/// Round 3 blocker 3: an unkeyed receipt that committed but whose reply was
/// lost, with reconciliation reads failing for a while, is still reconciled by
/// the daemon itself and handed off exactly once.
#[test]
fn an_unkeyed_committed_receipt_is_reconciled_and_handed_off_once() {
    let Some(root) = child("an_unkeyed_committed_receipt_is_reconciled_and_handed_off_once") else {
        return;
    };
    run(async {
        let engine = open(&root);
        let mut adoptions = engine.take_adoptions().unwrap();
        let (session, _) = spawn(&engine, None).await.unwrap().drive.unwrap();
        engine
            .faults
            .receipt_reply_lost
            .store(true, Ordering::Release);
        engine
            .faults
            .reconcile_unreadable
            .store(true, Ordering::Release);
        let (params, raw) = resume_raw(&session, None);
        assert_eq!(
            engine.resume(params, &raw).await.unwrap_err().data(),
            unknown_outcome()
        );
        // Still unreadable: the next admission reconciles nothing.
        spawn(&engine, None).await.unwrap();
        assert!(adoptions.try_recv().is_err());
        engine
            .faults
            .reconcile_unreadable
            .store(false, Ordering::Release);
        spawn(&engine, None).await.unwrap();
        assert_eq!(adoptions.try_recv().unwrap(), (session.clone(), turn(2)));
        spawn(&engine, None).await.unwrap();
        assert!(adoptions.try_recv().is_err(), "handed off exactly once");
        // Four sessions' first turns plus the adopted turn 2.
        assert_eq!(engine.active(), 5);
    });
}
