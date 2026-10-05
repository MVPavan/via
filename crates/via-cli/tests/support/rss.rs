//! The daemon's memory gate (Task 4 design §5.1, runtime §8): the sum over
//! holders for a harness-process count, and a 10 ms sampler of the daemon's
//! RSS, the fakes' written bytes and the anchors' peak RSS from `/proc`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Design §5.1's sum over holders in KiB for `slots` harness-process slots:
/// 210.3 MiB that do not scale (32 C1 sockets at 6 MiB, the Store at
/// 18.3 MiB) and 30.4 MiB per slot (a Wire connection at 6.3 MiB and a
/// running turn at 24.1 MiB).
pub(crate) fn sum_kib(slots: u64) -> u64 {
    (2_103 + 304 * slots) * 1024 / 10
}

/// The gate: peak RSS less the idle baseline stays within 1.25 × the sum.
pub(crate) fn limit_kib(slots: u64) -> u64 {
    sum_kib(slots) * 5 / 4
}

/// glibc's malloc arenas for the daemon (runtime §8): with the default
/// per-thread arenas, growth after 64 MiB failed about half of measured
/// runs, consistent with allocator retention; two arenas are an empirical
/// development proxy. Unset on musl, whose run is the authoritative memory
/// gate.
pub(crate) const GLIBC_ARENAS: Option<&str> = if cfg!(target_env = "gnu") {
    Some("2")
} else {
    None
};

/// A `/proc/<pid>/status` field in KiB.
pub(crate) fn status_kib(pid: u32, field: &str) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
}

/// Bytes a process wrote (`/proc/<pid>/io` `wchar`).
fn written(pid: u32) -> Option<u64> {
    let io = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    io.lines()
        .find_map(|line| line.strip_prefix("wchar:"))
        .and_then(|rest| rest.trim().parse().ok())
}

/// The pids whose executable is `exe`.
pub(crate) fn processes_of(exe: &Path) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|target| target == exe))
        .collect()
}

/// Whether `pid` is a `via` anchor process (`via __via_host_anchor …`).
pub(crate) fn is_anchor(pid: u32) -> bool {
    fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
        cmdline
            .split(|byte| *byte == 0)
            .nth(1)
            .is_some_and(|arg| arg == b"__via_host_anchor")
    })
}

/// One 10 ms sample: the daemon's RSS and the bytes the fakes had written.
#[derive(Clone, Copy)]
pub(crate) struct Sample {
    pub(crate) rss_kib: u64,
    pub(crate) flooded: u64,
}

/// What the sampler saw.
#[derive(Default)]
pub(crate) struct Samples {
    pub(crate) daemon: Vec<Sample>,
    /// Peak RSS (`VmHWM`) per anchor pid.
    pub(crate) anchors: HashMap<u32, u64>,
}

/// Samples the daemon's RSS every 10 ms, and the fakes' written bytes and
/// the anchors' peak RSS, until `stop`.
pub(crate) fn sampler(
    daemon: u32,
    fake: PathBuf,
    via: PathBuf,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<Samples> {
    thread::spawn(move || {
        let mut samples = Samples::default();
        let mut fakes: HashMap<u32, u64> = HashMap::new();
        let mut anchors: Vec<u32> = Vec::new();
        let mut refreshed: Option<Instant> = None;
        while !stop.load(Ordering::Acquire) {
            if refreshed.is_none_or(|at| at.elapsed() >= Duration::from_millis(200)) {
                refreshed = Some(Instant::now());
                for pid in processes_of(&fake) {
                    fakes.entry(pid).or_insert(0);
                }
                anchors = processes_of(&via)
                    .into_iter()
                    .filter(|pid| *pid != daemon && is_anchor(*pid))
                    .collect();
            }
            for (pid, bytes) in &mut fakes {
                if let Some(now) = written(*pid) {
                    *bytes = (*bytes).max(now);
                }
            }
            for pid in &anchors {
                if let Some(peak) = status_kib(*pid, "VmHWM:") {
                    let seen = samples.anchors.entry(*pid).or_insert(0);
                    *seen = (*seen).max(peak);
                }
            }
            if let Some(rss_kib) = status_kib(daemon, "VmRSS:") {
                samples.daemon.push(Sample {
                    rss_kib,
                    flooded: fakes.values().sum(),
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
        samples
    })
}
