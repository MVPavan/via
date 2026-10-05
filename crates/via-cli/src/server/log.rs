//! The daemon log `via.log` (Task 4 design §7.6, amendment A45): the
//! daemon's own `tracing` output and its shutdown summary. During startup
//! a line also goes to stderr, which the auto-starting CLI reads; once the
//! daemon serves, only `via.log` is written. It holds daemon warnings and
//! errors, never a prompt or vendor output, and no per-turn lifecycle,
//! which the Store keeps (owner, 2026-10-04).

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

/// A `via.log` longer than this is renamed `via.log.1` at start, and one a
/// line would take past it while the daemon runs (§7.6, bead via-23b).
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// The daemon's one log: its file once open, and whether startup is on.
struct DaemonLog {
    file: Mutex<Option<OpenLog>>,
    startup: AtomicBool,
}

/// The open `via.log`, its length and where it is, for rotation.
struct OpenLog {
    file: File,
    len: u64,
    dir: PathBuf,
    /// A rotation renamed the file but could not open the new one: `file`
    /// is `via.log.1`, and each line retries the open first.
    reopen: bool,
}

impl OpenLog {
    /// `via.log` in `dir`, opened by [`open_file`].
    fn open(dir: &Path) -> io::Result<Self> {
        let file = open_file(&dir.join("via.log"))?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            len,
            dir: dir.to_path_buf(),
            reopen: false,
        })
    }

    /// Writes one line, rotating first when it would take the file past
    /// [`ROTATE_BYTES`]. A failed rotation keeps the current file, which
    /// then grows past the limit rather than lose the line; a write error
    /// is ignored.
    fn write(&mut self, bytes: &[u8]) {
        let bytes_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.reopen {
            self.reopen();
        } else if self.len > 0 && self.len.saturating_add(bytes_len) > ROTATE_BYTES {
            self.rotate();
        }
        if self.file.write_all(bytes).is_ok() {
            self.len = self.len.saturating_add(bytes_len);
        }
    }

    /// Renames `via.log` to `via.log.1`, replacing it, and opens a new one.
    fn rotate(&mut self) {
        if fs::rename(self.dir.join("via.log"), self.dir.join("via.log.1")).is_ok() {
            self.reopen = true;
            self.reopen();
        }
    }

    /// Opens the new `via.log` after a rotation's rename; on failure, such
    /// as no file descriptor left, the next line tries again.
    fn reopen(&mut self) {
        if let Ok(file) = open_file(&self.dir.join("via.log")) {
            self.file = file;
            self.len = 0;
            self.reopen = false;
        }
    }
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
        file.write(bytes);
    }
}

/// Opens `<state>/via.log` after both locks are held, before the Store
/// opens (§7.6): one longer than 10 MiB is renamed `via.log.1` first,
/// replacing it; then it is opened for append, 0600, following no link and
/// without blocking. An entry that is not a regular file, such as a FIFO,
/// is refused before and after the open, never waited on.
pub(super) fn open(state: &Path) -> io::Result<()> {
    let path = state.join("via.log");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => return Err(irregular()),
        Ok(metadata) if metadata.len() > ROTATE_BYTES => {
            fs::rename(&path, state.join("via.log.1"))?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let log = OpenLog::open(state)?;
    *LOG.file.lock().unwrap_or_else(PoisonError::into_inner) = Some(log);
    Ok(())
}

fn irregular() -> io::Error {
    io::Error::other("must be a regular file")
}

/// `via.log` at `path` for append, 0600, following no link and without
/// blocking; refused unless it is a regular file.
fn open_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                .bits()
                .cast_signed(),
        )
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(irregular());
    }
    Ok(file)
}

/// Startup is over: from now on only `via.log` is written (§7.6).
pub(super) fn serving() {
    LOG.startup.store(false, Ordering::Release);
}

/// A startup failure after `via.log` opened (bead via-23b): one `ERROR`
/// line with its whole cause chain, in `via.log` only, since the daemon's
/// exit report already goes to stderr.
pub(super) fn startup_failed(error: &anyhow::Error) {
    serving();
    tracing::error!(cause = %format!("{error:#}"), "daemon startup failed");
}

/// The most bytes of a panic's `via.log` line.
const PANIC_LINE: usize = 1024;

/// Installs the daemon's panic hook once `via.log` is open (x.3.2 X0 item
/// 2.5, r8 R8-1). It replaces the default hook and never chains it: the
/// default writes stderr, which can block on an undrained pipe before the
/// unwind reaches an abort. The hook formats one JSON line, message and
/// location, truncated to 1 KiB, in a stack buffer and writes it to the
/// `via.log` file only, under `try_lock`: a held or poisoned log skips the
/// line and a write error is ignored. It never panics and never waits.
pub(super) fn panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let mut line = Bounded::default();
        line.push(br#"{"level":"ERROR","panic":""#);
        let message = info.payload_as_str().unwrap_or("Box<dyn Any>");
        line.escaped(message);
        line.push(br#"","location":""#);
        if let Some(location) = info.location() {
            line.escaped(location.file());
            let mut digits = itoa_buffer();
            line.push(b":");
            line.push(itoa(location.line(), &mut digits));
            line.push(b":");
            line.push(itoa(location.column(), &mut digits));
        }
        line.close(b"\"}\n");
        if let Ok(mut file) = LOG.file.try_lock()
            && let Some(file) = file.as_mut()
        {
            let _ = file.file.write_all(line.bytes());
        }
    }));
}

/// A line in a stack buffer, truncated at [`PANIC_LINE`] with room kept
/// for its close.
struct Bounded {
    buffer: [u8; PANIC_LINE],
    len: usize,
}

impl Default for Bounded {
    fn default() -> Self {
        Self {
            buffer: [0; PANIC_LINE],
            len: 0,
        }
    }
}

impl Bounded {
    /// Room kept for the close: `"}` and the newline.
    const CLOSE: usize = 3;

    /// Appends `bytes` whole, or nothing once they would not fit.
    fn push(&mut self, bytes: &[u8]) -> bool {
        let end = self.len + bytes.len();
        if end > PANIC_LINE - Self::CLOSE {
            return false;
        }
        self.buffer[self.len..end].copy_from_slice(bytes);
        self.len = end;
        true
    }

    /// Appends `text` as JSON string content, up to the bound.
    fn escaped(&mut self, text: &str) {
        for c in text.chars() {
            let mut utf8 = [0; 4];
            let fits = match c {
                '"' => self.push(b"\\\""),
                '\\' => self.push(b"\\\\"),
                '\n' => self.push(b"\\n"),
                c if u32::from(c) < 0x20 => {
                    let hex = b"0123456789abcdef";
                    let code = u32::from(c) as usize;
                    self.push(&[b'\\', b'u', b'0', b'0', hex[code >> 4], hex[code & 15]])
                }
                c => self.push(c.encode_utf8(&mut utf8).as_bytes()),
            };
            if !fits {
                return;
            }
        }
    }

    /// Appends the close, for which room was kept.
    fn close(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        self.buffer[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }

    fn bytes(&self) -> &[u8] {
        &self.buffer[..self.len]
    }
}

/// A buffer for [`itoa`].
fn itoa_buffer() -> [u8; 10] {
    [0; 10]
}

/// `value` in decimal, in `buffer`.
fn itoa(mut value: u32, buffer: &mut [u8; 10]) -> &[u8] {
    let mut start = buffer.len();
    loop {
        start -= 1;
        buffer[start] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        value /= 10;
        if value == 0 {
            return &buffer[start..];
        }
    }
}

#[cfg(test)]
mod tests {
    //! The hook's case runs in a child copy of this test binary, since the
    //! abort ends it: the parent re-executes itself on the one test and
    //! reads how the child ended (x.3.2 X0 item 2.5, Ruling D).

    use std::io::Write;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Names the scenario a child runs, with its state directory.
    const CHILD: &str = "VIA_PANIC_HOOK_CHILD";
    const SIGABRT: i32 = 6;

    /// The hook's line stays within 1 KiB and stays one JSON line, its
    /// message escaped and truncated whole characters at a time.
    #[test]
    fn panic_line_is_bounded_json() {
        let mut line = super::Bounded::default();
        line.push(br#"{"panic":""#);
        line.escaped(&format!("a\"b\\c\nd\u{1}é{}", "x".repeat(4096)));
        line.close(b"\"}\n");
        let bytes = line.bytes();
        assert!(bytes.len() <= super::PANIC_LINE);
        let text = std::str::from_utf8(bytes).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(
            value["panic"]
                .as_str()
                .unwrap()
                .starts_with("a\"b\\c\nd\u{1}éxx")
        );
        let mut digits = super::itoa_buffer();
        assert_eq!(super::itoa(0, &mut digits), b"0");
        assert_eq!(super::itoa(u32::MAX, &mut digits), b"4294967295");
    }

    /// Names the rotation scenario's directory in its child.
    const ROTATE_CHILD: &str = "VIA_LOG_ROTATE_CHILD";

    /// Bead via-23b fix round 1: a rotation whose reopen fails (no file
    /// descriptor left) keeps the line in the renamed file and retries the
    /// reopen on later lines, so `via.log` comes back once descriptors do.
    /// Runs in a child copy, since it lowers the process's descriptor limit.
    #[test]
    fn rotation_recovers_after_a_failed_reopen() {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        if let Ok(dir) = std::env::var(ROTATE_CHILD) {
            let dir = Path::new(&dir);
            let prefill = vec![b'a'; usize::try_from(super::ROTATE_BYTES).unwrap() - 2];
            std::fs::write(dir.join("via.log"), &prefill).unwrap();
            let mut log = super::OpenLog::open(dir).unwrap();
            let limit = getrlimit(Resource::Nofile);
            setrlimit(
                Resource::Nofile,
                Rlimit {
                    current: Some(256),
                    maximum: limit.maximum,
                },
            )
            .unwrap();
            let mut held = Vec::new();
            while let Ok(file) = std::fs::File::open("/dev/null") {
                held.push(file);
            }
            log.write(b"one\n");
            log.write(b"two\n");
            drop(held);
            log.write(b"three\n");
            setrlimit(Resource::Nofile, limit).unwrap();
            let mut rotated = prefill;
            rotated.extend_from_slice(b"one\ntwo\n");
            assert_eq!(
                std::fs::read_to_string(dir.join("via.log")).ok().as_deref(),
                Some("three\n"),
                "via.log did not come back"
            );
            assert!(
                std::fs::read(dir.join("via.log.1")).unwrap() == rotated,
                "via.log.1 is not the full log and the lines of the failed reopen"
            );
            std::process::exit(0);
        }
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "server::log::tests::rotation_recovers_after_a_failed_reopen",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROTATE_CHILD, dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child ended {:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// X0 item 2.7 `panic_hook_aborts_with_full_stderr`: with its stderr a
    /// full pipe nobody reads, a panic the daemon turns into an abort
    /// (`crash_on_panic`, the registry guard) still ends the daemon by
    /// `SIGABRT` within a bound: the hook writes its one line to `via.log`
    /// and never to stderr, where the default hook would block.
    #[test]
    fn panic_hook_aborts_with_full_stderr() {
        if let Ok(dir) = std::env::var(CHILD) {
            super::open(Path::new(&dir)).unwrap();
            super::serving();
            super::panic_hook();
            let unwound = std::panic::catch_unwind(|| panic!("registry step \"failed\""));
            if unwound.is_err() {
                std::process::abort();
            }
            std::process::exit(0);
        }
        let dir = tempfile::tempdir().unwrap();
        let (reader, mut writer) = std::io::pipe().unwrap();
        rustix::fs::fcntl_setfl(&writer, rustix::fs::OFlags::NONBLOCK).unwrap();
        let block = [b'x'; 4096];
        while writer.write(&block).is_ok() {}
        rustix::fs::fcntl_setfl(&writer, rustix::fs::OFlags::empty()).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "server::log::tests::panic_hook_aborts_with_full_stderr",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, dir.path())
            .stdout(Stdio::null())
            .stderr(writer)
            .spawn()
            .unwrap();
        let bound = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() > bound {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(reader);
        let status = status.expect("the child blocked on its full stderr");
        assert_eq!(status.signal(), Some(SIGABRT), "child ended {status:?}");
        let log = std::fs::read_to_string(dir.path().join("via.log")).unwrap();
        let line: serde_json::Value = serde_json::from_str(log.trim_end()).unwrap();
        assert_eq!(line["panic"], "registry step \"failed\"", "{log}");
        assert!(
            line["location"].as_str().unwrap().contains("log.rs"),
            "{log}"
        );
    }
}
