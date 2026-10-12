//! Linux identity and read-only group-existence evidence.

use std::{fs, io, path::Path};

use rustix::process::{self, Pid};

use crate::{CleanupEvidence, CleanupReason, GroupAbsenceProof, ProcessIdentity};

pub(crate) fn boot_id() -> io::Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

pub(crate) fn pid_namespace() -> io::Result<String> {
    Ok(fs::read_link("/proc/self/ns/pid")?
        .to_string_lossy()
        .into_owned())
}

/// The time namespace on a kernel without time namespaces, where
/// `/proc/self/ns/time` does not exist and no clock offset can apply.
pub(crate) const NO_TIME_NAMESPACE: &str = "time:none";

/// This process's time-namespace identity, `/proc/self/ns/time`
/// (`time:[<inode>]`), or [`NO_TIME_NAMESPACE`] when the kernel has none.
/// `/proc/<pid>/stat` start ticks are shifted by the reader's time-namespace
/// offset, so ticks compare only within one time namespace.
pub(crate) fn time_namespace() -> io::Result<String> {
    match fs::read_link("/proc/self/ns/time") {
        Ok(link) => Ok(link.to_string_lossy().into_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(NO_TIME_NAMESPACE.to_owned()),
        Err(error) => Err(error),
    }
}

pub(crate) fn process_stat(process_id: u32) -> io::Result<(u32, u64)> {
    let bytes = fs::read(format!("/proc/{process_id}/stat"))?;
    let stat = parse_process_stat(&bytes)?;
    Ok((stat.group_id, stat.start_ticks))
}

/// Process identity fields shared with the descriptor-bound scan (runtime §5).
pub(crate) struct ProcessStat {
    /// Kernel state used to exclude dead candidates (runtime §5).
    pub(crate) state: u8,
    /// Process group retained for Host identity checks (runtime §5).
    pub(crate) group_id: u32,
    /// Boot-relative identity and eligibility bound (runtime §5).
    pub(crate) start_ticks: u64,
}

/// Parse a previously read stat without reopening its PID (runtime §5).
pub(crate) fn parse_process_stat(bytes: &[u8]) -> io::Result<ProcessStat> {
    let start = bytes
        .windows(2)
        .rposition(|part| part == b") ")
        .ok_or_else(|| io::Error::other("bad process stat"))?
        + 2;
    let rest = std::str::from_utf8(&bytes[start..])
        .map_err(|_| io::Error::other("bad process stat fields"))?;
    let fields: Vec<_> = rest.split_ascii_whitespace().collect();
    let state = fields
        .first()
        .filter(|field| field.len() == 1)
        .map(|field| field.as_bytes()[0])
        .ok_or_else(|| io::Error::other("bad process state"))?;
    let group_id = fields
        .get(2)
        .ok_or_else(|| io::Error::other("missing group"))?
        .parse()
        .map_err(|_| io::Error::other("bad group"))?;
    let start_ticks = fields
        .get(19)
        .ok_or_else(|| io::Error::other("missing start ticks"))?
        .parse()
        .map_err(|_| io::Error::other("bad start ticks"))?;
    Ok(ProcessStat {
        state,
        group_id,
        start_ticks,
    })
}

pub(crate) fn uid_of(pid: u32) -> io::Result<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let line = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .ok_or_else(|| io::Error::other("missing uid"))?;
    line.split_ascii_whitespace()
        .nth(1)
        .ok_or_else(|| io::Error::other("missing uid value"))?
        .parse()
        .map_err(|_| io::Error::other("bad uid"))
}

/// The length of [`random_hex`]'s value: 16 random bytes in hex.
pub(crate) const RANDOM_HEX_LEN: usize = 32;

pub(crate) fn random_hex() -> io::Result<String> {
    use io::Read;
    let mut bytes = [0_u8; RANDOM_HEX_LEN / 2];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex_encode(&bytes))
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

/// A positive result is possible only from a same-boot/namespace ESRCH query.
pub(crate) fn probe_absence(identity: &ProcessIdentity, generation: &str) -> CleanupEvidence {
    if generation.is_empty()
        || identity.pid == 0
        || identity.pgid <= 1
        || identity.start_ticks == 0
        || identity.marker.as_str().is_empty()
    {
        return CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor);
    }
    let Ok(current_boot) = boot_id() else {
        return CleanupEvidence::Uncertain(CleanupReason::ProbeDenied);
    };
    let Ok(current_namespace) = pid_namespace() else {
        return CleanupEvidence::Uncertain(CleanupReason::ProbeDenied);
    };
    if identity.boot_id != current_boot || identity.pid_namespace != current_namespace {
        return CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor);
    }
    let Some(pgid) = i32::try_from(identity.pgid).ok().and_then(Pid::from_raw) else {
        return CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor);
    };
    match process::test_kill_process_group(pgid) {
        Err(rustix::io::Errno::SRCH) => CleanupEvidence::GroupAbsent(GroupAbsenceProof {
            anchor: identity.clone(),
            generation: generation.to_owned(),
            observed_at: observed_at(),
        }),
        Ok(()) => CleanupEvidence::Uncertain(CleanupReason::GroupPresent),
        Err(_) => CleanupEvidence::Uncertain(CleanupReason::ProbeDenied),
    }
}

/// The wall-clock start, in UTC, of a process that started `start_ticks`
/// clock ticks after this boot (`/proc/stat`'s `btime` plus the ticks);
/// `None` when the boot time cannot be read.
pub(crate) fn boot_ticks_utc(start_ticks: u64) -> Option<String> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let boot: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    let hz = rustix::param::clock_ticks_per_second();
    (hz > 0).then_some(())?;
    Some(utc(boot.checked_add(start_ticks / hz)?))
}

/// `seconds` after the Unix epoch as `YYYY-MM-DD HH:MM:SS UTC` (civil from
/// days, H. Hinnant).
pub(crate) fn utc(seconds: u64) -> String {
    format_utc(seconds, ' ', " UTC")
}

/// Second-precision UTC timestamp for leftover reports (runtime §5).
pub(crate) fn utc_rfc3339(seconds: u64) -> String {
    format_utc(seconds, 'T', "Z")
}

fn format_utc(seconds: u64, separator: char, suffix: &str) -> String {
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}{separator}{:02}:{:02}:{:02}{suffix}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

pub(crate) fn observed_at() -> String {
    // Evidence timestamp only. Monotonic deadlines remain in the caller.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_else(
            |_| String::from("0"),
            |duration| duration.as_nanos().to_string(),
        )
}

pub(crate) fn secure_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "anchor directory must be private",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcessMarker;

    fn identity(pgid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid: std::process::id(),
            pgid,
            uid: rustix::process::getuid().as_raw(),
            boot_id: boot_id().unwrap(),
            pid_namespace: pid_namespace().unwrap(),
            start_ticks: process_stat(std::process::id()).unwrap().1,
            marker: ProcessMarker::try_from_generated("private".into()).unwrap(),
        }
    }

    #[test]
    fn same_boot_namespace_esrch_is_positive_and_present_group_is_not() {
        let present = identity(process_stat(std::process::id()).unwrap().0);
        assert_eq!(
            probe_absence(&present, "generation"),
            CleanupEvidence::Uncertain(CleanupReason::GroupPresent)
        );
        let absent = identity(i32::MAX as u32);
        assert!(matches!(
            probe_absence(&absent, "generation"),
            CleanupEvidence::GroupAbsent(_)
        ));
    }

    #[test]
    fn mismatch_or_incomplete_identity_never_proves_absence() {
        let mut candidate = identity(i32::MAX as u32);
        candidate.boot_id.push_str("-other");
        assert_eq!(
            probe_absence(&candidate, "generation"),
            CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor)
        );
        candidate.boot_id = boot_id().unwrap();
        candidate.pid_namespace.push_str("-other");
        assert_eq!(
            probe_absence(&candidate, "generation"),
            CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor)
        );
        candidate.pid_namespace = pid_namespace().unwrap();
        candidate.start_ticks = 0;
        assert_eq!(
            probe_absence(&candidate, "generation"),
            CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor)
        );
    }
}

#[cfg(test)]
mod utc_tests {
    use super::utc;

    #[test]
    fn utc_formats_civil_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc(4_102_444_799), "2099-12-31 23:59:59 UTC");
        assert_eq!(utc(1_791_376_496), "2026-10-07 12:34:56 UTC");
    }
}
