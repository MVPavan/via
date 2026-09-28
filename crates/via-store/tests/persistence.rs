//! Failure-first checks for the real SQLite and raw durability boundary.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};

use serde_json::json;
use tempfile::TempDir;
use via_store::{
    AcceptanceRecord, AnchorIdentity, AnchorIntent, AnchorPhase, CommitOutcome, ConnectionId,
    RawRef, RawStream, SessionId, SpawnRecord, Store, StoreError, SubmissionRecord, TerminalRecord,
    TurnNumber,
};

fn session() -> SessionId {
    SessionId::try_from("s_7f3k9q2mzr4c").unwrap()
}

fn turn() -> TurnNumber {
    TurnNumber::try_from(1).unwrap()
}

fn spawn(hash: [u8; 32]) -> SpawnRecord {
    SpawnRecord {
        session_id: session(),
        handle_hash: hash,
        receipt: json!({"session_id":"s_7f3k9q2mzr4c","turn":"s_7f3k9q2mzr4c/1","state":"queued"}),
        params: json!({"harness":"fake"}),
        prompt: "test prompt".to_owned(),
        effective: json!({"deadlines":{"wall_ms":1}}),
        initial_event: json!({"type":"turn.queued","seq":1}),
    }
}

fn submission() -> SubmissionRecord {
    SubmissionRecord {
        session_id: session(),
        turn: turn(),
        event: json!({"type":"turn.submitted","seq":2,"at":"2026-01-01T00:00:00.000Z","raw_ref":null}),
    }
}

fn acceptance(raw_ref: &RawRef) -> AcceptanceRecord {
    AcceptanceRecord {
        session_id: session(),
        turn: turn(),
        raw_ref: raw_ref.clone(),
        correlation: "fake-turn-1".to_owned(),
        event: json!({"type":"turn.started","seq":3,"at":"2026-01-01T00:00:01.000Z","raw_ref":raw_ref}),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn spawn_submission_and_terminal_survive_reopen_without_leaking_handle() {
    let root = TempDir::new().unwrap();
    let state = root.path();
    fs::set_permissions(state, fs::Permissions::from_mode(0o700)).unwrap();
    let handle = "h_s3cr3t-private-handle-do-not-persist";
    let hash = [9_u8; 32];
    let rt = runtime();
    {
        let store = Store::open(state).unwrap();
        let client = store.client();
        rt.block_on(async {
            assert!(
                client
                    .commit_terminal(TerminalRecord {
                        session_id: session(),
                        turn: turn(),
                        envelope: json!({"state":"completed"}),
                        event: json!({"type":"turn.ended","seq":4,"raw_ref":null}),
                        raw_ref: None,
                    })
                    .await
                    .is_err()
            );
            let receipt = client.commit_spawn(spawn(hash)).await.unwrap();
            assert_eq!(receipt.receipt["state"], "queued");
            assert!(!client.authenticate(&session(), &[8_u8; 32]).await.unwrap());
            assert!(client.authenticate(&session(), &hash).await.unwrap());
            client.commit_submission(submission()).await.unwrap();
            assert!(
                client
                    .commit_terminal(TerminalRecord {
                        session_id: session(),
                        turn: turn(),
                        envelope: json!({"state":"completed"}),
                        event: json!({"type":"turn.ended","seq":3,"raw_ref":null}),
                        raw_ref: None,
                    })
                    .await
                    .is_err()
            );
            let accepted = store
                .runtime_resources()
                .into_wire_parts()
                .0
                .open(ConnectionId::try_from("c_accept").unwrap())
                .append(RawStream::Stdout, b"accepted\n".to_vec())
                .await
                .unwrap();
            client
                .commit_acceptance(acceptance(accepted.raw_ref()))
                .await
                .unwrap();
            client
                .commit_terminal(TerminalRecord {
                    session_id: session(),
                    turn: turn(),
                    envelope: json!({"state":"completed","final_text":"reply"}),
                    event: json!({"type":"turn.ended","seq":4,"raw_ref":null}),
                    raw_ref: None,
                })
                .await
                .unwrap();
        });
    }
    {
        let store = Store::open(state).unwrap();
        let client = store.client();
        rt.block_on(async {
            assert_eq!(
                client.result(&session(), turn()).await.unwrap().unwrap()["final_text"],
                "reply"
            );
            let events = client.events(&session(), 1, 10).await.unwrap();
            assert_eq!(events.len(), 4);
            assert_eq!(events[0].seq, 1);
            assert_eq!(events[1].seq, 2);
            assert_eq!(events[2].seq, 3);
            assert_eq!(events[3].seq, 4);
        });
    }
    for path in ["store.sqlite3", "store.sqlite3-wal"] {
        if Path::new(&state.join(path)).exists() {
            let contents = fs::read(state.join(path)).unwrap();
            assert!(
                !contents
                    .windows(handle.len())
                    .any(|w| w == handle.as_bytes())
            );
        }
    }
}

#[test]
fn raw_reference_requires_synced_index_entry() {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let rt = runtime();
    rt.block_on(async {
        client.commit_spawn(spawn([7_u8; 32])).await.unwrap();
        client.commit_submission(submission()).await.unwrap();
        let connection_id = ConnectionId::try_from("c_01").unwrap();
        let writer = store
            .runtime_resources()
            .into_wire_parts()
            .0
            .open(connection_id.clone());
        let accepted = writer
            .append(RawStream::Stdout, b"accepted\n".to_vec())
            .await
            .unwrap();
        client
            .commit_acceptance(acceptance(accepted.raw_ref()))
            .await
            .unwrap();
        let forged =
            RawRef::new(connection_id.clone(), accepted.raw_ref().end_offset(), 6).unwrap();
        assert!(matches!(
            client
                .commit_terminal(TerminalRecord {
                    session_id: session(),
                    turn: turn(),
                    envelope: json!({"state":"completed"}),
                    event: json!({"type":"turn.ended","seq":4,"raw_ref":forged}),
                    raw_ref: Some(forged),
                })
                .await,
            Err(StoreError::CorruptEvidence)
        ));
        client
            .commit_acceptance(acceptance(accepted.raw_ref()))
            .await
            .unwrap();
        let token = writer
            .append(RawStream::Stdout, b"reply\n".to_vec())
            .await
            .unwrap();
        assert_eq!(token.raw_ref().offset(), accepted.raw_ref().end_offset());
        client
            .commit_terminal(TerminalRecord {
                session_id: session(),
                turn: turn(),
                envelope: json!({"state":"completed"}),
                event: json!({"type":"turn.ended","seq":4,"raw_ref":token.raw_ref()}),
                raw_ref: Some(token.raw_ref().clone()),
            })
            .await
            .unwrap();
        let logs = client.logs(&session()).await.unwrap();
        assert_eq!(logs["entries"][0]["text"], "accepted\n");
        assert_eq!(logs["entries"][1]["text"], "reply\n");
        assert_eq!(logs["entries"][0]["offset"], 0);
        assert_eq!(logs["next_after"], 4);
    });
}

#[test]
fn raw_writer_refuses_symlink_targets() {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(root.path()).unwrap();
    let target = root.path().join("outside");
    fs::write(&target, b"unchanged").unwrap();
    symlink(&target, root.path().join("raw/c_link.raw")).unwrap();
    symlink(&target, root.path().join("raw/c_link.idx")).unwrap();
    let connection = ConnectionId::try_from("c_link").unwrap();
    let writer = store
        .runtime_resources()
        .into_wire_parts()
        .0
        .open(connection);
    assert!(
        runtime()
            .block_on(writer.append(RawStream::Stdout, b"secret\n".to_vec()))
            .is_err()
    );
    assert_eq!(fs::read(&target).unwrap(), b"unchanged");
}

#[test]
fn failure_before_vendor_acceptance_is_still_durable() {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        client.commit_spawn(spawn([5_u8; 32])).await.unwrap();
        client.commit_submission(submission()).await.unwrap();
        client
            .commit_terminal(TerminalRecord {
                session_id: session(),
                turn: turn(),
                envelope: json!({"state":"failed","failure":{"class":"process_exited"}}),
                event: json!({"type":"turn.ended","seq":3,"raw_ref":null}),
                raw_ref: None,
            })
            .await
            .unwrap();
        assert_eq!(
            client.result(&session(), turn()).await.unwrap().unwrap()["state"],
            "failed"
        );
    });
}

#[test]
fn logs_bound_counts_json_escaping() {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        client.commit_spawn(spawn([6_u8; 32])).await.unwrap();
        client.commit_submission(submission()).await.unwrap();
        let raw = store
            .runtime_resources()
            .into_wire_parts()
            .0
            .open(ConnectionId::try_from("c_escape").unwrap())
            .append(RawStream::Stdout, vec![1_u8; 300_000])
            .await
            .unwrap();
        client
            .commit_acceptance(acceptance(raw.raw_ref()))
            .await
            .unwrap();
        assert!(client.logs(&session()).await.is_err());
    });
}

#[test]
fn anchor_arm_requires_committed_matching_identity_and_version() {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let rt = runtime();
    {
        let store = Store::open(root.path()).unwrap();
        let client = store.client();
        let journal = store.runtime_resources().into_wire_parts().1;
        rt.block_on(async {
            client.commit_spawn(spawn([3_u8; 32])).await.unwrap();
            client.commit_submission(submission()).await.unwrap();
            let intent = AnchorIntent {
                anchor_id: "a_01".to_owned(),
                generation: "gen-1".to_owned(),
                marker: "private-marker".to_owned(),
                socket_path: root.path().join("a_01.sock"),
                owner_session: session(),
                owner_turn: turn(),
                uid: fs::metadata(root.path()).unwrap().uid(),
                boot_id: "boot-1".to_owned(),
                pid_namespace: "pid:[42]".to_owned(),
            };
            let CommitOutcome::Committed(receipt) = journal.commit_anchor_intent(intent).await
            else {
                panic!("anchor intent did not commit");
            };
            assert_eq!(receipt.record_version, 1);
            assert!(matches!(
                journal.commit_arm_intent("a_01", "gen-1", 1).await,
                CommitOutcome::NotCommitted(_)
            ));
            let identity = AnchorIdentity {
                pid: 120,
                pgid: 120,
                uid: fs::metadata(root.path()).unwrap().uid(),
                boot_id: "boot-1".to_owned(),
                pid_namespace: "pid:[42]".to_owned(),
                start_ticks: 99,
                marker: "private-marker".to_owned(),
            };
            assert!(matches!(
                journal
                    .commit_anchor_identified("a_01", "wrong", 1, identity.clone())
                    .await,
                CommitOutcome::NotCommitted(_)
            ));
            let CommitOutcome::Committed(version) = journal
                .commit_anchor_identified("a_01", "gen-1", 1, identity)
                .await
            else {
                panic!("identity did not commit");
            };
            assert_eq!(version, 2);
            assert!(matches!(
                journal.commit_arm_intent("a_01", "gen-1", 1).await,
                CommitOutcome::NotCommitted(_)
            ));
            assert!(matches!(
                journal.commit_arm_intent("a_01", "gen-1", 2).await,
                CommitOutcome::Committed(3)
            ));
            assert!(matches!(
                journal.commit_arm_intent("a_01", "gen-1", 2).await,
                CommitOutcome::NotCommitted(_)
            ));
        });
    }
    let store = Store::open(root.path()).unwrap();
    let records = rt
        .block_on(
            store
                .runtime_resources()
                .into_wire_parts()
                .1
                .list_anchor_records_page(None, via_store::ANCHOR_PAGE_LIMIT),
        )
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].phase, AnchorPhase::ArmIntent);
    assert_eq!(records[0].record_version, 3);
    assert_eq!(records[0].identity.as_ref().unwrap().pgid, 120);
}

#[test]
fn newer_schema_is_refused_without_mutation() {
    let root = TempDir::new().unwrap();
    let db = root.path().join("store.sqlite3");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&db).unwrap();
    assert!(Store::open(root.path()).is_err());
    assert_eq!(fs::read(&db).unwrap(), before);
}

#[test]
fn events_keep_dense_seq_and_cited_raw_span() {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    runtime().block_on(async {
        client.commit_spawn(spawn([2_u8; 32])).await.unwrap();
        let mut skipped = submission();
        skipped.event["seq"] = json!(3);
        assert!(matches!(
            client.commit_submission(skipped).await,
            Err(StoreError::Constraint(_))
        ));
        let mut untimed = submission();
        untimed.event.as_object_mut().unwrap().remove("at");
        assert!(client.commit_submission(untimed).await.is_err());
        client.commit_submission(submission()).await.unwrap();
        let writer = store
            .runtime_resources()
            .into_wire_parts()
            .0
            .open(ConnectionId::try_from("c_cite").unwrap());
        let first = writer
            .append(RawStream::Stdout, b"first\n".to_vec())
            .await
            .unwrap();
        let second = writer
            .append(RawStream::Stdout, b"second\n".to_vec())
            .await
            .unwrap();
        let mut miscited = acceptance(first.raw_ref());
        miscited.event["raw_ref"] = serde_json::to_value(second.raw_ref()).unwrap();
        assert!(matches!(
            client.commit_acceptance(miscited).await,
            Err(StoreError::Constraint(_))
        ));
        let mut uncited = acceptance(first.raw_ref());
        uncited.event.as_object_mut().unwrap().remove("raw_ref");
        assert!(matches!(
            client.commit_acceptance(uncited).await,
            Err(StoreError::Constraint(_))
        ));
        let mut nulled = acceptance(first.raw_ref());
        nulled.event["raw_ref"] = serde_json::Value::Null;
        assert!(matches!(
            client.commit_acceptance(nulled).await,
            Err(StoreError::Constraint(_))
        ));
        client
            .commit_acceptance(acceptance(first.raw_ref()))
            .await
            .unwrap();
        let events = client.events(&session(), 1, 10).await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].event["type"], "turn.started");
        assert_eq!(events[2].raw_ref.as_ref(), Some(first.raw_ref()));
    });
    drop(store);
    let db = rusqlite::Connection::open(root.path().join("store.sqlite3")).unwrap();
    let (submitted, accepted): (String, String) = db
        .query_row(
            "SELECT submitted_at,accepted_at FROM turns WHERE session_id=?1",
            [session().as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(submitted, "2026-01-01T00:00:00.000Z");
    assert_eq!(accepted, "2026-01-01T00:00:01.000Z");
}

/// Runtime §6: the unreleased dev formats (schema v1, the single-turn format;
/// v2, before the unproven-anchor index; v3, before frozen per-turn values)
/// are not migrated. Opening one fails with a named, actionable error and
/// leaves its bytes untouched.
#[test]
fn unreleased_v1_to_v3_stores_are_refused_with_a_recreate_instruction() {
    for version in [1, 2, 3] {
        let root = TempDir::new().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let db = root.path().join("store.sqlite3");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY)")
                .unwrap();
            conn.pragma_update(None, "user_version", version).unwrap();
        }
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        let before = fs::read(&db).unwrap();
        let Err(error) = Store::open(root.path()) else {
            panic!("a v{version} Store opened");
        };
        let message = error.to_string();
        assert!(
            message.contains(&format!("schema v{version}")) && message.contains("recreate"),
            "{message}"
        );
        assert_eq!(fs::read(&db).unwrap(), before);
    }
}

/// Runtime §6: `user_version = 0` is initialized only in a database VIA
/// creates. An existing file at version 0, empty or holding foreign tables,
/// gets the same named recreate refusal before any writable open, and its
/// bytes stay untouched (no journal-mode change, no schema, no WAL).
#[test]
fn an_existing_version_zero_store_is_refused_without_mutation() {
    for foreign in [false, true] {
        let root = TempDir::new().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let db = root.path().join("store.sqlite3");
        if foreign {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE notes (body TEXT)")
                .unwrap();
        } else {
            fs::write(&db, b"").unwrap();
        }
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        let before = fs::read(&db).unwrap();
        let Err(error) = Store::open(root.path()) else {
            panic!("an existing version-0 Store opened (foreign tables: {foreign})");
        };
        let message = error.to_string();
        assert!(
            message.contains("schema v0") && message.contains("recreate"),
            "{message}"
        );
        assert_eq!(fs::read(&db).unwrap(), before, "foreign tables: {foreign}");
        assert!(!root.path().join("store.sqlite3-wal").exists());
    }
}

/// Runtime §6: at most eight queued turns per session, checked inside the
/// receipt transaction itself, not only by Core before it.
#[test]
fn a_ninth_queued_turn_is_refused_inside_the_receipt_transaction() {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let rt = runtime();
    let at = "2026-01-01T00:00:00.000Z";
    let mut first = spawn([1; 32]);
    first.initial_event = json!({"type":"turn.queued","seq":1,"at":at});
    rt.block_on(client.commit_spawn(first)).unwrap();
    let resume = |turn: u32| via_store::ResumeRecord {
        session_id: session(),
        turn: TurnNumber::try_from(turn).unwrap(),
        prompt: "p".to_owned(),
        effective: json!({"deadlines":{"wall_ms":turn}}),
        event: json!({"type":"turn.queued","seq":turn,"at":at,"raw_ref":null}),
        operation: None,
    };
    // Turn 1 and turns 2..=8 are the eight queued turns.
    for turn in 2..=8 {
        rt.block_on(client.commit_resume(resume(turn))).unwrap();
    }
    let refused = rt.block_on(client.commit_resume(resume(9)));
    assert!(
        matches!(refused, Err(StoreError::Constraint(_))),
        "{refused:?}"
    );
    let snapshot = rt
        .block_on(client.session_snapshot(&session()))
        .unwrap()
        .unwrap();
    assert_eq!((snapshot.turns, snapshot.queued), (8, 8));
}

/// C1 P5: each turn's frozen values are stored with it, and a new turn
/// inherits from the latest accepted turn whatever its later state: a
/// queued turn cancelled afterwards still supplies them.
#[test]
fn frozen_turn_values_are_stored_and_the_latest_turn_supplies_inheritance() {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(root.path()).unwrap();
    let client = store.client();
    let rt = runtime();
    let at = "2026-01-01T00:00:00.000Z";
    let mut first = spawn([1; 32]);
    first.initial_event = json!({"type":"turn.queued","seq":1,"at":at});
    rt.block_on(client.commit_spawn(first)).unwrap();
    let second = via_store::ResumeRecord {
        session_id: session(),
        turn: TurnNumber::try_from(2).unwrap(),
        prompt: "p".to_owned(),
        effective: json!({"deadlines":{"wall_ms":2}}),
        event: json!({"type":"turn.queued","seq":2,"at":at,"raw_ref":null}),
        operation: None,
    };
    rt.block_on(client.commit_resume(second)).unwrap();
    let queued = rt
        .block_on(client.queued_turn(&session(), TurnNumber::try_from(2).unwrap()))
        .unwrap()
        .unwrap();
    assert_eq!(queued.effective, json!({"deadlines":{"wall_ms":2}}));
    rt.block_on(client.commit_terminal(TerminalRecord {
        session_id: session(),
        turn: TurnNumber::try_from(2).unwrap(),
        envelope: json!({"state":"cancelled"}),
        event: json!({"type":"turn.ended","seq":3,"raw_ref":null}),
        raw_ref: None,
    }))
    .unwrap();
    let snapshot = rt
        .block_on(client.session_snapshot(&session()))
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot.latest_effective,
        Some(json!({"deadlines":{"wall_ms":2}}))
    );
}
