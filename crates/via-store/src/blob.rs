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
    sync::Arc,
    time::Duration,
};

use sha2::{Digest, Sha256};

use crate::{StoreError, evidence::sync_dir};

/// A prompt longer than this many bytes is stored as a blob, not inline.
pub const INLINE_MAX: usize = 256 * 1024;

/// Largest chunk one [`BlobWriter::write`] takes or one
/// [`BlobReader::next_chunk`] returns.
pub const BLOB_CHUNK: usize = 64 * 1024;

/// Largest blob a prompt load accepts: C1's 16 MiB prompt.
const PROMPT_MAX: u64 = 16 * 1024 * 1024;

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

fn hex(bytes: &[u8]) -> String {
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

/// Runs one blob step on the blocking pool, within [`BLOB_IO`]. A step that
/// overran keeps running to its end, owning what it was given.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T, StoreError> {
    match tokio::time::timeout(BLOB_IO, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(Ok(value))) => Ok(value),
        Ok(Ok(Err(error))) => Err(StoreError::Write(format!("blob I/O: {error}"))),
        Ok(Err(error)) => Err(StoreError::Write(format!("blob task: {error}"))),
        Err(_) => Err(StoreError::Write("blob I/O exceeded 2 s".to_owned())),
    }
}

/// `<state>/blobs`, validated or created by `Store::open`.
#[derive(Clone, Debug)]
pub(crate) struct Blobs {
    dir: Arc<PathBuf>,
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
        let (id, file) = blocking(move || {
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
        let _ = blocking(move || fs::remove_file(path)).await;
    }

    /// Opens `blob` for reading: a regular file of its recorded length.
    pub(crate) async fn reader(&self, blob: &BlobRef) -> Result<BlobReader, StoreError> {
        let path = self.path(&blob.id);
        let len = blob.len;
        let opened = blocking(move || match open_regular(&path) {
            Ok((file, actual)) => Ok(Some((file, actual))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        })
        .await?;
        match opened {
            Some((file, actual)) if actual == len => Ok(BlobReader {
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
        let (file, hasher) = blocking(move || {
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
        blocking(move || {
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
        let _ = blocking(move || fs::remove_file(path)).await;
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        if !self.done {
            // An abandoned writer, such as a cancelled request: one unlink.
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Reads one blob in chunks of at most [`BLOB_CHUNK`] bytes.
pub struct BlobReader {
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
        let (file, chunk) = blocking(move || {
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
