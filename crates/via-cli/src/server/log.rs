//! The daemon log `via.log` (Task 4 design §7.6, amendment A45): the
//! daemon's own `tracing` output and its shutdown summary. During startup
//! a line also goes to stderr, which the auto-starting CLI reads; once the
//! daemon serves, only `via.log` is written.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

/// A `via.log` longer than this is renamed `via.log.1` at start (§7.6).
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// The daemon's one log: its file once open, and whether startup is on.
struct DaemonLog {
    file: Mutex<Option<File>>,
    startup: AtomicBool,
}

static LOG: DaemonLog = DaemonLog {
    file: Mutex::new(None),
    startup: AtomicBool::new(true),
};

/// The subscriber's writer: each formatted event is one `write`.
pub(super) struct Line;

impl Write for Line {
    /// Lines are rare: one synchronous `write_all` each, under the mutex.
    /// A write error is ignored.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        line(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Writes `bytes` to `via.log` once open and, during startup, to stderr.
pub(super) fn line(bytes: &[u8]) {
    if LOG.startup.load(Ordering::Acquire) {
        let _ = io::stderr().lock().write_all(bytes);
    }
    let mut file = LOG.file.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(file) = file.as_mut() {
        let _ = file.write_all(bytes);
    }
}

/// Opens `<state>/via.log` after both locks are held, before the Store
/// opens (§7.6): one longer than 10 MiB is renamed `via.log.1` first,
/// replacing it; then it is opened for append, 0600, following no link and
/// without blocking. An entry that is not a regular file, such as a FIFO,
/// is refused before and after the open, never waited on.
pub(super) fn open(state: &Path) -> io::Result<()> {
    let path = state.join("via.log");
    let irregular = || io::Error::other("must be a regular file");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => return Err(irregular()),
        Ok(metadata) if metadata.len() > ROTATE_BYTES => {
            fs::rename(&path, state.join("via.log.1"))?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                .bits()
                .cast_signed(),
        )
        .open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(irregular());
    }
    *LOG.file.lock().unwrap_or_else(PoisonError::into_inner) = Some(file);
    Ok(())
}

/// Startup is over: from now on only `via.log` is written (§7.6).
pub(super) fn serving() {
    LOG.startup.store(false, Ordering::Release);
}
