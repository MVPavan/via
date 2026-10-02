//! Blob files (Task 4 design §6.5): `<state>/blobs/b_<32 hex>.blob`, each
//! created exclusively (0600, never through a symlink) and owned by the one
//! handle that writes or reads it, on the blocking pool, each step within
//! 2 s. A row names a blob only after its writer finished (synced the file
//! and `blobs/`), in the same transaction; a blob no row names is swept at
//! start.

use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use tokio::{runtime::Handle, sync::oneshot, task::JoinSet};

use crate::{StoreError, evidence::sync_dir};

/// A prompt longer than this many bytes is stored as a blob, not inline.
pub const INLINE_MAX: usize = 256 * 1024;

/// Largest chunk one [`BlobWriter::write`] takes or one
/// [`BlobReader::next_chunk`] returns.
pub const BLOB_CHUNK: usize = 64 * 1024;

/// Largest blob a prompt load accepts, and largest prompt file: C1's
/// 16 MiB prompt (Task 4 design §5.2 `PROMPT_MAX`).
pub const PROMPT_MAX: u64 = 16 * 1024 * 1024;

/// Bound of each blocking blob step.
const BLOB_IO: Duration = Duration::from_secs(2);

/// A finished blob: its id, length and SHA-256, as a row stores it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobRef {
    id: String,
    len: u64,
    sha256: [u8; 32],
}

impl BlobRef {
    /// The blob's id, `b_<32 hex>`; its file is `blobs/<id>.blob`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The blob's length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the blob holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The SHA-256 of the blob's bytes.
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// The stored form, `<id>:<len>:<sha256 hex>`.
    pub(crate) fn encode(&self) -> String {
        format!("{}:{}:{}", self.id, self.len, hex(&self.sha256))
    }

    /// Reads a stored form back; anything else is corrupt evidence.
    pub(crate) fn decode(stored: &str) -> Result<Self, StoreError> {
        let mut parts = stored.split(':');
        let (Some(id), Some(len), Some(sha256), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(StoreError::CorruptEvidence);
        };
        if !valid_id(id) || sha256.len() != 64 {
            return Err(StoreError::CorruptEvidence);
        }
        let mut digest = [0_u8; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&sha256[index * 2..index * 2 + 2], 16)
                .map_err(|_| StoreError::CorruptEvidence)?;
        }
        Ok(Self {
            id: id.to_owned(),
            len: len.parse().map_err(|_| StoreError::CorruptEvidence)?,
            sha256: digest,
        })
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// `b_` and 32 lowercase hex digits.
fn valid_id(id: &str) -> bool {
    id.strip_prefix("b_").is_some_and(|digits| {
        digits.len() == 32
            && digits
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Most blob steps the Store owns at once. `spawn_blocking` work cannot be
/// aborted (coding-style §5), so a step that overran its caller's bound
/// stays owned until it ends; the cap bounds how many blocking threads a
/// stalled filesystem can hold. Four dispatch loads (§5.1) plus a dozen
/// concurrent receipt or discard steps; each healthy step is one 64 KiB
/// write or one sync, or a turn folder's short step (its creation, its
/// `undecoded.bin`, a `logs` `lstat`), so the cap is rarely reached except
/// by a stall.
const BLOB_TASKS: usize = 16;

/// How long `Store::drop` waits for owned blob steps, within final
/// shutdown's 2 s Store reserve (the writer join shares it).
pub(crate) const BLOB_DRAIN: Duration = Duration::from_secs(1);

/// The Store's owned blob steps (coding-style §5 task ownership): every
/// step runs on the blocking pool inside this `JoinSet`, which keeps it
/// until it ends. The mutex is never held across an `.await`. Final
/// shutdown holds a clone to count the steps still running after the
/// Store's bounded drain.
#[derive(Clone, Debug, Default)]
pub struct BlobTasks {
    set: Arc<Mutex<JoinSet<()>>>,
}

impl BlobTasks {
    fn lock(&self) -> MutexGuard<'_, JoinSet<()>> {
        self.set.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Admits `work` onto `runtime`'s blocking pool, after reaping ended
    /// steps; at the cap it is refused at once.
    fn admit(
        &self,
        runtime: &Handle,
        work: impl FnOnce() + Send + 'static,
    ) -> Result<(), StoreError> {
        let mut set = self.lock();
        while set.try_join_next().is_some() {}
        if set.len() >= BLOB_TASKS {
            return Err(StoreError::Write(
                "blob I/O: too many blob steps outstanding".to_owned(),
            ));
        }
        set.spawn_blocking_on(work, runtime);
        Ok(())
    }

    /// Runs one blob step, owned by this set, and waits for its result at
    /// most [`BLOB_IO`]. A step that overran keeps running to its end,
    /// owning what it was given; the caller's request is not committed.
    pub async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> Result<T, StoreError> {
        let runtime = Handle::try_current()
            .map_err(|error| StoreError::Write(format!("blob task: {error}")))?;
        let (reply, result) = oneshot::channel();
        self.admit(&runtime, move || {
            #[cfg(feature = "test-failpoints")]
            if let Err(error) = crate::failpoint::hit("blob.step.stall") {
                let _ = reply.send(Err(error));
                return;
            }
            let _ = reply.send(work());
        })?;
        match tokio::time::timeout(BLOB_IO, result).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(StoreError::Write(format!("blob I/O: {error}"))),
            Ok(Err(_)) => Err(StoreError::Write(
                "blob task ended without a result".to_owned(),
            )),
            Err(_) => Err(StoreError::Write("blob I/O exceeded 2 s".to_owned())),
        }
    }

    /// Like [`Self::run`] for a step on a caller's file, bounded by the
    /// caller's `deadline` instead of 2 s: `Ok(None)` when the deadline
    /// passed first, the step still owned until it ends. The step's own
    /// outcome, errors included, is its value.
    pub(crate) async fn run_until<T: Send + 'static>(
        &self,
        deadline: tokio::time::Instant,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<Option<T>, StoreError> {
        let runtime = Handle::try_current()
            .map_err(|error| StoreError::Write(format!("blob task: {error}")))?;
        let (reply, result) = oneshot::channel();
        self.admit(&runtime, move || {
            let _ = reply.send(work());
        })?;
        match tokio::time::timeout_at(deadline, result).await {
            Ok(Ok(value)) => Ok(Some(value)),
            Ok(Err(_)) => Err(StoreError::Write(
                "blob task ended without a result".to_owned(),
            )),
            Err(_) => Ok(None),
        }
    }

    /// Unlinks `path` from a synchronous `Drop`: inside a runtime the unlink
    /// is an owned step (never blocking a Tokio worker); at the cap it is
    /// left for the start-up sweep; outside any runtime it runs here.
    fn unlink_detached(&self, path: PathBuf) {
        match Handle::try_current() {
            Ok(runtime) => {
                let _ = self.admit(&runtime, move || {
                    let _ = fs::remove_file(path);
                });
            }
            Err(_) => {
                let _ = fs::remove_file(path);
            }
        }
    }

    /// Steps still owned, after reaping ended ones.
    pub fn outstanding(&self) -> usize {
        let mut set = self.lock();
        while set.try_join_next().is_some() {}
        set.len()
    }

    /// Waits at most `bound` for every owned step to end, from a blocking
    /// context (`Store::drop`); returns how many are still running.
    pub(crate) fn drain(&self, bound: Duration) -> usize {
        let deadline = Instant::now() + bound;
        loop {
            let pending = self.outstanding();
            if pending == 0 || Instant::now() >= deadline {
                return pending;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// `<state>/blobs`, validated or created by `Store::open`.
#[derive(Clone, Debug)]
pub(crate) struct Blobs {
    dir: Arc<PathBuf>,
    pub(crate) tasks: BlobTasks,
    /// Test builds: blob files created.
    #[cfg(feature = "test-failpoints")]
    writes: Arc<std::sync::atomic::AtomicU64>,
}

impl Blobs {
    /// Validates `<state>/blobs` as the State directory is validated, or
    /// creates it 0700 and makes it durable with one sync of `state`.
    pub(crate) fn open(state: &Path) -> Result<Self, StoreError> {
        let dir = state.join("blobs");
        if fs::symlink_metadata(&dir).is_ok() {
            crate::runtime::validate_dir(&dir)?;
        } else {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .map_err(|error| StoreError::Open(error.to_string()))?;
            sync_dir(state).map_err(|error| StoreError::Open(error.to_string()))?;
        }
        Ok(Self {
            dir: Arc::new(dir),
            tasks: BlobTasks::default(),
            #[cfg(feature = "test-failpoints")]
            writes: Arc::default(),
        })
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.blob"))
    }

    /// Test builds: how many blob files were created.
    #[cfg(feature = "test-failpoints")]
    pub(crate) fn writes(&self) -> u64 {
        self.writes.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Creates a new blob file under a fresh random id.
    pub(crate) async fn writer(&self) -> Result<BlobWriter, StoreError> {
        let dir = Arc::clone(&self.dir);
        let (id, file) = self
            .tasks
            .run(move || {
                let mut random = [0_u8; 16];
                File::open("/dev/urandom")?.read_exact(&mut random)?;
                let id = format!("b_{}", hex(&random));
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
                    .open(dir.join(format!("{id}.blob")))?;
                Ok((id, file))
            })
            .await?;
        #[cfg(feature = "test-failpoints")]
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(BlobWriter {
            path: self.path(&id),
            dir: Arc::clone(&self.dir),
            tasks: self.tasks.clone(),
            id,
            open: Some((file, Sha256::new())),
            len: 0,
            done: false,
        })
    }

    /// Unlinks a finished blob no committed row names; a failed unlink
    /// leaves it for the start-up sweep.
    pub(crate) async fn discard(&self, blob: &BlobRef) {
        let path = self.path(&blob.id);
        let _ = self.tasks.run(move || fs::remove_file(path)).await;
    }

    /// Opens `blob` for reading: a regular file of its recorded length.
    pub(crate) async fn reader(&self, blob: &BlobRef) -> Result<BlobReader, StoreError> {
        let path = self.path(&blob.id);
        let len = blob.len;
        let opened = self
            .tasks
            .run(move || match open_regular(&path) {
                Ok((file, actual)) => Ok(Some((file, actual))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            })
            .await?;
        match opened {
            Some((file, actual)) if actual == len => Ok(BlobReader {
                tasks: self.tasks.clone(),
                file: Some(file),
                left: len,
            }),
            // Missing, not a regular file, or another length: not the blob.
            _ => Err(StoreError::CorruptEvidence),
        }
    }

    /// Loads a prompt blob into one exact `String`, with a running SHA-256
    /// and a UTF-8 check (design §6.5): a blob that differs from its record
    /// is corrupt evidence.
    pub(crate) async fn load_text(&self, blob: &BlobRef) -> Result<String, StoreError> {
        if blob.len > PROMPT_MAX {
            return Err(StoreError::CorruptEvidence);
        }
        let capacity = usize::try_from(blob.len).map_err(|_| StoreError::CorruptEvidence)?;
        let mut reader = self.reader(blob).await?;
        let mut bytes = Vec::with_capacity(capacity);
        let mut hasher = Sha256::new();
        while let Some(chunk) = reader.next_chunk().await? {
            hasher.update(&chunk);
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != capacity || <[u8; 32]>::from(hasher.finalize()) != blob.sha256 {
            return Err(StoreError::CorruptEvidence);
        }
        String::from_utf8(bytes).map_err(|_| StoreError::CorruptEvidence)
    }

    /// Checks one referenced blob on the SQLite thread (design §6.5
    /// recovery): a regular file of the recorded length and SHA-256.
    pub(crate) fn verify(&self, blob: &BlobRef) -> Result<(), StoreError> {
        let corrupt = || StoreError::Corrupt(format!("blob {}", blob.id));
        let (mut file, len) = open_regular(&self.path(&blob.id)).map_err(|_| corrupt())?;
        if len != blob.len {
            return Err(corrupt());
        }
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; BLOB_CHUNK];
        loop {
            let read = file.read(&mut buffer).map_err(|_| corrupt())?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        if <[u8; 32]>::from(hasher.finalize()) == blob.sha256 {
            Ok(())
        } else {
            Err(corrupt())
        }
    }

    /// Unlinks every blob file whose id is not in `referenced`, on the
    /// SQLite thread; returns how many. Other names are left alone.
    pub(crate) fn sweep(&self, referenced: &HashSet<String>) -> Result<u64, StoreError> {
        let failed = |error: io::Error| StoreError::Write(format!("blob sweep: {error}"));
        let mut swept = 0;
        for entry in fs::read_dir(self.dir.as_path()).map_err(failed)? {
            let name = entry.map_err(failed)?.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".blob")) else {
                continue;
            };
            if valid_id(id) && !referenced.contains(id) {
                fs::remove_file(self.path(id)).map_err(failed)?;
                swept += 1;
            }
        }
        if swept > 0 {
            sync_dir(&self.dir).map_err(failed)?;
        }
        Ok(swept)
    }
}

/// Why a prompt file was not copied (Task 4 design §10.4 step 4).
#[derive(Debug)]
pub enum PromptFileError {
    /// Refused by `reason`: `unreadable`, `not_regular`, `too_large`,
    /// `not_utf8`, `changed` or `timeout`.
    Refused(&'static str),
    /// A blob step failed: the request is not committed.
    Store(StoreError),
}

/// A prompt file's metadata that must not change during the pass.
#[derive(Debug, Eq, PartialEq)]
struct Stamp {
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn of(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            len: metadata.len(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

impl Blobs {
    /// Copies the prompt file at `path` into a new finished blob in one
    /// pass (design §10.4): opened read-only with `O_NONBLOCK`, so a FIFO
    /// cannot block the open, and `fstat`ed: a regular file of at most
    /// `max` bytes ([`PROMPT_MAX`] for a prompt file). It streams in 64 KiB chunks through the blob's
    /// running SHA-256 with a streaming UTF-8 check, then `fstat`s again:
    /// bytes read other than the first size, or a changed size, `mtime`
    /// or `ctime`, refuse it as `changed`. The whole pass ends by
    /// `deadline`. Any refusal discards the blob; the caller holds no lock.
    pub(crate) async fn copy_file(
        &self,
        path: PathBuf,
        deadline: tokio::time::Instant,
        max: u64,
    ) -> Result<BlobRef, PromptFileError> {
        let opened = self
            .tasks
            .run_until(deadline, move || -> io::Result<(File, fs::Metadata)> {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed())
                    .open(path)?;
                let metadata = file.metadata()?;
                Ok((file, metadata))
            })
            .await
            .map_err(PromptFileError::Store)?;
        let (file, first) = match opened {
            None => return Err(PromptFileError::Refused("timeout")),
            Some(Err(_)) => return Err(PromptFileError::Refused("unreadable")),
            Some(Ok(opened)) => opened,
        };
        if !first.is_file() {
            return Err(PromptFileError::Refused("not_regular"));
        }
        if first.len() > max {
            return Err(PromptFileError::Refused("too_large"));
        }
        // Test builds: the pass holds here, with no lock held (§13.2).
        #[cfg(feature = "test-failpoints")]
        let _ = crate::failpoint::hit_async("prompt_file.copy.pause").await;
        let timeout = || PromptFileError::Refused("timeout");
        // The blob's own steps end by `deadline` too. A creation cut off
        // leaves at most an unnamed file, which the start-up sweep removes.
        let mut writer = tokio::time::timeout_at(deadline, self.writer())
            .await
            .map_err(|_| timeout())?
            .map_err(PromptFileError::Store)?;
        if let Err(error) = self
            .copy_chunks(file, &Stamp::of(&first), &mut writer, deadline)
            .await
        {
            writer.discard().await;
            return Err(error);
        }
        // A `finish` cut off drops its writer, which unlinks the file.
        let blob = tokio::time::timeout_at(deadline, writer.finish())
            .await
            .map_err(|_| timeout())?
            .map_err(PromptFileError::Store)?;
        if tokio::time::Instant::now() >= deadline {
            self.discard(&blob).await;
            return Err(timeout());
        }
        Ok(blob)
    }

    /// The streaming part of [`Self::copy_file`]: every chunk into
    /// `writer`, then the second `fstat`.
    async fn copy_chunks(
        &self,
        file: File,
        first: &Stamp,
        writer: &mut BlobWriter,
        deadline: tokio::time::Instant,
    ) -> Result<(), PromptFileError> {
        let timeout = || PromptFileError::Refused("timeout");
        let mut file = Some(file);
        let mut total = 0_u64;
        // An incomplete UTF-8 sequence at a chunk's end (at most 3 bytes).
        let mut carry = Vec::new();
        loop {
            let Some(mut taken) = file.take() else {
                return Err(PromptFileError::Refused("unreadable"));
            };
            let read = self
                .tasks
                .run_until(deadline, move || {
                    let chunk = read_chunk(&mut taken);
                    (taken, chunk)
                })
                .await
                .map_err(PromptFileError::Store)?;
            let (taken, chunk) = read.ok_or_else(timeout)?;
            let chunk = chunk.map_err(|_| PromptFileError::Refused("unreadable"))?;
            file = Some(taken);
            if chunk.is_empty() {
                break;
            }
            total += chunk.len() as u64;
            if total > first.len {
                return Err(PromptFileError::Refused("changed"));
            }
            if !utf8_continues(&mut carry, &chunk) {
                return Err(PromptFileError::Refused("not_utf8"));
            }
            tokio::time::timeout_at(deadline, writer.write(&chunk))
                .await
                .map_err(|_| timeout())?
                .map_err(PromptFileError::Store)?;
        }
        if !carry.is_empty() {
            return Err(PromptFileError::Refused("not_utf8"));
        }
        let Some(file) = file else {
            return Err(PromptFileError::Refused("unreadable"));
        };
        let second = self
            .tasks
            .run_until(deadline, move || file.metadata())
            .await
            .map_err(PromptFileError::Store)?
            .ok_or_else(timeout)?
            .map_err(|_| PromptFileError::Refused("unreadable"))?;
        if total != first.len || Stamp::of(&second) != *first {
            return Err(PromptFileError::Refused("changed"));
        }
        Ok(())
    }
}

/// Reads up to [`BLOB_CHUNK`] bytes, fewer only at end of file.
fn read_chunk(file: &mut File) -> io::Result<Vec<u8>> {
    let mut chunk = vec![0_u8; BLOB_CHUNK];
    let mut filled = 0;
    while filled < BLOB_CHUNK {
        match file.read(&mut chunk[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    chunk.truncate(filled);
    Ok(chunk)
}

/// Checks `chunk` as the continuation of UTF-8 text whose previous chunk
/// left `carry`, an incomplete sequence, which this updates.
fn utf8_continues(carry: &mut Vec<u8>, chunk: &[u8]) -> bool {
    let joined;
    let text = if carry.is_empty() {
        chunk
    } else {
        let mut bytes = std::mem::take(carry);
        bytes.extend_from_slice(chunk);
        joined = bytes;
        &joined[..]
    };
    match std::str::from_utf8(text) {
        Ok(_) => true,
        // Only an incomplete sequence at the end: the next chunk decides.
        Err(error) if error.error_len().is_none() => {
            *carry = text[error.valid_up_to()..].to_vec();
            true
        }
        Err(_) => false,
    }
}

/// Opens `path` without following a symlink and returns it with its length
/// when it is a regular file.
fn open_regular(path: &Path) -> io::Result<(File, u64)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("blob is not a regular file"));
    }
    Ok((file, metadata.len()))
}

/// Writes one new blob. Until [`BlobWriter::finish`] succeeds nothing may
/// name it; `discard` or dropping the handle unlinks it.
pub struct BlobWriter {
    path: PathBuf,
    dir: Arc<PathBuf>,
    tasks: BlobTasks,
    id: String,
    /// The file and the running SHA-256; `None` after a failed step.
    open: Option<(File, Sha256)>,
    len: u64,
    /// Finished or discarded: `Drop` leaves the file alone.
    done: bool,
}

impl BlobWriter {
    /// Appends one chunk of at most [`BLOB_CHUNK`] bytes within 2 s. A
    /// failure is final for this blob: nothing names it yet, so the request
    /// is not committed.
    pub async fn write(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        if chunk.len() > BLOB_CHUNK {
            return Err(StoreError::Constraint("blob chunk over 64 KiB"));
        }
        let (mut file, mut hasher) = self
            .open
            .take()
            .ok_or_else(|| StoreError::Write("blob writer failed earlier".to_owned()))?;
        // Test builds: `fail_io` at occurrence k fails the k-th chunk.
        #[cfg(feature = "test-failpoints")]
        crate::failpoint::hit("blob.write.fail_after")
            .map_err(|error| StoreError::Write(error.to_string()))?;
        let data = chunk.to_vec();
        let (file, hasher) = self
            .tasks
            .run(move || {
                file.write_all(&data)?;
                hasher.update(&data);
                Ok((file, hasher))
            })
            .await?;
        self.open = Some((file, hasher));
        self.len += chunk.len() as u64;
        Ok(())
    }

    /// Syncs the file and then `blobs/`, so the blob is durable before a
    /// row names it, and returns its reference.
    pub async fn finish(mut self) -> Result<BlobRef, StoreError> {
        let (file, hasher) = self
            .open
            .take()
            .ok_or_else(|| StoreError::Write("blob writer failed earlier".to_owned()))?;
        let dir = Arc::clone(&self.dir);
        self.tasks
            .run(move || {
                file.sync_all()?;
                sync_dir(&dir)
            })
            .await?;
        self.done = true;
        Ok(BlobRef {
            id: std::mem::take(&mut self.id),
            len: self.len,
            sha256: hasher.finalize().into(),
        })
    }

    /// Unlinks the unfinished blob on the blocking pool.
    pub async fn discard(mut self) {
        self.done = true;
        let path = std::mem::take(&mut self.path);
        drop(self.open.take());
        let _ = self.tasks.run(move || fs::remove_file(path)).await;
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        if !self.done {
            // An abandoned writer, such as a cancelled request: one owned
            // unlink, off the Tokio worker (review round 1).
            self.tasks.unlink_detached(std::mem::take(&mut self.path));
        }
    }
}

/// Reads one blob in chunks of at most [`BLOB_CHUNK`] bytes.
pub struct BlobReader {
    tasks: BlobTasks,
    file: Option<File>,
    /// Bytes still expected.
    left: u64,
}

impl BlobReader {
    /// The next chunk, `None` at the recorded end. A file that ends early
    /// is corrupt evidence.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, StoreError> {
        if self.left == 0 {
            return Ok(None);
        }
        let mut file = self
            .file
            .take()
            .ok_or_else(|| StoreError::Write("blob reader failed earlier".to_owned()))?;
        let want = usize::try_from(self.left.min(BLOB_CHUNK as u64)).unwrap_or(BLOB_CHUNK);
        let (file, chunk) = self
            .tasks
            .run(move || {
                let mut chunk = vec![0_u8; want];
                let mut filled = 0;
                while filled < want {
                    let read = file.read(&mut chunk[filled..])?;
                    if read == 0 {
                        break;
                    }
                    filled += read;
                }
                chunk.truncate(filled);
                Ok((file, chunk))
            })
            .await?;
        if chunk.is_empty() {
            return Err(StoreError::CorruptEvidence);
        }
        self.left -= chunk.len() as u64;
        self.file = Some(file);
        Ok(Some(chunk))
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::{BLOB_TASKS, BlobTasks, utf8_continues};
    use crate::StoreError;

    /// Review round 1: past [`BLOB_TASKS`] owned steps a new step is
    /// refused at once as `Write` (the request's `not_committed`), and a
    /// `Drop` unlink is left for the sweep; ended steps are reaped.
    #[test]
    fn blob_steps_past_the_cap_are_refused_at_once() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let tasks = BlobTasks::default();
        let (release, blocked) = mpsc::channel::<()>();
        let blocked = std::sync::Arc::new(std::sync::Mutex::new(blocked));
        let root = tempfile::tempdir().expect("dir");
        let kept = root.path().join("kept.blob");
        std::fs::write(&kept, b"x").expect("file");
        runtime.block_on(async {
            for _ in 0..BLOB_TASKS {
                let blocked = std::sync::Arc::clone(&blocked);
                tasks
                    .admit(&tokio::runtime::Handle::current(), move || {
                        let _ = blocked.lock().map(|blocked| blocked.recv());
                    })
                    .expect("admitted");
            }
            assert_eq!(tasks.outstanding(), BLOB_TASKS);
            let refused = tasks.run(|| Ok(())).await;
            assert!(
                matches!(&refused, Err(StoreError::Write(message)) if message.contains("outstanding")),
                "{refused:?}"
            );
            tasks.unlink_detached(kept.clone());
        });
        assert!(
            kept.exists(),
            "a Drop unlink at the cap is left for the sweep"
        );
        drop(release);
        assert_eq!(tasks.drain(Duration::from_secs(10)), 0);
    }

    /// Design §10.4: a character split across chunks carries over; an
    /// invalid byte fails wherever it is.
    #[test]
    fn utf8_check_carries_a_split_character() {
        let text = "a€b".as_bytes();
        let mut carry = Vec::new();
        assert!(utf8_continues(&mut carry, &text[..2]));
        assert_eq!(carry, text[1..2]);
        assert!(utf8_continues(&mut carry, &text[2..3]));
        assert!(utf8_continues(&mut carry, &text[3..]));
        assert!(carry.is_empty());
        let mut carry = Vec::new();
        assert!(!utf8_continues(&mut carry, &[b'a', 0xff]));
        let mut carry = Vec::new();
        assert!(utf8_continues(&mut carry, &[0xe2]));
        assert!(!utf8_continues(&mut carry, b"x"));
    }
}
