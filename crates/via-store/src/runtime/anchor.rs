//! Durable anchor identity, ARM intent and absence journal.

use super::{
    AnchorCohort, AnchorIdentity, AnchorIntent, AnchorIntentReceipt, AnchorOwner, AnchorPhase,
    AnchorQuery, AnchorRecord, Connection, GroupAbsenceRecord, OptionalExtension, PathBuf,
    ProcessOwner, SERVER_LINKS_LIMIT, ServerLink, SessionId, StoreError, TransactionBehavior,
    TurnNumber, params,
    sql::{before_commit, commit, sql_error},
};
use crate::ServerId;

/// An anchor row's owner columns, consecutive from `first`, as one
/// [`ProcessOwner`].
fn owner_at(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<ProcessOwner> {
    owner_columns(row, first, first + 1, first + 2)
}

/// An anchor row's owner columns as one [`ProcessOwner`]; any other
/// combination is a corrupt row.
fn owner_columns(
    row: &rusqlite::Row<'_>,
    session: usize,
    turn: usize,
    server: usize,
) -> rusqlite::Result<ProcessOwner> {
    let session: Option<String> = row.get(session)?;
    let turn: Option<u32> = row.get(turn)?;
    let server: Option<String> = row.get(server)?;
    match (session, turn, server) {
        (Some(session), Some(turn), None) => Ok(ProcessOwner::Turn {
            session_id: SessionId::try_from(session.as_str())
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            turn: TurnNumber::try_from(turn).map_err(|_| rusqlite::Error::InvalidQuery)?,
        }),
        (None, None, Some(server)) => Ok(ProcessOwner::Server {
            server_id: ServerId::try_from(server.as_str())
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
        }),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

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
    let (session, turn, server) = match &intent.owner {
        ProcessOwner::Turn { session_id, turn } => {
            (Some(session_id.as_str()), Some(turn.get()), None)
        }
        ProcessOwner::Server { server_id } => (None, None, Some(server_id.as_str())),
    };
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    tx.execute(
        "INSERT INTO anchors(anchor_id,generation,marker,socket_path,owner_session,owner_turn,owner_server,uid,boot_id,pid_namespace,phase,record_version)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'intent',1)",
        params![intent.anchor_id,intent.generation,intent.marker,socket_path,
            session,turn,server,intent.uid,intent.boot_id,intent.pid_namespace],
    ).map_err(sql_error)?;
    before_commit!("store.journal.anchor_intent");
    commit(tx)?;
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
        .map_err(sql_error)?;
    let changed = tx.execute(
        "UPDATE anchors SET pid=?4,pgid=?5,start_ticks=?6,phase='identified',record_version=record_version+1
         WHERE anchor_id=?1 AND generation=?2 AND record_version=?3 AND phase='intent'
           AND uid=?7 AND boot_id=?8 AND pid_namespace=?9 AND marker=?10",
        params![id,generation,i64::try_from(version).map_err(|_| StoreError::Constraint("anchor version overflow"))?,identity.pid,identity.pgid,i64::try_from(identity.start_ticks).map_err(|_| StoreError::Constraint("start ticks overflow"))?,
            identity.uid,identity.boot_id,identity.pid_namespace,identity.marker],
    ).map_err(sql_error)?;
    if changed != 1 {
        return Err(StoreError::Constraint("anchor identity/version mismatch"));
    }
    before_commit!("store.journal.identified");
    commit(tx)?;
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
        .map_err(sql_error)?;
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
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StoreError::Constraint(
            "ARM intent requires committed identity/version",
        ));
    }
    before_commit!("store.journal.arm_intent");
    commit(tx)?;
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
        .map_err(sql_error)?;
    let changed = tx.execute(
        "UPDATE anchors SET vendor_pid=?3 WHERE anchor_id=?1 AND generation=?2 AND phase='arm_intent' AND vendor_pid IS NULL",
        params![id,generation,pid],
    ).map_err(sql_error)?;
    if changed != 1 {
        return Err(StoreError::Constraint("vendor facts require ARM intent"));
    }
    before_commit!("store.journal.vendor_facts");
    commit(tx)
}

/// Records a group absence proof. Without an identity the anchor must
/// already have its committed identity, whose group was probed. With one
/// (design §7.2 row 4), an anchor still at `intent` phase records it, and a
/// later phase must match it; either way it must match the intent's owner
/// facts and private marker.
pub(super) fn commit_group_absence(
    conn: &mut Connection,
    proof: &GroupAbsenceRecord,
) -> Result<(), StoreError> {
    if proof.pgid <= 1 || proof.observed_at.is_empty() {
        return Err(StoreError::Constraint("invalid group absence evidence"));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let changed = match &proof.identity {
        None => tx
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
            .map_err(sql_error)?,
        Some(identity) => {
            if identity.pid <= 1
                || identity.pgid != proof.pgid
                || identity.start_ticks == 0
                || identity.boot_id != proof.boot_id
                || identity.pid_namespace != proof.pid_namespace
            {
                return Err(StoreError::Constraint("invalid group absence identity"));
            }
            let ticks = i64::try_from(identity.start_ticks)
                .map_err(|_| StoreError::Constraint("start ticks overflow"))?;
            tx.execute(
                "UPDATE anchors SET absence_time=?6,pid=coalesce(pid,?7),pgid=coalesce(pgid,?5),
                    start_ticks=coalesce(start_ticks,?8)
                 WHERE anchor_id=?1 AND generation=?2 AND boot_id=?3 AND pid_namespace=?4
                   AND uid=?9 AND marker=?10 AND phase IN ('intent','identified','arm_intent')
                   AND (pid IS NULL OR (pid=?7 AND pgid=?5 AND start_ticks=?8))",
                params![
                    proof.anchor_id,
                    proof.generation,
                    proof.boot_id,
                    proof.pid_namespace,
                    proof.pgid,
                    proof.observed_at,
                    identity.pid,
                    ticks,
                    identity.uid,
                    identity.marker
                ],
            )
            .map_err(sql_error)?
        }
    };
    if changed != 1 {
        return Err(StoreError::Constraint("group absence identity mismatch"));
    }
    before_commit!("store.journal.absence");
    commit(tx)
}

/// One page of anchor records in `anchor_id` order after `after`.
pub(super) fn read_anchor_records(
    conn: &Connection,
    page: &AnchorQuery,
) -> Result<Vec<AnchorRecord>, StoreError> {
    let mut query = conn.prepare(
        "SELECT anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,
                phase,record_version,pid,pgid,start_ticks,vendor_pid,absence_time,owner_server FROM anchors
                WHERE (?1 IS NULL OR anchor_id>?1) AND (?3=0 OR absence_time IS NULL)
                  AND (?4 IS NULL OR owner_session=?4) AND (?5 IS NULL OR rowid<=?5)
                ORDER BY anchor_id LIMIT ?2"
    ).map_err(sql_error)?;
    let rows = query
        .query_map(
            params![
                page.after,
                page.limit,
                page.unproven,
                page.owner.as_ref().map(SessionId::as_str),
                page.cohort.map(|cohort| cohort.0)
            ],
            |row| {
                let owner = owner_columns(row, 4, 5, 16)?;
                let intent = AnchorIntent {
                    anchor_id: row.get(0)?,
                    generation: row.get(1)?,
                    marker: row.get(2)?,
                    socket_path: PathBuf::from(row.get::<_, String>(3)?),
                    owner,
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
                    identity: None,
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
            },
        )
        .map_err(sql_error)?;
    rows.map(|row| row.map_err(sql_error)).collect()
}

/// Counts committed anchors after `after` with no absence proof, up to ?2,
/// only those in cohort ?3 when it is set.
pub(super) const UNPROVEN_ANCHORS_UP_TO: &str = "SELECT count(*) FROM (SELECT 1 FROM anchors
     WHERE anchor_id>?1 AND absence_time IS NULL AND (?3 IS NULL OR rowid<=?3)
     ORDER BY anchor_id LIMIT ?2)";

/// Committed anchors after `after` in `anchor_id` order with no absence proof.
/// Saturates at `limit`: a bounded range seek on the `anchors_unproven`
/// partial index, visiting at most `limit` unproven rows, never the whole
/// historical suffix. No anchor id is empty, so `''` starts at the first.
/// A cohort bound is checked on the index entries, which carry the rowid;
/// the seek then also passes the unproven anchors committed after the
/// cohort, at most one per live or held group of the current daemon.
pub(super) fn count_unproven_anchors(
    conn: &Connection,
    after: Option<&str>,
    limit: u32,
    cohort: Option<AnchorCohort>,
) -> Result<u64, StoreError> {
    conn.query_row(
        UNPROVEN_ANCHORS_UP_TO,
        params![after.unwrap_or(""), limit, cohort.map(|cohort| cohort.0)],
        |row| row.get::<_, i64>(0),
    )
    .map(i64::cast_unsigned)
    .map_err(sql_error)
}

/// One page of committed anchors, in `anchor_id` order after `after`, with
/// their owners, for recovery's coverage check; no marker, identity or
/// control path. A server anchor has no owning turn (`turn_running` false).
pub(super) fn read_anchor_owners(
    conn: &Connection,
    after: Option<&str>,
    limit: u32,
    cohort: Option<AnchorCohort>,
) -> Result<Vec<AnchorOwner>, StoreError> {
    let mut query = conn
        .prepare(
            "SELECT a.anchor_id,a.owner_session,a.owner_turn,a.owner_server,coalesce(t.state='running',0),a.phase FROM anchors a
             LEFT JOIN turns t ON t.session_id=a.owner_session AND t.number=a.owner_turn
             WHERE (?1 IS NULL OR a.anchor_id>?1) AND (?3 IS NULL OR a.rowid<=?3)
             ORDER BY a.anchor_id LIMIT ?2",
        )
        .map_err(sql_error)?;
    let rows = query
        .query_map(
            params![after, limit, cohort.map(|cohort| cohort.0)],
            |row| {
                Ok(AnchorOwner {
                    anchor_id: row.get(0)?,
                    owner: owner_at(row, 1)?,
                    turn_running: row.get(4)?,
                    phase: AnchorPhase::parse(&row.get::<_, String>(5)?).ok(),
                })
            },
        )
        .map_err(sql_error)?;
    rows.map(|row| row.map_err(sql_error)).collect()
}

/// Commits a turn's link to a server anchor (runtime §6 `server_turns`):
/// one `INSERT … SELECT` that inserts only for a server-owned anchor and a
/// `running` turn. Zero rows, or a second link of the turn, is
/// `Constraint`, so nothing committed.
pub(super) fn commit_server_turn(
    conn: &mut Connection,
    anchor_id: &str,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<(), StoreError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO server_turns(session_id,turn,anchor_id)
             SELECT t.session_id,t.number,a.anchor_id FROM anchors a, turns t
             WHERE a.anchor_id=?1 AND a.owner_server IS NOT NULL
               AND t.session_id=?2 AND t.number=?3 AND t.state='running'",
            params![anchor_id, session.as_str(), turn.get()],
        )
        .map_err(sql_error)?;
    if inserted != 1 {
        return Err(StoreError::Constraint(
            "a link needs a server anchor, a running turn and no earlier link",
        ));
    }
    before_commit!("store.journal.server_turn");
    commit(tx)
}

/// The links of `turns`, at most [`SERVER_LINKS_LIMIT`] of them.
pub(super) fn read_server_links(
    conn: &Connection,
    turns: &[(SessionId, TurnNumber)],
) -> Result<Vec<ServerLink>, StoreError> {
    if turns.len() > SERVER_LINKS_LIMIT {
        return Err(StoreError::Constraint("too many turns for one link read"));
    }
    let mut query = conn
        .prepare_cached("SELECT anchor_id FROM server_turns WHERE session_id=?1 AND turn=?2")
        .map_err(sql_error)?;
    let mut links = Vec::new();
    for (session, turn) in turns {
        let anchor: Option<String> = query
            .query_row(params![session.as_str(), turn.get()], |row| row.get(0))
            .optional()
            .map_err(sql_error)?;
        if let Some(anchor_id) = anchor {
            links.push(ServerLink {
                session_id: session.clone(),
                turn: *turn,
                anchor_id,
            });
        }
    }
    Ok(links)
}

/// The cohort of every committed anchor: the largest anchor rowid, 0 when
/// there is none. A read-only aggregate on the rowid.
pub(super) fn read_anchor_cohort(conn: &Connection) -> Result<AnchorCohort, StoreError> {
    conn.query_row("SELECT coalesce(max(rowid),0) FROM anchors", [], |row| {
        row.get(0)
    })
    .map(AnchorCohort)
    .map_err(sql_error)
}

#[cfg(test)]
mod tests {
    use super::{
        AnchorCohort, AnchorQuery, Connection, UNPROVEN_ANCHORS_UP_TO, count_unproven_anchors,
        read_anchor_cohort, read_anchor_records,
    };

    /// A current-schema Store with `total` anchors, every third one proved absent.
    fn store_with_anchors(total: u32) -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut conn = Connection::open(dir.path().join("store.sqlite3")).expect("open");
        super::super::configure(&mut conn, true, &super::super::WalLimits::default())
            .expect("schema");
        // Only the anchor rows matter here; their owning turns do not.
        conn.execute_batch("PRAGMA foreign_keys=OFF")
            .expect("pragma");
        for index in 0..total {
            let absence = (index % 3 == 0).then_some("1");
            conn.execute(
                "INSERT INTO anchors(anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,phase,record_version,absence_time)
                 VALUES (?1,'g','m','/s','s_000000000000',1,0,'b','n','intent',1,?2)",
                super::params![format!("a{index:05}"), absence],
            )
            .expect("insert");
        }
        (dir, conn)
    }

    /// Design §11 (T2-D round 2): the unread-anchor count seeks the
    /// `anchors_unproven` partial index and saturates at its limit.
    #[test]
    fn the_unread_anchor_count_seeks_the_partial_index_and_saturates() {
        let (_dir, conn) = store_with_anchors(30);
        let mut plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {UNPROVEN_ANCHORS_UP_TO}"))
            .expect("plan");
        let details: Vec<String> = plan
            .query_map(super::params!["", 4, None::<i64>], |row| row.get(3))
            .expect("plan rows")
            .collect::<Result<_, _>>()
            .expect("plan details");
        assert!(
            details.iter().any(|detail| {
                detail.starts_with("SEARCH anchors USING")
                    && detail.ends_with("INDEX anchors_unproven (anchor_id>?)")
            }),
            "{details:?}"
        );
        // A cohort bound keeps the same seek (round 1 decision 3).
        let details: Vec<String> = plan
            .query_map(super::params!["", 4, 10], |row| row.get(3))
            .expect("plan rows")
            .collect::<Result<_, _>>()
            .expect("plan details");
        assert!(
            details
                .iter()
                .any(|detail| detail.ends_with("INDEX anchors_unproven (anchor_id>?)")),
            "{details:?}"
        );
        // 20 unproven anchors: from the start, after a00020 (7 remain), past the end.
        assert_eq!(
            count_unproven_anchors(&conn, None, 4, None).expect("count"),
            4
        );
        assert_eq!(
            count_unproven_anchors(&conn, None, 100, None).expect("count"),
            20
        );
        let tail = count_unproven_anchors(&conn, Some("a00020"), 100, None).expect("count");
        assert_eq!(tail, 6);
        assert_eq!(
            count_unproven_anchors(&conn, Some("a00029"), 4, None).expect("count"),
            0
        );
    }

    /// Round 1 decision 3: a cohort read before later anchors commit bounds
    /// the unread count and the record page to the anchors before it,
    /// wherever the later ids sort.
    #[test]
    fn a_cohort_excludes_anchors_committed_after_it() {
        let (_dir, conn) = store_with_anchors(30);
        let cohort = read_anchor_cohort(&conn).expect("cohort");
        for id in ["0-later", "a00015x", "b-later"] {
            conn.execute(
                "INSERT INTO anchors(anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,phase,record_version)
                 VALUES (?1,'g','m','/s','s_000000000000',1,0,'b','n','intent',1)",
                super::params![id],
            )
            .expect("insert");
        }
        assert_eq!(
            count_unproven_anchors(&conn, None, 100, None).expect("count"),
            23
        );
        let bounded = count_unproven_anchors(&conn, None, 100, Some(cohort)).expect("count");
        assert_eq!(bounded, 20);
        let page = |cohort| {
            let query = AnchorQuery {
                after: None,
                limit: 256,
                unproven: false,
                owner: None,
                cohort,
            };
            read_anchor_records(&conn, &query)
                .expect("records")
                .into_iter()
                .map(|record| record.intent.anchor_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(page(None).len(), 33);
        let read = page(Some(cohort));
        assert_eq!(read.len(), 30);
        assert!(
            read.iter()
                .all(|id| !id.contains("later") && id != "a00015x"),
            "{read:?}"
        );
        let empty = tempfile::tempdir().expect("temporary directory");
        let mut fresh = Connection::open(empty.path().join("store.sqlite3")).expect("open");
        super::super::configure(&mut fresh, true, &super::super::WalLimits::default())
            .expect("schema");
        assert_eq!(read_anchor_cohort(&fresh).expect("cohort"), AnchorCohort(0));
    }
}
