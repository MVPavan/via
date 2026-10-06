//! Vendor binary resolution and the in-memory instance cache (adapter
//! design §5.4, C2 §5 AD7). Only `stat` and access checks: nothing here
//! starts a process or writes a file. The cache lives in memory only, so a
//! daemon restart clears it by construction.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustix::fs::{Access, AtFlags, CWD, accessat};

/// How long a refusal entry stays live after it was written (C2 §5).
pub const REFUSAL_TTL: Duration = Duration::from_mins(10);

/// The most harness and program path pairs whose last version the cache keeps.
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

/// A demonstrated incompatibility, the only refusal the cache can hold
/// (C2 §5). Spawn failures, timeouts, transport loss, auth, quota and
/// rate-limit failures have no variant, so they cannot be cached:
///
/// ```compile_fail
/// use std::time::Instant;
/// use std::path::Path;
/// use via_adapters::{Incompatibility, InstanceCache};
/// fn record(cache: &InstanceCache, program: &Path) {
///     let cause = Incompatibility::Timeout;
///     cache.record_refusal(program, "recipe".to_owned(), cause, Instant::now());
/// }
/// ```
///
/// The same call with a demonstrated incompatibility compiles:
///
/// ```
/// use std::time::Instant;
/// use std::path::Path;
/// use via_adapters::{Incompatibility, InstanceCache};
/// fn record(cache: &InstanceCache, program: &Path) {
///     let cause = Incompatibility::FeatureAbsent("interrupt_receipt_v1");
///     cache.record_refusal(program, "recipe".to_owned(), cause, Instant::now());
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Incompatibility {
    /// A relied-on feature is absent from the handshake.
    FeatureAbsent(&'static str),
    /// A readback differs from the value VIA sent.
    ReadbackDiffers(&'static str),
}

/// The file a program path names, as `stat` reports it (C2 §5): a binary
/// replaced at the same path, in place, by a rename or by a retargeted
/// symlink, has another identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileIdentity {
    /// The identity of the file `program` resolves to, following
    /// symlinks; `None` when it cannot be read.
    fn of(program: &Path) -> Option<Self> {
        let meta = fs::metadata(program).ok()?;
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}

/// The instance cache (C2 §5 AD7): the last version seen per harness and
/// resolved program path, and refusal entries keyed by the program path,
/// the file identity it had when the refusal was written (so a binary
/// replaced at the path does not inherit it), and the route's recipe key
/// (the adapter's canonical recipe string). The identity is read when the
/// refusal is recorded, after the handshake: a binary replaced between the
/// launch and that read is the accepted race of every route.
/// Retention is bounded: at most [`VERSIONS_KEPT`] version entries and
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
    /// Per harness and program path: the last version seen and its
    /// write's sequence number.
    versions: HashMap<(String, PathBuf), (String, u64)>,
    /// The sequence number of the next version write.
    written: u64,
    /// At most [`REFUSALS_KEPT`], in write order, the oldest first.
    refusals: Vec<Refusal>,
}

#[derive(Debug)]
struct Refusal {
    program: PathBuf,
    identity: FileIdentity,
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

    /// Records the version an instance of `harness` run from `program`
    /// reported at its handshake. Past [`VERSIONS_KEPT`] pairs, the least
    /// recently written entry is evicted.
    pub fn record_version(&self, harness: &str, program: &Path, version: String) {
        let mut entries = self.entries();
        let sequence = entries.written;
        entries.written += 1;
        let key = (harness.to_owned(), program.to_path_buf());
        entries.versions.insert(key, (version, sequence));
        if entries.versions.len() > VERSIONS_KEPT {
            let oldest = entries
                .versions
                .iter()
                .min_by_key(|(_, (_, written))| *written)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.versions.remove(&oldest);
            }
        }
    }

    /// The last version seen for `harness` run from `program` (C2 §5).
    pub fn last_version(&self, harness: &str, program: &Path) -> Option<String> {
        self.entries()
            .versions
            .iter()
            .find(|((name, path), _)| name == harness && path == program)
            .map(|(_, (version, _))| version.clone())
    }

    /// Records a refusal written at `now` for the file `program` names now;
    /// it expires [`REFUSAL_TTL`] later. Every refusal expired at `now` is
    /// dropped first, and past [`REFUSALS_KEPT`] entries the least recently
    /// written is evicted. A `recipe` longer than [`RECIPE_KEY_MAX`] bytes,
    /// or a program whose file cannot be read, is not remembered: the
    /// refusal still applies to its request.
    pub fn record_refusal(
        &self,
        program: &Path,
        recipe: String,
        cause: Incompatibility,
        now: Instant,
    ) {
        if recipe.len() > RECIPE_KEY_MAX {
            return;
        }
        let Some(identity) = FileIdentity::of(program) else {
            return;
        };
        let mut entries = self.entries();
        entries.refusals.retain(|refusal| {
            live(refusal.written, now) && !(refusal.program == program && refusal.recipe == recipe)
        });
        if entries.refusals.len() == REFUSALS_KEPT {
            entries.refusals.remove(0);
        }
        entries.refusals.push(Refusal {
            program: program.to_path_buf(),
            identity,
            recipe,
            written: now,
            cause,
        });
    }

    /// The live refusal for `program` and `recipe` at `now`, if any, while
    /// `program` still names the file it was recorded for; an expired
    /// entry is dropped, and so is one whose file was replaced.
    pub fn refusal(&self, program: &Path, recipe: &str, now: Instant) -> Option<Incompatibility> {
        let identity = FileIdentity::of(program);
        let mut entries = self.entries();
        let index = entries
            .refusals
            .iter()
            .position(|refusal| refusal.program == program && refusal.recipe == recipe)?;
        let refusal = &entries.refusals[index];
        if live(refusal.written, now) && identity == Some(refusal.identity) {
            return Some(refusal.cause);
        }
        entries.refusals.remove(index);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Programs `vendor-<index>` in one directory, created on first use: a
    /// refusal is recorded only for a file that exists (C2 §5).
    struct Programs(tempfile::TempDir);

    impl Programs {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        fn get(&self, index: u64) -> PathBuf {
            let path = self.0.path().join(format!("vendor-{index}"));
            if !path.exists() {
                fs::write(&path, "vendor").unwrap();
            }
            path
        }
    }

    fn program(index: u64) -> PathBuf {
        PathBuf::from(format!("/bin/vendor-{index}"))
    }

    fn refusals(cache: &InstanceCache) -> usize {
        cache.entries().refusals.len()
    }

    /// Critical r1 #3: refusal retention has a hard bound. Within one TTL,
    /// many distinct recipes keep 64 entries, the oldest written evicted
    /// first; a recipe key over 1 KiB is not remembered.
    #[test]
    fn refusals_are_bounded_within_one_ttl() {
        let programs = Programs::new();
        let program = |index| programs.get(index);
        let cache = InstanceCache::default();
        let now = Instant::now();
        let cause = Incompatibility::FeatureAbsent("tool_list");
        for recipe in 0..1000 {
            cache.record_refusal(&program(1), format!("recipe-{recipe}"), cause, now);
        }
        assert_eq!(refusals(&cache), 64);
        assert_eq!(cache.refusal(&program(1), "recipe-935", now), None);
        for recipe in 936..1000 {
            let key = format!("recipe-{recipe}");
            assert_eq!(cache.refusal(&program(1), &key, now), Some(cause), "{key}");
        }
        // Across programs too, and a rewrite counts as the newest write.
        cache.record_refusal(&program(1), "recipe-936".to_owned(), cause, now);
        cache.record_refusal(&program(2), "recipe-0".to_owned(), cause, now);
        assert_eq!(refusals(&cache), 64);
        assert_eq!(cache.refusal(&program(1), "recipe-936", now), Some(cause));
        assert_eq!(cache.refusal(&program(1), "recipe-937", now), None);
        assert_eq!(cache.refusal(&program(2), "recipe-0", now), Some(cause));

        let oversized = "k".repeat(1025);
        cache.record_refusal(&program(3), oversized.clone(), cause, now);
        assert_eq!(cache.refusal(&program(3), &oversized, now), None);
        assert_eq!(refusals(&cache), 64);
        let largest = "k".repeat(1024);
        cache.record_refusal(&program(3), largest.clone(), cause, now);
        assert_eq!(cache.refusal(&program(3), &largest, now), Some(cause));
    }

    /// Each refusal write sweeps the expired refusals.
    #[test]
    fn refusal_writes_sweep_expired_ones() {
        let programs = Programs::new();
        let program = |index| programs.get(index);
        let cache = InstanceCache::default();
        let written = Instant::now();
        let cause = Incompatibility::FeatureAbsent("tool_list");
        for recipe in 0..10 {
            cache.record_refusal(&program(1), format!("recipe-{recipe}"), cause, written);
        }
        assert_eq!(refusals(&cache), 10);
        let later = written + REFUSAL_TTL;
        cache.record_refusal(&program(1), "fresh".to_owned(), cause, later);
        assert_eq!(refusals(&cache), 1, "expired refusals survived a write");
    }

    /// At most [`VERSIONS_KEPT`] version entries are held.
    #[test]
    fn versions_are_bounded() {
        let cache = InstanceCache::default();
        for ino in 0..100 {
            cache.record_version("vendor", &program(ino), format!("0.{ino}"));
        }
        assert_eq!(cache.entries().versions.len(), VERSIONS_KEPT);
    }
}
