//! Task 4 design §6.4, §6.5, §13.2: a retry identity is stored as its length
//! and SHA-256 only, and a retry matches when both are equal. Written before
//! the v6 identity columns.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use via_store::{
    Identity, OperationRecord, ResumeRecord, SessionId, SpawnKey, SpawnRecord, Store, TurnNumber,
};

const SESSION: &str = "s_7f3k9q2mzr4c";

fn private_dir() -> TempDir {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn event(kind: &str, seq: u64, turn: u32) -> Value {
    json!({"type":kind,"seq":seq,"turn":turn,"at":"2026-01-01T00:00:00.000Z"})
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Design §13.2 (`s1_store_…` last row): identity compare by length and
/// SHA-256. Equal bytes match; one changed byte or one more byte does not;
/// Store keeps the pair, never the bytes, and refuses a digest that is not
/// 32 bytes.
#[test]
fn s1_store_identity_compares_length_and_sha256() {
    let spawn_bytes = br#"{"harness":"fake","handle":"00ff","prompt":"p"}"#;
    let resume_bytes = br#"{"session":"s_7f3k9q2mzr4c","op_key":"k","prompt":"q"}"#;
    let identity = Identity::of(spawn_bytes);
    assert_eq!(identity, Identity::of(spawn_bytes));
    let mut changed = spawn_bytes.to_vec();
    changed[2] ^= 1;
    assert_ne!(identity, Identity::of(&changed));
    let mut longer = spawn_bytes.to_vec();
    longer.push(b' ');
    assert_ne!(identity, Identity::of(&longer));
    assert_eq!(identity.len, spawn_bytes.len() as u64);
    assert_eq!(
        identity.sha256,
        <[u8; 32]>::from(Sha256::digest(spawn_bytes))
    );

    let root = private_dir();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let session = SessionId::try_from(SESSION).unwrap();
    runtime().block_on(async {
        client
            .commit_keyed_spawn(
                SpawnRecord {
                    session_id: session.clone(),
                    handle_hash: [7_u8; 32],
                    receipt: json!({"state":"queued"}),
                    params: json!({"harness":"fake"}),
                    prompt: "p".into(),
                    effective: json!({"deadlines":{"wall_ms":1}}),
                    initial_event: event("turn.queued", 1, 1),
                },
                Some(SpawnKey {
                    key: "k-spawn".to_owned(),
                    identity,
                }),
            )
            .await
            .unwrap();
        let stored = client.spawn_key("k-spawn").await.unwrap().unwrap();
        assert_eq!(stored.identity, identity);
        assert_ne!(stored.identity, Identity::of(&changed));
        client
            .commit_resume(ResumeRecord {
                session_id: session.clone(),
                turn: TurnNumber::try_from(2).unwrap(),
                prompt: "q".into(),
                effective: json!({"deadlines":{"wall_ms":1}}),
                event: event("turn.queued", 2, 2),
                operation: Some(OperationRecord {
                    op_key: "k".to_owned(),
                    identity: Identity::of(resume_bytes),
                    result: json!({"turn":2}),
                }),
            })
            .await
            .unwrap();
        let operation = client.operation(&session, "k").await.unwrap().unwrap();
        assert_eq!(operation.identity, Identity::of(resume_bytes));
    });
    drop(store);

    let conn = rusqlite::Connection::open(root.path().join("store.sqlite3")).unwrap();
    let (len, digest): (i64, Vec<u8>) = conn
        .query_row(
            "SELECT identity_len,identity_sha256 FROM spawn_keys WHERE key='k-spawn'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(len, i64::try_from(spawn_bytes.len()).unwrap());
    assert_eq!(digest, Sha256::digest(spawn_bytes).to_vec());
    let (len, digest): (i64, Vec<u8>) = conn
        .query_row(
            "SELECT identity_len,identity_sha256 FROM operations WHERE op_key='k'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(len, i64::try_from(resume_bytes.len()).unwrap());
    assert_eq!(digest, Sha256::digest(resume_bytes).to_vec());
    // No column keeps the identity bytes.
    for table in ["spawn_keys", "operations"] {
        let columns: Vec<String> = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            !columns.iter().any(|column| column == "identity"),
            "{columns:?}"
        );
    }
    let short = conn.execute(
        "INSERT INTO spawn_keys(key,session_id,identity_len,identity_sha256,receipt) VALUES ('bad',?1,1,?2,'{}')",
        rusqlite::params![SESSION, vec![0_u8; 31]],
    );
    assert!(short.is_err(), "a 31-byte digest was stored");
}
