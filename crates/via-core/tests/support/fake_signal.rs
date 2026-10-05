//! A replay fake's gate signal, sent by pid only to the launch that wrote
//! it. VIA's Host spawns and reaps the fake, so by the time a test signals
//! a launch read from the fake's launch log, that launch may have ended
//! and its pid gone to an unrelated process, whose default `SIGUSR1`
//! action would end it.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};

/// Sends `SIGUSR1` to the fake launched as `argv0` with pid `pid`, while
/// it is still that launch. A pidfd is opened first, so the signal can
/// reach only the process it opened; that process is then checked to be
/// a launch of `argv0` (its command line's first argument, which the fake
/// resolves its fixture beside). One that ended since, its pid perhaps
/// reused, is never signalled: the send fails instead.
pub(crate) fn gate(pid: u32, argv0: &Path) -> Result<(), String> {
    let raw = i32::try_from(pid).map_err(|_| format!("pid {pid} out of range"))?;
    let process = Pid::from_raw(raw).ok_or_else(|| format!("pid {pid} is no process"))?;
    let pidfd = pidfd_open(process, PidfdFlags::empty())
        .map_err(|error| format!("fake {pid} ended before its signal: {error}"))?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map_err(|error| format!("fake {pid} ended before its signal: {error}"))?;
    let first = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
    if first != argv0.as_os_str().as_bytes() {
        return Err(format!(
            "pid {pid} is no longer a launch of {}: not signalled",
            argv0.display()
        ));
    }
    pidfd_send_signal(&pidfd, Signal::USR1)
        .map_err(|error| format!("fake {pid} ended before its signal: {error}"))
}
