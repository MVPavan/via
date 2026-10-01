//! Vendor binary resolution and identity, and the in-memory instance cache
//! (adapter design §5.4, C2 §5 AD7). Pure `stat` work: nothing here starts
//! a process or writes a file. The cache lives in memory only, so a daemon
//! restart clears it by construction.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a refusal entry stays live after it was written (C2 §5).
pub const REFUSAL_TTL: Duration = Duration::from_mins(10);

/// The binary a harness runs: `configured` (`harnesses.<name>.binary`) when
/// set, else the first executable regular file named `default_binary` in
/// the captured `path`. Empty and relative `PATH` entries are skipped.
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
            fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
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
/// recipe key (the adapter's canonical recipe string).
#[derive(Debug, Default)]
pub struct InstanceCache {
    inner: Mutex<Entries>,
}

#[derive(Debug, Default)]
struct Entries {
    versions: HashMap<BinaryIdentity, String>,
    refusals: HashMap<BinaryIdentity, HashMap<String, (Instant, Incompatibility)>>,
}

impl InstanceCache {
    fn entries(&self) -> std::sync::MutexGuard<'_, Entries> {
        // The maps stay consistent after any panic: each write is one insert.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the version an instance of `identity` reported at its handshake.
    pub fn record_version(&self, identity: BinaryIdentity, version: String) {
        self.entries().versions.insert(identity, version);
    }

    /// The last version seen for `identity`.
    pub fn last_version(&self, identity: &BinaryIdentity) -> Option<String> {
        self.entries().versions.get(identity).cloned()
    }

    /// Records a refusal written at `now`; it expires [`REFUSAL_TTL`] later.
    pub fn record_refusal(
        &self,
        identity: BinaryIdentity,
        recipe: String,
        cause: Incompatibility,
        now: Instant,
    ) {
        self.entries()
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
        if now.saturating_duration_since(written) < REFUSAL_TTL {
            return Some(cause);
        }
        recipes.remove(recipe);
        if recipes.is_empty() {
            entries.refusals.remove(identity);
        }
        None
    }
}
