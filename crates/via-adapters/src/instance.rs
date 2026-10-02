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

/// The most binary identities whose last version the cache keeps.
pub const VERSIONS_KEPT: usize = 16;

/// The most refusal entries the cache keeps.
pub const REFUSALS_KEPT: usize = 64;

/// The longest recipe key, in bytes, whose refusal the cache remembers.
pub const RECIPE_KEY_MAX: usize = 1024;

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

    /// The identity as fixed-width little-endian bytes, for a launch key.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the Codex server key uses it (x.3.2 X2)")
    )]
    pub(crate) fn to_bytes(self) -> [u8; 40] {
        let mut bytes = [0; 40];
        let fields = [
            self.dev.to_le_bytes(),
            self.ino.to_le_bytes(),
            self.size.to_le_bytes(),
            self.mtime.to_le_bytes(),
            self.mtime_nsec.to_le_bytes(),
        ];
        for (chunk, field) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(fields) {
            chunk.copy_from_slice(&field);
        }
        bytes
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
/// bounded: at most [`VERSIONS_KEPT`] version entries and
/// [`REFUSALS_KEPT`] refusal entries, the least recently written evicted
/// first (a lost entry costs one cache miss); every refusal write sweeps
/// the expired refusals. What it holds is not inspectable from outside:
///
/// ```compile_fail
/// let cache = via_adapters::InstanceCache::default();
/// let _ = cache.retained();
/// ```
#[derive(Debug, Default)]
pub struct InstanceCache {
    inner: Mutex<Entries>,
}

#[derive(Debug, Default)]
struct Entries {
    /// Per identity: the last version seen and its write's sequence number.
    versions: HashMap<BinaryIdentity, (String, u64)>,
    /// The sequence number of the next version write.
    written: u64,
    /// At most [`REFUSALS_KEPT`], in write order, the oldest first.
    refusals: Vec<Refusal>,
}

#[derive(Debug)]
struct Refusal {
    identity: BinaryIdentity,
    recipe: String,
    written: Instant,
    cause: Incompatibility,
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

    /// Records the version an instance of the binary with `identity`
    /// reported at its handshake. Past [`VERSIONS_KEPT`] identities, the
    /// least recently written entry is evicted.
    pub fn record_version(&self, identity: BinaryIdentity, version: String) {
        let mut entries = self.entries();
        let sequence = entries.written;
        entries.written += 1;
        entries.versions.insert(identity, (version, sequence));
        if entries.versions.len() > VERSIONS_KEPT {
            let oldest = entries
                .versions
                .iter()
                .min_by_key(|(_, (_, written))| *written)
                .map(|(identity, _)| *identity);
            if let Some(oldest) = oldest {
                entries.versions.remove(&oldest);
            }
        }
    }

    /// The last version seen for the binary `identity` (C2 §5).
    pub fn last_version(&self, identity: &BinaryIdentity) -> Option<String> {
        self.entries()
            .versions
            .get(identity)
            .map(|(version, _)| version.clone())
    }

    /// Records a refusal written at `now`; it expires [`REFUSAL_TTL`] later.
    /// Every refusal expired at `now` is dropped first, and past
    /// [`REFUSALS_KEPT`] entries the least recently written is evicted. A
    /// `recipe` longer than [`RECIPE_KEY_MAX`] bytes is not remembered: the
    /// refusal still applies to its request.
    pub fn record_refusal(
        &self,
        identity: BinaryIdentity,
        recipe: String,
        cause: Incompatibility,
        now: Instant,
    ) {
        if recipe.len() > RECIPE_KEY_MAX {
            return;
        }
        let mut entries = self.entries();
        entries.refusals.retain(|refusal| {
            live(refusal.written, now)
                && !(refusal.identity == identity && refusal.recipe == recipe)
        });
        if entries.refusals.len() == REFUSALS_KEPT {
            entries.refusals.remove(0);
        }
        entries.refusals.push(Refusal {
            identity,
            recipe,
            written: now,
            cause,
        });
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
        let index = entries
            .refusals
            .iter()
            .position(|refusal| refusal.identity == *identity && refusal.recipe == recipe)?;
        let refusal = &entries.refusals[index];
        if live(refusal.written, now) {
            return Some(refusal.cause);
        }
        entries.refusals.remove(index);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(ino: u64) -> BinaryIdentity {
        BinaryIdentity {
            dev: 1,
            ino,
            size: 0,
            mtime: 0,
            mtime_nsec: 0,
        }
    }

    fn refusals(cache: &InstanceCache) -> usize {
        cache.entries().refusals.len()
    }

    /// Critical r1 #3: refusal retention has a hard bound. Within one TTL,
    /// many distinct recipes keep 64 entries, the oldest written evicted
    /// first; a recipe key over 1 KiB is not remembered.
    #[test]
    fn refusals_are_bounded_within_one_ttl() {
        let cache = InstanceCache::default();
        let now = Instant::now();
        let cause = Incompatibility::FeatureAbsent("tool_list");
        for recipe in 0..1000 {
            cache.record_refusal(identity(1), format!("recipe-{recipe}"), cause, now);
        }
        assert_eq!(refusals(&cache), 64);
        assert_eq!(cache.refusal(&identity(1), "recipe-935", now), None);
        for recipe in 936..1000 {
            let key = format!("recipe-{recipe}");
            assert_eq!(cache.refusal(&identity(1), &key, now), Some(cause), "{key}");
        }
        // Across identities too, and a rewrite counts as the newest write.
        cache.record_refusal(identity(1), "recipe-936".to_owned(), cause, now);
        cache.record_refusal(identity(2), "recipe-0".to_owned(), cause, now);
        assert_eq!(refusals(&cache), 64);
        assert_eq!(cache.refusal(&identity(1), "recipe-936", now), Some(cause));
        assert_eq!(cache.refusal(&identity(1), "recipe-937", now), None);
        assert_eq!(cache.refusal(&identity(2), "recipe-0", now), Some(cause));

        let oversized = "k".repeat(1025);
        cache.record_refusal(identity(3), oversized.clone(), cause, now);
        assert_eq!(cache.refusal(&identity(3), &oversized, now), None);
        assert_eq!(refusals(&cache), 64);
        let largest = "k".repeat(1024);
        cache.record_refusal(identity(3), largest.clone(), cause, now);
        assert_eq!(cache.refusal(&identity(3), &largest, now), Some(cause));
    }

    /// Each refusal write sweeps the expired refusals.
    #[test]
    fn refusal_writes_sweep_expired_ones() {
        let cache = InstanceCache::default();
        let written = Instant::now();
        let cause = Incompatibility::FeatureAbsent("tool_list");
        for recipe in 0..10 {
            cache.record_refusal(identity(1), format!("recipe-{recipe}"), cause, written);
        }
        assert_eq!(refusals(&cache), 10);
        let later = written + REFUSAL_TTL;
        cache.record_refusal(identity(1), "fresh".to_owned(), cause, later);
        assert_eq!(refusals(&cache), 1, "expired refusals survived a write");
    }

    /// At most [`VERSIONS_KEPT`] version entries are held.
    #[test]
    fn versions_are_bounded() {
        let cache = InstanceCache::default();
        for ino in 0..100 {
            cache.record_version(identity(ino), format!("0.{ino}"));
        }
        assert_eq!(cache.entries().versions.len(), VERSIONS_KEPT);
    }
}
