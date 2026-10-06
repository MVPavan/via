//! The anchor's launch fence (runtime §5 "Die with the anchor" and
//! "Exclusive launch lock"; `vendors/opencode.md` §3.2): the credential and
//! program checks, the best-effort version check, the exclusive lock, the
//! server record and the predecessor's exit proof. Everything here runs in
//! the anchor before ARM, except the record write. Processes are spawned
//! on the anchor's main thread only; filesystem work runs on Tokio's
//! blocking pool, which owns what it opens until it hands it back, so the
//! anchor keeps serving its control, `SIGTERM` and its timers meanwhile.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use rustix::{
    event::{PollFd, PollFlags, Timespec},
    io::Errno,
    process::{Pid, PidfdFlags},
};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, time::Instant};

use crate::{
    FenceRefusal, ProbeFailure, linux,
    protocol::{ProbeConfig, VendorConfig},
};

/// The server record's fixed length: magic, boot ID, PID namespace and time
/// namespace (each [`FIELD`] bytes, NUL-padded), pid, start ticks and a
/// SHA-256 of the rest.
pub(crate) const RECORD_LEN: usize = 8 + TEXT_FIELDS * FIELD + 4 + 8 + 32;
const MAGIC: &[u8; 8] = b"VIASRV\0\x01";
const FIELD: usize = 64;
/// The record's text fields: boot ID, PID namespace, time namespace.
const TEXT_FIELDS: usize = 3;
/// How long a present predecessor is waited for (runtime §5 step 4).
const PREDECESSOR_WAIT: Duration = Duration::from_secs(1);
/// The predecessor poll's interval: the pidfd is polled without blocking
/// the anchor's runtime.
const PREDECESSOR_POLL: Duration = Duration::from_millis(5);
/// The version check's bound and output cap (runtime §5 step 2).
const PROBE_BOUND: Duration = Duration::from_secs(2);
const PROBE_OUTPUT: usize = 256;
/// `PredecessorUncertain`'s namespace for a malformed record.
const MALFORMED: &str = "malformed server record";

/// One server record: the process a lock holder launched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServerRecord {
    pub boot_id: String,
    pub pid_namespace: String,
    /// The writer's time namespace: start ticks are shifted by the reader's
    /// time-namespace offset, so they compare only within one.
    pub time_namespace: String,
    pub pid: u32,
    pub start_ticks: u64,
}

impl ServerRecord {
    /// The record naming `pid` with `start_ticks` in this boot, PID
    /// namespace and time namespace.
    pub(crate) fn current(pid: u32, start_ticks: u64) -> io::Result<Self> {
        Ok(Self {
            boot_id: linux::boot_id()?,
            pid_namespace: linux::pid_namespace()?,
            time_namespace: linux::time_namespace()?,
            pid,
            start_ticks,
        })
    }

    /// The record's bytes, or `None` when a field does not fit.
    pub(crate) fn encode(&self) -> Option<[u8; RECORD_LEN]> {
        let mut bytes = [0_u8; RECORD_LEN];
        bytes[..8].copy_from_slice(MAGIC);
        let texts = [&self.boot_id, &self.pid_namespace, &self.time_namespace];
        for (index, text) in texts.into_iter().enumerate() {
            let start = 8 + index * FIELD;
            bytes
                .get_mut(start..start + text.len())
                .filter(|_| text.len() <= FIELD && !text.contains('\0'))?
                .copy_from_slice(text.as_bytes());
        }
        let numbers = 8 + TEXT_FIELDS * FIELD;
        bytes[numbers..numbers + 4].copy_from_slice(&self.pid.to_le_bytes());
        bytes[numbers + 4..numbers + 12].copy_from_slice(&self.start_ticks.to_le_bytes());
        let digest = Sha256::digest(&bytes[..RECORD_LEN - 32]);
        bytes[RECORD_LEN - 32..].copy_from_slice(&digest);
        Some(bytes)
    }

    /// What a server-record file's whole contents hold (runtime §5 step
    /// 4). Missing, short or torn (the checksum fails) is no record; a
    /// checksum that holds over anything but exactly one well-formed record
    /// (another magic, a malformed field, a pid out of range, more bytes
    /// after it) is malformed, never taken for absence.
    pub(crate) fn decode(bytes: &[u8]) -> Decoded {
        let Some(record) = bytes.get(..RECORD_LEN) else {
            return Decoded::Absent;
        };
        if Sha256::digest(&record[..RECORD_LEN - 32]).as_slice() != &record[RECORD_LEN - 32..] {
            return Decoded::Absent;
        }
        let text = |start: usize| {
            let field = &record[start..start + FIELD];
            let end = field.iter().position(|byte| *byte == 0).unwrap_or(FIELD);
            let (text, padding) = field.split_at(end);
            (padding.iter().all(|byte| *byte == 0) && text.iter().all(u8::is_ascii_graphic))
                .then(|| String::from_utf8_lossy(text).into_owned())
        };
        let numbers = 8 + TEXT_FIELDS * FIELD;
        let mut pid = [0_u8; 4];
        pid.copy_from_slice(&record[numbers..numbers + 4]);
        let pid = u32::from_le_bytes(pid);
        let mut start_ticks = [0_u8; 8];
        start_ticks.copy_from_slice(&record[numbers + 4..numbers + 12]);
        let start_ticks = u64::from_le_bytes(start_ticks);
        let well_formed = bytes.len() == RECORD_LEN
            && &record[..8] == MAGIC
            && i32::try_from(pid).is_ok_and(|pid| pid > 0);
        match (well_formed, text(8), text(8 + FIELD), text(8 + 2 * FIELD)) {
            (true, Some(boot_id), Some(pid_namespace), Some(time_namespace))
                if is_boot_id(&boot_id)
                    && is_namespace(&pid_namespace, "pid")
                    && is_time_namespace(&time_namespace) =>
            {
                Decoded::Record(Self {
                    boot_id,
                    pid_namespace,
                    time_namespace,
                    pid,
                    start_ticks,
                })
            }
            _ => Decoded::Malformed,
        }
    }
}

/// A server-record file's contents (see [`ServerRecord::decode`]).
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Decoded {
    /// Missing, short or torn: no record.
    Absent,
    /// The checksum holds but the contents are not one well-formed record.
    Malformed,
    /// One well-formed record.
    Record(ServerRecord),
}

/// Reads a server-record file's first `RECORD_LEN + 1` bytes from `file`,
/// enough to tell a record of exactly the fixed length from a longer file.
pub(crate) fn read_record_bytes(file: &File) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0_u8; RECORD_LEN + 1];
    let mut filled = 0;
    while filled < bytes.len() {
        match file.read_at(&mut bytes[filled..], filled as u64) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    bytes.truncate(filled);
    Ok(bytes)
}

/// Writes the record naming `pid` with `start_ticks` through the lock's
/// descriptor `file`: it first truncates the file, so no byte of an earlier
/// record survives, then writes the whole fixed-length record in one
/// positioned write at offset 0. Any error or short write fails, and leaves
/// at most a prefix shorter than a record, which decodes as no record.
pub(crate) fn write_record(file: &File, pid: u32, start_ticks: u64) -> io::Result<()> {
    let bytes = ServerRecord::current(pid, start_ticks)?
        .encode()
        .ok_or_else(|| io::Error::other("server record field too long"))?;
    file.set_len(0)?;
    // Test builds: a short write of all but the last byte (`OC02b`).
    #[cfg(feature = "test-failpoints")]
    if via_store::failpoint::hit("host.anchor.short_record_write").is_err() {
        let _ = file.write_at(&bytes[..RECORD_LEN - 1], 0);
        return Err(io::Error::other("short server record write"));
    }
    if file.write_at(&bytes, 0)? != RECORD_LEN {
        return Err(io::Error::other("short server record write"));
    }
    Ok(())
}

/// The exclusive launch lock: an open, `flock`ed descriptor the anchor
/// keeps, never unlocks and never closes; the kernel releases it when the
/// anchor exits. It is close-on-exec, so no child inherits it.
pub(crate) struct LaunchLock {
    file: File,
    path: PathBuf,
}

impl LaunchLock {
    /// The lock file's path, the exec entry's record path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// A second descriptor of the lock's open file, for the record write on
    /// the blocking pool. It shares the lock (one open file description);
    /// closing it never releases the lock while the anchor keeps its own.
    pub(crate) fn writer(&self) -> io::Result<File> {
        self.file.try_clone()
    }
}

/// The checks a fenced configuration passes before it is accepted
/// (runtime §5, `Configure` steps 1 to 4), in order. The lock, when one is
/// configured, is held on success.
///
/// The credential and program checks, and the lock's open, `flock` and
/// record read, run on the blocking pool; the version check is spawned
/// here, on the anchor's main thread. When this future is dropped (the
/// anchor stops serving the configuration), a blocking step still running
/// keeps what it opened until it returns and then drops it: the lock is
/// released then, or when the anchor exits, whichever comes first.
pub(crate) async fn configure(vendor: &VendorConfig) -> Result<Option<LaunchLock>, FenceRefusal> {
    let program = vendor.program();
    blocking(FenceRefusal::ProgramUnchecked { errno: None }, move || {
        let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
        if !unprivileged(&status) {
            return Err(FenceRefusal::PrivilegedVia);
        }
        check_program(&program)
    })
    .await?;
    if let Some(probe) = &vendor.version_probe {
        run_probe(&vendor.program(), probe).await?;
    }
    let Some(path) = vendor.exclusive_lock() else {
        return Ok(None);
    };
    let (lock, record) = blocking(FenceRefusal::LockUnavailable { errno: None }, move || {
        let lock = take_lock(&path)?;
        let bytes =
            read_record_bytes(&lock.file).map_err(|error| FenceRefusal::RecordUnreadable {
                errno: error.raw_os_error(),
            })?;
        Ok((lock, ServerRecord::decode(&bytes)))
    })
    .await?;
    check_predecessor(record).await?;
    Ok(Some(lock))
}

/// Runs `work` on the blocking pool, which owns everything it opens until
/// it returns; a task that panicked is `lost`.
async fn blocking<T: Send + 'static>(
    lost: FenceRefusal,
    work: impl FnOnce() -> Result<T, FenceRefusal> + Send + 'static,
) -> Result<T, FenceRefusal> {
    tokio::task::spawn_blocking(work).await.unwrap_or(Err(lost))
}

/// Runtime §5 step 1: in `/proc/self/status`, the four `Uid:` values are
/// equal and not 0, the four `Gid:` values are equal, and `CapPrm`,
/// `CapEff` and `CapAmb` are zero. Anything unreadable fails closed.
pub(crate) fn unprivileged(status: &str) -> bool {
    let field = |name: &str| -> Option<Vec<&str>> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|rest| rest.split_ascii_whitespace().collect())
    };
    let same = |values: &[&str]| -> Option<u32> {
        let [first, rest @ ..] = values else {
            return None;
        };
        (values.len() == 4 && rest.iter().all(|value| value == first))
            .then(|| first.parse().ok())
            .flatten()
    };
    let no_capabilities = ["CapPrm:", "CapEff:", "CapAmb:"].iter().all(|name| {
        field(name).is_some_and(|values| {
            values.len() == 1 && u64::from_str_radix(values[0], 16).is_ok_and(|mask| mask == 0)
        })
    });
    let uid = field("Uid:").and_then(|values| same(&values));
    let gid = field("Gid:").and_then(|values| same(&values));
    no_capabilities && uid.is_some_and(|uid| uid != 0) && gid.is_some()
}

/// Runtime §5 step 1: a program file with the set-user-ID or set-group-ID
/// bit or a `security.capability` attribute is refused; one whose
/// privileges cannot be read is refused too.
fn check_program(program: &Path) -> Result<(), FenceRefusal> {
    let unchecked = |errno: Option<i32>| FenceRefusal::ProgramUnchecked { errno };
    let metadata = fs::metadata(program).map_err(|error| unchecked(error.raw_os_error()))?;
    if metadata.mode() & 0o6000 != 0 {
        return Err(FenceRefusal::ProgramPrivileged);
    }
    let mut value = [0_u8; 64];
    match rustix::fs::getxattr(program, "security.capability", &mut value[..]) {
        Ok(_) | Err(Errno::RANGE) => Err(FenceRefusal::ProgramPrivileged),
        // No attribute, or a filesystem without extended attributes.
        Err(Errno::NODATA | Errno::NOTSUP) => Ok(()),
        Err(error) => Err(unchecked(Some(error.raw_os_error()))),
    }
}

/// The exec entry's command line (runtime §5): `/proc/self/exe
/// __via_host_exec <anchor pid> <record path or -> <program> <args…>`. Test
/// builds pass the failpoint controller's activation first.
pub(crate) fn exec_command(
    record: Option<&Path>,
    program: &Path,
    args: impl Iterator<Item = OsString>,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("/proc/self/exe");
    command.arg("__via_host_exec");
    #[cfg(feature = "test-failpoints")]
    if let Some((dir, token)) = via_store::failpoint::activation() {
        command.arg("--failpoints").arg(dir).arg(token);
    }
    command
        .arg(std::process::id().to_string())
        .arg(record.map_or_else(|| OsString::from("-"), |path| path.as_os_str().to_owned()))
        .arg(program)
        .args(args);
    command
}

/// Runtime §5 step 2: runs the program through the exec entry (no record)
/// with the probe's arguments, cwd and environment, stdin `/dev/null`,
/// stdout kept up to 256 bytes and stderr discarded, killing and waiting
/// for it at 2 s. Spawned on the anchor's main thread, so it dies with the
/// anchor.
async fn run_probe(program: &Path, probe: &ProbeConfig) -> Result<(), FenceRefusal> {
    let deadline = Instant::now() + PROBE_BOUND;
    let mut command = exec_command(None, program, probe.args());
    command
        .current_dir(probe.cwd())
        .env_clear()
        .envs(probe.env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Synchronous `chdir` at spawn: runtime §5 needs a local, responsive state directory.
    let mut child = crate::anchor::spawn_with_vendor_umask(&mut command).map_err(|error| {
        FenceRefusal::ProbeFailed {
            kind: ProbeFailure::Spawn {
                errno: error.raw_os_error(),
            },
        }
    })?;
    drop(command);
    let failed = |kind| FenceRefusal::ProbeFailed { kind };
    // The output is read up to one byte past the cap, so an overflow is
    // found as soon as it is read, not at the probe's exit.
    let stdout = child.stdout.take();
    let read = async {
        let mut output = Vec::with_capacity(PROBE_OUTPUT + 1);
        if let Some(stdout) = stdout {
            stdout
                .take(PROBE_OUTPUT as u64 + 1)
                .read_to_end(&mut output)
                .await?;
        }
        Ok::<_, io::Error>(output)
    };
    let output = match tokio::time::timeout_at(deadline, read).await {
        Ok(Ok(output)) if output.len() <= PROBE_OUTPUT => output,
        Ok(Ok(_)) => {
            stop(&mut child).await;
            return Err(failed(ProbeFailure::Overflow));
        }
        Ok(Err(_)) => {
            stop(&mut child).await;
            return Err(failed(ProbeFailure::Read));
        }
        Err(_) => {
            stop(&mut child).await;
            return Err(failed(ProbeFailure::Timeout));
        }
    };
    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            stop(&mut child).await;
            return Err(failed(ProbeFailure::Read));
        }
        Err(_) => {
            stop(&mut child).await;
            return Err(failed(ProbeFailure::Timeout));
        }
    };
    if !status.success() {
        use std::os::unix::process::ExitStatusExt;
        return Err(failed(ProbeFailure::Exit {
            code: status.code(),
            signal: status.signal(),
        }));
    }
    let trimmed = output.trim_ascii();
    if probe
        .admitted
        .iter()
        .any(|admitted| admitted.as_bytes() == trimmed)
    {
        return Ok(());
    }
    Err(FenceRefusal::ProbeRefused {
        output: printable(trimmed),
    })
}

/// Kills and waits for a probe that overran or overflowed; its exit is
/// waited for within a short bound, since `SIGKILL` cannot be refused.
async fn stop(child: &mut tokio::process::Child) {
    // A probe that already exited cannot be killed; the wait reaps it.
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
}

/// The output as printable ASCII, other bytes as `?`.
fn printable(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                char::from(*byte)
            } else {
                '?'
            }
        })
        .collect()
}

/// Runtime §5 step 3: opens the lock path (created 0600, no symlink
/// followed, close-on-exec, a regular file of VIA's uid) and takes
/// `flock(LOCK_EX | LOCK_NB)`.
fn take_lock(path: &Path) -> Result<LaunchLock, FenceRefusal> {
    let unavailable = |errno: Option<i32>| FenceRefusal::LockUnavailable { errno };
    crate::exec::seam("host.anchor.before_lock")
        .map_err(|error| unavailable(error.raw_os_error()))?;
    // std opens every file close-on-exec.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .map_err(|error| unavailable(error.raw_os_error()))?;
    let metadata = file
        .metadata()
        .map_err(|error| unavailable(error.raw_os_error()))?;
    if !metadata.file_type().is_file() || metadata.uid() != rustix::process::getuid().as_raw() {
        return Err(unavailable(None));
    }
    match file.try_lock() {
        Ok(()) => Ok(LaunchLock {
            file,
            path: path.to_path_buf(),
        }),
        Err(std::fs::TryLockError::WouldBlock) => Err(FenceRefusal::LockHeld),
        Err(std::fs::TryLockError::Error(error)) => Err(unavailable(error.raw_os_error())),
    }
}

/// A boot ID as the kernel prints it: a lowercase, hyphenated UUID. Only
/// such text can be evidence of another boot (review ochost2 #1).
fn is_boot_id(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// A namespace of `kind` as `/proc/self/ns/<kind>` reads: `<kind>:[<inode>]`.
fn is_namespace(text: &str, kind: &str) -> bool {
    text.strip_prefix(kind)
        .and_then(|rest| rest.strip_prefix(":["))
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|inode| !inode.is_empty() && inode.bytes().all(|byte| byte.is_ascii_digit()))
}

/// A time namespace as [`linux::time_namespace`] gives it: `time:[<inode>]`,
/// or the one token of a kernel without time namespaces.
fn is_time_namespace(text: &str) -> bool {
    text == linux::NO_TIME_NAMESPACE || is_namespace(text, "time")
}

/// Runtime §5 step 4: the recorded server is proved gone, or refused. A
/// malformed record proves nothing: `PredecessorUncertain`.
async fn check_predecessor(record: Decoded) -> Result<(), FenceRefusal> {
    let unreadable = |error: io::Error| FenceRefusal::RecordUnreadable {
        errno: error.raw_os_error(),
    };
    let record = match record {
        Decoded::Absent => return Ok(()),
        Decoded::Malformed => {
            return Err(FenceRefusal::PredecessorUncertain {
                namespace: MALFORMED.into(),
            });
        }
        Decoded::Record(record) => record,
    };
    if record.boot_id != linux::boot_id().map_err(unreadable)? {
        return Ok(());
    }
    if record.pid_namespace != linux::pid_namespace().map_err(unreadable)? {
        return Err(FenceRefusal::PredecessorUncertain {
            namespace: record.pid_namespace,
        });
    }
    // Start ticks are shifted by the reader's time-namespace offset (review
    // ochostcrit #1): another time namespace's ticks prove nothing here.
    if record.time_namespace != linux::time_namespace().map_err(unreadable)? {
        return Err(FenceRefusal::PredecessorUncertain {
            namespace: record.time_namespace,
        });
    }
    let deadline = Instant::now() + PREDECESSOR_WAIT;
    loop {
        if exited(record.pid, record.start_ticks).await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(FenceRefusal::PredecessorAlive {
                pid: record.pid,
                start_ticks: record.start_ticks,
            });
        }
        tokio::time::sleep(PREDECESSOR_POLL).await;
    }
}

/// What `/proc/<pid>/stat` says about the recorded process.
#[derive(Debug, Eq, PartialEq)]
enum Identity {
    /// Other start ticks: the pid names another process (reused).
    Gone,
    /// The recorded start ticks.
    Matches,
    /// No such entry (`ENOENT` or `ESRCH`): exited, or hidden by `hidepid`
    /// (a live same-uid non-dumpable process), so it proves nothing alone
    /// (review ochostcrit #2); the pidfd decides.
    Unseen,
    /// It could not be read: fails closed.
    Unreadable,
}

/// Classifies one read of `/proc/<pid>/stat` against `ticks`.
fn identity(read: io::Result<String>, ticks: u64) -> Identity {
    match read {
        Ok(text) => match stat_start_ticks(&text) {
            Some(start) if start == ticks => Identity::Matches,
            Some(_) => Identity::Gone,
            None => Identity::Unreadable,
        },
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::SRCH.raw_os_error()) =>
        {
            Identity::Unseen
        }
        Err(_) => Identity::Unreadable,
    }
}

fn stat_start_ticks(text: &str) -> Option<u64> {
    text.rsplit_once(") ")?
        .1
        .split_ascii_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn read_identity(pid: u32, ticks: u64) -> Identity {
    identity(fs::read_to_string(format!("/proc/{pid}/stat")), ticks)
}

/// One exact exit proof of the recorded process (runtime §5 step 4):
/// identity first, then `pidfd_open`, identity again, then a pidfd poll
/// that is readable only once the whole thread group has exited. Only a
/// readable entry with other start ticks is gone at an identity read; an
/// unseen entry goes on to the pidfd, whose `ESRCH` or readable poll
/// proves exit. A pid reused after the recorded process left procfs
/// (unseen, then reused by a live process) is present: that fails
/// closed, costing liveness only. Anything else is present.
async fn exited(pid: u32, ticks: u64) -> bool {
    match read_identity(pid, ticks) {
        Identity::Gone => return true,
        Identity::Unreadable => return false,
        Identity::Matches | Identity::Unseen => {}
    }
    proof_seam().await;
    let Some(raw) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    let pidfd = match rustix::process::pidfd_open(raw, PidfdFlags::empty()) {
        Ok(pidfd) => pidfd,
        Err(Errno::SRCH) => return true,
        Err(_) => return false,
    };
    match read_identity(pid, ticks) {
        Identity::Gone => return true,
        Identity::Unreadable => return false,
        Identity::Matches | Identity::Unseen => {}
    }
    let mut fds = [PollFd::new(&pidfd, PollFlags::IN)];
    let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    loop {
        match rustix::event::poll(&mut fds, Some(&zero)) {
            Ok(_) => return fds[0].revents().contains(PollFlags::IN),
            Err(Errno::INTR) => {}
            Err(_) => return false,
        }
    }
}

/// Test-only `host.anchor.predecessor_after_identity`: a pause holds the
/// predecessor proof between its first identity read and `pidfd_open`, so a
/// test can force an exec there (`OC02b`).
#[cfg(feature = "test-failpoints")]
async fn proof_seam() {
    // A pause point only: an acknowledgement that cannot be written skips
    // the pause, and the proof goes on unchanged.
    let _ = via_store::failpoint::hit_async("host.anchor.predecessor_after_identity").await;
}

/// Release builds never hold here.
#[cfg(not(feature = "test-failpoints"))]
#[expect(clippy::unused_async, reason = "test builds pause here")]
async fn proof_seam() {}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "Name:\tvia\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n\
                          CapInh:\t0000000000000000\nCapPrm:\t0000000000000000\n\
                          CapEff:\t0000000000000000\nCapBnd:\t000001ffffffffff\n\
                          CapAmb:\t0000000000000000\n";

    /// Runtime §5 step 1 (`PrivilegedVia`): one unprivileged identity
    /// passes; uid 0, differing user or group IDs, any permitted,
    /// effective or ambient capability, and a missing field are refused.
    #[test]
    fn credential_check_needs_one_unprivileged_identity() {
        assert!(unprivileged(STATUS));
        for (from, to) in [
            ("Uid:\t1000\t1000\t1000\t1000", "Uid:\t0\t0\t0\t0"),
            ("Uid:\t1000\t1000\t1000\t1000", "Uid:\t1000\t0\t1000\t1000"),
            (
                "Uid:\t1000\t1000\t1000\t1000",
                "Uid:\t1000\t1000\t1000\t1001",
            ),
            (
                "Gid:\t1000\t1000\t1000\t1000",
                "Gid:\t1000\t1001\t1001\t1000",
            ),
            ("Gid:\t1000\t1000\t1000\t1000", "Gid:\t1000\t1000\t1000"),
            ("CapPrm:\t0000000000000000", "CapPrm:\t0000000000000400"),
            ("CapEff:\t0000000000000000", "CapEff:\t0000000000000001"),
            ("CapAmb:\t0000000000000000", "CapAmb:\t0000000000002000"),
            ("CapAmb:\t0000000000000000\n", ""),
        ] {
            let status = STATUS.replace(from, to);
            assert_ne!(status, STATUS);
            assert!(!unprivileged(&status), "{to}");
        }
        assert!(!unprivileged(""));
    }

    fn sample() -> ServerRecord {
        ServerRecord {
            boot_id: "0d3f5c2e-1111-2222-3333-444455556666".into(),
            pid_namespace: "pid:[4026531836]".into(),
            time_namespace: "time:[4026531834]".into(),
            pid: 4242,
            start_ticks: 987_654,
        }
    }

    /// Re-seals `bytes` after an edit: its checksum holds again.
    fn resealed(mut bytes: [u8; RECORD_LEN]) -> [u8; RECORD_LEN] {
        let digest = Sha256::digest(&bytes[..RECORD_LEN - 32]);
        bytes[RECORD_LEN - 32..].copy_from_slice(&digest);
        bytes
    }

    /// The record round-trips. Missing, short and torn (any byte flipped)
    /// contents are no record; contents whose checksum holds but that are
    /// not exactly one well-formed record are malformed, never absent:
    /// trailing bytes, another magic, bytes after a field's NUL, a field
    /// with a non-graphic byte, a zero or out-of-range pid.
    #[test]
    fn records_decode_as_absent_only_when_missing_short_or_torn() {
        let bytes = sample().encode().unwrap();
        assert_eq!(ServerRecord::decode(&bytes), Decoded::Record(sample()));
        for index in [0, 9, 100, 140, RECORD_LEN - 1] {
            let mut torn = bytes;
            torn[index] ^= 0x40;
            assert_eq!(ServerRecord::decode(&torn), Decoded::Absent, "byte {index}");
        }
        assert_eq!(
            ServerRecord::decode(&bytes[..RECORD_LEN - 1]),
            Decoded::Absent
        );
        assert_eq!(ServerRecord::decode(&[]), Decoded::Absent);
        let mut longer = bytes.to_vec();
        longer.push(0);
        assert_eq!(ServerRecord::decode(&longer), Decoded::Malformed);
        let namespace_end = 8 + FIELD + sample().pid_namespace.len();
        for (index, value) in [(0, b'X'), (namespace_end + 1, b'x'), (8, b'\t')] {
            let mut edited = bytes;
            edited[index] = value;
            assert_eq!(
                ServerRecord::decode(&resealed(edited)),
                Decoded::Malformed,
                "byte {index}"
            );
        }
        for pid in [0, u32::MAX] {
            let named = ServerRecord { pid, ..sample() };
            assert_eq!(
                ServerRecord::decode(&named.encode().unwrap()),
                Decoded::Malformed,
                "pid {pid}"
            );
        }
        // Review ochost2 #1: identity text not in the kernel's form proves
        // nothing, so another boot ID cannot be read from it.
        for (boot_id, pid_namespace) in [
            ("", "pid:[4026531836]"),
            ("garbage", "pid:[4026531836]"),
            ("0D3F5C2E-1111-2222-3333-444455556666", "pid:[4026531836]"),
            ("0d3f5c2e_1111-2222-3333-444455556666", "pid:[4026531836]"),
            ("0d3f5c2e-1111-2222-3333-44445555666", "pid:[4026531836]"),
            ("0d3f5c2e-1111-2222-3333-444455556666", ""),
            ("0d3f5c2e-1111-2222-3333-444455556666", "pid:[]"),
            ("0d3f5c2e-1111-2222-3333-444455556666", "pid:[12x]"),
            ("0d3f5c2e-1111-2222-3333-444455556666", "net:[4026531836]"),
        ] {
            let named = ServerRecord {
                boot_id: boot_id.into(),
                pid_namespace: pid_namespace.into(),
                ..sample()
            };
            assert_eq!(
                ServerRecord::decode(&named.encode().unwrap()),
                Decoded::Malformed,
                "{boot_id:?} {pid_namespace:?}"
            );
        }
        // Review ochostcrit #1: the time namespace is in the kernel's form,
        // or the one token of a kernel without time namespaces.
        let none = ServerRecord {
            time_namespace: linux::NO_TIME_NAMESPACE.into(),
            ..sample()
        };
        assert_eq!(
            ServerRecord::decode(&none.encode().unwrap()),
            Decoded::Record(none)
        );
        for time_namespace in [
            "",
            "time:[]",
            "time:[12x]",
            "time:None",
            "time:none ",
            "pid:[4026531834]",
            "time_for_children:[4026531834]",
        ] {
            let named = ServerRecord {
                time_namespace: time_namespace.into(),
                ..sample()
            };
            assert_eq!(
                ServerRecord::decode(&named.encode().unwrap()),
                Decoded::Malformed,
                "{time_namespace:?}"
            );
        }
        let long = ServerRecord {
            boot_id: "x".repeat(FIELD + 1),
            ..sample()
        };
        assert_eq!(long.encode(), None);
    }

    fn check(record: ServerRecord) -> Result<(), FenceRefusal> {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(check_predecessor(Decoded::Record(record)))
    }

    /// Review ochostcrit #1: start ticks are shifted by the reader's
    /// time-namespace offset, so a record from another time namespace on
    /// this boot and PID namespace proves nothing, even with other ticks:
    /// `PredecessorUncertain` naming that namespace. The same record from
    /// this time namespace is admitted (other ticks: another process).
    #[test]
    fn a_record_from_another_time_namespace_is_uncertain() {
        let own = std::process::id();
        let current = ServerRecord::current(own, linux::process_stat(own).unwrap().1 + 1).unwrap();
        assert_eq!(check(current.clone()), Ok(()));
        let other = if current.time_namespace == "time:[1]" {
            "time:[2]"
        } else {
            "time:[1]"
        };
        assert_eq!(
            check(ServerRecord {
                time_namespace: other.into(),
                ..current
            }),
            Err(FenceRefusal::PredecessorUncertain {
                namespace: other.into()
            })
        );
    }

    /// The writer truncates before it writes: over a longer file, only the
    /// record remains, and it decodes.
    #[test]
    fn the_writer_leaves_exactly_one_record() {
        let path = std::env::temp_dir().join(format!(
            "via-fence-record-{}-{}",
            std::process::id(),
            linux::random_hex().unwrap()
        ));
        fs::write(&path, vec![0xa5_u8; 4 * RECORD_LEN]).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let own = std::process::id();
        let ticks = linux::process_stat(own).unwrap().1;
        write_record(&file, own, ticks).unwrap();
        let bytes = read_record_bytes(&file).unwrap();
        assert_eq!(bytes.len(), RECORD_LEN);
        assert!(matches!(
            ServerRecord::decode(&bytes),
            Decoded::Record(ServerRecord { pid, start_ticks, .. }) if pid == own && start_ticks == ticks
        ));
        fs::remove_file(&path).unwrap();
    }

    /// Identity before any open: only other start ticks are gone (a pid
    /// reused as a process or a thread). A missing entry and `ESRCH` are
    /// not (review ochostcrit #2: `hidepid` hides a live same-uid
    /// non-dumpable process); a read error other than those, or an
    /// unparsable entry, fails closed.
    #[test]
    fn identity_reads_fail_closed() {
        let stat =
            |ticks: u64| format!("42 (od d) S 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {ticks} 0 0");
        assert_eq!(identity(Ok(stat(77)), 77), Identity::Matches);
        assert_eq!(identity(Ok(stat(78)), 77), Identity::Gone);
        assert_ne!(
            identity(Err(io::ErrorKind::NotFound.into()), 77),
            Identity::Gone
        );
        assert_eq!(
            identity(Err(io::ErrorKind::NotFound.into()), 77),
            Identity::Unseen
        );
        assert_eq!(
            identity(
                Err(io::Error::from_raw_os_error(Errno::SRCH.raw_os_error())),
                77
            ),
            Identity::Unseen
        );
        assert_eq!(
            identity(Err(io::ErrorKind::PermissionDenied.into()), 77),
            Identity::Unreadable
        );
        assert_eq!(identity(Ok("garbage".into()), 77), Identity::Unreadable);
    }

    /// An exit proof of this live process is never given; a free pid is
    /// (unseen, then `pidfd_open` answers `ESRCH`).
    #[test]
    fn a_live_process_is_present_and_a_free_pid_gone() {
        let own = std::process::id();
        let ticks = linux::process_stat(own).unwrap().1;
        let exited = |pid, ticks| {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap()
                .block_on(exited(pid, ticks))
        };
        assert!(!exited(own, ticks));
        assert!(exited(own, ticks + 1));
        assert!(exited(i32::MAX as u32, 1));
    }

    #[test]
    fn printable_output_replaces_other_bytes() {
        assert_eq!(printable(b"opencode v2.0.23"), "opencode v2.0.23");
        assert_eq!(printable(b"a\tb\x1b[0m\xff"), "a?b?[0m?");
    }
}
