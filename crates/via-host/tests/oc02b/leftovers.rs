//! OC10's positive report path over a real anchor and fake vendor/tool.

use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    process::Command,
    time::{Duration, Instant},
};

use via_host::{CleanupEvidence, CloseMode, CloseRequest, EnvAllowList, LeftoverScope};

use super::support::{Fixture, alive, kill, pid_of, report, runtime, stat, within};

/// The original private group can be absent while its escaped tool remains.
/// Only the fixture cleans that tool; Host reports it without signalling it.
pub(crate) fn surviving_marked_tool() {
    runtime().block_on(async {
        rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
        let fixture = Fixture::new();
        let mut environment = Vec::new();
        for (name, directory) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "tool-state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            let directory = fixture.root.join(directory);
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .unwrap();
            environment.push((name.into(), directory.into_os_string()));
        }
        let host = fixture.host();
        let vendor_path = fixture.report("vendor");
        let tool_path = fixture.report("tool");
        let mut spec = fixture.spec(&[
            "report-child",
            vendor_path.to_str().unwrap(),
            tool_path.to_str().unwrap(),
        ]);
        spec.env = EnvAllowList::try_from_entries(environment.clone()).unwrap();
        let acquired = host.acquire(spec, within(10)).await.unwrap();
        let vendor = report(&vendor_path, Duration::from_secs(5)).await.unwrap();
        let vendor_ticks = stat(pid_of(&vendor)).unwrap().1;
        let pid = u32::try_from(vendor["child"].as_u64().unwrap()).unwrap();
        let mut tool = OwnedTool {
            pid,
            ticks: stat(pid).unwrap().1,
            reaped: false,
        };
        let tool_report = report(&tool_path, Duration::from_secs(5)).await;
        let close_by = within(5);
        let close = acquired
            .control
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: close_by,
            })
            .await;
        let leftovers = acquired
            .control
            .report_leftovers(LeftoverScope::Server, close_by)
            .await;
        let survived = alive(tool.pid, tool.ticks);
        let vendor_gone = !alive(pid_of(&vendor), vendor_ticks);
        drop(acquired);
        let shutdown = host.shutdown(within(5), &[]).await;
        // Settle every owned process and check the unique fixture root before
        // any acceptance assertion, including when the report is wrong.
        let reaped = tool.stop();
        let pgrep = Command::new("pgrep")
            .args(["-a", "-f", fixture.root.to_str().unwrap()])
            .env_clear()
            .envs(environment)
            .output()
            .unwrap();

        assert!(matches!(close.cleanup, CleanupEvidence::GroupAbsent(_)));
        assert!(vendor_gone);
        assert!(survived, "Host must not manage the escaped tool");
        assert!(tool_report.is_some());
        assert_eq!(leftovers.scope, LeftoverScope::Server);
        assert_eq!(leftovers.total, 1);
        assert_eq!(leftovers.processes.len(), 1);
        assert_eq!(leftovers.processes[0].pid, pid);
        assert!(!leftovers.processes[0].comm.is_empty());
        assert!(leftovers.processes[0].started_at.ends_with('Z'));
        // Other eligible same-uid entries may be unreadable on the test host;
        // incompleteness never erases the positively observed tool.
        assert!(reaped, "fixture tool was not reaped");
        assert_eq!(shutdown.pending_tasks, 0);
        assert_eq!(shutdown.failed_tasks, 0);
        assert_eq!(
            pgrep.status.code(),
            Some(1),
            "owned survivors: {:?}",
            String::from_utf8_lossy(&pgrep.stdout)
        );
    });
}

/// The helper is a descendant this fixture started. The unreaped child and
/// start-tick guard prevent a reused numeric pid from authorizing its cleanup.
struct OwnedTool {
    pid: u32,
    ticks: u64,
    reaped: bool,
}

impl OwnedTool {
    fn stop(&mut self) -> bool {
        if self.reaped {
            return true;
        }
        if alive(self.pid, self.ticks) {
            kill(self.pid);
        }
        let pid = rustix::process::Pid::from_raw(i32::try_from(self.pid).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG) {
                Ok(Some(_)) => {
                    self.reaped = true;
                    return true;
                }
                Err(rustix::io::Errno::CHILD) => return !alive(self.pid, self.ticks),
                Ok(None) | Err(_) => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for OwnedTool {
    fn drop(&mut self) {
        // Best-effort fallback if setup or an acceptance assertion failed.
        self.stop();
    }
}
