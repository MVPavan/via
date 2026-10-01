//! Vendor binary resolution and identity, and the in-memory instance cache
//! (adapter design §5.4, C2 §5 AD7). Only `stat` and access checks: nothing
//! here starts a process or writes a file. The cache lives in memory only, so a daemon
//! restart clears it by construction.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustix::fs::{Access, AtFlags, CWD, accessat};

/// How long a refusal entry stays live after it was written (C2 §5).
pub const REFUSAL_TTL: Duration = Duration::from_mins(10);

/// The binary a harness runs: `configured` (`harnesses.<name>.binary`) when
/// set, else the first regular file named `default_binary` in the captured
/// `path` that this process may execute, as the kernel judges it for the
/// effective ids (`faccessat(X_OK, AT_EACCESS)`: mode classes, ACLs and
/// `noexec` mounts included). Empty and relative `PATH` entries are skipped.
pub fn resolve_binary(
    configured: Option<&Path>,
    default_binary: &str,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(configured) = configured {
        return Some(configured.to_path_buf());
    }
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(default_binary))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|meta| meta.is_file())
                && accessat(CWD, candidate, Access::EXEC_OK, AtFlags::EACCESS).is_ok()
        })
}

/// A resolved binary's identity (C2 §5): device, inode, size and
/// modification time of the target, symlinks followed. Any change to the
/// file on disk gives a new identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BinaryIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl BinaryIdentity {
    /// The identity of the file `path` resolves to.
    pub fn of(path: &Path) -> io::Result<Self> {
        let meta = fs::metadata(path)?;
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        })
    }
}

/// A demonstrated incompatibility, the only refusal the cache can hold
/// (C2 §5). Spawn failures, timeouts, transport loss, auth, quota and
/// rate-limit failures have no variant, so they cannot be cached:
///
/// ```compile_fail
/// use std::time::Instant;
/// use via_adapters::{BinaryIdentity, Incompatibility, InstanceCache};
/// fn record(cache: &InstanceCache, identity: BinaryIdentity) {
///     let cause = Incompatibility::Timeout;
///     cache.record_refusal(identity, "recipe".to_owned(), cause, Instant::now());
/// }
/// ```
///
/// The same call with a demonstrated incompatibility compiles:
///
/// ```
/// use std::time::Instant;
/// use via_adapters::{BinaryIdentity, Incompatibility, InstanceCache};
/// fn record(cache: &InstanceCache, identity: BinaryIdentity) {
///     let cause = Incompatibility::FeatureAbsent("interrupt_receipt_v1");
///     cache.record_refusal(identity, "recipe".to_owned(), cause, Instant::now());
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Incompatibility {
    /// A relied-on feature is absent from the handshake.
    FeatureAbsent(&'static str),
    /// A readback differs from the value VIA sent.
    ReadbackDiffers(&'static str),
}

/// The instance cache (C2 §5 AD7): the last version seen per binary
/// identity, and refusal entries keyed by identity plus the route's
/// recipe key (the adapter's canonical recipe string). Retention is
/// bounded: one version entry per resolved binary path, and every refusal
/// write sweeps the expired refusals.
#[derive(Debug, Default)]
pub struct InstanceCache {
    inner: Mutex<Entries>,
}

#[derive(Debug, Default)]
struct Entries {
    /// Per resolved path: the identity it had and the version it reported.
    versions: HashMap<PathBuf, (BinaryIdentity, String)>,
    refusals: HashMap<BinaryIdentity, HashMap<String, (Instant, Incompatibility)>>,
}

/// Whether an entry written at `written` is live at `now`.
fn live(written: Instant, now: Instant) -> bool {
    now.saturating_duration_since(written) < REFUSAL_TTL
}

impl InstanceCache {
    fn entries(&self) -> std::sync::MutexGuard<'_, Entries> {
        // The maps stay consistent after any panic: each write is one insert.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the version an instance of the binary at `path`, with
    /// `identity`, reported at its handshake; it replaces the path's entry.
    pub fn record_version(&self, path: &Path, identity: BinaryIdentity, version: String) {
        self.entries()
            .versions
            .insert(path.to_path_buf(), (identity, version));
    }

    /// The last version seen for the binary at `path`, while it still has
    /// `identity`.
    pub fn last_version(&self, path: &Path, identity: &BinaryIdentity) -> Option<String> {
        self.entries()
            .versions
            .get(path)
            .filter(|(seen, _)| seen == identity)
            .map(|(_, version)| version.clone())
    }

    /// Records a refusal written at `now`; it expires [`REFUSAL_TTL`] later.
    /// Every refusal expired at `now` is dropped first.
    pub fn record_refusal(
        &self,
        identity: BinaryIdentity,
        recipe: String,
        cause: Incompatibility,
        now: Instant,
    ) {
        let mut entries = self.entries();
        entries.refusals.retain(|_, recipes| {
            recipes.retain(|_, (written, _)| live(*written, now));
            !recipes.is_empty()
        });
        entries
            .refusals
            .entry(identity)
            .or_default()
            .insert(recipe, (now, cause));
    }

    /// The live refusal for `identity` and `recipe` at `now`, if any; an
    /// expired entry is dropped.
    pub fn refusal(
        &self,
        identity: &BinaryIdentity,
        recipe: &str,
        now: Instant,
    ) -> Option<Incompatibility> {
        let mut entries = self.entries();
        let recipes = entries.refusals.get_mut(identity)?;
        let (written, cause) = *recipes.get(recipe)?;
        if live(written, now) {
            return Some(cause);
        }
        recipes.remove(recipe);
        if recipes.is_empty() {
            entries.refusals.remove(identity);
        }
        None
    }

    /// The entries held, versions and refusals together.
    pub fn retained(&self) -> usize {
        let entries = self.entries();
        entries.versions.len() + entries.refusals.values().map(HashMap::len).sum::<usize>()
    }
}
