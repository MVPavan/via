//! Actual daemon OC01/OC02/OC12 conformance (opencode.md §4.3, §12–§13).

mod errors;
mod fixtures;
mod scan;
mod secrecy;
mod status;

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::daemon::{Daemon, Raw, Sandbox, TestResult, collect_available, failure, infra, request};
use crate::scenario::{ScenarioError, run_scenario};
use crate::support::evidence::Evidence;

const HANDLE: &str = crate::HANDLE;
/// C1 §3.6: a bounded daemon wait, covering the fake's handshake and terminal.
const WAIT_MS: u32 = 30_000;

/// One private daemon and fake deployment (opencode.md §13, fakes only).
struct Case {
    sandbox: Sandbox,
    program: PathBuf,
    vendor: PathBuf,
    root: PathBuf,
}

impl Case {
    fn new() -> TestResult<Self> {
        let sandbox = Sandbox::new_private(&json!({"scripts":[]}))?;
        let root = sandbox.fixture.parent().ok_or("missing sandbox root")?;
        for name in ["a", "b", "bin"] {
            crate::daemon::private_dir(&root.join(name))?;
        }
        let root = root.to_owned();
        let program = root.join("bin/opencode");
        symlink(&sandbox.fake, &program)?;
        fs::write(
            sandbox.state.join("daemon.json"),
            serde_json::to_vec(&json!({"harnesses":{"opencode":{"binary":program}}}))?,
        )?;
        let vendor = sandbox.state.join("vendor");
        Ok(Self {
            sandbox,
            program,
            vendor,
            root,
        })
    }

    fn cwd(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn fixture(&self, fixture: &Value) -> Result<(), ScenarioError> {
        fs::write(
            self.program.with_extension("opencode.json"),
            serde_json::to_vec(fixture).map_err(infra)?,
        )
        .map_err(infra)
    }

    fn state(&self) -> &Path {
        &self.sandbox.state
    }
    fn vendor(&self) -> &Path {
        &self.vendor
    }

    fn namespace(&self) -> PathBuf {
        let mut hash = Sha256::new();
        for field in [
            b"via-opencode-namespace-v1".as_slice(),
            b"opencode-free-anonymous-v1".as_slice(),
            &1_u64.to_le_bytes(),
        ] {
            hash.update((field.len() as u64).to_le_bytes());
            hash.update(field);
        }
        let mut key = String::new();
        for byte in &hash.finalize()[..8] {
            // Formatting into String is infallible.
            let _ = write!(key, "{byte:02x}");
        }
        self.vendor.join("opencode").join(key)
    }

    fn audit(&self, suffix: &str) -> Result<Vec<Value>, ScenarioError> {
        let path = self.program.with_extension(suffix);
        if !path.exists() {
            return Ok(Vec::new());
        }
        fs::read_to_string(path)
            .map_err(infra)?
            .lines()
            .map(|line| serde_json::from_str(line).map_err(|_| failure("invalid fake audit")))
            .collect()
    }
    fn requests(&self) -> Result<Vec<Value>, ScenarioError> {
        self.audit("requests")
    }
    fn reports(&self) -> Result<Vec<Value>, ScenarioError> {
        self.audit("reports")
    }
    fn versions(&self) -> Result<Vec<Value>, ScenarioError> {
        self.audit("versions")
    }

    /// Snapshot while live; final cleanup collects once more (runtime §11.2).
    fn collect(&self, evidence: &Evidence) -> Result<(), ScenarioError> {
        let store = self.state().join("store.sqlite3");
        if store.exists() {
            evidence.backup_store(&store).map_err(infra)?;
        }
        for name in ["via.log", "via.log.1"] {
            let path = self.state().join(name);
            if path.exists() {
                evidence
                    .write(name, &fs::read(path).map_err(infra)?)
                    .map_err(infra)?;
            }
        }
        Ok(())
    }

    fn scenario(
        &self,
        name: &str,
        action: impl FnOnce(&Self, &Evidence, &mut Raw) -> Result<(), ScenarioError>,
    ) -> TestResult {
        self.scenario_scanned(name, action, |_, _| Ok(()))
    }

    fn scenario_scanned(
        &self,
        name: &str,
        action: impl FnOnce(&Self, &Evidence, &mut Raw) -> Result<(), ScenarioError>,
        after: impl Fn(&Self, &Path) -> Result<(), ScenarioError>,
    ) -> TestResult {
        // Hash the actual initial OpenCode stimulus; keep raw fixture bytes outside VIA evidence.
        let stimulus = self.root.join("opencode-stimulus.json");
        fs::copy(self.program.with_extension("opencode.json"), &stimulus)?;
        let mut evidence = Evidence::new(name, &self.sandbox.fake, &stimulus)?;
        evidence.folders_expected = name != "oc01_daemon_version_probe_refusal";
        evidence.write("envelopes.ndjson", b"")?;
        evidence.write("events.ndjson", b"")?;
        let report = run_scenario(
            evidence,
            |evidence| {
                let _daemon = Daemon::start(&self.sandbox, evidence)?;
                let mut raw = Raw::open(&self.sandbox)?;
                action(self, evidence, &mut raw)
            },
            |evidence| {
                collect_available(evidence, self.state(), &self.sandbox.teardown)?;
                // Record canonical rows from the consistent, post-shutdown backup (runtime §11.2).
                let store = rusqlite::Connection::open_with_flags(
                    evidence.dir.join("store.sqlite3"),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .map_err(infra)?;
                for (file, query) in [
                    (
                        "envelopes.ndjson",
                        "SELECT envelope FROM turns WHERE envelope IS NOT NULL ORDER BY session_id, number",
                    ),
                    (
                        "events.ndjson",
                        "SELECT event FROM events ORDER BY session_id, seq",
                    ),
                ] {
                    let mut statement = store.prepare(query).map_err(infra)?;
                    let rows = statement
                        .query_map([], |row| row.get::<_, String>(0))
                        .map_err(infra)?;
                    let mut bytes = Vec::new();
                    for row in rows {
                        bytes.extend_from_slice(row.map_err(infra)?.as_bytes());
                        bytes.push(b'\n');
                    }
                    evidence.write(file, &bytes).map_err(infra)?;
                }
                for report in self.reports()?.into_iter().chain(self.versions()?) {
                    let pid = report["pid"]
                        .as_u64()
                        .ok_or_else(|| failure("missing owned PID"))?;
                    let found = std::process::Command::new("pgrep")
                        .args([
                            "-f",
                            self.program
                                .to_str()
                                .ok_or_else(|| failure("invalid owned program path"))?,
                        ])
                        .env_clear()
                        .env("PATH", "/usr/bin:/bin")
                        .env("HOME", self.cwd("home"))
                        .env("XDG_CONFIG_HOME", self.cwd("home/config"))
                        .env("XDG_DATA_HOME", self.cwd("home/data"))
                        .env("XDG_CACHE_HOME", self.cwd("home/cache"))
                        .output()
                        .map_err(infra)?;
                    if !matches!(found.status.code(), Some(0 | 1)) {
                        return Err(infra("pgrep could not verify owned fake cleanup"));
                    }
                    if String::from_utf8_lossy(&found.stdout)
                        .lines()
                        .any(|line| line == pid.to_string())
                    {
                        return Err(failure("owned OpenCode fake remains after cleanup"));
                    }
                }
                after(self, &evidence.dir)
            },
        );
        report.require_pass()?;
        // Include final summary and manifest, which the scenario writes after collection.
        after(self, &report.artifact)?;
        Ok(())
    }
}

fn rpc(raw: &mut Raw, method: &str, params: &Value) -> Result<Value, ScenarioError> {
    raw.exchange(&request(1, method, params))
}

fn call(raw: &mut Raw, method: &str, params: &Value) -> Result<Value, ScenarioError> {
    rpc(raw, method, params)?
        .get("result")
        .cloned()
        .ok_or_else(|| failure(format!("{method} refused")))
}

fn spawn(raw: &mut Raw, cwd: &Path, extra: &Value) -> Result<String, ScenarioError> {
    let mut params = json!({"harness":"opencode","model":"opencode/big-pickle","cwd":cwd,
        "prompt":"Say done.","handle":HANDLE,
        "bound":{"mode":"full","extra_write_dirs":[],"network":true}});
    for (key, value) in extra.as_object().into_iter().flatten() {
        params[key] = value.clone();
    }
    call(raw, "spawn", &params)?["session_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| failure("missing session receipt"))
}

fn wait_turn(raw: &mut Raw, session: &str, turn: u32) -> Result<Value, ScenarioError> {
    call(
        raw,
        "wait",
        &json!({"address":format!("{session}/{turn}"),"timeout_ms":WAIT_MS}),
    )
}
