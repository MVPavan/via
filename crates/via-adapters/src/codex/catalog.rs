//! Last complete public Codex catalog (vendors/codex.md §3; C2 §5).
//! A single recipe-keyed snapshot survives retirement and daemon restart.
//! It never supplies live effort checks or authorizes vendor submission.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags};
use serde::{Deserialize, Serialize};

use super::normalize::DiscoveredModel;

/// Packet §3 discovery's byte budget plus the snapshot's small header.
const SNAPSHOT_BYTES: usize = via_routes::codex::MODEL_BYTES + 4096;
/// VIA-owned state, never a Codex configuration file (runtime §6.1).
const FILE: &str = ".via-catalog.json";
/// Packet §3: one crash leftover, serialized by the writer and data-root locks.
const TEMP: &str = ".via-catalog.tmp";

/// Only normalized public fields and the observed vendor version; the
/// recipe is an opaque digest, never environment values or a program path.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    /// Exact launch-recipe digest (packet §2).
    pub(super) key: String,
    /// Version observed with this complete discovery (C2 §5 OD1).
    pub(super) vendor_version: Option<String>,
    /// Complete normalized public catalog, in vendor order (packet §3).
    pub(super) models: Vec<DiscoveredModel>,
}

/// One complete snapshot, with writes serialized independently of readers.
#[derive(Default)]
pub(super) struct Catalog {
    latest: Mutex<Option<Snapshot>>,
    writer: Mutex<()>,
}

impl Catalog {
    /// Bootstrap-only bounded read; absent, unsafe or corrupt caches are
    /// misses. Live discovery can replace a corrupt private file; unsafe
    /// targets remain misses until removed (runtime §6.1 forbids repair).
    pub(super) fn load(vendor: &Path) -> Self {
        Self {
            // A cache miss affects discovery only; it is never recovery evidence.
            latest: Mutex::new(read(vendor).unwrap_or_default()),
            writer: Mutex::default(),
        }
    }

    /// The last complete discovery, only for the exact launch recipe.
    pub(super) fn for_key(&self, key: &str) -> Option<Snapshot> {
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|snapshot| snapshot.key == key)
            .cloned()
    }

    /// A whole live discovery replaces the snapshot, never a partial page.
    pub(super) fn replace(&self, snapshot: Snapshot) {
        *self.latest.lock().unwrap_or_else(PoisonError::into_inner) = Some(snapshot);
    }

    /// Runs on a driver-owned blocking task. A delayed older task writes
    /// the current snapshot, so it cannot overwrite a newer discovery.
    pub(super) fn persist(&self, vendor: &Path) -> io::Result<()> {
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = self
            .latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(snapshot) = snapshot else {
            return Ok(());
        };
        write(vendor, &snapshot)
    }
}

/// Opens and retains each managed directory without following symlinks.
fn directory(vendor: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let parent = rustix::fs::open(vendor, flags, Mode::empty())?;
    private(&parent, true)?;
    let dir = rustix::fs::openat(&parent, "codex", flags, Mode::empty())?;
    private(&dir, true)?;
    Ok(dir)
}

/// Files and directories follow runtime §6.1's owner/type/mode rules.
fn private(fd: &OwnedFd, directory: bool) -> io::Result<()> {
    let stat = rustix::fs::fstat(fd)?;
    let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
    let expected = if directory {
        rustix::fs::FileType::Directory
    } else {
        rustix::fs::FileType::RegularFile
    };
    let mode = if directory { 0o700 } else { 0o600 };
    if kind != expected
        || stat.st_uid != crate::private_dir::daemon_uid()
        || stat.st_mode & 0o777 != mode
    {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(())
}

fn read(vendor: &Path) -> io::Result<Option<Snapshot>> {
    let dir = match directory(vendor) {
        Ok(dir) => dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let fd = match rustix::fs::openat(
        &dir,
        FILE,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    private(&fd, false)?;
    let mut bytes = Vec::new();
    File::from(fd)
        .take((SNAPSHOT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > SNAPSHOT_BYTES {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
    if snapshot.key.len() != 64 || !snapshot.key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(Some(snapshot))
}

/// Runtime §6.1: an absent or private regular target may be replaced;
/// unsafe targets are refused without opening, following or repairing them.
fn private_target(dir: &OwnedFd, name: &str) -> io::Result<bool> {
    match rustix::fs::statat(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat)
            if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                != rustix::fs::FileType::RegularFile
                || stat.st_uid != crate::private_dir::daemon_uid()
                || stat.st_mode & 0o777 != 0o600 =>
        {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
        Ok(_) => Ok(true),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Synced temporary file then atomic rename; failures before rename leave
/// the old complete file intact. A failed directory sync after rename
/// leaves a complete visible replacement whose durability is uncertain.
fn write(vendor: &Path, snapshot: &Snapshot) -> io::Result<()> {
    let bytes = serde_json::to_vec(snapshot)?;
    if bytes.len() > SNAPSHOT_BYTES {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let dir = directory(vendor)?;
    private_target(&dir, FILE)?;
    // The writer mutex and sole-daemon data-root lock exclude other
    // writers. Reclaim only a private fixed-name crash leftover (packet §3).
    if private_target(&dir, TEMP)? {
        rustix::fs::unlinkat(&dir, TEMP, rustix::fs::AtFlags::empty())?;
    }
    let fd = rustix::fs::openat(
        &dir,
        TEMP,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let result = (|| {
        let mut file = File::from(fd);
        file.write_all(&bytes)?;
        file.sync_all()?;
        rustix::fs::renameat(&dir, TEMP, &dir, FILE)?;
        rustix::fs::fsync(&dir)?;
        Ok(())
    })();
    // After rename the temporary name is absent; on failure cleanup is
    // best effort and never unlinks the previous complete snapshot.
    let _ = rustix::fs::unlinkat(&dir, TEMP, rustix::fs::AtFlags::empty());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(dir.path().join("codex"))
            .unwrap();
        dir
    }

    fn snapshot(model: &str) -> Snapshot {
        Snapshot {
            key: "a".repeat(64),
            vendor_version: Some("0.159.2".to_owned()),
            models: vec![DiscoveredModel {
                model: model.to_owned(),
                efforts: vec!["low".to_owned()],
                hidden: false,
                default: true,
            }],
        }
    }

    /// Runtime §6.1: private, bounded snapshots round-trip; a failed write
    /// cannot replace the preceding complete one.
    #[test]
    fn a_failed_write_preserves_the_complete_private_snapshot() {
        let root = root();
        write(root.path(), &snapshot("luna")).unwrap();
        let file = root.path().join("codex").join(FILE);
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write(root.path(), &snapshot(&"x".repeat(SNAPSHOT_BYTES))).is_err());
        let kept = read(root.path()).unwrap().unwrap();
        assert_eq!(kept.models[0].model, "luna");
        assert_eq!(kept.vendor_version.as_deref(), Some("0.159.2"));
        assert_eq!(
            std::fs::read_dir(file.parent().unwrap()).unwrap().count(),
            1
        );
    }

    /// Runtime §6.1: cache loading never follows a link, repairs permissions
    /// or accepts truncated/oversized bytes as a complete catalog.
    #[test]
    fn unsafe_and_incomplete_cache_files_are_misses() {
        let root = root();
        let file = root.path().join("codex").join(FILE);
        let target = root.path().join("outside");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        assert!(
            Catalog::load(root.path())
                .for_key(&"a".repeat(64))
                .is_none()
        );
        assert!(write(root.path(), &snapshot("luna")).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
        std::fs::remove_file(&file).unwrap();
        write(root.path(), &snapshot("luna")).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(root.path()).is_err());
        assert!(write(root.path(), &snapshot("sol")).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        for bytes in [b"{\"models\":".to_vec(), vec![b' '; SNAPSHOT_BYTES + 1]] {
            std::fs::write(&file, bytes).unwrap();
            assert!(
                Catalog::load(root.path())
                    .for_key(&"a".repeat(64))
                    .is_none()
            );
        }
        write(root.path(), &snapshot("recovered")).unwrap();
        assert_eq!(
            read(root.path()).unwrap().unwrap().models[0].model,
            "recovered"
        );
    }

    /// Packet §3, runtime §6.1: one private crash leftover is reclaimed;
    /// an unsafe temporary target is refused without following or repair.
    #[test]
    fn a_stale_private_temporary_file_does_not_accumulate() {
        let root = root();
        write(root.path(), &snapshot("old")).unwrap();
        let temp = root.path().join("codex/.via-catalog.tmp");
        std::fs::write(&temp, "interrupted write").unwrap();
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)).unwrap();
        write(root.path(), &snapshot("new")).unwrap();
        assert_eq!(read(root.path()).unwrap().unwrap().models[0].model, "new");
        assert_eq!(
            std::fs::read_dir(temp.parent().unwrap()).unwrap().count(),
            1
        );

        let outside = root.path().join("outside");
        std::fs::write(&outside, "untouched").unwrap();
        std::os::unix::fs::symlink(&outside, &temp).unwrap();
        assert!(write(root.path(), &snapshot("unsafe")).is_err());
        assert!(std::fs::symlink_metadata(&temp).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
        std::fs::remove_file(&temp).unwrap();
        std::fs::write(&temp, "wrong mode").unwrap();
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(write(root.path(), &snapshot("unsafe")).is_err());
        assert_eq!(std::fs::read_to_string(&temp).unwrap(), "wrong mode");
        assert_eq!(read(root.path()).unwrap().unwrap().models[0].model, "new");
    }

    /// Delayed persistence reads the latest complete discovery; it cannot
    /// publish the older snapshot captured by a prior driver.
    #[test]
    fn delayed_writers_persist_the_latest_complete_discovery() {
        let root = root();
        let catalog = Catalog::load(root.path());
        catalog.replace(snapshot("old"));
        catalog.replace(snapshot("new"));
        catalog.persist(root.path()).unwrap();
        assert_eq!(read(root.path()).unwrap().unwrap().models[0].model, "new");
        assert!(catalog.for_key(&"b".repeat(64)).is_none());
    }
}
