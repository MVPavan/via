//! Keyed `steer` operation rows (C1 §3, §3.4; runtime §6): the intent row a
//! keyed steer writes before its input goes to the driver, and its one
//! outcome, recorded with the `steer.delivered` event that reports its
//! delivery, alone for a refusal, or by restart recovery.

use super::{
    Connection, OptionalExtension, SessionId, SteerIntent, SteerOutcome, StoreError,
    TransactionBehavior, Value, params,
    sql::{before_commit, commit, identity_len, json, sql_error},
};

/// Inserts a keyed steer's intent row with no result. A key the session
/// already holds, of any verb, violates the primary key: not committed.
pub(super) fn commit_steer_intent(
    conn: &mut Connection,
    intent: &SteerIntent,
) -> Result<(), StoreError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM operations WHERE session_id=?1 AND op_key=?2",
            params![intent.session_id.as_str(), intent.op_key],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if exists.is_some() {
        return Err(StoreError::Constraint("operation key already recorded"));
    }
    tx.execute(
        "INSERT INTO operations(session_id,op_key,verb,identity_len,identity_sha256,turn,result) VALUES (?1,?2,'steer',?3,?4,NULL,NULL)",
        params![
            intent.session_id.as_str(),
            intent.op_key,
            identity_len(&intent.identity)?,
            &intent.identity.sha256[..]
        ],
    )
    .map_err(sql_error)?;
    before_commit!("store.commit.steer_intent");
    commit(tx)
}

/// Records `outcome` on the session's open steer intent row in `tx`; the
/// caller's transaction makes it atomic with what it commits, such as the
/// `steer.delivered` event. Exactly that one row is updated: a missing
/// key, another verb's key or a resolved intent is refused, and the
/// caller's transaction rolls back (K2 r1 #2).
pub(super) fn record_steer_outcome(
    tx: &rusqlite::Transaction<'_>,
    session: &SessionId,
    outcome: &SteerOutcome,
) -> Result<(), StoreError> {
    let updated = tx
        .execute(
            "UPDATE operations SET result=?3 WHERE session_id=?1 AND op_key=?2 AND verb='steer' AND result IS NULL",
            params![session.as_str(), outcome.op_key, json(&outcome.result)?],
        )
        .map_err(sql_error)?;
    if updated != 1 {
        return Err(StoreError::Constraint("no open steer intent under the key"));
    }
    Ok(())
}

/// Records a keyed steer's outcome alone, on its open intent row
/// ([`record_steer_outcome`]).
pub(super) fn commit_steer_outcome(
    conn: &mut Connection,
    session: &SessionId,
    outcome: &SteerOutcome,
) -> Result<(), StoreError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    record_steer_outcome(&tx, session, outcome)?;
    before_commit!("store.commit.steer_outcome");
    commit(tx)
}

/// Records `result` as the outcome of every steer intent row without one,
/// in one transaction; returns how many. Restart recovery's, before
/// admission, when no attempt can still record an outcome; the partial
/// index `operations_open_steers` finds the rows.
pub(super) fn resolve_steer_intents(
    conn: &mut Connection,
    result: &Value,
) -> Result<u64, StoreError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let resolved = tx
        .execute(
            "UPDATE operations SET result=?1 WHERE verb='steer' AND result IS NULL",
            [json(result)?],
        )
        .map_err(sql_error)?;
    commit(tx)?;
    u64::try_from(resolved).map_err(|_| StoreError::Constraint("too many steer intents"))
}
