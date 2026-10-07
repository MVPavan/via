//! Recovery evidence through the real `via` binary, daemon and SQLite Store
//! (design T3 §9, §7.3, §11): a crash while a turn runs (F9), Host's two
//! positive cleanup paths after a crash (F22 a and b), the vendor
//! environment (F23), the durable `cancel.requested` and evidence folder
//! of a recovered turn, the restart handoff's corrupt-row
//! rule (O1.D8, §7.2 row 13), and restart with nondefault frozen values.
//! Waits are bounded waits on durable rows, fake gates, failpoint
//! acknowledgements or process exit; no sleep orders two events.

#[path = "support/daemon.rs"]
#[expect(
    dead_code,
    reason = "shared support; this file uses the direct status probe"
)]
mod daemon;
#[cfg(feature = "test-failpoints")]
#[path = "support/failpoints.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod failpoints;
#[cfg(feature = "test-failpoints")]
#[path = "support/hits.rs"]
mod hits;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/process.rs"]
mod process;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use daemon::collect_available;
use scenario::{Captured, ScenarioError, run_command, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[cfg(feature = "test-failpoints")]
const ACK_WAIT: Duration = Duration::from_secs(10);
/// A valid caller handle: `h_` and 43 base64url digits whose last is `A`.
const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
/// Values placed only in the daemon's environment (F23).
const SECRETS: [(&str, &str); 2] = [
    ("VIA_TEST_SECRET_TOKEN", "f23-daemon-only-secret"),
    ("ANTHROPIC_API_KEY", "f23-daemon-only-api-key"),
];

/// One scenario's private deployment.
struct Paths {
    _root: tempfile::TempDir,
    /// The scenario's one final teardown (runtime §11.2), which every
    /// daemon guard records into.
    teardown: outer_cleanup::Teardown,
    /// Daemon runs started so far, numbering their traces and reports.
    runs: std::sync::atomic::AtomicUsize,
    /// Command outputs that could not be written as evidence: reported at
    /// collection, beside the scenario's outcome, never in its place.
    lost_outputs: std::sync::Mutex<Vec<String>>,
    via: PathBuf,
    fake: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    sync: PathBuf,
    fixture: PathBuf,
    #[cfg(feature = "test-failpoints")]
    failpoints: failpoints::Failpoints,
}

impl Paths {
    fn new(fixture: &Value) -> TestResult<Self> {
        let via = PathBuf::from(env!("CARGO_BIN_EXE_via"));
        let fake = via
            .parent()
            .ok_or("via binary has no parent directory")?
            .join("via-fake-agent");
        if !fake.is_file() {
            return Err(
                format!("missing {}; build -p via-fake-agent first", fake.display()).into(),
            );
        }
        let root = tempfile::tempdir()?;
        let [state, runtime, sync] =
            ["state", "runtime", "sync"].map(|name| root.path().join(name));
        for path in [&state, &runtime, &sync] {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        let fixture_path = root.path().join("fixture.json");
        fs::write(&fixture_path, serde_json::to_vec(fixture)?)?;
        #[cfg(feature = "test-failpoints")]
        let failpoints = failpoints::Failpoints::new(root.path())?;
        Ok(Self {
            _root: root,
            teardown: outer_cleanup::Teardown::new(),
            runs: std::sync::atomic::AtomicUsize::new(0),
            lost_outputs: std::sync::Mutex::new(Vec::new()),
            via,
            fake,
            state,
            runtime,
            sync,
            fixture: fixture_path,
            #[cfg(feature = "test-failpoints")]
            failpoints,
        })
    }

    /// A client command: it never carries the failpoint activation inputs.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.via);
        command.env_clear();
        command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        command.env("VIA_STATE_DIR", &self.state);
        command.env("VIA_RUNTIME_DIR", &self.runtime);
        command.env("VIA_FAKE_AGENT_BINARY", &self.fake);
        command.env("VIA_FAKE_SCENARIO", &self.fixture);
        command.env("VIA_FAKE_SYNC_DIR", &self.sync);
        command
    }

    /// One CLI call, recorded as evidence.
    fn run(
        &self,
        evidence: &Evidence,
        name: &str,
        args: &[&str],
    ) -> Result<Captured, ScenarioError> {
        let mut command = self.command();
        command.args(args);
        let mut capture = run_command(&mut command, Duration::from_secs(20)).map_err(infra)?;
        // The captured outcome first: a lost output write is attached to
        // the capture and reported at collection, never in its place; the
        // caller classifies the exit (S1-evidence2 fix round 2 finding 5,
        // fix round 3).
        if let Err(error) = daemon::write_output(evidence, name, &capture) {
            capture.attached.push(error.detail().to_owned());
            self.lost_outputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(error.detail().to_owned());
        }
        if capture.timed_out {
            return Err(ScenarioError::Timeout(format!(
                "via {args:?} timed out{}",
                capture.notes()
            )));
        }
        Ok(capture)
    }

    /// Fails if any command output could not be written as evidence.
    fn outputs_written(&self) -> Result<(), ScenarioError> {
        let lost = self
            .lost_outputs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lost.is_empty() {
            Ok(())
        } else {
            Err(infra(format!(
                "command output not written: {}",
                lost.join("; ")
            )))
        }
    }

    /// One successful CLI call's JSON output.
    fn ok(&self, evidence: &Evidence, name: &str, args: &[&str]) -> Result<Value, ScenarioError> {
        let capture = self.run(evidence, name, args)?;
        let decoded = serde_json::from_slice::<Value>(&capture.stdout);
        check(
            daemon::cli_exit_matches(args, capture.status, decoded.as_ref().ok()),
            || {
                format!(
                    "via {args:?} exited {}: {}{}",
                    capture.status,
                    String::from_utf8_lossy(&capture.stderr),
                    capture.notes()
                )
            },
        )?;
        decoded.map_err(infra)
    }

    /// A read-only view of the committed rows the scenario asserts.
    fn store(&self) -> Result<rusqlite::Connection, ScenarioError> {
        let store = rusqlite::Connection::open_with_flags(
            self.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(infra)?;
        store.busy_timeout(Duration::from_secs(1)).map_err(infra)?;
        Ok(store)
    }

    /// A session's committed events, in sequence order; fails unless the
    /// sequence is dense from one.
    fn events(&self, session: &str) -> Result<Vec<Value>, ScenarioError> {
        let store = self.store()?;
        let mut query = store
            .prepare("SELECT seq,event FROM events WHERE session_id=?1 ORDER BY seq")
            .map_err(infra)?;
        let rows = query
            .query_map([session], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(infra)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(infra)?;
        let mut events = Vec::new();
        for (index, (seq, event)) in rows.into_iter().enumerate() {
            let event: Value = serde_json::from_str(&event).map_err(infra)?;
            check(
                i64::try_from(index + 1) == Ok(seq) && event["seq"] == seq,
                || format!("event sequence is not dense at {seq}: {event}"),
            )?;
            events.push(event);
        }
        Ok(events)
    }

    /// Event types of turn `n`, in sequence order.
    fn turn_types(&self, session: &str, n: u32) -> Result<Vec<String>, ScenarioError> {
        Ok(self
            .events(session)?
            .into_iter()
            .filter(|event| event["turn"] == n)
            .filter_map(|event| event["type"].as_str().map(str::to_owned))
            .collect())
    }

    /// Waits for turn `n`'s first durable event of `kind`.
    fn await_event(&self, session: &str, n: u32, kind: &str) -> Result<Value, ScenarioError> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(event) = self
                .events(session)?
                .into_iter()
                .find(|event| event["turn"] == n && event["type"] == kind)
            {
                return Ok(event);
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(format!(
                    "turn {n} never committed {kind}"
                )));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// A session's turn `n`: state and envelope.
    fn turn(&self, session: &str, n: u32) -> Result<(String, Value), ScenarioError> {
        let (state, envelope): (String, Option<String>) = self
            .store()?
            .query_row(
                "SELECT state,envelope FROM turns WHERE session_id=?1 AND number=?2",
                rusqlite::params![session, n],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(infra)?;
        let envelope =
            serde_json::from_str(envelope.as_deref().unwrap_or("null")).map_err(infra)?;
        Ok((state, envelope))
    }

    /// Committed anchors (launch attempts) of a session's turn `n`.
    fn anchors_of_turn(&self, session: &str, n: u32) -> Result<i64, ScenarioError> {
        self.store()?
            .query_row(
                "SELECT count(*) FROM anchors WHERE owner_session=?1 AND owner_turn=?2",
                rusqlite::params![session, n],
                |row| row.get(0),
            )
            .map_err(infra)
    }

    /// The session's only anchor: `(pid, pgid, marker, absence proved)`.
    fn anchor(&self, session: &str) -> Result<(u32, u32, String, bool), ScenarioError> {
        self.store()?
            .query_row(
                "SELECT pid,pgid,marker,absence_time IS NOT NULL FROM anchors WHERE owner_session=?1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(infra)
    }

    /// Waits until the fake created `name` in its sync dir.
    fn await_file(&self, name: &str) -> Result<(), ScenarioError> {
        let path = self.sync.join(name);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(format!(
                    "the fake never wrote {name}"
                )));
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// A pid the fake reported in its sync dir.
    fn pid(&self, name: &str) -> Result<u32, ScenarioError> {
        self.await_file(name)?;
        fs::read_to_string(self.sync.join(name))
            .map_err(infra)?
            .trim()
            .parse()
            .map_err(infra)
    }

    /// Writes every committed envelope and event, read-only, as evidence.
    fn write_store_evidence(&self, evidence: &Evidence) -> Result<(), ScenarioError> {
        let store = self.store()?;
        for (name, sql) in [
            (
                "envelopes.ndjson",
                "SELECT envelope FROM turns WHERE envelope IS NOT NULL ORDER BY session_id, number",
            ),
            (
                "events.ndjson",
                "SELECT event FROM events ORDER BY session_id, seq",
            ),
        ] {
            let mut query = store.prepare(sql).map_err(infra)?;
            let mut lines = Vec::new();
            for line in query
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(infra)?
            {
                lines.extend_from_slice(line.map_err(infra)?.as_bytes());
                lines.push(b'\n');
            }
            evidence.write(name, &lines).map_err(infra)?;
        }
        Ok(())
    }
}

/// Directly owned daemon child of one run. Its drop records the run's
/// teardown; the scenario's `cleanup.json` covers every run, crashed ones
/// included ([`daemon::collect_available`]).
struct Daemon<'a> {
    child: Child,
    paths: &'a Paths,
    /// `<n>-<run>`: the run's name in the teardown.
    run: String,
    /// The run's own cleanup report, `cleanup-<n>-<run>.json`.
    report: PathBuf,
    crash_snapshot: Option<Vec<outer_cleanup::AnchorRow>>,
    /// Set once the run was torn down: its drop then does nothing.
    torn_down: bool,
}

impl<'a> Daemon<'a> {
    /// Starts a daemon and waits until it admits requests; `run` names its
    /// trace and cleanup evidence.
    fn start(paths: &'a Paths, evidence: &Evidence, run: &str) -> Result<Self, ScenarioError> {
        Self::start_with(paths, evidence, run, &[])
    }

    /// `start` with extra daemon-only environment.
    fn start_with(
        paths: &'a Paths,
        evidence: &Evidence,
        run: &str,
        env: &[(&str, &str)],
    ) -> Result<Self, ScenarioError> {
        let mut daemon = Self::spawn(paths, evidence, run, env)?;
        daemon.wait_ready()?;
        Ok(daemon)
    }

    fn spawn(
        paths: &'a Paths,
        evidence: &Evidence,
        run: &str,
        env: &[(&str, &str)],
    ) -> Result<Self, ScenarioError> {
        if paths.teardown.begun() {
            return Err(infra("a daemon started after the final teardown began"));
        }
        // Every run appends to its trace under its own header, never
        // truncating an earlier run's (S1-evidence2 fix round 2, finding 1).
        let trace = if run == "final" {
            evidence.dir.join("daemon.trace")
        } else {
            evidence.dir.join(format!("daemon-{run}.trace"))
        };
        let number = paths
            .runs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let run = format!("{number}-{run}");
        let mut trace = File::options()
            .create(true)
            .append(true)
            .open(&trace)
            .map_err(infra)?;
        writeln!(trace, "=== daemon run {run} ===").map_err(infra)?;
        let mut command = paths.command();
        #[cfg(feature = "test-failpoints")]
        paths.failpoints.activate(&mut command);
        for (key, value) in env {
            command.env(key, value);
        }
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(trace);
        Ok(Self {
            child: command.spawn().map_err(infra)?,
            paths,
            report: evidence.dir.join(format!("cleanup-{run}.json")),
            run,
            crash_snapshot: None,
            torn_down: false,
        })
    }

    /// Waits until the daemon answers `daemon status`, i.e. admits requests.
    fn wait_ready(&mut self) -> Result<(), ScenarioError> {
        let paths = self.paths;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().map_err(infra)? {
                return Err(fail(&format!("daemon exited before readiness: {status}")));
            }
            // A direct probe: never auto-starts a second daemon, even over
            // the stale socket file a killed daemon left.
            let ready = daemon::serving_pid(&paths.runtime) == Some(self.child.id());
            if Instant::now() > deadline {
                return Err(ScenarioError::Timeout("daemon readiness".to_owned()));
            }
            if ready {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Kills the daemon with SIGKILL, as a crash would, and reaps it. Keep the
    /// value alive until the scenario ends: its drop runs the outer cleanup,
    /// which would otherwise stop a surviving anchor before the restart.
    fn kill(&mut self) -> Result<(), ScenarioError> {
        self.crash_snapshot = Some(
            outer_cleanup::snapshot(
                &self.paths.state.join("store.sqlite3"),
                Instant::now() + outer_cleanup::TEARDOWN,
            )
            .map_err(infra)?,
        );
        self.child.kill().map_err(infra)?;
        if !outer_cleanup::reap_by(&mut self.child, Instant::now() + outer_cleanup::REAP) {
            return Err(ScenarioError::Timeout(
                "the killed daemon was not reaped in 1 s".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Drop for Daemon<'_> {
    /// The scenario's final teardown of this run (runtime §11.2): a
    /// daemon's drop begins, or joins, the scenario's one teardown deadline
    /// (`Paths::teardown`), which bounds the force-stop (at most 2 s), the
    /// exit wait, the kill's 1 s reap and the anchor cleanup. An exited
    /// daemon's drop begins or joins it too; a deliberate stop before a
    /// restart is `Daemon::shutdown`, with its own bound. The run is recorded in
    /// `cleanup-<n>-<run>.json` and the teardown, which
    /// [`daemon::collect_available`] validates as a whole.
    fn drop(&mut self) {
        if self.torn_down {
            return;
        }
        // Final, live or exited: it begins or joins the scenario's one
        // deadline (S1-evidence2 fix round 3, Sol r3 finding 1). Only an
        // explicit `shutdown()` has its own bound.
        let deadline = self.paths.teardown.begin();
        self.tear_down(deadline);
    }
}

#[cfg(feature = "test-failpoints")]
impl Daemon<'_> {
    /// A deliberate intermediate shutdown before a restart, with its own
    /// runtime §11.2 bound: the final teardown has not begun, so the next
    /// run may start. Recorded like the final one; a run that needed a
    /// kill is a timeout, any other cleanup failure an infrastructure
    /// failure.
    fn shutdown(mut self) -> Result<(), ScenarioError> {
        let record = self.tear_down(Instant::now() + outer_cleanup::TEARDOWN);
        if record["direct_child"]["kill"] != "not_needed" {
            return Err(ScenarioError::Timeout(format!(
                "the daemon did not exit after its force-stop: {record}"
            )));
        }
        if record["failures"]
            .as_array()
            .is_some_and(|failures| !failures.is_empty())
        {
            return Err(infra(format!("intermediate shutdown incomplete: {record}")));
        }
        Ok(())
    }
}

impl Daemon<'_> {
    /// Tears this run down by `deadline` and records it.
    fn tear_down(&mut self, deadline: Instant) -> Value {
        self.torn_down = true;
        let paths = self.paths;
        let rows = self.crash_snapshot.take();
        paths.teardown.daemon_generation(
            &self.run,
            deadline,
            &mut self.child,
            Some((&paths.state.join("store.sqlite3"), rows)),
            Some(&self.report),
            |by| {
                outer_cleanup::run_within(
                    paths
                        .command()
                        .args(["daemon", "stop", "--force", "--json"]),
                    by,
                )
            },
        )
    }
}

#[cfg(feature = "test-failpoints")]
/// Waits for `child` to exit; `None` when it is still running at `within`.
/// Each observation is timestamped after it returns: an exit observed only
/// after the deadline is `None`.
fn wait_child(
    child: &mut Child,
    within: Duration,
) -> Result<Option<std::process::ExitStatus>, ScenarioError> {
    let deadline = Instant::now() + within;
    loop {
        let status = child.try_wait().map_err(infra)?;
        if Instant::now() > deadline {
            return Ok(None);
        }
        if status.is_some() {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn infra(error: impl std::fmt::Display) -> ScenarioError {
    ScenarioError::Infrastructure(error.to_string())
}

fn fail(detail: &str) -> ScenarioError {
    ScenarioError::Failure(detail.to_owned())
}

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(ScenarioError::Failure(detail()))
    }
}

fn scenario(
    name: &str,
    fixture: &Value,
    action: impl FnOnce(&Paths, &Evidence) -> Result<(), ScenarioError>,
) -> TestResult {
    let paths = Paths::new(fixture)?;
    let evidence = Evidence::new(name, &paths.fake, &paths.fixture)?;
    run_scenario(
        evidence,
        |evidence| action(&paths, evidence),
        // Every collection step runs; their failures are reported together.
        |evidence| {
            let failures: Vec<String> = [
                paths.write_store_evidence(evidence),
                collect_available(evidence, &paths.state, &paths.teardown),
                paths.outputs_written(),
            ]
            .into_iter()
            .filter_map(Result::err)
            .map(|error| error.detail().to_owned())
            .collect();
            if failures.is_empty() {
                Ok(())
            } else {
                Err(infra(failures.join("; ")))
            }
        },
    )
    .require_pass()
}

fn accepted(turn: u32) -> Value {
    json!({"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":format!("fake-turn-{turn}")}})
}

fn text(turn: u32, text: &str) -> Value {
    json!({"action":"emit","message":{"type":"text","vendor_turn_id":format!("fake-turn-{turn}"),"text":text}})
}

fn terminal(turn: u32, status: &str, stop_reason: &str) -> Value {
    json!({"action":"emit","message":{"type":"terminal","vendor_turn_id":format!("fake-turn-{turn}"),
        "status":status,"final_text":"done","stop_reason":stop_reason}})
}

fn step(action: &str) -> Value {
    json!({ "action": action })
}

fn gate(name: &str) -> Value {
    json!({"action":"gate","name":name})
}

fn script(prompt: &str, turn: u32, steps: Vec<Value>) -> Value {
    let mut script = json!({"expected_request":{"type":"start","turn":turn,"prompt":prompt}});
    script["steps"] = Value::Array(steps);
    script
}

fn completes(prompt: &str, turn: u32) -> Value {
    script(
        prompt,
        turn,
        vec![accepted(turn), terminal(turn, "completed", "end_turn")],
    )
}

/// A running turn that never ends by itself: acceptance, output, a
/// grandchild and the pids the harness checks, then a hang.
fn hanging(prompt: &str) -> Value {
    script(
        prompt,
        1,
        vec![
            accepted(1),
            text(1, "partial output"),
            json!({"action":"spawn_grandchild","name":"gc"}),
            step("report_pids"),
            step("hang"),
        ],
    )
}

/// `via spawn --background` with the scenario handle; returns the receipt.
fn spawn(
    paths: &Paths,
    evidence: &Evidence,
    name: &str,
    prompt: &str,
    extra: &[&str],
) -> Result<Value, ScenarioError> {
    let mut args = vec![
        "spawn",
        "--harness",
        "fake",
        "--model",
        "fake",
        "--prompt",
        prompt,
        "--handle",
        HANDLE,
        "--background",
        "--json",
    ];
    args.extend_from_slice(extra);
    paths.ok(evidence, name, &args)
}

fn session_of(receipt: &Value) -> Result<String, ScenarioError> {
    receipt["session_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| fail(&format!("receipt has no session: {receipt}")))
}

/// `via resume` with the scenario handle; returns the receipt.
fn resume(
    paths: &Paths,
    evidence: &Evidence,
    name: &str,
    session: &str,
    prompt: &str,
    extra: &[&str],
) -> Result<Value, ScenarioError> {
    let mut args = vec![
        "resume", session, "--prompt", prompt, "--handle", HANDLE, "--json",
    ];
    args.extend_from_slice(extra);
    paths.ok(evidence, name, &args)
}

/// Waits (bounded) for a turn's durable envelope through `via wait`.
fn wait(paths: &Paths, evidence: &Evidence, address: &str) -> Result<Value, ScenarioError> {
    let name = format!("wait-{}", address.replace('/', "-"));
    paths.ok(
        evidence,
        &name,
        &["wait", address, "--timeout-ms", "15000", "--json"],
    )
}

/// `pid` names a live, non-zombie process.
/// Unreadable process state is uncertainty, never absence (S1-evidence2
/// fix round 2, finding 13).
fn process_live(pid: u32) -> Result<bool, ScenarioError> {
    Ok(!process::exited(pid).map_err(infra)?)
}

/// The harness's own non-signalling query: only `ESRCH` proves the group gone.
fn group_absent(pgid: u32) -> bool {
    i32::try_from(pgid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|pgid| {
            rustix::process::test_kill_process_group(pgid) == Err(rustix::io::Errno::SRCH)
        })
}

/// Waits until every pid is gone and `pgid` answers `ESRCH`, observed by
/// the deadline: an absence observed only after it is not accepted.
fn await_gone(pids: &[u32], pgid: u32) -> Result<(), ScenarioError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut live = false;
        for pid in pids {
            live |= process_live(*pid)?;
        }
        let gone = !live && group_absent(pgid);
        if Instant::now() > deadline {
            return Err(fail(&format!(
                "processes {pids:?} or group {pgid} survived the cleanup"
            )));
        }
        if gone {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Warning codes of an envelope.
fn warnings(envelope: &Value) -> Vec<String> {
    envelope["warnings"]
        .as_array()
        .map(|warnings| {
            warnings
                .iter()
                .filter_map(|warning| warning["code"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// C1 §7.5: the restarted turn 1 is `unknown` with `daemon_restart`, its
/// recovery settlement last after its one `cancel.requested`, and it was
/// submitted exactly once.
fn recovered_unknown(paths: &Paths, session: &str) -> Result<Value, ScenarioError> {
    let (state, envelope) = paths.turn(session, 1)?;
    check(
        state == "unknown"
            && envelope["state"] == "unknown"
            && envelope["failure"]["class"] == "daemon_restart",
        || format!("turn 1 after restart: {state} {envelope}"),
    )?;
    let types = paths.turn_types(session, 1)?;
    let count = |kind: &str| types.iter().filter(|seen| *seen == kind).count();
    let requested = types.iter().position(|seen| seen == "cancel.requested");
    check(
        types.ends_with(&["cancel.settled".to_owned(), "turn.ended".to_owned()])
            && count("cancel.requested") == 1
            && requested < Some(types.len() - 2)
            && count("turn.submitted") == 1,
        || format!("turn 1 recovery history: {types:?}"),
    )?;
    Ok(envelope)
}

/// Task 4 design §7.5, §9: a recovered turn's envelope names its evidence
/// folder, computed from its address, and only the plan's warning.
fn recovered_evidence(session: &str, envelope: &Value) -> Result<(), ScenarioError> {
    let folder = envelope["evidence"]["folder"].as_str().unwrap_or_default();
    check(
        Path::new(folder).is_absolute()
            && folder.ends_with(&format!("evidence/{session}/1"))
            && envelope["evidence"]["transcript"].is_null()
            && warnings(envelope) == ["vendor_version_untested"],
        || format!("recovered envelope evidence: {envelope}"),
    )
}

// ------------------------------------------------------------------ F9

/// F9 (design §11), a characterization test. The acceptance committed and
/// text streamed, then SIGKILL: after restart turn 1 is `unknown`, the fake received exactly
/// one start, the queued successor is cancelled, and the envelope names the
/// turn's evidence folder and only the plan's warning (Task 4 design §7.5).
#[test]
fn s1_f09_kill_while_running_restarts_unknown_no_resend() -> TestResult {
    let fixture = json!({"scripts":[
        script("f09", 1, vec![accepted(1), text(1, "streamed"), gate("streamed"),
            terminal(1, "completed", "end_turn")]),
        completes("f09-next", 2),
    ]});
    scenario("s1_f09_kill_while_running", &fixture, |paths, evidence| {
        let mut daemon = Daemon::start(paths, evidence, "crashed")?;
        let session = session_of(&spawn(paths, evidence, "spawn", "f09", &[])?)?;
        paths.await_file("streamed.entered")?;
        // Model text is not an event (Task 4 design §2.1): the acceptance
        // is the turn's last committed event before the kill.
        paths.await_event(&session, 1, "turn.started")?;
        resume(paths, evidence, "resume", &session, "f09-next", &[])?;
        daemon.kill()?;
        let _daemon = Daemon::start(paths, evidence, "final")?;
        let envelope = recovered_unknown(paths, &session)?;
        check(
            paths.anchors_of_turn(&session, 1)? == 1 && paths.anchors_of_turn(&session, 2)? == 0,
            || "a turn launched again after the restart".to_owned(),
        )?;
        let (state, second) = paths.turn(&session, 2)?;
        check(
            state == "cancelled" && second["timestamps"]["submitted_at"].is_null(),
            || format!("queued successor after restart: {state} {second}"),
        )?;
        recovered_evidence(&session, &envelope)
    })
}

// ------------------------------------------------------------------ F22

/// F22 (a), characterization (design §11): after a SIGKILL the anchor's
/// control EOF starts its own-group cleanup; the harness sees the vendor,
/// grandchild and group gone before restart. Restart proves absence
/// (`quiescent`) without claiming a Host stop (`requested`, not `forced`).
#[test]
fn s1_f22_autonomous_eof_cleanup_proved_on_restart() -> TestResult {
    scenario(
        "s1_f22_autonomous_eof",
        &hanging("f22a"),
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let session = session_of(&spawn(paths, evidence, "spawn", "f22a", &[])?)?;
            let agent = paths.pid("agent.pid")?;
            let grandchild = paths.pid("gc.pid")?;
            paths.await_event(&session, 1, "turn.started")?;
            let (anchor, pgid, _, _) = paths.anchor(&session)?;
            daemon.kill()?;
            await_gone(&[anchor, agent, grandchild], pgid)?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let envelope = recovered_unknown(paths, &session)?;
            check(
                envelope["cancel"]["outcome"] == "requested"
                    && envelope["cancel"]["cleanup"] == "quiescent"
                    && paths.anchor(&session)?.3,
                || format!("autonomous cleanup not proved on restart: {envelope}"),
            )
        },
    )
}

/// F22 (b) (design §9, §11; runtime §5.2): a barrier-held anchor survives
/// the crash (`host.anchor.defer_cleanup` defers its EOF cleanup, and its
/// acknowledgement is the anchor's). Restart verifies the live anchor,
/// stops its group through it (`forced`), and proves `ESRCH`
/// (`quiescent`); the harness sees vendor and grandchild gone. It passes
/// on the base: a proof of existing Host recovery, not a regression.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f22_surviving_anchor_verified_and_stopped_on_restart() -> TestResult {
    scenario(
        "s1_f22_surviving_anchor",
        &hanging("f22b"),
        |paths, evidence| {
            let point = "host.anchor.defer_cleanup";
            paths.failpoints.arm(point, 1, "fail_io").map_err(infra)?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let session = session_of(&spawn(paths, evidence, "spawn", "f22b", &[])?)?;
            let agent = paths.pid("agent.pid")?;
            let grandchild = paths.pid("gc.pid")?;
            paths.await_event(&session, 1, "turn.started")?;
            let (anchor, pgid, _, _) = paths.anchor(&session)?;
            daemon.kill()?;
            paths
                .failpoints
                .wait_ack(point, 1, "fail_io", anchor, ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {point}: {error}")))?;
            let ack = paths.failpoints.ack_bytes(point, 1).map_err(infra)?;
            evidence
                .write(&format!("{point}.ack.json"), &ack)
                .map_err(infra)?;
            let live = [
                process_live(anchor)?,
                process_live(agent)?,
                process_live(grandchild)?,
            ];
            check(live == [true; 3] && !group_absent(pgid), || {
                format!(
                    "the held group did not survive the crash: anchor, agent, grandchild {live:?} group {}",
                    !group_absent(pgid)
                )
            })?;
            paths.failpoints.disarm(point).map_err(infra)?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let envelope = recovered_unknown(paths, &session)?;
            check(
                envelope["cancel"]["outcome"] == "forced"
                    && envelope["cancel"]["cleanup"] == "quiescent"
                    && paths.anchor(&session)?.3,
                || format!("the surviving anchor was not stopped and proved: {envelope}"),
            )?;
            await_gone(&[anchor, agent, grandchild], pgid)
        },
    )
}

// ------------------------------------------------------------------ F23

/// F23 (design §9 [r1.17]): the vendor sees exactly the fake adapter's
/// allow list plus Host's own `VIA_PROCESS_MARKER`, whose value is not the
/// anchor's private marker; no daemon-only secret reaches it. It passes
/// on the base: a proof of existing Host behaviour, not a regression.
#[test]
fn s1_f23_agent_sees_only_allow_listed_env() -> TestResult {
    let fixture = script(
        "f23",
        1,
        vec![
            step("dump_environment"),
            accepted(1),
            terminal(1, "completed", "end_turn"),
        ],
    );
    scenario("s1_f23_vendor_environment", &fixture, |paths, evidence| {
        let _daemon = Daemon::start_with(paths, evidence, "final", &SECRETS)?;
        let session = session_of(&spawn(paths, evidence, "spawn", "f23", &[])?)?;
        let envelope = wait(paths, evidence, &format!("{session}/1"))?;
        check(envelope["state"] == "completed", || envelope.to_string())?;
        let environment: BTreeMap<String, String> =
            serde_json::from_slice(&fs::read(paths.sync.join("environment.json")).map_err(infra)?)
                .map_err(infra)?;
        let keys: Vec<&str> = environment.keys().map(String::as_str).collect();
        check(
            keys == [
                "VIA_FAKE_SCENARIO",
                "VIA_FAKE_SYNC_DIR",
                "VIA_PROCESS_MARKER",
            ],
            || format!("vendor environment keys: {keys:?}"),
        )?;
        let fixture_path = paths.fixture.to_string_lossy();
        let sync_path = paths.sync.to_string_lossy();
        check(
            environment["VIA_FAKE_SCENARIO"] == fixture_path
                && environment["VIA_FAKE_SYNC_DIR"] == sync_path,
            || "the allow-listed values were altered".to_owned(),
        )?;
        let (_, _, anchor_marker, _) = paths.anchor(&session)?;
        let marker = &environment["VIA_PROCESS_MARKER"];
        check(
            !marker.is_empty()
                && *marker != anchor_marker
                && !environment
                    .values()
                    .any(|value| value.contains(&anchor_marker)),
            || "the vendor marker is empty or is the anchor's private marker".to_owned(),
        )?;
        check(
            !environment
                .values()
                .any(|value| SECRETS.iter().any(|(_, secret)| value.contains(secret))),
            || "a daemon-only secret reached the vendor".to_owned(),
        )
    })
}

// ------------------------------------------------- recovered cancel.requested

/// Design §9: a caller's `cancel.requested` committed before the crash is
/// kept: recovery commits no second one, and the recovered envelope's
/// `requested_at` is that event's `at`.
#[test]
fn s1_recovery_keeps_the_durable_cancel_requested() -> TestResult {
    scenario(
        "s1_recovery_durable_cancel_requested",
        &hanging("ordered"),
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let session = session_of(&spawn(paths, evidence, "spawn", "ordered", &[])?)?;
            paths.await_event(&session, 1, "turn.started")?;
            paths.ok(
                evidence,
                "cancel",
                &[
                    "cancel",
                    &session,
                    "--force-after",
                    "60000",
                    "--handle",
                    HANDLE,
                    "--json",
                ],
            )?;
            let requested = paths.await_event(&session, 1, "cancel.requested")?;
            daemon.kill()?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let envelope = recovered_unknown(paths, &session)?;
            let types = paths.turn_types(&session, 1)?;
            let orders = types
                .iter()
                .filter(|kind| *kind == "cancel.requested")
                .count();
            check(orders == 1, || {
                format!("{orders} cancel.requested events: {types:?}")
            })?;
            check(
                envelope["cancel"]["requested_at"] == requested["at"],
                || {
                    format!(
                        "requested_at not kept: {} vs {requested}",
                        envelope["cancel"]
                    )
                },
            )
        },
    )
}

/// Design §9: a recovery that commits `cancel.settled` but not its terminal
/// (`store.commit.terminal` fails the write, so startup fails with the same
/// durable state a crash there leaves) is completed by the next recovery
/// with exactly one `cancel.requested` and one `cancel.settled`; the
/// terminal's `cancel` is the durable one: `requested_at` and `settled_at`
/// are those events' `at`, and `outcome` and `cleanup` those settled.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_recovery_keeps_the_durable_cancel_settled() -> TestResult {
    scenario(
        "s1_recovery_durable_cancel_settled",
        &hanging("settled"),
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let session = session_of(&spawn(paths, evidence, "spawn", "settled", &[])?)?;
            paths.await_event(&session, 1, "turn.started")?;
            daemon.kill()?;
            let point = "store.commit.terminal";
            paths.failpoints.arm(point, 1, "fail_io").map_err(infra)?;
            let mut refused = Daemon::spawn(paths, evidence, "refused", &[])?;
            let status = wait_child(&mut refused.child, Duration::from_secs(15))?
                .ok_or_else(|| fail("the daemon admitted after a failed recovery terminal"))?;
            let trace =
                fs::read_to_string(evidence.dir.join("daemon-refused.trace")).map_err(infra)?;
            check(!status.success() && trace.contains("store_error"), || {
                format!("startup did not fail on the terminal write ({status}): {trace}")
            })?;
            refused.shutdown()?;
            let (state, _) = paths.turn(&session, 1)?;
            let types = paths.turn_types(&session, 1)?;
            check(
                state == "running" && types.last().map(String::as_str) == Some("cancel.settled"),
                || format!("before the second recovery: {state} {types:?}"),
            )?;
            let events = paths.events(&session)?;
            let durable = |kind: &str| {
                events
                    .iter()
                    .find(|event| event["turn"] == 1 && event["type"] == kind)
                    .cloned()
                    .ok_or_else(|| fail(&format!("no durable {kind}")))
            };
            let requested = durable("cancel.requested")?;
            let settled = durable("cancel.settled")?;
            paths.failpoints.disarm(point).map_err(infra)?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            let envelope = recovered_unknown(paths, &session)?;
            let types = paths.turn_types(&session, 1)?;
            let settlements = types
                .iter()
                .filter(|kind| *kind == "cancel.settled")
                .count();
            check(settlements == 1, || {
                format!("{settlements} cancel.settled events: {types:?}")
            })?;
            let cancel = &envelope["cancel"];
            check(
                cancel["requested_at"] == requested["at"]
                    && cancel["settled_at"] == settled["at"]
                    && cancel["outcome"] == settled["outcome"]
                    && cancel["cleanup"] == settled["cleanup"],
                || format!("the durable cancel was not kept: {cancel} vs {requested} {settled}"),
            )
        },
    )
}

// ------------------------------------------------- recovery without an armed anchor

/// Design §9: a recovered turn whose anchor never reached `arm_intent` (its
/// ARM intent commit is held by `store.journal.arm_intent`), and one with no
/// anchor at all (held right after submission intent), are `unknown` with
/// their evidence folder named and only the plan's warning.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_recovery_without_armed_anchor_names_its_evidence_folder() -> TestResult {
    let fixture = json!({"scripts":[completes("pre-arm", 1), completes("no-anchor", 1)]});
    scenario(
        "s1_recovery_unarmed_raw_complete",
        &fixture,
        |paths, evidence| {
            let intent = "core.intent.after_commit";
            paths.failpoints.arm(intent, 1, "pause").map_err(infra)?;
            let arm = "store.journal.arm_intent";
            paths.failpoints.arm(arm, 1, "pause").map_err(infra)?;
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let pid = daemon.child.id();
            let no_anchor = session_of(&spawn(paths, evidence, "spawn-a", "no-anchor", &[])?)?;
            paths
                .failpoints
                .wait_ack(intent, 1, "pause", pid, ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {intent}: {error}")))?;
            let pre_arm = session_of(&spawn(paths, evidence, "spawn-b", "pre-arm", &[])?)?;
            paths
                .failpoints
                .wait_ack(arm, 1, "pause", pid, ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {arm}: {error}")))?;
            let phase: String = paths
                .store()?
                .query_row(
                    "SELECT phase FROM anchors WHERE owner_session=?1",
                    [&pre_arm],
                    |row| row.get(0),
                )
                .map_err(infra)?;
            check(phase == "identified", || format!("anchor phase {phase}"))?;
            check(paths.anchors_of_turn(&no_anchor, 1)? == 0, || {
                "the held submission launched".to_owned()
            })?;
            daemon.kill()?;
            paths.failpoints.disarm(intent).map_err(infra)?;
            paths.failpoints.disarm(arm).map_err(infra)?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            // The restarted daemon's own turn, for the evidence set.
            let after = session_of(&spawn(paths, evidence, "spawn-c", "pre-arm", &[])?)?;
            wait(paths, evidence, &format!("{after}/1"))?;
            for session in [&no_anchor, &pre_arm] {
                let envelope = recovered_unknown(paths, session)?;
                recovered_evidence(session, &envelope)?;
            }
            Ok(())
        },
    )
}

// ------------------------------------------------- restart handoff: queued turns, corrupt rows (O1.D8)

/// Crashes a daemon with turn 1 completed and turns 2.. of `prompts` durably
/// `queued`: the dispatcher is held at `core.dispatch.before_grant` for
/// turn 2 while the later receipts commit. Returns the session.
#[cfg(feature = "test-failpoints")]
fn crash_with_queued_turns(
    paths: &Paths,
    evidence: &Evidence,
    prompts: &[&str],
    spawn_extra: &[&str],
    resume_extra: &[&str],
) -> Result<(String, Vec<Value>), ScenarioError> {
    let point = "core.dispatch.before_grant";
    paths.failpoints.arm(point, 2, "pause").map_err(infra)?;
    let mut daemon = Daemon::start(paths, evidence, "crashed")?;
    let (first, rest) = prompts.split_first().ok_or_else(|| infra("no prompts"))?;
    let session = session_of(&spawn(paths, evidence, "spawn", first, spawn_extra)?)?;
    let envelope = wait(paths, evidence, &format!("{session}/1"))?;
    check(envelope["state"] == "completed", || {
        format!("turn 1: {envelope}")
    })?;
    let mut receipts = Vec::new();
    for (index, prompt) in rest.iter().enumerate() {
        let name = format!("resume-{}", index + 2);
        receipts.push(resume(
            paths,
            evidence,
            &name,
            &session,
            prompt,
            resume_extra,
        )?);
        if index == 0 {
            paths
                .failpoints
                .wait_ack(point, 2, "pause", daemon.child.id(), ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {point}: {error}")))?;
        }
    }
    for n in 2..=u32::try_from(prompts.len()).map_err(infra)? {
        let (state, _) = paths.turn(&session, n)?;
        check(state == "queued", || {
            format!("turn {n} before the crash: {state}")
        })?;
    }
    daemon.kill()?;
    paths.failpoints.disarm(point).map_err(infra)?;
    // The caller restarts: an explicit intermediate shutdown of the killed
    // run (S1-evidence2 fix round 3).
    daemon.shutdown()?;
    Ok((session, receipts))
}

/// Writes `effective` into a queued turn's frozen row while no daemon runs.
#[cfg(feature = "test-failpoints")]
fn corrupt_effective(
    paths: &Paths,
    session: &str,
    n: u32,
    effective: &str,
) -> Result<(), ScenarioError> {
    let changed = rusqlite::Connection::open(paths.state.join("store.sqlite3"))
        .map_err(infra)?
        .execute(
            "UPDATE turns SET effective=?3 WHERE session_id=?1 AND number=?2 AND state='queued'",
            rusqlite::params![session, n, effective],
        )
        .map_err(infra)?;
    check(changed == 1, || format!("turn {n} was not queued"))
}

/// Design §7.2 row 2 shape for a handoff turn failed on a corrupt row: no
/// launch, `failed(store)` with `submitted_at` and `cancel: null`, and only
/// `turn.queued`, `turn.submitted` and `turn.ended` in its history.
#[cfg(feature = "test-failpoints")]
fn failed_on_corrupt_row(paths: &Paths, session: &str, n: u32) -> Result<(), ScenarioError> {
    let (state, envelope) = paths.turn(session, n)?;
    check(
        state == "failed"
            && envelope["state"] == "failed"
            && envelope["failure"]["class"] == "store"
            && envelope["cancel"].is_null()
            && envelope["timestamps"]["submitted_at"].is_string(),
        || format!("turn {n} after restart: {state} {envelope}"),
    )?;
    let types = paths.turn_types(session, n)?;
    check(
        types == ["turn.queued", "turn.submitted", "turn.ended"],
        || format!("turn {n} history: {types:?}"),
    )?;
    check(paths.anchors_of_turn(session, n)? == 0, || {
        format!("turn {n} launched")
    })
}

/// O1.D8, restart half (design §7.3): in the restart handoff a frozen value
/// that is present but unparseable fails that turn `failed(store)` through
/// `commit_submit_failed`, without agent I/O. Turn 2's `effective` is not
/// JSON (Store cannot read the row); turn 3's is JSON Core cannot parse.
/// The restarted daemon admits, and turn 4 runs. Each corrupt row reaches
/// the failure record before admission (design §7.5; T3-S5 round 1,
/// decision 5): `store_failure` is `corrupt_row` for turn 3, scope `turn`,
/// count 2, while `health` stays `healthy`.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f12_corrupt_frozen_row_fails_turn_on_restart() -> TestResult {
    let fixture = json!({"scripts":[completes("r1", 1), completes("r4", 4)]});
    scenario("s1_f12_corrupt_frozen_row", &fixture, |paths, evidence| {
        let (session, _) =
            crash_with_queued_turns(paths, evidence, &["r1", "r2", "r3", "r4"], &[], &[])?;
        corrupt_effective(paths, &session, 2, "not json {")?;
        corrupt_effective(paths, &session, 3, r#"{"deadlines":"unparseable"}"#)?;
        let _daemon = Daemon::start(paths, evidence, "final")?;
        failed_on_corrupt_row(paths, &session, 2)?;
        failed_on_corrupt_row(paths, &session, 3)?;
        let status = paths.ok(evidence, "status", &["daemon", "status", "--json"])?;
        let failure = &status["store_failure"];
        check(
            status["health"] == "healthy"
                && failure["kind"] == "corrupt_row"
                && failure["scope"] == "turn"
                && failure["count"] == 2
                && failure["affected"]["addresses"] == json!([format!("{session}/3")]),
            || format!("the handoff's corrupt rows were not recorded: {status}"),
        )?;
        let fourth = wait(paths, evidence, &format!("{session}/4"))?;
        check(fourth["state"] == "completed", || {
            format!("turn 4 after the corrupt rows: {fourth}")
        })?;
        check(paths.anchors_of_turn(&session, 4)? == 1, || {
            "turn 4 did not launch exactly once".to_owned()
        })
    })
}

/// Design §7.2 row 13: when the handoff's `commit_submit_failed` for a
/// corrupt row does not commit (`store.commit.terminal`), startup fails and
/// nothing is admitted; the turn stays `queued`. A later start commits it.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_recovery_corrupt_row_write_failure_fails_startup() -> TestResult {
    let fixture = json!({"scripts":[completes("w1", 1), completes("w3", 3)]});
    scenario(
        "s1_recovery_corrupt_row_write",
        &fixture,
        |paths, evidence| {
            let (session, _) =
                crash_with_queued_turns(paths, evidence, &["w1", "w2", "w3"], &[], &[])?;
            corrupt_effective(paths, &session, 2, "not json {")?;
            let point = "store.commit.terminal";
            paths.failpoints.arm(point, 1, "fail_io").map_err(infra)?;
            let mut refused = Daemon::spawn(paths, evidence, "refused", &[])?;
            let status = wait_child(&mut refused.child, Duration::from_secs(15))?
                .ok_or_else(|| fail("the daemon admitted after a failed handoff write"))?;
            let trace =
                fs::read_to_string(evidence.dir.join("daemon-refused.trace")).map_err(infra)?;
            check(!status.success() && trace.contains("store_error"), || {
                format!("startup did not fail on the Store failure ({status}): {trace}")
            })?;
            refused.shutdown()?;
            let (state, _) = paths.turn(&session, 2)?;
            check(state == "queued", || {
                format!("turn 2 after the failed write: {state}")
            })?;
            paths.failpoints.disarm(point).map_err(infra)?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            failed_on_corrupt_row(paths, &session, 2)?;
            let third = wait(paths, evidence, &format!("{session}/3"))?;
            check(third["state"] == "completed", || {
                format!("turn 3 after restart: {third}")
            })
        },
    )
}

/// Crashes a daemon while turn 1 of `hanging(prompts[0])` runs, with turns
/// 2.. of `prompts` durably `queued` behind it, then makes turn 2's frozen
/// row unreadable to Store. Returns the session and the crashed daemon,
/// which the caller keeps alive (see `Daemon::kill`).
#[cfg(feature = "test-failpoints")]
fn crash_behind_running<'a>(
    paths: &'a Paths,
    evidence: &Evidence,
    prompts: &[&str],
) -> Result<(String, Daemon<'a>), ScenarioError> {
    let mut daemon = Daemon::start(paths, evidence, "crashed")?;
    let (first, rest) = prompts.split_first().ok_or_else(|| infra("no prompts"))?;
    let session = session_of(&spawn(paths, evidence, "spawn", first, &[])?)?;
    paths.await_event(&session, 1, "turn.started")?;
    for (index, prompt) in rest.iter().enumerate() {
        resume(
            paths,
            evidence,
            &format!("resume-{}", index + 2),
            &session,
            prompt,
            &[],
        )?;
    }
    for n in 2..=u32::try_from(prompts.len()).map_err(infra)? {
        let (state, _) = paths.turn(&session, n)?;
        check(state == "queued", || {
            format!("turn {n} before the crash: {state}")
        })?;
    }
    daemon.kill()?;
    corrupt_effective(paths, &session, 2, "not json {")?;
    Ok((session, daemon))
}

/// C1 P6 behind a corrupt row: turn `n`, queued behind the recovered
/// `unknown` turn 1, is cancelled without launching.
#[cfg(feature = "test-failpoints")]
fn cancelled_behind_unknown(paths: &Paths, session: &str, n: u32) -> Result<(), ScenarioError> {
    let (state, envelope) = paths.turn(session, n)?;
    check(
        state == "cancelled" && envelope["state"] == "cancelled",
        || format!("turn {n} behind the unknown turn: {state} {envelope}"),
    )?;
    check(paths.anchors_of_turn(session, n)? == 0, || {
        format!("turn {n} launched behind the unknown turn")
    })
}

/// Design §7.3 with C1 P6: a queued turn whose row Store cannot read,
/// behind a recovered `unknown` turn, is cancelled like the rest of the
/// queue, so the valid turn after it is cancelled too and never runs.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_recovery_corrupt_row_keeps_the_unknown_barrier() -> TestResult {
    let fixture = json!({"scripts":[hanging("b1"), completes("b2", 2), completes("b3", 3)]});
    scenario(
        "s1_recovery_corrupt_barrier",
        &fixture,
        |paths, evidence| {
            let (session, _crashed) = crash_behind_running(paths, evidence, &["b1", "b2", "b3"])?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            recovered_unknown(paths, &session)?;
            cancelled_behind_unknown(paths, &session, 3)?;
            cancelled_behind_unknown(paths, &session, 2)
        },
    )
}

/// The same barrier across a crash between the corrupt row's resolution
/// and the next row's: the handoff is held at its next queued-row read
/// (`store.read.queued_turn`, occurrence 2) and killed. The next recovery
/// still cancels the valid turn behind the `unknown` one.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_recovery_corrupt_row_keeps_the_unknown_barrier_across_a_restart() -> TestResult {
    let fixture = json!({"scripts":[hanging("x1"), completes("x2", 2), completes("x3", 3)]});
    scenario(
        "s1_recovery_corrupt_barrier_restart",
        &fixture,
        |paths, evidence| {
            let (session, _crashed) = crash_behind_running(paths, evidence, &["x1", "x2", "x3"])?;
            let point = "store.read.queued_turn";
            paths.failpoints.arm(point, 2, "pause").map_err(infra)?;
            let mut interrupted = Daemon::spawn(paths, evidence, "interrupted", &[])?;
            paths
                .failpoints
                .wait_ack(point, 2, "pause", interrupted.child.id(), ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {point}: {error}")))?;
            interrupted.kill()?;
            paths.failpoints.disarm(point).map_err(infra)?;
            let (second, _) = paths.turn(&session, 2)?;
            let (third, _) = paths.turn(&session, 3)?;
            check(second != "queued" && third == "queued", || {
                format!("at the crash: turn 2 {second}, turn 3 {third}")
            })?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            recovered_unknown(paths, &session)?;
            cancelled_behind_unknown(paths, &session, 3)?;
            cancelled_behind_unknown(paths, &session, 2)
        },
    )
}

/// The failure message of a turn failed on its corrupt frozen row (design
/// §7.3), as opposed to the read streak's.
#[cfg(feature = "test-failpoints")]
const CORRUPT_ROW: &str = "a frozen value of the queued turn could not be read";

/// Design §7.3, live half (carried from S4 [s4.3, s4.8]): the handoff
/// enqueues a corrupt row behind an unresolved predecessor, and the
/// dispatcher's live rule fails it at the head. Turn 2 is valid and still
/// queued at the crash; turn 3's `effective` is not JSON (Store cannot read
/// the row), and turn 4's is JSON Core cannot parse. After the restart
/// turn 2 runs, turns 3 and 4 fail `failed(store)` without launch, each
/// with the corrupt-row message, and turn 5 runs.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_f12_live_corrupt_row_fails_at_the_head() -> TestResult {
    let fixture = json!({"scripts":[
        completes("l1", 1), completes("l2", 2), completes("l5", 5)
    ]});
    scenario("s1_f12_live_corrupt_row", &fixture, |paths, evidence| {
        let (session, _) =
            crash_with_queued_turns(paths, evidence, &["l1", "l2", "l3", "l4", "l5"], &[], &[])?;
        corrupt_effective(paths, &session, 3, "not json {")?;
        corrupt_effective(paths, &session, 4, r#"{"deadlines":"unparseable"}"#)?;
        let _daemon = Daemon::start(paths, evidence, "final")?;
        let fifth = wait(paths, evidence, &format!("{session}/5"))?;
        check(fifth["state"] == "completed", || {
            format!("turn 5 after the corrupt rows: {fifth}")
        })?;
        for n in [3, 4] {
            failed_on_corrupt_row(paths, &session, n)?;
            let (_, envelope) = paths.turn(&session, n)?;
            check(envelope["failure"]["message"] == CORRUPT_ROW, || {
                format!("turn {n} was not failed on its row: {envelope}")
            })?;
        }
        let (second, _) = paths.turn(&session, 2)?;
        check(second == "completed", || format!("turn 2: {second}"))
    })
}

/// Design §7.3 with C1 P6, live (carried from S4 [s4.8]): a row Store
/// cannot read, queued behind a turn that ends `unknown` after the restart,
/// is cancelled from its committed `turn.queued`, never submitted, so the
/// valid turn behind it is cancelled too. Turn 2 is valid and queued at the
/// crash, so the handoff enqueues turn 3 behind it; after the restart turn
/// 2 runs and a cancel whose `Stop` reply is lost
/// (`host.anchor.final_reply_lost`) ends it `unknown`. (The durable
/// `unknown` with `pending` cleanup is
/// `s1_recovery_corrupt_row_behind_a_pending_unknown_progresses_once_settled`.)
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_recovery_corrupt_row_behind_a_live_unknown_is_cancelled() -> TestResult {
    let fixture = json!({"scripts":[
        completes("u1", 1),
        script("u2", 2, vec![accepted(2), step("hang")]),
    ]});
    scenario("s1_recovery_live_unknown", &fixture, |paths, evidence| {
        let (session, _) =
            crash_with_queued_turns(paths, evidence, &["u1", "u2", "u3", "u4"], &[], &[])?;
        corrupt_effective(paths, &session, 3, "not json {")?;
        let _daemon = Daemon::start(paths, evidence, "final")?;
        paths.await_event(&session, 2, "turn.started")?;
        paths
            .failpoints
            .arm("host.anchor.final_reply_lost", 1, "fail_io")
            .map_err(infra)?;
        let reply = paths.ok(
            evidence,
            "cancel-2",
            &[
                "cancel",
                &session,
                "--turn",
                "2",
                "--force-after",
                "200",
                "--wait",
                "--handle",
                HANDLE,
                "--json",
            ],
        )?;
        check(reply["state"] == "unknown", || {
            format!("turn 2 did not end unknown: {reply}")
        })?;
        for n in [3, 4] {
            let envelope = wait(paths, evidence, &format!("{session}/{n}"))?;
            check(envelope["timestamps"]["submitted_at"].is_null(), || {
                format!("turn {n} was submitted: {envelope}")
            })?;
            cancelled_behind_unknown(paths, &session, n)?;
        }
        Ok(())
    })
}

/// Design §7.3 with C1 §7.3 and P6 (carried from S4 [s4.4, s4.8]; T3-S5
/// round 1, decision 7): a row Store cannot read, queued behind a durable
/// `unknown` turn whose cleanup is `pending`, waits; once the cleanup
/// settles, the live dispatcher cancels it from its committed `turn.queued`,
/// never submitted, and the valid turn behind it too.
///
/// No product path writes `pending` cleanup into a durable envelope (the
/// engine test `a_successor_waits_behind_a_cleanup_pending_predecessor`
/// notes it), so the state is written into Store while no daemon runs.
/// Turn 1 is recovered `unknown` by a restart held at the handoff's first
/// queued-row read (`store.read.queued_turn`) and killed there, before the
/// handoff's own P6 cancellation; its cleanup is then set to `pending`. The
/// next daemon's dispatcher waits (its predecessor reads, counted at
/// `store.read.dispatch`, repeat while turns 2 and 3 stay queued); the
/// cleanup is then set to `quiescent` while the daemon runs.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_recovery_corrupt_row_behind_a_pending_unknown_progresses_once_settled() -> TestResult {
    let fixture = json!({"scripts":[hanging("p1"), completes("p2", 2), completes("p3", 3)]});
    scenario(
        "s1_recovery_pending_unknown",
        &fixture,
        |paths, evidence| {
            let (session, _crashed) = crash_behind_running(paths, evidence, &["p1", "p2", "p3"])?;
            let point = "store.read.queued_turn";
            paths.failpoints.arm(point, 1, "pause").map_err(infra)?;
            let mut held = Daemon::spawn(paths, evidence, "held", &[])?;
            paths
                .failpoints
                .wait_ack(point, 1, "pause", held.child.id(), ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {point}: {error}")))?;
            held.kill()?;
            paths.failpoints.disarm(point).map_err(infra)?;
            recovered_unknown(paths, &session)?;
            for n in [2, 3] {
                let (state, _) = paths.turn(&session, n)?;
                check(state == "queued", || {
                    format!("turn {n} at the crash: {state}")
                })?;
            }
            let set_cleanup = |cleanup: &str| -> Result<(), ScenarioError> {
                let store =
                    rusqlite::Connection::open(paths.state.join("store.sqlite3")).map_err(infra)?;
                store.busy_timeout(Duration::from_secs(5)).map_err(infra)?;
                let changed = store
                    .execute(
                        "UPDATE turns SET envelope=json_set(envelope,'$.cancel.cleanup',?2)
                     WHERE session_id=?1 AND number=1",
                        rusqlite::params![session, cleanup],
                    )
                    .map_err(infra)?;
                check(changed == 1, || "turn 1 was not updated".to_owned())?;
                let (_, envelope) = paths.turn(&session, 1)?;
                check(envelope["cancel"]["cleanup"] == cleanup, || {
                    format!("turn 1 cleanup: {envelope}")
                })
            };
            set_cleanup("pending")?;
            let reads = "store.read.dispatch";
            let dir = paths
                .state
                .parent()
                .ok_or_else(|| infra("the state directory has no parent"))?
                .join("failpoints");
            hits::count(&dir, reads).map_err(infra)?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            // The handoff reads turns 2 and 3's predecessors once each; the
            // dispatcher's decisions for turn 2 follow, each a Wait.
            let waited = Instant::now() + Duration::from_secs(20);
            while hits::hits(&dir, reads).map_err(infra)? < 4 {
                check(Instant::now() < waited, || {
                    "the dispatcher did not re-read its predecessor".to_owned()
                })?;
                thread::sleep(Duration::from_millis(20));
            }
            for n in [2, 3] {
                let (state, _) = paths.turn(&session, n)?;
                check(state == "queued", || {
                    format!("turn {n} behind pending cleanup: {state}")
                })?;
            }
            set_cleanup("quiescent")?;
            for n in [2, 3] {
                let envelope = wait(paths, evidence, &format!("{session}/{n}"))?;
                check(envelope["timestamps"]["submitted_at"].is_null(), || {
                    format!("turn {n} was submitted: {envelope}")
                })?;
                cancelled_behind_unknown(paths, &session, n)?;
            }
            Ok(())
        },
    )
}

/// Design §7.3 [s4.8] (characterization: S4 built the handoff path): a row
/// Store cannot read, in a session whose close was durable at the crash, is
/// cancelled at restart with the close's cause from its committed
/// `turn.queued`, and the restart completes the close. The close's first
/// queued cancellation crashes the daemon (`store.commit.cancel`), so turns
/// 2 and 3 are still queued behind the running turn 1.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_recovery_unreadable_row_in_a_closing_session_is_cancelled() -> TestResult {
    let fixture = json!({"scripts":[hanging("c1")]});
    scenario(
        "s1_recovery_closing_unreadable",
        &fixture,
        |paths, evidence| {
            let mut daemon = Daemon::start(paths, evidence, "crashed")?;
            let session = session_of(&spawn(paths, evidence, "spawn", "c1", &[])?)?;
            paths.await_event(&session, 1, "turn.started")?;
            resume(paths, evidence, "resume-2", &session, "c2", &[])?;
            resume(paths, evidence, "resume-3", &session, "c3", &[])?;
            let point = "store.commit.cancel";
            paths.failpoints.arm(point, 1, "crash").map_err(infra)?;
            let close = paths.run(
                evidence,
                "close",
                &["close", &session, "--handle", HANDLE, "--json"],
            )?;
            check(!close.status.success(), || {
                "the close survived the crash".to_owned()
            })?;
            paths
                .failpoints
                .wait_ack(point, 1, "crash", daemon.child.id(), ACK_WAIT)
                .map_err(|error| fail(&format!("failpoint {point}: {error}")))?;
            daemon.kill()?;
            paths.failpoints.disarm(point).map_err(infra)?;
            for n in [2, 3] {
                let (state, _) = paths.turn(&session, n)?;
                check(state == "queued", || {
                    format!("turn {n} before the restart: {state}")
                })?;
            }
            corrupt_effective(paths, &session, 2, "not json {")?;
            let _daemon = Daemon::start(paths, evidence, "final")?;
            for n in [2, 3] {
                cancelled_behind_unknown(paths, &session, n)?;
                let cause: Option<String> = paths
                    .store()?
                    .query_row(
                        "SELECT cancel_cause FROM turns WHERE session_id=?1 AND number=?2",
                        rusqlite::params![session, n],
                        |row| row.get(0),
                    )
                    .map_err(infra)?;
                check(cause.as_deref() == Some("close"), || {
                    format!("turn {n} cancel cause: {cause:?}")
                })?;
            }
            let closed = paths
                .events(&session)?
                .iter()
                .any(|event| event["type"] == "session.closed");
            check(closed, || {
                "the restart did not close the session".to_owned()
            })?;
            drop(daemon);
            Ok(())
        },
    )
}

// ------------------------------------------------- nondefault frozen values

/// Design §11 (deferred by S2): the restart handoff and keyed replays keep a
/// nondefault `wall_ms` and `idle_ms`. Turn 2 inherits both, is queued at
/// the crash, and after restart runs under them: its `turn.started`
/// carries them, and the frozen 1.5 s idle deadline (not the 600 s
/// default) ends it `deadline_idle`. Keyed `spawn` and `resume` replays
/// after the restart return the original receipts. It passes on the base:
/// a proof of existing handoff behaviour, not a regression.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_restart_keeps_nondefault_frozen_values() -> TestResult {
    let mut idle = vec![accepted(2)];
    idle.push(json!({"action":"expect_request","expected":{"type":"interrupt"}}));
    idle.push(terminal(2, "interrupted", "interrupted"));
    let fixture = json!({"scripts":[completes("n1", 1), script("n2", 2, idle)]});
    scenario("s1_restart_frozen_values", &fixture, |paths, evidence| {
        let spawn_extra = [
            "--wall-ms",
            "45000",
            "--idle-ms",
            "1500",
            "--idempotency-key",
            "frozen-spawn",
        ];
        let resume_extra = ["--op-key", "frozen-resume"];
        let (session, receipts) =
            crash_with_queued_turns(paths, evidence, &["n1", "n2"], &spawn_extra, &resume_extra)?;
        let original_resume = receipts
            .first()
            .cloned()
            .ok_or_else(|| infra("no resume receipt"))?;
        let frozen = json!({"wall_ms":45000,"idle_ms":1500});
        check(original_resume["effective"]["deadlines"] == frozen, || {
            format!("turn 2 did not inherit the frozen values: {original_resume}")
        })?;
        let _daemon = Daemon::start(paths, evidence, "final")?;
        let second = wait(paths, evidence, &format!("{session}/2"))?;
        check(
            second["state"] == "failed" && second["failure"]["class"] == "deadline_idle",
            || format!("turn 2 did not run under its frozen idle deadline: {second}"),
        )?;
        let started = paths.await_event(&session, 2, "turn.started")?;
        check(started["effective"]["deadlines"] == frozen, || {
            format!("turn 2 started with {started}")
        })?;
        let replay = resume(
            paths,
            evidence,
            "resume-replay",
            &session,
            "n2",
            &resume_extra,
        )?;
        check(replay == original_resume, || {
            format!("resume replay after restart: {replay} vs {original_resume}")
        })?;
        let spawned = spawn(paths, evidence, "spawn-replay", "n1", &spawn_extra)?;
        check(
            spawned["session_id"] == session.as_str()
                && spawned["effective"]["deadlines"] == frozen,
            || format!("spawn replay after restart: {spawned}"),
        )
    })
}

/// S1-evidence2 fix round 3 (Sol r3 finding 2): a command's known exit
/// failure survives a lost output write. The reviewer's probe: `false`
/// (exit 1) with its stdout evidence path taken by a directory became
/// `Infrastructure("... Is a directory ...")`; the exit failure is
/// classified first, and the lost write is attached beside it.
#[test]
fn s1_recovery_harness_exit_failure_survives_a_lost_output_write() -> TestResult {
    let mut paths = Paths::new(&json!({"scripts": []}))?;
    paths.via = PathBuf::from("/bin/false");
    let evidence = Evidence::new("s1_recovery_lost_output", &paths.fake, &paths.fixture)?;
    fs::create_dir(evidence.dir.join("probe.stdout"))?;
    let error = paths
        .ok(&evidence, "probe", &["irrelevant"])
        .expect_err("a failed command passed");
    assert!(
        matches!(&error, ScenarioError::Failure(detail) if detail.contains("exited")
            && detail.contains("not written")),
        "the exit failure was replaced: {error:?}"
    );
    Ok(())
}
