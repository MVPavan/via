//! K2 r1 #6, #7 (C1 §3.4, design §10): a keyed steer's writes take the
//! commit seams. A failure before `COMMIT` rolls back the whole
//! transaction: the `steer.delivered` event and the outcome it records
//! together, an intent row, a lone outcome. Each test is its own process
//! under nextest, so the process-wide controller is private to it.
#![cfg(feature = "test-failpoints")]
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    EventRecord, Identity, SessionEventRecord, SessionId, SpawnRecord, SteerIntent, SteerOutcome,
    Store, StoreClient, StoreError, SubmissionRecord, TurnNumber, failpoint,
};

const TOKEN: &str = "k2-steer-seams-token-0123";
const SESSION: &str = "s_7f3k9q2mzr4c";

fn session() -> SessionId {
    SessionId::try_from(SESSION).unwrap()
}

fn turn() -> TurnNumber {
    TurnNumber::try_from(1).unwrap()
}

fn private_dir() -> TempDir {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

/// A private Store, its client, and an active failpoint controller.
struct Seams {
    _state: TempDir,
    points: TempDir,
    _store: Store,
    client: StoreClient,
}

impl Seams {
    fn new() -> Self {
        let points = private_dir();
        failpoint::activate(points.path(), TOKEN).unwrap();
        let state = private_dir();
        let store = Store::open(state.path()).unwrap();
        let client = store.client();
        Self {
            _state: state,
            points,
            _store: store,
            client,
        }
    }

    /// Fails the next hit of `point`, its first.
    fn fail_first(&self, point: &str) {
        let command = json!({"token":TOKEN,"occurrence":1,"action":"fail_io"});
        fs::write(
            self.points.path().join(format!("{point}.json")),
            command.to_string(),
        )
        .unwrap();
    }

    fn acked(&self, point: &str) -> bool {
        self.points.path().join(format!("{point}.1.ack")).exists()
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn event(kind: &str, seq: u64) -> Value {
    json!({"type":kind,"seq":seq,"turn":1,"late":false,"at":"2026-01-01T00:00:00.000Z"})
}

/// Session `SESSION` with turn 1 running, at sequence 2.
async fn running(client: &StoreClient) {
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
    client
        .commit_submission(SubmissionRecord {
            session_id: session(),
            turn: turn(),
            event: event("turn.submitted", 2),
        })
        .await
        .unwrap();
}

/// [`running`], with the open steer intent `k-1`.
async fn running_with_intent(client: &StoreClient) {
    running(client).await;
    client.commit_steer_intent(intent()).await.unwrap();
}

fn intent() -> SteerIntent {
    SteerIntent {
        session_id: session(),
        op_key: "k-1".to_owned(),
        identity: Identity::of(b"one"),
    }
}

fn delivered() -> SteerOutcome {
    SteerOutcome {
        op_key: "k-1".to_owned(),
        result: json!({"turn":format!("{SESSION}/1"),"delivery":"injected"}),
    }
}

/// The result of the row under `k-1`, if the row exists.
async fn result(client: &StoreClient) -> Option<Option<Value>> {
    client
        .keyed_operation(&session(), "k-1")
        .await
        .unwrap()
        .map(|row| row.result)
}

/// A turn's `steer.delivered` and the outcome it records roll back
/// together, after both were written, at the event's commit seam.
#[test]
fn a_steer_event_and_its_outcome_roll_back_together() {
    let seams = Seams::new();
    runtime().block_on(async {
        let client = &seams.client;
        running_with_intent(client).await;
        seams.fail_first("store.commit.event");
        let failed = client
            .commit_event(EventRecord {
                session_id: session(),
                turn: turn(),
                event: event("steer.delivered", 3),
                steer: Some(delivered()),
            })
            .await;
        assert!(matches!(failed, Err(StoreError::Write(_))), "{failed:?}");
        assert!(seams.acked("store.commit.event"));
        assert_eq!(result(client).await, Some(None), "the outcome rolled back");
        assert_eq!(client.next_seq(&session()).await.unwrap(), Some(3));
    });
}

/// A session-level `steer.delivered` and its outcome roll back together.
#[test]
fn a_session_steer_event_and_its_outcome_roll_back_together() {
    let seams = Seams::new();
    runtime().block_on(async {
        let client = &seams.client;
        running_with_intent(client).await;
        seams.fail_first("store.commit.session_event");
        let failed = client
            .commit_session_event(SessionEventRecord {
                session_id: session(),
                event: Some(event("steer.delivered", 3)),
                identity: None,
                steer: Some(delivered()),
            })
            .await;
        assert!(matches!(failed, Err(StoreError::Write(_))), "{failed:?}");
        assert!(seams.acked("store.commit.session_event"));
        assert_eq!(result(client).await, Some(None), "the outcome rolled back");
        assert_eq!(client.next_seq(&session()).await.unwrap(), Some(3));
    });
}

/// An intent's own transaction takes its seam: failed, no row exists, and
/// the key is free for the next intent.
#[test]
fn a_steer_intent_rolls_back_at_its_seam() {
    let seams = Seams::new();
    runtime().block_on(async {
        let client = &seams.client;
        running(client).await;
        seams.fail_first("store.commit.steer_intent");
        let failed = client.commit_steer_intent(intent()).await;
        assert!(matches!(failed, Err(StoreError::Write(_))), "{failed:?}");
        assert!(seams.acked("store.commit.steer_intent"));
        assert_eq!(result(client).await, None, "the intent rolled back");
        client.commit_steer_intent(intent()).await.unwrap();
        assert_eq!(result(client).await, Some(None));
    });
}

/// A lone outcome's transaction takes its seam: failed, the intent stays
/// open, and a later outcome records.
#[test]
fn a_lone_steer_outcome_rolls_back_at_its_seam() {
    let seams = Seams::new();
    runtime().block_on(async {
        let client = &seams.client;
        running_with_intent(client).await;
        seams.fail_first("store.commit.steer_outcome");
        let refusal = || SteerOutcome {
            op_key: "k-1".to_owned(),
            result: json!({"refused":"no_active_turn"}),
        };
        let failed = client.commit_steer_outcome(&session(), refusal()).await;
        assert!(matches!(failed, Err(StoreError::Write(_))), "{failed:?}");
        assert!(seams.acked("store.commit.steer_outcome"));
        assert_eq!(result(client).await, Some(None), "the outcome rolled back");
        client
            .commit_steer_outcome(&session(), refusal())
            .await
            .unwrap();
        assert_eq!(
            result(client).await,
            Some(Some(json!({"refused":"no_active_turn"})))
        );
    });
}
