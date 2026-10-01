//! S-LAUNCH (adapter design §5.4, §7; runtime §6.1, §8; C2 §7 item 1)
//! through the real `via` binary and an auto-started daemon: the daemon's
//! environment is exactly the bootstrap names, and `describe` and `models`
//! start nothing for a configured vendor harness. Written before the code.
//! Environment values are never read or shown: only names (runtime §6.1).

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses its probe only")]
mod daemon;
#[path = "support/evidenced.rs"]
mod evidenced;
#[path = "support/outer_cleanup.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod outer_cleanup;
#[path = "support/process.rs"]
mod process;
#[path = "support/scenario.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod scenario;
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use evidenced::evidenced;
use scenario::{Captured, ScenarioError, run_command};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Credential-like names the client sets; none may reach the daemon.
const CREDENTIALS: [&str; 3] = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "VIA_UNLISTED_SECRET"];

/// One isolated deployment whose CLI runs with a known environment: every
/// bootstrap name set, plus credential-like names. Its drop force-stops
/// whatever daemon the CLI auto-started and collects the evidence.
struct Sandbox {
    root: tempfile::TempDir,
    state: PathBuf,
    runtime: PathBuf,
    sync: PathBuf,
    fake: PathBuf,
    fixture: PathBuf,
    /// Prepended to the client's `PATH`.
    bin: PathBuf,
    evidence: Option<support::evidence::Evidence>,
    teardown: outer_cleanup::Teardown,
    /// Cleared by a scenario whose daemon never opens a Store.
    store: bool,
}

impl Sandbox {
    fn new() -> TestResult<Self> {
        let via = Path::new(env!("CARGO_BIN_EXE_via"));
        let fake = via
            .parent()
            .ok_or("via binary has no parent directory")?
            .join("via-fake-agent");
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let [state, runtime, sync, bin, xdg] =
            ["state", "runtime", "sync", "bin", "xdg"].map(|name| root.path().join(name));
        for dir in [&state, &runtime, &sync, &bin, &xdg] {
            daemon::private_dir(dir)?;
        }
        let fixture = root.path().join("fixture.json");
        fs::write(&fixture, br#"{"scripts":[]}"#)?;
        let evidence = Some(evidenced::open(&fake, &fixture)?);
        Ok(Self {
            root,
            state,
            runtime,
            sync,
            fake,
            fixture,
            bin,
            evidence,
            teardown: outer_cleanup::Teardown::new(),
            store: true,
        })
    }

    fn command(&self) -> Command {
        let mut path = OsString::from(self.bin.as_os_str());
        if let Some(inherited) = std::env::var_os("PATH") {
            path.push(":");
            path.push(inherited);
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_via"));
        command
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", path)
            .env("LANG", "C.UTF-8")
            .env("USER", "via-test")
            .env("LOGNAME", "via-test")
            .env("XDG_RUNTIME_DIR", self.root.path().join("xdg"))
            .env("VIA_FAKE_AGENT_BINARY", &self.fake)
            .env("VIA_FAKE_SCENARIO", &self.fixture)
            .env("VIA_FAKE_SYNC_DIR", &self.sync)
            .env("VIA_STATE_DIR", &self.state)
            .env("VIA_RUNTIME_DIR", &self.runtime);
        for name in CREDENTIALS {
            command.env(name, "not-a-real-credential");
        }
        // Test builds forward their own names (client.rs): one of them.
        #[cfg(feature = "test-failpoints")]
        command.env("VIA_TEST_IDLE_EXIT_MS", "600000");
        command
    }

    fn run(&self, args: &[&str]) -> TestResult<Captured> {
        self.run_within(args, Duration::from_secs(20))
    }

    /// Runs the CLI by `timeout`; a timed-out run is a typed
    /// [`ScenarioError::Timeout`] with its cleanup notes (runtime §11.2).
    fn run_within(&self, args: &[&str], timeout: Duration) -> TestResult<Captured> {
        let mut command = self.command();
        command.args(args);
        let captured = run_command(&mut command, timeout)?;
        if captured.timed_out {
            return Err(Box::new(ScenarioError::Timeout(format!(
                "via {args:?} timed out{}",
                captured.notes()
            ))));
        }
        Ok(captured)
    }

    /// Writes `<state>/daemon.json` (0600).
    fn config(&self, text: &str) -> TestResult {
        let path = self.state.join("daemon.json");
        fs::write(&path, text)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
}

/// Every daemon the CLI auto-started is force-stopped and proved gone,
/// then the evidence is collected (runtime §11.2). When the ordinary stop
/// leaves a sandbox process alive, it is killed by identity and the
/// incomplete clean stop is recorded as a cleanup failure.
impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(evidence) = self.evidence.take() {
            self.root.disable_cleanup(true);
            let mut exited =
                evidenced::stop_daemons(&self.runtime, &self.state, &self.teardown, |by| {
                    outer_cleanup::run_within(
                        self.command().args(["daemon", "stop", "--force", "--json"]),
                        by,
                    )
                });
            if exited.proof.is_err() {
                let killed = kill_survivors(&self.runtime, &self.state);
                exited.failures.push(format!(
                    "the ordinary stop left the sandbox's daemon running; killed: {killed}"
                ));
            }
            let expected = evidenced::Expected {
                store: self.store,
                folders: false,
            };
            evidenced::park(
                evidence,
                self.root.path().to_owned(),
                &self.state,
                expected,
                exited,
            );
        }
    }
}

/// Sends `signal` to process `pid` only while it is still the process that
/// started at `start` (clock ticks, `/proc/<pid>/stat`): its pidfd pins one
/// process, and the start time is checked again after the pidfd is open, so
/// a reused pid is never signalled. `false` when it is already gone.
fn signal_identified(pid: u32, start: u64, signal: rustix::process::Signal) -> TestResult<bool> {
    let raw = i32::try_from(pid)?;
    let Some(target) = rustix::process::Pid::from_raw(raw) else {
        return Err(format!("invalid pid {pid}").into());
    };
    let Ok(pidfd) = rustix::process::pidfd_open(target, rustix::process::PidfdFlags::empty())
    else {
        return Ok(false);
    };
    if outer_cleanup::process_stat(pid).map(|(_, now)| now) != Some(start) {
        return Ok(false);
    }
    match rustix::process::pidfd_send_signal(&pidfd, signal) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Whether the process `pid` that started at `start` has exited: gone, a
/// zombie, or the pid now another process's.
fn gone(pid: u32, start: u64) -> TestResult<bool> {
    Ok(
        process::exited(pid)?
            || outer_cleanup::process_stat(pid).map(|(_, now)| now) != Some(start),
    )
}

/// Kills the identified process `pid` (started at `start`) and waits, by a
/// bound, until it is gone. Returns its record.
fn kill_identified(pid: u32, start: u64) -> Value {
    let signalled = match signal_identified(pid, start, rustix::process::Signal::KILL) {
        Ok(signalled) => signalled,
        Err(error) => return json!({"pid":pid,"status":"kill_failed","reason":error.to_string()}),
    };
    let deadline = Instant::now() + outer_cleanup::REAP * 5;
    loop {
        match gone(pid, start) {
            Ok(true) => return json!({"pid":pid,"killed":signalled,"status":"gone"}),
            Ok(false) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(false) => return json!({"pid":pid,"killed":signalled,"status":"alive"}),
            Err(error) => {
                return json!({"pid":pid,"killed":signalled,"status":"unknown",
                    "reason":error.to_string()});
            }
        }
    }
}

/// Kills every sandbox process still alive: one whose environment names
/// the sandbox's runtime or State directory ([`evidenced::scan_processes`]),
/// identified by its start time ([`kill_identified`]). Returns the record.
fn kill_survivors(runtime: &Path, state: &Path) -> Value {
    match evidenced::scan_processes(runtime, state, |path| fs::read(path)) {
        Ok(pids) => pids
            .into_iter()
            .map(|pid| match outer_cleanup::process_stat(pid) {
                Some((_, start)) => kill_identified(pid, start),
                None => json!({"pid":pid,"status":"gone"}),
            })
            .collect(),
        Err(error) => json!({"status":"scan_failed","reason":error}),
    }
}

fn check(condition: bool, message: impl FnOnce() -> String) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message().into())
    }
}

/// The names in `/proc/<pid>/environ`; the values are never kept.
fn environ_names(pid: u32) -> TestResult<BTreeSet<String>> {
    let bytes = fs::read(format!("/proc/{pid}/environ"))?;
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let name = entry.split(|byte| *byte == b'=').next().unwrap_or_default();
            String::from_utf8_lossy(name).into_owned()
        })
        .collect())
}

/// The serving daemon's pid from an auto-starting `daemon status`, checked
/// against the direct, never auto-starting, socket probe.
fn auto_started(sandbox: &Sandbox) -> TestResult<u32> {
    let run = sandbox.run(&["daemon", "status", "--json"])?;
    check(run.status.success(), || {
        format!(
            "auto-start failed: {} {}",
            run.status,
            String::from_utf8_lossy(&run.stderr)
        )
    })?;
    let status: Value = serde_json::from_slice(&run.stdout)?;
    let pid = u32::try_from(status["pid"].as_u64().ok_or("status has no pid")?)?;
    check(daemon::serving_pid(&sandbox.runtime) == Some(pid), || {
        format!("pid {pid} does not serve the sandbox socket")
    })?;
    Ok(pid)
}

/// Design §7 S-LAUNCH (1), runtime §6.1: the auto-started daemon's
/// environment names are exactly the bootstrap names the client set
/// (`HOME`, `PATH`, `LANG`, `USER`, `LOGNAME`, `XDG_RUNTIME_DIR` and the
/// fake's three) plus the explicit state, runtime and test-build settings;
/// no credential-like client name reaches it.
#[test]
fn s_launch_autostarted_daemon_gets_bootstrap_env() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new()?;
        let pid = auto_started(&sandbox)?;
        let names = environ_names(pid)?;
        let bootstrap = [
            "HOME",
            "PATH",
            "LANG",
            "USER",
            "LOGNAME",
            "XDG_RUNTIME_DIR",
            "VIA_FAKE_AGENT_BINARY",
            "VIA_FAKE_SCENARIO",
            "VIA_FAKE_SYNC_DIR",
        ];
        let mut expected: BTreeSet<String> = bootstrap.map(str::to_owned).into();
        expected.extend(["VIA_STATE_DIR".to_owned(), "VIA_RUNTIME_DIR".to_owned()]);
        #[cfg(feature = "test-failpoints")]
        expected.insert("VIA_TEST_IDLE_EXIT_MS".to_owned());
        check(names == expected, || {
            format!("daemon environment names {names:?}, expected {expected:?}")
        })?;
        for name in CREDENTIALS {
            check(!names.contains(name), || {
                format!("{name} reached the daemon")
            })?;
        }
        check(
            via_core::BOOTSTRAP_ENV
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                == bootstrap.into_iter().collect(),
            || format!("BOOTSTRAP_ENV is {:?}", via_core::BOOTSTRAP_ENV),
        )?;
        Ok(())
    })
}

/// Writes an executable that appends its name to `marker` when it runs.
fn marking_script(path: &Path, marker: &Path) -> TestResult {
    fs::write(
        path,
        format!("#!/bin/sh\necho \"$0\" >> '{}'\nexit 0\n", marker.display()),
    )?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// C2 §7 item 1, design §7 S-LAUNCH acceptance: with `harnesses.claude`
/// and `harnesses.codex` pinned to marker-writing executables, and
/// `opencode` (and the defaults) on the client's `PATH`, `describe` and
/// `models` for each vendor harness run no binary. Their result is the
/// merged base's: no adapter exists before x.3.2, so `describe` refuses
/// `harness_unavailable` and `models` lists nothing. A guard for x.3.2.
#[test]
fn s_launch_describe_starts_nothing() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new()?;
        let marker = sandbox.root.path().join("ran.marker");
        let pinned = sandbox.root.path().join("pinned");
        daemon::private_dir(&pinned)?;
        for name in ["claude", "codex"] {
            marking_script(&pinned.join(format!("{name}-pinned")), &marker)?;
        }
        for name in ["claude", "codex", "opencode"] {
            marking_script(&sandbox.bin.join(name), &marker)?;
        }
        sandbox.config(
            &json!({"harnesses":{
                "claude":{"binary":pinned.join("claude-pinned")},
                "codex":{"binary":pinned.join("codex-pinned"),"inherit":{"hooks":true}},
            }})
            .to_string(),
        )?;
        auto_started(&sandbox)?;
        let mut outcomes = Vec::new();
        for harness in ["claude", "codex", "opencode"] {
            let described = sandbox.run(&["describe", "--harness", harness, "--json"])?;
            // A request error is printed on stderr.
            let reply: Value = serde_json::from_slice(&described.stderr).unwrap_or(Value::Null);
            outcomes.push(json!({"describe":harness,"exit":described.status.code(),
                "stdout":reply,"stderr":String::from_utf8_lossy(&described.stderr)}));
            check(
                described.status.code() == Some(2)
                    && reply["data"]["kind"] == "harness_unavailable",
                || format!("describe {harness}: {outcomes:?}"),
            )?;
            let models = sandbox.run(&["models", "--harness", harness, "--json"])?;
            let listed: Value = serde_json::from_slice(&models.stdout).unwrap_or(Value::Null);
            outcomes.push(json!({"models":harness,"exit":models.status.code(),
                "stdout":listed,"stderr":String::from_utf8_lossy(&models.stderr)}));
            check(
                models.status.success() && listed["models"] == json!([]),
                || format!("models {harness}: {outcomes:?}"),
            )?;
        }
        check(!marker.exists(), || {
            format!(
                "a vendor binary ran: {}",
                fs::read_to_string(&marker).unwrap_or_default()
            )
        })
    })
}

/// Every entry under `dir`, recursively, with its size and modification
/// time: the evidence that a refused start touched nothing.
fn listing(dir: &Path, into: &mut BTreeMap<PathBuf, (u64, SystemTime)>) -> TestResult {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        into.insert(path.clone(), (metadata.len(), metadata.modified()?));
        if metadata.is_dir() {
            listing(&path, into)?;
        }
    }
    Ok(())
}

/// Runtime §8, design §5.4: an invalid `harnesses` is an invalid
/// `daemon.json`, like every other key: `via daemon` exits 78 with one
/// `via: daemon config invalid: <key>: <rule>` line, touching nothing in
/// the state or runtime directory (no lock, no `via.log`, no Store, no
/// socket). A duplicate key at any level is refused like the rest of
/// the file's.
#[test]
fn s_launch_invalid_harnesses_refuse_start() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new()?;
        sandbox.store = false;
        let cases = [
            (
                r#"{"harnesses":{"claude":{"binary":"bin/claude"}}}"#,
                "harnesses.claude.binary",
                "must be an absolute path",
            ),
            (
                r#"{"harnesses":{"gemini":{}}}"#,
                "harnesses.gemini",
                "unknown harness",
            ),
            (
                r#"{"harnesses":{"codex":{"inherit":{"hooks":"yes"}}}}"#,
                "harnesses.codex.inherit.hooks",
                "must be a boolean",
            ),
            (
                r#"{"harnesses":{"claude":{},"claude":{}}}"#,
                "harnesses.claude",
                "duplicate key",
            ),
            (
                r#"{"harnesses":{"claude":{"binary":"/a","binary":"/b"}}}"#,
                "harnesses.claude.binary",
                "duplicate key",
            ),
            (
                r#"{"harnesses":{"claude":{"inherit":{},"inherit":{}}}}"#,
                "harnesses.claude.inherit",
                "duplicate key",
            ),
            (
                r#"{"harnesses":{"claude":{"inherit":{"hooks":true,"hooks":false}}}}"#,
                "harnesses.claude.inherit.hooks",
                "duplicate key",
            ),
        ];
        for (text, key, rule) in cases {
            sandbox.config(text)?;
            let mut before = BTreeMap::new();
            listing(&sandbox.state, &mut before)?;
            listing(&sandbox.runtime, &mut before)?;
            let mut command = sandbox.command();
            command.arg("daemon");
            let run = run_command(&mut command, Duration::from_secs(10))?;
            let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
            let prefix = format!("via: daemon config invalid: {key}: ");
            check(
                !run.timed_out
                    && run.status.code() == Some(78)
                    && stderr.lines().any(|line| {
                        line.starts_with(&prefix) && line[prefix.len()..].contains(rule)
                    }),
                || format!("{text}: exit {:?}, {stderr}", run.status),
            )?;
            let mut after = BTreeMap::new();
            listing(&sandbox.state, &mut after)?;
            listing(&sandbox.runtime, &mut after)?;
            check(before == after, || {
                format!("{text} changed the state or runtime directory: {before:?} {after:?}")
            })?;
            check(!sandbox.runtime.join("via.sock").exists(), || {
                format!("{text} left a socket")
            })?;
        }
        Ok(())
    })
}

/// Sol r1 #2, runtime §11.2: a daemon whose stop request cannot be served
/// (here, stopped by SIGSTOP) is still gone after the sandbox's teardown,
/// killed by identity after the ordinary stop, and the incomplete clean
/// stop fails the scenario.
#[test]
fn s_launch_teardown_kills_an_unresponsive_daemon() -> TestResult {
    let mut paused = None;
    let outcome = evidenced(|| {
        let sandbox = Sandbox::new()?;
        let pid = auto_started(&sandbox)?;
        let (_, start) = outer_cleanup::process_stat(pid).ok_or("no daemon stat")?;
        paused = Some((pid, start));
        check(
            signal_identified(pid, start, rustix::process::Signal::STOP)?,
            || format!("daemon {pid} vanished before it was paused"),
        )
    });
    let (pid, start) = paused.ok_or("the daemon was never paused")?;
    if !gone(pid, start)? {
        // Leave nothing behind, then fail.
        let record = kill_identified(pid, start);
        return Err(format!("the paused daemon survived the teardown: {record}").into());
    }
    let error = outcome
        .err()
        .ok_or("an incomplete clean stop passed the scenario")?;
    check(error.to_string().contains("killed"), || {
        format!("the failure does not record the kill: {error}")
    })
}

/// Sol r1 #5, runtime §11.2: a CLI run that outlives its bound is a typed
/// timeout, not a plain failure.
#[test]
fn s_launch_cli_timeout_is_typed() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new()?;
        sandbox.store = false;
        // The foreground daemon serves until killed at the bound.
        let error = sandbox
            .run_within(&["daemon"], Duration::from_millis(300))
            .err()
            .ok_or("the foreground daemon returned")?;
        check(
            matches!(
                error.downcast_ref::<ScenarioError>(),
                Some(ScenarioError::Timeout(_))
            ),
            || format!("not a typed timeout: {error:?}"),
        )
    })
}
