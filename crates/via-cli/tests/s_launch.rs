//! S-LAUNCH (adapter design §5.4, §7; runtime §6.1, §8; C2 §7 item 1)
//! through the real `via` binary and an auto-started daemon: the daemon's
//! environment is exactly the bootstrap names, and `describe` and `models`
//! start nothing for a configured vendor harness. Written before the code.
//! Environment values are never read or shown: only names (runtime §6.1).

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses its probe only")]
mod daemon;
#[path = "support/evidenced.rs"]
#[expect(
    dead_code,
    reason = "shared support; this file tears down with its own one-deadline fallback"
)]
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
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
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

/// Runtime §11.2's one teardown deadline is shared: the ordinary stop and
/// the exit wait end [`FALLBACK_SHARE`] before the kill fallback's end,
/// which leaves [`ANCHOR_SHARE`] to `park`'s anchor cleanup.
const FALLBACK_SHARE: Duration = Duration::from_secs(2);
const ANCHOR_SHARE: Duration = Duration::from_secs(3);

/// Every daemon the CLI auto-started is force-stopped and proved gone,
/// then the evidence is collected (runtime §11.2). When the ordinary stop
/// leaves a sandbox process alive, it is killed by identity within the
/// fallback's share of the deadline, and the incomplete clean stop is
/// recorded as a cleanup failure.
impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(evidence) = self.evidence.take() {
            self.root.disable_cleanup(true);
            let exited = self.tear_down();
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

impl Sandbox {
    /// [`evidenced::stop_daemons`]' proof with the kill fallback, all under
    /// the teardown's one deadline.
    fn tear_down(&self) -> evidenced::Exited {
        let deadline = self.teardown.begin();
        let kill_by = deadline.checked_sub(ANCHOR_SHARE).unwrap_or(deadline);
        let stop_by = kill_by.checked_sub(FALLBACK_SHARE).unwrap_or(kill_by);
        let scan = || evidenced::scan_processes(&self.runtime, &self.state, |path| fs::read(path));
        let mut failures = Vec::new();
        let mut stopped = false;
        let mut proof = loop {
            match scan() {
                Ok(alive) if alive.is_empty() => break Ok(()),
                Ok(alive) if Instant::now() >= stop_by => {
                    break Err(format!(
                        "sandbox processes {alive:?} did not exit after the ordinary stop"
                    ));
                }
                Ok(_) => {}
                Err(error) => break Err(error),
            }
            if stopped {
                std::thread::sleep(Duration::from_millis(10).min(outer_cleanup::left(stop_by)));
            } else {
                stopped = true;
                let (record, failure) = outer_cleanup::run_within(
                    self.command().args(["daemon", "stop", "--force", "--json"]),
                    stop_by.min(Instant::now() + outer_cleanup::ORDINARY_STOP),
                );
                self.teardown
                    .record(json!({"generation":"sandbox_stop","stop":record}), failure);
            }
        };
        if let Err(error) = &proof {
            let killed = match scan() {
                Ok(pids) => kill_candidates(
                    &pids,
                    &self.runtime,
                    &self.state,
                    &|path| fs::read(path),
                    kill_by,
                ),
                Err(reason) => json!({"status":"scan_failed","reason":reason}),
            };
            failures.push(format!(
                "the ordinary stop left the sandbox's daemon running ({error}); killed: {killed}"
            ));
            proof = match scan() {
                Ok(alive) if alive.is_empty() => Ok(()),
                Ok(alive) => Err(format!("sandbox processes {alive:?} survived the kill")),
                Err(reason) => Err(reason),
            };
        }
        if proof.is_ok() {
            proof = locks_released(&self.runtime, &self.state, kill_by);
        }
        if Instant::now() > kill_by {
            failures.push("the kill fallback overran its share of the teardown deadline".into());
        }
        let (_, mut recorded) = self.teardown.report();
        recorded.extend(failures);
        evidenced::Exited {
            proof,
            deadline,
            teardown: self.teardown.summary(),
            failures: recorded,
        }
    }
}

/// Neither `daemon.lock` nor `store.lock` is still held, by `by`.
fn locks_released(runtime: &Path, state: &Path, by: Instant) -> Result<(), String> {
    for lock in [runtime.join("daemon.lock"), state.join("store.lock")] {
        loop {
            let held = match fs::File::open(&lock) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(format!("{}: {error}", lock.display())),
                Ok(file) => file.try_lock().is_err(),
            };
            if !held {
                break;
            }
            if Instant::now() >= by {
                return Err(format!("{} is still held", lock.display()));
            }
            std::thread::sleep(Duration::from_millis(10).min(outer_cleanup::left(by)));
        }
    }
    Ok(())
}

/// Whether a `/proc` read failed because the process is gone (Sol r2
/// N4): any other failure is an observation error, never absence.
fn vanished(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
        || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error())
}

/// `/proc/<pid>/stat`'s pid, state and start time (clock ticks); `None`
/// when malformed.
fn parse_stat(bytes: &[u8]) -> Option<(u32, char, u64)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (head, rest) = text.rsplit_once(") ")?;
    let pid = head.split_once(" (")?.0.parse().ok()?;
    let fields: Vec<&str> = rest.split(' ').collect();
    let state = fields.first()?.chars().next()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((pid, state, start))
}

/// The pid the kernel reports for `pidfd` in `/proc/self/fdinfo`; `None`
/// once its process was reaped (`Pid: -1`).
fn pidfd_pid(pidfd: &OwnedFd) -> Result<Option<u32>, String> {
    let path = format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd());
    let text = fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .ok_or_else(|| format!("{path} has no Pid"))?
        .trim();
    if value == "-1" {
        return Ok(None);
    }
    value
        .parse()
        .map(Some)
        .map_err(|_| format!("{path}: malformed Pid {value:?}"))
}

/// How the kill fallback reads `/proc`: `fs::read`, or a test's seam.
type ProcRead<'a> = dyn Fn(&Path) -> std::io::Result<Vec<u8>> + 'a;

/// A candidate process after [`pin_member`].
enum Pinned {
    /// A sandbox process, held by its pidfd.
    Member(OwnedFd),
    /// It had exited: nothing to signal.
    Gone,
    /// Not provably the sandbox's: never signalled.
    Skipped(&'static str),
}

/// Pins `pid` before judging it (Sol r2 N1): opens its pidfd first, then
/// reads its environment and stat through `read`, then confirms through
/// the pidfd that the process is still `pid`, unreaped. A process holds
/// its pid until it is reaped, so the reads, made between the pidfd's open
/// and that confirmation, were of the pidfd's own process. It is the
/// sandbox's when its environment names the sandbox's runtime or State
/// directory, as every `via` process of the sandbox carries. Errors other
/// than disappearance are returned, never read as absence (N4).
fn pin_member(
    pid: u32,
    runtime: &Path,
    state: &Path,
    read: &ProcRead<'_>,
) -> Result<Pinned, String> {
    use std::os::unix::ffi::OsStrExt as _;
    let target = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| format!("invalid pid {pid}"))?;
    let pidfd = match rustix::process::pidfd_open(target, rustix::process::PidfdFlags::empty()) {
        Ok(pidfd) => pidfd,
        Err(rustix::io::Errno::SRCH) => return Ok(Pinned::Gone),
        Err(error) => return Err(format!("pidfd_open({pid}): {error}")),
    };
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let environ = match read(&proc.join("environ")) {
        Ok(environ) => environ,
        Err(error) if vanished(&error) => return Ok(Pinned::Gone),
        Err(error) => return Err(format!("{pid} environ: {error}")),
    };
    let marks = [
        [
            b"VIA_RUNTIME_DIR=".as_slice(),
            runtime.as_os_str().as_bytes(),
        ]
        .concat(),
        [b"VIA_STATE_DIR=".as_slice(), state.as_os_str().as_bytes()].concat(),
    ];
    if !environ
        .split(|byte| *byte == 0)
        .any(|variable| marks.iter().any(|mark| variable == mark.as_slice()))
    {
        return Ok(Pinned::Skipped("not a sandbox process"));
    }
    let stat = match read(&proc.join("stat")) {
        Ok(stat) => stat,
        Err(error) if vanished(&error) => return Ok(Pinned::Gone),
        Err(error) => return Err(format!("{pid} stat: {error}")),
    };
    let (stat_pid, process_state, _) =
        parse_stat(&stat).ok_or_else(|| format!("{pid} stat: malformed"))?;
    if process_state == 'Z' {
        return Ok(Pinned::Gone);
    }
    match pidfd_pid(&pidfd)? {
        None => Ok(Pinned::Gone),
        Some(held) if held == pid && stat_pid == pid => Ok(Pinned::Member(pidfd)),
        Some(_) => Ok(Pinned::Skipped("its /proc data is another process's")),
    }
}

/// Whether the pinned process `pid` has exited: reaped (its pidfd reports
/// no pid), vanished, or a zombie. Observation errors are returned.
fn pinned_exited(pidfd: &OwnedFd, pid: u32) -> Result<bool, String> {
    if pidfd_pid(pidfd)?.is_none() {
        return Ok(true);
    }
    match fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => parse_stat(&stat)
            .map(|(_, state, _)| state == 'Z')
            .ok_or_else(|| format!("{pid} stat: malformed")),
        Err(error) if vanished(&error) => Ok(true),
        Err(error) => Err(format!("{pid} stat: {error}")),
    }
}

/// The kill fallback over the `candidates` a scan named, reading `/proc`
/// through `read` (the test seam): each is pinned ([`pin_member`]), killed
/// through its pidfd and waited for until `kill_by`, the fallback's share
/// of the one teardown deadline. Returns one record per candidate.
fn kill_candidates(
    candidates: &[u32],
    runtime: &Path,
    state: &Path,
    read: &ProcRead<'_>,
    kill_by: Instant,
) -> Value {
    candidates
        .iter()
        .map(|&pid| {
            let pidfd = match pin_member(pid, runtime, state, read) {
                Ok(Pinned::Member(pidfd)) => pidfd,
                Ok(Pinned::Gone) => return json!({"pid":pid,"status":"gone"}),
                Ok(Pinned::Skipped(reason)) => {
                    return json!({"pid":pid,"status":"skipped","reason":reason});
                }
                Err(reason) => return json!({"pid":pid,"status":"unknown","reason":reason}),
            };
            match rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                Err(error) => {
                    return json!({"pid":pid,"status":"kill_failed","reason":error.to_string()});
                }
            }
            loop {
                match pinned_exited(&pidfd, pid) {
                    Ok(true) => return json!({"pid":pid,"status":"gone"}),
                    Ok(false) if Instant::now() < kill_by => std::thread::sleep(
                        Duration::from_millis(10).min(outer_cleanup::left(kill_by)),
                    ),
                    Ok(false) => return json!({"pid":pid,"status":"alive_at_deadline"}),
                    Err(reason) => return json!({"pid":pid,"status":"unknown","reason":reason}),
                }
            }
        })
        .collect()
}

/// Whether the process `pid` that started at `start` has exited: vanished,
/// a zombie, or its pid now another process's. Observation errors are
/// returned (N4).
fn gone(pid: u32, start: u64) -> TestResult<bool> {
    match fs::read(format!("/proc/{pid}/stat")) {
        Ok(bytes) => {
            let (_, state, began) = parse_stat(&bytes).ok_or("malformed stat")?;
            Ok(state == 'Z' || began != start)
        }
        Err(error) if vanished(&error) => Ok(true),
        Err(error) => Err(error.into()),
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
        paused = Some((pid, start, sandbox.runtime.clone(), sandbox.state.clone()));
        let Pinned::Member(pidfd) = pin_member(pid, &sandbox.runtime, &sandbox.state, &|path| {
            fs::read(path)
        })?
        else {
            return Err(format!("daemon {pid} is not pinnable").into());
        };
        rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::STOP)?;
        Ok(())
    });
    let (pid, start, runtime, state) = paused.ok_or("the daemon was never paused")?;
    if !gone(pid, start)? {
        // Leave nothing behind, then fail.
        let by = Instant::now() + outer_cleanup::TEARDOWN;
        let record = kill_candidates(&[pid], &runtime, &state, &|path| fs::read(path), by);
        return Err(format!("the paused daemon survived the teardown: {record}").into());
    }
    let error = outcome
        .err()
        .ok_or("an incomplete clean stop passed the scenario")?
        .to_string();
    check(error.contains("killed"), || {
        format!("the failure does not record the kill: {error}")
    })?;
    // Sol r2 N2: one teardown deadline, shared; the fallback leaves the
    // anchor cleanup its time, which the artifact's cleanup.json shows.
    let artifact = error
        .split_once("scenario evidence: ")
        .and_then(|(_, rest)| rest.split_once(": "))
        .map(|(path, _)| PathBuf::from(path))
        .ok_or_else(|| format!("no artifact named: {error}"))?;
    let cleanup = fs::read_to_string(artifact.join("cleanup.json"))?;
    check(
        !error.contains("overran")
            && !error.contains("unverified")
            && !cleanup.contains("no time left"),
        || format!("the fallback broke the teardown deadline: {error}\n{cleanup}"),
    )
}

/// Kills the test's own helper children, whatever happened.
struct Children(Vec<std::process::Child>);

impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Sol r2 N1: the kill fallback signals only a process whose sandbox
/// membership it read from that same process after pinning it with a
/// pidfd. Through the seam, (a) a scan that names a process the sandbox
/// did not start, and (b) reads that return another (member) process's
/// `/proc` data for that pid, signal nothing; (c) a member is killed.
#[test]
fn s_launch_kill_fallback_never_signals_a_non_member() -> TestResult {
    let dir = tempfile::tempdir()?;
    let runtime = dir.path().join("runtime");
    let state = dir.path().join("state");
    let sleeper = |member: bool| {
        let mut command = Command::new("sleep");
        command.arg("60").env_clear();
        if member {
            command.env("VIA_RUNTIME_DIR", &runtime);
        }
        command.spawn()
    };
    let mut children = Children(vec![sleeper(false)?, sleeper(true)?]);
    let (outsider, member) = (children.0[0].id(), children.0[1].id());
    let kill_by = Instant::now() + Duration::from_secs(5);

    let scanned = kill_candidates(
        &[outsider],
        &runtime,
        &state,
        &|path| fs::read(path),
        kill_by,
    );
    check(children.0[0].try_wait()?.is_none(), || {
        format!("(a) a process the sandbox did not start was signalled: {scanned}")
    })?;
    let member_proc = PathBuf::from(format!("/proc/{member}"));
    let swapped = kill_candidates(
        &[outsider],
        &runtime,
        &state,
        &|path| fs::read(member_proc.join(path.file_name().unwrap_or_default())),
        kill_by,
    );
    check(children.0[0].try_wait()?.is_none(), || {
        format!("(b) another process's /proc data got the outsider signalled: {swapped}")
    })?;
    check(children.0[1].try_wait()?.is_none(), || {
        format!("(b) the member was signalled: {swapped}")
    })?;

    let killed = kill_candidates(&[member], &runtime, &state, &|path| fs::read(path), kill_by);
    let status = children.0[1].wait()?;
    check(
        status.signal() == Some(9) && killed[0]["status"] == "gone",
        || format!("(c) the member was not killed: {status} {killed}"),
    )
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

/// Sol r2 N4: an observation error is not absence. Through the seam, an
/// unreadable environment or a malformed stat of a live sandbox process
/// is reported `unknown`, never `gone`, and nothing is signalled.
#[test]
fn s_launch_kill_fallback_reports_observation_errors() -> TestResult {
    let dir = tempfile::tempdir()?;
    let runtime = dir.path().join("runtime");
    let state = dir.path().join("state");
    let mut member = Command::new("sleep");
    member
        .arg("60")
        .env_clear()
        .env("VIA_RUNTIME_DIR", &runtime);
    let mut children = Children(vec![member.spawn()?]);
    let pid = children.0[0].id();
    let kill_by = Instant::now() + Duration::from_secs(5);
    let denied = |_: &Path| -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    };
    let malformed = |path: &Path| -> std::io::Result<Vec<u8>> {
        if path.ends_with("stat") {
            Ok(b"garbage".to_vec())
        } else {
            fs::read(path)
        }
    };
    let observations: [&ProcRead<'_>; 2] = [&denied, &malformed];
    for read in observations {
        let record = kill_candidates(&[pid], &runtime, &state, read, kill_by);
        check(record[0]["status"] == "unknown", || {
            format!("an observation error was not reported unknown: {record}")
        })?;
        check(children.0[0].try_wait()?.is_none(), || {
            format!("a process observed with an error was signalled: {record}")
        })?;
    }
    Ok(())
}
