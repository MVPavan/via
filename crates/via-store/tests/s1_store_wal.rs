//! Task 4 design §5.4 (T4-7 review round 1): the WAL limit holds across a
//! Store reopen. Written before the open-time check.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::json;
use tempfile::TempDir;
use via_store::{SessionId, SpawnRecord, Store, StoreClient, StoreError, StoreLock, WalLimits};

const MIB: u64 = 1024 * 1024;

fn limits() -> WalLimits {
    WalLimits {
        max: 4 * MIB,
        checkpoint_bytes: 64 * 1024,
        checkpoint_commits: 1000,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn session(n: usize) -> SessionId {
    SessionId::try_from(format!("s_7f3k9q2m{n:04}").as_str()).unwrap()
}

async fn spawn(client: &StoreClient, n: usize) -> Result<(), StoreError> {
    client
        .commit_spawn(SpawnRecord {
            session_id: session(n),
            handle_hash: [7_u8; 32],
            receipt: json!({"state":"queued"}),
            // Grows the WAL by about 200 KiB a receipt.
            params: json!({"harness":"fake","pad":"x".repeat(200 * 1024)}),
            label: None,
            prompt: "p".into(),
            effective: json!({"deadlines":{"wall_ms":1}}),
            initial_event: json!({"type":"turn.queued","seq":1,"at":"2026-01-01T00:00:00.000Z"}),
        })
        .await
        .map(|_| ())
}

fn wal_len(root: &TempDir) -> u64 {
    fs::metadata(root.path().join("store.sqlite3-wal")).map_or(0, |metadata| metadata.len())
}

/// A WAL at `wal.max` that a reader holds across a reopen: the reopened
/// Store tries one `TRUNCATE` at open and, still at the limit, refuses the
/// first new receipt with `WalFull`; once the reader is gone a later write
/// retries `TRUNCATE` and admits new work.
#[test]
fn s1_store_wal_limit_is_checked_when_the_store_opens() {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open_with_limits(
        root.path(),
        StoreLock::acquire(root.path()).unwrap(),
        limits(),
    )
    .unwrap();
    let client = store.client();
    runtime().block_on(spawn(&client, 0)).unwrap();
    let reader = rusqlite::Connection::open_with_flags(
        root.path().join("store.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    let mut n = 1;
    while !client.wal_full() {
        runtime().block_on(spawn(&client, n)).unwrap();
        n += 1;
        assert!(n < 100, "the WAL never reached wal.max");
    }
    drop(client);
    drop(store);
    assert!(wal_len(&root) >= 4 * MIB, "WAL {}", wal_len(&root));

    let store = Store::open_with_limits(
        root.path(),
        StoreLock::acquire(root.path()).unwrap(),
        limits(),
    )
    .unwrap();
    let client = store.client();
    let first = runtime().block_on(spawn(&client, n));
    assert!(
        matches!(first, Err(StoreError::WalFull)),
        "first receipt after the reopen: {first:?}"
    );
    assert!(client.wal_full());
    drop(reader);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    // A write at least 1 s after the last attempt retries TRUNCATE first.
    runtime().block_on(spawn(&client, n + 1)).unwrap();
    assert!(!client.wal_full());
    assert!(wal_len(&root) < 4 * MIB, "WAL {}", wal_len(&root));
}
