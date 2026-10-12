//! Report-only marker scan (runtime §5). Results never grant signalling authority.

use rustix::fs::{CWD, Mode, OFlags, openat};
use std::os::fd::AsFd;
use std::{
    fs::{self, File},
    io::{self, Read},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::time::Instant;

/// Destination of an explicitly requested leftover report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeftoverScope {
    /// A per-turn process stopped.
    Turn,
    /// A shared server stopped with active turns.
    Server,
}

/// A process observed carrying the exact launch marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeftoverProcess {
    /// Process identifier; passive evidence only.
    pub pid: u32,
    /// Kernel name, limited to 15 bytes and decoded lossily.
    pub comm: String,
    /// RFC 3339 UTC boot time plus start ticks, second precision.
    pub started_at: String,
}

/// Bounded best-effort snapshot, never ownership or liveness evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeftoverReport {
    /// Existing surface receiving the snapshot.
    pub scope: LeftoverScope,
    /// Oldest 16 matches by start ticks, then pid.
    pub processes: Vec<LeftoverProcess>,
    /// Matches observed, a lower bound when incomplete.
    pub total: u32,
    /// Eligibility or complete enumeration could not be settled.
    pub incomplete: bool,
}

impl LeftoverReport {
    pub(crate) const fn incomplete(scope: LeftoverScope) -> Self {
        Self {
            scope,
            processes: Vec::new(),
            total: 0,
            incomplete: true,
        }
    }
}

// Runtime §5 mandates the environment cap. Metadata caps keep the same
// best-effort scan bounded even on large procfs tables; overflow is incomplete.
const ENV_MAX: usize = 256 * 1024;
const BOOT_STAT_MAX: usize = 1024 * 1024;
const MOUNTS_MAX: usize = 1024 * 1024;
const PROCESS_STAT_MAX: usize = 8192;
const PROCESS_STATUS_MAX: usize = 64 * 1024;
const PROCESS_COMM_MAX: usize = 4096;

/// Shared progress retains only public report data. The task belongs to Host,
/// including after a caller deadline; cancellation stops further procfs reads.
pub(crate) struct ScanState {
    pub(crate) report: Mutex<LeftoverReport>,
    pub(crate) cancelled: AtomicBool,
}

impl ScanState {
    pub(crate) fn new(scope: LeftoverScope) -> Arc<Self> {
        Arc::new(Self {
            report: Mutex::new(LeftoverReport::incomplete(scope)),
            cancelled: AtomicBool::new(false),
        })
    }
    pub(crate) fn snapshot(&self) -> LeftoverReport {
        self.report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Cancelling the caller also cancels the scan's subsequent reads, while Host
/// retains and collects the blocking task. No environment escapes the scanner.
pub(crate) struct CancelScan(pub(crate) Arc<ScanState>);
impl Drop for CancelScan {
    fn drop(&mut self) {
        self.0.cancelled.store(true, Ordering::Release);
    }
}

pub(crate) fn scan(
    root: &Path,
    marker: &[u8],
    bound: Option<u64>,
    scope: LeftoverScope,
    deadline: Instant,
    state: &ScanState,
) -> LeftoverReport {
    let mut report = LeftoverReport::incomplete(scope);
    let Some(bound) = bound else { return report };
    if expired(state, deadline) {
        return report;
    }
    let Ok(root_dir) = openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) else {
        return report;
    };
    let Some(boot) = read_boot(&root_dir, state, deadline) else {
        return report;
    };
    let hz = rustix::param::clock_ticks_per_second();
    if hz == 0 {
        return report;
    }
    if expired(state, deadline) {
        return report;
    }
    report.incomplete = mounts_incomplete(&root_dir, state, deadline);
    if expired(state, deadline) {
        report.incomplete = true;
        return report;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return LeftoverReport::incomplete(scope);
    };
    let mut retained: Vec<(u64, LeftoverProcess)> = Vec::with_capacity(17);
    let exact = [b"VIA_PROCESS_MARKER=".as_slice(), marker].concat();
    let uid = rustix::process::getuid().as_raw();
    for entry in entries {
        if expired(state, deadline) {
            report.incomplete = true;
            break;
        }
        let Ok(entry) = entry else {
            report.incomplete = true;
            continue;
        };
        // Non-numeric procfs entries are not process candidates.
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let candidate = candidate(
            &entry.path(),
            pid,
            uid,
            bound,
            &exact,
            boot,
            hz,
            state,
            deadline,
        );
        match candidate {
            Ok(Some((ticks, process))) => {
                report.total = report.total.saturating_add(1);
                retained.push((ticks, process));
                retained.sort_unstable_by_key(|(ticks, process)| (*ticks, process.pid));
                retained.truncate(16);
                report.processes = retained
                    .iter()
                    .map(|(_, process)| process.clone())
                    .collect();
            }
            Ok(None) => {}
            Err(error) if disappeared(&error) => {}
            Err(_) => report.incomplete = true,
        }
        // A caller cut off at the deadline receives only this public prefix.
        let mut snapshot = report.clone();
        snapshot.incomplete = true;
        *state
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
    }
    if expired(state, deadline) {
        report.incomplete = true;
    }
    report
}

fn read_boot(dir: &impl AsFd, state: &ScanState, deadline: Instant) -> Option<u64> {
    // Failed, oversized or malformed boot metadata cannot authorize candidate
    // reads; the discarded errors leave the initial incomplete report intact.
    read_at(dir, "stat", BOOT_STAT_MAX, state, deadline)
        .ok()
        .flatten()
        .and_then(|bytes| {
            let text = std::str::from_utf8(&bytes).ok()?;
            text.lines()
                .find_map(|line| line.strip_prefix("btime "))?
                .trim()
                .parse::<u64>()
                .ok()
        })
}

fn mounts_incomplete(dir: &impl AsFd, state: &ScanState, deadline: Instant) -> bool {
    // Both tables are procfs metadata, never environments or command lines.
    match read_at(dir, "mounts", MOUNTS_MAX, state, deadline) {
        Ok(Some(bytes)) => match std::str::from_utf8(&bytes) {
            Ok(mounts) => mounts.lines().any(|line| {
                let fields: Vec<_> = line.split_ascii_whitespace().collect();
                fields.get(1) == Some(&"/proc")
                    && fields.get(3).is_some_and(|options| {
                        options
                            .split(',')
                            .any(|option| matches!(option, "hidepid=4" | "hidepid=ptraceable"))
                    })
            }),
            Err(_) => true,
        },
        Ok(None) | Err(_) => true,
    }
}

fn expired(state: &ScanState, deadline: Instant) -> bool {
    state.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline
}

fn disappeared(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(2 | 3))
}

/// All candidate reads are bound to this directory descriptor, never a
/// reopened numeric PID. Eligible environments are transient and not logged.
#[expect(
    clippy::too_many_arguments,
    reason = "one scanner candidate carries its bounded eligibility context"
)]
fn candidate(
    path: &Path,
    pid: u32,
    uid: u32,
    bound: u64,
    exact: &[u8],
    boot: u64,
    hz: u64,
    state: &ScanState,
    deadline: Instant,
) -> io::Result<Option<(u64, LeftoverProcess)>> {
    check_deadline(state, deadline)?;
    let dir = openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let Some(stat) = read_at(&dir, "stat", PROCESS_STAT_MAX, state, deadline)? else {
        return Ok(None);
    };
    let stat = crate::linux::parse_process_stat(&stat)?;
    let ticks = stat.start_ticks;
    if ticks < bound || matches!(stat.state, b'Z' | b'X' | b'x') {
        return Ok(None);
    }
    let Some(status) = read_at(&dir, "status", PROCESS_STATUS_MAX, state, deadline)? else {
        return Ok(None);
    };
    if parse_uid(&status)? != uid {
        return Ok(None);
    }
    let Some(env) = read_at(&dir, "environ", ENV_MAX, state, deadline)? else {
        return Ok(None);
    };
    let matches = env.split(|byte| *byte == 0).any(|entry| entry == exact);
    drop(env);
    if !matches {
        return Ok(None);
    }
    let Some(comm) = read_at(&dir, "comm", PROCESS_COMM_MAX, state, deadline)? else {
        return Ok(None);
    };
    let Some(stat) = read_at(&dir, "stat", PROCESS_STAT_MAX, state, deadline)? else {
        return Ok(None);
    };
    let final_stat = crate::linux::parse_process_stat(&stat)?;
    let Some(status) = read_at(&dir, "status", PROCESS_STATUS_MAX, state, deadline)? else {
        return Ok(None);
    };
    if ticks != final_stat.start_ticks
        || parse_uid(&status)? != uid
        || matches!(final_stat.state, b'Z' | b'X' | b'x')
    {
        return Ok(None);
    }
    let comm = comm.strip_suffix(b"\n").unwrap_or(&comm);
    let comm = String::from_utf8_lossy(&comm[..comm.len().min(15)]).into_owned();
    let seconds = boot
        .checked_add(ticks / hz)
        .ok_or_else(|| io::Error::other("start time overflow"))?;
    let started_at = crate::linux::utc_rfc3339(seconds);
    Ok(Some((
        ticks,
        LeftoverProcess {
            pid,
            comm,
            started_at,
        },
    )))
}

fn check_deadline(state: &ScanState, deadline: Instant) -> io::Result<()> {
    if expired(state, deadline) {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "leftover scan deadline",
        ))
    } else {
        Ok(())
    }
}

fn read_at(
    dir: &impl AsFd,
    name: &str,
    cap: usize,
    state: &ScanState,
    deadline: Instant,
) -> io::Result<Option<Vec<u8>>> {
    check_deadline(state, deadline)?;
    let fd = openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let mut file = File::from(fd);
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        check_deadline(state, deadline)?;
        let limit = chunk.len().min(cap + 1 - bytes.len());
        let count = file.read(&mut chunk[..limit])?;
        check_deadline(state, deadline)?;
        if count == 0 {
            return Ok((!bytes.is_empty()).then_some(bytes));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > cap {
            return Err(io::Error::other("leftover scan file exceeds cap"));
        }
    }
}

fn parse_uid(bytes: &[u8]) -> io::Result<u32> {
    bytes
        .split(|byte| *byte == b'\n')
        .find_map(|line| {
            let line = line.strip_prefix(b"Uid:")?;
            let word = line
                .split(u8::is_ascii_whitespace)
                .find(|word| !word.is_empty())?;
            // Invalid uid fields yield the error below, never eligibility.
            std::str::from_utf8(word).ok()?.parse().ok()
        })
        .ok_or_else(|| io::Error::other("missing real uid"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, time::Duration};

    struct ProcFixture(PathBuf);
    impl ProcFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "via-leftovers-{}-{}",
                std::process::id(),
                crate::linux::random_hex().unwrap()
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("stat"), "btime 1700000000\n").unwrap();
            fs::write(root.join("mounts"), "proc /proc proc rw,hidepid=0 0 0\n").unwrap();
            Self(root)
        }
        fn entry(&self, pid: u32, ticks: u64, uid: u32, state: char, env: &[u8]) {
            let dir = self.0.join(pid.to_string());
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("stat"), format!("{pid} (name with ) spaces) {state} 1 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 {ticks}\n")).unwrap();
            fs::write(
                dir.join("status"),
                format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
            )
            .unwrap();
            fs::write(dir.join("environ"), env).unwrap();
            fs::write(dir.join("comm"), "fixture-worker\n").unwrap();
        }
        fn report(&self, bound: Option<u64>) -> LeftoverReport {
            scan(
                &self.0,
                b"match",
                bound,
                LeftoverScope::Server,
                Instant::now() + Duration::from_secs(1),
                &ScanState::new(LeftoverScope::Server),
            )
        }
    }
    impl Drop for ProcFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn leftover_scan_matches_only_eligible_exact_entries() {
        let proc = ProcFixture::new();
        let uid = rustix::process::getuid().as_raw();
        proc.entry(41, 100, uid, 'S', b"A=secret\0VIA_PROCESS_MARKER=match\0");
        proc.entry(42, 99, uid, 'S', b"VIA_PROCESS_MARKER=match\0");
        proc.entry(43, 101, uid, 'S', b"VIA_PROCESS_MARKER=matching\0");
        proc.entry(44, 101, uid, 'Z', b"VIA_PROCESS_MARKER=match\0");
        proc.entry(45, 101, uid + 1, 'S', b"VIA_PROCESS_MARKER=match\0");
        let report = proc.report(Some(100));
        assert_eq!(report.total, 1);
        assert_eq!(report.processes[0].pid, 41);
        assert_eq!(report.processes[0].comm, "fixture-worker");
        assert!(report.processes[0].started_at.ends_with('Z'));
        assert!(!report.incomplete);
        assert!(!format!("{report:?}").contains("secret"));
    }

    #[test]
    fn leftover_scan_caps_environments_and_sorts_the_oldest_sixteen() {
        let proc = ProcFixture::new();
        let uid = rustix::process::getuid().as_raw();
        for pid in 100..119 {
            proc.entry(
                pid,
                u64::from(200 - pid),
                uid,
                'S',
                b"VIA_PROCESS_MARKER=match\0",
            );
        }
        let mut oversized = b"VIA_PROCESS_MARKER=match\0".to_vec();
        oversized.resize(ENV_MAX + 1, b'x');
        proc.entry(120, 100, uid, 'S', &oversized);
        let report = proc.report(Some(80));
        assert_eq!(report.total, 19);
        assert_eq!(report.processes.len(), 16);
        assert_eq!(report.processes[0].pid, 118);
        assert_eq!(report.processes[15].pid, 103);
        assert!(report.incomplete);
    }

    #[test]
    fn leftover_scan_marks_hidden_or_malformed_sets_incomplete() {
        let proc = ProcFixture::new();
        let uid = rustix::process::getuid().as_raw();
        proc.entry(41, 100, uid, 'S', b"VIA_PROCESS_MARKER=match\0");
        for hidden in ["4", "ptraceable"] {
            fs::write(
                proc.0.join("mounts"),
                format!("proc /proc proc rw,hidepid={hidden} 0 0\n"),
            )
            .unwrap();
            let report = proc.report(Some(100));
            assert_eq!(report.total, 1);
            assert!(report.incomplete);
        }
        fs::write(proc.0.join("mounts"), "proc /proc proc rw 0 0\n").unwrap();
        fs::write(proc.0.join("41/stat"), "malformed").unwrap();
        let report = proc.report(Some(100));
        assert_eq!(report.total, 0);
        assert!(report.incomplete);
    }

    #[test]
    fn leftover_scan_drops_disappeared_entries_without_failure() {
        let proc = ProcFixture::new();
        proc.entry(
            41,
            100,
            rustix::process::getuid().as_raw(),
            'S',
            b"VIA_PROCESS_MARKER=match\0",
        );
        fs::remove_file(proc.0.join("41/status")).unwrap();
        let report = proc.report(Some(100));
        assert_eq!(report.total, 0);
        assert!(!report.incomplete);
    }

    #[test]
    fn leftover_scan_accepts_environment_at_the_cap_only_after_eof() {
        let proc = ProcFixture::new();
        let mut env = b"VIA_PROCESS_MARKER=match\0OTHER=".to_vec();
        env.resize(ENV_MAX, b'x');
        proc.entry(41, 0, rustix::process::getuid().as_raw(), 'S', &env);
        let report = proc.report(Some(0));
        assert_eq!(report.total, 1);
        assert_eq!(report.processes[0].started_at, "2023-11-14T22:13:20Z");
        assert!(!report.incomplete);
    }

    #[test]
    fn leftover_reads_stay_bound_to_the_open_process_directory() {
        let proc = ProcFixture::new();
        proc.entry(
            41,
            100,
            rustix::process::getuid().as_raw(),
            'S',
            b"VIA_PROCESS_MARKER=match\0",
        );
        let dir = openat(
            CWD,
            proc.0.join("41"),
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .unwrap();
        fs::rename(proc.0.join("41"), proc.0.join("old")).unwrap();
        proc.entry(
            41,
            200,
            rustix::process::getuid().as_raw(),
            'S',
            b"VIA_PROCESS_MARKER=unrelated\0",
        );
        let state = ScanState::new(LeftoverScope::Server);
        let bytes = read_at(
            &dir,
            "environ",
            ENV_MAX,
            &state,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap()
        .unwrap();
        assert_eq!(bytes, b"VIA_PROCESS_MARKER=match\0");
        let bytes = read_at(
            &dir,
            "stat",
            PROCESS_STAT_MAX,
            &state,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            crate::linux::parse_process_stat(&bytes)
                .unwrap()
                .start_ticks,
            100
        );
    }

    #[test]
    fn leftover_scan_without_bound_or_budget_reads_nothing() {
        let proc = ProcFixture::new();
        assert_eq!(
            proc.report(None),
            LeftoverReport::incomplete(LeftoverScope::Server)
        );
        let expired = scan(
            &proc.0,
            b"match",
            Some(0),
            LeftoverScope::Turn,
            Instant::now(),
            &ScanState::new(LeftoverScope::Turn),
        );
        assert_eq!(expired, LeftoverReport::incomplete(LeftoverScope::Turn));
    }

    #[test]
    fn leftover_scan_rejects_oversized_boot_metadata() {
        let proc = ProcFixture::new();
        proc.entry(
            41,
            100,
            rustix::process::getuid().as_raw(),
            'S',
            b"VIA_PROCESS_MARKER=match\0",
        );
        let mut stat = "btime 1700000000\n".to_owned();
        stat.extend(std::iter::repeat_n(' ', 2 * 1024 * 1024));
        fs::write(proc.0.join("stat"), stat).unwrap();
        let report = proc.report(Some(100));
        assert_eq!(report.total, 0);
        assert!(report.incomplete);
    }

    #[test]
    fn leftover_scan_rejects_oversized_mount_metadata() {
        let proc = ProcFixture::new();
        proc.entry(
            41,
            100,
            rustix::process::getuid().as_raw(),
            'S',
            b"VIA_PROCESS_MARKER=match\0",
        );
        let mut mounts = "proc /proc proc rw,hidepid=0 0 0\n".to_owned();
        mounts.extend(std::iter::repeat_n(' ', 2 * 1024 * 1024));
        fs::write(proc.0.join("mounts"), mounts).unwrap();
        let report = proc.report(Some(100));
        assert_eq!(report.total, 1);
        assert!(report.incomplete);
    }
}
