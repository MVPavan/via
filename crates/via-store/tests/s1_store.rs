//! Task 3 S1 Store primitives (design §7.1, §10): schema v5, the close and
//! F12 operations, and the error split. Written before the operations.
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
    OperationVerb, ProcessJournal, ResumeRecord, SessionId, SpawnRecord, Store, StoreClient,
    StoreError, SubmissionRecord, SubmitFailedRecord, TerminalExtras, TerminalRecord, TurnNumber,
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
    json!({"type":kind,"seq":seq,"at":"2026-01-01T00:00:00.000Z","raw_ref":null})
}

async fn spawn(client: &StoreClient, id: &str) {
    client
        .commit_spawn(SpawnRecord {
            session_id: SessionId::try_from(id).unwrap(),
            handle_hash: [7_u8; 32],
            receipt: json!({"state":"queued"}),
            params: json!({"harness":"fake"}),
            prompt: "p".to_owned(),
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
            prompt: "p".to_owned(),
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
        raw_ref: None,
    }
}

fn close_cause() -> TerminalExtras {
    TerminalExtras {
        cancel_cause: Some(CancelCause::Close),
        raw_incomplete: None,
    }
}

fn read_db(root: &TempDir) -> rusqlite::Connection {
    rusqlite::Connection::open(root.path().join("store.sqlite3")).unwrap()
}

/// Design §10: a fresh Store is schema v5; a v4 Store is an unreleased
/// format refused with the recreate instruction, bytes untouched.
#[test]
fn fresh_store_is_v5_and_a_v4_store_is_refused() {
    let root = private_dir();
    drop(Store::open(root.path()).unwrap());
    let version: i64 = read_db(&root)
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 5);

    let old = private_dir();
    let db = old.path().join("store.sqlite3");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY)")
            .unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
    }
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&db).unwrap();
    let Err(error) = Store::open(old.path()) else {
        panic!("a v4 Store opened");
    };
    assert!(error.to_string().contains("schema v4"), "{error}");
    assert!(error.to_string().contains("recreate"), "{error}");
    assert_eq!(fs::read(&db).unwrap(), before);
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
            identity: b"close-params".to_vec(),
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
        assert_eq!(keyed.identity, b"close-params");
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

        // Closed by the force closure pass instead: derived the same way.
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
        let result = client.result(&session(), turn(1)).await.unwrap().unwrap();
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

/// Design §7.2 row 6: a terminal may carry one `raw_log.incomplete` event in
/// its own transaction, sequenced before `turn.ended`.
#[test]
fn terminal_carries_its_raw_incomplete_event() {
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
        client
            .commit_terminal_with(
                ended(1, 4, "failed"),
                TerminalExtras {
                    cancel_cause: None,
                    raw_incomplete: Some(event("raw_log.incomplete", 3)),
                },
            )
            .await
            .unwrap();
        let events = client.events(&session(), 1, 10).await.unwrap();
        let kinds: Vec<&str> = events
            .iter()
            .map(|stored| stored.event["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "turn.queued",
                "turn.submitted",
                "raw_log.incomplete",
                "turn.ended"
            ]
        );
    });
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
            raw_incomplete: None,
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
                    .result(&session(), turn(number))
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
                raw_incomplete: None,
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
        assert!(client.result(&session(), turn(1)).await.unwrap().is_none());
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
                raw_incomplete: None,
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
                    .result(&session(), turn(number))
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
        owner_session: session(),
        owner_turn: turn(1),
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
                owner_session: SessionId::try_from(OTHER).unwrap(),
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
