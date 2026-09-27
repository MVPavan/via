//! Durable anchor identity, ARM intent and absence journal.

use super::{
    AnchorIdentity, AnchorIntent, AnchorIntentReceipt, AnchorOwner, AnchorPhase, AnchorRecord,
    Connection, GroupAbsenceRecord, PathBuf, SessionId, StoreError, TransactionBehavior,
    TurnNumber, params,
};

pub(super) fn commit_anchor_intent(
    conn: &mut Connection,
    intent: &AnchorIntent,
) -> Result<AnchorIntentReceipt, StoreError> {
    if intent.anchor_id.is_empty()
        || intent.generation.is_empty()
        || intent.marker.is_empty()
        || intent.boot_id.is_empty()
        || intent.pid_namespace.is_empty()
        || intent.socket_path.as_os_str().is_empty()
    {
        return Err(StoreError::Constraint("anchor intent missing identity"));
    }
    let socket_path = intent
        .socket_path
        .to_str()
        .ok_or(StoreError::Constraint("socket path is not UTF-8"))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| StoreError::Write(error.to_string()))?;
    tx.execute(
        "INSERT INTO anchors(anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,phase,record_version)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'intent',1)",
        params![intent.anchor_id,intent.generation,intent.marker,socket_path,
            intent.owner_session.as_str(),intent.owner_turn.get(),intent.uid,intent.boot_id,intent.pid_namespace],
    ).map_err(|error| StoreError::Write(error.to_string()))?;
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))?;
    Ok(AnchorIntentReceipt { record_version: 1 })
}

pub(super) fn commit_anchor_identified(
    conn: &mut Connection,
    id: &str,
    generation: &str,
    version: u64,
    identity: &AnchorIdentity,
) -> Result<u64, StoreError> {
    if identity.pid <= 1 || identity.pgid <= 1 || identity.start_ticks == 0 {
        return Err(StoreError::Constraint("invalid anchor process identity"));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let changed = tx.execute(
        "UPDATE anchors SET pid=?4,pgid=?5,start_ticks=?6,phase='identified',record_version=record_version+1
         WHERE anchor_id=?1 AND generation=?2 AND record_version=?3 AND phase='intent'
           AND uid=?7 AND boot_id=?8 AND pid_namespace=?9 AND marker=?10",
        params![id,generation,i64::try_from(version).map_err(|_| StoreError::Constraint("anchor version overflow"))?,identity.pid,identity.pgid,i64::try_from(identity.start_ticks).map_err(|_| StoreError::Constraint("start ticks overflow"))?,
            identity.uid,identity.boot_id,identity.pid_namespace,identity.marker],
    ).map_err(|error| StoreError::Write(error.to_string()))?;
    if changed != 1 {
        return Err(StoreError::Constraint("anchor identity/version mismatch"));
    }
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))?;
    version
        .checked_add(1)
        .ok_or(StoreError::Constraint("anchor version overflow"))
}

pub(super) fn commit_arm_intent(
    conn: &mut Connection,
    id: &str,
    generation: &str,
    version: u64,
) -> Result<u64, StoreError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let changed = tx
        .execute(
            "UPDATE anchors SET phase='arm_intent',record_version=record_version+1
         WHERE anchor_id=?1 AND generation=?2 AND record_version=?3 AND phase='identified'
           AND pid>1 AND pgid>1 AND start_ticks>0",
            params![
                id,
                generation,
                i64::try_from(version)
                    .map_err(|_| StoreError::Constraint("anchor version overflow"))?
            ],
        )
        .map_err(|error| StoreError::Write(error.to_string()))?;
    if changed != 1 {
        return Err(StoreError::Constraint(
            "ARM intent requires committed identity/version",
        ));
    }
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))?;
    version
        .checked_add(1)
        .ok_or(StoreError::Constraint("anchor version overflow"))
}

pub(super) fn commit_vendor_facts(
    conn: &mut Connection,
    id: &str,
    generation: &str,
    pid: u32,
) -> Result<(), StoreError> {
    if pid <= 1 {
        return Err(StoreError::Constraint("invalid vendor PID"));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let changed = tx.execute(
        "UPDATE anchors SET vendor_pid=?3 WHERE anchor_id=?1 AND generation=?2 AND phase='arm_intent' AND vendor_pid IS NULL",
        params![id,generation,pid],
    ).map_err(|error| StoreError::Write(error.to_string()))?;
    if changed != 1 {
        return Err(StoreError::Constraint("vendor facts require ARM intent"));
    }
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
}

pub(super) fn commit_group_absence(
    conn: &mut Connection,
    proof: &GroupAbsenceRecord,
) -> Result<(), StoreError> {
    if proof.pgid <= 1 || proof.observed_at.is_empty() {
        return Err(StoreError::Constraint("invalid group absence evidence"));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let changed = tx
        .execute(
            "UPDATE anchors SET absence_time=?6 WHERE anchor_id=?1 AND generation=?2
         AND boot_id=?3 AND pid_namespace=?4 AND pgid=?5 AND phase IN ('identified','arm_intent')",
            params![
                proof.anchor_id,
                proof.generation,
                proof.boot_id,
                proof.pid_namespace,
                proof.pgid,
                proof.observed_at
            ],
        )
        .map_err(|error| StoreError::Write(error.to_string()))?;
    if changed != 1 {
        return Err(StoreError::Constraint("group absence identity mismatch"));
    }
    tx.commit()
        .map_err(|error| StoreError::Uncertain(error.to_string()))
}

/// One page of anchor records in `anchor_id` order after `after`.
pub(super) fn read_anchor_records(
    conn: &Connection,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<AnchorRecord>, StoreError> {
    let mut query = conn.prepare(
        "SELECT anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,
                phase,record_version,pid,pgid,start_ticks,vendor_pid,absence_time FROM anchors
                WHERE ?1 IS NULL OR anchor_id>?1 ORDER BY anchor_id LIMIT ?2"
    ).map_err(|error| StoreError::Write(error.to_string()))?;
    let rows = query
        .query_map(params![after, limit], |row| {
            let owner: String = row.get(4)?;
            let owner_session =
                SessionId::try_from(owner.as_str()).map_err(|_| rusqlite::Error::InvalidQuery)?;
            let owner_turn = TurnNumber::try_from(row.get::<_, u32>(5)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            let intent = AnchorIntent {
                anchor_id: row.get(0)?,
                generation: row.get(1)?,
                marker: row.get(2)?,
                socket_path: PathBuf::from(row.get::<_, String>(3)?),
                owner_session,
                owner_turn,
                uid: row.get(6)?,
                boot_id: row.get(7)?,
                pid_namespace: row.get(8)?,
            };
            let phase = AnchorPhase::parse(&row.get::<_, String>(9)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            let anchor_pid: Option<u32> = row.get(11)?;
            let group_id: Option<u32> = row.get(12)?;
            let start_ticks: Option<i64> = row.get(13)?;
            let identity = match (anchor_pid, group_id, start_ticks) {
                (Some(anchor_pid), Some(group_id), Some(start_ticks)) => Some(AnchorIdentity {
                    pid: anchor_pid,
                    pgid: group_id,
                    uid: intent.uid,
                    boot_id: intent.boot_id.clone(),
                    pid_namespace: intent.pid_namespace.clone(),
                    start_ticks: u64::try_from(start_ticks)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    marker: intent.marker.clone(),
                }),
                (None, None, None) => None,
                _ => return Err(rusqlite::Error::InvalidQuery),
            };
            let absence_time: Option<String> = row.get(15)?;
            let absence = absence_time.map(|observed_at| GroupAbsenceRecord {
                anchor_id: intent.anchor_id.clone(),
                generation: intent.generation.clone(),
                boot_id: intent.boot_id.clone(),
                pid_namespace: intent.pid_namespace.clone(),
                pgid: group_id.unwrap_or_default(),
                observed_at,
            });
            Ok(AnchorRecord {
                intent,
                identity,
                phase,
                record_version: u64::try_from(row.get::<_, i64>(10)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                vendor_pid: row.get(14)?,
                absence,
            })
        })
        .map_err(|error| StoreError::Write(error.to_string()))?;
    rows.map(|row| row.map_err(|error| StoreError::Write(error.to_string())))
        .collect()
}

/// Committed anchors after `after` in `anchor_id` order with no absence proof.
pub(super) fn count_unproven_anchors(
    conn: &Connection,
    after: Option<&str>,
) -> Result<u64, StoreError> {
    conn.query_row(
        "SELECT count(*) FROM anchors WHERE (?1 IS NULL OR anchor_id>?1) AND absence_time IS NULL",
        params![after],
        |row| row.get::<_, i64>(0),
    )
    .map(i64::cast_unsigned)
    .map_err(|error| StoreError::Write(error.to_string()))
}

/// One page of committed anchors, in `anchor_id` order after `after`, with
/// their owning turns, for recovery's coverage check; no marker, identity or
/// control path.
pub(super) fn read_anchor_owners(
    conn: &Connection,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<AnchorOwner>, StoreError> {
    let mut query = conn
        .prepare(
            "SELECT a.anchor_id,a.owner_session,a.owner_turn,t.state='running' FROM anchors a
             JOIN turns t ON t.session_id=a.owner_session AND t.number=a.owner_turn
             WHERE ?1 IS NULL OR a.anchor_id>?1 ORDER BY a.anchor_id LIMIT ?2",
        )
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let rows = query
        .query_map(params![after, limit], |row| {
            let owner: String = row.get(1)?;
            let session =
                SessionId::try_from(owner.as_str()).map_err(|_| rusqlite::Error::InvalidQuery)?;
            let turn = TurnNumber::try_from(row.get::<_, u32>(2)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            Ok(AnchorOwner {
                anchor_id: row.get(0)?,
                session_id: session,
                turn,
                turn_running: row.get(3)?,
            })
        })
        .map_err(|error| StoreError::Write(error.to_string()))?;
    rows.map(|row| row.map_err(|error| StoreError::Write(error.to_string())))
        .collect()
}
