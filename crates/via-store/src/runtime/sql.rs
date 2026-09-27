//! SQLite migration and single-writer transaction implementation.

use super::{
    AcceptanceRecord, Command, CommitOutcome, Connection, ConnectionId, Duration, EventRecord,
    MetadataExt, OptionalExtension, Path, RAW_UNIT_LIMIT, RawRef, RawStream, ReceiptRecord,
    Receiver, SCHEMA_VERSION, SessionId, SpawnRecord, StoreError, StoreFailureKind, StoredEvent,
    SubmissionRecord, TerminalRecord, TransactionBehavior, TurnNumber, UnfinishedTurn, Value,
    commit_anchor_identified, commit_anchor_intent, commit_arm_intent, commit_group_absence,
    commit_vendor_facts, fs, oneshot, params, read_anchor_records, read_raw_ref, validate_raw_ref,
};

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

pub(super) fn configure(conn: &mut Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(|error| StoreError::Open(error.to_string()))?;
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
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| StoreError::Open(error.to_string()))?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::Open("newer Store schema".to_owned()));
    }
    if version == 0 {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| StoreError::Open(error.to_string()))?;
        tx.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, handle_hash BLOB NOT NULL CHECK(length(handle_hash)=32),
                receipt TEXT NOT NULL, params TEXT NOT NULL, state TEXT NOT NULL,
                next_seq INTEGER NOT NULL CHECK(next_seq>=2));
             CREATE TABLE turns (
                session_id TEXT NOT NULL REFERENCES sessions(id), number INTEGER NOT NULL,
                prompt TEXT NOT NULL, state TEXT NOT NULL, submitted_at TEXT,
                accepted_at TEXT, correlation TEXT, envelope TEXT,
                PRIMARY KEY(session_id,number));
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
             PRAGMA user_version=1;",
        )
        .map_err(|error| StoreError::Open(error.to_string()))?;
        tx.commit()
            .map_err(|error| StoreError::Uncertain(error.to_string()))?;
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "one arm per variant of the closed Store command enum"
)]
pub(super) fn writer_loop(mut conn: Connection, root: &Path, receiver: &Receiver<Command>) {
    let mut commits = 0_u32;
    while let Ok(command) = receiver.recv() {
        let wrote = match command {
            Command::Spawn(record, reply) => {
                send_commit(reply, commit_spawn(&mut conn, record));
                true
            }
            Command::Submission(record, reply) => {
                send_commit(reply, commit_submission(&mut conn, &record));
                true
            }
            Command::Acceptance(record, reply) => {
                send_commit(reply, commit_acceptance(&mut conn, root, &record));
                true
            }
            Command::Event(record, reply) => {
                send_commit(reply, commit_event(&mut conn, root, &record));
                true
            }
            Command::Terminal(record, reply) => {
                send_commit(reply, commit_terminal(&mut conn, root, &record, None));
                true
            }
            Command::ClosingTerminal(record, closed, reply) => {
                send_commit(
                    reply,
                    commit_terminal(&mut conn, root, &record, Some(&closed)),
                );
                true
            }
            Command::Result(session, turn, reply) => {
                let _ = reply.send(read_result(&conn, &session, turn));
                false
            }
            Command::Terminated(turns, reply) => {
                let _ = reply.send(read_terminated(&conn, turns));
                false
            }
            Command::Events(session, from, limit, reply) => {
                let _ = reply.send(read_events(&conn, &session, from, limit));
                false
            }
            Command::Logs(session, reply) => {
                let _ = reply.send(read_logs(&conn, root, &session));
                false
            }
            Command::Unfinished(reply) => {
                let _ = reply.send(read_unfinished(&conn));
                false
            }
            Command::Authenticate(session, hash, reply) => {
                let _ = reply.send(authenticate(&conn, &session, &hash));
                false
            }
            Command::AnchorIntent(intent, reply) => {
                let _ = reply.send(as_commit(commit_anchor_intent(&mut conn, &intent)));
                true
            }
            Command::AnchorIdentified(id, generation, version, identity, reply) => {
                let _ = reply.send(as_commit(commit_anchor_identified(
                    &mut conn,
                    &id,
                    &generation,
                    version,
                    &identity,
                )));
                true
            }
            Command::ArmIntent(id, generation, version, reply) => {
                let _ = reply.send(as_commit(commit_arm_intent(
                    &mut conn,
                    &id,
                    &generation,
                    version,
                )));
                true
            }
            Command::VendorFacts(id, generation, pid, reply) => {
                let _ = reply.send(as_commit(commit_vendor_facts(
                    &mut conn,
                    &id,
                    &generation,
                    pid,
                )));
                true
            }
            Command::GroupAbsence(proof, reply) => {
                let _ = reply.send(as_commit(commit_group_absence(&mut conn, &proof)));
                true
            }
            Command::AnchorRecords(reply) => {
                let _ = reply.send(read_anchor_records(&conn).map_err(|error| error.kind()));
                false
            }
            Command::Shutdown => break,
        };
        if wrote {
            commits += 1;
            if commits >= 1000 {
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
                commits = 0;
            }
        }
    }
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
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
        Err(StoreError::Uncertain(_)) => {
            CommitOutcome::Uncertain(StoreFailureKind::UncertainCommit)
        }
        Err(error) => CommitOutcome::NotCommitted(error.kind()),
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

fn commit_spawn(conn: &mut Connection, record: SpawnRecord) -> Result<ReceiptRecord, StoreError> {
    if seq(&record.initial_event)? != 1 {
        return Err(StoreError::Constraint("initial event sequence must be one"));
    }
    let receipt = json(&record.receipt)?;
    let params_json = json(&record.params)?;
    let event = json(&record.initial_event)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| StoreError::Write(error.to_string()))?;
    tx.execute(
        "INSERT INTO sessions(id,handle_hash,receipt,params,state,next_seq) VALUES (?1,?2,?3,?4,'active',2)",
        params![record.session_id.as_str(), &record.handle_hash[..], receipt, params_json],
    )
    .map_err(|error| StoreError::Write(error.to_string()))?;
    tx.execute(
        "INSERT INTO turns(session_id,number,prompt,state) VALUES (?1,1,?2,'queued')",
        params![record.session_id.as_str(), record.prompt],
    )
    .map_err(|error| StoreError::Write(error.to_string()))?;
    tx.execute(
        "INSERT INTO events(session_id,seq,event) VALUES (?1,1,?2)",
        params![record.session_id.as_str(), event],
    )
    .map_err(|error| StoreError::Write(error.to_string()))?;
    // Every row is written but uncommitted: none may survive a crash here.
    #[cfg(feature = "test-failpoints")]
    crate::failpoint::hit("store.spawn.before_commit")
        .map_err(|error| StoreError::Write(error.to_string()))?;
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

fn commit_submission(conn: &mut Connection, record: &SubmissionRecord) -> Result<(), StoreError> {
    let session = &record.session_id;
    let at = event_at(&record.event)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let changed = tx
        .execute(
            "UPDATE turns SET state='running',submitted_at=?3 WHERE session_id=?1 AND number=?2 AND state='queued'",
            params![session.as_str(), record.turn.get(), at],
        )
        .map_err(|error| StoreError::Write(error.to_string()))?;
    if changed != 1 {
        return Err(StoreError::Constraint("turn is not queued"));
    }
    insert_event(&tx, session, &record.event, None)?;
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
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let existing: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT state,correlation FROM turns WHERE session_id=?1 AND number=?2",
            params![session.as_str(), turn.get()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| StoreError::Write(error.to_string()))?;
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
    .map_err(|error| StoreError::Write(error.to_string()))?;
    insert_event(&tx, session, &record.event, Some(&record.raw_ref))?;
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
        .map_err(|error| StoreError::Write(error.to_string()))?;
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
    .map_err(|error| StoreError::Write(error.to_string()))?;
    tx.execute(
        "UPDATE sessions SET next_seq=next_seq+1 WHERE id=?1",
        [session.as_str()],
    )
    .map_err(|error| StoreError::Write(error.to_string()))?;
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
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let state: Option<String> = tx
        .query_row(
            "SELECT state FROM turns WHERE session_id=?1 AND number=?2",
            params![record.session_id.as_str(), record.turn.get()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| StoreError::Write(error.to_string()))?;
    if state.as_deref() != Some("running") {
        return Err(StoreError::Constraint("turn is not running"));
    }
    insert_event(
        &tx,
        &record.session_id,
        &record.event,
        record.raw_ref.as_ref(),
    )?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
}

/// Commits the terminal and, when `closed` is given, the session's
/// `session.closed` event after it, in the same transaction.
fn commit_terminal(
    conn: &mut Connection,
    root: &Path,
    record: &TerminalRecord,
    closed: Option<&Value>,
) -> Result<(), StoreError> {
    let state = record
        .envelope
        .get("state")
        .and_then(Value::as_str)
        .ok_or(StoreError::Constraint("terminal state missing"))?;
    if !matches!(state, "completed" | "failed" | "cancelled" | "unknown") {
        return Err(StoreError::Constraint("terminal state invalid"));
    }
    if let Some(reference) = &record.raw_ref {
        validate_raw_ref(root, reference)?;
    }
    let envelope = json(&record.envelope)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let changed = tx
        .execute(
            "UPDATE turns SET state=?3,envelope=?4 WHERE session_id=?1 AND number=?2 AND state='running' AND (?3!='completed' OR correlation IS NOT NULL)",
            params![record.session_id.as_str(), record.turn.get(), state, envelope],
        )
        .map_err(|error| StoreError::Write(error.to_string()))?;
    if changed != 1 {
        return Err(StoreError::Constraint("turn is not running"));
    }
    insert_event(
        &tx,
        &record.session_id,
        &record.event,
        record.raw_ref.as_ref(),
    )?;
    if let Some(closed) = closed {
        insert_event(&tx, &record.session_id, closed, None)?;
    }
    tx.execute(
        "UPDATE sessions SET state=?2 WHERE id=?1",
        params![
            record.session_id.as_str(),
            if closed.is_some() { "closed" } else { "idle" }
        ],
    )
    .map_err(|error| StoreError::Write(error.to_string()))?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
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
        .map_err(|error| StoreError::Write(error.to_string()))?
        .flatten();
    raw.map(|value| serde_json::from_str(&value).map_err(|_| StoreError::CorruptEvidence))
        .transpose()
}

fn read_unfinished(conn: &Connection) -> Result<Vec<UnfinishedTurn>, StoreError> {
    let mut statement = conn
        .prepare_cached(
            "SELECT session_id,number,submitted_at,correlation FROM turns WHERE state='running' ORDER BY session_id,number LIMIT 1000",
        )
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let mut turns = Vec::new();
    for row in rows {
        let (session, number, submitted_at, correlation) =
            row.map_err(|error| StoreError::Write(error.to_string()))?;
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
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let mut terminated = Vec::new();
    for (session, turn) in turns {
        let found = statement
            .exists(params![session.as_str(), turn.get()])
            .map_err(|error| StoreError::Write(error.to_string()))?;
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
        .map_err(|error| StoreError::Write(error.to_string()))?;
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
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let mut events = Vec::new();
    for row in rows {
        let (seq, event, connection, offset, len) =
            row.map_err(|error| StoreError::Write(error.to_string()))?;
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
        .map_err(|error| StoreError::Write(error.to_string()))?;
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
