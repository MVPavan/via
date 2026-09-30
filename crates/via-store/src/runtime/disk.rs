//! Task 4 design §5.3 and §5.4: the WAL limit the SQLite thread keeps, the
//! free space the disk floor reads and the apparent size of VIA's data.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rusqlite::Connection;

use super::{Command, StoreError};

/// One SQLite page: `wal.checkpoint_bytes` applies as whole pages (§5.5).
pub const PAGE_BYTES: u64 = 4096;

/// The WAL thresholds of design §5.4, from daemon config (§5.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalLimits {
    /// At or above this WAL length, new work is refused (`wal_full`).
    pub max: u64,
    /// A passive checkpoint after this much WAL growth
    /// (`wal_autocheckpoint`, in whole pages); also `journal_size_limit`.
    pub checkpoint_bytes: u64,
    /// A passive checkpoint after this many commits.
    pub checkpoint_commits: u32,
}

impl Default for WalLimits {
    /// Design §5.5's defaults: 32 MiB, 8 MiB and 1000 commits.
    fn default() -> Self {
        Self {
            max: 32 * 1024 * 1024,
            checkpoint_bytes: 8 * 1024 * 1024,
            checkpoint_commits: 1000,
        }
    }
}

/// While `wal_full` is set, a write at least this long after the last
/// attempt retries `TRUNCATE` first (§5.4).
const TRUNCATE_RETRY: Duration = Duration::from_secs(1);

/// The SQLite thread's WAL policy (§5.4). `full` is shared with every
/// client, which reads it for a queued turn's dispatch; the rest lives on
/// the SQLite thread.
pub(super) struct Wal {
    limits: WalLimits,
    /// `<state>/store.sqlite3-wal`.
    path: PathBuf,
    full: Arc<AtomicBool>,
    commits: u32,
    /// The last `TRUNCATE` attempt.
    attempted: Option<Instant>,
}

impl Wal {
    pub(super) fn new(limits: WalLimits, db: &Path, full: Arc<AtomicBool>) -> Self {
        let mut path = db.as_os_str().to_owned();
        path.push("-wal");
        Self {
            limits,
            path: PathBuf::from(path),
            full,
            commits: 0,
            attempted: None,
        }
    }

    /// At open, before the first mutation: a WAL an earlier writer left at
    /// or above `wal.max` (a reader held it) gets one `TRUNCATE` attempt,
    /// and the Store starts with new work refused while it stays at the
    /// limit, as after a commit (review r1).
    pub(super) fn opened(&mut self, conn: &Connection) {
        if self.length().is_some_and(|len| len >= self.limits.max) {
            self.full.store(true, Ordering::Release);
            self.truncate(conn);
        }
    }

    /// Before a mutation: while `wal_full` is set, a write at least 1 s
    /// after the last attempt retries `TRUNCATE`, which clears the flag
    /// below `wal.max`. Then a `spawn` or `resume` receipt still meeting
    /// the flag is refused before `BEGIN` ([`StoreError::WalFull`], known
    /// not committed); every other write is returned to be served.
    pub(super) fn admit(&mut self, conn: &Connection, command: Command) -> Option<Command> {
        if self.full.load(Ordering::Acquire)
            && self
                .attempted
                .is_none_or(|at| at.elapsed() >= TRUNCATE_RETRY)
        {
            self.truncate(conn);
        }
        if !self.full.load(Ordering::Acquire) {
            return Some(command);
        }
        if let Command::Spawn(_, _, reply) = command {
            let _ = reply.send(Err(StoreError::WalFull));
            return None;
        }
        if let Command::Resume(_, reply) = command {
            let _ = reply.send(Err(StoreError::WalFull));
            return None;
        }
        Some(command)
    }

    /// After a mutation: a passive checkpoint every `checkpoint_commits`
    /// commits, then the WAL file's length; at or above `wal.max` the flag
    /// is set and `TRUNCATE` runs (§5.4).
    pub(super) fn committed(&mut self, conn: &Connection) {
        self.commits += 1;
        if self.commits >= self.limits.checkpoint_commits {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
            self.commits = 0;
        }
        if !self.full.load(Ordering::Acquire)
            && self.length().is_some_and(|len| len >= self.limits.max)
        {
            self.full.store(true, Ordering::Release);
            self.truncate(conn);
        }
    }

    /// One `TRUNCATE` attempt; below `wal.max` afterwards, `wal_full`
    /// clears. A reader holding a snapshot makes it fail, which leaves the
    /// flag set: an oversized WAL is a known state, not a Store failure.
    fn truncate(&mut self, conn: &Connection) {
        self.attempted = Some(Instant::now());
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        if self.length().is_some_and(|len| len < self.limits.max) {
            self.full.store(false, Ordering::Release);
        }
    }

    /// The WAL file's length; no file is an empty WAL. Another error leaves
    /// the length unknown and the flag as it is.
    fn length(&self) -> Option<u64> {
        match fs::metadata(&self.path) {
            Ok(metadata) => Some(metadata.len()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Some(0),
            Err(_) => None,
        }
    }
}

/// The free space of `state`'s filesystem for an unprivileged writer,
/// `f_bavail × f_frsize` (§5.3). Test builds: the point
/// `store.statvfs.free_bytes` may report a value instead (§13.1).
pub(super) fn free_bytes(state: &Path) -> io::Result<u64> {
    #[cfg(feature = "test-failpoints")]
    if let Some(value) = crate::failpoint::value("store.statvfs.free_bytes")? {
        return Ok(value);
    }
    let stat = rustix::fs::statvfs(state)?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// The files of VIA's data directly in the State directory (§5.3): the
/// Store, its sidecars and the daemon log with its rotated copy.
const DATA_FILES: [&str; 5] = [
    "store.sqlite3",
    "store.sqlite3-wal",
    "store.sqlite3-shm",
    "via.log",
    "via.log.1",
];

/// The apparent length (`metadata().len()`) of VIA's data under `state`
/// (§5.3 [t4r17.7]): [`DATA_FILES`] and every file under `blobs/` and
/// `evidence/`, in one walk that follows no symbolic link. A file or
/// directory gone meanwhile counts nothing; any other error fails the walk.
pub(super) fn data_bytes(state: &Path) -> io::Result<u64> {
    let mut total = 0_u64;
    for name in DATA_FILES {
        total = total.saturating_add(present(fs::symlink_metadata(state.join(name)))?);
    }
    for dir in ["blobs", "evidence"] {
        total = total.saturating_add(walk(&state.join(dir))?);
    }
    Ok(total)
}

/// The length of a regular file; nothing for another kind or one gone.
fn present(metadata: io::Result<fs::Metadata>) -> io::Result<u64> {
    match metadata {
        Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
        Ok(_) => Ok(0),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

/// The summed lengths of the regular files under `dir`, recursively.
fn walk(dir: &Path) -> io::Result<u64> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut total = 0_u64;
    for entry in entries {
        let path = entry?.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let bytes = if metadata.is_dir() {
            walk(&path)?
        } else {
            present(Ok(metadata))?
        };
        total = total.saturating_add(bytes);
    }
    Ok(total)
}
