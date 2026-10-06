//! The same binary's internal exec entry (runtime §5 "Die with the
//! anchor"): `/proc/self/exe __via_host_exec <anchor pid> <record path or
//! -> <program> <args…>`, started by the anchor with the child's cwd,
//! environment, umask and standard streams in place. It sets its
//! parent-death signal to `SIGKILL`, checks that its parent is the anchor,
//! waits for the server record naming it when it has a record path, then
//! replaces itself with the program. It uses only safe rustix and std
//! calls and adopts no inherited descriptor.

use std::{
    ffi::OsString,
    io,
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal};

use crate::{
    fence::{self, Decoded, ServerRecord},
    linux,
};

/// A failure before the program starts (steps 1 to 3).
const SETUP_FAILED: i32 = 125;
/// The program could not be executed.
const EXEC_FAILED: i32 = 126;
/// The program was not found.
const NOT_FOUND: i32 = 127;
/// How long the child waits for its record.
const RECORD_BOUND: Duration = Duration::from_secs(5);
/// The record poll's interval.
const RECORD_POLL: Duration = Duration::from_millis(2);

/// Runs the internal exec entry from its arguments after
/// `__via_host_exec`. It returns only on failure: 125 before the program
/// starts, 126 when it cannot be executed, 127 when it is not found.
pub fn run_exec_from_args(args: &[OsString]) -> i32 {
    let started = Instant::now();
    #[cfg(feature = "test-failpoints")]
    let args = match args {
        [flag, dir, token, rest @ ..] if flag == "--failpoints" => {
            if via_store::failpoint::activate(Path::new(dir), &token.to_string_lossy()).is_err() {
                return SETUP_FAILED;
            }
            rest
        }
        _ => args,
    };
    let [anchor, record, program, program_args @ ..] = args else {
        return SETUP_FAILED;
    };
    let Some(anchor) = anchor
        .to_str()
        .and_then(|anchor| anchor.parse::<i32>().ok())
        .and_then(Pid::from_raw)
    else {
        return SETUP_FAILED;
    };
    if seam("host.exec.before_death_signal").is_err() {
        return SETUP_FAILED;
    }
    // Step 1: the signal is set and read back before anything else.
    if rustix::process::set_parent_process_death_signal(Some(Signal::KILL)).is_err()
        || !matches!(
            rustix::process::parent_process_death_signal(),
            Ok(Some(Signal::KILL))
        )
    {
        return SETUP_FAILED;
    }
    // Step 2: an anchor that died first left another parent.
    let parent_is_anchor = || rustix::process::getppid() == Some(anchor);
    if !parent_is_anchor() || seam("host.exec.after_parent_check").is_err() {
        return SETUP_FAILED;
    }
    // Step 3: under the exclusive lock, only a record naming this process.
    if record != "-" {
        let named = own_record().and_then(|own| {
            wait_for_record(
                Path::new(record),
                &own,
                parent_is_anchor,
                started + RECORD_BOUND,
            )
        });
        if named.is_err() {
            return SETUP_FAILED;
        }
    }
    // Step 4: the program keeps this pid, start ticks, group and signal.
    let error = Command::new(program).args(program_args).exec();
    if error.kind() == io::ErrorKind::NotFound {
        NOT_FOUND
    } else {
        EXEC_FAILED
    }
}

/// The record that would name this process.
fn own_record() -> io::Result<ServerRecord> {
    let pid = std::process::id();
    ServerRecord::current(pid, linux::process_stat(pid)?.1)
}

/// Reads `path` by path about every 2 ms (read-only, no lock, no symlink
/// followed, a fresh open and close each time), re-checking the parent
/// each time, until it holds exactly one valid record equal to `own`. A
/// changed parent, a read error or reaching `bound` is a failure, checked
/// before a record is accepted: a child suspended past its bound never
/// executes, whatever record it then finds.
fn wait_for_record(
    path: &Path,
    own: &ServerRecord,
    parent_is_anchor: impl Fn() -> bool,
    bound: Instant,
) -> io::Result<()> {
    loop {
        let bytes = read_record_bytes(path)?;
        if !parent_is_anchor() {
            return Err(io::Error::other("parent changed"));
        }
        if Instant::now() >= bound {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        if matches!(ServerRecord::decode(&bytes), Decoded::Record(record) if record == *own) {
            return Ok(());
        }
        std::thread::sleep(RECORD_POLL);
    }
}

fn read_record_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)?;
    fence::read_record_bytes(&file)
}

/// Test builds: the named seam; a `fail_io` fails the setup.
#[cfg(feature = "test-failpoints")]
pub(crate) fn seam(point: &'static str) -> io::Result<()> {
    via_store::failpoint::hit(point)
}

/// Release builds have no seam.
#[cfg(not(feature = "test-failpoints"))]
#[expect(clippy::unnecessary_wraps, reason = "test builds can fail here")]
pub(crate) fn seam(_point: &'static str) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "via-exec-entry-{}-{}",
            std::process::id(),
            linux::random_hex().unwrap()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    /// Step 3: a record naming this process releases the child; another
    /// pid or other start ticks never do, until the bound; a changed
    /// parent and a missing file fail at once.
    #[test]
    fn only_a_record_naming_this_process_releases_it() {
        let folder = folder();
        let path = folder.join("server.lock");
        let own = own_record().unwrap();
        std::fs::write(&path, own.encode().unwrap()).unwrap();
        let soon = Instant::now() + Duration::from_secs(1);
        assert!(wait_for_record(&path, &own, || true, soon).is_ok());
        // Past the bound (a child suspended while it polled), even the
        // record naming it never releases it.
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        let error = wait_for_record(&path, &own, || true, past).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        for other in [
            ServerRecord {
                pid: own.pid + 1,
                ..own.clone()
            },
            ServerRecord {
                start_ticks: own.start_ticks + 1,
                ..own.clone()
            },
        ] {
            std::fs::write(&path, other.encode().unwrap()).unwrap();
            let started = Instant::now();
            let bound = started + Duration::from_millis(100);
            let error = wait_for_record(&path, &own, || true, bound).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(started.elapsed() >= Duration::from_millis(100));
        }

        let far = Instant::now() + Duration::from_secs(60);
        let started = Instant::now();
        assert!(wait_for_record(&path, &own, || false, far).is_err());
        assert!(started.elapsed() < Duration::from_secs(1), "changed parent");
        let missing = folder.join("missing");
        assert!(wait_for_record(&missing, &own, || true, far).is_err());
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
