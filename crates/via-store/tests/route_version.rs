//! Decision H3, Sol r2 #6: a session's recorded adapter version is the
//! latest *started* turn's, which records it in its `turn.started`
//! effective values, else the receipt's. A turn cancelled before it was
//! ever submitted changes nothing.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    AcceptanceRecord, CancelCause, ResumeRecord, SessionId, SpawnRecord, Store, StoreClient,
    SubmissionRecord, TerminalExtras, TerminalRecord, TurnNumber,
};

const SESSION: &str = "s_7f3k9q2mzr4c";
const AT: &str = "2026-01-01T00:00:00.000Z";

fn session() -> SessionId {
    SessionId::try_from(SESSION).unwrap()
}

fn turn(number: u32) -> TurnNumber {
    TurnNumber::try_from(number).unwrap()
}

fn event(kind: &str, seq: u64, number: u32) -> Value {
    json!({"type":kind,"seq":seq,"turn":number,"late":false,"at":AT})
}

fn effective() -> Value {
    json!({"model":"fake","effort":null,"bound":null,
           "deadlines":{"wall_ms":1,"idle_ms":1},"max_steps":null})
}

async fn version(client: &StoreClient) -> Option<String> {
    client
        .session_snapshot(&session())
        .await
        .unwrap()
        .unwrap()
        .route
        .adapter_version
}

async fn end(client: &StoreClient, number: u32, seq: u64, state: &str) {
    client
        .commit_terminal_with(
            TerminalRecord {
                session_id: session(),
                turn: turn(number),
                envelope: json!({"state":state}),
                event: event("turn.ended", seq, number),
                steps: Vec::new(),
            },
            TerminalExtras {
                cancel_cause: (state == "cancelled").then_some(CancelCause::Cancel),
            },
        )
        .await
        .unwrap();
}

#[test]
fn the_recorded_adapter_version_is_the_latest_started_turns() {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            client
                .commit_spawn(SpawnRecord {
                    session_id: session(),
                    handle_hash: [7_u8; 32],
                    receipt: json!({"state":"queued","route":"fake","adapter_version":"1"}),
                    params: json!({"harness":"fake"}),
                    label: None,
                    prompt: "p".into(),
                    effective: effective(),
                    initial_event: event("turn.queued", 1, 1),
                })
                .await
                .unwrap();
            // No turn started: the receipt's.
            assert_eq!(version(&client).await.as_deref(), Some("1"));
            client
                .commit_submission(SubmissionRecord {
                    session_id: session(),
                    turn: turn(1),
                    event: event("turn.submitted", 2, 1),
                })
                .await
                .unwrap();
            assert_eq!(version(&client).await.as_deref(), Some("1"));
            // Turn 1 starts on adapter version 2.
            let mut started = event("turn.started", 3, 1);
            let mut values = effective();
            values["adapter_version"] = json!("2");
            started["effective"] = values;
            client
                .commit_acceptance(AcceptanceRecord {
                    session_id: session(),
                    turn: turn(1),
                    correlation: "fake-turn-1".to_owned(),
                    event: started,
                })
                .await
                .unwrap();
            assert_eq!(version(&client).await.as_deref(), Some("2"));
            end(&client, 1, 4, "completed").await;
            // Turn 2 is cancelled before it was ever submitted.
            client
                .commit_resume(ResumeRecord {
                    session_id: session(),
                    turn: turn(2),
                    prompt: "q".into(),
                    effective: effective(),
                    event: event("turn.queued", 5, 2),
                    operation: None,
                })
                .await
                .unwrap();
            end(&client, 2, 6, "cancelled").await;
            assert_eq!(version(&client).await.as_deref(), Some("2"));
        });
}
