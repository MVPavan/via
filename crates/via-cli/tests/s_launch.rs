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
    reason = "shared support; this file tears down with its own owned-child fallback"
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

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use evidenced::evidenced;
use scenario::{Captured, ScenarioError, run_command};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Credential-like names the client sets; none may reach the daemon.
const CREDENTIALS: [&str; 3] = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "VIA_UNLISTED_SECRET"];

/// One isolated deployment whose CLI runs with a known environment: every
/// bootstrap name set, plus credential-like names. Its daemon is its own
/// direct child ([`Sandbox::start_daemon`]), or, for the auto-start
/// scenario only, the one the CLI started. Its drop stops the daemon and
/// collects the evidence.
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
    /// The directly owned daemon, the only process teardown may signal.
    daemon: Option<Child>,
    /// The teardown's one deadline, once it began.
    deadline: Rc<Cell<Option<Instant>>>,
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
            daemon: None,
            deadline: Rc::default(),
        })
    }

    /// Starts `via daemon` as this sandbox's directly owned child, with
    /// the CLI's environment and roots, its stderr traced, and waits until
    /// it serves the sandbox's socket. Teardown signals it only through
    /// this retained handle (runtime §11.2).
    fn start_daemon(&mut self) -> TestResult<u32> {
        let trace = fs::File::create(self.root.path().join("daemon.trace"))?;
        let child = self
            .command()
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(trace)
            .spawn()?;
        let pid = child.id();
        self.daemon = Some(child);
        self.wait_serving(pid, Instant::now() + Duration::from_secs(20))
    }

    /// Waits until the directly owned daemon `pid` serves the sandbox's
    /// socket, by `by`: the deadline is checked before each probe, and a
    /// probe's exchange is bounded by it and accepted only by it
    /// ([`daemon::serving_pid_by`]).
    fn wait_serving(&mut self, pid: u32, by: Instant) -> TestResult<u32> {
        let child = self.daemon.as_mut().ok_or("no directly owned daemon")?;
        loop {
            if Instant::now() >= by {
                return Err(Box::new(ScenarioError::Timeout(format!(
                    "daemon {pid} did not serve by its startup deadline"
                ))));
            }
            if daemon::serving_pid_by(&self.runtime, by) == Some(pid) {
                return Ok(pid);
            }
            if let Some(status) = child.try_wait()? {
                return Err(format!("daemon {pid} exited before serving: {status}").into());
            }
            std::thread::sleep(Duration::from_millis(10).min(outer_cleanup::left(by)));
        }
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

/// The sandbox's daemon is force-stopped and proved gone, then the
/// evidence is collected (runtime §11.2). Only the directly owned daemon
/// is ever signalled, by its retained child handle, when the ordinary stop
/// left it running; that incomplete clean stop is a cleanup failure. A
/// daemon the CLI auto-started has no handle and is never signalled: an
/// exit not proved is recorded with the processes observed, the sandbox
/// is kept and the scenario fails.
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
    /// [`evidenced::stop_daemons`]' proof with the owned-child fallback,
    /// all under the teardown's one deadline. `/proc` is only observed,
    /// each scan by its cutoff.
    fn tear_down(&mut self) -> evidenced::Exited {
        let deadline = self.teardown.begin();
        self.deadline.set(Some(deadline));
        let kill_by = deadline.checked_sub(ANCHOR_SHARE).unwrap_or(deadline);
        let stop_by = kill_by.checked_sub(FALLBACK_SHARE).unwrap_or(kill_by);
        let scan = |cutoff| {
            evidenced::scan_processes_by(&self.runtime, &self.state, Some(cutoff), read_capped)
        };
        let mut failures = Vec::new();
        let mut observed = Vec::new();
        let mut stopped = false;
        let mut proof = loop {
            match scan(stop_by) {
                Ok(alive) if alive.is_empty() => break Ok(()),
                Ok(alive) => observed = alive,
                Err(reason) if observed.is_empty() => break Err(reason),
                Err(reason) => {
                    break Err(format!(
                        "sandbox processes {observed:?} did not exit after the ordinary stop ({reason})"
                    ));
                }
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
        if let Some(mut child) = self.daemon.take() {
            let pid = child.id();
            if let Err(error) = &proof {
                failures.push(kill_owned(&mut child, error, kill_by));
            }
            if !outer_cleanup::reap_by(&mut child, kill_by) {
                failures.push(format!(
                    "daemon child {pid} was not reaped by the fallback's deadline"
                ));
            }
            if proof.is_err() {
                proof = match scan(kill_by) {
                    Ok(alive) if alive.is_empty() => Ok(()),
                    Ok(alive) => Err(format!(
                        "sandbox processes {alive:?} survived; nothing else signalled"
                    )),
                    Err(reason) => Err(reason),
                };
            }
        } else if let Err(error) = &proof {
            failures.push(format!(
                "the sandbox's exit is unproven ({error}); nothing signalled: \
                 no retained child handle"
            ));
        }
        if proof.is_ok() {
            proof = locks_released(&self.runtime, &self.state, kill_by);
        }
        if Instant::now() > kill_by {
            failures.push("the teardown overran the fallback's share of its deadline".into());
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

    /// Stops (SIGSTOP) the directly owned daemon through its retained
    /// handle, checked unreaped first: only this process reaps it, so its
    /// pid cannot name another process.
    fn pause_daemon(&mut self) -> TestResult {
        let child = self.daemon.as_mut().ok_or("no directly owned daemon")?;
        if let Some(status) = child.try_wait()? {
            return Err(format!("the daemon already exited: {status}").into());
        }
        let pid = i32::try_from(child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .ok_or("invalid daemon pid")?;
        rustix::process::kill_process(pid, rustix::process::Signal::STOP)?;
        Ok(())
    }
}

/// The owned-child fallback (runtime §11.2): kills the retained `child`,
/// left running after the ordinary stop for `cause`, only while `kill_by`
/// has not passed, checked immediately before the signal. Returns the
/// cleanup failure to record; past `kill_by`, nothing is signalled and the
/// child is left to the kept sandbox.
fn kill_owned(child: &mut Child, cause: &str, kill_by: Instant) -> String {
    let pid = child.id();
    if Instant::now() >= kill_by {
        return format!(
            "the ordinary stop left the sandbox's daemon running ({cause}); the fallback's \
             cutoff passed first: nothing signalled, daemon child {pid} left running"
        );
    }
    let killed = child.kill().map_err(|error| error.to_string());
    format!(
        "the ordinary stop left the sandbox's daemon running ({cause}); \
         killed its retained child {pid}: {killed:?}"
    )
}

/// At most `cap` bytes of `path`: the teardown's `/proc` reads.
fn read_capped(path: &Path, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(cap).read_to_end(&mut bytes)?;
    Ok(bytes)
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

/// Whether the process `pid` that started at `start` outlived its
/// teardown, observed until that teardown's own `deadline` (critical r1
/// #4): never signalled and given no new budget. Expiry is checked before
/// each observation and after it, so only an absence observed by the
/// deadline proves exit (critical r2 N3); otherwise the survivor is
/// reported uncertain.
fn survived(pid: u32, start: u64, deadline: Instant) -> TestResult<Option<String>> {
    let uncertain = || {
        Ok(Some(format!(
            "process {pid} survived its teardown: uncertain at the teardown deadline; \
             nothing signalled"
        )))
    };
    loop {
        if Instant::now() >= deadline {
            return uncertain();
        }
        let ended = gone(pid, start)?;
        if Instant::now() > deadline {
            return uncertain();
        }
        if ended {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10).min(outer_cleanup::left(deadline)));
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
        let mut sandbox = Sandbox::new()?;
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
        sandbox.start_daemon()?;
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

/// `via daemon` in the foreground, by `timeout`: a run that outlives it
/// is a typed timeout with its cleanup notes ([`Sandbox::run_within`]).
fn foreground_daemon(sandbox: &Sandbox, timeout: Duration) -> TestResult<Captured> {
    sandbox.run_within(&["daemon"], timeout)
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
            let run = foreground_daemon(&sandbox, Duration::from_secs(10))?;
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

/// Sol r1 #2, critical r1 #1, runtime §11.2: a directly owned daemon
/// whose stop request cannot be served (here, stopped by SIGSTOP) is
/// killed by its retained child handle and reaped within the teardown's
/// deadline, and the incomplete clean stop fails the scenario.
#[test]
fn s_launch_teardown_kills_an_unresponsive_daemon() -> TestResult {
    let mut paused = None;
    let mut deadline = Rc::default();
    let outcome = evidenced(|| {
        let mut sandbox = Sandbox::new()?;
        deadline = Rc::clone(&sandbox.deadline);
        let pid = sandbox.start_daemon()?;
        let (_, start) = outer_cleanup::process_stat(pid).ok_or("no daemon stat")?;
        paused = Some((pid, start));
        sandbox.pause_daemon()
    });
    let (pid, start) = paused.ok_or("the daemon was never paused")?;
    let deadline = deadline.get().ok_or("the teardown never began")?;
    if let Some(survivor) = survived(pid, start, deadline)? {
        return Err(survivor.into());
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
    /// Kills and reaps each child within a bound, never waiting without
    /// end; one not reaped by then fails the test (unless it already
    /// panicked).
    fn drop(&mut self) {
        let unreaped: Vec<u32> = self
            .0
            .iter_mut()
            .filter_map(|child| {
                let by = Instant::now() + outer_cleanup::REAP;
                (!outer_cleanup::kill_and_reap(child, by)).then(|| child.id())
            })
            .collect();
        assert!(
            unreaped.is_empty() || std::thread::panicking(),
            "helpers {unreaped:?} were not reaped within their bound"
        );
    }
}

/// Sol r1 #5, runtime §11.2: a CLI run that outlives its bound is a typed
/// timeout, not a plain failure.
#[test]
fn s_launch_cli_timeout_is_typed() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new()?;
        sandbox.store = false;
        // The foreground daemon serves until killed at the bound; the
        // invalid-configuration runs share this path.
        let error = foreground_daemon(&sandbox, Duration::from_millis(300))
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

/// Critical r1 #2, runtime §11.2: a process scan keeps its cutoff. One
/// whose cutoff has passed reads nothing and reports uncertainty, never
/// absence; one that reaches its cutoff mid-scan stops before its next
/// read. Each environment and command-line read is capped, and an
/// environment cut at its cap is indeterminate for this build's `via`.
#[test]
fn s_launch_scan_keeps_its_cutoff() -> TestResult {
    use std::cell::{Cell, RefCell};
    let dir = tempfile::tempdir()?;
    let (runtime, state) = (dir.path().join("runtime"), dir.path().join("state"));
    let reads = Cell::new(0_usize);
    let counted = |path: &Path, cap: u64| {
        reads.set(reads.get() + 1);
        read_capped(path, cap)
    };
    let expired = evidenced::scan_processes_by(&runtime, &state, Some(Instant::now()), counted);
    check(expired.is_err() && reads.get() == 0, || {
        format!("an expired scan: {expired:?} after {} reads", reads.get())
    })?;

    reads.set(0);
    let slow = |path: &Path, cap: u64| {
        reads.set(reads.get() + 1);
        std::thread::sleep(Duration::from_millis(100));
        read_capped(path, cap)
    };
    let cutoff = Instant::now() + Duration::from_millis(50);
    let cut = evidenced::scan_processes_by(&runtime, &state, Some(cutoff), slow);
    check(cut.is_err() && reads.get() == 1, || {
        format!(
            "a scan past its cutoff: {cut:?} after {} reads",
            reads.get()
        )
    })?;

    let caps = RefCell::new(BTreeSet::new());
    let via = Path::new(env!("CARGO_BIN_EXE_via"));
    let capped = |path: &Path, cap: u64| -> std::io::Result<Vec<u8>> {
        caps.borrow_mut()
            .insert((path.file_name().map(ToOwned::to_owned), cap));
        if path.ends_with("environ") {
            // A full environment without the mark: possibly cut.
            Ok(vec![
                b'x';
                usize::try_from(cap).unwrap_or(usize::MAX).min(1 << 20)
            ])
        } else {
            Ok([via.as_os_str().as_encoded_bytes(), b"\0"].concat())
        }
    };
    let far = Instant::now() + Duration::from_secs(5);
    let truncated = evidenced::scan_processes_by(&runtime, &state, Some(far), capped);
    let caps = caps.into_inner();
    check(
        truncated.is_err() && caps.iter().all(|(_, cap)| *cap <= evidenced::ENVIRON_CAP),
        || format!("a cut environment of a `via`: {truncated:?}; caps {caps:?}"),
    )
}

/// Critical r1 #1, runtime §11.2: teardown never signals a guessed daemon
/// pid. An outsider that carries the sandbox's runtime mark and its own
/// genuine stat, still alive after the ordinary stop, is not signalled:
/// the teardown records the uncertainty with its pid, keeps the sandbox
/// and fails.
#[test]
fn s_launch_teardown_never_signals_an_outsider() -> TestResult {
    let mut children = Children(Vec::new());
    let mut root = None;
    let outcome = evidenced(|| {
        let mut sandbox = Sandbox::new()?;
        sandbox.store = false;
        root = Some(sandbox.root.path().to_owned());
        let mut outsider = Command::new("sleep");
        outsider
            .arg("60")
            .env_clear()
            .env("VIA_RUNTIME_DIR", &sandbox.runtime);
        children.0.push(outsider.spawn()?);
        Ok(())
    });
    let pid = children
        .0
        .first()
        .map(std::process::Child::id)
        .ok_or("the outsider never started")?;
    let exit = outer_cleanup::wait_by(&mut children.0[0], Instant::now() + Duration::from_secs(1));
    drop(children);
    // The kept sandbox is this test's own; the outsider is gone now.
    if let Some(root) = root {
        let _ = fs::remove_dir_all(root);
    }
    check(exit.is_none(), || {
        format!("the teardown signalled an outsider: {exit:?} {outcome:?}")
    })?;
    let error = outcome
        .err()
        .ok_or("a teardown that left an outsider passed")?
        .to_string();
    check(
        error.contains(&pid.to_string()) && error.contains("nothing signalled"),
        || format!("the uncertainty is not recorded: {error}"),
    )
}

/// Critical r1 #4, runtime §11.2: the survivor check after a teardown
/// keeps that teardown's deadline: once it has passed, a survivor is
/// reported uncertain at once, never signalled and given no new budget.
#[test]
fn s_launch_survivor_check_keeps_the_deadline() -> TestResult {
    let dir = tempfile::tempdir()?;
    let runtime = dir.path().join("runtime");
    let mut children = Children(Vec::new());
    let mut survivor = Command::new("sleep");
    survivor
        .arg("60")
        .env_clear()
        .env("VIA_RUNTIME_DIR", &runtime);
    children.0.push(survivor.spawn()?);
    let pid = children.0[0].id();
    let (_, start) = outer_cleanup::process_stat(pid).ok_or("no helper stat")?;
    let began = Instant::now();
    let report = survived(pid, start, began)?;
    let took = began.elapsed();
    let exit = outer_cleanup::wait_by(&mut children.0[0], Instant::now() + Duration::from_secs(1));
    check(exit.is_none(), || {
        format!("the survivor check signalled: {exit:?} {report:?}")
    })?;
    check(
        report
            .as_deref()
            .is_some_and(|report| report.contains("uncertain"))
            && took < Duration::from_millis(500),
        || format!("the expired survivor check: {report:?} after {took:?}"),
    )
}

/// Critical r2 N1, runtime §11.2: an observation finished after the
/// cutoff proves nothing. With one entry, so the delayed observation is
/// the scan's last, each path (a vanished process, an unmarked
/// environment, an unrelated command line, a marked process's exit)
/// reports uncertainty once its observation ends past the cutoff; on time,
/// the same entry is absence.
#[test]
fn s_launch_scan_rejects_a_late_final_observation() -> TestResult {
    let dir = tempfile::tempdir()?;
    let proc = dir.path().join("proc");
    fs::create_dir_all(proc.join("4242"))?;
    let (runtime, state) = (dir.path().join("runtime"), dir.path().join("state"));
    let marks = evidenced::Marks {
        runtime: &runtime,
        state: &state,
    };
    let marked = [
        b"VIA_RUNTIME_DIR=".as_slice(),
        runtime.as_os_str().as_encoded_bytes(),
    ]
    .concat();
    let late = Duration::from_millis(100);
    let pause = |delayed: bool| {
        if delayed {
            std::thread::sleep(late);
        }
    };
    // `delayed` makes the path's observation finish past the cutoff.
    let mut wrong = Vec::new();
    for path in ["vanished", "unmarked", "cmdline", "exited"] {
        for delayed in [false, true] {
            let read = |file: &Path, _: u64| -> std::io::Result<Vec<u8>> {
                let environ = file.ends_with("environ");
                match (path, environ) {
                    ("vanished", true) => {
                        pause(delayed);
                        Err(std::io::ErrorKind::NotFound.into())
                    }
                    ("unmarked", true) => {
                        pause(delayed);
                        Ok(b"HOME=/x\0".to_vec())
                    }
                    ("cmdline", true) => Err(std::io::ErrorKind::PermissionDenied.into()),
                    ("cmdline", false) => {
                        pause(delayed);
                        Ok(b"/bin/sleep\0".to_vec())
                    }
                    ("exited", true) => Ok([marked.as_slice(), b"\0"].concat()),
                    _ => Err(std::io::ErrorKind::NotFound.into()),
                }
            };
            let exited = |_: u32| -> Result<bool, String> {
                pause(delayed && path == "exited");
                Ok(true)
            };
            let cutoff = Instant::now() + Duration::from_millis(50);
            let scan = evidenced::scan_entries_by(&proc, marks, Some(cutoff), read, exited);
            if delayed != scan.is_err() || scan.as_ref().is_ok_and(|alive| !alive.is_empty()) {
                wrong.push(format!("{path}, delayed {delayed}: {scan:?}"));
            }
        }
    }
    check(wrong.is_empty(), || wrong.join("; "))
}

/// Critical r2 N2, runtime §11.2: no destructive step starts after the
/// fallback's cutoff. When observation consumed the fallback's allowance,
/// the retained child is not signalled, and the incomplete cleanup is
/// recorded.
#[test]
fn s_launch_owned_fallback_keeps_its_cutoff() -> TestResult {
    let mut children = Children(Vec::new());
    let mut helper = Command::new("sleep");
    helper.arg("60").env_clear();
    children.0.push(helper.spawn()?);
    let failure = kill_owned(&mut children.0[0], "observation was slow", Instant::now());
    let exit = outer_cleanup::wait_by(&mut children.0[0], Instant::now() + Duration::from_secs(1));
    check(exit.is_none(), || {
        format!("the retained child was signalled after the cutoff: {exit:?} {failure}")
    })?;
    check(failure.contains("nothing signalled"), || {
        format!("the incomplete cleanup is not recorded: {failure}")
    })
}

/// Critical r2 N3: an absence observed once the teardown's deadline has
/// passed does not prove exit by it: the survivor check reports it
/// uncertain.
#[test]
fn s_launch_survivor_check_rejects_a_late_absence() -> TestResult {
    let mut ended = Command::new("true").env_clear().spawn()?;
    let pid = ended.id();
    let reaped = outer_cleanup::wait_by(&mut ended, Instant::now() + outer_cleanup::REAP);
    check(reaped.is_some(), || format!("helper {pid} was not reaped"))?;
    let report = survived(pid, 0, Instant::now())?;
    check(
        report
            .as_deref()
            .is_some_and(|report| report.contains("uncertain")),
        || format!("a late absence was accepted: {report:?}"),
    )
}

/// Critical r2 N4: readiness keeps the startup deadline. A readiness
/// probe's exchange is bounded by the time left, so a socket that never
/// answers costs no more than that.
#[test]
fn s_launch_readiness_probe_keeps_its_deadline() -> TestResult {
    let dir = tempfile::tempdir()?;
    let _silent = std::os::unix::net::UnixListener::bind(dir.path().join("via.sock"))?;
    let began = Instant::now();
    let pid = daemon::serving_pid_by(dir.path(), began + Duration::from_millis(200));
    let took = began.elapsed();
    check(pid.is_none() && took < Duration::from_secs(1), || {
        format!("a silent socket's probe: {pid:?} after {took:?}")
    })
}

/// Critical r2 N4: the startup wait does not accept readiness once its
/// deadline has passed: the start is a typed timeout.
#[test]
fn s_launch_startup_wait_keeps_its_deadline() -> TestResult {
    evidenced(|| {
        let mut sandbox = Sandbox::new()?;
        let pid = sandbox.start_daemon()?;
        let late = sandbox.wait_serving(pid, Instant::now());
        check(
            late.as_ref().is_err_and(|error| {
                matches!(
                    error.downcast_ref::<ScenarioError>(),
                    Some(ScenarioError::Timeout(_))
                )
            }),
            || format!("readiness accepted after the startup deadline: {late:?}"),
        )
    })
}
