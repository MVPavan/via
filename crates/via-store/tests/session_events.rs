//! S-CORE chunk 4 fix round 2 (decision H3 as narrowed): the two guarded
//! session-level writes. A durable session observation commits at the
//! session's dense next sequence whether or not a turn runs, keeping its
//! own `turn` and `late`; a confirmed identity's open event writes the
//! session's identity columns in the same transaction; a closed session
//! refuses both.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    CancelCause, ClosedOutcome, ClosedRecord, ClosingRecord, SessionEventRecord, SessionId,
    SessionIdentity, SpawnRecord, Store, StoreClient, StoreError, TerminalExtras, TerminalRecord,
    TurnNumber,
};

const SESSION: &str = "s_7f3k9q2mzr4c";

fn session() -> SessionId {
    SessionId::try_from(SESSION).unwrap()
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

fn event(kind: &str, seq: u64, turn: Option<u32>, late: bool) -> Value {
    json!({"type":kind,"seq":seq,"turn":turn,"late":late,"at":"2026-01-01T00:00:00.000Z"})
}

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
            initial_event: event("turn.queued", 1, Some(1), false),
        })
        .await
        .unwrap();
}

fn observation(seq: u64, turn: Option<u32>, late: bool) -> SessionEventRecord {
    SessionEventRecord {
        session_id: session(),
        event: Some(event("action.denied", seq, turn, late)),
        identity: None,
    }
}

#[test]
fn session_events_commit_outside_a_running_turn_until_the_session_closes() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client).await;
        // Turn 1 is queued, not running: a session-level and a late item
        // commit at the dense next sequence with their own attribution.
        client
            .commit_session_event(observation(2, None, false))
            .await
            .unwrap();
        client
            .commit_session_event(observation(3, Some(1), true))
            .await
            .unwrap();
        let skipped = client
            .commit_session_event(observation(5, None, false))
            .await;
        assert!(
            matches!(skipped, Err(StoreError::Constraint(_))),
            "{skipped:?}"
        );
        let events = client.events(&session(), 1, 10).await.unwrap();
        let kept: Vec<(u64, Value, Value)> = events
            .iter()
            .map(|event| {
                (
                    event.seq,
                    event.event["turn"].clone(),
                    event.event["late"].clone(),
                )
            })
            .collect();
        assert_eq!(
            kept,
            [
                (1, json!(1), json!(false)),
                (2, Value::Null, json!(false)),
                (3, json!(1), json!(true))
            ]
        );

        // An observation carries no identity; an identity commit needs one.
        let mut wrong = observation(4, None, false);
        wrong.identity = Some(SessionIdentity {
            vendor_session_id: "v".to_owned(),
            transcript: None,
        });
        assert!(client.commit_session_event(wrong).await.is_err());
        assert!(
            client
                .commit_identity(observation(4, None, false))
                .await
                .is_err()
        );

        // A closed session refuses it.
        client
            .commit_closing(ClosingRecord {
                session_id: session(),
                operation: None,
            })
            .await
            .unwrap();
        client
            .commit_terminal_with(
                TerminalRecord {
                    session_id: session(),
                    turn: TurnNumber::try_from(1).unwrap(),
                    envelope: json!({"state":"cancelled"}),
                    event: event("turn.ended", 4, Some(1), false),
                    steps: Vec::new(),
                    link_released: false,
                },
                TerminalExtras {
                    cancel_cause: Some(CancelCause::Close),
                },
            )
            .await
            .unwrap();
        let closed = client
            .commit_closed(ClosedRecord {
                session_id: session(),
                event: event("session.closed", 5, None, false),
                operation: None,
            })
            .await
            .unwrap();
        assert!(matches!(closed, ClosedOutcome::Closed(_)));
        let refused = client
            .commit_session_event(observation(6, None, false))
            .await;
        assert!(
            matches!(refused, Err(StoreError::Refused(_))),
            "{refused:?}"
        );
    });
}

#[test]
fn an_identity_commit_writes_the_session_columns_with_its_event() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client).await;
        let opened = |seq, transcript: Option<&str>| SessionEventRecord {
            session_id: session(),
            event: Some(
                json!({"type":"session.opened","seq":seq,"turn":null,"late":false,
                          "at":"2026-01-01T00:00:00.000Z","route":"fake",
                          "vendor_session_id":"v1","vendor_version":null}),
            ),
            identity: Some(SessionIdentity {
                vendor_session_id: "v1".to_owned(),
                transcript: transcript.map(str::to_owned),
            }),
        };
        client
            .commit_identity(opened(2, Some("/t/v1.jsonl")))
            .await
            .unwrap();
        let snapshot = client.session_snapshot(&session()).await.unwrap().unwrap();
        assert_eq!(snapshot.route.vendor_session_id.as_deref(), Some("v1"));
        assert_eq!(snapshot.route.transcript.as_deref(), Some("/t/v1.jsonl"));
        let logs = client
            .evidence_refs(&session(), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(logs.vendor_session_id.as_deref(), Some("v1"));
        assert_eq!(logs.transcript_hint.as_deref(), Some("/t/v1.jsonl"));
        // A failed event writes no column either: a stale sequence.
        let stale = SessionEventRecord {
            identity: Some(SessionIdentity {
                vendor_session_id: "v2".to_owned(),
                transcript: None,
            }),
            ..opened(2, None)
        };
        assert!(client.commit_identity(stale).await.is_err());
        let snapshot = client.session_snapshot(&session()).await.unwrap().unwrap();
        assert_eq!(snapshot.route.vendor_session_id.as_deref(), Some("v1"));
    });
}

/// Sol r2 #4: a repeated confirmation of the same connection generation
/// writes the session's identity columns without another open event; a
/// session observation always has its event; a closed session refuses it.
#[test]
fn a_repeated_confirmation_writes_the_columns_without_an_event() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        spawn(&client).await;
        let columns = |transcript: &str| SessionEventRecord {
            session_id: session(),
            event: None,
            identity: Some(SessionIdentity {
                vendor_session_id: "v1".to_owned(),
                transcript: Some(transcript.to_owned()),
            }),
        };
        client.commit_identity(columns("/t/a.jsonl")).await.unwrap();
        client.commit_identity(columns("/t/b.jsonl")).await.unwrap();
        let snapshot = client.session_snapshot(&session()).await.unwrap().unwrap();
        assert_eq!(snapshot.route.vendor_session_id.as_deref(), Some("v1"));
        assert_eq!(snapshot.route.transcript.as_deref(), Some("/t/b.jsonl"));
        // No event was written: the next one is still sequence 2.
        assert_eq!(client.events(&session(), 1, 10).await.unwrap().len(), 1);
        let mut eventless = observation(2, None, false);
        eventless.event = None;
        assert!(client.commit_session_event(eventless).await.is_err());
        client
            .commit_session_event(observation(2, None, false))
            .await
            .unwrap();
        client
            .commit_closing(ClosingRecord {
                session_id: session(),
                operation: None,
            })
            .await
            .unwrap();
        client
            .commit_terminal_with(
                TerminalRecord {
                    session_id: session(),
                    turn: TurnNumber::try_from(1).unwrap(),
                    envelope: json!({"state":"cancelled"}),
                    event: event("turn.ended", 3, Some(1), false),
                    steps: Vec::new(),
                    link_released: false,
                },
                TerminalExtras {
                    cancel_cause: Some(CancelCause::Close),
                },
            )
            .await
            .unwrap();
        let closed = client
            .commit_closed(ClosedRecord {
                session_id: session(),
                event: event("session.closed", 4, None, false),
                operation: None,
            })
            .await
            .unwrap();
        assert!(matches!(closed, ClosedOutcome::Closed(_)));
        let refused = client.commit_identity(columns("/t/c.jsonl")).await;
        assert!(
            matches!(refused, Err(StoreError::Refused(_))),
            "{refused:?}"
        );
        let snapshot = client.session_snapshot(&session()).await.unwrap().unwrap();
        assert_eq!(snapshot.route.transcript.as_deref(), Some("/t/b.jsonl"));
    });
}
