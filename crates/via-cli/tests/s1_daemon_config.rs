//! Task 4 T4-7 (design §4.6, §5.3–§5.5, §7.6, §11.2; amendments A4, A37,
//! A42, A45) through the real `via` binary and daemon: `daemon.json` read
//! once at start, validated and reported; the daemon's own lines in
//! `via.log`; the disk free-space floor and the WAL limit refusing only new
//! work; the cached data-size warning; `daemon/status` counts, `describe`
//! and `models`. Every scenario activates the failpoint controller.
//! Written before the mechanisms.
#![cfg(feature = "test-failpoints")]

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/failpoints.rs"]
mod failpoints;
#[path = "support/hits.rs"]
mod hits;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use daemon::{Daemon, Sandbox, TestResult, cli, collect_available, failure, infra};
use failpoints::Failpoints;
use scenario::{ScenarioError, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const MIB: u64 = 1024 * 1024;
const FREE: &str = "store.statvfs.free_bytes";
const WALKS: &str = "core.data_size.walks";

// ---------------------------------------------------------------- fixtures

fn emit(message: &Value) -> Value {
    json!({"action":"emit","message":message})
}

fn gate(name: &str) -> Value {
    json!({"action":"gate","name":name})
}

fn accepted() -> Value {
    emit(&json!({"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}))
}

fn text() -> Value {
    json!({"type":"text","vendor_turn_id":"fake-turn-1","text":"model output"})
}

fn completed() -> Value {
    emit(
        &json!({"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed",
        "final_text":"done","stop_reason":"end_turn"}),
    )
}

/// `count` model steps: each a tool round ended by model output.
fn steps(count: u64) -> Value {
    let mut block = String::new();
    for message in [
        json!({"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t","name":"shell",
            "input_summary":"input"}),
        json!({"type":"tool_ended","vendor_turn_id":"fake-turn-1","tool_id":"t",
            "status":"completed","output_summary":"output"}),
        text(),
    ] {
        block.push_str(&message.to_string());
        block.push('\n');
    }
    json!({"action":"flood","text":block,"count":count})
}

/// A turn-1 script for `prompt`: `middle` between acceptance and completion.
fn script(prompt: &str, middle: &[Value]) -> Value {
    let mut all = vec![accepted(), emit(&text())];
    all.extend_from_slice(middle);
    all.push(completed());
    json!({"expected_request":{"type":"start","id":1,"turn":1,"prompt":prompt},"steps":all})
}

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

fn line(id: u64, method: &str, params: &Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string()
}

fn spawn_params(prompt: &str) -> Value {
    json!({"harness":"fake","model":"fake","prompt":prompt,"handle":HANDLE})
}

fn is_error(reply: &Value, code: i64, kind: &str) -> bool {
    reply["error"]["code"] == code && reply["error"]["data"]["kind"] == kind
}

/// The five effective thresholds as `daemon/status` reports them (A37).
fn limits(floor: u64, warn: u64, max: u64, checkpoint: u64, commits: u64) -> Value {
    json!({"disk":{"free_floor":floor,"warn_size":warn},
        "wal":{"max":max,"checkpoint_bytes":checkpoint,"checkpoint_commits":commits}})
}

fn defaults() -> Value {
    limits(5 * 1024 * MIB, 2 * 1024 * MIB, 32 * MIB, 8 * MIB, 1000)
}

// ----------------------------------------------------------------- harness

/// One scenario's deployment: the sandbox and its failpoint directory.
struct Setup {
    sandbox: Sandbox,
    failpoints: Failpoints,
    dir: PathBuf,
}

impl Setup {
    fn new(fixture: &Value) -> TestResult<Self> {
        let sandbox = Sandbox::new(fixture)?;
        let root = sandbox
            .state
            .parent()
            .ok_or("sandbox state has no parent")?
            .to_owned();
        let failpoints = Failpoints::new(&root)?;
        let dir = root.join("failpoints");
        Ok(Self {
            sandbox,
            failpoints,
            dir,
        })
    }

    fn evidence(&self, name: &str) -> TestResult<Evidence> {
        Evidence::new(name, &self.sandbox.fake, &self.sandbox.fixture)
    }

    fn start(&self, evidence: &Evidence) -> Result<Daemon<'_>, ScenarioError> {
        Daemon::start_with(&self.sandbox, evidence, |command| {
            self.failpoints.activate(command);
        })
    }

    /// Writes `<state>/daemon.json` (0600).
    fn config(&self, text: &str) -> Result<(), ScenarioError> {
        let path = self.sandbox.state.join("daemon.json");
        fs::write(&path, text).map_err(infra)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(infra)
    }

    fn daemon_status(&self, evidence: &Evidence, name: &str) -> Result<Value, ScenarioError> {
        cli(
            &self.sandbox,
            evidence,
            name,
            &["daemon", "status", "--json"],
        )
    }

    fn wait(&self, evidence: &Evidence, name: &str, address: &str) -> Result<Value, ScenarioError> {
        cli(
            &self.sandbox,
            evidence,
            name,
            &["wait", address, "--timeout-ms", "30000", "--json"],
        )
    }

    /// A background spawn of `prompt`; returns its session.
    fn spawn(&self, evidence: &Evidence, prompt: &str) -> Result<String, ScenarioError> {
        let receipt = cli(
            &self.sandbox,
            evidence,
            &format!("spawn_{prompt}"),
            &[
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
            ],
        )?;
        receipt["session_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| failure(format!("spawn {prompt}: {receipt}")))
    }

    fn resume(
        &self,
        evidence: &Evidence,
        session: &str,
        prompt: &str,
    ) -> Result<Value, ScenarioError> {
        cli(
            &self.sandbox,
            evidence,
            &format!("resume_{prompt}"),
            &[
                "resume", session, "--prompt", prompt, "--handle", HANDLE, "--json",
            ],
        )
    }

    /// The serving daemon's pid, through the direct socket probe: never
    /// an auto-start.
    fn pid(&self) -> Result<u32, ScenarioError> {
        daemon::serving_pid(&self.sandbox.runtime)
            .ok_or_else(|| failure("no daemon serves the socket"))
    }

    fn count(&self, sql: &str) -> Result<i64, ScenarioError> {
        self.sandbox.count(sql)
    }

    /// The regular files under `dir`, recursively, with their lengths.
    fn lengths(dir: &Path, into: &mut u64) -> Result<(), ScenarioError> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in entries {
            let entry = entry.map_err(infra)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(infra)?;
            if metadata.is_dir() {
                Self::lengths(&entry.path(), into)?;
            } else if metadata.is_file() {
                *into += metadata.len();
            }
        }
        Ok(())
    }

    /// Design §5.3 [t4r17.7]: the apparent lengths of the Store files,
    /// `via.log`, `via.log.1` and every file under `blobs/` and `evidence/`.
    fn data_bytes(&self) -> Result<u64, ScenarioError> {
        let state = &self.sandbox.state;
        let mut total = 0;
        for name in [
            "store.sqlite3",
            "store.sqlite3-wal",
            "store.sqlite3-shm",
            "via.log",
            "via.log.1",
        ] {
            if let Ok(metadata) = fs::symlink_metadata(state.join(name)) {
                total += metadata.len();
            }
        }
        Self::lengths(&state.join("blobs"), &mut total)?;
        Self::lengths(&state.join("evidence"), &mut total)?;
        Ok(total)
    }

    fn wal_len(&self) -> u64 {
        fs::metadata(self.sandbox.state.join("store.sqlite3-wal")).map_or(0, |meta| meta.len())
    }

    fn via_log(&self) -> String {
        fs::read_to_string(self.sandbox.state.join("via.log")).unwrap_or_default()
    }

    /// Scenario cleanup: every stored envelope and event, then the Store,
    /// the evidence folders and `via.log`.
    fn collect(&self, evidence: &Evidence) -> Result<(), ScenarioError> {
        let path = self.sandbox.state.join("store.sqlite3");
        let (mut envelopes, mut events) = (String::new(), String::new());
        if path.is_file() {
            let store = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(infra)?;
            for (sql, out) in [
                (
                    "SELECT envelope FROM turns WHERE envelope IS NOT NULL \
                     ORDER BY session_id,number",
                    &mut envelopes,
                ),
                (
                    "SELECT event FROM events ORDER BY session_id,seq",
                    &mut events,
                ),
            ] {
                let mut statement = store.prepare(sql).map_err(infra)?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(infra)?;
                for row in rows {
                    out.push_str(&row.map_err(infra)?);
                    out.push('\n');
                }
            }
        }
        evidence
            .write("envelopes.ndjson", envelopes.as_bytes())
            .map_err(infra)?;
        evidence
            .write("events.ndjson", events.as_bytes())
            .map_err(infra)?;
        collect_available(evidence, &self.sandbox.state, &self.sandbox.teardown)
    }

    /// One completed turn of `prompt`, so the scenario leaves an evidence
    /// folder.
    fn one_turn(&self, evidence: &Evidence, prompt: &str) -> Result<(), ScenarioError> {
        let session = self.spawn(evidence, prompt)?;
        let envelope = self.wait(evidence, &format!("wait_{prompt}"), &format!("{session}/1"))?;
        check(envelope["state"] == "completed", || {
            format!("{prompt} turn: {envelope}")
        })
    }

    /// A private listing of a directory: name → (length, modified).
    fn listing(dir: &Path) -> Result<BTreeMap<String, (u64, SystemTime)>, ScenarioError> {
        let mut listing = BTreeMap::new();
        for entry in fs::read_dir(dir).map_err(infra)? {
            let entry = entry.map_err(infra)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(infra)?;
            listing.insert(
                entry.file_name().to_string_lossy().into_owned(),
                (metadata.len(), metadata.modified().map_err(infra)?),
            );
        }
        Ok(listing)
    }
}

/// A raw C1 socket after `hello`.
struct Conn {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
}

impl Conn {
    fn open(sandbox: &Sandbox) -> Result<Self, ScenarioError> {
        let stream = UnixStream::connect(sandbox.runtime.join("via.sock")).map_err(infra)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(infra)?;
        let mut conn = Self {
            reader: BufReader::new(stream.try_clone().map_err(infra)?),
            stream,
            next: 1,
        };
        let params =
            json!({"api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"s1-t4-7"});
        let hello = conn.call("hello", &params)?;
        check(hello["result"]["api_version"] == 1, || {
            format!("hello refused: {hello}")
        })?;
        Ok(conn)
    }

    fn send(&mut self, method: &str, params: &Value) -> Result<(), ScenarioError> {
        let request = line(self.next, method, params);
        self.next += 1;
        self.stream.write_all(request.as_bytes()).map_err(infra)?;
        self.stream.write_all(b"\n").map_err(infra)
    }

    fn reply(&mut self) -> Result<Value, ScenarioError> {
        let mut reply = Vec::new();
        if self.reader.read_until(b'\n', &mut reply).map_err(infra)? == 0 {
            return Err(failure("daemon closed the connection"));
        }
        serde_json::from_slice(&reply).map_err(infra)
    }

    fn call(&mut self, method: &str, params: &Value) -> Result<Value, ScenarioError> {
        self.send(method, params)?;
        self.reply()
    }
}

/// Waits until `condition` holds, polling; `what` names it on timeout.
fn wait_until(
    what: &str,
    within: Duration,
    mut condition: impl FnMut() -> Result<bool, ScenarioError>,
) -> Result<(), ScenarioError> {
    let deadline = Instant::now() + within;
    loop {
        if condition()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!("{what} did not happen")));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

// ------------------------------------------------------------------ config

/// Design §5.5, §13.2 [t4r16.3]: no file gives the defaults; lowered values
/// apply only after a restart; an unknown key (a `memory` object
/// included), a duplicate key, `wal.max` ≤ `checkpoint_bytes`,
/// `checkpoint_bytes` 0 or 4095, a value past 2^62 and a group-writable
/// file each exit 78 naming key and rule, touching no Store or socket;
/// `limits` equals the effective values.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario walks the defaults, a restart and each invalid file"
)]
fn s1_config_is_read_at_start_validated_and_reported() -> TestResult {
    let setup = Setup::new(&script("config", &[]))?;
    let evidence = setup.evidence("s1_config_read_validated")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let daemon = setup.start(evidence)?;
            let status = setup.daemon_status(evidence, "status_defaults")?;
            check(status["limits"] == defaults(), || {
                format!("no daemon.json: {}", status["limits"])
            })?;
            // Read once, at start: a file written now changes nothing yet.
            setup.config(
                r#"{"disk":{"free_floor":1048576,"warn_size":2048},
                    "wal":{"max":5242880,"checkpoint_bytes":1052000,"checkpoint_commits":7}}"#,
            )?;
            let status = setup.daemon_status(evidence, "status_unchanged")?;
            check(status["limits"] == defaults(), || {
                format!("a running daemon re-read daemon.json: {}", status["limits"])
            })?;
            daemon.shutdown()?;
            let daemon = setup.start(evidence)?;
            let status = setup.daemon_status(evidence, "status_lowered")?;
            // `checkpoint_bytes` applies as whole 4 KiB pages.
            check(
                status["limits"] == limits(MIB, 2048, 5 * MIB, 1_052_000 / 4096 * 4096, 7),
                || format!("lowered after a restart: {}", status["limits"]),
            )?;
            daemon.shutdown()?;

            let runtime = &setup.sandbox.runtime;
            let state = &setup.sandbox.state;
            let cases: [(&str, &str, &str); 14] = [
                // An explicit null is neither absent nor a value (review r1).
                (
                    r#"{"disk":{"free_floor":null}}"#,
                    "disk.free_floor",
                    "integer",
                ),
                (
                    r#"{"wal":{"checkpoint_commits":null}}"#,
                    "wal.checkpoint_commits",
                    "integer",
                ),
                (r#"{"wal":null}"#, "wal", "must be an object"),
                (r#"{"memory":{"max":1}}"#, "memory", "unknown key"),
                (r#"{"disk":{"floor":1}}"#, "disk.floor", "unknown key"),
                (
                    r#"{"wal":{"max":8388608,"max":8388608}}"#,
                    "wal.max",
                    "duplicate key",
                ),
                (r#"{"disk":{},"disk":{}}"#, "disk", "duplicate key"),
                (
                    r#"{"wal":{"max":8388608,"checkpoint_bytes":8388608}}"#,
                    "wal.max",
                    "above wal.checkpoint_bytes",
                ),
                (
                    r#"{"wal":{"checkpoint_bytes":0}}"#,
                    "wal.checkpoint_bytes",
                    "at least 4096",
                ),
                (
                    r#"{"wal":{"checkpoint_bytes":4095}}"#,
                    "wal.checkpoint_bytes",
                    "at least 4096",
                ),
                (
                    r#"{"disk":{"warn_size":4611686018427387905}}"#,
                    "disk.warn_size",
                    "at most 2^62",
                ),
                (r#"{"wal":{"max":4194303}}"#, "wal.max", "at least 4194304"),
                (
                    r#"{"wal":{"checkpoint_commits":0}}"#,
                    "wal.checkpoint_commits",
                    "from 1 to 4294967295",
                ),
                (
                    r#"{"disk":{"free_floor":-1}}"#,
                    "disk.free_floor",
                    "integer",
                ),
            ];
            let mut outcomes = Vec::new();
            let mut invalid = |text: &str, key: &str, rule: &str, mode: u32| {
                setup.config(text)?;
                fs::set_permissions(state.join("daemon.json"), fs::Permissions::from_mode(mode))
                    .map_err(infra)?;
                let before = (Setup::listing(state)?, Setup::listing(runtime)?);
                let run = setup
                    .sandbox
                    .run(&["daemon"], Duration::from_secs(10))
                    .map_err(infra)?;
                let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
                outcomes.push(json!({"file":text,"mode":mode,"exit":run.status.code(),
                    "stderr":stderr}));
                let prefix = format!("via: daemon config invalid: {key}: ");
                check(
                    run.status.code() == Some(78)
                        && stderr.lines().any(|line| {
                            line.starts_with(&prefix) && line[prefix.len()..].contains(rule)
                        }),
                    || format!("{text} (mode {mode:o}): exit {:?}, {stderr}", run.status),
                )?;
                let after = (Setup::listing(state)?, Setup::listing(runtime)?);
                check(before == after, || {
                    format!("{text} changed the state or runtime directory: {before:?} {after:?}")
                })?;
                check(!runtime.join("via.sock").exists(), || {
                    format!("{text} left a socket")
                })
            };
            for (text, key, rule) in cases {
                invalid(text, key, rule, 0o600)?;
            }
            invalid("{}", "daemon.json", "group- or world-writable", 0o620)?;
            invalid("{\"disk\":", "daemon.json", "not JSON", 0o600)?;
            evidence
                .write(
                    "invalid_configs.json",
                    Value::Array(outcomes).to_string().as_bytes(),
                )
                .map_err(infra)?;
            // A fresh State: an invalid file creates no Store file at all.
            let fresh = Sandbox::new(&script("config", &[])).map_err(infra)?;
            let path = fresh.state.join("daemon.json");
            fs::write(&path, r#"{"memory":{}}"#).map_err(infra)?;
            let run = fresh
                .run(&["daemon"], Duration::from_secs(10))
                .map_err(infra)?;
            let names: Vec<String> = fs::read_dir(&fresh.state)
                .map_err(infra)?
                .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect::<Result<_, _>>()
                .map_err(infra)?;
            check(
                run.status.code() == Some(78) && names == ["daemon.json"],
                || {
                    format!(
                        "fresh state after an invalid start: {:?} {names:?}",
                        run.status
                    )
                },
            )?;
            fs::remove_file(state.join("daemon.json")).map_err(infra)?;
            let _daemon = setup.start(evidence)?;
            let status = setup.daemon_status(evidence, "status_removed")?;
            check(status["limits"] == defaults(), || {
                format!("daemon.json removed: {}", status["limits"])
            })?;
            setup.one_turn(evidence, "config")
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Design §5.5, §7.6 (T4-fix; Astra 4): a FIFO at `daemon.json` or at
/// `via.log` is refused at once, never waited on for a writer or a reader.
/// `daemon.json` exits 78 "must be a regular file" before any Store or
/// socket change; `via.log` fails startup with a message naming it before
/// the Store opens. Each start is bounded by 10 s: before the fix both hung.
#[test]
fn s1_config_and_daemon_log_fifo_are_refused_at_once() -> TestResult {
    let setup = Setup::new(&script("fifo", &[]))?;
    let evidence = setup.evidence("s1_config_fifo")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let mut outcomes = Vec::new();
            for name in ["daemon.json", "via.log"] {
                let fresh = Sandbox::new(&script("fifo", &[])).map_err(infra)?;
                let made = std::process::Command::new("mkfifo")
                    .arg(fresh.state.join(name))
                    .status()
                    .map_err(infra)?;
                check(made.success(), || "mkfifo failed".to_owned())?;
                let run = fresh
                    .run(&["daemon"], Duration::from_secs(10))
                    .map_err(infra)?;
                let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
                let names: Vec<String> = fs::read_dir(&fresh.state)
                    .map_err(infra)?
                    .map(|entry| {
                        entry.map(|entry| entry.file_name().to_string_lossy().into_owned())
                    })
                    .collect::<Result<_, _>>()
                    .map_err(infra)?;
                outcomes.push(json!({"fifo":name,"exit":run.status.code(),
                    "timed_out":run.timed_out,"stderr":stderr,"state":names}));
                check(!run.timed_out, || format!("a {name} FIFO hung the start"))?;
                let expected = if name == "daemon.json" {
                    run.status.code() == Some(78) && stderr.lines().any(|line| {
                        line == "via: daemon config invalid: daemon.json: must be a regular file"
                    })
                } else {
                    run.status.code().is_some_and(|code| code != 0)
                        && stderr.contains("via.log")
                        && stderr.contains("must be a regular file")
                };
                check(expected, || {
                    format!("a {name} FIFO: exit {:?}, {stderr}", run.status)
                })?;
                // `store.lock` precedes `via.log` (§7.6); the Store does not.
                check(
                    !names.iter().any(|entry| entry.starts_with("store.sqlite3")),
                    || format!("a {name} FIFO opened the Store: {names:?}"),
                )?;
                check(!fresh.runtime.join("via.sock").exists(), || {
                    format!("a {name} FIFO left a socket")
                })?;
            }
            evidence
                .write(
                    "fifo_starts.json",
                    Value::Array(outcomes).to_string().as_bytes(),
                )
                .map_err(infra)?;
            // The scenario's own deployment runs one turn for its evidence.
            let _daemon = setup.start(evidence)?;
            setup.one_turn(evidence, "fifo")
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ------------------------------------------------------------- daemon log

/// Design §7.6, §13.2 [t4r16.7.1]: an invalid `daemon.json` is reported by
/// the auto-starting CLI from stderr; after a start that recovered a turn,
/// the recovery warning is in `via.log` with its `session` and `turn`; a
/// later warning is in `via.log` and not on stderr; the shutdown summary is
/// the last line of `via.log`; a `via.log` of 10 MiB + 1 byte becomes
/// `via.log.1` at the next start.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario covers the start report, recovery, a later warning and rotation"
)]
fn s1_daemon_log_after_startup_and_rotation() -> TestResult {
    let setup = Setup::new(&script("logged", &[]))?;
    let evidence = setup.evidence("s1_daemon_log_rotation")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            setup.config(r#"{"wal":{"bogus":1}}"#)?;
            let run = setup
                .sandbox
                .run(&["daemon", "status", "--json"], Duration::from_secs(20))
                .map_err(infra)?;
            let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
            evidence
                .write("autostart.stderr", &run.stderr)
                .map_err(infra)?;
            check(
                run.status.code() == Some(4)
                    && stderr.contains("via: daemon config invalid: wal.bogus: unknown key"),
                || {
                    format!(
                        "auto-start with an invalid config: {:?} {stderr}",
                        run.status
                    )
                },
            )?;
            fs::remove_file(setup.sandbox.state.join("daemon.json")).map_err(infra)?;

            // A crash after submission leaves a turn for recovery.
            let point = "core.accept.before_commit";
            setup.failpoints.arm(point, 1, "crash").map_err(infra)?;
            let crashed = setup.start(evidence)?;
            let pid = setup.pid()?;
            let session = setup.spawn(evidence, "logged")?;
            setup
                .failpoints
                .wait_ack(point, 1, "crash", pid, Duration::from_secs(10))
                .map_err(infra)?;
            // Reaps the crashed daemon: an intermediate shutdown, since it
            // may still be exiting (S1-evidence2 fix round 2, finding 8).
            crashed.shutdown()?;
            setup.failpoints.disarm(point).map_err(infra)?;
            let trace = evidence.dir.join("daemon.trace");
            let recovered = setup.start(evidence)?;
            let log = setup.via_log();
            evidence
                .write("via.log.recovered", log.as_bytes())
                .map_err(infra)?;
            let recovery = log
                .lines()
                .find(|line| line.contains("recovered"))
                .unwrap_or_default()
                .to_owned();
            check(
                recovery.contains(&format!("session={session}")) && recovery.contains("turn=1"),
                || format!("no recovery warning naming {session}/1 in via.log: {log}"),
            )?;
            // A later warning: an accepted forced stop, then the summary.
            let before = fs::read(&trace).map_err(infra)?;
            let stop = cli(
                &setup.sandbox,
                evidence,
                "stop_force",
                &["daemon", "stop", "--force", "--json"],
            )?;
            check(stop["stopping"] == true, || format!("stop: {stop}"))?;
            wait_until("the summary", Duration::from_secs(15), || {
                Ok(setup.via_log().contains("daemon_shutdown"))
            })?;
            recovered.shutdown()?;
            let log = setup.via_log();
            let after = fs::read(&trace).map_err(infra)?;
            let last = log.lines().last().unwrap_or_default();
            let summary: Value = serde_json::from_str(last).unwrap_or(Value::Null);
            check(summary["daemon_shutdown"]["mode"] == "force", || {
                format!("the last line of via.log is not the summary: {last}")
            })?;
            check(
                log.lines()
                    .any(|line| line.contains("WARN") && line.contains("forced stop")),
                || format!("no later warning in via.log: {log}"),
            )?;
            let added = String::from_utf8_lossy(&after[before.len().min(after.len())..]);
            check(
                !added.contains("forced stop") && !added.contains("daemon_shutdown"),
                || format!("a line after startup reached stderr: {added}"),
            )?;

            // Rotation past 10 MiB at the next start.
            let path = setup.sandbox.state.join("via.log");
            let mut big = log.into_bytes();
            big.resize(usize::try_from(10 * MIB + 1).map_err(infra)?, b'x');
            fs::write(&path, &big).map_err(infra)?;
            let _daemon = setup.start(evidence)?;
            let rotated = fs::read(setup.sandbox.state.join("via.log.1")).map_err(infra)?;
            let fresh = fs::metadata(&path).map_err(infra)?;
            check(rotated == big && fresh.len() < MIB, || {
                format!(
                    "rotation: via.log.1 {} bytes, via.log {} bytes",
                    rotated.len(),
                    fresh.len()
                )
            })?;
            check(fresh.permissions().mode() & 0o777 == 0o600, || {
                format!("via.log mode {:o}", fresh.permissions().mode())
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// -------------------------------------------------------------- disk floor

/// Design §5.3, §13.2 [t4r16.3, t4r17.1]: free space below a lowered floor:
/// `spawn` and `resume` are `admission_refused` `disk_free_floor` with no
/// write, while a keyed retry of a receipt stored before returns that
/// receipt; a queued turn fails `store` at dispatch; a running turn ends
/// normally with its rows; a close and a queued-turn cancel commit;
/// `below_free_floor` shows; a daemon started below the floor serves reads.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario holds a running, a queued and a keyed turn across the floor"
)]
fn s1_store_disk_floor_refuses_new_work_only() -> TestResult {
    let fixture = json!({"scripts":[
        script("run", &[gate("run"), steps(2)]),
        script("done", &[]),
        script("keyed", &[]),
    ]});
    let setup = Setup::new(&fixture)?;
    let evidence = setup.evidence("s1_store_disk_floor")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            setup.config(r#"{"disk":{"free_floor":1048576}}"#)?;
            let daemon = setup.start(evidence)?;
            let running = setup.spawn(evidence, "run")?;
            setup.sandbox.await_gate("run")?;
            setup.resume(evidence, &running, "second")?;
            setup.resume(evidence, &running, "third")?;
            let done = setup.spawn(evidence, "done")?;
            let envelope = setup.wait(evidence, "wait_done", &format!("{done}/1"))?;
            check(envelope["state"] == "completed", || format!("{envelope}"))?;
            let mut conn = Conn::open(&setup.sandbox)?;
            let mut keyed = spawn_params("keyed");
            keyed["idempotency_key"] = json!("k1");
            let stored = conn.call("spawn", &keyed)?;
            let keyed_session = stored["result"]["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("keyed spawn: {stored}")))?
                .to_owned();
            setup.wait(evidence, "wait_keyed", &format!("{keyed_session}/1"))?;

            setup
                .failpoints
                .arm(FREE, 1, "value_persist:4096")
                .map_err(infra)?;
            let sessions = setup.count("SELECT COUNT(*) FROM sessions")?;
            let turns = setup.count("SELECT COUNT(*) FROM turns")?;
            let refused = conn.call("spawn", &spawn_params("refused"))?;
            check(
                is_error(&refused, -32012, "admission_refused")
                    && refused["error"]["data"]["kind2"] == "disk_free_floor"
                    && refused["error"]["data"]["free_bytes"] == 4096
                    && refused["error"]["data"]["floor_bytes"] == MIB,
                || format!("spawn below the floor: {refused}"),
            )?;
            let resumed = conn.call(
                "resume",
                &json!({"session":done,"handle":HANDLE,"prompt":"refused"}),
            )?;
            check(
                is_error(&resumed, -32012, "admission_refused")
                    && resumed["error"]["data"]["kind2"] == "disk_free_floor",
                || format!("resume below the floor: {resumed}"),
            )?;
            check(
                setup.count("SELECT COUNT(*) FROM sessions")? == sessions
                    && setup.count("SELECT COUNT(*) FROM turns")? == turns,
                || "a refused request wrote".to_owned(),
            )?;
            let replay = conn.call("spawn", &keyed)?;
            check(replay["result"] == stored["result"], || {
                format!("keyed retry below the floor: {replay}")
            })?;
            let cancelled = conn.call(
                "cancel",
                &json!({"session":running,"handle":HANDLE,"turn":3}),
            )?;
            check(cancelled["result"].is_object(), || {
                format!("queued-turn cancel below the floor: {cancelled}")
            })?;
            let status = setup.daemon_status(evidence, "status_below")?;
            check(
                status["storage"]["below_free_floor"] == true
                    && status["storage"]["free_bytes"] == 4096,
                || format!("storage below the floor: {}", status["storage"]),
            )?;

            setup.sandbox.release_gate("run")?;
            let first = setup.wait(evidence, "wait_run", &format!("{running}/1"))?;
            check(first["state"] == "completed", || {
                format!("running turn: {first}")
            })?;
            let rows = setup.count(&format!(
                "SELECT COUNT(*) FROM steps WHERE session_id='{running}' AND turn=1"
            ))?;
            check(rows >= 2, || format!("running turn rows: {rows}"))?;
            let second = setup.wait(evidence, "wait_second", &format!("{running}/2"))?;
            check(
                second["state"] == "failed"
                    && second["failure"]["class"] == "store"
                    && second["failure"]["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("free space")),
                || format!("queued turn below the floor: {second}"),
            )?;
            let closed = conn.call("close", &json!({"session":done,"handle":HANDLE}))?;
            check(closed["result"].is_object(), || {
                format!("close below the floor: {closed}")
            })?;
            drop(conn);
            daemon.shutdown()?;

            // Started below the floor: reads are served, new work refused.
            let _daemon = setup.start(evidence)?;
            let result = cli(
                &setup.sandbox,
                evidence,
                "result_restarted",
                &["result", &format!("{running}/1"), "--json"],
            )?;
            check(result["state"] == "completed", || format!("{result}"))?;
            let status = setup.daemon_status(evidence, "status_restarted")?;
            check(status["storage"]["below_free_floor"] == true, || {
                format!("restarted below the floor: {}", status["storage"])
            })?;
            let mut conn = Conn::open(&setup.sandbox)?;
            let refused = conn.call("spawn", &spawn_params("refused"))?;
            check(is_error(&refused, -32012, "admission_refused"), || {
                format!("spawn after a restart below the floor: {refused}")
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// --------------------------------------------------------------- data size

/// Design §5.3, §13.2 [t4r17.7]: a lowered `warn_size` gives
/// `over_warn_size`; `data_bytes` equals the summed apparent lengths,
/// `via.log` and `via.log.1` included; a second call within 60 s walks
/// nothing.
#[test]
fn s1_store_data_size_warning_is_cached() -> TestResult {
    // One script for any prompt: the turn's prompt is a 300 KiB file.
    let any = json!({"expected_request":{"type":"start","id":1,"turn":1},
        "steps":[accepted(), emit(&text()), completed()]});
    let setup = Setup::new(&json!({ "scripts": [any] }))?;
    let evidence = setup.evidence("s1_store_data_size")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            // A first run leaves a prompt blob and an evidence folder.
            let daemon = setup.start(evidence)?;
            let big = setup.sandbox.sync.join("big-prompt.txt");
            fs::write(&big, "p".repeat(300 * 1024)).map_err(infra)?;
            let big = big.to_str().ok_or_else(|| infra("path"))?;
            let receipt = cli(
                &setup.sandbox,
                evidence,
                "spawn_blob",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt-file",
                    big,
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let turn = receipt["turn"]
                .as_str()
                .ok_or_else(|| failure(format!("{receipt}")))?
                .to_owned();
            setup.wait(evidence, "wait_blob", &turn)?;
            daemon.shutdown()?;
            fs::write(setup.sandbox.state.join("via.log.1"), b"rotated lines\n").map_err(infra)?;
            setup.config(r#"{"disk":{"warn_size":1024}}"#)?;
            hits::count(&setup.dir, WALKS).map_err(infra)?;
            let _daemon = setup.start(evidence)?;
            let first = setup.daemon_status(evidence, "status_first")?;
            let expected = setup.data_bytes()?;
            let walks = hits::hits(&setup.dir, WALKS).map_err(infra)?;
            let second = setup.daemon_status(evidence, "status_second")?;
            let storage = &first["storage"];
            check(
                storage["data_bytes"] == expected
                    && storage["over_warn_size"] == true
                    && storage["below_free_floor"] == false
                    && storage["free_bytes"].as_u64().is_some_and(|free| free > 0)
                    && storage["data_measured_at"].is_string(),
                || format!("storage {storage}, expected data_bytes {expected}"),
            )?;
            check(
                walks >= 1
                    && hits::hits(&setup.dir, WALKS).map_err(infra)? == walks
                    && second["storage"]["data_measured_at"] == storage["data_measured_at"]
                    && second["storage"]["data_bytes"] == storage["data_bytes"],
                || format!("a second call walked again: {walks}, {}", second["storage"]),
            )
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Design §5.3, §15 (T4-fix; Astra 5, Fable F1): a walk that overruns its
/// 2 s step is still the minute's one walk. With the walk held
/// (`store.data_size.walk`), two `daemon/status` calls within 60 s start
/// one walk and both report `data_bytes: null`; the held walk does not
/// hold another blob-step slot per call.
#[test]
fn s1_store_data_size_overrun_walks_once() -> TestResult {
    const WALK: &str = "store.data_size.walk";
    let setup = Setup::new(&json!({ "scripts": [script("sized", &[])] }))?;
    let evidence = setup.evidence("s1_store_data_size_overrun")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            hits::count(&setup.dir, WALKS).map_err(infra)?;
            setup.failpoints.arm(WALK, 1, "pause").map_err(infra)?;
            let _daemon = setup.start(evidence)?;
            let pid = setup.pid()?;
            // Every CLI command sends `daemon/status`: the spawn and the
            // wait take part in the minute's walk count too.
            let session = setup.spawn(evidence, "sized")?;
            setup.wait(evidence, "wait_sized", &format!("{session}/1"))?;
            let first = setup.daemon_status(evidence, "status_first");
            let second = setup.daemon_status(evidence, "status_second");
            let walks = hits::hits(&setup.dir, WALKS).map_err(infra)?;
            let held = setup
                .failpoints
                .wait_ack(WALK, 1, "pause", pid, Duration::from_secs(5))
                .map_err(infra);
            setup.failpoints.release(WALK, 1).map_err(infra)?;
            held?;
            evidence
                .write(
                    "walk_ack.json",
                    &setup.failpoints.ack_bytes(WALK, 1).map_err(infra)?,
                )
                .map_err(infra)?;
            let (first, second) = (first?, second?);
            check(walks == 1, || format!("{walks} walks within 60 s"))?;
            for status in [&first, &second] {
                let storage = &status["storage"];
                check(
                    storage["data_bytes"].is_null()
                        && storage["data_measured_at"].is_null()
                        && storage["over_warn_size"].is_null()
                        && storage["free_bytes"].as_u64().is_some_and(|free| free > 0),
                    || format!("storage after an overrun walk: {storage}"),
                )?;
            }
            Ok(())
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// --------------------------------------------------------------------- WAL

/// Grows the WAL by releasing `session`'s flood rounds `r<first>..` until
/// `done` holds; returns the next round to release.
fn grow(
    setup: &Setup,
    (session, prompt, per): (&str, &str, u64),
    (first, rounds): (usize, usize),
    mut done: impl FnMut(&Setup) -> bool,
) -> Result<usize, ScenarioError> {
    let mut round = first;
    while round < rounds && !done(setup) {
        step(setup, (session, prompt, per), round)?;
        round += 1;
    }
    Ok(round)
}

/// Releases every remaining flood round.
fn drain(
    setup: &Setup,
    (session, prompt, per): (&str, &str, u64),
    (first, rounds): (usize, usize),
) -> Result<(), ScenarioError> {
    for round in first..rounds {
        step(setup, (session, prompt, per), round)?;
    }
    Ok(())
}

/// Releases round `round` once the fake waits at its gate and Core has
/// committed the rows of every round before it: the harness keeps the
/// flood within the Wire's queue however slow the commits are.
fn step(
    setup: &Setup,
    (session, prompt, per): (&str, &str, u64),
    round: usize,
) -> Result<(), ScenarioError> {
    let name = format!("{prompt}-r{round}");
    setup.sandbox.await_gate(&name)?;
    let rows = (u64::try_from(round).map_err(infra)? * per).saturating_sub(1);
    let sql = format!("SELECT COUNT(*) FROM steps WHERE session_id='{session}' AND turn=1");
    wait_until("the flood's rows", Duration::from_secs(30), || {
        Ok(u64::try_from(setup.count(&sql)?).unwrap_or(0) >= rows)
    })?;
    setup.sandbox.release_gate(&name)
}

/// A turn-1 script whose flood `rounds` of `per` steps each wait at
/// gates `<prompt>-r0..`, after a gate `<prompt>-start`; a gate is used
/// once per sandbox.
fn rounds_script(prompt: &str, rounds: usize, per: u64) -> Value {
    let mut middle = vec![gate(&format!("{prompt}-start"))];
    for round in 0..rounds {
        middle.push(gate(&format!("{prompt}-r{round}")));
        middle.push(steps(per));
    }
    script(prompt, &middle)
}

/// Design §5.4, §13.2 [t4r16.3, t4r17.2]: `journal_size_limit` cuts a WAL
/// that grew past `checkpoint_bytes` at its reset. With a lowered
/// `wal.max` and an external reader holding a snapshot: a spawn is
/// `store_error` `not_committed` `wal_full` and a queued turn fails `store`
/// at dispatch; a running turn's step rows and terminal commit and it ends
/// normally; a queued-turn cancel and a close commit; a keyed retry of a
/// stored spawn returns its receipt; health stays healthy; the reader
/// closes, a write after 1 s retries `TRUNCATE` and writes resume. WAL
/// growth while the reader holds is recorded for `via-d9o.2.3` (§16).
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario grows the WAL past its limit under a reader and recovers"
)]
fn s1_store_wal_limit_refuses_only_new_work() -> TestResult {
    const ROUNDS: usize = 40;
    const PER: u64 = 40;
    let fixture = json!({"scripts":[
        rounds_script("grow", ROUNDS, PER),
        rounds_script("walrun", ROUNDS, PER),
        script("keyed", &[]),
        script("after", &[]),
    ]});
    let setup = Setup::new(&fixture)?;
    let evidence = setup.evidence("s1_store_wal_limit")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            setup.config(r#"{"wal":{"max":4194304,"checkpoint_bytes":65536}}"#)?;
            let _daemon = setup.start(evidence)?;
            let status = setup.daemon_status(evidence, "status_limits")?;
            check(status["limits"]["wal"]["max"] == 4 * MIB, || {
                format!("{}", status["limits"])
            })?;
            let store = setup.sandbox.state.join("store.sqlite3");
            let reader = |store: &Path| -> Result<rusqlite::Connection, ScenarioError> {
                let reader = rusqlite::Connection::open_with_flags(
                    store,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .map_err(infra)?;
                reader.execute_batch("BEGIN").map_err(infra)?;
                let _: i64 = reader
                    .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
                    .map_err(infra)?;
                Ok(reader)
            };

            // Phase A: a WAL grown past `checkpoint_bytes` under a reader,
            // but below `wal.max`, is cut at its next reset.
            let grow_session = setup.spawn(evidence, "grow")?;
            setup.sandbox.await_gate("grow-start")?;
            let held = reader(&store)?;
            setup.sandbox.release_gate("grow-start")?;
            let next = grow(&setup, (&grow_session, "grow", PER), (0, ROUNDS), |setup| {
                setup.wal_len() > MIB
            })?;
            let grown = setup.wal_len();
            check((MIB..4 * MIB).contains(&grown), || {
                format!("phase A WAL length {grown}")
            })?;
            drop(held);
            drain(&setup, (&grow_session, "grow", PER), (next, ROUNDS))?;
            let envelope = setup.wait(evidence, "wait_grow", &format!("{grow_session}/1"))?;
            check(envelope["state"] == "completed", || format!("{envelope}"))?;
            let cut = setup.wal_len();
            check(cut <= 256 * 1024, || {
                format!("journal_size_limit: WAL {grown} bytes stayed {cut} after its reset")
            })?;

            // Phase B: at `wal.max` under a reader.
            let mut conn = Conn::open(&setup.sandbox)?;
            let mut keyed = spawn_params("keyed");
            keyed["idempotency_key"] = json!("kb");
            let stored = conn.call("spawn", &keyed)?;
            let keyed_session = stored["result"]["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("keyed spawn: {stored}")))?
                .to_owned();
            setup.wait(evidence, "wait_keyed", &format!("{keyed_session}/1"))?;
            let running = setup.spawn(evidence, "walrun")?;
            setup.sandbox.await_gate("walrun-start")?;
            setup.resume(evidence, &running, "second")?;
            setup.resume(evidence, &running, "third")?;
            let held = reader(&store)?;
            let mut peak = 0;
            setup.sandbox.release_gate("walrun-start")?;
            let next = grow(&setup, (&running, "walrun", PER), (0, ROUNDS), |setup| {
                peak = peak.max(setup.wal_len());
                setup.wal_len() >= 4 * MIB
            })?;
            // A Store read is served after the writer's check of the last commit.
            setup.daemon_status(evidence, "status_full")?;
            cli(
                &setup.sandbox,
                evidence,
                "status_running",
                &["status", &running, "--json"],
            )?;
            let refused = conn.call("spawn", &spawn_params("refused"))?;
            check(
                is_error(&refused, -32018, "store_error")
                    && refused["error"]["data"]["commit_outcome"] == "not_committed"
                    && refused["error"]["data"]["kind2"] == "wal_full",
                || format!("spawn at wal.max: {refused}"),
            )?;
            let replay = conn.call("spawn", &keyed)?;
            check(replay["result"] == stored["result"], || {
                format!("keyed retry at wal.max: {replay}")
            })?;
            let cancelled = conn.call(
                "cancel",
                &json!({"session":running,"handle":HANDLE,"turn":3}),
            )?;
            check(cancelled["result"].is_object(), || {
                format!("queued-turn cancel at wal.max: {cancelled}")
            })?;
            drain(&setup, (&running, "walrun", PER), (next, ROUNDS))?;
            let first = setup.wait(evidence, "wait_walrun", &format!("{running}/1"))?;
            check(first["state"] == "completed", || {
                format!("running turn at wal.max: {first}")
            })?;
            let rows = setup.count(&format!(
                "SELECT COUNT(*) FROM steps WHERE session_id='{running}' AND turn=1"
            ))?;
            check(u64::try_from(rows).unwrap_or(0) >= PER, || {
                format!("running turn rows at wal.max: {rows}")
            })?;
            let second = setup.wait(evidence, "wait_second", &format!("{running}/2"))?;
            check(
                second["state"] == "failed"
                    && second["failure"]["class"] == "store"
                    && second["failure"]["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("WAL")),
                || format!("queued turn at wal.max: {second}"),
            )?;
            let closed = conn.call("close", &json!({"session":keyed_session,"handle":HANDLE}))?;
            check(closed["result"].is_object(), || {
                format!("close at wal.max: {closed}")
            })?;
            let status = setup.daemon_status(evidence, "status_healthy")?;
            check(status["health"] == "healthy", || {
                format!("health at wal.max: {}", status["health"])
            })?;
            peak = peak.max(setup.wal_len());
            evidence
                .write(
                    "wal_growth.json",
                    json!({"wal_max":4 * MIB,"checkpoint_bytes":65536,
                        "peak_wal_bytes_while_reader_held":peak,
                        "journal_size_limit_cut":{"grown":grown,"after_reset":cut}})
                    .to_string()
                    .as_bytes(),
                )
                .map_err(infra)?;
            drop(held);

            // The reader is gone: a write 1 s after the last attempt
            // retries TRUNCATE, and new work is admitted again.
            let mut admitted = Value::Null;
            wait_until(
                "a spawn after the reader closed",
                Duration::from_secs(10),
                || {
                    let answer = conn.call("spawn", &spawn_params("after"))?;
                    let done = answer["result"].is_object();
                    admitted = answer;
                    Ok(done)
                },
            )?;
            let session = admitted["result"]["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("{admitted}")))?
                .to_owned();
            let envelope = setup.wait(evidence, "wait_after", &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || format!("{envelope}"))?;
            check(setup.wal_len() < 4 * MIB, || {
                format!("WAL after TRUNCATE: {}", setup.wal_len())
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Design §5.4 (review r1): a WAL left at `wal.max` by a reader that holds
/// it across a restart refuses the first new receipt of the next daemon:
/// the Store checks the WAL it opens, tries one `TRUNCATE` and starts with
/// new work refused while the WAL stays at the limit.
#[test]
fn s1_store_wal_limit_holds_across_a_restart() -> TestResult {
    const ROUNDS: usize = 40;
    const PER: u64 = 40;
    let fixture = json!({"scripts":[rounds_script("grow", ROUNDS, PER), script("after", &[])]});
    let setup = Setup::new(&fixture)?;
    let evidence = setup.evidence("s1_store_wal_restart")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            setup.config(r#"{"wal":{"max":4194304,"checkpoint_bytes":65536}}"#)?;
            let daemon = setup.start(evidence)?;
            let session = setup.spawn(evidence, "grow")?;
            setup.sandbox.await_gate("grow-start")?;
            let store = setup.sandbox.state.join("store.sqlite3");
            let reader = rusqlite::Connection::open_with_flags(
                &store,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(infra)?;
            reader.execute_batch("BEGIN").map_err(infra)?;
            let _: i64 = reader
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
                .map_err(infra)?;
            setup.sandbox.release_gate("grow-start")?;
            drain(&setup, (&session, "grow", PER), (0, ROUNDS))?;
            let envelope = setup.wait(evidence, "wait_grow", &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || format!("{envelope}"))?;
            daemon.shutdown()?;
            let held = setup.wal_len();
            check(held >= 4 * MIB, || {
                format!("the WAL held across the restart is {held} bytes")
            })?;
            let _daemon = setup.start(evidence)?;
            let mut conn = Conn::open(&setup.sandbox)?;
            let refused = conn.call("spawn", &spawn_params("after"))?;
            evidence
                .write(
                    "wal_restart.json",
                    json!({"wal_bytes_at_restart":held,"first_spawn":refused})
                        .to_string()
                        .as_bytes(),
                )
                .map_err(infra)?;
            check(
                is_error(&refused, -32018, "store_error")
                    && refused["error"]["data"]["commit_outcome"] == "not_committed"
                    && refused["error"]["data"]["kind2"] == "wal_full",
                || format!("first spawn after a restart at wal.max: {refused}"),
            )?;
            drop(reader);
            wait_until(
                "a spawn after the reader closed",
                Duration::from_secs(10),
                || Ok(conn.call("spawn", &spawn_params("after"))?["result"].is_object()),
            )
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ----------------------------------------------------- status and describe

/// Design §4.6, §11.2 (A4): `started_at` is set once; of the sessions
/// not closed (+1 at a spawn receipt, −1 at each closed-now answer, seeded
/// at restart), `closing` is the durable closing set, `active` the
/// sessions with an unresolved turn that are not closing and `idle` the
/// rest; `describe` answers the fake's route plan and `models` its one
/// model, with no process and no write; an unknown model is
/// `unknown_model` at `describe` and at `spawn`.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario reads the counts across a close, a restart and the verbs"
)]
fn s1_c1_daemon_status_counts_describe_and_models() -> TestResult {
    let fixture = json!({"scripts":[
        script("held", &[gate("held")]),
        script("closer", &[]),
        script("done", &[]),
    ]});
    let setup = Setup::new(&fixture)?;
    let evidence = setup.evidence("s1_c1_daemon_status_counts")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let daemon = setup.start(evidence)?;
            let pid = setup.pid()?;
            let first = setup.daemon_status(evidence, "status_first")?;
            let started = first["started_at"].clone();
            check(started.is_string(), || format!("started_at: {first}"))?;
            check(
                first["sessions"] == json!({"idle":0,"active":0,"closing":0}),
                || format!("empty daemon: {}", first["sessions"]),
            )?;
            let held = setup.spawn(evidence, "held")?;
            setup.sandbox.await_gate("held")?;
            let done = setup.spawn(evidence, "done")?;
            setup.wait(evidence, "wait_done", &format!("{done}/1"))?;
            let closer = setup.spawn(evidence, "closer")?;
            setup.wait(evidence, "wait_closer", &format!("{closer}/1"))?;
            let point = "store.commit.closed";
            setup.failpoints.arm(point, 1, "pause").map_err(infra)?;
            let mut closing = Conn::open(&setup.sandbox)?;
            closing.send("close", &json!({"session":closer,"handle":HANDLE}))?;
            setup
                .failpoints
                .wait_ack(point, 1, "pause", pid, Duration::from_secs(10))
                .map_err(infra)?;
            let during = setup.daemon_status(evidence, "status_during")?;
            check(
                during["sessions"] == json!({"idle":1,"active":1,"closing":1})
                    && during["started_at"] == started,
                || format!("held, done and closing: {during}"),
            )?;
            setup.failpoints.release(point, 1).map_err(infra)?;
            let answer = closing.reply()?;
            check(answer["result"].is_object(), || format!("close: {answer}"))?;
            setup.sandbox.release_gate("held")?;
            setup.wait(evidence, "wait_held", &format!("{held}/1"))?;
            wait_until("the held turn resolved", Duration::from_secs(10), || {
                let status = setup.daemon_status(evidence, "status_after")?;
                Ok(status["sessions"] == json!({"idle":2,"active":0,"closing":0}))
            })?;

            let mut conn = Conn::open(&setup.sandbox)?;
            let sessions = setup.count("SELECT COUNT(*) FROM sessions")?;
            let described = cli(
                &setup.sandbox,
                evidence,
                "describe",
                &["describe", "--harness", "fake", "--model", "fake", "--json"],
            )?;
            let receipt = setup.count("SELECT COUNT(*) FROM sessions")?;
            check(
                described["harness"] == "fake"
                    && described["model"] == json!({"requested":"fake","resolved":"fake"})
                    && described["route"] == "fake"
                    && described["capabilities"]["verbs"]["spawn"].is_object()
                    && described["refusals"] == json!([])
                    && receipt == sessions,
                || format!("describe: {described}"),
            )?;
            let unknown = conn.call("describe", &json!({"model":"nope"}))?;
            check(is_error(&unknown, -32010, "unknown_model"), || {
                format!("describe of an unknown model: {unknown}")
            })?;
            let strict = conn.call("describe", &json!({"model":"fake","colour":"red"}))?;
            check(
                is_error(&strict, -32602, "invalid_params")
                    && strict["error"]["data"]["kind2"] == "unknown_field",
                || format!("describe with an unknown member: {strict}"),
            )?;
            let refusing = conn.call(
                "describe",
                &json!({"harness":"fake","model":"fake","require":["steer"]}),
            )?;
            check(
                refusing["result"]["refusals"]
                    .as_array()
                    .is_some_and(|refusals| refusals.len() == 1),
                || format!("describe requiring steer: {refusing}"),
            )?;
            let spawn = conn.call(
                "spawn",
                &json!({"harness":"fake","model":"nope","prompt":"p","handle":HANDLE}),
            )?;
            // Design §5.2 (S-CORE chunk 4, design-listed): an uncatalogued
            // model with an explicit harness passes through to the vendor.
            check(
                spawn["result"]["state"] == "queued"
                    && spawn["result"]["effective"]["model"] == "nope",
                || format!("spawn of an uncatalogued model: {spawn}"),
            )?;
            let passed = spawn["result"]["session_id"]
                .as_str()
                .ok_or_else(|| infra(format!("no session in {spawn}")))?
                .to_owned();
            setup.wait(evidence, "wait_passed", &format!("{passed}/1"))?;
            let models = cli(&setup.sandbox, evidence, "models", &["models", "--json"])?;
            check(
                models["models"]
                    == json!([{"model":"fake","harness":"fake","aliases":[],"source":"builtin"}]),
                || format!("models: {models}"),
            )?;
            let other = conn.call("models", &json!({"harness":"codex"}))?;
            check(other["result"]["models"] == json!([]), || {
                format!("models of another harness: {other}")
            })?;
            check(
                setup.count("SELECT COUNT(*) FROM sessions")? == sessions + 1,
                || "describe or models wrote".to_owned(),
            )?;
            drop(conn);
            drop(closing);
            daemon.shutdown()?;

            // Seeded at restart from the sessions not closed.
            let _daemon = setup.start(evidence)?;
            let restarted = setup.daemon_status(evidence, "status_restarted")?;
            check(
                restarted["sessions"] == json!({"idle":3,"active":0,"closing":0})
                    && restarted["started_at"] != started,
                || format!("after a restart: {restarted}"),
            )
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}
