//! A fallible observation of a process's existence: only a genuinely
//! vanished process, or a zombie awaiting its reaper, has exited. An
//! unreadable or malformed `/proc/<pid>/stat` is uncertainty, never
//! absence (S1-evidence2 fix round 2, finding 13).

use std::fs;

/// Whether process `pid` has exited: `Ok(true)` only when `/proc/<pid>/stat`
/// is gone (`NotFound` or `ESRCH`) or shows state `Z` or `X`; `Ok(false)`
/// when it shows any other state; `Err` when it cannot be read or parsed.
pub(crate) fn exited(pid: u32) -> Result<bool, String> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            return Ok(true);
        }
        Err(error) => return Err(format!("process {pid}: /proc stat unreadable: {error}")),
    };
    let state = stat
        .rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next())
        .ok_or_else(|| format!("process {pid}: malformed /proc stat"))?;
    Ok(matches!(state, 'Z' | 'X'))
}
