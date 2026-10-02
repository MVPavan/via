//! Task 4 design §6.6, runtime §6: the schema, v9 since server-owned
//! anchors and the turn → server-anchor link (v8: each turn's recorded
//! instance), is frozen by a golden DDL.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{fs, os::unix::fs::PermissionsExt};

use tempfile::TempDir;
use via_store::Store;

/// The v9 schema: every `sqlite_master` entry as `type name tbl_name sql`,
/// with the SQL's whitespace collapsed. Changing it is a schema change.
const GOLDEN: &[(&str, &str, &str, &str)] = &[
    (
        "index",
        "anchors_one_server",
        "anchors",
        "CREATE UNIQUE INDEX anchors_one_server ON anchors(owner_server) WHERE owner_server IS NOT NULL",
    ),
    (
        "index",
        "anchors_unproven",
        "anchors",
        "CREATE INDEX anchors_unproven ON anchors(anchor_id) WHERE absence_time IS NULL",
    ),
    (
        "index",
        "events_turn",
        "events",
        "CREATE INDEX events_turn ON events(session_id,turn,seq)",
    ),
    (
        "index",
        "server_turns_anchor",
        "server_turns",
        "CREATE INDEX server_turns_anchor ON server_turns(anchor_id)",
    ),
    ("index", "sqlite_autoindex_anchors_1", "anchors", ""),
    ("index", "sqlite_autoindex_events_1", "events", ""),
    ("index", "sqlite_autoindex_operations_1", "operations", ""),
    ("index", "sqlite_autoindex_sessions_1", "sessions", ""),
    ("index", "sqlite_autoindex_sessions_2", "sessions", ""),
    ("index", "sqlite_autoindex_spawn_keys_1", "spawn_keys", ""),
    ("index", "sqlite_autoindex_turns_1", "turns", ""),
    (
        "index",
        "turns_one_running",
        "turns",
        "CREATE UNIQUE INDEX turns_one_running ON turns(session_id) WHERE state='running'",
    ),
    (
        "table",
        "anchors",
        "anchors",
        "CREATE TABLE anchors ( anchor_id TEXT PRIMARY KEY, generation TEXT NOT NULL, \
         marker TEXT NOT NULL, socket_path TEXT NOT NULL, owner_session TEXT, \
         owner_turn INTEGER, owner_server TEXT, uid INTEGER NOT NULL, boot_id TEXT NOT NULL, \
         pid_namespace TEXT NOT NULL, phase TEXT NOT NULL, record_version INTEGER NOT NULL, \
         pid INTEGER, pgid INTEGER, start_ticks INTEGER, vendor_pid INTEGER, absence_time TEXT, \
         CHECK((owner_session IS NULL) = (owner_turn IS NULL)), \
         CHECK((owner_server IS NULL) <> (owner_session IS NULL)), \
         FOREIGN KEY(owner_session,owner_turn) REFERENCES turns(session_id,number))",
    ),
    (
        "table",
        "events",
        "events",
        "CREATE TABLE events ( session_id TEXT NOT NULL REFERENCES sessions(id), \
         seq INTEGER NOT NULL, turn INTEGER, type TEXT NOT NULL, event TEXT NOT NULL, \
         PRIMARY KEY(session_id,seq), FOREIGN KEY(session_id,turn) REFERENCES \
         turns(session_id,number) DEFERRABLE INITIALLY DEFERRED)",
    ),
    (
        "table",
        "operations",
        "operations",
        "CREATE TABLE operations ( session_id TEXT NOT NULL REFERENCES sessions(id), \
         op_key TEXT NOT NULL, verb TEXT NOT NULL CHECK(verb IN ('resume','close')), \
         identity_len INTEGER NOT NULL, identity_sha256 BLOB NOT NULL \
         CHECK(length(identity_sha256)=32), turn INTEGER, result TEXT, \
         PRIMARY KEY(session_id,op_key), \
         CHECK(verb='close' OR (turn IS NOT NULL AND result IS NOT NULL)), \
         FOREIGN KEY(session_id,turn) REFERENCES turns(session_id,number))",
    ),
    (
        "table",
        "server_turns",
        "server_turns",
        "CREATE TABLE server_turns ( session_id TEXT NOT NULL, turn INTEGER NOT NULL, \
         anchor_id TEXT NOT NULL REFERENCES anchors(anchor_id), \
         PRIMARY KEY(session_id, turn), \
         FOREIGN KEY(session_id, turn) REFERENCES turns(session_id, number) ) WITHOUT ROWID",
    ),
    (
        "table",
        "session_ord",
        "session_ord",
        "CREATE TABLE session_ord (only INTEGER PRIMARY KEY CHECK(only = 1), next INTEGER NOT NULL)",
    ),
    (
        "table",
        "sessions",
        "sessions",
        "CREATE TABLE sessions ( id TEXT PRIMARY KEY, \
         handle_hash BLOB NOT NULL CHECK(length(handle_hash)=32), receipt TEXT NOT NULL, \
         params TEXT NOT NULL, state TEXT NOT NULL, next_seq INTEGER NOT NULL CHECK(next_seq>=2), \
         admission TEXT NOT NULL DEFAULT 'open' CHECK(admission IN ('open','closing')), \
         close_result TEXT, created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL, \
         harness TEXT NOT NULL, label TEXT, ord INTEGER NOT NULL UNIQUE, \
         vendor_session_id TEXT, transcript_hint TEXT, adapter_version TEXT)",
    ),
    (
        "table",
        "spawn_keys",
        "spawn_keys",
        "CREATE TABLE spawn_keys ( key TEXT PRIMARY KEY, \
         session_id TEXT NOT NULL REFERENCES sessions(id), identity_len INTEGER NOT NULL, \
         identity_sha256 BLOB NOT NULL CHECK(length(identity_sha256)=32), receipt TEXT NOT NULL)",
    ),
    (
        "table",
        "steps",
        "steps",
        "CREATE TABLE steps ( session_id TEXT NOT NULL, turn INTEGER NOT NULL, \
         step INTEGER NOT NULL CHECK(step >= 1), started_ms INTEGER NOT NULL, \
         ended_ms INTEGER NOT NULL, tokens INTEGER CHECK(tokens IS NULL OR tokens >= 0), \
         PRIMARY KEY(session_id, turn, step), \
         FOREIGN KEY(session_id, turn) REFERENCES turns(session_id, number) ) WITHOUT ROWID",
    ),
    (
        "table",
        "turns",
        "turns",
        "CREATE TABLE turns ( session_id TEXT NOT NULL REFERENCES sessions(id), \
         number INTEGER NOT NULL, prompt TEXT, prompt_blob TEXT, effective TEXT NOT NULL, \
         state TEXT NOT NULL, queued_at TEXT, queued_seq INTEGER NOT NULL, submitted_at TEXT, \
         accepted_at TEXT, correlation TEXT, envelope TEXT, \
         cancel_cause TEXT CHECK(cancel_cause IN ('cancel','close')), ended_seq INTEGER, \
         evidence_dir TEXT, vendor_version TEXT, \
         version_status TEXT CHECK(version_status IN ('tested','untested')), \
         CHECK((state IN ('completed','failed','cancelled','unknown')) = (ended_seq IS NOT NULL)), \
         CHECK((prompt IS NULL) <> (prompt_blob IS NULL)), PRIMARY KEY(session_id,number))",
    ),
];

fn private_dir() -> TempDir {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn collapse(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Design §6.6, runtime §6: a fresh Store is v9 exactly as frozen here, with the
/// `session_ord` counter at `(1, 0)` and the evidence root beside it.
#[test]
fn s1_store_v9_schema_is_frozen() {
    let root = private_dir();
    drop(Store::open(root.path()).unwrap());
    let conn = rusqlite::Connection::open(root.path().join("store.sqlite3")).unwrap();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 9);
    let mut query = conn
        .prepare("SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name")
        .unwrap();
    let actual: Vec<(String, String, String, String)> = query
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                collapse(&row.get::<_, Option<String>>(3)?.unwrap_or_default()),
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let golden: Vec<(String, String, String, String)> = GOLDEN
        .iter()
        .map(|(kind, name, table, sql)| {
            (
                (*kind).to_owned(),
                (*name).to_owned(),
                (*table).to_owned(),
                collapse(sql),
            )
        })
        .collect();
    for (actual, golden) in actual.iter().zip(&golden) {
        assert_eq!(actual, golden);
    }
    assert_eq!(actual.len(), golden.len(), "{actual:#?}");
    let counter: (i64, i64) = conn
        .query_row("SELECT only,next FROM session_ord", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(counter, (1, 0));
    let evidence = fs::symlink_metadata(root.path().join("evidence")).unwrap();
    assert!(evidence.is_dir());
    assert_eq!(evidence.permissions().mode() & 0o777, 0o700);
    assert!(!root.path().join("raw").exists());
}
