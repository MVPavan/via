//! Task 4 design §3 and §6.7 at the Store level: step rows, their commit on
//! the Internal lane and in the terminal transaction, the keyed range read
//! and delete, and `session_status`, one read that returns the durable
//! `status` members and a page of the selected turn's rows. Written before
//! the steps write path and the read.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    ResumeRecord, SessionId, SpawnRecord, StepRow, StepsRecord, Store, StoreClient, StoreError,
    SubmissionRecord, TerminalRecord, TurnNumber,
};

const SESSION: &str = "s_7f3k9q2mzr4c";
const OTHER: &str = "s_7f3k9q2mzr4d";

fn id(session: &str) -> SessionId {
    SessionId::try_from(session).unwrap()
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

fn event(kind: &str, seq: u64, turn: u32) -> Value {
    json!({"type":kind,"seq":seq,"turn":turn,"at":"2026-01-01T00:00:00.000Z"})
}

fn row(step: u32) -> StepRow {
    StepRow {
        step,
        started_ms: 1_000 * i64::from(step),
        ended_ms: 1_000 * i64::from(step) + 500,
        tokens: Some(u64::from(step) * 10),
    }
}

/// Spawns `session` with turn 1 submitted (running).
async fn running(client: &StoreClient, session: &str) {
    client
        .commit_spawn(SpawnRecord {
            session_id: id(session),
            handle_hash: [7_u8; 32],
            receipt: json!({"state":"queued","route":"fake"}),
            params: json!({"harness":"fake","model":"fake-model"}),
            label: None,
            prompt: "p".into(),
            effective: json!({"deadlines":{"wall_ms":1}}),
            initial_event: event("turn.queued", 1, 1),
        })
        .await
        .unwrap();
    client
        .commit_submission(SubmissionRecord {
            session_id: id(session),
            turn: turn(1),
            event: event("turn.submitted", 2, 1),
        })
        .await
        .unwrap();
}

async fn steps(client: &StoreClient, session: &str, rows: Vec<StepRow>) -> Result<(), StoreError> {
    client
        .commit_steps(StepsRecord {
            session_id: id(session),
            turn: turn(1),
            rows,
        })
        .await
}

fn read_db(root: &TempDir) -> rusqlite::Connection {
    rusqlite::Connection::open(root.path().join("store.sqlite3")).unwrap()
}

/// Design §3.4, §13.2: the page read and the retirement delete are keyed
/// range scans of the primary key; of two interleaved sessions one is
/// deleted, the other intact.
#[test]
fn s1_store_steps_delete_is_one_keyed_range() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        running(&client, SESSION).await;
        running(&client, OTHER).await;
        for step in 1..=3 {
            steps(&client, SESSION, vec![row(step)]).await.unwrap();
            steps(&client, OTHER, vec![row(step)]).await.unwrap();
        }
    });
    drop(store);
    let db = read_db(&root);
    let plan = |sql: &str| -> String {
        db.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([SESSION], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("; ")
    };
    let page = plan(
        "SELECT step,started_ms,ended_ms,tokens FROM steps \
         WHERE session_id=?1 AND turn=1 AND step>0 ORDER BY step LIMIT 100",
    );
    assert!(page.contains("USING PRIMARY KEY"), "page plan: {page}");
    assert!(!page.contains("TEMP B-TREE"), "page plan sorts: {page}");
    let delete = plan("DELETE FROM steps WHERE session_id=?1");
    assert!(
        delete.contains("USING PRIMARY KEY"),
        "delete plan: {delete}"
    );
    let deleted = db
        .execute("DELETE FROM steps WHERE session_id=?1", [SESSION])
        .unwrap();
    assert_eq!(deleted, 3);
    let left: Vec<(String, u32)> = db
        .prepare("SELECT session_id,step FROM steps ORDER BY session_id,step")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        left,
        [
            (OTHER.to_owned(), 1),
            (OTHER.to_owned(), 2),
            (OTHER.to_owned(), 3)
        ]
    );
}

/// Design §3.2: a row commits only for a running turn; the terminal
/// transaction carries the rows it is given with `turn.ended`.
#[test]
fn step_rows_commit_while_running_and_ride_in_the_terminal() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        running(&client, SESSION).await;
        steps(&client, SESSION, vec![row(1)]).await.unwrap();
        client
            .commit_terminal(TerminalRecord {
                session_id: id(SESSION),
                turn: turn(1),
                envelope: json!({"state":"failed"}),
                event: event("turn.ended", 3, 1),
                steps: vec![row(2), row(3)],
            })
            .await
            .unwrap();
        // The turn is terminal: no more rows.
        let late = steps(&client, SESSION, vec![row(4)]).await;
        assert!(matches!(late, Err(StoreError::Constraint(_))), "{late:?}");
    });
    drop(store);
    let rows: Vec<(u32, i64, i64, Option<i64>)> = read_db(&root)
        .prepare("SELECT step,started_ms,ended_ms,tokens FROM steps ORDER BY step")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        [
            (1, 1_000, 1_500, Some(10)),
            (2, 2_000, 2_500, Some(20)),
            (3, 3_000, 3_500, Some(30))
        ]
    );
}

/// Design §4.2, §11.3: one read selects the turn (the param, else the
/// running turn, else the latest) and returns the durable members and a
/// page of that turn's rows after `after_step`, at most `limit`.
#[test]
fn session_status_selects_a_turn_and_pages_its_rows() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client().public();
    let internal = store.client();
    runtime().block_on(async {
        assert!(
            client
                .session_status(&id(OTHER), None, 0, 100)
                .await
                .unwrap()
                .is_none()
        );
        running(&internal, SESSION).await;
        steps(&internal, SESSION, (1..=5).map(row).collect())
            .await
            .unwrap();
        internal
            .commit_resume(ResumeRecord {
                session_id: id(SESSION),
                turn: turn(2),
                prompt: "p".into(),
                effective: json!({"deadlines":{"wall_ms":2}}),
                event: event("turn.queued", 3, 2),
                operation: None,
            })
            .await
            .unwrap();
        let status = client
            .session_status(&id(SESSION), None, 1, 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.selected, Some((1, "running".to_owned())));
        assert_eq!(status.state, "active");
        assert_eq!(status.admission, "open");
        assert_eq!(status.harness, "fake");
        assert_eq!(status.model.as_deref(), Some("fake-model"));
        assert_eq!(status.route.as_deref(), Some("fake"));
        assert_eq!(status.cwd, None);
        assert_eq!(status.label, None);
        assert_eq!(status.vendor_session_id, None);
        assert!(!status.cleanup_uncertain);
        let active = status.active.as_ref().unwrap();
        assert_eq!(active.turn, 1);
        assert!(!active.accepted);
        assert_eq!(active.last_event_seq, 2);
        assert_eq!(active.cancel_requested_at, None);
        assert_eq!(status.queue.len(), 1);
        assert_eq!(status.queue[0].turn, 2);
        assert_eq!(
            status.queue[0].effective,
            json!({"deadlines":{"wall_ms":2}})
        );
        assert_eq!(
            status.turns,
            [(2, "queued".to_owned()), (1, "running".to_owned())]
        );
        assert_eq!(status.steps, [row(2), row(3)]);
        assert!(status.more);
        let second = client
            .session_status(&id(SESSION), Some(turn(2)), 0, 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.selected, Some((2, "queued".to_owned())));
        assert!(second.steps.is_empty() && !second.more);
        let missing = client
            .session_status(&id(SESSION), Some(turn(3)), 0, 100)
            .await;
        // A turn the session never had selects nothing.
        assert!(matches!(missing, Ok(Some(ref status)) if status.selected.is_none()));
    });
}

/// Design §4.2, §13.1: `session_status` is exactly one Store read.
#[cfg(feature = "test-failpoints")]
#[test]
fn session_status_is_one_read() {
    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        running(&client, SESSION).await;
        let before = store.read_count();
        client
            .public()
            .session_status(&id(SESSION), None, 0, 1000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(store.read_count() - before, 1);
    });
}
