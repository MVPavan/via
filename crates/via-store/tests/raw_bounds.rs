//! A corrupt index must be rejected before its claimed length is allocated.
#![expect(
    clippy::unwrap_used,
    reason = "isolated regression fixture fails loudly"
)]

use std::{env, fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

use serde_json::json;
use tempfile::TempDir;
use via_store::{ConnectionId, RawStream, SessionId, SpawnRecord, Store, StoreError, TurnNumber};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn oversized_raw_index_is_rejected_before_allocation() {
    if let Some(state) = env::var_os("VIA_RAW_BOUNDS_CHILD") {
        let store = Store::open(Path::new(&state)).unwrap();
        let session = SessionId::try_from("s_7f3k9q2mzr4c").unwrap();
        let result = runtime().block_on(store.client().logs(&session));
        assert!(matches!(result, Err(StoreError::CorruptEvidence)));
        return;
    }

    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let session = SessionId::try_from("s_7f3k9q2mzr4c").unwrap();
    let turn = TurnNumber::try_from(1).unwrap();
    {
        let store = Store::open(root.path()).unwrap();
        let client = store.client();
        runtime().block_on(async {
            client
                .commit_spawn(SpawnRecord {
                    session_id: session.clone(),
                    handle_hash: [4_u8; 32],
                    receipt: json!({"state":"queued"}),
                    params: json!({"harness":"fake"}),
                    prompt: "x".to_owned(),
                    initial_event: json!({"type":"turn.queued","seq":1}),
                })
                .await
                .unwrap();
            client.commit_submission(&session, turn).await.unwrap();
            let raw = store
                .runtime_resources()
                .into_wire_parts()
                .0
                .open(ConnectionId::try_from("c_huge").unwrap())
                .append(RawStream::Stdout, b"ok\n".to_vec())
                .await
                .unwrap();
            client
                .commit_acceptance(&session, turn, raw.raw_ref(), "fake-turn-1")
                .await
                .unwrap();
        });
    }

    let db = rusqlite::Connection::open(root.path().join("store.sqlite3")).unwrap();
    db.execute(
        "UPDATE events SET raw_len=?1 WHERE session_id=?2 AND seq=3",
        rusqlite::params![i64::from(u32::MAX), session.as_str()],
    )
    .unwrap();
    drop(db);
    let index = root.path().join("raw/c_huge.idx");
    let mut bytes = fs::read(&index).unwrap();
    bytes[17..21].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&index, bytes).unwrap();

    // A 128 MiB address-space cap turns the old 4 GiB allocation into a
    // bounded child failure; the corrected read returns CorruptEvidence.
    let output = Command::new("prlimit")
        .arg("--as=134217728")
        .arg("--")
        .arg(env::current_exe().unwrap())
        .arg("--exact")
        .arg("oversized_raw_index_is_rejected_before_allocation")
        .arg("--nocapture")
        .env("VIA_RAW_BOUNDS_CHILD", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bounded read child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
