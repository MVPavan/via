//! Synthetic anchors for recovery scenarios, written straight to a stopped
//! daemon's Store. Each copies the identity of an existing real anchor and
//! carries a committed absence proof, so Host accepts it without probing;
//! its group (`pgid` beyond Linux's `pid_max`) cannot exist.

use std::path::Path;
use std::time::Duration;

/// Commits `count` synthetic anchors owned by turn 1 of `owner` into the
/// Store at `store`. Ids start with `prefix`.
pub(crate) fn insert_proven_absent(
    store: &Path,
    owner: &str,
    prefix: &str,
    count: u32,
) -> Result<(), String> {
    let text = |error: rusqlite::Error| error.to_string();
    let mut store = rusqlite::Connection::open(store).map_err(text)?;
    let tx = store.transaction().map_err(text)?;
    for index in 0..count {
        let changed = tx
            .execute(
                "INSERT INTO anchors(anchor_id,generation,marker,socket_path,owner_session,owner_turn,uid,boot_id,pid_namespace,phase,record_version,pid,pgid,start_ticks,absence_time)
                 SELECT ?1,'g'||?1,a.marker,'/nonexistent',?2,1,a.uid,a.boot_id,a.pid_namespace,'arm_intent',1,?3,?3,1,'1'
                 FROM anchors a WHERE a.pid IS NOT NULL LIMIT 1",
                rusqlite::params![format!("{prefix}{index:05}"), owner, 4_194_305 + index],
            )
            .map_err(text)?;
        if changed != 1 {
            return Err("no real anchor to copy".to_owned());
        }
    }
    tx.commit().map_err(text)
}

/// Removes the synthetic anchors whose ids start with `prefix` while no
/// daemon runs, so teardown and the next recovery see only anchors that ran.
pub(crate) fn delete_synthetic(store: &Path, prefix: &str) -> Result<(), String> {
    let text = |error: rusqlite::Error| error.to_string();
    let store = rusqlite::Connection::open(store).map_err(text)?;
    store.busy_timeout(Duration::from_secs(5)).map_err(text)?;
    store
        .execute(
            "DELETE FROM anchors WHERE anchor_id LIKE ?1",
            [format!("{prefix}%")],
        )
        .map(drop)
        .map_err(text)
}
