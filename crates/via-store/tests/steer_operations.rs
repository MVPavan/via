//! K2 (via-jm4.36, C1 §3, §3.4, runtime §6): keyed `steer` operation rows.
//! A keyed steer's intent row has no result until its outcome is recorded,
//! once: with the `steer.delivered` event that reports its delivery, in
//! that event's transaction, or alone for a refusal. Recovery gives every
//! intent left without a result one stored outcome. Written before the
//! operations.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    CloseIntent, ClosingRecord, EventRecord, Identity, KeyedOperation, OperationVerb,
    SessionEventRecord, SessionId, SpawnRecord, SteerIntent, SteerOutcome, Store, StoreClient,
    StoreError, SubmissionRecord, TurnNumber,
};

const SESSION: &str = "s_7f3k9q2mzr4c";

fn session() -> SessionId {
    SessionId::try_from(SESSION).unwrap()
}

fn turn() -> TurnNumber {
    TurnNumber::try_from(1).unwrap()
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
    json!({"type":kind,"seq":seq,"turn":1,"late":false,"at":"2026-01-01T00:00:00.000Z"})
}

/// Session `SESSION` with turn 1 queued, at sequence 1.
async fn spawn(client: &StoreClient) {
    client
        .commit_spawn(SpawnRecord {
            session_id: session(),
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

/// Turn 1 running, at sequence 2.
async fn submit(client: &StoreClient) {
    client
        .commit_submission(SubmissionRecord {
            session_id: session(),
            turn: turn(),
            event: event("turn.submitted", 2),
        })
        .await
        .unwrap();
}

fn intent(key: &str, identity: &[u8]) -> SteerIntent {
    SteerIntent {
        session_id: session(),
        op_key: key.to_owned(),
        identity: Identity::of(identity),
    }
}

fn outcome(key: &str, result: &Value) -> SteerOutcome {
    SteerOutcome {
        op_key: key.to_owned(),
        result: result.clone(),
    }
}

/// The row under `key`: its verb, identity and result.
async fn row(client: &StoreClient, key: &str) -> Option<(OperationVerb, Identity, Option<Value>)> {
    client.keyed_operation(&session(), key).await.unwrap().map(
        |KeyedOperation {
             verb,
             identity,
             result,
         }| (verb, identity, result),
    )
}

/// An intent row reads as a steer with no result. Its outcome is recorded
/// once: a second outcome keeps and returns the first. A key already held
/// refuses a second intent, and an outcome needs an intent.
#[test]
fn a_steer_intent_takes_one_outcome() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client).await;
        client
            .commit_steer_intent(intent("k-1", b"one"))
            .await
            .unwrap();
        assert_eq!(
            row(&client, "k-1").await,
            Some((OperationVerb::Steer, Identity::of(b"one"), None))
        );
        let first = json!({"refused":"no_active_turn"});
        let stored = client
            .commit_steer_outcome(&session(), outcome("k-1", &first))
            .await
            .unwrap();
        assert_eq!(stored, first);
        let stored = client
            .commit_steer_outcome(&session(), outcome("k-1", &json!({"other":true})))
            .await
            .unwrap();
        assert_eq!(stored, first, "the first outcome stays");
        assert_eq!(
            row(&client, "k-1").await,
            Some((OperationVerb::Steer, Identity::of(b"one"), Some(first)))
        );
        let again = client.commit_steer_intent(intent("k-1", b"one")).await;
        assert!(matches!(again, Err(StoreError::Constraint(_))), "{again:?}");
        let missing = client
            .commit_steer_outcome(&session(), outcome("k-2", &json!({})))
            .await;
        assert!(
            matches!(missing, Err(StoreError::Constraint(_))),
            "{missing:?}"
        );
    });
}

/// The `steer.delivered` event that reports a keyed steer's delivery
/// records its outcome in the event's own transaction, during a turn and
/// between turns. An event that is not committed records nothing.
#[test]
fn a_steer_event_records_its_outcome_in_its_transaction() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client).await;
        client
            .commit_steer_intent(intent("k-1", b"one"))
            .await
            .unwrap();
        client
            .commit_steer_intent(intent("k-2", b"two"))
            .await
            .unwrap();
        client
            .commit_steer_intent(intent("k-3", b"three"))
            .await
            .unwrap();
        let delivered = json!({"turn":format!("{SESSION}/1"),"delivery":"injected"});

        // Turn 1 is queued, not running: a turn event is refused, and its
        // outcome is not recorded.
        let refused = client
            .commit_event(EventRecord {
                session_id: session(),
                turn: turn(),
                event: event("steer.delivered", 2),
                steer: Some(outcome("k-1", &delivered)),
            })
            .await;
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(row(&client, "k-1").await.unwrap().2, None);

        submit(&client).await;
        client
            .commit_event(EventRecord {
                session_id: session(),
                turn: turn(),
                event: event("steer.delivered", 3),
                steer: Some(outcome("k-1", &delivered)),
            })
            .await
            .unwrap();
        assert_eq!(
            row(&client, "k-1").await.unwrap().2,
            Some(delivered.clone())
        );

        client
            .commit_session_event(SessionEventRecord {
                session_id: session(),
                event: Some(event("steer.delivered", 4)),
                identity: None,
                steer: Some(outcome("k-2", &delivered)),
            })
            .await
            .unwrap();
        assert_eq!(row(&client, "k-2").await.unwrap().2, Some(delivered));

        // Out of sequence: rolled back, so no outcome either.
        let skipped = client
            .commit_session_event(SessionEventRecord {
                session_id: session(),
                event: Some(event("steer.delivered", 9)),
                identity: None,
                steer: Some(outcome("k-3", &json!({"x":1}))),
            })
            .await;
        assert!(skipped.is_err(), "{skipped:?}");
        assert_eq!(row(&client, "k-3").await.unwrap().2, None);
    });
}

/// Restart (C1 §3.4): every keyed steer intent left without a result gets
/// the one stored outcome recovery gives it; a recorded outcome and a
/// close's intent row are left as they are.
#[test]
fn recovery_resolves_only_open_steer_intents() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client).await;
        client
            .commit_steer_intent(intent("open", b"one"))
            .await
            .unwrap();
        client
            .commit_steer_intent(intent("done", b"two"))
            .await
            .unwrap();
        let done = json!({"turn":format!("{SESSION}/1"),"delivery":"injected"});
        client
            .commit_steer_outcome(&session(), outcome("done", &done))
            .await
            .unwrap();
        client
            .commit_closing(ClosingRecord {
                session_id: session(),
                operation: Some(CloseIntent {
                    op_key: "close".to_owned(),
                    identity: Identity::of(b"close"),
                }),
            })
            .await
            .unwrap();
        let uncertain = json!({"refused":"steer_failed","reason":"not_delivered",
                               "delivery":"uncertain"});
        assert_eq!(
            client
                .resolve_steer_intents(uncertain.clone())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            row(&client, "open").await.unwrap().2,
            Some(uncertain.clone())
        );
        assert_eq!(row(&client, "done").await.unwrap().2, Some(done));
        assert_eq!(
            row(&client, "close").await,
            Some((OperationVerb::Close, Identity::of(b"close"), None))
        );
        assert_eq!(client.resolve_steer_intents(uncertain).await.unwrap(), 0);
    });
}
