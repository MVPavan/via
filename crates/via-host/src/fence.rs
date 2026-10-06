//! The anchor's launch fence (runtime §5 "Die with the anchor" and
//! "Exclusive launch lock"; `vendors/opencode.md` §3.2): the credential and
//! program checks, the best-effort version check, the exclusive lock, the
//! server record and the predecessor's exit proof. Everything here runs in
//! the anchor, on its main thread, before ARM, except the record write.

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

/// The server record's fixed length: magic, boot ID and PID namespace (each
/// [`FIELD`] bytes, NUL-padded), pid, start ticks and a SHA-256 of the rest.
pub(crate) const RECORD_LEN: usize = 8 + 2 * FIELD + 4 + 8 + 32;
const MAGIC: &[u8; 8] = b"VIASRV\0\x01";
const FIELD: usize = 64;
/// How long a present predecessor is waited for (runtime §5 step 4).
const PREDECESSOR_WAIT: Duration = Duration::from_secs(1);
/// The predecessor poll's interval: the pidfd is polled without blocking
/// the anchor's runtime.
const PREDECESSOR_POLL: Duration = Duration::from_millis(5);
/// The version check's bound and output cap (runtime §5 step 2).
const PROBE_BOUND: Duration = Duration::from_secs(2);
const PROBE_OUTPUT: usize = 256;

/// One server record: the process a lock holder launched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServerRecord {
    pub boot_id: String,
    pub pid_namespace: String,
    pub pid: u32,
    pub start_ticks: u64,
}

impl ServerRecord {
    /// The record naming `pid` with `start_ticks` in this boot and PID
    /// namespace.
    pub(crate) fn current(pid: u32, start_ticks: u64) -> io::Result<Self> {
        Ok(Self {
            boot_id: linux::boot_id()?,
            pid_namespace: linux::pid_namespace()?,
            pid,
            start_ticks,
        })
    }

    /// The record's bytes, or `None` when a field does not fit.
    pub(crate) fn encode(&self) -> Option<[u8; RECORD_LEN]> {
        let mut bytes = [0_u8; RECORD_LEN];
        bytes[..8].copy_from_slice(MAGIC);
        for (index, text) in [&self.boot_id, &self.pid_namespace].into_iter().enumerate() {
            let start = 8 + index * FIELD;
            bytes
                .get_mut(start..start + text.len())
                .filter(|_| text.len() <= FIELD && !text.contains('\0'))?
                .copy_from_slice(text.as_bytes());
        }
        let numbers = 8 + 2 * FIELD;
        bytes[numbers..numbers + 4].copy_from_slice(&self.pid.to_le_bytes());
        bytes[numbers + 4..numbers + 12].copy_from_slice(&self.start_ticks.to_le_bytes());
        let digest = Sha256::digest(&bytes[..RECORD_LEN - 32]);
        bytes[RECORD_LEN - 32..].copy_from_slice(&digest);
        Some(bytes)
    }

    /// The record at the start of `bytes`, or `None` when it is missing,
    /// short, torn (its checksum fails) or not a record of this format.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes = bytes.get(..RECORD_LEN)?;
        if &bytes[..8] != MAGIC
            || Sha256::digest(&bytes[..RECORD_LEN - 32]).as_slice() != &bytes[RECORD_LEN - 32..]
        {
            return None;
        }
        let text = |start: usize| {
            let field = &bytes[start..start + FIELD];
            let end = field.iter().position(|byte| *byte == 0).unwrap_or(FIELD);
            let (text, padding) = field.split_at(end);
            (padding.iter().all(|byte| *byte == 0) && text.iter().all(u8::is_ascii_graphic))
                .then(|| String::from_utf8_lossy(text).into_owned())
        };
        let numbers = 8 + 2 * FIELD;
        let pid = u32::from_le_bytes(bytes[numbers..numbers + 4].try_into().ok()?);
        let start_ticks = u64::from_le_bytes(bytes[numbers + 4..numbers + 12].try_into().ok()?);
        let positive = i32::try_from(pid).is_ok_and(|pid| pid > 0);
        positive.then_some(())?;
        Some(Self {
            boot_id: text(8)?,
            pid_namespace: text(8 + FIELD)?,
            pid,
            start_ticks,
        })
    }
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

    /// The record at the file's start, `None` when missing or torn.
    fn read_record(&self) -> io::Result<Option<ServerRecord>> {
        let mut bytes = [0_u8; RECORD_LEN];
        let mut filled = 0;
        while filled < RECORD_LEN {
            match self.file.read_at(&mut bytes[filled..], filled as u64) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(ServerRecord::decode(&bytes[..filled]))
    }

    /// Writes the record naming the child `pid` with `start_ticks`, in one
    /// fixed-size write at offset 0 through the lock's descriptor.
    pub(crate) fn write_record(&self, pid: u32, start_ticks: u64) -> io::Result<()> {
        let bytes = ServerRecord::current(pid, start_ticks)?
            .encode()
            .ok_or_else(|| io::Error::other("server record field too long"))?;
        if self.file.write_at(&bytes, 0)? != RECORD_LEN {
            return Err(io::Error::other("short server record write"));
        }
        Ok(())
    }
}

/// The checks a fenced configuration passes before it is accepted
/// (runtime §5, `Configure` steps 1 to 4), in order. The lock, when one is
/// configured, is held on success.
pub(crate) async fn configure(vendor: &VendorConfig) -> Result<Option<LaunchLock>, FenceRefusal> {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    if !unprivileged(&status) {
        return Err(FenceRefusal::PrivilegedVia);
    }
    check_program(&vendor.program())?;
    if let Some(probe) = &vendor.version_probe {
        run_probe(&vendor.program(), probe).await?;
    }
    let Some(path) = vendor.exclusive_lock() else {
        return Ok(None);
    };
    let lock = take_lock(&path)?;
    check_predecessor(&lock).await?;
    Ok(Some(lock))
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
    let mut child = crate::anchor::spawn_with_vendor_umask(&mut command).map_err(|error| {
        FenceRefusal::ProbeFailed {
            kind: ProbeFailure::Spawn {
                errno: error.raw_os_error(),
            },
        }
    })?;
    drop(command);
    let failed = |kind| FenceRefusal::ProbeFailed { kind };
    let read = async {
        let mut output = Vec::with_capacity(PROBE_OUTPUT + 1);
        if let Some(stdout) = child.stdout.take() {
            stdout
                .take(PROBE_OUTPUT as u64 + 1)
                .read_to_end(&mut output)
                .await?;
        }
        let status = child.wait().await?;
        Ok::<_, io::Error>((output, status))
    };
    let outcome = tokio::time::timeout_at(deadline, read).await;
    let (output, status) = match outcome {
        Ok(Ok(finished)) => finished,
        Ok(Err(_)) => {
            stop(&mut child).await;
            return Err(failed(ProbeFailure::Read));
        }
        Err(_) => {
            stop(&mut child).await;
            return Err(failed(ProbeFailure::Timeout));
        }
    };
    if output.len() > PROBE_OUTPUT {
        stop(&mut child).await;
        return Err(failed(ProbeFailure::Overflow));
    }
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

/// Runtime §5 step 4: the recorded server is proved gone, or refused.
async fn check_predecessor(lock: &LaunchLock) -> Result<(), FenceRefusal> {
    let unreadable = |error: io::Error| FenceRefusal::RecordUnreadable {
        errno: error.raw_os_error(),
    };
    let Some(record) = lock.read_record().map_err(unreadable)? else {
        return Ok(());
    };
    if record.boot_id != linux::boot_id().map_err(unreadable)? {
        return Ok(());
    }
    if record.pid_namespace != linux::pid_namespace().map_err(unreadable)? {
        return Err(FenceRefusal::PredecessorUncertain {
            namespace: record.pid_namespace,
        });
    }
    let deadline = Instant::now() + PREDECESSOR_WAIT;
    loop {
        if exited(record.pid, record.start_ticks) {
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
    /// No such entry, or other start ticks: the pid is free or reused.
    Gone,
    /// The recorded start ticks.
    Matches,
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
            Identity::Gone
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
/// that is readable only once the whole thread group has exited. Anything
/// else is present.
fn exited(pid: u32, ticks: u64) -> bool {
    match read_identity(pid, ticks) {
        Identity::Gone => return true,
        Identity::Unreadable => return false,
        Identity::Matches => {}
    }
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
        Identity::Matches => {}
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
            pid: 4242,
            start_ticks: 987_654,
        }
    }

    /// The record round-trips; a torn byte anywhere, a short read, another
    /// magic and a zero pid decode as no record.
    #[test]
    fn record_round_trips_and_torn_records_decode_as_none() {
        let bytes = sample().encode().unwrap();
        assert_eq!(ServerRecord::decode(&bytes), Some(sample()));
        let mut longer = bytes.to_vec();
        longer.extend_from_slice(b"trailing");
        assert_eq!(ServerRecord::decode(&longer), Some(sample()));
        for index in [0, 9, 100, 140, RECORD_LEN - 1] {
            let mut torn = bytes;
            torn[index] ^= 0x40;
            assert_eq!(ServerRecord::decode(&torn), None, "byte {index}");
        }
        assert_eq!(ServerRecord::decode(&bytes[..RECORD_LEN - 1]), None);
        assert_eq!(ServerRecord::decode(&[]), None);
        let zero = ServerRecord { pid: 0, ..sample() };
        assert_eq!(ServerRecord::decode(&zero.encode().unwrap()), None);
        let long = ServerRecord {
            boot_id: "x".repeat(FIELD + 1),
            ..sample()
        };
        assert_eq!(long.encode(), None);
    }

    /// Identity before any open: a missing entry, `ESRCH` and other start
    /// ticks are gone (a pid reused as a process or a thread); a read
    /// error other than those, or an unparsable entry, fails closed.
    #[test]
    fn identity_reads_fail_closed() {
        let stat =
            |ticks: u64| format!("42 (od d) S 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {ticks} 0 0");
        assert_eq!(identity(Ok(stat(77)), 77), Identity::Matches);
        assert_eq!(identity(Ok(stat(78)), 77), Identity::Gone);
        assert_eq!(
            identity(Err(io::ErrorKind::NotFound.into()), 77),
            Identity::Gone
        );
        assert_eq!(
            identity(
                Err(io::Error::from_raw_os_error(Errno::SRCH.raw_os_error())),
                77
            ),
            Identity::Gone
        );
        assert_eq!(
            identity(Err(io::ErrorKind::PermissionDenied.into()), 77),
            Identity::Unreadable
        );
        assert_eq!(identity(Ok("garbage".into()), 77), Identity::Unreadable);
    }

    /// An exit proof of this live process is never given; a free pid is.
    #[test]
    fn a_live_process_is_present_and_a_free_pid_gone() {
        let own = std::process::id();
        let ticks = linux::process_stat(own).unwrap().1;
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
