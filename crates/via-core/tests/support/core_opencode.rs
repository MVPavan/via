//! `opencode-serve` under Core's public Engine (`vendors/opencode.md` §13):
//! a case is a private daemon root, the fake vendor (`via-fake-agent` in
//! its `opencode` mode) behind a small `opencode` program, and the session
//! cwds. The program is a shell script that executes the fake as
//! `<case>/fake/opencode`, beside its fixture `opencode.opencode.json`, so
//! the fake's pid is the vendor pid Host spawned, and a test can replace
//! the program at the same path with a new file (C2 §5: a replaced binary
//! is a new file identity). Every fake request is logged with its JSON
//! body in `opencode.requests`; every server start in `opencode.reports`.

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use via_core::{
    AdapterConfig, BootstrapEnv, CloseParams, Deadline, Engine, EventsParams, LogsParams,
    ResumeParams, SessionId, SpawnParams, StatusParams, WaitParams,
};

/// Owns exactly one subprocess, including when a fixture unwinds.
pub(crate) struct OwnedChild(std::process::Child);

impl std::ops::Deref for OwnedChild {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Only this exact spawned Child handle can be stopped by the guard.
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

/// The caller handle every request names.
pub(crate) const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// A wait for a turn expected to end by itself.
const WAIT_MS: u64 = 60_000;

/// How long a fake server may take to be gone after shutdown.
const GONE: Duration = Duration::from_secs(10);

/// The model every case runs.
pub(crate) const MODEL: &str = "opencode/big-pickle";

/// The vendor session ID the fake creates.
pub(crate) const SES: &str = "ses_via0001";

/// A workspace test build's sibling binary.
pub(crate) fn binary(name: &str) -> PathBuf {
    let deps = std::env::current_exe().unwrap();
    let path = deps.parent().unwrap().parent().unwrap().join(name);
    assert!(
        path.is_file(),
        "missing {}; build the workspace first",
        path.display()
    );
    path
}

/// The packet's §5 permission rules, with `{skill,*,deny}` when skills are
/// off.
pub(crate) fn rules(skills_off: bool) -> Value {
    let mut rules = vec![
        json!({"action": "*", "resource": "*", "effect": "allow"}),
        json!({"action": "question", "resource": "*", "effect": "deny"}),
        json!({"action": "opencode_session_move", "resource": "*", "effect": "deny"}),
        json!({"action": "opencode_session_rename", "resource": "*", "effect": "deny"}),
        json!({"action": "opencode_list_mcp_resources", "resource": "*", "effect": "deny"}),
        json!({"action": "opencode_read_mcp_resource", "resource": "*", "effect": "deny"}),
    ];
    if skills_off {
        rules.push(json!({"action": "skill", "resource": "*", "effect": "deny"}));
    }
    Value::Array(rules)
}

/// One catalog entry of `MODEL` with `variants`.
pub(crate) fn catalog_entry(variants: &[&str]) -> Value {
    json!({
        "providerID": "opencode",
        "id": "big-pickle",
        "name": "Big Pickle",
        "variants": variants.iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
    })
}

/// A `Session.Info` readback of [`SES`] at `cwd`.
pub(crate) fn session_info(cwd: &str, variant: &str, rules: &Value) -> Value {
    json!({"data": {
        "id": SES, "projectID": "p", "agent": "via",
        "model": {"providerID": "opencode", "id": "big-pickle", "variant": variant},
        "permissions": rules, "location": {"directory": cwd},
        "cost": 0, "tokens": {}, "time": {"created": 1, "updated": 1},
    }})
}

/// A route answering `responses` in order, the last repeated.
pub(crate) fn route(method: &str, path: &str, responses: Value) -> Value {
    let mut route = json!({"method": method, "path": path});
    route["responses"] = responses;
    route
}

/// A fully compatible 2.0.22 server whose session `SES` lives at `cwd`
/// with the default rules, no variant and VIA's instructions `text`.
pub(crate) fn server(cwd: &str, instructions: Option<&str>) -> Value {
    let entries = match instructions {
        Some(text) => json!({"data": [{"key": "via", "value": text}]}),
        None => json!({"data": []}),
    };
    let fixture = json!({"routes": [
        route("GET", "/api/info", json!([{"status": 200, "json": {"version": "2.0.22", "pid": "$PID"}}])),
        route("GET", "/api/integration", json!([{"status": 200, "json": {"data": []}}])),
        route("GET", "/api/model", json!([{"status": 200, "json": {"data": [catalog_entry(&["high"])]}}])),
        {"method": "GET", "path": "/api/event",
         "sse": {"events": [{"type": "server.connected", "properties": {}}]}},
        route("POST", "/api/session", json!([{"status": 200, "json": session_info(cwd, "default", &rules(false))}])),
        route("GET", &format!("/api/session/{SES}"), json!([{"status": 200, "json": session_info(cwd, "default", &rules(false))}])),
        route("PUT", &format!("/api/experimental/session/{SES}/instructions/entries/via"), json!([{"status": 204}])),
        route("GET", &format!("/api/experimental/session/{SES}/instructions/entries"), json!([{"status": 200, "json": entries}])),
        route("POST", &format!("/api/session/{SES}/model"), json!([{"status": 204}])),
    ]});
    let fixture = with_route(
        fixture,
        route(
            "GET",
            &format!("/api/session/{SES}/inbox"),
            json!([{"status": 200, "json": {"data": []}}]),
        ),
    );
    with_route(fixture, prompt_route(success_events("done")))
}

/// A synthetic event retaining the vendor's observed envelope/data shape.
pub(crate) fn event(kind: &str, mut data: Value) -> Value {
    if data.get("sessionID").is_none() {
        data["sessionID"] = json!("$SESSION");
    }
    json!({"type": kind, "data": data, "id": "evt_fixture", "created": 1})
}

/// Acceptance and owned execution/step, with caller-dependent message keys.
pub(crate) fn begin_events() -> Vec<Value> {
    vec![
        event("session.inbox.enqueued", json!({"inboxID": "$INPUT"})),
        event("session.execution.started", json!({})),
        event("session.inbox.delivered", json!({"inboxID": "$INPUT"})),
        event(
            "session.step.started",
            json!({"assistantMessageID": "$INPUT:a", "agent": "via",
            "model": {"providerID": "opencode", "id": "big-pickle"}, "started": 1}),
        ),
    ]
}

/// One completed model call. Independent values make supersession visible.
pub(crate) fn step_end(message: &str, input: u64, cost: f64) -> Value {
    event(
        "session.step.ended",
        json!({"assistantMessageID": message, "finish": "stop",
        "tokens": {"input": input, "output": 7, "reasoning": 2, "cache": {"read": 3, "write": 4}},
        "cost": cost}),
    )
}

pub(crate) fn success_events(text: &str) -> Vec<Value> {
    let mut events = begin_events();
    events.extend([
        event(
            "session.text.delta",
            json!({"assistantMessageID": "$INPUT:a", "ordinal": 0, "delta": text}),
        ),
        event(
            "session.text.ended",
            json!({"assistantMessageID": "$INPUT:a", "ordinal": 0, "text": text}),
        ),
        step_end("$INPUT:a", 11, 0.25),
        event("session.execution.succeeded", json!({})),
    ]);
    events
}

pub(crate) fn prompt_response(events: Vec<Value>) -> Value {
    json!({"status": 200, "json": {"data": {"id": "$INPUT", "sessionID": "$SESSION"}}, "emit": events.into_iter().collect::<Value>()})
}

pub(crate) fn prompt_route(events: Vec<Value>) -> Value {
    route(
        "POST",
        &format!("/api/session/{SES}/prompt"),
        json!([prompt_response(events)]),
    )
}

/// `fixture` with the routes of `method` and `path` replaced by `route`,
/// placed first (a location's route must precede the plain one).
pub(crate) fn with_route(mut fixture: Value, route: Value) -> Value {
    let routes = fixture["routes"].as_array_mut().unwrap();
    routes.retain(|existing| {
        existing["path"] != route["path"] || existing["method"] != route["method"]
    });
    routes.insert(0, route);
    fixture
}

/// `GET /api/model?location[directory]=<cwd>`, as the route encodes it.
pub(crate) fn location_target(cwd: &str) -> String {
    let mut encoded = String::new();
    for byte in cwd.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").unwrap();
        }
    }
    format!("/api/model?location[directory]={encoded}")
}

/// One case: its daemon root, the program and the fake's folder.
pub(crate) struct Case {
    root: tempfile::TempDir,
    program: PathBuf,
    fake: PathBuf,
}

impl Case {
    /// A private root (`state/vendor`, `runtime/anchors`) and the program
    /// serving `fixture`.
    pub(crate) fn new(fixture: &Value) -> Self {
        let root = tempfile::Builder::new()
            .prefix("via-oc-core-")
            .tempdir_in("/tmp")
            .unwrap();
        for part in [
            "state",
            "state/vendor",
            "runtime",
            "runtime/anchors",
            "fake",
            "cwd",
        ] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.path().join(part))
                .unwrap();
        }
        let fake = root.path().join("fake").join("opencode");
        std::os::unix::fs::symlink(binary("via-fake-agent"), &fake).unwrap();
        let case = Self {
            program: root.path().join("opencode"),
            fake,
            root,
        };
        case.write_program();
        case.fixture(fixture);
        case
    }

    /// Writes the program as a new file (a new inode) at its path.
    fn write_program(&self) {
        let staged = self.root.path().join(".opencode.new");
        let script = format!("#!/bin/sh\nexec '{}' \"$@\"\n", self.fake.display());
        fs::write(&staged, script).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&staged, &self.program).unwrap();
    }

    /// Replaces the program at its path with another file (C2 §5).
    pub(crate) fn replace_program(&self) {
        let before = fs::metadata(&self.program).unwrap();
        self.write_program();
        let after = fs::metadata(&self.program).unwrap();
        assert_ne!(before.ino(), after.ino(), "a new file");
    }

    /// Serves `fixture` from the next server start on.
    pub(crate) fn fixture(&self, fixture: &Value) {
        fs::write(self.side(".opencode.json"), fixture.to_string()).unwrap();
    }

    fn side(&self, suffix: &str) -> PathBuf {
        let mut name = self.fake.clone().into_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    fn lines(&self, suffix: &str) -> Vec<Value> {
        fs::read_to_string(self.side(suffix))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Every request the fake servers answered, in order.
    pub(crate) fn requests(&self) -> Vec<Value> {
        self.lines(".requests")
    }

    /// Sanitized event-type/write-time audit in the fake's stream order.
    pub(crate) fn frames(&self) -> Vec<Value> {
        self.lines(".frames")
    }

    /// The requests of `method` whose target starts with `prefix`.
    pub(crate) fn requests_to(&self, method: &str, prefix: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| {
                request["method"] == method
                    && request["target"]
                        .as_str()
                        .is_some_and(|target| target.starts_with(prefix))
            })
            .collect()
    }

    /// The session creations, `POST /api/session` exactly.
    pub(crate) fn creates(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| request["method"] == "POST" && request["target"] == "/api/session")
            .collect()
    }

    /// How many servers started.
    pub(crate) fn starts(&self) -> usize {
        self.lines(".reports").len()
    }

    /// Asserts that every fake server is gone.
    pub(crate) async fn all_gone(&self) {
        for report in self.lines(".reports") {
            let pid = report["pid"].as_u64().unwrap();
            let proc = PathBuf::from(format!("/proc/{pid}"));
            let by = tokio::time::Instant::now() + GONE;
            while proc.exists() {
                assert!(
                    tokio::time::Instant::now() < by,
                    "fake server {pid} still present"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let pattern = format!("^{}( |$)", self.fake.display());
        let result = std::process::Command::new("pgrep")
            .args(["-f", &pattern])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path())
            .env("XDG_DATA_HOME", self.root.path())
            .env("XDG_STATE_HOME", self.root.path())
            .env("XDG_CACHE_HOME", self.root.path())
            .env("XDG_RUNTIME_DIR", self.root.path())
            .env("TMPDIR", self.root.path())
            .output()
            .unwrap();
        assert_eq!(
            result.status.code(),
            Some(1),
            "owned fake still found: {}",
            String::from_utf8_lossy(&result.stdout)
        );
    }

    /// Durable Host group-absence proofs after daemon shutdown. This reads
    /// only the case's private database, after its Store writer has stopped.
    pub(crate) fn proven_server_groups(&self) -> usize {
        let db = rusqlite::Connection::open_with_flags(
            self.root.path().join("state/store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let count: u32 = db.query_row("SELECT count(*) FROM anchors WHERE owner_server IS NOT NULL AND absence_time IS NOT NULL", [], |row| row.get(0)).unwrap();
        usize::try_from(count).unwrap()
    }

    /// A session cwd `name` under the case, created.
    pub(crate) fn cwd(&self, name: &str) -> String {
        let path = self.root.path().join("cwd").join(name);
        if !path.exists() {
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        }
        path.to_str().unwrap().to_owned()
    }

    /// The daemon's vendor state directory.
    pub(crate) fn vendor(&self) -> PathBuf {
        self.root.path().join("state").join("vendor")
    }

    /// The adapter config pinning `opencode` to the program, with the
    /// harness's `inherit` (`daemon.json`'s booleans) when given.
    fn config(&self, inherit: Option<&Value>) -> AdapterConfig {
        let mut harness = json!({"binary": self.program});
        if let Some(inherit) = inherit {
            harness["inherit"] = inherit.clone();
        }
        let raw =
            serde_json::value::RawValue::from_string(json!({ "opencode": harness }).to_string())
                .unwrap();
        AdapterConfig::load(BootstrapEnv::capture(), Some(&raw)).unwrap()
    }

    /// Every file under the daemon root, with its bytes.
    pub(crate) fn daemon_files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        for part in ["state", "runtime"] {
            walk(&self.root.path().join(part), &mut files);
        }
        files
    }

    /// An owned subprocess running the same test under the case's private roots.
    pub(crate) fn crash_child(&self, test: &str) -> OwnedChild {
        self.owned_child(
            std::process::Command::new(std::env::current_exe().unwrap()).args([
                "--exact",
                test,
                "--nocapture",
            ]),
        )
    }

    /// Every owned subprocess has the same private roots as the fixture.
    pub(crate) fn owned_child(&self, command: &mut std::process::Command) -> OwnedChild {
        OwnedChild(
            command
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", self.root.path())
                .env("XDG_CONFIG_HOME", self.root.path())
                .env("XDG_DATA_HOME", self.root.path())
                .env("XDG_STATE_HOME", self.root.path())
                .env("XDG_CACHE_HOME", self.root.path())
                .env("XDG_RUNTIME_DIR", self.root.path())
                .env("TMPDIR", self.root.path())
                .env("VIA_OC_CRASH_ROOT", self.root.path())
                .current_dir(self.root.path())
                .spawn()
                .unwrap(),
        )
    }

    pub(crate) fn crash_marker(&self) -> PathBuf {
        self.root.path().join("crash-accepted.json")
    }

    /// Opens a daemon on the case's root.
    pub(crate) fn daemon(&self) -> Daemon {
        Daemon::open(self.root.path(), self.config(None))
    }

    /// Opens a daemon on the case's root whose `harnesses.opencode.inherit`
    /// is `inherit`.
    pub(crate) fn daemon_with(&self, inherit: &Value) -> Daemon {
        Daemon::open(self.root.path(), self.config(Some(inherit)))
    }
}

fn walk(dir: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let path = entry.path();
        if kind.is_dir() {
            walk(&path, files);
        } else if kind.is_file() {
            files.push((path.clone(), fs::read(&path).unwrap_or_default()));
        }
    }
}

/// One daemon's Engine, whose session dispatchers run as daemon main runs
/// them.
pub(crate) struct Daemon {
    engine: Arc<Engine>,
    starter: tokio::task::JoinHandle<()>,
    dispatchers: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Daemon {
    /// The owned crash subprocess opens exactly the parent's private fixture.
    pub(crate) fn crash_fixture(root: &Path) -> Self {
        let raw = serde_json::value::RawValue::from_string(
            json!({"opencode":{"binary":root.join("opencode")}}).to_string(),
        )
        .unwrap();
        let adapters = AdapterConfig::load(BootstrapEnv::capture(), Some(&raw)).unwrap();
        Self::open(root, adapters)
    }

    fn open(root: &Path, adapters: AdapterConfig) -> Self {
        let engine = Engine::open(
            &root.join("state"),
            &root.join("runtime"),
            adapters,
            binary("via"),
        )
        .unwrap();
        let mut starts = engine.take_starts().unwrap();
        let starting = Arc::clone(&engine);
        let dispatchers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let joins = Arc::clone(&dispatchers);
        let starter = tokio::spawn(async move {
            while let Some(session) = starts.recv().await {
                let engine = Arc::clone(&starting);
                let join = tokio::spawn(async move {
                    let _ = engine.dispatcher(session).await;
                });
                joins.lock().unwrap().push(join);
            }
        });
        Self {
            engine,
            starter,
            dispatchers,
        }
    }

    /// Spawns an `opencode` session on `cwd` with `extra` members; its ID,
    /// or the C1 error's `data`.
    pub(crate) async fn try_spawn(
        &self,
        cwd: &str,
        extra: &Value,
    ) -> Result<(SessionId, Value), Value> {
        let mut raw = json!({
            "harness": "opencode", "model": MODEL, "prompt": "Say done.", "handle": HANDLE,
            "cwd": cwd, "bound": {"mode": "full", "extra_write_dirs": [], "network": true},
        });
        for (member, value) in extra.as_object().into_iter().flatten() {
            raw[member] = value.clone();
        }
        let params: SpawnParams = serde_json::from_value(raw.clone()).unwrap();
        match self.engine.spawn(params, &raw.to_string()).await {
            Ok(receipted) => Ok((receipted.enqueued.unwrap().0, receipted.receipt)),
            Err(error) => Err(error.data()),
        }
    }

    /// [`Self::try_spawn`], which must be receipted.
    pub(crate) async fn spawn(&self, cwd: &str, extra: &Value) -> (SessionId, Value) {
        match self.try_spawn(cwd, extra).await {
            Ok(spawned) => spawned,
            Err(data) => panic!("spawn refused: {data}"),
        }
    }

    /// Queues a turn on `session` with `extra` members; the C1 error's
    /// `data` when refused.
    pub(crate) async fn try_resume(
        &self,
        session: &SessionId,
        extra: &Value,
    ) -> Result<Value, Value> {
        let mut raw = json!({"session": session, "handle": HANDLE, "prompt": "Again."});
        for (member, value) in extra.as_object().into_iter().flatten() {
            raw[member] = value.clone();
        }
        let params: ResumeParams = serde_json::from_value(raw.clone()).unwrap();
        match self.engine.resume(params, &raw.to_string()).await {
            Ok(receipted) => Ok(receipted.receipt),
            Err(error) => Err(error.data()),
        }
    }

    /// The envelope of `session`'s turn `turn` once it is terminal.
    pub(crate) async fn wait(&self, session: &SessionId, turn: u32) -> Value {
        self.wait_result(session, turn).await.unwrap()
    }

    /// A fallible wait so a proving fixture shuts down before unwrapping an
    /// unexpected timeout or Store error.
    pub(crate) async fn wait_result(
        &self,
        session: &SessionId,
        turn: u32,
    ) -> Result<Value, via_core::ApiError> {
        let params = WaitParams {
            address: format!("{session}/{turn}"),
            timeout_ms: Some(WAIT_MS),
        };
        let envelope = self.engine.wait(params).await?;
        Ok(serde_json::from_str(envelope.get()).unwrap())
    }

    /// `session`'s C1 status.
    pub(crate) async fn status(&self, session: &SessionId) -> Value {
        let params: StatusParams = serde_json::from_value(json!({"session": session})).unwrap();
        self.engine.status(params).await.unwrap()
    }

    /// Public event records for this turn.
    pub(crate) async fn events(&self, session: &SessionId, turn: u32) -> Value {
        let params: EventsParams =
            serde_json::from_value(json!({"turn": format!("{session}/{turn}"), "limit": 1000}))
                .unwrap();
        let events = self.engine.events(params).await.unwrap();
        serde_json::from_str(events.get()).unwrap()
    }

    pub(crate) async fn logs(&self, session: &SessionId, turn: u32) -> Value {
        let params: LogsParams =
            serde_json::from_value(json!({"turn": format!("{session}/{turn}")})).unwrap();
        self.engine.logs(params).await.unwrap()
    }

    /// Closes `session` gracefully.
    pub(crate) async fn close(&self, session: &SessionId) -> Value {
        let raw = json!({"session": session, "handle": HANDLE});
        let params: CloseParams = serde_json::from_value(raw.clone()).unwrap();
        self.engine.close(params, &raw.to_string()).await.unwrap()
    }

    /// The daemon's public startup recovery sequence, before new admission.
    pub(crate) async fn recover(&self) {
        self.engine.recover().await.unwrap();
        self.engine.bound_resumed_paging().await.unwrap();
        self.engine.hand_off_queued().await.unwrap();
    }

    /// Clean shutdown, then every task holding the Engine ends, so the
    /// Store lock is free for the next daemon on the same root.
    pub(crate) async fn stop(self) {
        let report = self
            .engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        assert!(report.is_clean(), "{report:?}");
        self.starter.abort();
        let _ = self.starter.await;
        let handles = std::mem::take(&mut *self.dispatchers.lock().unwrap());
        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
        assert_eq!(Arc::strong_count(&self.engine), 1, "the Engine is held");
    }
}

/// The failure class of an envelope.
pub(crate) fn class(envelope: &Value) -> &str {
    envelope["failure"]["class"].as_str().unwrap_or_default()
}

/// Setup fixtures now require a real successful turn; setup checks remain
/// independently asserted by each caller.
pub(crate) fn setup_succeeded(envelope: &Value) {
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert!(envelope["failure"].is_null(), "{envelope}");
}
