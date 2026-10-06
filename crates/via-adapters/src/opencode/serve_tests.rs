//! Chunk A's real-route fixtures (`vendors/opencode.md` §13: OC01, OC02,
//! `OC02b`, OC10, OC12, `OC12b`, route parts): the adapter's recipe through
//! the `OpenCode` registry, Wire and a real Host anchor (the built `via`)
//! and Store, against the scripted fake vendor (`via-fake-agent` started
//! as `opencode` beside its fixture). Every process started has private
//! roots under the test's own folders, and each test proves every fake
//! server it started gone before it ends.

use std::collections::BTreeSet;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use via_routes::codex::LossCause;
use via_routes::codex::ServerId;
use via_routes::codex::testing::TestRuntime;
use via_routes::opencode::{
    CatalogModel, GenerationEnd, HANDSHAKE, LaunchError, LaunchFailure, Refusal, SILENCE,
    ServerPin, Servers,
};
use via_routes::{RouteError, RouteRuntime, TurnNumber};

use super::launch::{CONFIG_CONTENT, PrivateRoot};
use super::{CHECKED, OpenCodeServers};
use crate::ProcessOwner;

/// The bound on one fixture's launch, beyond the registry's own.
const WAIT: Duration = Duration::from_secs(40);

/// How long a retired or lost fake may take to be gone.
const GONE: Duration = Duration::from_secs(10);

/// A synthetic secret: it must reach no file VIA writes.
const STDERR_SECRET: &str = "SYNTHETIC-STDERR-SECRET-9f3c";

/// A synthetic provider key in a catalog entry.
const CATALOG_SECRET: &str = "SYNTHETIC-CATALOG-KEY-4d2a";

/// The workspace binary `name`, beside this test's directory.
fn sibling(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().unwrap().parent().unwrap().join(name);
    assert!(
        path.is_file(),
        "missing {}; build the workspace first",
        path.display()
    );
    path
}

fn model() -> Value {
    json!({
        "providerID": "opencode",
        "id": "big-pickle",
        "name": "Big Pickle",
        "variants": [{"id": "high", "settings": {"apiKey": CATALOG_SECRET}}],
        "settings": {"apiKey": CATALOG_SECRET},
        "headers": {"Authorization": CATALOG_SECRET},
        "options": {"baseURL": CATALOG_SECRET},
    })
}

/// A fully compatible 2.0.22 server.
fn compatible() -> Value {
    json!({
        "routes": [
            {"method": "GET", "path": "/api/info",
             "responses": [{"status": 200, "json": {"version": "2.0.22", "pid": "$PID"}}]},
            {"method": "GET", "path": "/api/integration",
             "responses": [{"status": 200, "json": {"data": []}}]},
            {"method": "GET", "path": "/api/model",
             "responses": [{"status": 200, "json": {"data": [model()]}}]},
            {"method": "GET", "path": "/api/event",
             "sse": {"events": [{"type": "server.connected", "properties": {}}]}},
        ]
    })
}

/// `fixture` with `path`'s route replaced by `route` (added when absent).
fn with_route(mut fixture: Value, route: Value) -> Value {
    let routes = fixture["routes"].as_array_mut().unwrap();
    routes.retain(|existing| existing["path"] != route["path"]);
    routes.push(route);
    fixture
}

fn get(path: &str, responses: &Value) -> Value {
    json!({"method": "GET", "path": path, "responses": responses})
}

fn events(sse: &Value) -> Value {
    json!({"method": "GET", "path": "/api/event", "sse": sse})
}

/// `fixture` with top-level `key` set.
fn with(mut fixture: Value, key: &str, value: Value) -> Value {
    fixture[key] = value;
    fixture
}

/// One registry over a real Route runtime whose anchor is the built `via`,
/// and the fake vendor as `opencode` in a folder of its own.
struct Rig {
    store: TestRuntime,
    runtime: Arc<RouteRuntime>,
    servers: Arc<Servers>,
    adapter: OpenCodeServers,
    /// The fake's folder, kept for the test's life.
    _fake: tempfile::TempDir,
    program: PathBuf,
}

impl Rig {
    fn new(fixture: &Value) -> Self {
        Self::bounded(fixture, HANDSHAKE)
    }

    fn bounded(fixture: &Value, bound: Duration) -> Self {
        let store = TestRuntime::new();
        let (mut config, resources) = store.parts();
        config.anchor_binary = sibling("via");
        let runtime = Arc::new(RouteRuntime::new(config, resources).unwrap());
        let servers = Servers::with_handshake_bound(Arc::clone(&runtime), bound);
        let fake = tempfile::tempdir().unwrap();
        let program = fake.path().join("opencode");
        std::os::unix::fs::symlink(sibling("via-fake-agent"), &program).unwrap();
        let adapter = OpenCodeServers::new(
            &program,
            Some(std::ffi::OsStr::new("/usr/bin:/bin")),
            Arc::clone(&servers),
        );
        let rig = Self {
            store,
            runtime,
            servers,
            adapter,
            _fake: fake,
            program,
        };
        rig.fixture(fixture);
        rig
    }

    fn fixture(&self, fixture: &Value) {
        std::fs::write(
            self.side(".opencode.json"),
            serde_json::to_vec(fixture).unwrap(),
        )
        .unwrap();
    }

    fn side(&self, suffix: &str) -> PathBuf {
        let mut name = self.program.clone().into_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    fn lines(&self, suffix: &str) -> Vec<Value> {
        std::fs::read_to_string(self.side(suffix))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The fake servers' start reports, in order.
    fn reports(&self) -> Vec<Value> {
        self.lines(".reports")
    }

    /// The requests the fake servers answered, in order.
    fn requests(&self) -> Vec<Value> {
        self.lines(".requests")
    }

    fn targets(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|request| request["target"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The test's own root: `state/`, `runtime/` and `vendor/`.
    fn root(&self) -> PathBuf {
        self.vendor_state_dir().parent().unwrap().to_owned()
    }

    fn vendor_state_dir(&self) -> PathBuf {
        self.servers.vendor_state_dir().to_owned()
    }

    fn namespace(&self) -> PrivateRoot {
        PrivateRoot::namespace(&self.vendor_state_dir())
    }

    fn acquire(&self, capacity: via_routes::CapacityToken) -> Result<ServerPin, LaunchFailure> {
        let owner = ProcessOwner::Server {
            server_id: ServerId::try_from("v_000000000000").unwrap(),
        };
        self.adapter.acquire(owner, capacity)
    }

    /// One acquisition awaited to its launch's end.
    async fn launch(&self) -> Result<ServerPin, LaunchError> {
        let pin = self.acquire(Box::new(())).map_err(LaunchError::from)?;
        let ready = tokio::time::timeout(WAIT, pin.ready(std::future::pending()))
            .await
            .expect("the launch ends within its bound");
        match ready {
            Ok(()) => Ok(pin),
            Err(error) => Err(error.expect("only the launch ends the wait")),
        }
    }

    /// A launch expected to fail: its failure, once its process is gone.
    async fn refused(&self) -> LaunchError {
        let before = self.reports().len();
        let error = self.launch().await.expect_err("the launch fails");
        for report in &self.reports()[before..] {
            gone(pid(report)).await;
        }
        error
    }

    /// Every file VIA's state root holds (Store, evidence, anchors,
    /// namespace), as bytes; the fake's own folder is the test's.
    fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        walk(&self.root(), &mut files);
        files
    }

    /// Fences the registry, joins it beside the runtime's shutdown, and
    /// proves every fake server gone.
    async fn finish(self) {
        // As `AdapterSet::shutdown`: fenced, then joined beside Host's
        // shutdown, which stops every live server.
        self.servers.fence();
        let deadline = via_routes::Deadline::at(tokio::time::Instant::now() + GONE);
        let (_shutdown, joined) = tokio::join!(
            self.runtime.shutdown(deadline, &[]),
            self.servers.join(deadline)
        );
        assert_eq!(joined, (0, 0));
        for report in self.reports() {
            gone(pid(&report)).await;
        }
        drop(self.store);
    }
}

fn walk(dir: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let path = entry.path();
        if kind.is_dir() {
            walk(&path, files);
        } else if kind.is_file() {
            let mut bytes = Vec::new();
            // A socket or a file another process holds may not open.
            if let Ok(mut file) = std::fs::File::open(&path) {
                let _ = file.read_to_end(&mut bytes);
            }
            files.push((path, bytes));
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn pid(report: &Value) -> u32 {
    u32::try_from(report["pid"].as_u64().unwrap()).unwrap()
}

/// Waits until `pid` is gone (reaped), failing after [`GONE`].
async fn gone(pid: u32) {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let deadline = tokio::time::Instant::now() + GONE;
    while proc.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "fake server {pid} still present"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn turn() -> TurnNumber {
    TurnNumber::try_from(1).unwrap()
}

/// The launch's failure as its waiting turn reports it.
fn cause(error: &LaunchError) -> RouteError {
    error.failure.clone().route_failure(turn()).cause
}

/// The step a transient failure names.
fn step(error: &LaunchError) -> Option<&'static str> {
    error
        .failure
        .clone()
        .route_failure(turn())
        .launch
        .map(|launch| launch.step)
}

/// OC02: the exact argv, cwd, environment and generated configuration;
/// the version check in the probe root; a fresh namespace makes no
/// integration call; only the handshake's endpoints are requested, all
/// authenticated; the vendor pid matches `/api/info.pid` (`OC02b`); the
/// stderr is counted only and nothing is captured (`OC12b`); retirement
/// leaves the process gone.
#[tokio::test]
async fn oc02_launch_environment_and_fresh_handshake() {
    let rig = Rig::new(&with(compatible(), "stderr", json!(STDERR_SECRET)));
    let pin = rig.launch().await.expect("admitted");
    let (server, facts) = pin.live().expect("live");
    let reports = rig.reports();
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert_eq!(
        report["argv"],
        json!([
            rig.program.to_str().unwrap(),
            "serve",
            "--stdio",
            "--hostname",
            "127.0.0.1",
            "--port",
            "0"
        ])
    );
    let namespace = rig.namespace();
    assert_eq!(report["cwd"], json!(namespace.path().to_str().unwrap()));
    let env = report["env"].as_object().unwrap();
    let names: BTreeSet<&str> = env.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "HOME",
            "LANG",
            "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_DISABLE_AUTOUPDATE",
            "OPENCODE_PASSWORD",
            "PATH",
            "TMPDIR",
            "VIA_PROCESS_MARKER",
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_RUNTIME_DIR",
            "XDG_STATE_HOME",
        ])
    );
    let under = |part: &str| json!(namespace.path().join(part).to_str().unwrap());
    for (name, part) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("TMPDIR", "tmp"),
    ] {
        assert_eq!(env[name], under(part), "{name}");
    }
    assert_eq!(env["LANG"], json!("C.UTF-8"));
    assert_eq!(env["PATH"], json!("/usr/bin:/bin"));
    assert_eq!(env["OPENCODE_DISABLE_AUTOUPDATE"], json!("1"));
    assert_eq!(env["OPENCODE_CONFIG_CONTENT"], json!(CONFIG_CONTENT));
    let config: Value = serde_json::from_str(CONFIG_CONTENT).unwrap();
    assert_eq!(config["default_agent"], json!("via"));
    assert_eq!(config["permission"]["question"], json!("deny"));
    assert_eq!(env["OPENCODE_PASSWORD"], json!(true));
    assert_eq!(report["password_len"], json!(64));
    // The version check ran in the probe root, never the namespace.
    let probe = PrivateRoot::probe(&rig.vendor_state_dir());
    assert!(probe.path().join("home").join("via-fake-version").is_file());
    assert!(
        !namespace
            .path()
            .join("home")
            .join("via-fake-version")
            .exists()
    );
    let versions = rig.lines(".versions");
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0]["password"], json!(false));
    // A fresh namespace: no integration call; only the handshake's
    // endpoints, each authenticated.
    assert_eq!(rig.targets(), ["/api/info", "/api/model", "/api/event"]);
    assert!(rig.requests().iter().all(|request| request["auth"] == "ok"));
    // Identity: Host's vendor pid is the server's.
    assert_eq!(server.vendor_pid(), pid(report));
    assert_eq!(facts.vendor_pid, pid(report));
    assert_eq!(facts.version, "2.0.22");
    assert!(!facts.credential_unchecked);
    let reports_listed = rig.servers.reports();
    assert_eq!(reports_listed.len(), 1);
    assert_eq!(reports_listed[0].key.len(), 16);
    assert_eq!(reports_listed[0].version, "2.0.22");
    // `OC12b`: the server's evidence folder has no stderr.log and nothing
    // undecoded; the stderr secret reached no file.
    let evidence = rig.root().join("state").join("evidence").join("servers");
    let server_folder = evidence.join(server.id().as_str());
    assert!(server_folder.is_dir());
    for (path, bytes) in rig.files() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_ne!(name, "stderr.log", "{}", path.display());
        assert_ne!(name, "undecoded.bin", "{}", path.display());
        assert!(
            !contains(&bytes, STDERR_SECRET.as_bytes()),
            "{}",
            path.display()
        );
    }
    drop((server, facts));
    drop(pin);
    gone(pid(report)).await;
    rig.finish().await;
}

/// OC02: a second acquisition joins the launching server and drops its
/// own capacity; one server, one capacity held.
#[tokio::test]
async fn oc02_acquisitions_share_one_server_and_one_capacity() {
    let rig = Rig::new(&compatible());
    let (first_token, second_token) = (Arc::new(()), Arc::new(()));
    let first = rig.acquire(Box::new(Arc::clone(&first_token))).unwrap();
    let second = rig.acquire(Box::new(Arc::clone(&second_token))).unwrap();
    assert_eq!(first.server(), second.server());
    assert_eq!(
        Arc::strong_count(&second_token),
        1,
        "the joiner's capacity is dropped"
    );
    first.ready(std::future::pending()).await.unwrap();
    second.ready(std::future::pending()).await.unwrap();
    assert_eq!(
        Arc::strong_count(&first_token),
        2,
        "the launch holds its capacity"
    );
    let third = rig.acquire(Box::new(())).unwrap();
    assert_eq!(third.server(), first.server());
    assert_eq!(rig.reports().len(), 1);
    let lease = third.lease().unwrap();
    assert_eq!(rig.servers.reports()[0].sessions, 1);
    drop((first, second, third));
    assert!(lease.live().is_some(), "a lease holds the server");
    drop(lease);
    gone(pid(&rig.reports()[0])).await;
    // Host dropped the capacity once the group was proved absent.
    let deadline = tokio::time::Instant::now() + GONE;
    while Arc::strong_count(&first_token) > 1 {
        assert!(tokio::time::Instant::now() < deadline, "capacity held");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    rig.finish().await;
}

/// OC02 / §4.3: an existing database makes the integration check run: a
/// known shape with a connection refuses (`check credential state`, not
/// cached: the next launch runs it again), one without proceeds, an
/// unknown shape proceeds with `credential_unchecked`.
/// `/api/credential` is never requested.
#[tokio::test]
async fn oc02_credential_state_is_checked_when_the_namespace_has_data() {
    let stored = json!({"data": [
        {"id": "anthropic", "connections": [
            {"type": "credential", "id": "c1", "label": "key", "method": "api"}]},
        {"id": "openai", "connections": []},
    ]});
    let rig = Rig::new(&with_route(
        compatible(),
        get(
            "/api/integration",
            &json!([{"status": 200, "json": stored}]),
        ),
    ));
    let namespace = rig.namespace();
    namespace.create().unwrap();
    let database = namespace.database();
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    std::fs::write(&database, b"").unwrap();

    let error = rig.refused().await;
    assert_eq!(
        error.failure,
        LaunchFailure::Credential {
            integrations: vec!["anthropic".to_owned()]
        }
    );
    assert_eq!(cause(&error), RouteError::TransportLost { turn: turn() });
    assert_eq!(step(&error), Some("check credential state"));
    assert_eq!(rig.targets(), ["/api/info", "/api/integration"]);

    rig.fixture(&with_route(
        compatible(),
        get(
            "/api/integration",
            &json!([{"status": 200, "json": {"data": [{"id": "openai", "connections": []}]}}]),
        ),
    ));
    let pin = rig.launch().await.expect("clean credentials admit");
    assert!(!pin.live().unwrap().1.credential_unchecked);
    drop(pin);

    rig.fixture(&with_route(
        compatible(),
        get(
            "/api/integration",
            &json!([{"status": 200, "json": {"items": 3}}]),
        ),
    ));
    let pin = rig.launch().await.expect("an unknown shape admits");
    assert!(pin.live().unwrap().1.credential_unchecked);
    drop(pin);

    let targets = rig.targets();
    assert_eq!(
        targets
            .iter()
            .filter(|target| *target == "/api/integration")
            .count(),
        3
    );
    for target in &targets {
        assert!(
            ["/api/info", "/api/integration", "/api/model", "/api/event"]
                .contains(&target.as_str()),
            "{target}"
        );
    }
    rig.finish().await;
}

/// OC01's incompatible fixtures, each with its refusal.
fn incompatible() -> Vec<(&'static str, Value, Refusal)> {
    let long = format!(
        "{{\"url\":\"http://127.0.0.1:1\",\"pad\":\"{}\"}}",
        "x".repeat(5000)
    );
    vec![
        (
            "url not JSON",
            with(
                compatible(),
                "url",
                json!({"raw": "listening on 127.0.0.1"}),
            ),
            Refusal::UrlLine("is not JSON"),
        ),
        (
            "url wrong shape",
            with(compatible(), "url", json!({"raw": "{\"port\":4096}"})),
            Refusal::UrlLine("has no url string"),
        ),
        (
            "url not loopback",
            with(
                compatible(),
                "url",
                json!({"raw": "{\"url\":\"http://10.0.0.1:4096\"}"}),
            ),
            Refusal::UrlLine("is not a loopback http URL"),
        ),
        (
            "url over 4 KiB",
            with(compatible(), "url", json!({"raw": long})),
            Refusal::UrlLine("is longer than 4 KiB"),
        ),
        (
            "info HTML",
            with_route(
                compatible(),
                get(
                    "/api/info",
                    &json!([{"status": 200, "raw": "<html>hi</html>", "content_type": "text/html"}]),
                ),
            ),
            Refusal::Info("is not JSON"),
        ),
        (
            "info missing pid",
            with_route(
                compatible(),
                get(
                    "/api/info",
                    &json!([{"status": 200, "json": {"version": "2.0.22"}}]),
                ),
            ),
            Refusal::Info("lacks a string version or an integer pid"),
        ),
        (
            "info 404",
            with_route(
                compatible(),
                get("/api/info", &json!([{"status": 404, "json": {}}])),
            ),
            Refusal::NotFound {
                endpoint: "/api/info",
            },
        ),
        (
            "model 404",
            with_route(
                compatible(),
                get("/api/model", &json!([{"status": 404, "json": {}}])),
            ),
            Refusal::NotFound {
                endpoint: "/api/model",
            },
        ),
        (
            "first event",
            with_route(
                compatible(),
                events(&json!({"events": [{"type": "session.idle", "properties": {}}]})),
            ),
            Refusal::FirstEvent,
        ),
        (
            "event 404",
            with_route(compatible(), events(&json!({"status": 404}))),
            Refusal::NotFound {
                endpoint: "/api/event",
            },
        ),
    ]
}

/// OC01: each demonstrated incompatibility is `handshake_refused`; the
/// server is retired through Host before publication and nothing but the
/// handshake's reads reached it.
#[tokio::test]
async fn oc01_incompatible_handshakes_are_refused() {
    let cases = incompatible();
    let rig = Rig::new(&compatible());
    for (name, fixture, refusal) in cases {
        rig.fixture(&fixture);
        let error = rig.refused().await;
        assert_eq!(error.failure, LaunchFailure::Refused(refusal), "{name}");
        assert_eq!(
            cause(&error),
            RouteError::HandshakeRefused { turn: turn() },
            "{name}"
        );
    }
    assert!(rig.servers.reports().is_empty());
    rig.finish().await;
}

/// OC01: a fully compatible server reporting 2.0.23 is refused naming the
/// version and the checked set; it was retired before publication and
/// only `/api/info` was read; a 2.0.22 server then admits at once.
#[tokio::test]
async fn oc01_unchecked_version_is_refused_before_publication() {
    let fixture = with_route(
        compatible(),
        get(
            "/api/info",
            &json!([{"status": 200, "json": {"version": "2.0.23", "pid": "$PID"}}]),
        ),
    );
    let rig = Rig::new(&with(fixture, "stderr", json!(STDERR_SECRET)));
    let error = rig.refused().await;
    let refusal = Refusal::Unchecked {
        version: "2.0.23".to_owned(),
        checked: CHECKED,
    };
    assert_eq!(error.failure, LaunchFailure::Refused(refusal.clone()));
    assert_eq!(error.version.as_deref(), Some("2.0.23"));
    let text = refusal.to_string();
    assert!(text.contains("2.0.23") && text.contains("2.0.22"), "{text}");
    assert_eq!(cause(&error), RouteError::HandshakeRefused { turn: turn() });
    assert_eq!(rig.targets(), ["/api/info"]);
    for (path, bytes) in rig.files() {
        assert!(
            !contains(&bytes, STDERR_SECRET.as_bytes()),
            "{}",
            path.display()
        );
    }
    rig.fixture(&compatible());
    let pin = rig.launch().await.expect("2.0.22 admits");
    drop(pin);
    rig.finish().await;
}

/// OC01's transient fixtures, each with the step it names.
fn transient() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        (
            "no url",
            with(compatible(), "url", json!("none")),
            "read the URL line",
        ),
        (
            "pid mismatch",
            with_route(
                compatible(),
                get(
                    "/api/info",
                    &json!([{"status": 200, "json": {"version": "2.0.22", "pid": 1}}]),
                ),
            ),
            "match /api/info.pid",
        ),
        (
            "info 500",
            with_route(
                compatible(),
                get("/api/info", &json!([{"status": 500, "json": {}}])),
            ),
            "read /api/info",
        ),
        (
            "info 401",
            with(compatible(), "auth", json!("reject")),
            "read /api/info",
        ),
        (
            "info redirect",
            with_route(
                compatible(),
                get(
                    "/api/info",
                    &json!([{"status": 302, "json": {}, "headers": ["Location: /api/elsewhere"]}]),
                ),
            ),
            "read /api/info",
        ),
        (
            "model 503",
            with_route(
                compatible(),
                get("/api/model", &json!([{"status": 503, "json": {}}])),
            ),
            "read /api/model",
        ),
        (
            "model undecodable",
            with_route(
                compatible(),
                get(
                    "/api/model",
                    &json!([{"status": 200, "content_type": "application/json",
                             "raw": format!("{{\"data\": [{{\"apiKey\": \"{CATALOG_SECRET}\"")}]),
                ),
            ),
            "decode /api/model",
        ),
        (
            "model over cap",
            with_route(
                compatible(),
                get(
                    "/api/model",
                    &json!([{"status": 200, "json": {"data": [model()]}, "pad_to": 4 * 1024 * 1024 + 1}]),
                ),
            ),
            "read /api/model",
        ),
        (
            "model truncated",
            with_route(
                compatible(),
                get(
                    "/api/model",
                    &json!([{"status": 200, "json": {"data": [model()]}, "declared_length": 4096}]),
                ),
            ),
            "read /api/model",
        ),
        (
            "stream ends",
            with_route(compatible(), events(&json!({"close_after_ms": 0}))),
            "read the first event",
        ),
    ]
}

/// OC01: transient startup failures name their step and are not
/// refusals: exit before the URL line, a `pid` mismatch, a 5xx, a 401, a
/// redirect (never followed), an undecodable, over-cap or truncated
/// catalog, an event stream that ends before its first event.
#[tokio::test]
async fn oc01_transient_handshake_failures_name_their_step() {
    let cases = transient();
    let rig = Rig::new(&compatible());
    for (name, fixture, expected) in cases {
        rig.fixture(&fixture);
        let error = rig.refused().await;
        assert_eq!(
            error.failure,
            LaunchFailure::Transient { step: expected },
            "{name}"
        );
        assert_eq!(
            cause(&error),
            RouteError::TransportLost { turn: turn() },
            "{name}"
        );
        assert_eq!(step(&error), Some(expected), "{name}");
        // `OC12b`: a failed catalog's payload reaches no file VIA writes
        // and no failure value.
        assert!(!format!("{error:?}").contains(CATALOG_SECRET), "{name}");
        for (path, bytes) in rig.files() {
            assert!(
                !contains(&bytes, CATALOG_SECRET.as_bytes()),
                "{name}: {}",
                path.display()
            );
        }
    }
    assert!(
        !rig.targets()
            .iter()
            .any(|target| target == "/api/elsewhere")
    );
    rig.fixture(&compatible());
    drop(
        rig.launch()
            .await
            .expect("a compatible server admits after them"),
    );
    rig.finish().await;
}

/// OC01: an empty catalog until the handshake's bound is the deadline,
/// transient.
#[tokio::test]
async fn oc01_empty_catalog_ends_at_the_deadline() {
    let fixture = with_route(
        compatible(),
        get(
            "/api/model",
            &json!([{"status": 200, "json": {"data": []}}]),
        ),
    );
    let rig = Rig::bounded(&fixture, Duration::from_secs(2));
    let error = rig.refused().await;
    assert_eq!(error.failure, LaunchFailure::Deadline);
    assert_eq!(cause(&error), RouteError::TransportLost { turn: turn() });
    let polls = rig
        .targets()
        .iter()
        .filter(|target| *target == "/api/model")
        .count();
    assert!(polls >= 3, "the catalog is polled 200 ms apart ({polls})");
    rig.finish().await;
}

/// OC01: the best-effort version check: output outside the checked set is
/// `handshake_refused` with nothing launched, no lock and no file in the
/// namespace; a check that fails, hangs or overflows is transient; a
/// well-behaved binary then admits.
#[tokio::test]
async fn oc01_version_check_refuses_before_launch() {
    let rig = Rig::new(&with(
        compatible(),
        "version",
        json!({"output": "opencode v2.0.23"}),
    ));
    let error = rig.refused().await;
    let refusal = Refusal::VersionCheck {
        output: "opencode v2.0.23".to_owned(),
        checked: CHECKED,
    };
    assert_eq!(error.failure, LaunchFailure::Refused(refusal));
    assert_eq!(cause(&error), RouteError::HandshakeRefused { turn: turn() });
    assert!(rig.reports().is_empty(), "nothing launched");
    let namespace = rig.namespace();
    assert!(!namespace.lock().exists(), "the lock was taken");
    let mut files = Vec::new();
    walk(namespace.path(), &mut files);
    assert!(files.is_empty(), "files in the namespace: {files:?}");
    let probe = PrivateRoot::probe(&rig.vendor_state_dir());
    assert!(probe.path().join("home").join("via-fake-version").is_file());

    for (name, version) in [
        ("non-zero", json!({"output": "opencode v2.0.22", "code": 1})),
        (
            "hang",
            json!({"output": "opencode v2.0.22", "sleep_ms": 3000}),
        ),
        ("overflow", json!({"output": "v".repeat(300)})),
    ] {
        rig.fixture(&with(compatible(), "version", version));
        let error = rig.refused().await;
        assert!(
            matches!(
                error.failure,
                LaunchFailure::Acquire {
                    launched: false,
                    ..
                }
            ),
            "{name}: {:?}",
            error.failure
        );
        assert_eq!(
            cause(&error),
            RouteError::TransportLost { turn: turn() },
            "{name}"
        );
        assert!(step(&error).is_some(), "{name}");
        assert!(rig.reports().is_empty(), "{name}: launched");
    }
    rig.fixture(&compatible());
    drop(rig.launch().await.expect("a well-behaved binary admits"));
    rig.finish().await;
}

/// `OC02b`: a held `server.lock` refuses the launch at the lock step,
/// transient; once released the next launch is admitted.
#[tokio::test]
async fn oc02b_held_lock_refuses_until_released() {
    let rig = Rig::new(&compatible());
    let namespace = rig.namespace();
    namespace.create().unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(namespace.lock())
        .unwrap();
    lock.lock().unwrap();
    let error = rig.refused().await;
    assert!(
        matches!(
            error.failure,
            LaunchFailure::Acquire {
                launched: false,
                ..
            }
        ),
        "{:?}",
        error.failure
    );
    assert_eq!(step(&error), Some("take launch lock"));
    assert!(rig.reports().is_empty());
    lock.unlock().unwrap();
    drop(lock);
    drop(rig.launch().await.expect("admitted once released"));
    rig.finish().await;
}

/// OC10: the server's exit is the generation's loss, `ServerLost`; an
/// event stream that ends with the process alive is `TransportLost` and
/// Host stops the process; an over-cap event is `Overflow`, with no
/// payload kept (`OC12b`). Each lost server leaves the registry and its
/// process is gone.
#[tokio::test]
async fn oc10_generation_loss_is_classified_and_stopped() {
    let cases = [
        (
            "exit",
            with(compatible(), "exit_after_ms", json!(1500)),
            LossCause::ServerLost,
        ),
        (
            "stream ends",
            with_route(
                compatible(),
                events(&json!({"events": [{"type": "server.connected"}], "close_after_ms": 1000})),
            ),
            LossCause::TransportLost,
        ),
        (
            "event over cap",
            with_route(
                compatible(),
                events(
                    &json!({"events": [{"type": "server.connected"}], "oversize": 1024 * 1024 + 16}),
                ),
            ),
            LossCause::Overflow,
        ),
    ];
    let rig = Rig::new(&compatible());
    for (name, fixture, expected) in cases {
        rig.fixture(&fixture);
        let pin = rig.launch().await.expect("admitted");
        let (server, _facts) = pin.live().unwrap();
        let end = tokio::time::timeout(WAIT, server.wait_end()).await.unwrap();
        let GenerationEnd::Lost(loss) = end else {
            panic!("{name}: {end:?}");
        };
        assert_eq!(loss.cause, expected, "{name}");
        assert_eq!(server.failure(), Some(expected), "{name}");
        gone(server.vendor_pid()).await;
        let deadline = tokio::time::Instant::now() + GONE;
        while pin.live().is_some() || rig.servers.is_live(server.id()) {
            assert!(tokio::time::Instant::now() < deadline, "{name}: still live");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            rig.servers
                .pin(&rig.adapter.recipe.server_key(super::ADAPTER_VERSION))
                .is_none()
        );
        drop(pin);
    }
    for (path, _) in rig.files() {
        assert_ne!(
            path.file_name().unwrap(),
            "undecoded.bin",
            "{}",
            path.display()
        );
    }
    let ended = rig.servers.ended();
    assert_eq!(ended.len(), 3, "{ended:?}");
    rig.finish().await;
}

/// OC10: a retired generation's successor waits for its retirement (the
/// lock is never found held), gets a fresh password, and the idle
/// retirement closes stdin (the fake exits 0 on it).
#[tokio::test]
async fn oc10_generations_serialize_and_rotate_the_password() {
    let rig = Rig::new(&compatible());
    let first = rig.launch().await.expect("first");
    drop(first);
    // At once, while the first may still retire.
    let second = rig.launch().await.expect("second waits, never LockHeld");
    let reports = rig.reports();
    assert_eq!(reports.len(), 2);
    assert_ne!(reports[0]["pid"], reports[1]["pid"]);
    assert_ne!(
        reports[0]["password_fingerprint"],
        reports[1]["password_fingerprint"]
    );
    gone(pid(&reports[0])).await;
    let ended = rig.servers.ended();
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0].loss, None);
    assert_eq!(
        ended[0].exit.map(|exit| exit.code),
        Some(Some(0)),
        "{ended:?}"
    );
    drop(second);
    rig.finish().await;
}

/// OC12: the password is in the launch environment only: the fake
/// authenticates with it, and it is in no argv, file under the test's
/// roots, or `Debug` output of the registry's values.
#[tokio::test]
async fn oc12_password_only_in_the_launch_environment() {
    let rig = Rig::new(&compatible());
    let pin = rig.launch().await.expect("admitted");
    let (server, facts) = pin.live().unwrap();
    let environ = std::fs::read(format!("/proc/{}/environ", server.vendor_pid())).unwrap();
    let password = environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"OPENCODE_PASSWORD="))
        .expect("the password in the launch environment")
        .to_vec();
    assert_eq!(password.len(), 64);
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", server.vendor_pid())).unwrap();
    assert!(!contains(&cmdline, &password));
    assert!(rig.requests().iter().all(|request| request["auth"] == "ok"));
    for (path, bytes) in rig.files() {
        assert!(!contains(&bytes, &password), "{}", path.display());
    }
    let debug = format!(
        "{pin:?} {server:?} {facts:?} {:?} {:?} {:?}",
        server.http(),
        rig.servers.reports(),
        rig.servers.ended()
    );
    assert!(!contains(debug.as_bytes(), &password));
    drop((server, facts, pin));
    rig.finish().await;
}

/// `OC12b`: the catalog keeps only `providerID`, `id`, `name` and
/// `variants[].id`; the synthetic key reaches neither the facts nor any
/// file.
#[tokio::test]
async fn oc12b_catalog_keeps_only_allow_listed_fields() {
    let rig = Rig::new(&compatible());
    let pin = rig.launch().await.expect("admitted");
    let (_, facts) = pin.live().unwrap();
    assert_eq!(
        facts.models,
        [CatalogModel {
            provider_id: "opencode".to_owned(),
            id: "big-pickle".to_owned(),
            name: "Big Pickle".to_owned(),
            variants: vec!["high".to_owned()],
        }]
    );
    assert!(!format!("{facts:?}").contains(CATALOG_SECRET));
    for (path, bytes) in rig.files() {
        assert!(
            !contains(&bytes, CATALOG_SECRET.as_bytes()),
            "{}",
            path.display()
        );
    }
    drop((facts, pin));
    rig.finish().await;
}

/// §9 and §10's bounds as the route states them.
#[test]
fn bounds_are_the_packets() {
    assert_eq!(SILENCE, Duration::from_secs(45));
    assert_eq!(HANDSHAKE, Duration::from_secs(30));
    assert_eq!(via_routes::opencode::LOSS_EXIT, Duration::from_secs(3));
}

/// Review ocrouteA minor: the URL line's 4 KiB excludes its LF, in Wire's
/// cap and the decoder's alike: 4096 bytes and an LF are admitted, 4097
/// refused.
#[tokio::test]
async fn oc01_url_line_cap_is_4_kib_before_its_lf() {
    let rig = Rig::new(&with(compatible(), "url", json!({"padded": 4096})));
    drop(rig.launch().await.expect("a 4096-byte URL line admits"));
    rig.fixture(&with(compatible(), "url", json!({"padded": 4097})));
    let error = rig.refused().await;
    assert_eq!(
        error.failure,
        LaunchFailure::Refused(Refusal::UrlLine("is longer than 4 KiB"))
    );
    rig.finish().await;
}

/// Review ocrouteA #3: §2.2's bound runs from spawn: a version check
/// taking most of it before the spawn leaves the handshake its whole
/// bound (here 1.5 s, the catalog ready at its fourth poll, 600 ms after
/// the first).
#[tokio::test]
async fn oc01_handshake_bound_starts_at_spawn() {
    let empty = json!({"status": 200, "json": {"data": []}});
    let fixture = with(
        with_route(
            compatible(),
            get(
                "/api/model",
                &json!([empty, empty, empty, {"status": 200, "json": {"data": [model()]}}]),
            ),
        ),
        "version",
        json!({"output": "opencode v2.0.22", "sleep_ms": 1200}),
    );
    let rig = Rig::bounded(&fixture, Duration::from_millis(1500));
    drop(
        rig.launch()
            .await
            .expect("the handshake has its bound from spawn"),
    );
    rig.finish().await;
}

/// Review ocrouteA #2 (runtime §6.1): every managed directory from VIA's
/// `vendor/` down to the namespace's subdirectories must be a directory
/// of the daemon's user, mode 0700, never a symlink; one that is not is a
/// named refusal before anything launches, and it is never chmod-ed or
/// followed.
#[tokio::test]
async fn oc02_managed_directories_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new(&compatible());
    let namespace = rig.namespace();
    let mode = |path: &Path| {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    // A symlinked `data/`, pointing outside VIA's state.
    let outside = tempfile::tempdir().unwrap();
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(namespace.path())
        .unwrap();
    std::fs::set_permissions(
        namespace.path().parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(namespace.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(outside.path(), namespace.path().join("data")).unwrap();
    let LaunchFailure::Unsafe { detail } = rig.refused().await.failure else {
        panic!("a symlinked data/ is refused");
    };
    assert!(
        detail.contains("/data") && detail.contains("symlink"),
        "{detail}"
    );
    assert!(rig.reports().is_empty() && rig.lines(".versions").is_empty());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    std::fs::remove_file(namespace.path().join("data")).unwrap();

    // A namespace of mode 0755 is refused and left 0755.
    std::fs::set_permissions(namespace.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let LaunchFailure::Unsafe { detail } = rig.refused().await.failure else {
        panic!("a 0755 namespace is refused");
    };
    assert!(detail.contains("0755"), "{detail}");
    assert_eq!(mode(namespace.path()), 0o755, "never chmod-ed");
    assert!(rig.reports().is_empty() && rig.lines(".versions").is_empty());

    // A symlinked probe root is refused too.
    std::fs::set_permissions(namespace.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let probe = PrivateRoot::probe(&rig.vendor_state_dir());
    std::os::unix::fs::symlink(outside.path(), probe.path()).unwrap();
    let LaunchFailure::Unsafe { detail } = rig.refused().await.failure else {
        panic!("a symlinked probe root is refused");
    };
    assert!(
        detail.contains("probe") && detail.contains("symlink"),
        "{detail}"
    );
    std::fs::remove_file(probe.path()).unwrap();

    // Repaired: admitted, and the missing ones were created 0700.
    drop(rig.launch().await.expect("private directories admit"));
    for part in ["home", "config", "data", "state", "cache", "runtime", "tmp"] {
        assert_eq!(mode(&namespace.path().join(part)), 0o700, "{part}");
        assert_eq!(mode(&probe.path().join(part)), 0o700, "{part}");
    }
    assert_eq!(mode(probe.path()), 0o700);
    rig.finish().await;
}

/// Review ocrouteA2 #1: §2.2's bound runs from Host's `Spawned`, not from
/// the acquisition's return: a vendor-facts commit delayed 750 ms past
/// the spawn leaves a 500 ms bound already spent, so the launch fails at
/// its deadline although the server is compatible.
#[cfg(feature = "test-failpoints")]
#[tokio::test]
async fn oc01_handshake_bound_runs_from_the_spawn_report() {
    use std::os::unix::fs::PermissionsExt;
    let points = tempfile::tempdir().unwrap();
    std::fs::set_permissions(points.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let token = "oc-spawn-bound-from-spawned";
    via_routes::failpoint::activate(points.path(), token).unwrap();
    std::fs::write(
        points.path().join("store.journal.vendor_facts.json"),
        json!({"token": token, "occurrence": 1, "action": "delay", "value": 750}).to_string(),
    )
    .unwrap();
    let rig = Rig::bounded(&compatible(), Duration::from_millis(500));
    let error = rig.refused().await;
    assert_eq!(error.failure, LaunchFailure::Deadline);
    assert!(
        points
            .path()
            .join("store.journal.vendor_facts.1.ack")
            .is_file(),
        "the delay ran"
    );
    rig.finish().await;
}

/// Review ocrouteA3: the managed-directory job has one owner, the
/// registry's launch task. A caller that cancels its acquisition while
/// the job is held only drops its wait: the fenced registry's join waits
/// for the job and collects it, and nothing is created after the join.
#[cfg(feature = "test-failpoints")]
#[tokio::test]
async fn a_cancelled_acquisition_leaves_its_directory_job_owned() {
    use std::os::unix::fs::PermissionsExt;
    let points = tempfile::tempdir().unwrap();
    std::fs::set_permissions(points.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let token = "oc-directory-job-owned";
    via_routes::failpoint::activate(points.path(), token).unwrap();
    std::fs::write(
        points.path().join("adapters.opencode.prepare.json"),
        json!({"token": token, "occurrence": 1, "action": "pause"}).to_string(),
    )
    .unwrap();
    let ack = points.path().join("adapters.opencode.prepare.1.ack");
    let rig = Rig::new(&compatible());
    let namespace = rig.namespace();
    {
        // Polled until its directory job is held, then cancelled.
        let acquisition = rig.launch();
        tokio::pin!(acquisition);
        let deadline = tokio::time::Instant::now() + GONE;
        while !ack.is_file() {
            assert!(tokio::time::Instant::now() < deadline, "the job never ran");
            let polled = tokio::time::timeout(Duration::from_millis(20), &mut acquisition).await;
            assert!(
                polled.is_err(),
                "the acquisition ended while its job was held"
            );
        }
    }
    rig.servers.fence();
    let short = via_routes::Deadline::at(tokio::time::Instant::now() + Duration::from_millis(300));
    let (unjoined, _) = rig.servers.join(short).await;
    assert!(unjoined > 0, "the join waits for the held directory job");
    assert!(!namespace.path().exists(), "the job is still held");
    std::fs::write(
        points.path().join("adapters.opencode.prepare.1.release"),
        b"",
    )
    .unwrap();
    let full = via_routes::Deadline::at(tokio::time::Instant::now() + GONE);
    assert_eq!(rig.servers.join(full).await, (0, 0));
    let tree = || {
        let mut found = Vec::new();
        let mut stack = vec![rig.vendor_state_dir()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                found.push(path);
            }
        }
        found.sort();
        found
    };
    let at_join = tree();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(tree(), at_join, "created after the join");
    assert!(rig.reports().is_empty(), "nothing launched after the fence");
    rig.finish().await;
}
