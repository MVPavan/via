//! S-CORE chunk 4, Core halves (adapter design §7 S-CORE row): each case
//! runs turns end to end through Core's public Engine, as daemon main
//! drives it, over the real Store, Route, Wire and Host with the fake agent
//! on scenario profiles (decisions H1, H2), and asserts the committed
//! envelopes. The adapter halves are `conformance_driver.rs`. Each case
//! re-executes this binary with its fake settings, which Core reads from
//! the environment once at daemon start.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    env, fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use via_core::{
    AdapterConfig, BootstrapEnv, CloseParams, Deadline, Engine, ResumeParams, SessionId,
    SpawnParams, WaitParams,
};

const CHILD: &str = "VIA_CONFORMANCE_CORE_CHILD";
const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
/// Bound on one child case.
const CHILD_LIMIT: Duration = Duration::from_secs(120);
/// A wait for a turn expected to end by itself.
const WAIT_MS: u64 = 60_000;

/// A workspace test build's sibling binary.
fn binary(name: &str) -> PathBuf {
    let deps = env::current_exe().unwrap();
    let path = deps.parent().unwrap().parent().unwrap().join(name);
    assert!(
        path.is_file(),
        "missing {}; build the workspace first",
        path.display()
    );
    path
}

/// In the parent, runs `name` again in a child whose fake deployment is
/// `scenario`, with `env` set, and returns `None`; in that child, returns
/// its root.
fn child(name: &str, scenario: &Value, env: &[(&str, &str)]) -> Option<PathBuf> {
    if let Some(root) = env::var_os(CHILD) {
        return Some(PathBuf::from(root));
    }
    let root = tempfile::tempdir().unwrap();
    for part in ["state", "runtime", "runtime/anchors", "sync", "points"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.path().join(part))
            .unwrap();
    }
    let scenario_path = root.path().join("scenario.json");
    fs::write(&scenario_path, scenario.to_string()).unwrap();
    fs::set_permissions(&scenario_path, fs::Permissions::from_mode(0o600)).unwrap();
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, root.path())
        .env("VIA_FAKE_AGENT_BINARY", binary("via-fake-agent"))
        .env("VIA_FAKE_SCENARIO", &scenario_path)
        .env("VIA_FAKE_SYNC_DIR", root.path().join("sync"));
    for (key, value) in env {
        command.env(key, value);
    }
    let mut running = command.spawn().unwrap();
    // A hung child is a failure, not a stuck suite.
    let limit = Instant::now() + CHILD_LIMIT;
    let status = loop {
        if let Some(status) = running.try_wait().unwrap() {
            break status;
        }
        let expired = Instant::now() >= limit;
        if expired {
            let _ = running.kill();
            let _ = running.wait();
        }
        assert!(
            !expired,
            "{name} child did not finish within {CHILD_LIMIT:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{name} child failed: {status}");
    None
}

/// Runs `body` on a current-thread runtime.
fn run<F: Future<Output = ()>>(body: F) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body);
}

/// One daemon's Engine, whose session dispatchers run as daemon main runs
/// them: started from the Engine's start channel.
struct Daemon {
    engine: Arc<Engine>,
    starter: tokio::task::JoinHandle<()>,
    dispatchers: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    root: PathBuf,
}

impl Daemon {
    fn open(root: &Path) -> Self {
        let engine = Engine::open(
            &root.join("state"),
            &root.join("runtime"),
            AdapterConfig::load(BootstrapEnv::capture(), None).unwrap(),
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
            dispatchers,
            starter,
            root: root.to_path_buf(),
        }
    }

    /// Spawns a session whose first turn's prompt is `prompt`, with the
    /// spawn members `extra`.
    async fn spawn(&self, prompt: &str, extra: &Value) -> SessionId {
        let mut raw = json!({"harness":"fake","model":"fake","prompt":prompt,"handle":HANDLE});
        for (member, value) in extra.as_object().into_iter().flatten() {
            raw[member] = value.clone();
        }
        let params: SpawnParams = serde_json::from_value(raw.clone()).unwrap();
        let receipted = self.engine.spawn(params, &raw.to_string()).await.unwrap();
        receipted.enqueued.unwrap().0
    }

    /// Queues turn `prompt` on `session`.
    async fn resume(&self, session: &SessionId, prompt: &str) {
        let raw = json!({"session":session,"handle":HANDLE,"prompt":prompt});
        let params: ResumeParams = serde_json::from_value(raw.clone()).unwrap();
        self.engine.resume(params, &raw.to_string()).await.unwrap();
    }

    /// The envelope of `session`'s turn `turn` once it is terminal.
    async fn wait(&self, session: &SessionId, turn: u32) -> Value {
        let params = WaitParams {
            address: format!("{session}/{turn}"),
            timeout_ms: Some(WAIT_MS),
        };
        let envelope = self.engine.wait(params).await.unwrap();
        serde_json::from_str(envelope.get()).unwrap()
    }

    /// Closes `session` gracefully.
    async fn close(&self, session: &SessionId) -> Value {
        let raw = json!({"session":session,"handle":HANDLE});
        let params: CloseParams = serde_json::from_value(raw.clone()).unwrap();
        self.engine.close(params, &raw.to_string()).await.unwrap()
    }

    /// Releases the fake agent's gate `name`.
    fn release(&self, name: &str) {
        fs::write(self.root.join("sync").join(format!("{name}.release")), b"").unwrap();
    }

    /// Waits until the fake agent entered gate `name`.
    async fn entered(&self, name: &str) {
        let path = self.root.join("sync").join(format!("{name}.entered"));
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        while !path.exists() {
            assert!(
                tokio::time::Instant::now() < by,
                "gate {name} never entered"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A forced `daemon/stop`, as daemon main runs it: the force, then
    /// every dispatcher joined, then final shutdown (design §6.8). Returns
    /// the shutdown report.
    #[cfg(feature = "test-failpoints")]
    async fn force_stop(self) -> via_core::EngineShutdown {
        let force = serde_json::from_value(json!({"force":true})).unwrap();
        self.engine.request_stop(&force).await.unwrap();
        let joins = std::mem::take(&mut *self.dispatchers.lock().unwrap());
        for join in joins {
            // A dispatcher that never joins fails the case here.
            tokio::time::timeout(Duration::from_secs(20), join)
                .await
                .unwrap()
                .unwrap();
        }
        let report = self
            .engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        self.starter.abort();
        report
    }

    /// Final shutdown: every anchor is reconciled before the root goes.
    async fn shutdown(self) {
        let report = self
            .engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        self.starter.abort();
        assert!(report.is_clean(), "{report:?}");
    }

    /// Shuts down cleanly, then ends every task holding the Engine, so its
    /// Store lock is free for the next daemon on the same root.
    async fn stop(self) {
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

fn vendor_turn(turn: u32) -> String {
    format!("fake-turn-{turn}")
}

fn emit(message: &Value) -> Value {
    json!({"action":"emit","message":message})
}

fn accepted(turn: u32) -> Value {
    emit(&json!({"type":"accepted","id":1,"vendor_turn_id":vendor_turn(turn)}))
}

fn terminal(turn: u32, status: &str, stop_reason: &str) -> Value {
    emit(
        &json!({"type":"terminal","vendor_turn_id":vendor_turn(turn),
                  "status":status,"final_text":"done","stop_reason":stop_reason}),
    )
}

fn text(turn: u32) -> Value {
    emit(&json!({"type":"text","vendor_turn_id":vendor_turn(turn),"text":"t"}))
}

fn tool_started(turn: u32, id: &str) -> Value {
    emit(
        &json!({"type":"tool_started","vendor_turn_id":vendor_turn(turn),
                  "tool_id":id,"name":"bash"}),
    )
}

fn tool_ended(turn: u32, id: &str) -> Value {
    emit(&json!({"type":"tool_ended","vendor_turn_id":vendor_turn(turn),"tool_id":id}))
}

fn identity(id: &str) -> Value {
    emit(&json!({"type":"identity","vendor_session_id":id}))
}

fn hello(version: &str, features: &[&str]) -> Value {
    json!({"action":"hello","message":{"type":"hello","vendor_version":version,"features":features}})
}

fn gate(name: &str) -> Value {
    json!({"action":"gate","name":name})
}

fn hang() -> Value {
    json!({"action":"hang"})
}

fn expect_interrupt(turn: u32) -> Value {
    json!({"action":"expect_request",
           "expected":{"type":"interrupt","id":2,"vendor_turn_id":vendor_turn(turn)}})
}

/// The script run by the start whose prompt is `prompt`.
fn script(prompt: &str, steps: &[Value]) -> Value {
    json!({"expected_request":{"type":"start","prompt":prompt},"steps":steps})
}

/// A `{profile, scripts}` scenario (decision H2).
fn scenario(profile: &Value, scripts: &[Value]) -> Value {
    json!({"profile": profile, "scripts": scripts})
}

/// The persistent-connection profile (decision H1).
fn persistent() -> Value {
    json!({"persistent": true})
}

/// A handshake profile: `1.0` is checked; `turns` is relied on.
fn handshake() -> Value {
    json!({"handshake": {"checked": ["1.0"], "requires": ["turns"]}})
}

/// The failure class of `envelope`.
fn class(envelope: &Value) -> &Value {
    &envelope["failure"]["class"]
}

/// (2) A vendor `class_hint` decides the failure class (C1 §8.2): the
/// fake's `rate_limit` terminal fails `rate_limit`, keeping its detail and
/// vendor code.
#[test]
fn core_rate_limit_hint_fails_rate_limit() {
    let steps = [
        accepted(1),
        emit(
            &json!({"type":"terminal","vendor_turn_id":vendor_turn(1),"status":"failed",
                      "final_text":"","stop_reason":"error","vendor_code":"429",
                      "class_hint":"rate_limit","detail":"slow down"}),
        ),
    ];
    let Some(root) = child(
        "core_rate_limit_hint_fails_rate_limit",
        &scenario(&json!({}), &[script("p", &steps)]),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "rate_limit", "{envelope}");
        assert_eq!(envelope["failure"]["message"], "slow down", "{envelope}");
        assert_eq!(envelope["failure"]["vendor_code"], "429", "{envelope}");
        daemon.shutdown().await;
    });
}

/// One usage message of `turn`.
fn usage(turn: u32, key: Option<&str>, total: u64, cached: Option<u64>) -> Value {
    let mut message = json!({"type":"usage","vendor_turn_id":vendor_turn(turn),
                             "total_tokens":total,"input":total - 1,"output":1,
                             "reasoning_output":0});
    if let Some(key) = key {
        message["key"] = json!(key);
    }
    if let Some(cached) = cached {
        message["cached_input"] = json!(cached);
    }
    message
}

/// A step boundary: a tool round, then model output.
fn tool_round(turn: u32, id: &str) -> Vec<Value> {
    vec![tool_started(turn, id), tool_ended(turn, id), text(turn)]
}

/// `count` usage lines of `turn` with keys `k<from>`.., written at once.
fn keyed_burst(turn: u32, from: usize, count: usize) -> Value {
    let lines: String = (from..from + count)
        .map(|index| usage(turn, Some(&format!("k{index}")), 1, Some(0)).to_string() + "\n")
        .collect();
    json!({"action":"emit_raw","text":lines})
}

/// (3) AD6 turn-wide ledger, end to end: a key repeated after a step
/// boundary counts once and a keyless sample adds; a sample without
/// `cached_input` makes it `null`; the 1,025th key overflows into
/// `vendor_interval` with `usage_interval_unverified`; a turn aggregate
/// supersedes the samples.
#[test]
fn core_usage_ledger_cases() {
    let mut first = vec![accepted(1), text(1)];
    first.push(emit(&usage(1, Some("a"), 10, Some(2))));
    first.extend(tool_round(1, "t1"));
    first.push(emit(&usage(1, Some("a"), 12, Some(3))));
    first.push(emit(&usage(1, None, 5, None)));
    first.push(terminal(1, "completed", "end_turn"));
    // Three bursts of 342 keys, each below Wire's 1,024-message queue; a
    // durable marker after each tells the test Core handled the burst.
    let marker = |name: &str| {
        emit(&json!({"type":"denial","vendor_turn_id":vendor_turn(2),
                     "kind":"command","target":name,"reason":"policy"}))
    };
    let second = vec![
        accepted(2),
        text(2),
        keyed_burst(2, 0, 342),
        marker("burst1"),
        gate("burst1"),
        keyed_burst(2, 342, 342),
        marker("burst2"),
        gate("burst2"),
        keyed_burst(2, 684, 341),
        terminal(2, "completed", "end_turn"),
    ];
    let third = vec![
        accepted(3),
        text(3),
        emit(&usage(3, Some("m1"), 6, Some(0))),
        emit(
            &json!({"type":"terminal","vendor_turn_id":vendor_turn(3),"status":"completed",
                      "final_text":"done","stop_reason":"end_turn",
                      "usage":{"input":156,"output":177,"total":333}}),
        ),
    ];
    let Some(root) = child(
        "core_usage_ledger_cases",
        &scenario(
            &json!({}),
            &[
                script("first", &first),
                script("second", &second),
                script("third", &third),
            ],
        ),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("first", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        let usage = &envelope["usage"];
        assert_eq!(usage["total_tokens"], 12 + 5, "{envelope}");
        assert_eq!(usage["input_tokens"], 11 + 4, "{envelope}");
        assert_eq!(usage["output_tokens"], 2, "{envelope}");
        assert!(usage["cached_input_tokens"].is_null(), "{envelope}");
        assert_eq!(usage["scope"], "turn", "{envelope}");

        daemon.resume(&session, "second").await;
        for gate in ["burst1", "burst2"] {
            daemon.entered(gate).await;
            // Core handled the burst: Wire's queue holds none of it.
            until_denied(&daemon, &session, gate).await;
            daemon.release(gate);
        }
        let envelope = daemon.wait(&session, 2).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["usage"]["total_tokens"], 1025, "{envelope}");
        assert_eq!(envelope["usage"]["scope"], "vendor_interval", "{envelope}");
        let warned = envelope["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning["code"] == "usage_interval_unverified");
        assert!(warned, "{envelope}");

        daemon.resume(&session, "third").await;
        let envelope = daemon.wait(&session, 3).await;
        let usage = &envelope["usage"];
        assert_eq!(usage["input_tokens"], 156, "{envelope}");
        assert_eq!(usage["output_tokens"], 177, "{envelope}");
        assert_eq!(usage["total_tokens"], 333, "{envelope}");
        assert!(usage["cached_input_tokens"].is_null(), "{envelope}");
        assert_eq!(usage["scope"], "turn", "{envelope}");
        daemon.shutdown().await;
    });
}

/// (5) #36: the adapter's `StopReason` is kept. An unknown vendor reason
/// (`tool_use`) is `other`, kept verbatim in `vendor_stop_reason`; a failed
/// `max_steps` terminal keeps `max_steps`, its code and its step count.
#[test]
fn core_stop_reason_other_and_failed_max_steps_kept() {
    let scripts = [
        script(
            "other",
            &[accepted(1), terminal(1, "completed", "tool_use")],
        ),
        script(
            "steps",
            &[
                accepted(2),
                emit(&json!({"type":"terminal","vendor_turn_id":vendor_turn(2),
                              "status":"failed","final_text":"","stop_reason":"max_steps",
                              "vendor_code":"step_limit","steps":7})),
            ],
        ),
    ];
    let Some(root) = child(
        "core_stop_reason_other_and_failed_max_steps_kept",
        &scenario(&json!({}), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("other", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["stop_reason"], "other", "{envelope}");
        assert_eq!(envelope["vendor_stop_reason"], "tool_use", "{envelope}");

        daemon.resume(&session, "steps").await;
        let envelope = daemon.wait(&session, 2).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "vendor_error", "{envelope}");
        assert_eq!(envelope["stop_reason"], "max_steps", "{envelope}");
        assert_eq!(envelope["vendor_stop_reason"], "max_steps", "{envelope}");
        assert_eq!(
            envelope["failure"]["vendor_code"], "step_limit",
            "{envelope}"
        );
        assert_eq!(envelope["steps"], 7, "{envelope}");
        daemon.shutdown().await;
    });
}

/// Arms Core's failpoint `point` in the child's controller.
#[cfg(feature = "test-failpoints")]
fn arm(root: &Path, point: &str, action: &str) {
    arm_at(root, point, 1, action);
}

/// Arms `point`'s hit `occurrence` with `action`.
#[cfg(feature = "test-failpoints")]
fn arm_at(root: &Path, point: &str, occurrence: u64, action: &str) {
    let command = json!({"token":"conformance-core","occurrence":occurrence,"action":action});
    fs::write(
        root.join("points").join(format!("{point}.json")),
        command.to_string(),
    )
    .unwrap();
}

/// Arms `point`'s hit `occurrence` to be acknowledged only.
#[cfg(feature = "test-failpoints")]
fn acknowledge(root: &Path, point: &str, occurrence: u64) {
    let command = json!({"token":"conformance-core","occurrence":occurrence,
                         "action":"delay","value":0});
    fs::write(
        root.join("points").join(format!("{point}.json")),
        command.to_string(),
    )
    .unwrap();
}

/// Waits for `point`'s hit `occurrence` to be acknowledged.
#[cfg(feature = "test-failpoints")]
async fn until_acked(root: &Path, point: &str, occurrence: u64) {
    let ack = root
        .join("points")
        .join(format!("{point}.{occurrence}.ack"));
    let by = tokio::time::Instant::now() + Duration::from_secs(30);
    while !ack.exists() {
        assert!(
            tokio::time::Instant::now() < by,
            "{point} hit {occurrence} was never acknowledged"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// (7) AD4: Core holds its first observation unhandled
/// (`core.observations.pause`), so the session channel fills and the
/// driver's delivery stalls past the lowered stall bound: the turn fails
/// `overflow`, and the vendor terminal the driver retained still reaches
/// the envelope as its `vendor_stop_reason`.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_retained_terminal_under_stalled_observations() {
    let flood_line = json!({"type":"text","vendor_turn_id":vendor_turn(1)}).to_string() + "\n";
    let steps = [
        accepted(1),
        json!({"action":"flood","text":flood_line,"count":512}),
        gate("flood"),
        json!({"action":"flood","text":flood_line,"count":512}),
        terminal(1, "completed", "end_turn"),
    ];
    let Some(root) = child(
        "core_retained_terminal_under_stalled_observations",
        &scenario(&json!({}), &[script("p", &steps)]),
        &[("VIA_TEST_EVENT_STALL_MS", "250")],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, "core.observations.pause", "pause");
    acknowledge(&root, "adapter.observation.admitted", 513);
    acknowledge(&root, "adapter.observation.stalled", 1);
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        let ack = root.join("points").join("core.observations.pause.1.ack");
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        while !ack.exists() {
            assert!(tokio::time::Instant::now() < by, "Core never paused");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // The first half reaches the channel (the acceptance and 512
        // items) before the second is written, so Wire's queue never holds
        // a channel's worth.
        daemon.entered("flood").await;
        until_acked(&root, "adapter.observation.admitted", 513).await;
        daemon.release("flood");
        // The stalled delivery fails the turn; then Core resumes.
        until_acked(&root, "adapter.observation.stalled", 1).await;
        fs::write(
            root.join("points")
                .join("core.observations.pause.1.release"),
            b"",
        )
        .unwrap();
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "overflow", "{envelope}");
        assert_eq!(envelope["vendor_stop_reason"], "end_turn", "{envelope}");
        daemon.shutdown().await;
    });
}

/// Critical r1b #12 (runtime §8: control before data): with a stop order
/// and an observation both ready, the run loop services the order first.
/// Core is held on the turn's second observation while the vendor's
/// denial reaches the channel and a cancel attaches its order; once
/// released, `cancel.requested` commits before `action.denied`.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_a_ready_order_is_serviced_before_a_ready_observation() {
    let steps = [
        accepted(1),
        text(1),
        gate("ready"),
        emit(&json!({"type":"denial","vendor_turn_id":vendor_turn(1),
                     "kind":"command","target":"ready","reason":"policy"})),
        hang(),
    ];
    let Some(root) = child(
        "core_a_ready_order_is_serviced_before_a_ready_observation",
        &scenario(&json!({}), &[script("p", &steps)]),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    // Held on the text, the acceptance handled.
    arm_at(&root, "core.observations.pause", 2, "pause");
    acknowledge(&root, "adapter.observation.admitted", 3);
    acknowledge(&root, "core.cancel.ordered", 1);
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        until_acked(&root, "core.observations.pause", 2).await;
        daemon.entered("ready").await;
        daemon.release("ready");
        // The denial is in the session channel.
        until_acked(&root, "adapter.observation.admitted", 3).await;
        let ((), ()) = tokio::join!(cancel(&daemon, &session, 1, 200), async {
            until_acked(&root, "core.cancel.ordered", 1).await;
            fs::write(
                root.join("points")
                    .join("core.observations.pause.2.release"),
                b"",
            )
            .unwrap();
        });
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "cancelled", "{envelope}");
        let events = events(&daemon, &session).await;
        let seq = |kind: &str| {
            events
                .iter()
                .find(|event| event["type"] == kind)
                .and_then(|event| event["seq"].as_u64())
                .unwrap_or_else(|| panic!("no {kind}: {events:?}"))
        };
        assert!(
            seq("cancel.requested") < seq("action.denied"),
            "the ready order is serviced first: {events:?}"
        );
        daemon.shutdown().await;
    });
}

/// Cancels `session`'s turn `turn` with `force_after_ms`.
async fn cancel(daemon: &Daemon, session: &SessionId, turn: u32, force_after_ms: u64) {
    let params = serde_json::from_value(json!({
        "session":session,"handle":HANDLE,"turn":turn,"force_after_ms":force_after_ms
    }))
    .unwrap();
    daemon.engine.cancel(params).await.unwrap();
}

/// (8) AD9's table on fake profiles, end to end. Per-turn route: a cancel
/// during an open tool, the vendor interrupted and exited with the tool
/// still open, gives `quiescent` from its own group's absence. Server
/// route (persistent profile): a tool still open at P7's bound (here the
/// wall) gives `uncertain`; one that ended gives `quiescent`. No settled
/// envelope carries `pending`.
#[test]
fn core_cleanup_follows_ad9_on_fake_profiles() {
    const NAME: &str = "core_cleanup_follows_ad9_on_fake_profiles";
    let interrupted = |ends: bool, hangs: bool| {
        let mut steps = vec![accepted(1), tool_started(1, "t1"), expect_interrupt(1)];
        if ends {
            steps.push(tool_ended(1, "t1"));
        }
        steps.push(terminal(1, "interrupted", "interrupted"));
        if hangs {
            steps.push(hang());
        }
        steps
    };
    let per_turn = scenario(&json!({}), &[script("p", &interrupted(false, false))]);
    let server = scenario(
        &persistent(),
        &[
            script("open", &interrupted(false, true)),
            script("ended", &interrupted(true, false)),
        ],
    );
    let Some(root) = child(NAME, &per_turn, &[]) else {
        child_case(NAME, &server, "server");
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let cases: &[(&str, &str)] = if case().as_deref() == Some("server") {
            &[("open", "uncertain"), ("ended", "quiescent")]
        } else {
            &[("p", "quiescent")]
        };
        for &(prompt, cleanup) in cases {
            let session = daemon
                .spawn(prompt, &json!({"deadlines":{"wall_ms":4000}}))
                .await;
            wait_for_tool(&daemon, &session).await;
            cancel(&daemon, &session, 1, 30_000).await;
            let envelope = daemon.wait(&session, 1).await;
            assert_eq!(envelope["state"], "cancelled", "{prompt}: {envelope}");
            assert_eq!(
                envelope["cancel"]["outcome"], "acknowledged",
                "{prompt}: {envelope}"
            );
            assert_eq!(
                envelope["cancel"]["cleanup"], cleanup,
                "{prompt}: {envelope}"
            );
        }
        daemon.shutdown().await;
    });
}

/// Sol r1 F13 (AD4 two deadlines, AD9 server route), end to end: after a
/// cancel's acknowledgement the stop order's `close_by` (here 13 s after
/// the request) no longer bounds the wait; P7's window does. A reported
/// tool that ends 20 s after acknowledgement, past `close_by` and before
/// the 60 s grace, settles the cancel `quiescent`.
#[test]
fn core_tool_ending_20_s_after_acknowledgement_settles_quiescent() {
    let steps = [
        accepted(1),
        tool_started(1, "t1"),
        expect_interrupt(1),
        terminal(1, "interrupted", "interrupted"),
        gate("tool_ends"),
        tool_ended(1, "t1"),
    ];
    let Some(root) = child(
        "core_tool_ending_20_s_after_acknowledgement_settles_quiescent",
        &scenario(&persistent(), &[script("p", &steps)]),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon
            .spawn("p", &json!({"deadlines":{"wall_ms":120_000}}))
            .await;
        wait_for_tool(&daemon, &session).await;
        let requested = tokio::time::Instant::now();
        cancel(&daemon, &session, 1, 10_000).await;
        daemon.entered("tool_ends").await;
        let acknowledged = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert!(
            requested.elapsed() > Duration::from_secs(13),
            "past close_by"
        );
        daemon.release("tool_ends");
        let envelope = daemon.wait(&session, 1).await;
        assert!(acknowledged.elapsed() >= Duration::from_secs(20));
        assert_eq!(envelope["state"], "cancelled", "{envelope}");
        assert_eq!(envelope["cancel"]["outcome"], "acknowledged", "{envelope}");
        assert_eq!(envelope["cancel"]["cleanup"], "quiescent", "{envelope}");
        daemon.shutdown().await;
    });
}

/// Selects a test's second child case.
const CASE: &str = "VIA_CONFORMANCE_CORE_CASE";

/// Runs test `name` again as its child case `case` with `scenario`.
fn child_case(name: &str, scenario: &Value, case: &str) {
    let _ = child(name, scenario, &[(CASE, case)]);
}

/// The child case this process runs, if any.
fn case() -> Option<String> {
    env::var(CASE).ok()
}

/// Waits until `session`'s running turn reports a running tool.
async fn wait_for_tool(daemon: &Daemon, session: &SessionId) {
    let by = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let params = serde_json::from_value(json!({"session":session})).unwrap();
        let status = daemon.engine.status(params).await.unwrap();
        if status["progress"]["running_tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
        {
            return;
        }
        assert!(tokio::time::Instant::now() < by, "no tool ran: {status}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn completed(turn: u32) -> Vec<Value> {
    vec![accepted(turn), terminal(turn, "completed", "end_turn")]
}

/// (9) AD16 (decision H1): four persistent sessions each keep their
/// connection's slot between turns, so a fifth session waits for one;
/// their next turns run on the pinned connections; closing one releases
/// its slot and the fifth runs.
#[test]
fn core_persistent_sessions_hold_slots_until_close() {
    let scripts = [
        script("first", &completed(1)),
        script("again", &completed(2)),
    ];
    let Some(root) = child(
        "core_persistent_sessions_hold_slots_until_close",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let mut held = Vec::new();
        for _ in 0..4 {
            let session = daemon.spawn("first", &json!({})).await;
            let envelope = daemon.wait(&session, 1).await;
            assert_eq!(envelope["state"], "completed", "{envelope}");
            held.push(session);
        }
        assert_eq!(daemon.engine.connections().in_use, 4);
        let fifth = daemon.spawn("first", &json!({})).await;
        let params = WaitParams {
            address: format!("{fifth}/1"),
            timeout_ms: Some(1500),
        };
        assert!(
            daemon.engine.wait(params).await.is_err(),
            "the fifth session waits for a slot"
        );
        for session in &held {
            daemon.resume(session, "again").await;
            let envelope = daemon.wait(session, 2).await;
            assert_eq!(envelope["state"], "completed", "{envelope}");
        }
        assert_eq!(daemon.engine.connections().in_use, 4);
        daemon.close(&held[0]).await;
        let envelope = daemon.wait(&fifth, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        daemon.shutdown().await;
    });
}

/// The AD7 members of `envelope`.
fn version(envelope: &Value) -> (&Value, &Value) {
    (&envelope["vendor_version"], &envelope["version_status"])
}

/// (16) AD7, end to end: once the instance's handshake was read, the
/// envelope carries its version on a protocol failure, an overflow and a
/// forced stop; before any handshake it is `null`.
#[test]
fn core_envelope_version_after_handshake_on_failures() {
    const NAME: &str = "core_envelope_version_after_handshake_on_failures";
    let big = "x".repeat(1024);
    let scripts = [
        script(
            "protocol",
            &[
                hello("1.0", &["turns"]),
                accepted(1),
                json!({"action":"emit_raw","text":"not json\n"}),
                hang(),
            ],
        ),
        script(
            "overflow",
            &[
                accepted(1),
                json!({"action":"flood","text":big,"count":1100}),
                hang(),
            ],
        ),
        script("forced", &[accepted(1), gate("forced")]),
    ];
    let silent = scenario(&handshake(), &[script("silent", &[hang()])]);
    let Some(root) = child(NAME, &scenario(&handshake(), &scripts), &[]) else {
        child_case(NAME, &silent, "silent");
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        if case().is_some() {
            let session = daemon
                .spawn("silent", &json!({"deadlines":{"wall_ms":1500}}))
                .await;
            let envelope = daemon.wait(&session, 1).await;
            assert_eq!(class(&envelope), "deadline_wall", "{envelope}");
            assert_eq!(
                version(&envelope),
                (&Value::Null, &json!("untested")),
                "{envelope}"
            );
            daemon.shutdown().await;
            return;
        }
        for (prompt, state, failure) in [
            ("protocol", "failed", json!("protocol")),
            ("overflow", "failed", json!("overflow")),
            ("forced", "cancelled", Value::Null),
        ] {
            let session = daemon.spawn(prompt, &json!({})).await;
            if prompt == "forced" {
                daemon.entered("forced").await;
                cancel(&daemon, &session, 1, 200).await;
            }
            let envelope = daemon.wait(&session, 1).await;
            assert_eq!(envelope["state"], state, "{prompt}: {envelope}");
            assert_eq!(class(&envelope), &failure, "{prompt}: {envelope}");
            if prompt == "forced" {
                assert_eq!(envelope["cancel"]["outcome"], "forced", "{envelope}");
            }
            assert_eq!(
                version(&envelope),
                (&json!("1.0"), &json!("tested")),
                "{prompt}: {envelope}"
            );
        }
        daemon.shutdown().await;
    });
}

/// (16), (17) Persistent profile: the server's exit before a terminal
/// fails the turn `server_lost`; on the session's replaced connection, a
/// live server whose stdout closed is transport loss, `unknown`. Each
/// envelope keeps the instance's handshake version.
#[test]
fn core_server_lost_versus_transport_lost() {
    let mut profile = persistent();
    profile["handshake"] = handshake()["handshake"].clone();
    let scripts = [
        script(
            "exit",
            &[
                hello("1.1", &["turns"]),
                accepted(1),
                json!({"action":"exit","code":3}),
            ],
        ),
        script(
            "lost",
            &[accepted(2), json!({"action":"close_stdout","name":"lost"})],
        ),
    ];
    let Some(root) = child(
        "core_server_lost_versus_transport_lost",
        &scenario(&profile, &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("exit", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "server_lost", "{envelope}");
        assert_eq!(
            version(&envelope),
            (&json!("1.1"), &json!("untested")),
            "{envelope}"
        );
        daemon.resume(&session, "lost").await;
        let envelope = daemon.wait(&session, 2).await;
        assert_eq!(envelope["state"], "unknown", "{envelope}");
        assert!(envelope["failure"].is_null(), "{envelope}");
        assert_eq!(envelope["vendor_version"], "1.1", "{envelope}");
        daemon.shutdown().await;
    });
}

/// (20) AD4, end to end (persistent profile): at the wall the turn fails
/// `deadline_wall`; an interrupted terminal after the interrupt gives
/// `{acknowledged, quiescent}`, no answer `{requested, uncertain}`. A
/// cancel whose force is capped at the wall, unanswered, on the shared
/// server that is never killed, ends `unknown` with outcome `unknown`; so
/// does one whose `force_at` passes long before the wall, at its force.
#[test]
fn core_wall_soft_stop_on_the_persistent_profile() {
    let scripts = [
        script(
            "answers",
            &[
                accepted(1),
                expect_interrupt(1),
                terminal(1, "interrupted", "interrupted"),
            ],
        ),
        script("silent", &[accepted(1), hang()]),
        script("capped", &[accepted(1), gate("capped")]),
        script("early", &[accepted(1), gate("early")]),
    ];
    let Some(root) = child(
        "core_wall_soft_stop_on_the_persistent_profile",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let wall = json!({"deadlines":{"wall_ms":1500}});
        for (prompt, outcome, cleanup) in [
            ("answers", "acknowledged", "quiescent"),
            ("silent", "requested", "uncertain"),
        ] {
            let session = daemon.spawn(prompt, &wall).await;
            let envelope = daemon.wait(&session, 1).await;
            assert_eq!(envelope["state"], "failed", "{prompt}: {envelope}");
            assert_eq!(class(&envelope), "deadline_wall", "{prompt}: {envelope}");
            assert_eq!(
                (
                    &envelope["cancel"]["outcome"],
                    &envelope["cancel"]["cleanup"]
                ),
                (&json!(outcome), &json!(cleanup)),
                "{prompt}: {envelope}"
            );
        }
        let session = daemon.spawn("capped", &wall).await;
        daemon.entered("capped").await;
        cancel(&daemon, &session, 1, 60_000).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "unknown", "{envelope}");
        assert_eq!(envelope["cancel"]["outcome"], "unknown", "{envelope}");
        let session = daemon
            .spawn("early", &json!({"deadlines":{"wall_ms":30_000}}))
            .await;
        daemon.entered("early").await;
        cancel(&daemon, &session, 1, 500).await;
        let envelope = daemon.wait(&session, 1).await;
        // Sol r1 F13: the order's `force_at` passed long before the wall,
        // unanswered on the shared server: the order's row, `unknown`
        // with outcome `unknown`, settled at the force, never the wall's
        // `deadline_wall`.
        assert_eq!(envelope["state"], "unknown", "{envelope}");
        assert!(envelope["failure"].is_null(), "{envelope}");
        assert_eq!(envelope["cancel"]["outcome"], "unknown", "{envelope}");
        assert!(
            envelope["duration_ms"].as_u64().unwrap() < 10_000,
            "settled at the force, not the wall: {envelope}"
        );
        daemon.shutdown().await;
    });
}

/// AD7, chunk 3 carry-over: a handshake missing a relied-on feature is
/// `submit_failed` with `failure.data.reason: "handshake_refused"`, the
/// instance's version kept; no start reached the vendor.
#[test]
fn core_handshake_refused_is_submit_failed() {
    let scripts = [script(
        "p",
        &[hello("2.0", &["turns"]), json!({"action":"report_pids"})],
    )];
    let Some(root) = child(
        "core_handshake_refused_is_submit_failed",
        &scenario(
            &json!({"handshake": {"requires": ["turns", "steer"]}}),
            &scripts,
        ),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "submit_failed", "{envelope}");
        assert_eq!(
            envelope["failure"]["data"],
            json!({"reason":"handshake_refused"}),
            "{envelope}"
        );
        assert_eq!(envelope["vendor_version"], "2.0", "{envelope}");
        assert!(
            !root.join("sync").join("agent.pid").exists(),
            "no start reached the vendor"
        );
        daemon.shutdown().await;
    });
}

/// C2 §2 Reopen (spec amendment r3), end to end. Turn 1 confirmed `v1`.
/// Before acceptance, a turn whose connection returns `v2` fails
/// `resume_mismatch`. After a retained terminal, a mismatch keeps the
/// turn's result and fails only the driver: the session's next turn runs
/// on a replaced one, which resumes `v1`. After acceptance and before a
/// terminal, it fails `resume_mismatch` with the turn's evidence.
#[test]
fn core_resume_mismatch_before_acceptance_and_after_the_terminal() {
    let scripts = [
        script(
            "first",
            &[
                identity("v1"),
                accepted(1),
                terminal(1, "completed", "end_turn"),
            ],
        ),
        script(
            "other",
            &[
                identity("v2"),
                accepted(2),
                terminal(2, "completed", "end_turn"),
            ],
        ),
        script(
            "after",
            &[
                identity("v1"),
                accepted(3),
                terminal(3, "completed", "end_turn"),
                identity("v2"),
            ],
        ),
        script(
            "next",
            &[
                identity("v1"),
                accepted(4),
                terminal(4, "completed", "end_turn"),
            ],
        ),
        script(
            "during",
            &[
                identity("v1"),
                accepted(5),
                identity("v2"),
                terminal(5, "completed", "end_turn"),
            ],
        ),
    ];
    let Some(root) = child(
        "core_resume_mismatch_before_acceptance_and_after_the_terminal",
        &scenario(&json!({}), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("first", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["vendor_session_id"], "v1", "{envelope}");

        daemon.resume(&session, "other").await;
        let envelope = daemon.wait(&session, 2).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "resume_mismatch", "{envelope}");

        daemon.resume(&session, "after").await;
        let envelope = daemon.wait(&session, 3).await;
        assert_eq!(
            envelope["state"], "completed",
            "the turn keeps its result: {envelope}"
        );
        assert_eq!(envelope["vendor_session_id"], "v1", "{envelope}");

        daemon.resume(&session, "next").await;
        let envelope = daemon.wait(&session, 4).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["vendor_session_id"], "v1", "{envelope}");

        daemon.resume(&session, "during").await;
        let envelope = daemon.wait(&session, 5).await;
        // Sol r1 F13: after acceptance, before any terminal is retained,
        // the turn fails `resume_mismatch` (`Err(ResumeMismatch {
        // evidence })`): the later terminal is not retained, the confirmed
        // ID is not replaced, and the per-turn process's exit is the
        // turn's evidence.
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "resume_mismatch", "{envelope}");
        assert!(
            envelope["timestamps"]["accepted_at"].is_string(),
            "{envelope}"
        );
        assert!(envelope["vendor_stop_reason"].is_null(), "{envelope}");
        assert_eq!(envelope["final_text"], "", "{envelope}");
        assert_eq!(envelope["vendor_session_id"], "v1", "{envelope}");
        assert!(envelope["exit"].is_object(), "{envelope}");
        assert!(envelope["evidence"]["folder"].is_string(), "{envelope}");
        daemon.shutdown().await;
    });
}

/// Sol r1 F7: a daemon force that arrives after the vendor terminal was
/// decoded, while the per-turn process still runs, keeps that terminal's
/// vendor stop reason in the forced envelope (AD4).
#[cfg(feature = "test-failpoints")]
#[test]
fn core_terminal_then_daemon_force_keeps_the_vendor_stop_reason() {
    // The terminal's structured output spills on the forced path too.
    let (terminal, spilled) = structured(1, 32 * 1024 + 1);
    let steps = [accepted(1), terminal, gate("after_terminal")];
    let Some(root) = child(
        "core_terminal_then_daemon_force_keeps_the_vendor_stop_reason",
        &scenario(&json!({}), &[script("p", &steps)]),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    acknowledge(&root, "routes.finalize.entered", 1);
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        daemon.entered("after_terminal").await;
        // Route has decoded the terminal and waits for the process's exit.
        until_acked(&root, "routes.finalize.entered", 1).await;
        let engine = Arc::clone(&daemon.engine);
        let report = daemon.force_stop().await;
        assert_eq!(report.unresolved_turns, 0, "{report:?}");
        let envelope = engine.result(&format!("{session}/1")).await.unwrap();
        let envelope: Value = serde_json::from_str(envelope.get()).unwrap();
        assert_eq!(envelope["vendor_stop_reason"], "end_turn", "{envelope}");
        assert!(envelope["structured_output"].is_null(), "{envelope}");
        let path = envelope["structured_output_file"]["path"].as_str().unwrap();
        let written = fs::read(path).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&written).unwrap(), spilled);
    });
}

/// A completed terminal of `turn` whose structured output is an object
/// encoding to exactly `bytes`.
fn structured(turn: u32, bytes: usize) -> (Value, Value) {
    let output = json!({"pad":"p".repeat(bytes - r#"{"pad":""}"#.len())});
    let step = emit(
        &json!({"type":"terminal","vendor_turn_id":vendor_turn(turn),
                            "status":"completed","final_text":"done",
                            "stop_reason":"end_turn","structured_output":output}),
    );
    (step, output)
}

/// C1 §5 (spill amendment): a structured output is inline up to 32 KiB
/// encoded; one byte more goes to `structured_output.json` in the turn's
/// evidence folder, the envelope naming it with `{path, bytes}` and
/// carrying `structured_output: null`.
#[test]
fn core_structured_output_spills_past_32_kib() {
    let (inline_step, inline) = structured(1, 32 * 1024);
    let (spilled_step, spilled) = structured(2, 32 * 1024 + 1);
    let Some(root) = child(
        "core_structured_output_spills_past_32_kib",
        &scenario(
            &json!({}),
            &[
                script("a", &[accepted(1), inline_step]),
                script("b", &[accepted(2), spilled_step]),
            ],
        ),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("a", &json!({})).await;
        let first = daemon.wait(&session, 1).await;
        assert_eq!(first["state"], "completed", "{first}");
        assert_eq!(first["structured_output"], inline);
        assert!(first["structured_output_file"].is_null(), "{first}");
        daemon.resume(&session, "b").await;
        let second = daemon.wait(&session, 2).await;
        assert_eq!(second["state"], "completed", "{second}");
        assert!(second["structured_output"].is_null(), "{second}");
        let file = &second["structured_output_file"];
        assert_eq!(file["bytes"], 32 * 1024 + 1, "{file}");
        let path = PathBuf::from(file["path"].as_str().unwrap());
        assert_eq!(
            path.parent().unwrap(),
            Path::new(second["evidence"]["folder"].as_str().unwrap())
        );
        assert_eq!(path.file_name().unwrap(), "structured_output.json");
        let written = fs::read(&path).unwrap();
        assert_eq!(written.len(), 32 * 1024 + 1);
        assert_eq!(serde_json::from_slice::<Value>(&written).unwrap(), spilled);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        daemon.shutdown().await;
    });
}

/// Arms `point` to fail every hit from its first (`persist`) or only the
/// first.
#[cfg(feature = "test-failpoints")]
fn arm_failing(root: &Path, point: &str, persist: bool) {
    let command = json!({"token":"conformance-core","occurrence":1,"action":"fail_io",
                         "persist":persist});
    fs::write(
        root.join("points").join(format!("{point}.json")),
        command.to_string(),
    )
    .unwrap();
}

/// C1 §5, §7.6 (spill amendment): the spill write is part of the commit
/// that names it, retried with it. A first failed write is retried and the
/// turn keeps its result and file; when the retry fails too, the commit
/// failed: reads report `store_error`, and final shutdown's resolution
/// batch ends the turn `failed(store)` with both fields `null`, no partial
/// file left in its folder.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_structured_output_write_failure_fails_the_commit() {
    const NAME: &str = "core_structured_output_write_failure_fails_the_commit";
    let (step, spilled) = structured(1, 32 * 1024 + 1);
    let persist = case().as_deref() == Some("persist");
    let scenario = scenario(&json!({}), &[script("p", &[accepted(1), step])]);
    let Some(root) = child(NAME, &scenario, &[]) else {
        child_case(NAME, &scenario, "persist");
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm_failing(&root, "structured_output.write.fail", persist);
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        if !persist {
            let envelope = daemon.wait(&session, 1).await;
            assert_eq!(envelope["state"], "completed", "{envelope}");
            let path = envelope["structured_output_file"]["path"].as_str().unwrap();
            let written = fs::read(path).unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&written).unwrap(), spilled);
            // Sol r2 #8: the retried write's first failure reached the
            // Store failure hook, as a retried commit's does.
            let status = daemon.engine.store_failure_status().expect("reported");
            assert_eq!(status["count"], 1, "{status}");
            daemon.shutdown().await;
            return;
        }
        let params = WaitParams {
            address: format!("{session}/1"),
            timeout_ms: Some(WAIT_MS),
        };
        let error = daemon.engine.wait(params).await.unwrap_err();
        assert_eq!(error.kind, "store_error");
        let engine = Arc::clone(&daemon.engine);
        let report = engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        daemon.starter.abort();
        assert_eq!(report.failure_batches.committed, 1, "{report:?}");
        let envelope = engine.result(&format!("{session}/1")).await.unwrap();
        let envelope: Value = serde_json::from_str(envelope.get()).unwrap();
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "store", "{envelope}");
        assert!(envelope["structured_output"].is_null(), "{envelope}");
        assert!(envelope["structured_output_file"].is_null(), "{envelope}");
        let folder = PathBuf::from(envelope["evidence"]["folder"].as_str().unwrap());
        assert!(!folder.join("structured_output.json").exists());
    });
}

/// Sol r2 #8 (C1 §5, §7.6 spill amendment; design §7.2 row 7): the spill
/// write and the terminal commit are one logical commit with one retry. A
/// first spill write that fails takes the retry, so the terminal's first
/// failed attempt is its last: no third attempt. Reads report
/// `store_error`, and final shutdown's resolution batch ends the turn
/// `failed(store)`.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_spill_and_terminal_share_one_retry() {
    let (step, _spilled) = structured(1, 32 * 1024 + 1);
    let scenario = scenario(&json!({}), &[script("p", &[accepted(1), step])]);
    let Some(root) = child("core_spill_and_terminal_share_one_retry", &scenario, &[]) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm_failing(&root, "structured_output.write.fail", false);
    arm_failing(&root, "store.commit.terminal", false);
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        let params = WaitParams {
            address: format!("{session}/1"),
            timeout_ms: Some(WAIT_MS),
        };
        let error = daemon.engine.wait(params).await.unwrap_err();
        assert_eq!(error.kind, "store_error");
        assert!(
            root.join("points")
                .join("store.commit.terminal.1.ack")
                .exists()
        );
        let engine = Arc::clone(&daemon.engine);
        let report = engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        daemon.starter.abort();
        assert_eq!(report.failure_batches.committed, 1, "{report:?}");
        let envelope = engine.result(&format!("{session}/1")).await.unwrap();
        let envelope: Value = serde_json::from_str(envelope.get()).unwrap();
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "store", "{envelope}");
    });
}

/// Sol r1 F2, F3, F13 (C2 §2 health, AD16): a persistent session whose
/// driver's health fails between turns (its server's retirement unproven,
/// `RetirementUncertain`, its committed slot still held) is retired by the
/// lane's own health monitor, with no observation and no dispatch: its
/// slot is released at once. Its next turn runs on a replacement driver
/// that keeps the confirmed identity. (The daemon's first launched
/// retirement is made unproven here.)
#[cfg(feature = "test-failpoints")]
#[test]
fn core_failed_lane_is_retired_by_its_health_monitor() {
    let scripts = [
        script(
            "retires",
            &[
                identity("v1"),
                accepted(1),
                terminal(1, "completed", "end_turn"),
                json!({"action":"exit","code":0}),
            ],
        ),
        script(
            "again",
            &[
                identity("v1"),
                accepted(2),
                terminal(2, "completed", "end_turn"),
            ],
        ),
    ];
    let Some(root) = child(
        "core_failed_lane_is_retired_by_its_health_monitor",
        &scenario(&persistent(), &scripts),
        &[("VIA_TEST_FAKE_RETIREMENT_UNCERTAIN", "1")],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let failed = daemon.spawn("retires", &json!({})).await;
        let envelope = daemon.wait(&failed, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["vendor_session_id"], "v1", "{envelope}");
        // The lane's actor closes the failed driver: its slot is released
        // without a dispatch.
        let released = tokio::time::Instant::now() + Duration::from_secs(5);
        while daemon.engine.connections().in_use != 0 {
            assert!(
                tokio::time::Instant::now() < released,
                "the failed driver keeps its slot"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        daemon.resume(&failed, "again").await;
        let params = WaitParams {
            address: format!("{failed}/2"),
            timeout_ms: Some(20_000),
        };
        let envelope = daemon
            .engine
            .wait(params)
            .await
            .map_err(|error| error.kind)
            .expect("the replacement turn ran");
        let envelope: Value = serde_json::from_str(envelope.get()).unwrap();
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["vendor_session_id"], "v1", "{envelope}");
        // The first retirement is unproven, so the report is not clean.
        let _report = daemon
            .engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        daemon.starter.abort();
    });
}

/// Critical r1 #4 (runtime §7: every uncertain write latches Store
/// failure): a persistent connection's retirement whose Host journal
/// write is uncertain (its absence proof, after the logical turn already
/// ended) latches the daemon's Store failure, separately from the
/// retirement's unproven cleanup, which retires the lane.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_uncertain_retirement_journal_latches_store_failure() {
    let scripts = [script(
        "retires",
        &[
            accepted(1),
            terminal(1, "completed", "end_turn"),
            json!({"action":"exit","code":0}),
        ],
    )];
    let Some(root) = child(
        "core_uncertain_retirement_journal_latches_store_failure",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    // The absence proof's write fails and its rollback too: its outcome
    // is uncertain (design §7.1).
    arm(&root, "store.journal.absence", "fail_io");
    arm(&root, "store.rollback.fail", "fail_io");
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("retires", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        // The retirement's health failure retires the lane: its slot goes.
        let released = tokio::time::Instant::now() + Duration::from_secs(5);
        while daemon.engine.connections().in_use != 0 {
            assert!(
                tokio::time::Instant::now() < released,
                "the failed driver keeps its slot"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            root.join("points")
                .join("store.journal.absence.1.ack")
                .exists(),
            "the absence proof write was refused"
        );
        assert!(
            daemon.engine.store_failed(),
            "the uncertain journal write latched Store failure"
        );
        let _report = daemon
            .engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        daemon.starter.abort();
    });
}

/// Sol r1 F3, Sol r2 #1, #2 (C2 §2 health, AD16): four persistent
/// sessions hold every connection slot, and one's driver fails between
/// turns while its lane's actor is held between its health read and the
/// lane's end (`core.lane.retire`). That session's next turn, needing a
/// fifth slot, is refused the failed lane's claim and asks for its
/// retirement at dispatch, which the held actor carries out (releasing
/// the slot, Sol r3 N4); the turn runs on a successor, and the hold's
/// later release finds nothing to retire: the successor is untouched.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_failed_lane_with_every_slot_held_retires_at_dispatch() {
    let scripts = [
        script("first", &completed(1)),
        script(
            "retires",
            &[
                accepted(1),
                terminal(1, "completed", "end_turn"),
                json!({"action":"exit","code":0}),
            ],
        ),
        script("again", &completed(2)),
        script("third", &completed(3)),
    ];
    let Some(root) = child(
        "core_failed_lane_with_every_slot_held_retires_at_dispatch",
        &scenario(&persistent(), &scripts),
        &[("VIA_TEST_FAKE_RETIREMENT_UNCERTAIN", "4")],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, "core.lane.retire", "pause");
    run(async {
        let daemon = Daemon::open(&root);
        for _ in 0..3 {
            let session = daemon.spawn("first", &json!({})).await;
            let envelope = daemon.wait(&session, 1).await;
            assert_eq!(envelope["state"], "completed", "{envelope}");
        }
        let failed = daemon.spawn("retires", &json!({})).await;
        let envelope = daemon.wait(&failed, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        let ack = root.join("points").join("core.lane.retire.1.ack");
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        while !ack.exists() {
            assert!(
                tokio::time::Instant::now() < by,
                "the lane's actor never saw the failure"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(daemon.engine.connections().in_use, 4);
        daemon.resume(&failed, "again").await;
        let params = WaitParams {
            address: format!("{failed}/2"),
            timeout_ms: Some(20_000),
        };
        let envelope = daemon
            .engine
            .wait(params)
            .await
            .map_err(|error| error.kind)
            .expect("the successor turn ran after the held actor retired the lane");
        let envelope: Value = serde_json::from_str(envelope.get()).unwrap();
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(daemon.engine.connections().in_use, 4);
        // The dispatch's ask ended the actor's hold (its pause was given
        // up, so this release has no waiter) and the actor retired the
        // lane before turn 2 ran: the successor keeps its slot and serves
        // the session's next turn.
        fs::write(root.join("points").join("core.lane.retire.1.release"), b"").unwrap();
        assert_eq!(daemon.engine.connections().in_use, 4);
        daemon.resume(&failed, "third").await;
        let envelope = daemon.wait(&failed, 3).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        // The fourth session's retirement is unproven: not clean.
        let _report = daemon
            .engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        daemon.starter.abort();
    });
}

/// (14) AD4, C1 §6.1, end to end (Sol r1 F13): while turn 2 runs, the
/// vendor reports a denial for turn 1's vendor turn. Core commits it
/// `action.denied` with `turn: 1` and `late: true`, and it stays out of
/// turn 2's `denied_actions`; turn 2's own denial is kept there.
#[test]
fn core_late_denial_is_committed_late_end_to_end() {
    let denial = |turn: u32, target: &str| {
        emit(&json!({"type":"denial","vendor_turn_id":vendor_turn(turn),
                     "kind":"command","target":target,"reason":"policy"}))
    };
    let scripts = [
        script("first", &completed(1)),
        script(
            "second",
            &[
                accepted(2),
                denial(1, "late"),
                denial(2, "own"),
                terminal(2, "completed", "end_turn"),
            ],
        ),
    ];
    let Some(root) = child(
        "core_late_denial_is_committed_late_end_to_end",
        &scenario(&json!({}), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("first", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        daemon.resume(&session, "second").await;
        let envelope = daemon.wait(&session, 2).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["denied_actions_total"], 1, "{envelope}");
        assert_eq!(envelope["denied_actions"][0]["target"], "own", "{envelope}");
        let events = events(&daemon, &session).await;
        let denied: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == "action.denied")
            .collect();
        assert_eq!(denied.len(), 2, "{events:?}");
        assert_eq!(
            (&denied[0]["turn"], &denied[0]["late"], &denied[0]["target"]),
            (&json!(1), &json!(true), &json!("late")),
        );
        assert_eq!(
            (&denied[1]["turn"], &denied[1]["late"], &denied[1]["target"]),
            (&json!(2), &json!(false), &json!("own")),
        );
        daemon.shutdown().await;
    });
}

/// Waits until `session` committed an `action.denied` naming `target`.
async fn until_denied(daemon: &Daemon, session: &SessionId, target: &str) {
    let by = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let events = events(daemon, session).await;
        if events
            .iter()
            .any(|event| event["type"] == "action.denied" && event["target"] == target)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < by,
            "{target} was never committed"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The session's committed events, in order.
async fn events(daemon: &Daemon, session: &SessionId) -> Vec<Value> {
    let params = serde_json::from_value(json!({"session":session,"limit":1000})).unwrap();
    let page = daemon.engine.events(params).await.unwrap();
    let page: Value = serde_json::from_str(page.get()).unwrap();
    page["events"].as_array().unwrap().clone()
}

/// Sol r1 F1 (C2 §2 delayed identity, C1 §6.1, decision H3): a confirmed
/// vendor identity is committed through the session journal, as one
/// `session.opened` (`turn: null`, the route, the ID and the handshake's
/// version; the transcript hint into the session's columns) for the
/// session's first connection generation and one
/// `session.reopened` for each later one, before the same message's
/// acceptance. After a daemon restart the lane recovers the committed
/// identity, so a resume whose vendor returns another session is
/// `resume_mismatch`; status and logs keep the ID and its transcript.
#[test]
fn core_confirmed_identity_is_durable_across_a_restart() {
    let with_transcript = emit(&json!({"type":"identity","vendor_session_id":"v1",
                                       "transcript":"/t/v1.jsonl"}));
    let scripts = [
        script(
            "first",
            &[
                with_transcript,
                accepted(1),
                terminal(1, "completed", "end_turn"),
            ],
        ),
        script(
            "second",
            &[
                identity("v1"),
                accepted(2),
                terminal(2, "completed", "end_turn"),
            ],
        ),
        script(
            "other",
            &[
                identity("v2"),
                accepted(3),
                terminal(3, "completed", "end_turn"),
            ],
        ),
    ];
    let Some(root) = child(
        "core_confirmed_identity_is_durable_across_a_restart",
        &scenario(&json!({}), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let session = {
            let daemon = Daemon::open(&root);
            let session = daemon.spawn("first", &json!({})).await;
            let envelope = daemon.wait(&session, 1).await;
            assert_eq!(envelope["state"], "completed", "{envelope}");
            daemon.resume(&session, "second").await;
            let envelope = daemon.wait(&session, 2).await;
            assert_eq!(envelope["state"], "completed", "{envelope}");
            let events = events(&daemon, &session).await;
            let opens: Vec<&Value> = events
                .iter()
                .filter(|event| {
                    event["type"] == "session.opened" || event["type"] == "session.reopened"
                })
                .collect();
            assert_eq!(opens.len(), 2, "one per connection generation: {events:?}");
            assert_eq!(opens[0]["type"], "session.opened", "{events:?}");
            assert_eq!(opens[1]["type"], "session.reopened", "{events:?}");
            for open in &opens {
                assert!(open["turn"].is_null(), "{open}");
                assert_eq!(open["vendor_session_id"], "v1", "{open}");
                assert_eq!(open["route"], "fake", "{open}");
            }
            // C1 §6.1's members only: the fake's handshake carries no
            // version; the transcript hint goes to the session's columns.
            for open in &opens {
                assert_eq!(open.get("vendor_version"), Some(&Value::Null), "{open}");
                assert!(open.get("transcript").is_none(), "{open}");
            }
            assert_eq!(opens[1]["reason"], "resume", "{events:?}");
            // Committed before the same turn's acceptance.
            for (open, turn) in opens.iter().zip([1, 2]) {
                let started = events
                    .iter()
                    .find(|event| event["type"] == "turn.started" && event["turn"] == turn)
                    .unwrap();
                assert!(open["seq"].as_u64() < started["seq"].as_u64(), "{events:?}");
            }
            let params = serde_json::from_value(json!({"session":session})).unwrap();
            let status = daemon.engine.status(params).await.unwrap();
            assert_eq!(status["vendor_identity_verified"], true, "{status}");
            daemon.stop().await;
            session
        };
        let daemon = Daemon::open(&root);
        daemon.engine.recover().await.unwrap();
        daemon.engine.hand_off_queued().await.unwrap();
        let params = serde_json::from_value(json!({"session":session})).unwrap();
        let status = daemon.engine.status(params).await.unwrap();
        assert_eq!(status["vendor_session_id"], "v1", "{status}");
        // C1 §3.7: the historical ID, unverified until a new generation
        // confirms.
        assert_eq!(status["vendor_identity_verified"], false, "{status}");
        let params = serde_json::from_value(json!({"turn":format!("{session}/1")})).unwrap();
        let logs = daemon.engine.logs(params).await.unwrap();
        assert_eq!(logs["vendor_session_id"], "v1", "{logs}");
        assert_eq!(logs["transcript"], "/t/v1.jsonl", "{logs}");
        daemon.resume(&session, "other").await;
        let envelope = daemon.wait(&session, 3).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "resume_mismatch", "{envelope}");
        daemon.shutdown().await;
    });
}

/// The session's `vendor_identity_verified` in `status`.
async fn verified(daemon: &Daemon, session: &SessionId) -> Value {
    let params = serde_json::from_value(json!({"session":session})).unwrap();
    let status = daemon.engine.status(params).await.unwrap();
    status["vendor_identity_verified"].clone()
}

/// Sol r2 #4 (C2 §2 delayed identity, C1 §3.7, decision H3): verification
/// belongs to the driver's current connection generation. A generation
/// confirming again with a new transcript hint writes the session's
/// columns with no second open event. A reopen resets verification until
/// its own generation confirms; a turn refused before its generation
/// confirms leaves it false.
#[test]
fn core_identity_verification_follows_the_connection_generation() {
    let named = |transcript: &str| {
        emit(&json!({"type":"identity","vendor_session_id":"v1","transcript":transcript}))
    };
    let scripts = [
        script(
            "first",
            &[
                named("/t/a.jsonl"),
                accepted(1),
                named("/t/b.jsonl"),
                terminal(1, "completed", "end_turn"),
            ],
        ),
        script(
            "reopen",
            &[
                gate("reopen"),
                identity("v1"),
                accepted(2),
                terminal(2, "completed", "end_turn"),
            ],
        ),
        script("refused", &[json!({"action":"exit","code":1})]),
    ];
    let Some(root) = child(
        "core_identity_verification_follows_the_connection_generation",
        &scenario(&json!({}), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("first", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        let opens = |events: &[Value]| {
            events
                .iter()
                .filter(|event| {
                    event["type"] == "session.opened" || event["type"] == "session.reopened"
                })
                .count()
        };
        assert_eq!(opens(&events(&daemon, &session).await), 1);
        let params = serde_json::from_value(json!({"turn":format!("{session}/1")})).unwrap();
        let logs = daemon.engine.logs(params).await.unwrap();
        assert_eq!(logs["transcript"], "/t/b.jsonl", "{logs}");
        assert_eq!(verified(&daemon, &session).await, true);
        // A reopen: unverified until its generation confirms.
        daemon.resume(&session, "reopen").await;
        daemon.entered("reopen").await;
        assert_eq!(verified(&daemon, &session).await, false);
        daemon.release("reopen");
        let envelope = daemon.wait(&session, 2).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(opens(&events(&daemon, &session).await), 2);
        assert_eq!(verified(&daemon, &session).await, true);
        // Refused before its generation confirms: unverified.
        daemon.resume(&session, "refused").await;
        let envelope = daemon.wait(&session, 3).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(verified(&daemon, &session).await, false);
        daemon.shutdown().await;
    });
}

/// C1 §3.3, decision H3, Sol r2 #6: a started turn advances the session's
/// recorded adapter version to the running adapter's, here the profile's
/// `9.9.9`, not the receipt's. A later turn cancelled before it was ever
/// submitted changes nothing.
#[test]
fn core_started_turns_record_the_running_adapter_version() {
    let scripts = [
        script(
            "first",
            &[
                accepted(1),
                gate("hold"),
                terminal(1, "completed", "end_turn"),
            ],
        ),
        script("never", &completed(2)),
    ];
    let Some(root) = child(
        "core_started_turns_record_the_running_adapter_version",
        &scenario(&json!({"adapter_version":"9.9.9"}), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("first", &json!({})).await;
        daemon.entered("hold").await;
        daemon.resume(&session, "never").await;
        cancel(&daemon, &session, 2, 1000).await;
        let envelope = daemon.wait(&session, 2).await;
        assert_eq!(envelope["state"], "cancelled", "{envelope}");
        daemon.release("hold");
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        daemon.stop().await;
        let store = via_store::Store::open(&root.join("state")).unwrap();
        let snapshot = store
            .client()
            .session_snapshot(&session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.route.adapter_version.as_deref(), Some("9.9.9"));
    });
}

/// Sol r3 N1, N2 (lane actor ruling): the turn's caller, its dispatcher,
/// is aborted while the turn's final drain has a real Store commit in
/// flight, with one more durable item behind it. The turn is the lane
/// actor's, which runs it to completion: every denial is committed and
/// the turn reaches its terminal; nothing is left without a consumer.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_an_aborted_dispatcher_strands_no_turn_work() {
    let denial = |target: &str| {
        emit(&json!({"type":"denial","vendor_turn_id":vendor_turn(1),
                     "kind":"command","target":target,"reason":"policy"}))
    };
    let steps = [
        accepted(1),
        denial("a"),
        denial("b"),
        denial("c"),
        terminal(1, "completed", "end_turn"),
    ];
    let Some(root) = child(
        "core_an_aborted_dispatcher_strands_no_turn_work",
        &scenario(&json!({}), &[script("p", &steps)]),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    // Core holds the acceptance, so the rest queues behind it and the
    // driver's turn ends; then the second denial's commit is held (the
    // acceptance's is the first, the first denial's the second).
    arm(&root, "core.observations.pause", "pause");
    arm_at(&root, "store.commit.event", 3, "pause");
    // Acknowledged only: the driver's turn returned.
    fs::write(
        root.join("points").join("core.run.returned.json"),
        json!({"token":"conformance-core","occurrence":1,"action":"delay","value":0}).to_string(),
    )
    .unwrap();
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        let points = root.join("points");
        let until = async |name: &str| {
            let path = points.join(name);
            let by = tokio::time::Instant::now() + Duration::from_secs(30);
            while !path.exists() {
                assert!(tokio::time::Instant::now() < by, "{name} never reached");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        until("core.observations.pause.1.ack").await;
        // The driver delivered the turn's items and returned.
        until("core.run.returned.1.ack").await;
        fs::write(points.join("core.observations.pause.1.release"), b"").unwrap();
        until("store.commit.event.3.ack").await;
        let dispatchers = std::mem::take(&mut *daemon.dispatchers.lock().unwrap());
        for dispatcher in dispatchers {
            dispatcher.abort();
            let _ = dispatcher.await;
        }
        fs::write(points.join("store.commit.event.3.release"), b"").unwrap();
        let params = WaitParams {
            address: format!("{session}/1"),
            timeout_ms: Some(10_000),
        };
        let envelope = daemon.engine.wait(params).await.map_err(|error| error.kind);
        let envelope: Value = serde_json::from_str(envelope.unwrap().get()).unwrap();
        assert_eq!(envelope["state"], "completed", "{envelope}");
        let denied: Vec<Value> = events(&daemon, &session)
            .await
            .into_iter()
            .filter(|event| event["type"] == "action.denied")
            .map(|event| event["target"].clone())
            .collect();
        assert_eq!(denied, [json!("a"), json!("b"), json!("c")]);
        let _report = daemon
            .engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        daemon.starter.abort();
    });
}
