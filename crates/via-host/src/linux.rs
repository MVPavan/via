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

pub(crate) fn process_stat(process_id: u32) -> io::Result<(u32, u64)> {
    let text = fs::read_to_string(format!("/proc/{process_id}/stat"))?;
    let rest = text
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("bad process stat"))?
        .1;
    let fields: Vec<_> = rest.split_ascii_whitespace().collect();
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
    Ok((group_id, start_ticks))
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

pub(crate) fn random_hex() -> io::Result<String> {
    use io::Read;
    let mut bytes = [0_u8; 16];
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
