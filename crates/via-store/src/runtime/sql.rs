//! SQLite migration and single-writer transaction implementation.

use super::{
    AcceptanceRecord, CancelCause, ClosedOutcome, ClosedRecord, ClosingRecord, Command,
    CommitOutcome, Connection, ConnectionId, Duration, EventRecord, FAILURE_BATCH_CANCELLATIONS,
    FailureResolutionRecord, KeyedOperation, MetadataExt, OperationRecord, OperationVerb,
    OptionalExtension, Path, Predecessors, QueuedTurn, RAW_UNIT_LIMIT, RawRef, RawStream,
    ReceiptRecord, Receiver, ResumeRecord, SESSION_QUEUE_LIMIT, SessionId, SessionSnapshot,
    SpawnKey, SpawnRecord, StoreError, StoredEvent, StoredSpawnKey, SubmissionRecord,
    SubmitFailedRecord, TerminalExtras, TerminalRecord, TransactionBehavior, TurnNumber,
    UnfinishedTurn, Value, check_schema_version, commit_anchor_identified, commit_anchor_intent,
    commit_arm_intent, commit_group_absence, commit_vendor_facts, count_unproven_anchors, fs,
    oneshot, params, read_anchor_owners, read_anchor_records, read_raw_ref, validate_raw_ref,
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

/// Test-only seams inside a transaction, just before `COMMIT` (design §10):
/// a `fail_io` at `point` or at `store.commit.fail_persistent` rolls the
/// transaction back, so the write is not committed; a pause holds the writer.
#[cfg(feature = "test-failpoints")]
pub(super) fn before_commit(point: &'static str) -> Result<(), StoreError> {
    for point in [point, "store.commit.fail_persistent"] {
        crate::failpoint::hit(point).map_err(|error| StoreError::Write(error.to_string()))?;
    }
    Ok(())
}

/// Release builds have no seam before `COMMIT`.
#[cfg(not(feature = "test-failpoints"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "test builds can fail at the seam; callers stay identical"
)]
pub(super) fn before_commit(_point: &'static str) -> Result<(), StoreError> {
    Ok(())
}

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

fn current_uid() -> Result<u32, StoreError> {
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

/// Configures the sole writable connection; initializes the schema only in
/// a database this open `created`. The version is checked again before the
/// first mutation, the journal-mode switch.
pub(super) fn configure(conn: &mut Connection, created: bool) -> Result<(), StoreError> {
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
    if version == 0 {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| StoreError::Open(error.to_string()))?;
        tx.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, handle_hash BLOB NOT NULL CHECK(length(handle_hash)=32),
                receipt TEXT NOT NULL, params TEXT NOT NULL, state TEXT NOT NULL,
                next_seq INTEGER NOT NULL CHECK(next_seq>=2),
                admission TEXT NOT NULL DEFAULT 'open' CHECK(admission IN ('open','closing')),
                close_result TEXT);
             CREATE TABLE turns (
                session_id TEXT NOT NULL REFERENCES sessions(id), number INTEGER NOT NULL,
                prompt TEXT NOT NULL, effective TEXT NOT NULL, state TEXT NOT NULL, queued_at TEXT,
                queued_seq INTEGER NOT NULL, submitted_at TEXT,
                accepted_at TEXT, correlation TEXT, envelope TEXT,
                cancel_cause TEXT CHECK(cancel_cause IN ('cancel','close')),
                PRIMARY KEY(session_id,number));
             CREATE UNIQUE INDEX turns_one_running ON turns(session_id) WHERE state='running';
             CREATE TABLE spawn_keys (
                key TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                identity BLOB NOT NULL, receipt TEXT NOT NULL);
             CREATE TABLE operations (
                session_id TEXT NOT NULL REFERENCES sessions(id), op_key TEXT NOT NULL,
                verb TEXT NOT NULL CHECK(verb IN ('resume','close')), identity BLOB NOT NULL,
                turn INTEGER, result TEXT, PRIMARY KEY(session_id,op_key),
                CHECK(verb='close' OR (turn IS NOT NULL AND result IS NOT NULL)),
                FOREIGN KEY(session_id,turn) REFERENCES turns(session_id,number));
             CREATE TABLE events (
                session_id TEXT NOT NULL REFERENCES sessions(id), seq INTEGER NOT NULL,
                event TEXT NOT NULL, connection_id TEXT, raw_offset INTEGER, raw_len INTEGER,
                PRIMARY KEY(session_id,seq));
             CREATE TABLE anchors (
                anchor_id TEXT PRIMARY KEY, generation TEXT NOT NULL, marker TEXT NOT NULL,
                socket_path TEXT NOT NULL, owner_session TEXT NOT NULL, owner_turn INTEGER NOT NULL,
                uid INTEGER NOT NULL, boot_id TEXT NOT NULL, pid_namespace TEXT NOT NULL,
                phase TEXT NOT NULL, record_version INTEGER NOT NULL,
                pid INTEGER, pgid INTEGER, start_ticks INTEGER, vendor_pid INTEGER,
                absence_time TEXT,
                FOREIGN KEY(owner_session,owner_turn) REFERENCES turns(session_id,number));
             CREATE INDEX anchors_unproven ON anchors(anchor_id) WHERE absence_time IS NULL;
             PRAGMA user_version=5;",
        )
        .map_err(|error| StoreError::Open(error.to_string()))?;
        tx.commit()
            .map_err(|error| StoreError::Uncertain(error.to_string()))?;
    }
    Ok(())
}

pub(super) fn writer_loop(mut conn: Connection, root: &Path, receiver: &Receiver<Command>) {
    let mut commits = 0_u32;
    while let Ok(command) = receiver.recv() {
        if matches!(command, Command::Shutdown) {
            break;
        }
        // Test-only `store.writer.lost`: the worker drops the request and its
        // reply unserved, as a writer that is gone would (design §7.1).
        #[cfg(feature = "test-failpoints")]
        if crate::failpoint::hit("store.writer.lost").is_err() {
            drop(command);
            continue;
        }
        // Reads are served first; anything else is a mutation.
        let Some(command) = serve_read(&conn, root, command) else {
            continue;
        };
        serve_write(&mut conn, root, command);
        commits += 1;
        if commits >= 1000 {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
            commits = 0;
        }
    }
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
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
                | Self::Result(..)
                | Self::CloseResult(..)
                | Self::ClosingSessions(..)
                | Self::Terminated(..)
                | Self::Events(..)
                | Self::Logs(..)
                | Self::Authenticate(..)
                | Self::Unfinished(..)
                | Self::AnchorOwners(..)
                | Self::UnprovenAnchors(..)
                | Self::QueuedTurns(..)
                | Self::AnchorRecords(..)
        )
    }

    /// Replies `error` unserved: a journal mutation reports its outcome class.
    #[cfg_attr(
        not(feature = "test-failpoints"),
        expect(dead_code, reason = "only the test-only read seams fail a request")
    )]
    fn fail(self, error: StoreError) {
        match self {
            Self::Spawn(_, _, reply) => drop(reply.send(Err(error))),
            Self::SpawnKey(_, reply) => drop(reply.send(Err(error))),
            Self::Resume(_, reply)
            | Self::Submission(_, reply)
            | Self::Acceptance(_, reply)
            | Self::Event(_, reply)
            | Self::Terminal(_, _, reply)
            | Self::Closing(_, reply)
            | Self::SubmitFailed(_, reply)
            | Self::FailureResolution(_, reply) => drop(reply.send(Err(error))),
            Self::Operation(_, _, reply) => drop(reply.send(Err(error))),
            Self::KeyedOperation(_, _, reply) => drop(reply.send(Err(error))),
            Self::Snapshot(_, reply) => drop(reply.send(Err(error))),
            Self::QueuedTurn(_, _, reply) => drop(reply.send(Err(error))),
            Self::NextSeq(_, reply) => drop(reply.send(Err(error))),
            Self::UnprovenAnchors(_, _, reply) => drop(reply.send(Err(error))),
            Self::Predecessors(_, _, reply) => drop(reply.send(Err(error))),
            Self::ClosingTerminal(_, _, reply) | Self::SessionClosed(_, _, reply) => {
                drop(reply.send(Err(error)));
            }
            Self::Closed(_, reply) => drop(reply.send(Err(error))),
            Self::Result(_, _, reply) | Self::CloseResult(_, reply) => {
                drop(reply.send(Err(error)));
            }
            Self::ClosingSessions(_, _, reply) => drop(reply.send(Err(error))),
            Self::Terminated(_, reply) | Self::QueuedTurns(_, _, reply) => {
                drop(reply.send(Err(error)));
            }
            Self::Events(_, _, _, reply) => drop(reply.send(Err(error))),
            Self::Logs(_, reply) => drop(reply.send(Err(error))),
            Self::Unfinished(reply) => drop(reply.send(Err(error))),
            Self::AnchorOwners(_, _, reply) => drop(reply.send(Err(error))),
            Self::Authenticate(_, _, reply) => drop(reply.send(Err(error))),
            Self::AnchorIntent(_, reply) => drop(reply.send(error.journal_outcome())),
            Self::AnchorIdentified(.., reply) | Self::ArmIntent(.., reply) => {
                drop(reply.send(error.journal_outcome()));
            }
            Self::VendorFacts(.., reply) | Self::GroupAbsence(_, reply) => {
                drop(reply.send(error.journal_outcome()));
            }
            Self::AnchorRecords(_, reply) => drop(reply.send(Err(error.kind()))),
            Self::Shutdown => {}
        }
    }
}

/// Test-only seams on a read the worker dequeued (design §6.7, §7.1, §7.3):
/// `store.read.stall` pauses the worker; `store.sqlite.corrupt` reports
/// corruption; `store.read.dispatch` fails the dispatcher's head reads
/// (predecessors, the queued row, the head's next sequence) and
/// `store.read.queued_turn` only the queued-row read.
#[cfg(feature = "test-failpoints")]
fn read_seams(command: &Command) -> Result<(), StoreError> {
    use crate::failpoint::hit;
    let injected = |error: std::io::Error| StoreError::Write(error.to_string());
    hit("store.read.stall").map_err(injected)?;
    hit("store.sqlite.corrupt").map_err(|error| StoreError::Corrupt(error.to_string()))?;
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

/// Serves a read command; returns any other command unserved.
fn serve_read(conn: &Connection, root: &Path, command: Command) -> Option<Command> {
    if !command.is_read() {
        return Some(command);
    }
    #[cfg(feature = "test-failpoints")]
    if let Err(error) = read_seams(&command) {
        command.fail(error);
        return None;
    }
    match command {
        Command::SpawnKey(key, reply) => {
            let _ = reply.send(read_spawn_key(conn, &key));
        }
        Command::Operation(session, op_key, reply) => {
            let _ = reply.send(read_operation(conn, &session, &op_key));
        }
        Command::KeyedOperation(session, op_key, reply) => {
            let _ = reply.send(read_keyed_operation(conn, &session, &op_key));
        }
        Command::Snapshot(session, reply) => {
            let _ = reply.send(read_snapshot(conn, &session));
        }
        Command::QueuedTurn(session, turn, reply) => {
            let _ = reply.send(read_queued_turn(conn, &session, turn));
        }
        Command::Predecessors(session, turn, reply) => {
            let _ = reply.send(read_predecessors(conn, &session, turn));
        }
        Command::NextSeq(session, reply) => {
            let _ = reply.send(read_next_seq(conn, &session));
        }
        Command::Result(session, turn, reply) => {
            let _ = reply.send(read_result(conn, &session, turn));
        }
        Command::CloseResult(session, reply) => {
            let _ = reply.send(read_close_result(conn, &session));
        }
        Command::ClosingSessions(after, limit, reply) => {
            let _ = reply.send(read_closing_sessions(conn, after.as_ref(), limit));
        }
        Command::Terminated(turns, reply) => {
            let _ = reply.send(read_terminated(conn, turns));
        }
        Command::Events(session, from, limit, reply) => {
            let _ = reply.send(read_events(conn, &session, from, limit));
        }
        Command::Logs(session, reply) => {
            let _ = reply.send(read_logs(conn, root, &session));
        }
        Command::Authenticate(session, hash, reply) => {
            let _ = reply.send(authenticate(conn, &session, &hash));
        }
        Command::Unfinished(reply) => {
            let _ = reply.send(read_unfinished(conn));
        }
        Command::AnchorOwners(after, limit, reply) => {
            let _ = reply.send(read_anchor_owners(conn, after.as_deref(), limit));
        }
        Command::UnprovenAnchors(after, limit, reply) => {
            let _ = reply.send(count_unproven_anchors(conn, after.as_deref(), limit));
        }
        Command::QueuedTurns(after, limit, reply) => {
            let _ = reply.send(read_queued_turns(conn, after.as_ref(), limit));
        }
        Command::AnchorRecords(query, reply) => {
            let _ = reply.send(read_anchor_records(conn, &query).map_err(|error| error.kind()));
        }
        // `is_read` returned every other command above.
        command @ (Command::Spawn(..)
        | Command::Resume(..)
        | Command::Submission(..)
        | Command::Acceptance(..)
        | Command::Event(..)
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
        | Command::Shutdown) => return Some(command),
    }
    None
}

/// Serves one mutation command.
fn serve_write(conn: &mut Connection, root: &Path, command: Command) {
    match command {
        Command::Spawn(record, key, reply) => {
            send_commit(reply, commit_spawn(conn, record, key));
        }
        Command::Resume(record, reply) => {
            send_commit(reply, commit_resume(conn, &record));
        }
        Command::Submission(record, reply) => {
            send_commit(reply, commit_submission(conn, &record));
        }
        Command::Acceptance(record, reply) => {
            send_commit(reply, commit_acceptance(conn, root, &record));
        }
        Command::Event(record, reply) => {
            send_commit(reply, commit_event(conn, root, &record));
        }
        Command::Terminal(record, extras, reply) => {
            send_commit(
                reply,
                commit_terminal(conn, root, &record, &extras, None).map(drop),
            );
        }
        Command::SessionClosed(session, closed, reply) => {
            send_commit(reply, commit_session_closed(conn, &session, &closed));
        }
        Command::ClosingTerminal(record, closed, reply) => {
            let extras = TerminalExtras::default();
            send_commit(
                reply,
                commit_terminal(conn, root, &record, &extras, Some(&closed)),
            );
        }
        Command::Closing(record, reply) => {
            send_commit(reply, commit_closing(conn, &record));
        }
        Command::Closed(record, reply) => {
            send_commit(reply, commit_closed(conn, &record));
        }
        Command::SubmitFailed(record, reply) => {
            send_commit(reply, commit_submit_failed(conn, &record));
        }
        Command::FailureResolution(record, reply) => {
            send_commit(reply, commit_failure_resolution(conn, root, &record));
        }
        Command::AnchorIntent(intent, reply) => {
            let _ = reply.send(as_commit(commit_anchor_intent(conn, &intent)));
        }
        Command::AnchorIdentified(id, generation, version, identity, reply) => {
            let _ = reply.send(as_commit(commit_anchor_identified(
                conn,
                &id,
                &generation,
                version,
                &identity,
            )));
        }
        Command::ArmIntent(id, generation, version, reply) => {
            let _ = reply.send(as_commit(commit_arm_intent(
                conn,
                &id,
                &generation,
                version,
            )));
        }
        Command::VendorFacts(id, generation, pid, reply) => {
            let _ = reply.send(as_commit(commit_vendor_facts(conn, &id, &generation, pid)));
        }
        Command::GroupAbsence(proof, reply) => {
            let _ = reply.send(as_commit(commit_group_absence(conn, &proof)));
        }
        // `writer_loop` serves reads and Shutdown before any mutation.
        Command::SpawnKey(..)
        | Command::Operation(..)
        | Command::KeyedOperation(..)
        | Command::Snapshot(..)
        | Command::QueuedTurn(..)
        | Command::NextSeq(..)
        | Command::Predecessors(..)
        | Command::Result(..)
        | Command::CloseResult(..)
        | Command::ClosingSessions(..)
        | Command::Terminated(..)
        | Command::Events(..)
        | Command::Logs(..)
        | Command::Authenticate(..)
        | Command::Unfinished(..)
        | Command::AnchorOwners(..)
        | Command::UnprovenAnchors(..)
        | Command::QueuedTurns(..)
        | Command::AnchorRecords(..)
        | Command::Shutdown => {}
    }
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
    let event = json(&record.initial_event)?;
    // Core's queued event always carries its time; bare fixtures of lower layers may not.
    let queued_at = record.initial_event.get("at").and_then(Value::as_str);
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    tx.execute(
        "INSERT INTO sessions(id,handle_hash,receipt,params,state,next_seq) VALUES (?1,?2,?3,?4,'active',2)",
        params![record.session_id.as_str(), &record.handle_hash[..], receipt, params_json],
    )
    .map_err(sql_error)?;
    tx.execute(
        "INSERT INTO turns(session_id,number,prompt,effective,state,queued_at,queued_seq) VALUES (?1,1,?2,?3,'queued',?4,1)",
        params![record.session_id.as_str(), record.prompt, effective, queued_at],
    )
    .map_err(sql_error)?;
    tx.execute(
        "INSERT INTO events(session_id,seq,event) VALUES (?1,1,?2)",
        params![record.session_id.as_str(), event],
    )
    .map_err(sql_error)?;
    if let Some(key) = key {
        tx.execute(
            "INSERT INTO spawn_keys(key,session_id,identity,receipt) VALUES (?1,?2,?3,?4)",
            params![key.key, record.session_id.as_str(), key.identity, receipt],
        )
        .map_err(sql_error)?;
    }
    // Every row is written but uncommitted: none may survive a crash here.
    #[cfg(feature = "test-failpoints")]
    crate::failpoint::hit("store.spawn.before_commit")
        .map_err(|error| StoreError::Write(error.to_string()))?;
    before_commit("store.commit.receipt")?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))?;
    // Committed but unacknowledged: all rows survive together.
    #[cfg(feature = "test-failpoints")]
    crate::failpoint::hit("store.spawn.after_commit")
        .map_err(|error| StoreError::Uncertain(error.to_string()))?;
    Ok(ReceiptRecord {
        receipt: record.receipt,
    })
}

fn read_spawn_key(conn: &Connection, key: &str) -> Result<Option<StoredSpawnKey>, StoreError> {
    let row: Option<(String, Vec<u8>, String)> = conn
        .query_row(
            "SELECT session_id,identity,receipt FROM spawn_keys WHERE key=?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql_error)?;
    row.map(|(session, identity, receipt)| {
        Ok(StoredSpawnKey {
            session_id: SessionId::try_from(session.as_str())
                .map_err(|_| StoreError::CorruptEvidence)?,
            identity,
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
    tx.execute(
        "INSERT INTO turns(session_id,number,prompt,effective,state,queued_at,queued_seq) VALUES (?1,?2,?3,?4,'queued',?5,?6)",
        params![
            session.as_str(),
            record.turn.get(),
            record.prompt,
            json(&record.effective)?,
            queued_at,
            queued_seq
        ],
    )
    .map_err(sql_error)?;
    insert_event(&tx, session, &record.event, None)?;
    if let Some(operation) = &record.operation {
        tx.execute(
            "INSERT INTO operations(session_id,op_key,verb,identity,turn,result) VALUES (?1,?2,'resume',?3,?4,?5)",
            params![
                session.as_str(),
                operation.op_key,
                operation.identity,
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
    before_commit("store.commit.receipt")?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
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
    let row: Option<(String, Vec<u8>, Option<String>)> = conn
        .query_row(
            "SELECT verb,identity,result FROM operations WHERE session_id=?1 AND op_key=?2",
            params![session.as_str(), op_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql_error)?;
    row.map(|(verb, identity, result)| {
        Ok(KeyedOperation {
            verb: match verb.as_str() {
                "resume" => OperationVerb::Resume,
                "close" => OperationVerb::Close,
                _ => return Err(StoreError::CorruptEvidence),
            },
            identity,
            result: result
                .map(|result| serde_json::from_str(&result))
                .transpose()
                .map_err(|_| StoreError::CorruptEvidence)?,
        })
    })
    .transpose()
}

fn read_snapshot(
    conn: &Connection,
    session: &SessionId,
) -> Result<Option<SessionSnapshot>, StoreError> {
    let row: Option<(String, String, u32, u32, Option<String>)> = conn
        .query_row(
            "SELECT state,admission,
                (SELECT coalesce(max(number),0) FROM turns WHERE session_id=?1),
                (SELECT count(*) FROM turns WHERE session_id=?1 AND state='queued'),
                (SELECT effective FROM turns WHERE session_id=?1 ORDER BY number DESC LIMIT 1)
             FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    row.map(|(state, admission, turns, queued, latest)| {
        Ok(SessionSnapshot {
            closed: state == "closed",
            closing: admission == "closing",
            turns,
            queued,
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
    let row: Option<(String, String, Option<String>, i64)> = conn
        .query_row(
            "SELECT prompt,effective,queued_at,queued_seq FROM turns WHERE session_id=?1 AND number=?2 AND state='queued'",
            params![session.as_str(), turn.get()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(sql_error)?;
    row.map(|(prompt, effective, queued_at, queued_seq)| {
        Ok(QueuedTurn {
            prompt,
            effective: serde_json::from_str(&effective).map_err(|_| StoreError::CorruptEvidence)?,
            queued_at: queued_at.ok_or(StoreError::CorruptEvidence)?,
            queued_seq: u64::try_from(queued_seq).map_err(|_| StoreError::CorruptEvidence)?,
        })
    })
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
    let envelope: Option<Option<String>> = conn
        .query_row(
            "SELECT envelope FROM turns WHERE session_id=?1 AND number<?2 AND submitted_at IS NOT NULL ORDER BY number DESC LIMIT 1",
            params![session.as_str(), turn.get()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let last_submitted = envelope
        .flatten()
        .map(|envelope| serde_json::from_str(&envelope).map_err(|_| StoreError::CorruptEvidence))
        .transpose()?;
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
    let changed = tx
        .execute(
            "UPDATE turns SET state='running',submitted_at=?3 WHERE session_id=?1 AND number=?2 AND state='queued'",
            params![session.as_str(), record.turn.get(), at],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StoreError::Constraint("turn is not queued"));
    }
    insert_event(&tx, session, &record.event, None)?;
    before_commit("store.commit.submission")?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
}

fn commit_acceptance(
    conn: &mut Connection,
    root: &Path,
    record: &AcceptanceRecord,
) -> Result<(), StoreError> {
    let session = &record.session_id;
    let turn = record.turn;
    let correlation = record.correlation.as_str();
    validate_raw_ref(root, &record.raw_ref)?;
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
    insert_event(&tx, session, &record.event, Some(&record.raw_ref))?;
    before_commit("store.commit.event")?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
}

fn event_at(event: &Value) -> Result<&str, StoreError> {
    event
        .get("at")
        .and_then(Value::as_str)
        .ok_or(StoreError::Constraint("event time missing"))
}

/// Appends one event at the session's dense next sequence inside `tx`.
///
/// The event document's `raw_ref` (explicit `null` when there is no span)
/// must equal the span stored in the columns, so `logs` reads exactly the
/// bytes the event cites and no stored span is hidden from the public event.
fn insert_event(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionId,
    event: &Value,
    raw_ref: Option<&RawRef>,
) -> Result<(), StoreError> {
    let next: i64 = tx
        .query_row(
            "SELECT next_seq FROM sessions WHERE id=?1",
            [session.as_str()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if seq(event)? != u64::try_from(next).map_err(|_| StoreError::CorruptEvidence)? {
        return Err(StoreError::Constraint("event sequence is not the next one"));
    }
    let cited = event
        .get("raw_ref")
        .ok_or(StoreError::Constraint("event raw_ref missing"))?;
    let stored =
        serde_json::to_value(raw_ref).map_err(|error| StoreError::Write(error.to_string()))?;
    if *cited != stored {
        return Err(StoreError::Constraint(
            "event raw_ref differs from its span",
        ));
    }
    let (connection, offset, len) = match raw_ref {
        Some(reference) => (
            Some(reference.connection_id().as_str()),
            Some(
                i64::try_from(reference.offset())
                    .map_err(|_| StoreError::Constraint("raw offset too large"))?,
            ),
            Some(i64::from(reference.byte_len())),
        ),
        None => (None, None, None),
    };
    tx.execute(
        "INSERT INTO events(session_id,seq,event,connection_id,raw_offset,raw_len) VALUES (?1,?2,?3,?4,?5,?6)",
        params![session.as_str(), next, json(event)?, connection, offset, len],
    )
    .map_err(sql_error)?;
    tx.execute(
        "UPDATE sessions SET next_seq=next_seq+1 WHERE id=?1",
        [session.as_str()],
    )
    .map_err(sql_error)?;
    Ok(())
}

fn commit_event(
    conn: &mut Connection,
    root: &Path,
    record: &EventRecord,
) -> Result<(), StoreError> {
    if let Some(reference) = &record.raw_ref {
        validate_raw_ref(root, reference)?;
    }
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
    insert_event(
        &tx,
        &record.session_id,
        &record.event,
        record.raw_ref.as_ref(),
    )?;
    before_commit("store.commit.event")?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
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
    insert_event(&tx, session, closed, None)?;
    tx.execute(
        "UPDATE sessions SET state='closed' WHERE id=?1",
        [session.as_str()],
    )
    .map_err(sql_error)?;
    before_commit("store.commit.session_closed")?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))?;
    Ok(true)
}

/// Commits the terminal and, when `closed` is given, the session's
/// `session.closed` event after it, in the same transaction. `extras` adds
/// the turn's `cancel_cause` and a `raw_log.incomplete` event before
/// `turn.ended` (design §7.2 row 6, §10).
fn commit_terminal(
    conn: &mut Connection,
    root: &Path,
    record: &TerminalRecord,
    extras: &TerminalExtras,
    closed: Option<&Value>,
) -> Result<bool, StoreError> {
    if let Some(reference) = &record.raw_ref {
        validate_raw_ref(root, reference)?;
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    // A queued turn can only be cancelled: that is `store.commit.cancel`.
    let queued = turn_state(&tx, &record.session_id, record.turn)?.as_deref() == Some("queued");
    let closed = insert_terminal(&tx, record, extras, closed)?;
    before_commit(if queued {
        "store.commit.cancel"
    } else {
        "store.commit.terminal"
    })?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))?;
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

/// Writes one terminal inside `tx`: its `raw_log.incomplete` event, the
/// turn's state, envelope and `cancel_cause`, `turn.ended` and, with
/// `closed`, `session.closed` when no other turn of the session is queued or
/// running. Returns whether the close was written.
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
    if let Some(incomplete) = &extras.raw_incomplete {
        // Only a running turn has a connection whose raw log can be incomplete.
        if turn_state(tx, &record.session_id, record.turn)?.as_deref() != Some("running") {
            return Err(StoreError::Constraint("turn is not running"));
        }
        insert_event(tx, &record.session_id, incomplete, None)?;
    }
    let changed = tx
        .execute(
            "UPDATE turns SET state=?3,envelope=?4,cancel_cause=?5 WHERE session_id=?1 AND number=?2 AND (?3!='completed' OR correlation IS NOT NULL) AND (state='running' OR (state='queued' AND ?3='cancelled'))",
            params![
                record.session_id.as_str(),
                record.turn.get(),
                state,
                envelope,
                extras.cancel_cause.map(CancelCause::as_str)
            ],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StoreError::Constraint("turn is not running"));
    }
    insert_event(
        tx,
        &record.session_id,
        &record.event,
        record.raw_ref.as_ref(),
    )?;
    // `session.closed` only when no other turn of the session is queued or
    // running (this turn is already terminal); otherwise the terminal commits
    // alone and the close is reported as not written.
    let closed = match closed {
        Some(closed) => {
            // Design §10 [r3.6]: the rider seam fails the combined
            // transaction after the terminal insert.
            before_commit("store.commit.rider")?;
            if unfinished_turns(tx, &record.session_id)? {
                false
            } else {
                insert_event(tx, &record.session_id, closed, None)?;
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
            "INSERT INTO operations(session_id,op_key,verb,identity,turn,result) VALUES (?1,?2,'close',?3,NULL,NULL)",
            params![session.as_str(), operation.op_key, operation.identity],
        )
        .map_err(sql_error)?;
    }
    before_commit("store.commit.closing")?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
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
    insert_event(&tx, session, &record.event, None)?;
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
        let completed = tx
            .execute(
                "UPDATE operations SET result=?4 WHERE session_id=?1 AND op_key=?2 AND verb='close' AND identity=?3 AND result IS NULL",
                params![session.as_str(), operation.op_key, operation.identity, encoded],
            )
            .map_err(sql_error)?;
        if completed == 0 {
            tx.execute(
                "INSERT INTO operations(session_id,op_key,verb,identity,turn,result) VALUES (?1,?2,'close',?3,NULL,?4)",
                params![session.as_str(), operation.op_key, operation.identity, encoded],
            )
            .map_err(sql_error)?;
        }
    }
    before_commit("store.commit.closed")?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))?;
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
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let changed = tx
        .execute(
            "UPDATE turns SET state='failed',submitted_at=?3,envelope=?4 WHERE session_id=?1 AND number=?2 AND state='queued'",
            params![session.as_str(), record.turn.get(), at, envelope],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StoreError::Constraint("turn is not queued"));
    }
    insert_event(&tx, session, &record.submitted, None)?;
    insert_event(&tx, session, &record.ended, None)?;
    update_session_state(&tx, session, false)?;
    before_commit("store.commit.terminal")?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
}

/// Design §7.4: one running turn's terminal, then its session's queued
/// cancellations, in one transaction and within the event bound.
fn commit_failure_resolution(
    conn: &mut Connection,
    root: &Path,
    record: &FailureResolutionRecord,
) -> Result<(), StoreError> {
    let session = &record.terminal.session_id;
    if record.cancellations.len() > FAILURE_BATCH_CANCELLATIONS {
        return Err(StoreError::Constraint(
            "a failure batch carries at most 8 cancellations",
        ));
    }
    if let Some(reference) = &record.terminal.raw_ref {
        validate_raw_ref(root, reference)?;
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let extras = TerminalExtras {
        cancel_cause: None,
        raw_incomplete: record.raw_incomplete.clone(),
    };
    insert_terminal(&tx, &record.terminal, &extras, None)?;
    for cancellation in &record.cancellations {
        if cancellation.session_id != *session
            || cancellation.envelope.get("state").and_then(Value::as_str) != Some("cancelled")
            || turn_state(&tx, session, cancellation.turn)?.as_deref() != Some("queued")
        {
            return Err(StoreError::Constraint(
                "a failure batch cancels only its session's queued turns",
            ));
        }
        insert_terminal(&tx, cancellation, &TerminalExtras::default(), None)?;
    }
    before_commit("store.commit.terminal")?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
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

fn read_result(
    conn: &Connection,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<Option<Value>, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT envelope FROM turns WHERE session_id=?1 AND number=?2",
            params![session.as_str(), turn.get()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?
        .flatten();
    raw.map(|value| serde_json::from_str(&value).map_err(|_| StoreError::CorruptEvidence))
        .transpose()
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
        .prepare("SELECT seq,event,connection_id,raw_offset,raw_len FROM events WHERE session_id=?1 AND seq>=?2 ORDER BY seq LIMIT ?3")
        .map_err(sql_error)?;
    let rows = query
        .query_map(params![session.as_str(), from, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(sql_error)?;
    let mut events = Vec::new();
    for row in rows {
        let (seq, event, connection, offset, len) = row.map_err(sql_error)?;
        let raw_ref = match (connection, offset, len) {
            (Some(connection), Some(offset), Some(len)) => Some(
                RawRef::new(
                    ConnectionId::try_from(connection.as_str())
                        .map_err(|_| StoreError::CorruptEvidence)?,
                    u64::try_from(offset).map_err(|_| StoreError::CorruptEvidence)?,
                    u32::try_from(len).map_err(|_| StoreError::CorruptEvidence)?,
                )
                .map_err(|_| StoreError::CorruptEvidence)?,
            ),
            (None, None, None) => None,
            _ => return Err(StoreError::CorruptEvidence),
        };
        events.push(StoredEvent {
            seq: u64::try_from(seq).map_err(|_| StoreError::CorruptEvidence)?,
            event: serde_json::from_str(&event).map_err(|_| StoreError::CorruptEvidence)?,
            raw_ref,
        });
    }
    Ok(events)
}

fn read_logs(conn: &Connection, root: &Path, session: &SessionId) -> Result<Value, StoreError> {
    let events = read_events(conn, session, 1, 1000)?;
    let mut entries = Vec::new();
    let mut total = 0_usize;
    let mut next_after = 0_u64;
    for event in events {
        let Some(reference) = event.raw_ref else {
            continue;
        };
        let (stream, bytes) = read_raw_ref(root, &reference)?;
        let text = String::from_utf8_lossy(&bytes);
        let entry = serde_json::json!({
            "seq":event.seq,
            "direction":match stream { RawStream::Stdin => "input", RawStream::Stdout | RawStream::Stderr => "output" },
            "connection_id":reference.connection_id().as_str(),
            "offset":reference.offset(),"len":reference.byte_len(),"text":text,
        });
        let encoded = serde_json::to_vec(&entry)
            .map_err(|error| StoreError::Write(error.to_string()))?
            .len();
        if total.saturating_add(encoded) > RAW_UNIT_LIMIT.saturating_sub(64) {
            return Err(StoreError::Constraint("raw log response exceeds 1 MiB"));
        }
        total += encoded;
        next_after = event.seq;
        entries.push(entry);
    }
    Ok(serde_json::json!({"entries":entries,"next_after":next_after}))
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
