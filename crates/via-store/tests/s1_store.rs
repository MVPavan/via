//! Task 3 S1 Store primitives (design §7.1, §10): the close and F12
//! operations and the error split, on schema v6 since Task 4. Written before
//! the operations.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    AnchorIdentity, AnchorIntent, AnchorPhase, CancelCause, CloseIntent, ClosedOutcome,
    ClosedRecord, ClosingRecord, CommitOutcome, FailureResolutionRecord, GroupAbsenceRecord,
    Identity, OperationVerb, ProcessJournal, ResumeRecord, SessionId, SpawnRecord, Store,
    StoreClient, StoreError, StoreLock, SubmissionRecord, SubmitFailedRecord, TerminalExtras,
    TerminalRecord, TurnNumber,
};

const SESSION: &str = "s_7f3k9q2mzr4c";
const OTHER: &str = "s_7f3k9q2mzr4d";

fn session() -> SessionId {
    SessionId::try_from(SESSION).unwrap()
}

fn turn(number: u32) -> TurnNumber {
    TurnNumber::try_from(number).unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn private_dir() -> TempDir {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn event(kind: &str, seq: u64) -> Value {
    json!({"type":kind,"seq":seq,"at":"2026-01-01T00:00:00.000Z"})
}

async fn spawn(client: &StoreClient, id: &str) {
    client
        .commit_spawn(SpawnRecord {
            session_id: SessionId::try_from(id).unwrap(),
            handle_hash: [7_u8; 32],
            receipt: json!({"state":"queued"}),
            params: json!({"harness":"fake"}),
            label: None,
            prompt: "p".into(),
            effective: json!({"deadlines":{"wall_ms":1}}),
            initial_event: event("turn.queued", 1),
        })
        .await
        .unwrap();
}

async fn resume(client: &StoreClient, number: u32, seq: u64) -> Result<(), StoreError> {
    client
        .commit_resume(ResumeRecord {
            session_id: session(),
            turn: turn(number),
            prompt: "p".into(),
            effective: json!({"deadlines":{"wall_ms":1}}),
            event: event("turn.queued", seq),
            operation: None,
        })
        .await
}

fn ended(number: u32, seq: u64, state: &str) -> TerminalRecord {
    TerminalRecord {
        session_id: session(),
        turn: turn(number),
        envelope: json!({"state":state}),
        event: event("turn.ended", seq),
        steps: Vec::new(),
        link_released: false,
    }
}

fn close_cause() -> TerminalExtras {
    TerminalExtras {
        cancel_cause: Some(CancelCause::Close),
    }
}

fn read_db(root: &TempDir) -> rusqlite::Connection {
    rusqlite::Connection::open(root.path().join("store.sqlite3")).unwrap()
}

/// Task 4 design §6.6, runtime §6: a fresh Store is schema v10; a v9 Store,
/// older than the build, is an unreleased format refused with the named
/// recreate instruction, bytes untouched.
#[test]
fn fresh_store_is_v10_and_a_v9_store_is_refused() {
    let root = private_dir();
    drop(Store::open(root.path()).unwrap());
    let version: i64 = read_db(&root)
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 10);

    // A full Store stamped v9, as a v9 build left it but for the columns.
    let old = private_dir();
    drop(Store::open(old.path()).unwrap());
    let db = old.path().join("store.sqlite3");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 9).unwrap();
        conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
    }
    let before = fs::read(&db).unwrap();
    let Err(error) = Store::open(old.path()) else {
        panic!("a v9 Store opened");
    };
    assert!(error.to_string().contains("schema v9"), "{error}");
    assert!(error.to_string().contains("recreate"), "{error}");
    assert_eq!(fs::read(&db).unwrap(), before);
}

/// T3-S3 round 1, decision 5 (runtime §6.1): a Store is opened, and an
/// existing file probed, only under `store.lock`, the writer exclusion the
/// probe relies on. While another holder has the lock, `open` is refused
/// before the file is read.
#[test]
fn a_store_opens_only_under_store_lock() {
    use std::os::unix::fs::OpenOptionsExt;
    let root = private_dir();
    drop(Store::open(root.path()).unwrap());
    let holder = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.path().join("store.lock"))
        .unwrap();
    holder.try_lock().unwrap();
    let Err(error) = Store::open(root.path()) else {
        panic!("a Store opened while another holder had store.lock");
    };
    assert!(error.to_string().contains("store.lock"), "{error}");
    drop(holder);
    drop(Store::open(root.path()).unwrap());
}

/// T3-S3 round 2, decision 10 (runtime §6.1): a `StoreLock` is bound to
/// the State directory it was taken for. A guard for directory A does not
/// open directory B, whose own `store.lock` another holder has: the
/// mismatch is refused before B's Store is read.
#[test]
fn a_store_lock_opens_only_its_own_state_directory() {
    use std::os::unix::fs::OpenOptionsExt;
    let (a, b) = (private_dir(), private_dir());
    drop(Store::open(b.path()).unwrap());
    let holder = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(b.path().join("store.lock"))
        .unwrap();
    holder.try_lock().unwrap();
    let lock = StoreLock::acquire(a.path()).unwrap();
    let Err(error) = Store::open_locked(b.path(), lock) else {
        panic!("a guard for another State directory opened this Store");
    };
    assert!(
        error.to_string().contains("another State directory"),
        "{error}"
    );
    drop(holder);
    let lock = StoreLock::acquire(a.path()).unwrap();
    drop(Store::open_locked(a.path(), lock).unwrap());
}

/// Design §4 and §10: `Closing` gates `resume` (a refusal, not a failure),
/// the close's cancellations record `cancel_cause = 'close'`, and `Closed`
/// derives `cancelled_turns` and `cleanup` from durable rows, storing the
/// result and the keyed operation's result in the same transaction.
#[test]
fn close_derives_its_result_from_durable_cancel_cause_rows() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client, SESSION).await;
        resume(&client, 2, 2).await.unwrap();
        let intent = CloseIntent {
            op_key: "close-1".to_owned(),
            identity: Identity::of(b"close-params"),
        };
        client
            .commit_closing(ClosingRecord {
                session_id: session(),
                operation: Some(intent.clone()),
            })
            .await
            .unwrap();
        let snapshot = client.session_snapshot(&session()).await.unwrap().unwrap();
        assert!(snapshot.closing && !snapshot.closed);
        let refused = resume(&client, 3, 3).await;
        assert!(
            matches!(refused, Err(StoreError::Refused(_))),
            "{refused:?}"
        );
        let keyed = client
            .keyed_operation(&session(), "close-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(keyed.verb, OperationVerb::Close);
        assert_eq!(keyed.identity, Identity::of(b"close-params"));
        assert!(keyed.result.is_none());
        assert_eq!(
            client.closing_sessions_page(None, 10).await.unwrap(),
            vec![session()]
        );

        // Turn 1 still queued: `Closed` is refused, not failed, and writes nothing.
        let closed = ClosedRecord {
            session_id: session(),
            event: event("session.closed", 5),
            operation: Some(intent.clone()),
        };
        let early = client
            .commit_closed(ClosedRecord {
                event: event("session.closed", 3),
                ..closed.clone()
            })
            .await
            .unwrap();
        assert_eq!(early, ClosedOutcome::Unfinished);

        client
            .commit_terminal_with(ended(1, 3, "cancelled"), close_cause())
            .await
            .unwrap();
        client
            .commit_terminal_with(ended(2, 4, "cancelled"), close_cause())
            .await
            .unwrap();
        let ClosedOutcome::Closed(result) = client.commit_closed(closed).await.unwrap() else {
            panic!("close refused with no unfinished turn");
        };
        assert_closed_result(&client, &result).await;
    });
    drop(store);
    let causes: Vec<Option<String>> = read_db(&root)
        .prepare("SELECT cancel_cause FROM turns ORDER BY number")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(causes, [Some("close".to_owned()), Some("close".to_owned())]);
}

/// The two close cancellations' result, stored and replayed by key.
async fn assert_closed_result(client: &StoreClient, result: &Value) {
    let expected = json!({
        "session_id": SESSION,
        "state": "closed",
        "cancelled_turns": [format!("{SESSION}/1"), format!("{SESSION}/2")],
        "cleanup": "quiescent",
        "leftovers": null,
    });
    assert_eq!(*result, expected);
    assert_eq!(
        client.session_close_result(&session()).await.unwrap(),
        Some(expected.clone())
    );
    let keyed = client
        .keyed_operation(&session(), "close-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(keyed.result, Some(expected));
    assert!(
        client
            .session_snapshot(&session())
            .await
            .unwrap()
            .unwrap()
            .closed
    );
    assert!(
        client
            .closing_sessions_page(None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Design §4 [r1.8]: `cleanup` is `uncertain` while any group of the
/// session's turns lacks a durable absence proof; a session closed another
/// way still derives its result.
#[test]
fn close_cleanup_is_uncertain_with_an_unproven_group() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let journal = store.runtime_resources().into_wire_parts().1;
    runtime().block_on(async {
        spawn(&client, SESSION).await;
        let CommitOutcome::Committed(_) = journal.commit_anchor_intent(intent("a1")).await else {
            panic!("intent not committed");
        };
        client
            .commit_closing(ClosingRecord {
                session_id: session(),
                operation: None,
            })
            .await
            .unwrap();
        client
            .commit_terminal_with(ended(1, 2, "cancelled"), close_cause())
            .await
            .unwrap();
        let outcome = client
            .commit_closed(ClosedRecord {
                session_id: session(),
                event: event("session.closed", 3),
                operation: None,
            })
            .await
            .unwrap();
        let ClosedOutcome::Closed(result) = outcome else {
            panic!("close refused");
        };
        assert_eq!(result["cleanup"], "uncertain");

        // Closed by the force closure pass instead: derived the same way,
        // on read (no stored result), `leftovers` present and null (C1 §3.6).
        spawn(&client, OTHER).await;
        let other = SessionId::try_from(OTHER).unwrap();
        client
            .commit_terminal(TerminalRecord {
                session_id: other.clone(),
                ..ended(1, 2, "cancelled")
            })
            .await
            .unwrap();
        assert!(
            client
                .commit_session_closed(&other, event("session.closed", 3))
                .await
                .unwrap()
        );
        assert_eq!(
            client.session_close_result(&other).await.unwrap(),
            Some(json!({
                "session_id": OTHER,
                "state": "closed",
                "cancelled_turns": [],
                "cleanup": "quiescent",
                "leftovers": null,
            }))
        );
    });
}

/// Design §7.2 row 2 and §7.3: `commit_submit_failed` takes a queued turn
/// `queued → running → failed` with `turn.submitted` and `turn.ended` in one
/// transaction; a turn that is not queued commits nothing.
#[test]
fn submit_failed_commits_submission_and_terminal_atomically() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client, SESSION).await;
        let record = || SubmitFailedRecord {
            session_id: session(),
            turn: turn(1),
            submitted: event("turn.submitted", 2),
            ended: event("turn.ended", 3),
            envelope: json!({"state":"failed","failure":{"class":"store"}}),
        };
        client.commit_submit_failed(record()).await.unwrap();
        let result = client
            .result_text(&session(), turn(1))
            .await
            .unwrap()
            .unwrap();
        let result: Value = serde_json::from_str(result.get()).unwrap();
        assert_eq!(result["failure"]["class"], "store");
        assert_eq!(client.next_seq(&session()).await.unwrap(), Some(4));
        assert!(client.commit_submit_failed(record()).await.is_err());
    });
    drop(store);
    let (state, submitted): (String, Option<String>) = read_db(&root)
        .query_row("SELECT state,submitted_at FROM turns", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(state, "failed");
    assert_eq!(submitted.as_deref(), Some("2026-01-01T00:00:00.000Z"));
}

/// Task 4 design §6.6: every terminal records the `seq` of its
/// `turn.ended` in `turns.ended_seq`: a plain terminal, one with extras,
/// a submit failure and each record of a failure-resolution batch.
#[test]
fn every_terminal_records_its_ended_seq() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client, SESSION).await;
        resume(&client, 2, 2).await.unwrap();
        client
            .commit_terminal_with(ended(2, 3, "cancelled"), close_cause())
            .await
            .unwrap();
        client
            .commit_submission(SubmissionRecord {
                session_id: session(),
                turn: turn(1),
                event: event("turn.submitted", 4),
            })
            .await
            .unwrap();
        resume(&client, 3, 5).await.unwrap();
        client
            .commit_failure_resolution(FailureResolutionRecord {
                terminal: ended(1, 6, "failed"),
                cancellations: vec![ended(3, 7, "cancelled")],
            })
            .await
            .unwrap();
        spawn(&client, OTHER).await;
        client
            .commit_submit_failed(SubmitFailedRecord {
                session_id: SessionId::try_from(OTHER).unwrap(),
                turn: turn(1),
                submitted: event("turn.submitted", 2),
                ended: event("turn.ended", 3),
                envelope: json!({"state":"failed","failure":{"class":"store"}}),
            })
            .await
            .unwrap();
        resume(&client, 4, 8).await.unwrap();
        client
            .commit_terminal(ended(4, 9, "cancelled"))
            .await
            .unwrap();
    });
    drop(store);
    let rows: Vec<(String, u32, Option<i64>)> = read_db(&root)
        .prepare("SELECT session_id,number,ended_seq FROM turns ORDER BY session_id,number")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        [
            (SESSION.to_owned(), 1, Some(6)),
            (SESSION.to_owned(), 2, Some(3)),
            (SESSION.to_owned(), 3, Some(7)),
            (SESSION.to_owned(), 4, Some(9)),
            (OTHER.to_owned(), 1, Some(3)),
        ]
    );
}

/// Design §7.4: the latch batch commits one terminal and the session's queued
/// cancellations in one transaction; more than 8 cancellations are refused
/// and nothing is written.
#[test]
fn failure_resolution_batch_is_one_bounded_transaction() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client, SESSION).await;
        client
            .commit_submission(SubmissionRecord {
                session_id: session(),
                turn: turn(1),
                event: event("turn.submitted", 2),
            })
            .await
            .unwrap();
        for number in 2..=8 {
            resume(&client, number, u64::from(number) + 1)
                .await
                .unwrap();
        }
        let batch = |count: u32| FailureResolutionRecord {
            terminal: ended(1, 10, "failed"),
            cancellations: (2..2 + count)
                .map(|number| ended(number, u64::from(number) + 9, "cancelled"))
                .collect(),
        };
        assert!(matches!(
            client.commit_failure_resolution(batch(9)).await,
            Err(StoreError::Constraint(_))
        ));
        assert_eq!(client.next_seq(&session()).await.unwrap(), Some(10));
        client.commit_failure_resolution(batch(7)).await.unwrap();
        for number in 1..=8 {
            assert!(
                client
                    .result_text(&session(), turn(number))
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(client.next_seq(&session()).await.unwrap(), Some(18));
    });
}

/// S1 round-1 decision 2: the batch is refused, writing nothing, when its
/// primary turn is not running. A queued turn given a `cancelled` terminal
/// is not a failure resolution.
#[test]
fn failure_resolution_refuses_a_primary_turn_that_is_not_running() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client, SESSION).await;
        resume(&client, 2, 2).await.unwrap();
        let refused = client
            .commit_failure_resolution(FailureResolutionRecord {
                terminal: ended(1, 3, "cancelled"),
                cancellations: vec![ended(2, 4, "cancelled")],
            })
            .await;
        assert!(
            matches!(
                refused,
                Err(StoreError::Refused(
                    "a failure batch resolves a running turn"
                ))
            ),
            "{refused:?}"
        );
        assert_eq!(client.next_seq(&session()).await.unwrap(), Some(3));
        assert!(
            client
                .result_text(&session(), turn(1))
                .await
                .unwrap()
                .is_none()
        );
    });
}

/// S1 round-1 decision 2: the batch is refused, writing nothing, unless its
/// cancellations are exactly the session's queued turns (design §7.4): a
/// batch that leaves a queued turn behind is refused.
#[test]
fn failure_resolution_refuses_a_partial_cancellation_set() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client, SESSION).await;
        client
            .commit_submission(SubmissionRecord {
                session_id: session(),
                turn: turn(1),
                event: event("turn.submitted", 2),
            })
            .await
            .unwrap();
        resume(&client, 2, 3).await.unwrap();
        resume(&client, 3, 4).await.unwrap();
        let refused = client
            .commit_failure_resolution(FailureResolutionRecord {
                terminal: ended(1, 5, "failed"),
                cancellations: vec![ended(2, 6, "cancelled")],
            })
            .await;
        assert!(
            matches!(
                refused,
                Err(StoreError::Refused(
                    "a failure batch cancels exactly its session's queued turns"
                ))
            ),
            "{refused:?}"
        );
        assert_eq!(client.next_seq(&session()).await.unwrap(), Some(5));
        for number in 1..=3 {
            assert!(
                client
                    .result_text(&session(), turn(number))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    });
}

fn intent(anchor_id: &str) -> AnchorIntent {
    AnchorIntent {
        anchor_id: anchor_id.to_owned(),
        generation: format!("g-{anchor_id}"),
        marker: "m".to_owned(),
        socket_path: PathBuf::from("/private/a.sock"),
        owner: via_store::ProcessOwner::Turn {
            session_id: session(),
            turn: turn(1),
        },
        uid: 1000,
        boot_id: "boot".to_owned(),
        pid_namespace: "pid:[1]".to_owned(),
    }
}

fn identity(pid: u32) -> AnchorIdentity {
    AnchorIdentity {
        pid,
        pgid: pid,
        uid: 1000,
        boot_id: "boot".to_owned(),
        pid_namespace: "pid:[1]".to_owned(),
        start_ticks: 42,
        marker: "m".to_owned(),
    }
}

fn absence(anchor_id: &str, identity: Option<AnchorIdentity>) -> GroupAbsenceRecord {
    GroupAbsenceRecord {
        anchor_id: anchor_id.to_owned(),
        generation: format!("g-{anchor_id}"),
        boot_id: "boot".to_owned(),
        pid_namespace: "pid:[1]".to_owned(),
        pgid: 4242,
        observed_at: "1".to_owned(),
        identity,
    }
}

async fn records(journal: &ProcessJournal, owner: Option<SessionId>) -> Vec<String> {
    journal
        .unproven_anchor_records_page(None, 10, owner)
        .await
        .unwrap()
        .into_iter()
        .map(|record| record.intent.anchor_id)
        .collect()
}

/// Design §7.2 row 4 [r3.12]: an anchor still at `intent` phase accepts an
/// absence proof that carries the full identity Host held in memory, so
/// restart can settle it; a proof whose identity disagrees with the intent
/// commits nothing. Unproven records page with an optional owner filter, and
/// anchor owners report their phase.
#[test]
fn absence_proof_records_the_identity_of_an_intent_phase_anchor() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let journal = store.runtime_resources().into_wire_parts().1;
    runtime().block_on(async {
        spawn(&client, SESSION).await;
        spawn(&client, OTHER).await;
        for id in ["a1", "a2"] {
            let CommitOutcome::Committed(_) = journal.commit_anchor_intent(intent(id)).await else {
                panic!("intent not committed");
            };
        }
        let CommitOutcome::Committed(_) = journal
            .commit_anchor_intent(AnchorIntent {
                owner: via_store::ProcessOwner::Turn {
                    session_id: SessionId::try_from(OTHER).unwrap(),
                    turn: turn(1),
                },
                ..intent("b1")
            })
            .await
        else {
            panic!("intent not committed");
        };
        let owners = client.anchor_owners_page(None, 10).await.unwrap();
        assert!(
            owners
                .iter()
                .all(|owner| owner.phase == Some(AnchorPhase::Intent))
        );
        assert_eq!(records(&journal, None).await, ["a1", "a2", "b1"]);
        assert_eq!(records(&journal, Some(session())).await, ["a1", "a2"]);

        // Without an identity an intent-phase anchor cannot be proved absent.
        assert!(matches!(
            journal.commit_group_absence(absence("a1", None)).await,
            CommitOutcome::NotCommitted(_)
        ));
        let mut wrong = identity(4242);
        wrong.marker = "other".to_owned();
        assert!(matches!(
            journal
                .commit_group_absence(absence("a1", Some(wrong)))
                .await,
            CommitOutcome::NotCommitted(_)
        ));
        assert!(matches!(
            journal
                .commit_group_absence(absence("a1", Some(identity(4242))))
                .await,
            CommitOutcome::Committed(())
        ));
        assert_eq!(records(&journal, Some(session())).await, ["a2"]);
        let all = journal.list_anchor_records_page(None, 10).await.unwrap();
        let proved = all
            .iter()
            .find(|record| record.intent.anchor_id == "a1")
            .unwrap();
        assert_eq!(proved.phase, AnchorPhase::Intent);
        assert_eq!(proved.identity, Some(identity(4242)));
        assert!(proved.absence.is_some());
    });
}
