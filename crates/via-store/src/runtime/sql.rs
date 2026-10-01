//! SQLite migration and single-writer transaction implementation.

use super::disk::{PAGE_BYTES, Wal, WalLimits};
use super::{
    AcceptanceRecord, ActiveTurn, CancelCause, ClosedOutcome, ClosedRecord, ClosingRecord, Command,
    CommitOutcome, Connection, Duration, EventRecord, EventsPage, EventsQuery, EventsRead,
    EvidenceRefs, EvidenceRoot, FAILURE_BATCH_CANCELLATIONS, FailureResolutionRecord, Identity,
    KeyedOperation, ListPage, ListQuery, MetadataExt, OperationRecord, OperationVerb,
    OptionalExtension, PAGE_MAX, PAGE_SCAN, PAGE_WRAPPER, Path, Predecessors, Prompt,
    QueuedSummary, QueuedTurn, ReadCorruption, ReceiptRecord, ResumeRecord, SESSION_QUEUE_LIMIT,
    STATUS_ANCHORS, STATUS_QUEUE, STATUS_TURNS, SessionEventRecord, SessionId, SessionRoute,
    SessionSnapshot, SessionStatus, SessionSummary, SpawnKey, SpawnRecord, StatusQuery, StepRow,
    StepsRecord, StoreError, StoredEvent, StoredSpawnKey, SubmissionRecord, SubmitFailedRecord,
    TerminalCancel, TerminalExtras, TerminalFacts, TerminalRecord, TransactionBehavior, TurnNumber,
    UnfinishedTurn, Value, check_schema_version, commit_anchor_identified, commit_anchor_intent,
    commit_arm_intent, commit_group_absence, commit_vendor_facts, count_unproven_anchors, fs,
    oneshot, params, read_anchor_cohort, read_anchor_owners, read_anchor_records,
};
use crate::{
    blob::{BlobRef, Blobs},
    lanes::{DeadGuard, Lanes},
};

/// Classifies a SQLite error before `COMMIT`: corruption is `Corrupt`, which
/// always latches; anything else rolled the transaction back (design §7.1).
#[expect(
    clippy::needless_pass_by_value,
    reason = "the map_err adapter receives the error by value"
)]
pub(super) fn sql_error(error: rusqlite::Error) -> StoreError {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
            StoreError::Corrupt(error.to_string())
        }
        _ => StoreError::Write(error.to_string()),
    }
}

/// Classifies a failed `COMMIT` step (design §7.1): corruption is `Corrupt`,
/// which always latches. `SQLITE_FULL` (SQLite's unix VFS reports `ENOSPC`
/// as it) rolls back and is known not committed (Task 4 design §5.3):
/// `Write`, which [`settled`] turns `Uncertain` if the rollback failed. Any
/// other failure leaves the outcome unknown, so it is `Uncertain`.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the map_err adapter receives the error by value"
)]
pub(super) fn commit_error(error: rusqlite::Error) -> StoreError {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
            StoreError::Corrupt(error.to_string())
        }
        Some(rusqlite::ErrorCode::DiskFull) => StoreError::Write(error.to_string()),
        _ => StoreError::Uncertain(error.to_string()),
    }
}

/// A mutation's result once its transaction ended (Task 4 design §5.3): a
/// failure that wrote nothing is known only if the transaction is rolled
/// back. rusqlite rolls back a dropped transaction and ignores a failed
/// rollback, so the writer checks: a connection still inside a transaction
/// gets one more `ROLLBACK`, and if that fails too the outcome is
/// `Uncertain`, which latches. The test-only `store.rollback.fail` point
/// reports a failed rollback.
fn settled<T>(conn: &Connection, result: Result<T, StoreError>) -> Result<T, StoreError> {
    let error = match result {
        Ok(value) => return Ok(value),
        Err(
            error @ (StoreError::Uncertain(_) | StoreError::WriterLost | StoreError::Corrupt(_)),
        ) => return Err(error),
        Err(error) => error,
    };
    #[cfg(feature = "test-failpoints")]
    if crate::failpoint::hit("store.rollback.fail").is_err() {
        return Err(StoreError::Uncertain(format!(
            "rollback failed after: {error}"
        )));
    }
    if conn.is_autocommit() {
        return Err(error);
    }
    match conn.execute_batch("ROLLBACK") {
        Ok(()) if conn.is_autocommit() => Err(error),
        _ => Err(StoreError::Uncertain(format!(
            "rollback failed after: {error}"
        ))),
    }
}

/// Commits `tx`; every Store transaction commits through here, so every
/// commit site classifies a failure alike ([`commit_error`]).
pub(super) fn commit(tx: rusqlite::Transaction<'_>) -> Result<(), StoreError> {
    tx.commit().map_err(commit_error)
}

/// Test-only seams inside a transaction, just before `COMMIT` (design §10):
/// a `fail_io` at `point` or at `store.commit.fail_persistent` rolls the
/// transaction back, so the write is not committed; a pause holds the writer.
#[cfg(feature = "test-failpoints")]
pub(super) fn commit_seam(point: &'static str) -> Result<(), StoreError> {
    for point in [point, "store.commit.fail_persistent"] {
        crate::failpoint::hit(point).map_err(|error| StoreError::Write(error.to_string()))?;
    }
    Ok(())
}

/// [`commit_seam`] at a named point; nothing at all, not even the point's
/// name, in a release build.
macro_rules! before_commit {
    ($point:expr) => {
        #[cfg(feature = "test-failpoints")]
        crate::runtime::sql::commit_seam($point)?;
    };
}
pub(super) use before_commit;

pub(super) fn validate_state(path: &Path) -> Result<(), StoreError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| StoreError::Open(error.to_string()))?;
    if !metadata.file_type().is_dir()
        || metadata.mode() & 0o777 != 0o700
        || metadata.uid() != current_uid()?
    {
        return Err(StoreError::Open(format!(
            "unsafe State directory: mode {:o}, uid {}, expected uid {}",
            metadata.mode() & 0o777,
            metadata.uid(),
            current_uid()?
        )));
    }
    Ok(())
}

pub(super) fn current_uid() -> Result<u32, StoreError> {
    // Linux S1 reads its own kernel process metadata; no vendor environment is inspected.
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|error| StoreError::Open(error.to_string()))?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| StoreError::Open("current uid unavailable".to_owned()))
}

pub(super) fn validate_regular(path: &Path) -> Result<(), StoreError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| StoreError::Open(error.to_string()))?;
    if !metadata.file_type().is_file()
        || metadata.mode() & 0o777 != 0o600
        || metadata.uid() != current_uid()?
    {
        return Err(StoreError::Open("unsafe Store file".to_owned()));
    }
    Ok(())
}

/// Schema v7 (runtime §6; Task 4 design §6.6 plus the session's persisted
/// `adapter_version`), frozen by `s1_store_v7_schema_is_frozen`.
const SCHEMA_V7: &str = "CREATE TABLE sessions (
    id TEXT PRIMARY KEY, handle_hash BLOB NOT NULL CHECK(length(handle_hash)=32),
    receipt TEXT NOT NULL, params TEXT NOT NULL, state TEXT NOT NULL,
    next_seq INTEGER NOT NULL CHECK(next_seq>=2),
    admission TEXT NOT NULL DEFAULT 'open' CHECK(admission IN ('open','closing')),
    close_result TEXT, created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL,
    harness TEXT NOT NULL, label TEXT, ord INTEGER NOT NULL UNIQUE,
    vendor_session_id TEXT, transcript_hint TEXT, adapter_version TEXT);
 CREATE TABLE session_ord (only INTEGER PRIMARY KEY CHECK(only = 1), next INTEGER NOT NULL);
 INSERT INTO session_ord(only,next) VALUES (1,0);
 CREATE TABLE turns (
    session_id TEXT NOT NULL REFERENCES sessions(id), number INTEGER NOT NULL,
    prompt TEXT, prompt_blob TEXT, effective TEXT NOT NULL, state TEXT NOT NULL,
    queued_at TEXT, queued_seq INTEGER NOT NULL, submitted_at TEXT,
    accepted_at TEXT, correlation TEXT, envelope TEXT,
    cancel_cause TEXT CHECK(cancel_cause IN ('cancel','close')), ended_seq INTEGER,
    evidence_dir TEXT,
    CHECK((state IN ('completed','failed','cancelled','unknown')) = (ended_seq IS NOT NULL)),
    CHECK((prompt IS NULL) <> (prompt_blob IS NULL)),
    PRIMARY KEY(session_id,number));
 CREATE UNIQUE INDEX turns_one_running ON turns(session_id) WHERE state='running';
 CREATE TABLE spawn_keys (
    key TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
    identity_len INTEGER NOT NULL,
    identity_sha256 BLOB NOT NULL CHECK(length(identity_sha256)=32), receipt TEXT NOT NULL);
 CREATE TABLE operations (
    session_id TEXT NOT NULL REFERENCES sessions(id), op_key TEXT NOT NULL,
    verb TEXT NOT NULL CHECK(verb IN ('resume','close')), identity_len INTEGER NOT NULL,
    identity_sha256 BLOB NOT NULL CHECK(length(identity_sha256)=32),
    turn INTEGER, result TEXT, PRIMARY KEY(session_id,op_key),
    CHECK(verb='close' OR (turn IS NOT NULL AND result IS NOT NULL)),
    FOREIGN KEY(session_id,turn) REFERENCES turns(session_id,number));
 CREATE TABLE events (
    session_id TEXT NOT NULL REFERENCES sessions(id), seq INTEGER NOT NULL,
    turn INTEGER, type TEXT NOT NULL, event TEXT NOT NULL,
    PRIMARY KEY(session_id,seq),
    FOREIGN KEY(session_id,turn) REFERENCES turns(session_id,number)
        DEFERRABLE INITIALLY DEFERRED);
 CREATE INDEX events_turn ON events(session_id,turn,seq);
 CREATE TABLE steps (
    session_id TEXT NOT NULL,
    turn INTEGER NOT NULL,
    step INTEGER NOT NULL CHECK(step >= 1),
    started_ms INTEGER NOT NULL,
    ended_ms INTEGER NOT NULL,
    tokens INTEGER CHECK(tokens IS NULL OR tokens >= 0),
    PRIMARY KEY(session_id, turn, step),
    FOREIGN KEY(session_id, turn) REFERENCES turns(session_id, number)
 ) WITHOUT ROWID;
 CREATE TABLE anchors (
    anchor_id TEXT PRIMARY KEY, generation TEXT NOT NULL, marker TEXT NOT NULL,
    socket_path TEXT NOT NULL, owner_session TEXT NOT NULL, owner_turn INTEGER NOT NULL,
    uid INTEGER NOT NULL, boot_id TEXT NOT NULL, pid_namespace TEXT NOT NULL,
    phase TEXT NOT NULL, record_version INTEGER NOT NULL,
    pid INTEGER, pgid INTEGER, start_ticks INTEGER, vendor_pid INTEGER,
    absence_time TEXT,
    FOREIGN KEY(owner_session,owner_turn) REFERENCES turns(session_id,number));
 CREATE INDEX anchors_unproven ON anchors(anchor_id) WHERE absence_time IS NULL;
 PRAGMA user_version=7;";

/// Configures the sole writable connection; initializes the schema only in
/// a database this open `created`. The version is checked again before the
/// first mutation, the journal-mode switch. The checkpoint policy is
/// `wal`'s (Task 4 design §5.4): a passive checkpoint after
/// `checkpoint_bytes` of growth, in whole pages, and a WAL cut to
/// `checkpoint_bytes` when it resets (`journal_size_limit`).
pub(super) fn configure(
    conn: &mut Connection,
    created: bool,
    wal: &WalLimits,
) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(|error| StoreError::Open(error.to_string()))?;
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| StoreError::Open(error.to_string()))?;
    check_schema_version(version, created)?;
    let journal: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(|error| StoreError::Open(error.to_string()))?;
    if journal != "wal" {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| StoreError::Open(error.to_string()))?;
        let changed: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(|error| StoreError::Open(error.to_string()))?;
        if changed != "wal" {
            return Err(StoreError::Open("WAL mode unavailable".to_owned()));
        }
    }
    conn.execute_batch(
        "PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA cache_size=-8192; PRAGMA mmap_size=0;",
    )
    .map_err(|error| StoreError::Open(error.to_string()))?;
    let pages = i64::try_from(wal.checkpoint_bytes / PAGE_BYTES)
        .map_err(|error| StoreError::Open(error.to_string()))?;
    let limit =
        i64::try_from(wal.checkpoint_bytes).map_err(|error| StoreError::Open(error.to_string()))?;
    conn.execute_batch(&format!(
        "PRAGMA wal_autocheckpoint={pages}; PRAGMA journal_size_limit={limit};"
    ))
    .map_err(|error| StoreError::Open(error.to_string()))?;
    if version == 0 {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| StoreError::Open(error.to_string()))?;
        tx.execute_batch(SCHEMA_V7)
            .map_err(|error| StoreError::Open(error.to_string()))?;
        commit(tx)?;
    }
    Ok(())
}

/// The SQLite thread's body (design §6.1, §6.3): serves the lanes in
/// service order until the fence drains them. It runs under a
/// [`DeadGuard`], so however it ends, unwinding included, the writer is
/// marked dead and the request in hand and every queued one fail
/// `WriterLost`. Every mutation passes `wal`'s policy (Task 4 design
/// §5.4) before and after it.
pub(super) fn writer_loop(
    mut conn: Connection,
    lanes: &Lanes,
    corruption: &ReadCorruption,
    blobs: &Blobs,
    mut wal: Wal,
) {
    let mut guard = DeadGuard::new(lanes);
    while let Some(command) = lanes.pop() {
        guard.in_flight = Some(command);
        // Test-only `store.writer.before_serve`: a pause holds the request
        // in hand; `fail_io` kills the writer with it (design §13.1).
        #[cfg(feature = "test-failpoints")]
        if crate::failpoint::hit("store.writer.before_serve").is_err() {
            writer_died();
        }
        let Some(command) = guard.in_flight.take() else {
            continue;
        };
        // Test-only `store.writer.lost`: the worker drops the request and its
        // reply unserved, as a writer that is gone would (design §7.1).
        #[cfg(feature = "test-failpoints")]
        if crate::failpoint::hit("store.writer.lost").is_err() {
            drop(command);
            continue;
        }
        // Test builds: each read is counted (`Store::read_count`) and
        // `store.read.delay_ms` may delay it (design §13.1).
        #[cfg(feature = "test-failpoints")]
        if command.is_read() {
            lanes.count_read();
            let _ = crate::failpoint::hit("store.read.delay_ms");
        }
        // Reads are served first; anything else is a mutation.
        let Some(command) = serve_read(&conn, command, corruption) else {
            continue;
        };
        let Some(command) = wal.admit(&conn, command) else {
            continue;
        };
        serve_write(&mut conn, command, blobs);
        wal.committed(&conn);
    }
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    drop(guard);
}

/// Test builds: the writer thread dies, unwinding, as a failed SQLite
/// thread would.
#[cfg(feature = "test-failpoints")]
#[expect(clippy::panic, reason = "the seam kills the writer thread")]
fn writer_died() -> ! {
    panic!("store.writer.before_serve: the writer died");
}

impl Command {
    /// Whether the command only reads.
    fn is_read(&self) -> bool {
        matches!(
            self,
            Self::SpawnKey(..)
                | Self::Operation(..)
                | Self::KeyedOperation(..)
                | Self::Snapshot(..)
                | Self::QueuedTurn(..)
                | Self::Predecessors(..)
                | Self::NextSeq(..)
                | Self::ResultText(..)
                | Self::TerminalFacts(..)
                | Self::CloseResult(..)
                | Self::ClosingSessions(..)
                | Self::Terminated(..)
                | Self::Events(..)
                | Self::EventsPage(..)
                | Self::ListPage(..)
                | Self::EvidenceRefs(..)
                | Self::Status(..)
                | Self::Authenticate(..)
                | Self::Unfinished(..)
                | Self::AnchorOwners(..)
                | Self::UnprovenAnchors(..)
                | Self::AnchorCohort(..)
                | Self::QueuedTurns(..)
                | Self::AnchorRecords(..)
        )
    }

    /// The test-only seam that reports SQLite corruption on this read
    /// command alone (design §10); `None` for a mutation.
    #[cfg(feature = "test-failpoints")]
    fn corrupt_point(&self) -> Option<&'static str> {
        Some(match self {
            Self::SpawnKey(..) => "store.read.corrupt.spawn_key",
            Self::Operation(..) => "store.read.corrupt.operation",
            Self::KeyedOperation(..) => "store.read.corrupt.keyed_operation",
            Self::Snapshot(..) => "store.read.corrupt.snapshot",
            Self::QueuedTurn(..) => "store.read.corrupt.queued_turn",
            Self::Predecessors(..) => "store.read.corrupt.predecessors",
            Self::NextSeq(..) => "store.read.corrupt.next_seq",
            Self::ResultText(..) => "store.read.corrupt.result",
            Self::TerminalFacts(..) => "store.read.corrupt.terminal_facts",
            Self::CloseResult(..) => "store.read.corrupt.close_result",
            Self::ClosingSessions(..) => "store.read.corrupt.closing_sessions",
            Self::Terminated(..) => "store.read.corrupt.terminated",
            Self::Events(..) => "store.read.corrupt.events",
            Self::EventsPage(..) => "store.read.corrupt.events_page",
            Self::ListPage(..) => "store.read.corrupt.list",
            Self::EvidenceRefs(..) => "store.read.corrupt.logs",
            Self::Status(..) => "store.read.corrupt.status",
            Self::Authenticate(..) => "store.read.corrupt.authenticate",
            Self::Unfinished(..) => "store.read.corrupt.unfinished",
            Self::AnchorOwners(..) => "store.read.corrupt.anchor_owners",
            Self::UnprovenAnchors(..) => "store.read.corrupt.unproven_anchors",
            Self::AnchorCohort(..) => "store.read.corrupt.anchor_cohort",
            Self::QueuedTurns(..) => "store.read.corrupt.queued_turns",
            Self::AnchorRecords(..) => "store.read.corrupt.anchor_records",
            Self::Spawn(..)
            | Self::Resume(..)
            | Self::Submission(..)
            | Self::Acceptance(..)
            | Self::Event(..)
            | Self::SessionEvent(..)
            | Self::Steps(..)
            | Self::Terminal(..)
            | Self::Closing(..)
            | Self::Closed(..)
            | Self::SubmitFailed(..)
            | Self::FailureResolution(..)
            | Self::ClosingTerminal(..)
            | Self::SessionClosed(..)
            | Self::AnchorIntent(..)
            | Self::AnchorIdentified(..)
            | Self::ArmIntent(..)
            | Self::VendorFacts(..)
            | Self::GroupAbsence(..)
            | Self::VerifyBlobs(..)
            | Self::SweepBlobs(..) => return None,
        })
    }
}

/// Test-only seams on a read the worker dequeued (design §6.7, §7.1, §7.3):
/// `store.read.stall` pauses the worker; `store.sqlite.corrupt` reports
/// corruption on every read, and `store.read.corrupt.<command>` only on
/// that read command ([`Command::corrupt_point`]); `store.read.dispatch`
/// fails the dispatcher's head reads (predecessors, the queued row, the
/// head's next sequence) and `store.read.queued_turn` only the queued-row
/// read.
#[cfg(feature = "test-failpoints")]
fn read_seams(command: &Command) -> Result<(), StoreError> {
    use crate::failpoint::hit;
    let injected = |error: std::io::Error| StoreError::Write(error.to_string());
    let corrupt = |error: std::io::Error| StoreError::Corrupt(error.to_string());
    hit("store.read.stall").map_err(injected)?;
    hit("store.sqlite.corrupt").map_err(corrupt)?;
    if let Some(point) = command.corrupt_point() {
        hit(point).map_err(corrupt)?;
    }
    if matches!(
        command,
        Command::Predecessors(..) | Command::QueuedTurn(..) | Command::NextSeq(..)
    ) {
        hit("store.read.dispatch").map_err(injected)?;
    }
    if matches!(command, Command::QueuedTurn(..)) {
        hit("store.read.queued_turn").map_err(injected)?;
    }
    Ok(())
}

/// Serves a read command; returns any other command unserved. Every read
/// reply, a test seam's failure included, passes [`answer`], so SQLite
/// corruption on any read reaches the observer before the reply.
fn serve_read(conn: &Connection, command: Command, corruption: &ReadCorruption) -> Option<Command> {
    if !command.is_read() {
        return Some(command);
    }
    // A seam's failure is the read's reply, through the same `answer`.
    #[cfg(feature = "test-failpoints")]
    let seam = read_seams(&command);
    #[cfg(not(feature = "test-failpoints"))]
    let seam: Result<(), StoreError> = Ok(());
    // Every reply below passes `answer`, after the seam.
    macro_rules! reply {
        ($reply:expr, $read:expr) => {{
            answer(corruption, $reply, seam.and_then(|()| $read));
        }};
    }
    match command {
        Command::SpawnKey(key, reply) => reply!(reply, read_spawn_key(conn, &key)),
        Command::Operation(session, op_key, reply) => {
            reply!(reply, read_operation(conn, &session, &op_key));
        }
        Command::KeyedOperation(session, op_key, reply) => {
            reply!(reply, read_keyed_operation(conn, &session, &op_key));
        }
        Command::Snapshot(session, reply) => reply!(reply, read_snapshot(conn, &session)),
        Command::QueuedTurn(session, turn, reply) => {
            reply!(reply, read_queued_turn(conn, &session, turn));
        }
        Command::Predecessors(session, turn, reply) => {
            reply!(reply, read_predecessors(conn, &session, turn));
        }
        Command::NextSeq(session, reply) => reply!(reply, read_next_seq(conn, &session)),
        Command::ResultText(session, turn, reply) => {
            reply!(reply, read_result_text(conn, &session, turn));
        }
        Command::TerminalFacts(session, turn, reply) => {
            reply!(reply, read_terminal_facts(conn, &session, turn));
        }
        Command::CloseResult(session, reply) => reply!(reply, read_close_result(conn, &session)),
        Command::ClosingSessions(after, limit, reply) => {
            reply!(reply, read_closing_sessions(conn, after.as_ref(), limit));
        }
        Command::Terminated(turns, reply) => reply!(reply, read_terminated(conn, turns)),
        Command::Events(session, from, limit, reply) => {
            reply!(reply, read_events(conn, &session, from, limit));
        }
        Command::EventsPage(query, reply) => reply!(reply, read_events_page(conn, &query)),
        Command::ListPage(query, reply) => reply!(reply, read_list_page(conn, &query)),
        Command::EvidenceRefs(session, turn, reply) => {
            reply!(reply, read_evidence_refs(conn, &session, turn));
        }
        Command::Status(query, reply) => reply!(reply, read_session_status(conn, &query)),
        Command::Authenticate(session, hash, reply) => {
            reply!(reply, authenticate(conn, &session, &hash));
        }
        Command::Unfinished(reply) => reply!(reply, read_unfinished(conn)),
        Command::AnchorOwners(after, limit, cohort, reply) => reply!(
            reply,
            read_anchor_owners(conn, after.as_deref(), limit, cohort)
        ),
        Command::UnprovenAnchors(after, limit, cohort, reply) => reply!(
            reply,
            count_unproven_anchors(conn, after.as_deref(), limit, cohort)
        ),
        Command::AnchorCohort(reply) => reply!(reply, read_anchor_cohort(conn)),
        Command::QueuedTurns(after, limit, reply) => {
            reply!(reply, read_queued_turns(conn, after.as_ref(), limit));
        }
        Command::AnchorRecords(query, reply) => {
            let records = seam.and_then(|()| read_anchor_records(conn, &query));
            if matches!(records, Err(StoreError::Corrupt(_))) {
                corruption.report();
            }
            let _ = reply.send(records.map_err(|error| error.kind()));
        }
        // `is_read` returned every other command above.
        command @ (Command::Spawn(..)
        | Command::Resume(..)
        | Command::Submission(..)
        | Command::Acceptance(..)
        | Command::Event(..)
        | Command::SessionEvent(..)
        | Command::Steps(..)
        | Command::Terminal(..)
        | Command::ClosingTerminal(..)
        | Command::SessionClosed(..)
        | Command::Closing(..)
        | Command::Closed(..)
        | Command::SubmitFailed(..)
        | Command::FailureResolution(..)
        | Command::AnchorIntent(..)
        | Command::AnchorIdentified(..)
        | Command::ArmIntent(..)
        | Command::VendorFacts(..)
        | Command::GroupAbsence(..)
        | Command::VerifyBlobs(..)
        | Command::SweepBlobs(..)) => return Some(command),
    }
    None
}

/// Sends a read reply; SQLite corruption reaches the observer first
/// (design §7.1).
fn answer<T>(
    corruption: &ReadCorruption,
    reply: oneshot::Sender<Result<T, StoreError>>,
    result: Result<T, StoreError>,
) {
    if matches!(result, Err(StoreError::Corrupt(_))) {
        corruption.report();
    }
    let _ = reply.send(result);
}

/// Serves one mutation command, or a blob check the thread runs before
/// admission. Every mutation's result passes [`settled`] before its reply.
fn serve_write(conn: &mut Connection, command: Command, blobs: &Blobs) {
    if let Command::VerifyBlobs(reply) = command {
        let _ = reply.send(verify_blobs(conn, blobs));
        return;
    }
    if let Command::SweepBlobs(reply) = command {
        let _ = reply.send(sweep_blobs(conn, blobs));
        return;
    }
    #[cfg(feature = "test-failpoints")]
    let full = full_seam(conn);
    serve_mutation(conn, command);
    #[cfg(feature = "test-failpoints")]
    if full {
        let _ = conn.pragma_update(None, "max_page_count", 4_294_967_294_i64);
    }
}

/// Test builds: `fail_io` at `store.sqlite.full` caps the database at its
/// current size for the next mutation, so a transaction that needs a new
/// page meets a real `SQLITE_FULL` (design §5.3, §13.2). True when armed.
#[cfg(feature = "test-failpoints")]
fn full_seam(conn: &Connection) -> bool {
    if crate::failpoint::hit("store.sqlite.full").is_ok() {
        return false;
    }
    let Ok(pages) = conn.pragma_query_value(None, "page_count", |row| row.get::<_, i64>(0)) else {
        return false;
    };
    conn.pragma_update(None, "max_page_count", pages).is_ok()
}

/// Serves one mutation command; its reply follows [`settled`].
fn serve_mutation(conn: &mut Connection, command: Command) {
    macro_rules! reply {
        ($reply:expr, $result:expr) => {{
            let result = $result;
            send_commit($reply, settled(conn, result));
        }};
    }
    macro_rules! journal {
        ($reply:expr, $result:expr) => {{
            let result = $result;
            let _ = $reply.send(as_commit(settled(conn, result)));
        }};
    }
    match command {
        Command::Spawn(record, key, reply) => reply!(reply, commit_spawn(conn, record, key)),
        Command::Resume(record, reply) => reply!(reply, commit_resume(conn, &record)),
        Command::Submission(record, reply) => reply!(reply, commit_submission(conn, &record)),
        Command::Acceptance(record, reply) => reply!(reply, commit_acceptance(conn, &record)),
        Command::Event(record, reply) => reply!(reply, commit_event(conn, &record)),
        Command::SessionEvent(record, reply) => {
            reply!(reply, commit_session_event(conn, &record));
        }
        Command::Steps(record, reply) => reply!(reply, commit_steps(conn, &record)),
        Command::Terminal(record, extras, reply) => {
            reply!(
                reply,
                commit_terminal(conn, &record, &extras, None).map(drop)
            );
        }
        Command::SessionClosed(session, closed, reply) => {
            reply!(reply, commit_session_closed(conn, &session, &closed));
        }
        Command::ClosingTerminal(record, closed, reply) => {
            let extras = TerminalExtras::default();
            reply!(
                reply,
                commit_terminal(conn, &record, &extras, Some(&closed))
            );
        }
        Command::Closing(record, reply) => reply!(reply, commit_closing(conn, &record)),
        Command::Closed(record, reply) => reply!(reply, commit_closed(conn, &record)),
        Command::SubmitFailed(record, reply) => reply!(reply, commit_submit_failed(conn, &record)),
        Command::FailureResolution(record, reply) => {
            reply!(reply, commit_failure_resolution(conn, &record));
        }
        Command::AnchorIntent(intent, reply) => {
            journal!(reply, commit_anchor_intent(conn, &intent));
        }
        Command::AnchorIdentified(id, generation, version, identity, reply) => {
            journal!(
                reply,
                commit_anchor_identified(conn, &id, &generation, version, &identity)
            );
        }
        Command::ArmIntent(id, generation, version, reply) => {
            journal!(reply, commit_arm_intent(conn, &id, &generation, version));
        }
        Command::VendorFacts(id, generation, pid, reply) => {
            journal!(reply, commit_vendor_facts(conn, &id, &generation, pid));
        }
        Command::GroupAbsence(proof, reply) => journal!(reply, commit_group_absence(conn, &proof)),
        // `writer_loop` serves reads first and `serve_write` the blob checks.
        Command::SpawnKey(..)
        | Command::Operation(..)
        | Command::KeyedOperation(..)
        | Command::Snapshot(..)
        | Command::QueuedTurn(..)
        | Command::NextSeq(..)
        | Command::Predecessors(..)
        | Command::ResultText(..)
        | Command::TerminalFacts(..)
        | Command::CloseResult(..)
        | Command::ClosingSessions(..)
        | Command::Terminated(..)
        | Command::Events(..)
        | Command::EventsPage(..)
        | Command::ListPage(..)
        | Command::EvidenceRefs(..)
        | Command::Status(..)
        | Command::Authenticate(..)
        | Command::Unfinished(..)
        | Command::AnchorOwners(..)
        | Command::UnprovenAnchors(..)
        | Command::AnchorCohort(..)
        | Command::QueuedTurns(..)
        | Command::AnchorRecords(..)
        | Command::VerifyBlobs(..)
        | Command::SweepBlobs(..) => {}
    }
}

/// Every blob a row names (design §6.5 recovery).
fn referenced_blobs(conn: &Connection) -> Result<Vec<BlobRef>, StoreError> {
    let mut statement = conn
        .prepare("SELECT prompt_blob FROM turns WHERE prompt_blob IS NOT NULL")
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sql_error)?;
    rows.map(|row| {
        let stored = row.map_err(sql_error)?;
        BlobRef::decode(&stored).map_err(|_| StoreError::Corrupt("blob reference".to_owned()))
    })
    .collect()
}

/// Checks every referenced blob: a regular file of its recorded length and
/// SHA-256, else `Corrupt`.
fn verify_blobs(conn: &Connection, blobs: &Blobs) -> Result<(), StoreError> {
    for blob in referenced_blobs(conn)? {
        blobs.verify(&blob)?;
    }
    Ok(())
}

/// Unlinks every blob file no row names.
fn sweep_blobs(conn: &Connection, blobs: &Blobs) -> Result<u64, StoreError> {
    let referenced = referenced_blobs(conn)?
        .into_iter()
        .map(|blob| blob.id().to_owned())
        .collect();
    blobs.sweep(&referenced)
}

/// Replies to a Core lifecycle mutation after its transaction ended. The
/// test-only `store.commit.reply_lost` point can lose that reply: the caller
/// then learns nothing about a mutation that may be durable.
fn send_commit<T>(reply: oneshot::Sender<T>, result: T) {
    #[cfg(feature = "test-failpoints")]
    if crate::failpoint::hit("store.commit.reply_lost").is_err() {
        return;
    }
    let _ = reply.send(result);
}

fn as_commit<T>(result: Result<T, StoreError>) -> CommitOutcome<T> {
    match result {
        Ok(value) => CommitOutcome::Committed(value),
        Err(error) => error.journal_outcome(),
    }
}

fn seq(event: &Value) -> Result<u64, StoreError> {
    event
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or(StoreError::Constraint("event sequence missing"))
}

fn json(value: &Value) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::Write(error.to_string()))
}

/// A stored identity length; one past `i64` cannot be stored.
fn identity_len(identity: &Identity) -> Result<i64, StoreError> {
    i64::try_from(identity.len).map_err(|_| StoreError::Constraint("identity too long"))
}

/// Reads back a stored identity pair.
fn stored_identity(len: i64, sha256: &[u8]) -> Result<Identity, StoreError> {
    Ok(Identity {
        len: u64::try_from(len).map_err(|_| StoreError::CorruptEvidence)?,
        sha256: sha256.try_into().map_err(|_| StoreError::CorruptEvidence)?,
    })
}

/// Parses an event's `at`, strictly `YYYY-MM-DDTHH:MM:SS.mmmZ` as Core
/// writes it, to Unix milliseconds (design §6.6); anything else is
/// `Constraint`.
pub(super) fn at_ms(at: &str) -> Result<i64, StoreError> {
    const BAD: StoreError = StoreError::Constraint("event time is not RFC 3339 UTC milliseconds");
    let bytes = at.as_bytes();
    if bytes.len() != 24
        || [
            (4, b'-'),
            (7, b'-'),
            (10, b'T'),
            (13, b':'),
            (16, b':'),
            (19, b'.'),
            (23, b'Z'),
        ]
        .iter()
        .any(|&(index, byte)| bytes[index] != byte)
    {
        return Err(BAD);
    }
    let number = |from: usize, to: usize| -> Result<i64, StoreError> {
        let digits = &bytes[from..to];
        if !digits.iter().all(u8::is_ascii_digit) {
            return Err(BAD);
        }
        Ok(digits
            .iter()
            .fold(0_i64, |value, digit| value * 10 + i64::from(digit - b'0')))
    };
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second, milli) = (
        number(11, 13)?,
        number(14, 16)?,
        number(17, 19)?,
        number(20, 23)?,
    );
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(BAD),
    };
    if !(1..=month_days).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return Err(BAD);
    }
    // Days from civil (H. Hinnant), the inverse of Core's `rfc3339`.
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Ok(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + milli)
}

fn commit_spawn(
    conn: &mut Connection,
    record: SpawnRecord,
    key: Option<SpawnKey>,
) -> Result<ReceiptRecord, StoreError> {
    if seq(&record.initial_event)? != 1 {
        return Err(StoreError::Constraint("initial event sequence must be one"));
    }
    let receipt = json(&record.receipt)?;
    let params_json = json(&record.params)?;
    let effective = json(&record.effective)?;
    let harness = record
        .params
        .get("harness")
        .and_then(Value::as_str)
        .ok_or(StoreError::Constraint("frozen params name no harness"))?;
    let queued_at = event_at(&record.initial_event)?;
    let created = at_ms(queued_at)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    // Design §6.6: the session's creation ordinal, from a counter that is
    // never decremented, so no ordinal is ever reused.
    let ord: i64 = tx
        .query_row(
            "UPDATE session_ord SET next=next+1 WHERE only=1 RETURNING next",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    tx.execute(
        "INSERT INTO sessions(id,handle_hash,receipt,params,state,next_seq,created_ms,updated_ms,harness,ord,label)
         VALUES (?1,?2,?3,?4,'active',2,?5,?5,?6,?7,?8)",
        params![
            record.session_id.as_str(),
            &record.handle_hash[..],
            receipt,
            params_json,
            created,
            harness,
            ord,
            record.label
        ],
    )
    .map_err(sql_error)?;
    let (prompt, prompt_blob) = prompt_columns(&record.prompt);
    tx.execute(
        "INSERT INTO turns(session_id,number,prompt,prompt_blob,effective,state,queued_at,queued_seq) VALUES (?1,1,?2,?3,?4,'queued',?5,1)",
        params![record.session_id.as_str(), prompt, prompt_blob, effective, queued_at],
    )
    .map_err(sql_error)?;
    insert_event_row(&tx, &record.session_id, 1, &record.initial_event)?;
    if let Some(key) = key {
        tx.execute(
            "INSERT INTO spawn_keys(key,session_id,identity_len,identity_sha256,receipt) VALUES (?1,?2,?3,?4,?5)",
            params![
                key.key,
                record.session_id.as_str(),
                identity_len(&key.identity)?,
                &key.identity.sha256[..],
                receipt
            ],
        )
        .map_err(sql_error)?;
    }
    // Every row is written but uncommitted: none may survive a crash here.
    #[cfg(feature = "test-failpoints")]
    crate::failpoint::hit("store.spawn.before_commit")
        .map_err(|error| StoreError::Write(error.to_string()))?;
    before_commit!("store.commit.receipt");
    commit(tx)?;
    // Committed but unacknowledged: all rows survive together.
    #[cfg(feature = "test-failpoints")]
    crate::failpoint::hit("store.spawn.after_commit")
        .map_err(|error| StoreError::Uncertain(error.to_string()))?;
    Ok(ReceiptRecord {
        receipt: record.receipt,
    })
}

/// A prompt's `turns.prompt` and `turns.prompt_blob`: exactly one is set.
fn prompt_columns(prompt: &Prompt) -> (Option<&str>, Option<String>) {
    match prompt {
        Prompt::Inline(text) => (Some(text), None),
        Prompt::Blob(blob) => (None, Some(blob.encode())),
    }
}

fn read_spawn_key(conn: &Connection, key: &str) -> Result<Option<StoredSpawnKey>, StoreError> {
    let row: Option<(String, i64, Vec<u8>, String)> = conn
        .query_row(
            "SELECT session_id,identity_len,identity_sha256,receipt FROM spawn_keys WHERE key=?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(sql_error)?;
    row.map(|(session, len, sha256, receipt)| {
        Ok(StoredSpawnKey {
            session_id: SessionId::try_from(session.as_str())
                .map_err(|_| StoreError::CorruptEvidence)?,
            identity: stored_identity(len, &sha256)?,
            receipt: serde_json::from_str(&receipt).map_err(|_| StoreError::CorruptEvidence)?,
        })
    })
    .transpose()
}

/// Commits a queued turn at the session's next number with its frozen
/// effective values, its `turn.queued` event and, when keyed, the `op_key`
/// result, in one transaction. Checking that the turn is the session's next
/// inside the transaction also proves that the latest turn Core inherited
/// from under admission is still the latest (C1 P5).
fn commit_resume(conn: &mut Connection, record: &ResumeRecord) -> Result<(), StoreError> {
    let session = &record.session_id;
    let queued_at = event_at(&record.event)?;
    let queued_seq = i64::try_from(seq(&record.event)?).map_err(|_| StoreError::CorruptEvidence)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let (state, admission, turns, queued): (String, String, u32, u32) = tx
        .query_row(
            "SELECT state,admission,(SELECT coalesce(max(number),0) FROM turns WHERE session_id=?1),
                (SELECT count(*) FROM turns WHERE session_id=?1 AND state='queued')
             FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(sql_error)?
        .ok_or(StoreError::Constraint("session does not exist"))?;
    if state == "closed" {
        return Err(StoreError::Constraint("session is closed"));
    }
    // Design §4: a closing session admits no turn; a refusal, not a failure.
    if admission == "closing" {
        return Err(StoreError::Refused("session is closing"));
    }
    if record.turn.get() != turns + 1 {
        return Err(StoreError::Constraint("turn is not the session's next"));
    }
    if queued >= SESSION_QUEUE_LIMIT {
        return Err(StoreError::Constraint("session queue is full"));
    }
    let (prompt, prompt_blob) = prompt_columns(&record.prompt);
    tx.execute(
        "INSERT INTO turns(session_id,number,prompt,prompt_blob,effective,state,queued_at,queued_seq) VALUES (?1,?2,?3,?4,?5,'queued',?6,?7)",
        params![
            session.as_str(),
            record.turn.get(),
            prompt,
            prompt_blob,
            json(&record.effective)?,
            queued_at,
            queued_seq
        ],
    )
    .map_err(sql_error)?;
    insert_event(&tx, session, &record.event)?;
    if let Some(operation) = &record.operation {
        tx.execute(
            "INSERT INTO operations(session_id,op_key,verb,identity_len,identity_sha256,turn,result) VALUES (?1,?2,'resume',?3,?4,?5,?6)",
            params![
                session.as_str(),
                operation.op_key,
                identity_len(&operation.identity)?,
                &operation.identity.sha256[..],
                record.turn.get(),
                json(&operation.result)?
            ],
        )
        .map_err(sql_error)?;
    }
    tx.execute(
        "UPDATE sessions SET state='active' WHERE id=?1",
        [session.as_str()],
    )
    .map_err(sql_error)?;
    before_commit!("store.commit.receipt");
    commit(tx)
}

fn read_operation(
    conn: &Connection,
    session: &SessionId,
    op_key: &str,
) -> Result<Option<OperationRecord>, StoreError> {
    Ok(
        read_keyed_operation(conn, session, op_key)?.map(|operation| OperationRecord {
            op_key: op_key.to_owned(),
            identity: operation.identity,
            // A close still in progress has no result yet (design §4).
            result: operation.result.unwrap_or(Value::Null),
        }),
    )
}

fn read_keyed_operation(
    conn: &Connection,
    session: &SessionId,
    op_key: &str,
) -> Result<Option<KeyedOperation>, StoreError> {
    let row: Option<(String, i64, Vec<u8>, Option<String>)> = conn
        .query_row(
            "SELECT verb,identity_len,identity_sha256,result FROM operations WHERE session_id=?1 AND op_key=?2",
            params![session.as_str(), op_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(sql_error)?;
    row.map(|(verb, len, sha256, result)| {
        Ok(KeyedOperation {
            verb: match verb.as_str() {
                "resume" => OperationVerb::Resume,
                "close" => OperationVerb::Close,
                _ => return Err(StoreError::CorruptEvidence),
            },
            identity: stored_identity(len, &sha256)?,
            result: result
                .map(|result| serde_json::from_str(&result))
                .transpose()
                .map_err(|_| StoreError::CorruptEvidence)?,
        })
    })
    .transpose()
}

/// The selected columns of a session's [`SessionRoute`], for a query whose
/// row is `sessions` (decision H3): the adapter version the latest
/// `turn.started` commit persisted, else the receipt's (runtime §6), with
/// the confirmed identity its `session.opened`/`session.reopened` wrote.
const ROUTE_COLUMNS: &str = "harness,json_extract(receipt,'$.route'),
    coalesce(adapter_version,json_extract(receipt,'$.adapter_version')),
    vendor_session_id,transcript_hint";

/// The route identity read at `first` and the four columns after it.
fn route_at(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<SessionRoute> {
    Ok(SessionRoute {
        harness: row.get(first)?,
        route: row.get(first + 1)?,
        adapter_version: row.get(first + 2)?,
        vendor_session_id: row.get(first + 3)?,
        transcript: row.get(first + 4)?,
    })
}

fn read_snapshot(
    conn: &Connection,
    session: &SessionId,
) -> Result<Option<SessionSnapshot>, StoreError> {
    /// State, admission, turns, queued turns, latest effective, `cwd`,
    /// route identity.
    type Row = (
        String,
        String,
        u32,
        u32,
        Option<String>,
        Option<String>,
        SessionRoute,
    );
    let row: Option<Row> = conn
        .query_row(
            &format!(
                "SELECT state,admission,
                    (SELECT coalesce(max(number),0) FROM turns WHERE session_id=?1),
                    (SELECT count(*) FROM turns WHERE session_id=?1 AND state='queued'),
                    (SELECT effective FROM turns WHERE session_id=?1 ORDER BY number DESC LIMIT 1),
                    json_extract(params,'$.cwd'),{ROUTE_COLUMNS}
                 FROM sessions WHERE id=?1"
            ),
            [session.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    route_at(row, 6)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    row.map(|(state, admission, turns, queued, latest, cwd, route)| {
        Ok(SessionSnapshot {
            closed: state == "closed",
            closing: admission == "closing",
            turns,
            queued,
            cwd,
            route,
            latest_effective: latest
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .map_err(|_| StoreError::CorruptEvidence)?,
        })
    })
    .transpose()
}

/// One page of durable `queued` turns in `(session, turn)` order after `after`.
fn read_queued_turns(
    conn: &Connection,
    after: Option<&(SessionId, TurnNumber)>,
    limit: u32,
) -> Result<Vec<(SessionId, TurnNumber)>, StoreError> {
    let (session, number) = after.map_or((None, 0), |(session, turn)| {
        (Some(session.as_str()), turn.get())
    });
    let mut query = conn
        .prepare_cached(
            "SELECT session_id,number FROM turns WHERE state='queued'
             AND (?1 IS NULL OR session_id>?1 OR (session_id=?1 AND number>?2))
             ORDER BY session_id,number LIMIT ?3",
        )
        .map_err(sql_error)?;
    let rows = query
        .query_map(params![session, number, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
        })
        .map_err(sql_error)?;
    let mut turns = Vec::new();
    for row in rows {
        let (session, number) = row.map_err(sql_error)?;
        turns.push((
            SessionId::try_from(session.as_str()).map_err(|_| StoreError::CorruptEvidence)?,
            TurnNumber::try_from(number).map_err(|_| StoreError::CorruptEvidence)?,
        ));
    }
    Ok(turns)
}

fn read_queued_turn(
    conn: &Connection,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<Option<QueuedTurn>, StoreError> {
    /// Prompt, prompt blob, effective values, `queued_at`, `queued_seq`,
    /// the session's frozen `cwd` and route identity.
    type Row = (
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        i64,
        Option<String>,
        SessionRoute,
    );
    let row: Option<Row> = conn
        .query_row(
            &format!(
                "SELECT t.prompt,t.prompt_blob,t.effective,t.queued_at,t.queued_seq,
                        json_extract(s.params,'$.cwd'),{ROUTE_COLUMNS}
                 FROM turns t JOIN sessions s ON s.id=t.session_id
                 WHERE t.session_id=?1 AND t.number=?2 AND t.state='queued'"
            ),
            params![session.as_str(), turn.get()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    route_at(row, 6)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    row.map(
        |(prompt, blob, effective, queued_at, queued_seq, cwd, route)| {
            let prompt = match (prompt, blob) {
                (Some(text), None) => Prompt::Inline(text),
                (None, Some(blob)) => Prompt::Blob(BlobRef::decode(&blob)?),
                _ => return Err(StoreError::CorruptEvidence),
            };
            Ok(QueuedTurn {
                prompt,
                cwd,
                effective: serde_json::from_str(&effective)
                    .map_err(|_| StoreError::CorruptEvidence)?,
                queued_at: queued_at.ok_or(StoreError::CorruptEvidence)?,
                queued_seq: u64::try_from(queued_seq).map_err(|_| StoreError::CorruptEvidence)?,
                route,
            })
        },
    )
    .transpose()
}

fn read_predecessors(
    conn: &Connection,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<Predecessors, StoreError> {
    let unresolved: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE session_id=?1 AND number<?2 AND state IN ('queued','running'))",
            params![session.as_str(), turn.get()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let latest: Option<u32> = conn
        .query_row(
            "SELECT number FROM turns WHERE session_id=?1 AND number<?2 AND submitted_at IS NOT NULL ORDER BY number DESC LIMIT 1",
            params![session.as_str(), turn.get()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let last_submitted = match latest {
        Some(number) => read_terminal_facts(
            conn,
            session,
            TurnNumber::try_from(number).map_err(|_| StoreError::CorruptEvidence)?,
        )?,
        None => None,
    };
    Ok(Predecessors {
        unresolved,
        last_submitted,
    })
}

fn read_next_seq(conn: &Connection, session: &SessionId) -> Result<Option<u64>, StoreError> {
    let next: Option<i64> = conn
        .query_row(
            "SELECT next_seq FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    next.map(|next| u64::try_from(next).map_err(|_| StoreError::CorruptEvidence))
        .transpose()
}

fn commit_submission(conn: &mut Connection, record: &SubmissionRecord) -> Result<(), StoreError> {
    let session = &record.session_id;
    let at = event_at(&record.event)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    // Design §7.1: the turn's evidence folder, relative to the State
    // directory, commits with `turn.submitted`.
    let changed = tx
        .execute(
            "UPDATE turns SET state='running',submitted_at=?3,evidence_dir=?4 WHERE session_id=?1 AND number=?2 AND state='queued'",
            params![
                session.as_str(),
                record.turn.get(),
                at,
                EvidenceRoot::relative(session, record.turn)
            ],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StoreError::Constraint("turn is not queued"));
    }
    insert_event(&tx, session, &record.event)?;
    before_commit!("store.commit.submission");
    commit(tx)
}

fn commit_acceptance(conn: &mut Connection, record: &AcceptanceRecord) -> Result<(), StoreError> {
    let session = &record.session_id;
    let turn = record.turn;
    let correlation = record.correlation.as_str();
    let at = event_at(&record.event)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let existing: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT state,correlation FROM turns WHERE session_id=?1 AND number=?2",
            params![session.as_str(), turn.get()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((state, recorded_correlation)) = existing else {
        return Err(StoreError::Constraint("turn does not exist"));
    };
    if recorded_correlation.as_deref() == Some(correlation) {
        return Ok(());
    }
    if state != "running" || recorded_correlation.is_some() {
        return Err(StoreError::Constraint(
            "acceptance phase or correlation mismatch",
        ));
    }
    // The vendor correlation and accepted_at stay as internal C2 evidence; the
    // public event is Core's canonical one.
    tx.execute(
        "UPDATE turns SET correlation=?3,accepted_at=?4 WHERE session_id=?1 AND number=?2",
        params![session.as_str(), turn.get(), correlation, at],
    )
    .map_err(sql_error)?;
    // C1 §3.3: the turn.started commit advances the session's recorded
    // adapter version to the running adapter's (decision H3).
    if let Some(version) = record.adapter_version.as_deref() {
        tx.execute(
            "UPDATE sessions SET adapter_version=?2 WHERE id=?1",
            params![session.as_str(), version],
        )
        .map_err(sql_error)?;
    }
    insert_event(&tx, session, &record.event)?;
    // Test builds: SQLite reports corruption on the acceptance write itself,
    // after its prerequisite read (design §7.1); the transaction rolls back.
    #[cfg(feature = "test-failpoints")]
    crate::failpoint::hit("store.commit.corrupt.acceptance")
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    before_commit!("store.commit.event");
    commit(tx)
}

fn event_at(event: &Value) -> Result<&str, StoreError> {
    event
        .get("at")
        .and_then(Value::as_str)
        .ok_or(StoreError::Constraint("event time missing"))
}

/// Appends one event at the session's dense next sequence inside `tx`.
fn insert_event(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionId,
    event: &Value,
) -> Result<(), StoreError> {
    let next: i64 = tx
        .query_row(
            "SELECT next_seq FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let next = u64::try_from(next).map_err(|_| StoreError::CorruptEvidence)?;
    if seq(event)? != next {
        return Err(StoreError::Constraint("event sequence is not the next one"));
    }
    insert_event_row(tx, session, next, event)?;
    tx.execute(
        "UPDATE sessions SET next_seq=next_seq+1 WHERE id=?1",
        [session.as_str()],
    )
    .map_err(sql_error)?;
    Ok(())
}

/// Writes event `seq` with its v6 `turn` and `type` columns, both from the
/// event document, and moves the session's `updated_ms` to the event's time
/// (design §6.6). Events are written in sequence order, so a transaction
/// leaves `updated_ms` at the time of its highest-`seq` event.
fn insert_event_row(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionId,
    seq: u64,
    event: &Value,
) -> Result<(), StoreError> {
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .ok_or(StoreError::Constraint("event type missing"))?;
    let turn = match event.get("turn") {
        None | Some(Value::Null) => None,
        Some(turn) => Some(
            turn.as_u64()
                .and_then(|turn| u32::try_from(turn).ok())
                .ok_or(StoreError::Constraint("event turn invalid"))?,
        ),
    };
    let at = at_ms(event_at(event)?)?;
    let seq = i64::try_from(seq).map_err(|_| StoreError::Constraint("event sequence too large"))?;
    tx.execute(
        "INSERT INTO events(session_id,seq,turn,type,event) VALUES (?1,?2,?3,?4,?5)",
        params![session.as_str(), seq, turn, kind, json(event)?],
    )
    .map_err(sql_error)?;
    tx.execute(
        "UPDATE sessions SET updated_ms=?2 WHERE id=?1",
        params![session.as_str(), at],
    )
    .map_err(sql_error)?;
    Ok(())
}

fn commit_event(conn: &mut Connection, record: &EventRecord) -> Result<(), StoreError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let state: Option<String> = tx
        .query_row(
            "SELECT state FROM turns WHERE session_id=?1 AND number=?2",
            params![record.session_id.as_str(), record.turn.get()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if state.as_deref() != Some("running") {
        return Err(StoreError::Constraint("turn is not running"));
    }
    insert_event(&tx, &record.session_id, &record.event)?;
    before_commit!("store.commit.event");
    commit(tx)
}

/// Commits a session-level event at the session's next sequence, with a
/// confirmed identity's columns when it carries one, unless the session is
/// closed (C2 §2, decision H3 as narrowed).
fn commit_session_event(
    conn: &mut Connection,
    record: &SessionEventRecord,
) -> Result<(), StoreError> {
    let session = &record.session_id;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let state: String = tx
        .query_row(
            "SELECT state FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?
        .ok_or(StoreError::Constraint("session does not exist"))?;
    if state == "closed" {
        return Err(StoreError::Refused("session is closed"));
    }
    if let Some(event) = &record.event {
        insert_event(&tx, session, event)?;
    }
    if let Some(identity) = &record.identity {
        tx.execute(
            "UPDATE sessions SET vendor_session_id=?2,transcript_hint=?3 WHERE id=?1",
            params![
                session.as_str(),
                identity.vendor_session_id,
                identity.transcript
            ],
        )
        .map_err(sql_error)?;
    }
    before_commit!("store.commit.session_event");
    commit(tx)
}

/// Commits completed step rows of a turn that is still running (design
/// §3.2); a terminal turn takes none.
fn commit_steps(conn: &mut Connection, record: &StepsRecord) -> Result<(), StoreError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    if turn_state(&tx, &record.session_id, record.turn)?.as_deref() != Some("running") {
        return Err(StoreError::Constraint("turn is not running"));
    }
    insert_steps(&tx, &record.session_id, record.turn, &record.rows)?;
    commit(tx)
}

/// Inserts step rows of `turn` (design §3.1).
fn insert_steps(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionId,
    turn: TurnNumber,
    rows: &[StepRow],
) -> Result<(), StoreError> {
    let mut insert = tx
        .prepare_cached(
            "INSERT INTO steps(session_id,turn,step,started_ms,ended_ms,tokens) VALUES (?1,?2,?3,?4,?5,?6)",
        )
        .map_err(sql_error)?;
    for row in rows {
        let tokens = row
            .tokens
            .map(i64::try_from)
            .transpose()
            .map_err(|_| StoreError::Constraint("step tokens too large"))?;
        insert
            .execute(params![
                session.as_str(),
                turn.get(),
                row.step,
                row.started_ms,
                row.ended_ms,
                tokens
            ])
            .map_err(sql_error)?;
    }
    Ok(())
}

/// Commits `session.closed` alone once every turn of the session has a
/// terminal, and marks the session closed. A session already closed or
/// holding a queued or running turn commits nothing and reports the close as
/// not written; that is not a Store failure.
fn commit_session_closed(
    conn: &mut Connection,
    session: &SessionId,
    closed: &Value,
) -> Result<bool, StoreError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let open: bool = tx
        .query_row(
            "SELECT state!='closed' FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if !open || unfinished_turns(&tx, session)? {
        return Ok(false);
    }
    insert_event(&tx, session, closed)?;
    tx.execute(
        "UPDATE sessions SET state='closed' WHERE id=?1",
        [session.as_str()],
    )
    .map_err(sql_error)?;
    before_commit!("store.commit.session_closed");
    commit(tx)?;
    Ok(true)
}

/// Commits the terminal and, when `closed` is given, the session's
/// `session.closed` event after it, in the same transaction. `extras` adds
/// the turn's `cancel_cause` (design §10).
fn commit_terminal(
    conn: &mut Connection,
    record: &TerminalRecord,
    extras: &TerminalExtras,
    closed: Option<&Value>,
) -> Result<bool, StoreError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    // A queued turn can only be cancelled: that is `store.commit.cancel`.
    #[cfg(feature = "test-failpoints")]
    let queued = turn_state(&tx, &record.session_id, record.turn)?.as_deref() == Some("queued");
    let closed = insert_terminal(&tx, record, extras, closed)?;
    // Test builds: SQLite reports corruption on the terminal write itself
    // (design §7.1); the transaction rolls back.
    #[cfg(feature = "test-failpoints")]
    crate::failpoint::hit("store.commit.corrupt.terminal")
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    before_commit!(if queued {
        "store.commit.cancel"
    } else {
        "store.commit.terminal"
    });
    commit(tx)?;
    Ok(closed)
}

fn turn_state(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<Option<String>, StoreError> {
    tx.query_row(
        "SELECT state FROM turns WHERE session_id=?1 AND number=?2",
        params![session.as_str(), turn.get()],
        |row| row.get(0),
    )
    .optional()
    .map_err(sql_error)
}

/// Writes one terminal inside `tx`: the turn's state, envelope,
/// `cancel_cause` and `ended_seq` (the sequence of its `turn.ended`, design
/// §6.6), `turn.ended` and, with `closed`, `session.closed` when no other
/// turn of the session is queued or running. Returns whether the close was
/// written.
fn insert_terminal(
    tx: &rusqlite::Transaction<'_>,
    record: &TerminalRecord,
    extras: &TerminalExtras,
    closed: Option<&Value>,
) -> Result<bool, StoreError> {
    let state = record
        .envelope
        .get("state")
        .and_then(Value::as_str)
        .ok_or(StoreError::Constraint("terminal state missing"))?;
    if !matches!(state, "completed" | "failed" | "cancelled" | "unknown") {
        return Err(StoreError::Constraint("terminal state invalid"));
    }
    let envelope = json(&record.envelope)?;
    let ended_seq = i64::try_from(seq(&record.event)?)
        .map_err(|_| StoreError::Constraint("event sequence too large"))?;
    let changed = tx
        .execute(
            "UPDATE turns SET state=?3,envelope=?4,cancel_cause=?5,ended_seq=?6 WHERE session_id=?1 AND number=?2 AND (?3!='completed' OR correlation IS NOT NULL) AND (state='running' OR (state='queued' AND ?3='cancelled'))",
            params![
                record.session_id.as_str(),
                record.turn.get(),
                state,
                envelope,
                extras.cancel_cause.map(CancelCause::as_str),
                ended_seq
            ],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StoreError::Constraint("turn is not running"));
    }
    insert_event(tx, &record.session_id, &record.event)?;
    insert_steps(tx, &record.session_id, record.turn, &record.steps)?;
    // `session.closed` only when no other turn of the session is queued or
    // running (this turn is already terminal); otherwise the terminal commits
    // alone and the close is reported as not written.
    let closed = match closed {
        Some(closed) => {
            if unfinished_turns(tx, &record.session_id)? {
                false
            } else {
                insert_event(tx, &record.session_id, closed)?;
                // Design §10 [r3.6]: the rider seam fails the combined
                // transaction after the terminal and the close are inserted,
                // before `COMMIT`; a terminal committing alone never reaches it.
                before_commit!("store.commit.rider");
                true
            }
        }
        None => false,
    };
    // C1 §7.1: a session with queued or running work stays active; closed is final.
    update_session_state(tx, &record.session_id, closed)?;
    Ok(closed)
}

/// Recomputes a session's state after a terminal; `closed` makes it final.
fn update_session_state(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionId,
    closed: bool,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE sessions SET state=CASE
            WHEN ?2 OR state='closed' THEN 'closed'
            WHEN EXISTS(SELECT 1 FROM turns WHERE session_id=?1 AND state IN ('queued','running')) THEN 'active'
            ELSE 'idle' END WHERE id=?1",
        params![session.as_str(), closed],
    )
    .map_err(sql_error)?;
    Ok(())
}

/// Design §4 step 6: the `closing` gate and a keyed close's intent row. A
/// closed session is refused; a session already closing only gains the row.
fn commit_closing(conn: &mut Connection, record: &ClosingRecord) -> Result<(), StoreError> {
    let session = &record.session_id;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let state: String = tx
        .query_row(
            "SELECT state FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?
        .ok_or(StoreError::Constraint("session does not exist"))?;
    if state == "closed" {
        return Err(StoreError::Refused("session is closed"));
    }
    tx.execute(
        "UPDATE sessions SET admission='closing' WHERE id=?1",
        [session.as_str()],
    )
    .map_err(sql_error)?;
    if let Some(operation) = &record.operation {
        tx.execute(
            "INSERT INTO operations(session_id,op_key,verb,identity_len,identity_sha256,turn,result) VALUES (?1,?2,'close',?3,?4,NULL,NULL)",
            params![
                session.as_str(),
                operation.op_key,
                identity_len(&operation.identity)?,
                &operation.identity.sha256[..]
            ],
        )
        .map_err(sql_error)?;
    }
    before_commit!("store.commit.closing");
    commit(tx)
}

/// Design §4 dispatcher step 5: `Closed` with the close result derived in
/// the transaction. Refused while a turn of the session is unfinished.
fn commit_closed(
    conn: &mut Connection,
    record: &ClosedRecord,
) -> Result<ClosedOutcome, StoreError> {
    let session = &record.session_id;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let (state, admission): (String, String) = tx
        .query_row(
            "SELECT state,admission FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?
        .ok_or(StoreError::Constraint("session does not exist"))?;
    if state == "closed" {
        return Err(StoreError::Refused("session is closed"));
    }
    if admission != "closing" {
        return Err(StoreError::Refused("session is not closing"));
    }
    if unfinished_turns(&tx, session)? {
        return Ok(ClosedOutcome::Unfinished);
    }
    insert_event(&tx, session, &record.event)?;
    let result = derive_close_result(&tx, session)?;
    let encoded = json(&result)?;
    tx.execute(
        "UPDATE sessions SET state='closed',close_result=?2 WHERE id=?1",
        params![session.as_str(), encoded],
    )
    .map_err(sql_error)?;
    if let Some(operation) = &record.operation {
        // The intent row `Closing` wrote, or a new one for a key first seen
        // on a session already closing.
        let len = identity_len(&operation.identity)?;
        let sha256 = &operation.identity.sha256[..];
        let completed = tx
            .execute(
                "UPDATE operations SET result=?5 WHERE session_id=?1 AND op_key=?2 AND verb='close' AND identity_len=?3 AND identity_sha256=?4 AND result IS NULL",
                params![session.as_str(), operation.op_key, len, sha256, encoded],
            )
            .map_err(sql_error)?;
        if completed == 0 {
            tx.execute(
                "INSERT INTO operations(session_id,op_key,verb,identity_len,identity_sha256,turn,result) VALUES (?1,?2,'close',?3,?4,NULL,?5)",
                params![session.as_str(), operation.op_key, len, sha256, encoded],
            )
            .map_err(sql_error)?;
        }
    }
    before_commit!("store.commit.closed");
    commit(tx)?;
    Ok(ClosedOutcome::Closed(result))
}

/// C1 §3.6 close result from durable rows only [r1.6, r1.8]: the turns a
/// close cancelled, in turn order, and `quiescent` cleanup only when every
/// group of the session's turns has an absence proof.
fn derive_close_result(conn: &Connection, session: &SessionId) -> Result<Value, StoreError> {
    let mut query = conn
        .prepare_cached(
            "SELECT number FROM turns WHERE session_id=?1 AND cancel_cause='close' ORDER BY number",
        )
        .map_err(sql_error)?;
    let cancelled = query
        .query_map([session.as_str()], |row| row.get::<_, u32>(0))
        .map_err(sql_error)?
        .map(|number| {
            number
                .map(|number| Value::String(format!("{}/{number}", session.as_str())))
                .map_err(sql_error)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let unproven: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM anchors WHERE owner_session=?1 AND absence_time IS NULL)",
            [session.as_str()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    Ok(serde_json::json!({
        "session_id": session.as_str(),
        "state": "closed",
        "cancelled_turns": cancelled,
        "cleanup": if unproven { "uncertain" } else { "quiescent" },
    }))
}

fn read_close_result(conn: &Connection, session: &SessionId) -> Result<Option<Value>, StoreError> {
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT state,close_result FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    match row {
        Some((state, Some(result))) if state == "closed" => serde_json::from_str(&result)
            .map(Some)
            .map_err(|_| StoreError::CorruptEvidence),
        Some((state, None)) if state == "closed" => derive_close_result(conn, session).map(Some),
        Some(_) | None => Ok(None),
    }
}

fn read_closing_sessions(
    conn: &Connection,
    after: Option<&SessionId>,
    limit: u32,
) -> Result<Vec<SessionId>, StoreError> {
    let mut query = conn
        .prepare_cached(
            "SELECT id FROM sessions WHERE admission='closing' AND state!='closed'
             AND (?1 IS NULL OR id>?1) ORDER BY id LIMIT ?2",
        )
        .map_err(sql_error)?;
    let rows = query
        .query_map(params![after.map(SessionId::as_str), limit], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sql_error)?;
    rows.map(|row| {
        let id = row.map_err(sql_error)?;
        SessionId::try_from(id.as_str()).map_err(|_| StoreError::CorruptEvidence)
    })
    .collect()
}

/// Design §7.2 row 2 and §7.3: `queued → running → failed` in one
/// transaction, with `turn.submitted` and `turn.ended`.
fn commit_submit_failed(
    conn: &mut Connection,
    record: &SubmitFailedRecord,
) -> Result<(), StoreError> {
    let session = &record.session_id;
    let at = event_at(&record.submitted)?;
    if record.envelope.get("state").and_then(Value::as_str) != Some("failed") {
        return Err(StoreError::Constraint("a failed submission ends failed"));
    }
    let envelope = json(&record.envelope)?;
    let ended_seq = i64::try_from(seq(&record.ended)?)
        .map_err(|_| StoreError::Constraint("event sequence too large"))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    // No agent I/O, so no evidence folder: `evidence_dir` stays NULL.
    let changed = tx
        .execute(
            "UPDATE turns SET state='failed',submitted_at=?3,envelope=?4,ended_seq=?5 WHERE session_id=?1 AND number=?2 AND state='queued'",
            params![session.as_str(), record.turn.get(), at, envelope, ended_seq],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StoreError::Constraint("turn is not queued"));
    }
    insert_event(&tx, session, &record.submitted)?;
    insert_event(&tx, session, &record.ended)?;
    update_session_state(&tx, session, false)?;
    before_commit!("store.commit.terminal");
    commit(tx)
}

/// Design §7.4: one running turn's terminal, then its session's queued
/// cancellations, in one transaction and within the event bound.
fn commit_failure_resolution(
    conn: &mut Connection,
    record: &FailureResolutionRecord,
) -> Result<(), StoreError> {
    let session = &record.terminal.session_id;
    if record.cancellations.len() > FAILURE_BATCH_CANCELLATIONS {
        return Err(StoreError::Constraint(
            "a failure batch carries at most 8 cancellations",
        ));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    // Both checks read the durable turns before any write: a refused batch
    // writes nothing and never leaves queued work behind.
    if turn_state(&tx, session, record.terminal.turn)?.as_deref() != Some("running") {
        return Err(StoreError::Refused(
            "a failure batch resolves a running turn",
        ));
    }
    let mut cancelled: Vec<u32> = record
        .cancellations
        .iter()
        .map(|cancellation| cancellation.turn.get())
        .collect();
    cancelled.sort_unstable();
    if record
        .cancellations
        .iter()
        .any(|cancellation| cancellation.session_id != *session)
        || cancelled != queued_turn_numbers(&tx, session)?
    {
        return Err(StoreError::Refused(
            "a failure batch cancels exactly its session's queued turns",
        ));
    }
    insert_terminal(&tx, &record.terminal, &TerminalExtras::default(), None)?;
    for cancellation in &record.cancellations {
        if cancellation.envelope.get("state").and_then(Value::as_str) != Some("cancelled") {
            return Err(StoreError::Constraint(
                "a failure batch cancels only its session's queued turns",
            ));
        }
        insert_terminal(&tx, cancellation, &TerminalExtras::default(), None)?;
    }
    before_commit!("store.commit.terminal");
    commit(tx)
}

/// The session's queued turn numbers, ascending.
fn queued_turn_numbers(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionId,
) -> Result<Vec<u32>, StoreError> {
    let mut statement = tx
        .prepare("SELECT number FROM turns WHERE session_id=?1 AND state='queued' ORDER BY number")
        .map_err(sql_error)?;
    statement
        .query_map([session.as_str()], |row| row.get(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<u32>, _>>()
        .map_err(sql_error)
}

/// Whether any turn of the session is queued or running.
fn unfinished_turns(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionId,
) -> Result<bool, StoreError> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM turns WHERE session_id=?1 AND state IN ('queued','running'))",
        [session.as_str()],
        |row| row.get(0),
    )
    .map_err(sql_error)
}

/// The stored envelope text, checked as one JSON value without building
/// it (design §6.7 `result_text`); anything else is corrupt evidence.
fn read_result_text(
    conn: &Connection,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<Option<Box<serde_json::value::RawValue>>, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT envelope FROM turns WHERE session_id=?1 AND number=?2",
            params![session.as_str(), turn.get()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?
        .flatten();
    raw.map(|text| {
        serde_json::value::RawValue::from_string(text).map_err(|_| StoreError::CorruptEvidence)
    })
    .transpose()
}

/// A committed terminal's state and `cancel`, extracted by SQLite from the
/// stored envelope (design §6.7): no value is built from it.
fn read_terminal_facts(
    conn: &Connection,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<Option<TerminalFacts>, StoreError> {
    /// State, the type of `cancel`, and its four members.
    type Row = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<Row> = conn
        .query_row(
            "SELECT json_extract(envelope,'$.state'),json_type(envelope,'$.cancel'),
                json_extract(envelope,'$.cancel.outcome'),json_extract(envelope,'$.cancel.cleanup'),
                json_extract(envelope,'$.cancel.requested_at'),json_extract(envelope,'$.cancel.settled_at')
             FROM turns WHERE session_id=?1 AND number=?2 AND envelope IS NOT NULL",
            params![session.as_str(), turn.get()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((state, kind, outcome, cleanup, requested_at, settled_at)) = row else {
        return Ok(None);
    };
    let cancel = match kind.as_deref() {
        None | Some("null") => None,
        Some("object") => Some(TerminalCancel {
            outcome: outcome.ok_or(StoreError::CorruptEvidence)?,
            cleanup: cleanup.ok_or(StoreError::CorruptEvidence)?,
            requested_at: requested_at.ok_or(StoreError::CorruptEvidence)?,
            settled_at: settled_at.ok_or(StoreError::CorruptEvidence)?,
        }),
        Some(_) => return Err(StoreError::CorruptEvidence),
    };
    Ok(Some(TerminalFacts {
        state: state.ok_or(StoreError::CorruptEvidence)?,
        cancel,
    }))
}

fn read_unfinished(conn: &Connection) -> Result<Vec<UnfinishedTurn>, StoreError> {
    let mut statement = conn
        .prepare_cached(
            "SELECT session_id,number,submitted_at,correlation FROM turns WHERE state='running' ORDER BY session_id,number LIMIT 1000",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(sql_error)?;
    let mut turns = Vec::new();
    for row in rows {
        let (session, number, submitted_at, correlation) = row.map_err(sql_error)?;
        turns.push(UnfinishedTurn {
            session_id: SessionId::try_from(session.as_str())
                .map_err(|_| StoreError::CorruptEvidence)?,
            turn: TurnNumber::try_from(number).map_err(|_| StoreError::CorruptEvidence)?,
            // A running turn always has its submission time.
            submitted_at: submitted_at.ok_or(StoreError::CorruptEvidence)?,
            correlation,
        });
    }
    Ok(turns)
}

fn read_terminated(
    conn: &Connection,
    turns: Vec<(SessionId, TurnNumber)>,
) -> Result<Vec<(SessionId, TurnNumber)>, StoreError> {
    let mut statement = conn
        .prepare_cached(
            "SELECT 1 FROM turns WHERE session_id=?1 AND number=?2 AND envelope IS NOT NULL",
        )
        .map_err(sql_error)?;
    let mut terminated = Vec::new();
    for (session, turn) in turns {
        let found = statement
            .exists(params![session.as_str(), turn.get()])
            .map_err(sql_error)?;
        if found {
            terminated.push((session, turn));
        }
    }
    Ok(terminated)
}

fn read_events(
    conn: &Connection,
    session: &SessionId,
    from: u64,
    limit: u32,
) -> Result<Vec<StoredEvent>, StoreError> {
    let from =
        i64::try_from(from).map_err(|_| StoreError::Constraint("event sequence too large"))?;
    let mut query = conn
        .prepare(
            "SELECT seq,event FROM events WHERE session_id=?1 AND seq>=?2 ORDER BY seq LIMIT ?3",
        )
        .map_err(sql_error)?;
    let rows = query
        .query_map(params![session.as_str(), from, limit], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error)?;
    let mut events = Vec::new();
    for row in rows {
        let (seq, event) = row.map_err(sql_error)?;
        events.push(StoredEvent {
            seq: u64::try_from(seq).map_err(|_| StoreError::CorruptEvidence)?,
            event: serde_json::from_str(&event).map_err(|_| StoreError::CorruptEvidence)?,
        });
    }
    Ok(events)
}

/// `events`' one read (design §4.3): the window `after < seq ≤ after +
/// 1000` with `turn` and `types` as SQL predicates, stopping at `limit`
/// matches or before [`PAGE_MAX`], each row's borrowed length checked
/// before it is copied; the array is written as one JSON text. The writer
/// serves it alone, so the head is read in the same state.
fn read_events_page(conn: &Connection, query: &EventsQuery) -> Result<EventsRead, StoreError> {
    let session = query.session.as_str();
    let next_seq: Option<i64> = conn
        .query_row(
            "SELECT next_seq FROM sessions WHERE id=?1",
            [session],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let Some(next_seq) = next_seq else {
        return Ok(EventsRead::SessionNotFound);
    };
    let head = u64::try_from(next_seq - 1).map_err(|_| StoreError::CorruptEvidence)?;
    let turn = query.turn.map(TurnNumber::get);
    if let Some(number) = turn {
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM turns WHERE session_id=?1 AND number=?2",
                params![session, number],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        if exists.is_none() {
            return Ok(EventsRead::TurnNotFound);
        }
    }
    // Nothing follows the head: an empty page that stays at `after`, also
    // for a cursor above `i64::MAX`, past every sequence SQLite stores.
    if query.after >= head {
        return Ok(EventsRead::Page(EventsPage {
            events: "[]".to_owned(),
            next_after: query.after,
            more: false,
        }));
    }
    let too_large = || StoreError::Constraint("event sequence too large");
    let end = query.after.saturating_add(PAGE_SCAN);
    let after = i64::try_from(query.after).map_err(|_| too_large())?;
    let last = i64::try_from(end).unwrap_or(i64::MAX);
    let types = if query.types.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&query.types).map_err(|_| too_large())?)
    };
    let mut statement = conn
        .prepare(
            "SELECT seq,event FROM events WHERE session_id=?1 AND seq>?2 AND seq<=?3
               AND (?4 IS NULL OR turn=?4)
               AND (?5 IS NULL OR type IN (SELECT value FROM json_each(?5)))
             ORDER BY seq LIMIT ?6",
        )
        .map_err(sql_error)?;
    let mut rows = statement
        .query(params![session, after, last, turn, types, query.limit])
        .map_err(sql_error)?;
    let mut events = String::from("[");
    let mut count = 0_u32;
    let mut last_returned = None;
    let mut first_left_out = None;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let seq: i64 = row.get(0).map_err(sql_error)?;
        let seq = u64::try_from(seq).map_err(|_| StoreError::CorruptEvidence)?;
        let event = row
            .get_ref(1)
            .map_err(sql_error)?
            .as_str()
            .map_err(|_| StoreError::CorruptEvidence)?;
        // The array's comma and closing bracket, and the page's members.
        if events.len() + event.len() + 2 > PAGE_MAX - PAGE_WRAPPER {
            debug_assert!(count > 0, "the first match always fits a page");
            first_left_out = Some(seq);
            break;
        }
        if count > 0 {
            events.push(',');
        }
        events.push_str(event);
        count += 1;
        last_returned = Some(seq);
    }
    events.push(']');
    let next_after = match (first_left_out, last_returned) {
        (Some(left_out), _) => left_out - 1,
        (None, Some(returned)) if count == query.limit => returned,
        _ => end.min(head).max(query.after),
    };
    Ok(EventsRead::Page(EventsPage {
        events,
        next_after,
        more: next_after < head,
    }))
}

/// `list`'s one read (design §6.8): at most 1000 sessions below `before`
/// by `ord`, newest first, the filters applied to each row in order,
/// stopping at `limit` matches or before [`PAGE_MAX`] (a row's borrowed
/// length checked before its summary is built). `next` is the last
/// examined `ord`, `None` once the oldest session was examined.
fn read_list_page(conn: &Connection, query: &ListQuery) -> Result<ListPage, StoreError> {
    /// A summary's encoded members besides its variable text.
    const SUMMARY_FIXED: usize = 192;
    /// Most bytes one text byte encodes to (`\u0001`).
    const ESCAPED: usize = 6;
    let before = match query.before {
        Some(before) => {
            i64::try_from(before).map_err(|_| StoreError::Constraint("list cursor too large"))?
        }
        None => i64::MAX,
    };
    let oldest: Option<i64> = conn
        .query_row("SELECT min(ord) FROM sessions", [], |row| row.get(0))
        .map_err(sql_error)?;
    let mut statement = conn
        .prepare(
            "SELECT id,state,admission,harness,json_extract(params,'$.model'),label,
                created_ms,updated_ms,ord
             FROM sessions WHERE ord<?1 ORDER BY ord DESC LIMIT 1000",
        )
        .map_err(sql_error)?;
    let mut rows = statement.query([before]).map_err(sql_error)?;
    let mut sessions = Vec::new();
    let mut bytes = 0_usize;
    let mut examined = None;
    let text = |row: &rusqlite::Row<'_>, index: usize| -> Result<Option<String>, StoreError> {
        row.get(index).map_err(sql_error)
    };
    while let Some(row) = rows.next().map_err(sql_error)? {
        let mut borrowed = 0;
        for index in 0..6 {
            if let rusqlite::types::ValueRef::Text(value) = row.get_ref(index).map_err(sql_error)? {
                borrowed += value.len();
            }
        }
        let size = SUMMARY_FIXED + ESCAPED * borrowed;
        if bytes + size > PAGE_MAX - PAGE_WRAPPER {
            // C1 §3.10 (A49): the first summary always fits a page.
            debug_assert!(bytes > 0, "the first summary always fits a page");
            break;
        }
        let ord: i64 = row.get(8).map_err(sql_error)?;
        examined = Some(ord);
        let state: String = row.get(1).map_err(sql_error)?;
        let harness: String = row.get(3).map_err(sql_error)?;
        let label = text(row, 5)?;
        let updated_ms: i64 = row.get(7).map_err(sql_error)?;
        let matches = query.state.as_ref().is_none_or(|wanted| *wanted == state)
            && query
                .harness
                .as_ref()
                .is_none_or(|wanted| *wanted == harness)
            && query
                .label
                .as_ref()
                .is_none_or(|wanted| label.as_ref() == Some(wanted))
            && query.since_ms.is_none_or(|since| updated_ms >= since);
        if !matches {
            continue;
        }
        let id: String = row.get(0).map_err(sql_error)?;
        bytes += size;
        sessions.push(SessionSummary {
            session_id: SessionId::try_from(id.as_str())
                .map_err(|_| StoreError::CorruptEvidence)?,
            state,
            admission: row.get(2).map_err(sql_error)?,
            harness,
            model: text(row, 4)?,
            label,
            created_ms: row.get(6).map_err(sql_error)?,
            last_active_ms: updated_ms,
        });
        if sessions.len() == query.limit as usize {
            break;
        }
    }
    let next = match examined {
        Some(ord) if Some(ord) != oldest => {
            Some(u64::try_from(ord).map_err(|_| StoreError::CorruptEvidence)?)
        }
        _ => None,
    };
    Ok(ListPage { sessions, next })
}

/// Design §4.4, §6.7: the addressed turn, or the session's running turn,
/// else its latest submitted turn, else its latest turn; with the turn's
/// `evidence_dir` and the session's vendor identity and transcript hint.
/// One small row: no file is read.
fn read_evidence_refs(
    conn: &Connection,
    session: &SessionId,
    turn: Option<TurnNumber>,
) -> Result<Option<EvidenceRefs>, StoreError> {
    /// Vendor session ID, transcript hint, turn number and its folder.
    type Row = (Option<String>, Option<String>, Option<u32>, Option<String>);
    let row: Option<Row> = conn
        .query_row(
            "SELECT s.vendor_session_id,s.transcript_hint,t.number,t.evidence_dir
             FROM sessions s LEFT JOIN turns t ON t.session_id=s.id AND t.number=coalesce(?2,
                (SELECT number FROM turns WHERE session_id=s.id AND state='running'),
                (SELECT max(number) FROM turns WHERE session_id=s.id AND submitted_at IS NOT NULL),
                (SELECT max(number) FROM turns WHERE session_id=s.id))
             WHERE s.id=?1",
            params![session.as_str(), turn.map(TurnNumber::get)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((vendor_session_id, transcript_hint, number, evidence_dir)) = row else {
        return Ok(None);
    };
    Ok(Some(EvidenceRefs {
        turn: number
            .map(TurnNumber::try_from)
            .transpose()
            .map_err(|_| StoreError::CorruptEvidence)?,
        evidence_dir,
        vendor_session_id,
        transcript_hint,
    }))
}

/// `status`'s one read (design §4.2, §6.7, §11.3): the session's durable
/// members, the selected turn (the param, else the running turn, else the
/// latest) and a primary-key range page of its step rows. The writer
/// serves it alone, so its statements see one state.
fn read_session_status(
    conn: &Connection,
    query: &StatusQuery,
) -> Result<Option<SessionStatus>, StoreError> {
    /// Session columns, `model`, `cwd` and `route`.
    type Session = (
        String,
        String,
        String,
        Option<String>,
        i64,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let session = query.session.as_str();
    let row: Option<Session> = conn
        .query_row(
            "SELECT state,admission,harness,label,created_ms,updated_ms,
                json_extract(params,'$.model'),json_extract(params,'$.cwd'),
                json_extract(receipt,'$.route'),vendor_session_id
             FROM sessions WHERE id=?1",
            [session],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((state, admission, harness, label, created_ms, updated_ms, model, cwd, route, vendor)) =
        row
    else {
        return Ok(None);
    };
    let unproven_anchors = read_unproven_anchors(conn, session)?;
    let active = read_active_turn(conn, session)?;
    let selected: Option<(u32, String)> = conn
        .query_row(
            "SELECT number,state FROM turns WHERE session_id=?1 AND number=coalesce(?2,
                (SELECT number FROM turns WHERE session_id=?1 AND state='running'),
                (SELECT max(number) FROM turns WHERE session_id=?1))",
            params![session, query.turn.map(TurnNumber::get)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let queue = read_status_queue(conn, session)?;
    let turns = read_status_turns(conn, session)?;
    let (steps, more) = match &selected {
        Some((number, _)) => read_step_page(conn, session, *number, query)?,
        None => (Vec::new(), false),
    };
    Ok(Some(SessionStatus {
        state,
        admission,
        harness,
        label,
        created_ms,
        updated_ms,
        model,
        cwd,
        route,
        vendor_session_id: vendor,
        cleanup_uncertain: !unproven_anchors.is_empty(),
        unproven_anchors,
        selected,
        active,
        queue,
        turns,
        steps,
        more,
    }))
}

/// The newest [`STATUS_ANCHORS`] anchors of the session with no absence
/// proof (§11.3 `process`).
fn read_unproven_anchors(conn: &Connection, session: &str) -> Result<Vec<String>, StoreError> {
    conn.prepare_cached(
        "SELECT anchor_id FROM anchors WHERE owner_session=?1 AND absence_time IS NULL
             ORDER BY rowid DESC LIMIT ?2",
    )
    .and_then(|mut statement| {
        statement
            .query_map(params![session, STATUS_ANCHORS], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()
    })
    .map_err(sql_error)
}

/// The session's first [`STATUS_QUEUE`] queued turns (§11.3 `queue`).
fn read_status_queue(conn: &Connection, session: &str) -> Result<Vec<QueuedSummary>, StoreError> {
    conn
        .prepare_cached(
            "SELECT t.number,o.op_key,t.queued_at,t.effective FROM turns t
             LEFT JOIN operations o ON o.session_id=t.session_id AND o.turn=t.number AND o.verb='resume'
             WHERE t.session_id=?1 AND t.state='queued' ORDER BY t.number LIMIT ?2",
        )
        .and_then(|mut statement| {
            statement
                .query_map(params![session, STATUS_QUEUE], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get::<_, String>(3)?))
                })?
                .collect::<Result<Vec<(u32, Option<String>, Option<String>, String)>, _>>()
        })
        .map_err(sql_error)?
        .into_iter()
        .map(|(turn, op_key, queued_at, effective)| {
            Ok(QueuedSummary {
                turn,
                op_key,
                queued_at,
                effective: serde_json::from_str(&effective)
                    .map_err(|_| StoreError::CorruptEvidence)?,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()
}

/// The session's newest [`STATUS_TURNS`] turns and their states (§11.3).
fn read_status_turns(conn: &Connection, session: &str) -> Result<Vec<(u32, String)>, StoreError> {
    conn.prepare_cached(
        "SELECT number,state FROM turns WHERE session_id=?1 ORDER BY number DESC LIMIT ?2",
    )
    .and_then(|mut statement| {
        statement
            .query_map(params![session, STATUS_TURNS], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<Vec<(u32, String)>, _>>()
    })
    .map_err(sql_error)
}

/// The session's running turn as `status` reports it (§11.3).
fn read_active_turn(conn: &Connection, session: &str) -> Result<Option<ActiveTurn>, StoreError> {
    conn.query_row(
        "SELECT t.number,t.accepted_at IS NOT NULL,t.submitted_at,
            (SELECT max(seq) FROM events WHERE session_id=t.session_id AND turn=t.number),
            (SELECT json_extract(event,'$.at') FROM events
                WHERE session_id=t.session_id AND turn=t.number AND type='cancel.requested'
                ORDER BY seq LIMIT 1)
         FROM turns t WHERE t.session_id=?1 AND t.state='running'",
        [session],
        |row| {
            Ok(ActiveTurn {
                turn: row.get(0)?,
                accepted: row.get(1)?,
                submitted_at: row.get(2)?,
                last_event_seq: row
                    .get::<_, Option<i64>>(3)?
                    .and_then(|seq| u64::try_from(seq).ok())
                    .unwrap_or(0),
                cancel_requested_at: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(sql_error)
}

/// A primary-key range page of a turn's step rows after `after_step`, and
/// whether more follow (design §3.4).
fn read_step_page(
    conn: &Connection,
    session: &str,
    turn: u32,
    query: &StatusQuery,
) -> Result<(Vec<StepRow>, bool), StoreError> {
    let mut rows = conn
        .prepare_cached(
            "SELECT step,started_ms,ended_ms,tokens FROM steps
             WHERE session_id=?1 AND turn=?2 AND step>?3 ORDER BY step LIMIT ?4",
        )
        .and_then(|mut statement| {
            statement
                .query_map(
                    params![session, turn, query.after_step, query.limit + 1],
                    |row| {
                        Ok(StepRow {
                            step: row.get(0)?,
                            started_ms: row.get(1)?,
                            ended_ms: row.get(2)?,
                            tokens: row
                                .get::<_, Option<i64>>(3)?
                                .and_then(|tokens| u64::try_from(tokens).ok()),
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(sql_error)?;
    let limit = usize::try_from(query.limit).unwrap_or(usize::MAX);
    let more = rows.len() > limit;
    rows.truncate(limit);
    Ok((rows, more))
}

fn authenticate(
    conn: &Connection,
    session: &SessionId,
    hash: &[u8; 32],
) -> Result<bool, StoreError> {
    let stored: Option<Vec<u8>> = conn
        .query_row(
            "SELECT handle_hash FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    Ok(stored.is_some_and(|stored| {
        stored.len() == 32
            && stored
                .iter()
                .zip(hash.iter())
                .fold(0_u8, |difference, (left, right)| {
                    difference | (left ^ right)
                })
                == 0
    }))
}
