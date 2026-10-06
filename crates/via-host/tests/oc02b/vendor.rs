//! The fake vendor of the `OC02b` fixtures, run as `<test binary>
//! __oc02b_vendor <mode> …`. It reports its pid, parent, group, open
//! descriptors and whether any of them holds a lock (`fdinfo`), and plays
//! the version check's outcomes. It never reads vendor credentials; it is
//! a test stand-in.

use std::{
    ffi::OsString,
    fs,
    io::Write,
    os::unix::process::CommandExt,
    process::{Command, ExitCode, Stdio},
    time::Duration,
};

/// Runs the fake vendor `mode`.
pub(crate) fn run(args: &[OsString]) -> ExitCode {
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["version", text] => {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{text}");
            let _ = out.flush();
            ExitCode::SUCCESS
        }
        ["version-flood"] => {
            let mut out = std::io::stdout();
            let _ = out.write_all(&[b'x'; 300]);
            let _ = out.flush();
            ExitCode::SUCCESS
        }
        ["version-exit", code] => ExitCode::from(code.parse::<u8>().unwrap_or(1)),
        ["version-hang", pid_file] => {
            write_atomic(pid_file, std::process::id().to_string().as_bytes());
            sleep_forever()
        }
        ["report", path] => {
            report(path, None);
            sleep_forever()
        }
        ["report-child", path, child_path] => {
            // A leftover in its own group, as a tool shell would be.
            let child = Command::new(std::env::current_exe().unwrap_or_default())
                .args(["__oc02b_vendor", "report", child_path])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn();
            report(path, child.ok().map(|child| child.id()));
            sleep_forever()
        }
        ["escape", path] => {
            // Out of the threat model (vendors/opencode.md §3.2): used only to
            // keep a predecessor alive past its anchor.
            let cleared = rustix::process::set_parent_process_death_signal(None).is_ok();
            let left = rustix::process::setsid().is_ok();
            if cleared && left {
                report(path, None);
            }
            sleep_forever()
        }
        ["own-group", path] => {
            // Leaves the anchor's group but keeps its parent-death signal,
            // so Host's group reaper never reaps it (an unreaped zombie
            // under a test subreaper).
            if rustix::process::setsid().is_ok() {
                report(path, None);
            }
            sleep_forever()
        }
        ["thread-exec", path] => {
            // Re-executes itself from a non-leader thread, which never had
            // the parent-death signal (runtime §5 limits, L14).
            let path = (*path).to_owned();
            let worker = std::thread::spawn(move || {
                Command::new(std::env::current_exe().unwrap_or_default())
                    .args(["__oc02b_vendor", "report", &path])
                    .exec()
            });
            let _ = worker.join();
            ExitCode::from(126)
        }
        ["stderr-secret", secret] => {
            let mut err = std::io::stderr();
            let _ = writeln!(err, "{secret}");
            let _ = err.flush();
            ExitCode::SUCCESS
        }
        _ => ExitCode::from(2),
    }
}

fn sleep_forever() -> ExitCode {
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// Writes `bytes` to `path` through a temporary file and a rename, so a
/// reader sees all of it or nothing.
pub(crate) fn write_atomic(path: &str, bytes: &[u8]) {
    let temporary = format!("{path}.tmp");
    if fs::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(&temporary, path);
    }
}

/// Writes this process's report: pid, parent, group, its parent-death
/// signal, and every open descriptor's target with whether its `fdinfo`
/// holds a `lock:` line.
fn report(path: &str, child: Option<u32>) {
    let pid = std::process::id();
    let own_dir = format!("/proc/{pid}/fd");
    let mut descriptors = Vec::new();
    if let Ok(entries) = fs::read_dir("/proc/self/fd") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let target = fs::read_link(entry.path())
                .map(|target| target.to_string_lossy().into_owned())
                .unwrap_or_default();
            // The directory being listed is this listing's own descriptor.
            if target == own_dir {
                continue;
            }
            let locked = fs::read_to_string(format!("/proc/self/fdinfo/{name}"))
                .is_ok_and(|info| info.lines().any(|line| line.starts_with("lock:")));
            descriptors.push(serde_json::json!({"fd": name, "target": target, "locked": locked}));
        }
    }
    let value = serde_json::json!({
        "pid": pid,
        "ppid": rustix::process::Pid::as_raw(rustix::process::getppid()),
        "pgid": rustix::process::getpgrp().as_raw_pid(),
        "death_signal": rustix::process::parent_process_death_signal()
            .ok()
            .flatten()
            .map(rustix::process::Signal::as_raw),
        "descriptors": descriptors,
        "child": child,
    });
    write_atomic(path, value.to_string().as_bytes());
}
