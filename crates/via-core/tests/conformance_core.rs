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

#[path = "support/core_codex.rs"]
mod core_codex;

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
        // An ignored case run on purpose runs in its child too.
        .args(["--exact", name, "--nocapture", "--include-ignored"])
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
fn run<T, F: Future<Output = T>>(body: F) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body)
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
        Self::open_with(
            root,
            AdapterConfig::load(BootstrapEnv::capture(), None).unwrap(),
        )
    }

    /// Opens with the adapter config `adapters`.
    fn open_with(root: &Path, adapters: AdapterConfig) -> Self {
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

    /// Closes `session` gracefully under a `deadline_ms` deadline.
    #[cfg(feature = "test-failpoints")]
    async fn close_within(&self, session: &SessionId, deadline_ms: u64) -> Value {
        let raw = json!({"session":session,"handle":HANDLE,"deadline_ms":deadline_ms});
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

/// x.3.2 J0: a route failure's envelope message is harness-neutral, since
/// every route shares the failure vocabulary: the fake's process exiting
/// after acceptance with no terminal names no harness.
#[test]
fn core_route_failure_message_names_no_harness() {
    let steps = [accepted(1), json!({"action":"exit","code":3})];
    let Some(root) = child(
        "core_route_failure_message_names_no_harness",
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
        let message = envelope["failure"]["message"].as_str().unwrap();
        assert!(!message.is_empty(), "{envelope}");
        for harness in via_adapters::harness_names() {
            assert!(!message.contains(harness), "{harness}: {message}");
        }
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

/// (9) AD16 (decision H1): persistent sessions filling every connection
/// slot (the default 8) each keep their connection's slot between turns,
/// so one more session waits for one; their next turns run on the pinned
/// connections; closing one releases its slot and the waiting one runs.
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
        let slots = daemon.engine.harness_processes().limit;
        assert_eq!(slots, 8, "the default slot count");
        let mut held = Vec::new();
        for _ in 0..slots {
            let session = daemon.spawn("first", &json!({})).await;
            let envelope = daemon.wait(&session, 1).await;
            assert_eq!(envelope["state"], "completed", "{envelope}");
            held.push(session);
        }
        assert_eq!(daemon.engine.harness_processes().in_use, slots);
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
        assert_eq!(daemon.engine.harness_processes().in_use, slots);
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
        // C1 §3.12: `logs` states the file the envelope names, and none
        // that no envelope names (Sol r5 #1).
        let spill = |logs: &Value| {
            logs["files"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|file| {
                    file["name"]
                        .as_str()
                        .unwrap()
                        .starts_with("structured_output")
                })
                .cloned()
                .collect::<Vec<Value>>()
        };
        let logs = |turn: u32| {
            serde_json::from_value(json!({"turn": format!("{session}/{turn}")})).unwrap()
        };
        let second_logs = daemon.engine.logs(logs(2)).await.unwrap();
        assert_eq!(
            spill(&second_logs),
            [json!({"name":"structured_output.json","bytes":32 * 1024 + 1})],
            "{second_logs}"
        );
        let orphan =
            Path::new(first["evidence"]["folder"].as_str().unwrap()).join("structured_output.json");
        fs::write(&orphan, b"{}").unwrap();
        let first_logs = daemon.engine.logs(logs(1)).await.unwrap();
        assert!(spill(&first_logs).is_empty(), "{first_logs}");
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
        while daemon.engine.harness_processes().in_use != 0 {
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
        while daemon.engine.harness_processes().in_use != 0 {
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
/// sessions hold every harness-process slot (the pool pinned at four, bead
/// via-oq3: with a free slot a successor that reserved its permit before
/// the retirement would pass), and one's driver fails between
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
        &[
            ("VIA_TEST_FAKE_RETIREMENT_UNCERTAIN", "4"),
            ("VIA_TEST_HARNESS_PROCESSES", "4"),
        ],
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
        let processes = daemon.engine.harness_processes();
        assert_eq!(
            (processes.limit, processes.in_use),
            (4, 4),
            "every slot is held before the resume"
        );
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
        assert_eq!(daemon.engine.harness_processes().in_use, 4);
        // The dispatch's ask ended the actor's hold (its pause was given
        // up, so this release has no waiter) and the actor retired the
        // lane before turn 2 ran: the successor keeps its slot and serves
        // the session's next turn.
        fs::write(root.join("points").join("core.lane.retire.1.release"), b"").unwrap();
        assert_eq!(daemon.engine.harness_processes().in_use, 4);
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

/// Sol r2 #7 (C1 §5, §8.2): a structured output is validated after the
/// turn's final text, evidence and Store classification. The vendor
/// completes with an output the schema refuses and a final text past the
/// inline limit, whose file then fails its sync (`final_text.sync.fail`)
/// once the vendor's terminal is disposed: the turn fails `store`, which
/// stands, and its envelope warns `structured_output_invalid` with
/// `reason: "invalid"`.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_structured_output_validated_after_a_store_failure() {
    let profile = json!({"capabilities": {
        "verbs": {"spawn":{"support":"native"},"resume":{"support":"native"},
                  "steer":{"support":"unsupported","reason":"no"},
                  "cancel":{"support":"native"},"close":{"support":"native"}},
        "params": {"instructions":{"support":"unsupported","reason":"no"},
                   "output_schema":{"support":"native"},
                   "effort":{"support":"unsupported","reason":"no"},
                   "max_steps":{"support":"unsupported","reason":"no"}},
        "bounds": [], "network_control": false,
        "recover": {"support":"unsupported","reason":"no"},
        "usage": {"tokens":"turn","cost":"unavailable"}
    }});
    // Past the 256 KiB inline limit, so the text goes to its file.
    let spilled = "a".repeat(256 * 1024);
    let steps = [
        accepted(1),
        emit(&json!({"type":"terminal","vendor_turn_id":vendor_turn(1),
                     "status":"completed","final_text":spilled,"stop_reason":"end_turn",
                     "structured_output":{"b":1}})),
    ];
    let Some(root) = child(
        "core_structured_output_validated_after_a_store_failure",
        &scenario(&profile, &[script("p", &steps)]),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, "final_text.sync.fail", "fail_io");
    run(async {
        let daemon = Daemon::open(&root);
        let schema = json!({"type":"object","required":["a"]});
        let session = daemon.spawn("p", &json!({"output_schema":schema})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(envelope["failure"]["class"], "store", "{envelope}");
        assert!(envelope["final_text_file"].is_null(), "{envelope}");
        let warned: Vec<&Value> = envelope["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|warning| warning["code"] == "structured_output_invalid")
            .collect();
        assert_eq!(warned.len(), 1, "{envelope}");
        assert_eq!(warned[0]["data"], json!({"reason":"invalid"}), "{envelope}");
        daemon.stop().await;
    });
}

/// A fake profile that takes an `output_schema`.
#[cfg(feature = "test-failpoints")]
fn schema_profile() -> Value {
    json!({"capabilities": {
        "verbs": {"spawn":{"support":"native"},"resume":{"support":"native"},
                  "steer":{"support":"unsupported","reason":"no"},
                  "cancel":{"support":"native"},"close":{"support":"native"}},
        "params": {"instructions":{"support":"unsupported","reason":"no"},
                   "output_schema":{"support":"native"},
                   "effort":{"support":"unsupported","reason":"no"},
                   "max_steps":{"support":"unsupported","reason":"no"}},
        "bounds": [], "network_control": false,
        "recover": {"support":"unsupported","reason":"no"},
        "usage": {"tokens":"turn","cost":"unavailable"}
    }})
}

/// The envelope's `structured_output_invalid` warnings.
#[cfg(feature = "test-failpoints")]
fn invalid_warnings(envelope: &Value) -> Vec<&Value> {
    envelope["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|warning| warning["code"] == "structured_output_invalid")
        .collect()
}

/// Critical r1 #3 (C1 §5, Q2): the forced path validates the structured
/// output before it spills, as the natural path does. A vendor terminal
/// decoded before a daemon force, whose output the schema refuses and
/// which is past 32 KiB, is spilled and the forced envelope warns
/// `structured_output_invalid` with `reason: "invalid"`.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_forced_spilled_output_is_validated() {
    let (terminal, spilled) = structured(1, 32 * 1024 + 1);
    let steps = [accepted(1), terminal, gate("after_terminal")];
    let Some(root) = child(
        "core_forced_spilled_output_is_validated",
        &scenario(&schema_profile(), &[script("p", &steps)]),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    acknowledge(&root, "routes.finalize.entered", 1);
    run(async {
        let daemon = Daemon::open(&root);
        let schema = json!({"type":"object","required":["a"]});
        let session = daemon.spawn("p", &json!({"output_schema":schema})).await;
        daemon.entered("after_terminal").await;
        until_acked(&root, "routes.finalize.entered", 1).await;
        let engine = Arc::clone(&daemon.engine);
        let report = daemon.force_stop().await;
        assert_eq!(report.unresolved_turns, 0, "{report:?}");
        let envelope = engine.result(&format!("{session}/1")).await.unwrap();
        let envelope: Value = serde_json::from_str(envelope.get()).unwrap();
        assert!(envelope["structured_output"].is_null(), "{envelope}");
        let path = envelope["structured_output_file"]["path"].as_str().unwrap();
        let written = fs::read(path).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&written).unwrap(), spilled);
        let warned = invalid_warnings(&envelope);
        assert_eq!(warned.len(), 1, "{envelope}");
        assert_eq!(warned[0]["data"], json!({"reason":"invalid"}), "{envelope}");
    });
}

/// Critical r1 #4 (C1 §5, §8.2; design §7.4): the validation outcome is
/// kept apart from the failure class and projected after the final Store
/// classification. A completed turn whose output the schema refuses
/// fails `structured_output_invalid`; its terminal write fails both
/// attempts, so final shutdown's resolution batch ends it
/// `failed(store)`, which still warns `structured_output_invalid` with
/// `reason: "invalid"` and keeps the output.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_validation_survives_a_terminal_write_failure() {
    let steps = [
        accepted(1),
        emit(&json!({"type":"terminal","vendor_turn_id":vendor_turn(1),
                     "status":"completed","final_text":"done","stop_reason":"end_turn",
                     "structured_output":{"b":1}})),
    ];
    let Some(root) = child(
        "core_validation_survives_a_terminal_write_failure",
        &scenario(&schema_profile(), &[script("p", &steps)]),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm_failing(&root, "store.commit.terminal", true);
    run(async {
        let daemon = Daemon::open(&root);
        let schema = json!({"type":"object","required":["a"]});
        let session = daemon.spawn("p", &json!({"output_schema":schema})).await;
        let params = WaitParams {
            address: format!("{session}/1"),
            timeout_ms: Some(WAIT_MS),
        };
        let error = daemon.engine.wait(params).await.unwrap_err();
        assert_eq!(error.kind, "store_error");
        // Both attempts failed; the batch's own write then commits.
        let points = root.join("points");
        assert!(points.join("store.commit.terminal.2.ack").exists());
        fs::remove_file(points.join("store.commit.terminal.json")).unwrap();
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
        assert_eq!(envelope["structured_output"], json!({"b":1}), "{envelope}");
        let warned = invalid_warnings(&envelope);
        assert_eq!(warned.len(), 1, "{envelope}");
        assert_eq!(warned[0]["data"], json!({"reason":"invalid"}), "{envelope}");
    });
}

/// Critical r1 #5 (C1 §3.4, C2 §2 `SteerReceipt`): a steer is answered
/// only after its `steer.delivered` event committed. The vendor reports
/// the delivery, and the event's write fails (`store.commit.event`'s
/// second hit, after the acceptance's): the steer is `store_error`, never
/// a success, and no `steer.delivered` event exists.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_steer_delivery_commit_failure_is_store_error() {
    let mut profile = schema_profile();
    profile["capabilities"]["verbs"]["steer"] = json!({"support":"native"});
    let steps = [
        accepted(1),
        json!({"action":"expect_request","expected":{"type":"steer","id":3}}),
        emit(&json!({"type":"steer_delivered","id":3,"vendor_turn_id":vendor_turn(1)})),
        gate("delivered"),
        terminal(1, "completed", "end_turn"),
    ];
    let Some(root) = child(
        "core_steer_delivery_commit_failure_is_store_error",
        &scenario(&profile, &[script("p", &steps)]),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm_at(&root, "store.commit.event", 2, "fail_io");
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        // Accepted: the steer is admitted into the running turn.
        let accepted = tokio::time::Instant::now() + Duration::from_secs(20);
        while !events(&daemon, &session)
            .await
            .iter()
            .any(|event| event["type"] == "turn.started")
        {
            assert!(tokio::time::Instant::now() < accepted, "never accepted");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let raw = json!({"session":session,"handle":HANDLE,"text":"also"});
        let params = serde_json::from_value(raw.clone()).unwrap();
        let steering = tokio::spawn({
            let engine = Arc::clone(&daemon.engine);
            async move { engine.steer(params, &raw.to_string()).await }
        });
        daemon.entered("delivered").await;
        let reply = tokio::time::timeout(Duration::from_secs(20), steering)
            .await
            .expect("the steer is answered")
            .unwrap();
        assert!(
            root.join("points")
                .join("store.commit.event.2.ack")
                .exists()
        );
        let error = reply.expect_err("a failed delivery commit is no success");
        assert_eq!(error.kind, "store_error", "{error:?}");
        daemon.release("delivered");
        assert!(
            !events(&daemon, &session)
                .await
                .iter()
                .any(|event| event["type"] == "steer.delivered")
        );
        let engine = Arc::clone(&daemon.engine);
        let _report = engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        daemon.starter.abort();
    });
}

/// K2 (via-jm4.36, C1 §3.4): a keyed steer whose `steer.delivered` write
/// fails (`store.commit.event`'s second hit) is `store_error`, and its
/// outcome is not recorded with it. A repeat under the key, with no first
/// attempt in flight, gets the stored uncertain outcome, `steer_failed`
/// with `data.reason: "not_delivered"` and `data.delivery: "uncertain"`,
/// and never sends the input again: it is answered while the vendor holds
/// the turn, and no `steer.delivered` exists.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_keyed_steer_whose_outcome_was_not_recorded_replays_uncertain() {
    let mut profile = schema_profile();
    profile["capabilities"]["verbs"]["steer"] = json!({"support":"native"});
    // `ready`: the steer goes out once the turn runs, never racing its
    // dispatch into `no_active_turn`.
    let steps = [
        accepted(1),
        gate("ready"),
        json!({"action":"expect_request","expected":{"type":"steer","id":3}}),
        emit(&json!({"type":"steer_delivered","id":3,"vendor_turn_id":vendor_turn(1)})),
        gate("delivered"),
        terminal(1, "completed", "end_turn"),
    ];
    let Some(root) = child(
        "core_keyed_steer_whose_outcome_was_not_recorded_replays_uncertain",
        &scenario(&profile, &[script("p", &steps)]),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm_at(&root, "store.commit.event", 2, "fail_io");
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        let raw = json!({"session":session,"handle":HANDLE,"text":"also","op_key":"k-1"});
        let steer = || {
            let engine = Arc::clone(&daemon.engine);
            let raw = raw.clone();
            async move {
                let params = serde_json::from_value(raw.clone()).unwrap();
                engine.steer(params, &raw.to_string()).await
            }
        };
        daemon.entered("ready").await;
        daemon.release("ready");
        let first = tokio::time::timeout(Duration::from_secs(20), steer())
            .await
            .expect("the steer is answered");
        let error = first.expect_err("a failed delivery commit is no success");
        assert_eq!(error.kind, "store_error", "{error:?}");
        daemon.entered("delivered").await;
        let repeat = tokio::time::timeout(Duration::from_secs(10), steer())
            .await
            .expect("the repeat is answered while the vendor holds the turn");
        let error = repeat.expect_err("the stored outcome is uncertain");
        assert_eq!(
            (
                error.kind,
                &error.data()["reason"],
                &error.data()["delivery"]
            ),
            ("steer_failed", &json!("not_delivered"), &json!("uncertain")),
            "{error:?}"
        );
        assert_eq!(
            error.message,
            "the steer's delivery outcome was not durably recorded; whether its input was applied is unknown",
            "K2 r1 #5"
        );
        let again = steer().await.expect_err("replayed");
        assert_eq!(again.data()["delivery"], "uncertain", "{again:?}");
        daemon.release("delivered");
        assert!(
            !events(&daemon, &session)
                .await
                .iter()
                .any(|event| event["type"] == "steer.delivered")
        );
        let engine = Arc::clone(&daemon.engine);
        let _report = engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        daemon.starter.abort();
    });
}

/// Critical r2 #3 (C1 §3.4 `steer_failed`, C2 `SteerError::NotRecorded`):
/// Core holds the turn's first text item (`core.observations.pause`), so
/// the session channel is exactly full when the vendor reports a steer it
/// took; the report's delivery stalls past the lowered bound. The steer is
/// `steer_failed` with `data.reason: "not_recorded"` and the delivery a
/// success would give (`injected`), and no `steer.delivered` exists.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_steer_report_overflow_is_not_recorded() {
    let mut profile = schema_profile();
    profile["capabilities"]["verbs"]["steer"] = json!({"support":"native"});
    let flood_line = json!({"type":"text","vendor_turn_id":vendor_turn(1)}).to_string() + "\n";
    // The acceptance, then one text item Core holds and the channel's
    // worth behind it.
    let steps = [
        accepted(1),
        json!({"action":"flood","text":flood_line,"count":512}),
        gate("flood"),
        json!({"action":"flood","text":flood_line,"count":513}),
        json!({"action":"expect_request","expected":{"type":"steer","id":3}}),
        emit(&json!({"type":"steer_delivered","id":3,"vendor_turn_id":vendor_turn(1)})),
        terminal(1, "completed", "end_turn"),
    ];
    let Some(root) = child(
        "core_steer_report_overflow_is_not_recorded",
        &scenario(&profile, &[script("p", &steps)]),
        &[("VIA_TEST_EVENT_STALL_MS", "250")],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm_at(&root, "core.observations.pause", 2, "pause");
    acknowledge(&root, "adapter.observation.admitted", 513);
    acknowledge(&root, "adapter.observation.stalled", 1);
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("p", &json!({})).await;
        until_acked(&root, "core.observations.pause", 2).await;
        daemon.entered("flood").await;
        until_acked(&root, "adapter.observation.admitted", 513).await;
        acknowledge(&root, "adapter.observation.admitted", 1026);
        daemon.release("flood");
        until_acked(&root, "adapter.observation.admitted", 1026).await;
        let raw = json!({"session":session,"handle":HANDLE,"text":"also"});
        let params = serde_json::from_value(raw.clone()).unwrap();
        let steering = tokio::spawn({
            let engine = Arc::clone(&daemon.engine);
            async move { engine.steer(params, &raw.to_string()).await }
        });
        until_acked(&root, "adapter.observation.stalled", 1).await;
        let reply = tokio::time::timeout(Duration::from_secs(20), steering)
            .await
            .expect("the steer is answered")
            .unwrap();
        let error = reply.expect_err("an unrecorded steer is no success");
        assert_eq!(
            (
                error.kind,
                &error.data()["reason"],
                &error.data()["delivery"]
            ),
            ("steer_failed", &json!("not_recorded"), &json!("injected")),
            "{error:?}"
        );
        fs::write(
            root.join("points")
                .join("core.observations.pause.2.release"),
            b"",
        )
        .unwrap();
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(class(&envelope), "overflow", "{envelope}");
        assert!(
            !events(&daemon, &session)
                .await
                .iter()
                .any(|event| event["type"] == "steer.delivered")
        );
        daemon.shutdown().await;
    });
}

/// Idle session lanes the daemon keeps (runtime §8).
#[cfg(feature = "test-failpoints")]
const IDLE_LANES: usize = 32;

/// The scripts of the idle-lane cases: every session's first turn
/// confirms `v1`; a later one confirms `v2`, or `v1` again (as turn 2
/// or 3).
#[cfg(feature = "test-failpoints")]
fn idle_lane_scripts() -> Vec<Value> {
    let turn = |prompt: &str, id: &str, number: u32| {
        script(
            prompt,
            &[
                identity(id),
                accepted(number),
                terminal(number, "completed", "end_turn"),
            ],
        )
    };
    vec![
        turn("first", "v1", 1),
        turn("other", "v2", 2),
        turn("second", "v1", 3),
        turn("again", "v1", 2),
    ]
}

/// Runs `count` more sessions, one after another, each to the end of its
/// first turn.
#[cfg(feature = "test-failpoints")]
async fn run_sessions(daemon: &Daemon, count: usize) {
    for _ in 0..count {
        let session = daemon.spawn("first", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
    }
}

/// Waits until the daemon's lanes, their live actors and the tracked
/// tasks (an actor and a journal consumer per lane) are within the idle
/// lanes' bound, with nothing running.
#[cfg(feature = "test-failpoints")]
async fn until_bounded(daemon: &Daemon) {
    let by = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (lanes, live, tasks) = daemon.engine.lane_census();
        if lanes <= IDLE_LANES && live <= IDLE_LANES && tasks <= 2 * IDLE_LANES {
            return;
        }
        assert!(
            tokio::time::Instant::now() < by,
            "(lanes, live actors, tracked tasks) ({lanes}, {live}, {tasks}) past the bound \
             {IDLE_LANES}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Critical r1b #8 (runtime §8 idle session lanes, C2 §3 idle lanes):
/// sessions run one after another to completion past the bound keep at
/// most `IDLE_LANES` lanes registered and their actors' tasks live: the
/// least recently used idle lane's driver is closed, which is not the
/// session's close. Its session keeps its stored identity, unverified as
/// after a restart, and its next turn opens a new driver from it: another
/// vendor session is `resume_mismatch`, and the same one completes.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_idle_lanes_past_the_bound_reopen_from_the_stored_identity() {
    let Some(root) = child(
        "core_idle_lanes_past_the_bound_reopen_from_the_stored_identity",
        &scenario(&json!({}), &idle_lane_scripts()),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let evicted = daemon.spawn("first", &json!({})).await;
        let envelope = daemon.wait(&evicted, 1).await;
        assert_eq!(envelope["vendor_session_id"], "v1", "{envelope}");
        assert_eq!(verified(&daemon, &evicted).await, true);
        run_sessions(&daemon, IDLE_LANES + 1).await;
        until_bounded(&daemon).await;
        // C1 §3.7: the historical ID, unverified until a new generation
        // confirms; the session is not closed.
        let params = serde_json::from_value(json!({"session":evicted})).unwrap();
        let status = daemon.engine.status(params).await.unwrap();
        assert_eq!(status["vendor_session_id"], "v1", "{status}");
        assert_eq!(status["vendor_identity_verified"], false, "{status}");
        assert_eq!(status["state"], "idle", "{status}");
        let types: Vec<Value> = events(&daemon, &evicted)
            .await
            .iter()
            .map(|event| event["type"].clone())
            .collect();
        assert!(!types.contains(&json!("session.closed")), "{types:?}");

        daemon.resume(&evicted, "other").await;
        let envelope = daemon.wait(&evicted, 2).await;
        assert_eq!(class(&envelope), "resume_mismatch", "{envelope}");
        assert_eq!(envelope["vendor_session_id"], "v1", "{envelope}");
        daemon.resume(&evicted, "second").await;
        let envelope = daemon.wait(&evicted, 3).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["vendor_session_id"], "v1", "{envelope}");
        daemon.shutdown().await;
    });
}

/// Critical r1b #8 (C2 §3 idle lanes): a dispatch racing the eviction of
/// its session's lane, acknowledged at its wait (critical r2 F8), waits
/// for the evicted lane's end, its driver closed and its channel drained,
/// before the turn is submitted and a new driver opened: one driver at a
/// time. The turn is not lost: it runs on the reopened driver with the
/// stored identity.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_an_eviction_racing_a_dispatch_keeps_the_turn_and_opens_once() {
    let Some(root) = child(
        "core_an_eviction_racing_a_dispatch_keeps_the_turn_and_opens_once",
        &scenario(&json!({}), &idle_lane_scripts()),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    // The first lane to end, the evicted one, is held after its driver's
    // close, before its channel's drain.
    arm(&root, "core.lane.admission_close", "pause");
    acknowledge(&root, "core.lane.claim_wait", 1);
    run(async {
        let daemon = Daemon::open(&root);
        let evicted = daemon.spawn("first", &json!({})).await;
        let envelope = daemon.wait(&evicted, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        run_sessions(&daemon, IDLE_LANES).await;
        until_acked(&root, "core.lane.admission_close", 1).await;
        daemon.resume(&evicted, "again").await;
        // The dispatch reached its wait on the ending lane: nothing is
        // submitted.
        until_acked(&root, "core.lane.claim_wait", 1).await;
        let submitted = |events: &[Value]| {
            events
                .iter()
                .any(|event| event["type"] == "turn.submitted" && event["turn"] == 2)
        };
        let held = events(&daemon, &evicted).await;
        assert!(!submitted(&held), "{held:?}");
        fs::write(
            root.join("points")
                .join("core.lane.admission_close.1.release"),
            b"",
        )
        .unwrap();
        let envelope = daemon.wait(&evicted, 2).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["vendor_session_id"], "v1", "{envelope}");
        let after = events(&daemon, &evicted).await;
        assert!(submitted(&after), "{after:?}");
        let reopened = after
            .iter()
            .filter(|event| event["type"] == "session.reopened")
            .count();
        assert_eq!(reopened, 1, "one new driver's generation: {after:?}");
        until_bounded(&daemon).await;
        daemon.shutdown().await;
    });
}

/// The persistent profile with a native `output_schema` (via-jm4.35).
fn persistent_with_schema() -> Value {
    json!({"persistent": true, "capabilities": {
        "verbs": {"spawn":{"support":"native"},"resume":{"support":"native"},
                  "steer":{"support":"unsupported","reason":"no steer input"},
                  "cancel":{"support":"native"},"close":{"support":"native"}},
        "params": {"instructions":{"support":"unsupported","reason":"no instructions input"},
                   "output_schema":{"support":"native"},
                   "effort":{"support":"unsupported","reason":"no effort setting"},
                   "max_steps":{"support":"unsupported","reason":"no step limit"}},
        "bounds": [], "network_control": false,
        "recover": {"support":"unsupported","reason":"no recovery"},
        "usage": {"tokens":"turn","cost":"unavailable"}
    }})
}

/// The late scripts (via-jm4.35): the turn's acceptance, then a gate the
/// test releases once the turn ended `unknown`, then the helper's terminal.
fn late_scripts() -> Vec<Value> {
    let invalid = emit(&json!({"type":"terminal","vendor_turn_id":vendor_turn(1),
                               "status":"completed","final_text":"done",
                               "stop_reason":"end_turn","structured_output":{"a":"x"}}));
    vec![
        script(
            "late",
            &[
                accepted(1),
                gate("late"),
                terminal(1, "completed", "end_turn"),
            ],
        ),
        script("invalid", &[accepted(1), gate("invalid"), invalid]),
    ]
}

/// The session's `status` `turns`.
async fn status_turns(daemon: &Daemon, session: &SessionId) -> Value {
    let params = serde_json::from_value(json!({"session":session})).unwrap();
    daemon.engine.status(params).await.unwrap()["turns"].clone()
}

/// The stored envelope of `session`'s turn `turn`.
async fn stored(daemon: &Daemon, session: &SessionId, turn: u32) -> Value {
    let envelope = daemon
        .engine
        .result(&format!("{session}/{turn}"))
        .await
        .unwrap();
    serde_json::from_str(envelope.get()).unwrap()
}

/// The session's committed `turn.revised` events.
async fn revisions(daemon: &Daemon, session: &SessionId) -> Vec<Value> {
    events(daemon, session)
        .await
        .into_iter()
        .filter(|event| event["type"] == "turn.revised")
        .collect()
}

/// Waits until `session` committed a `turn.revised`.
async fn until_revised(daemon: &Daemon, session: &SessionId) {
    let by = tokio::time::Instant::now() + Duration::from_secs(30);
    while revisions(daemon, session).await.is_empty() {
        assert!(
            tokio::time::Instant::now() < by,
            "the turn was never revised"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Runs `prompt`'s session to its `unknown` end: the cancel's force passes
/// unanswered on the shared server, so the turn retained no terminal.
async fn unknown_turn(daemon: &Daemon, prompt: &str, extra: &Value) -> SessionId {
    let session = daemon.spawn(prompt, extra).await;
    daemon.entered(prompt).await;
    cancel(daemon, &session, 1, 500).await;
    let envelope = daemon.wait(&session, 1).await;
    assert_eq!(envelope["state"], "unknown", "{envelope}");
    assert_eq!(envelope["cancel"]["outcome"], "unknown", "{envelope}");
    assert!(envelope["vendor_stop_reason"].is_null(), "{envelope}");
    assert_eq!(envelope["revision"], 0, "{envelope}");
    assert_eq!(
        status_turns(daemon, &session).await,
        json!([{"n":1,"state":"unknown","revision":0}])
    );
    // Runtime §6 (fix round 1 #3): the caller's cancel that stopped it.
    assert_eq!(cancel_cause(daemon, &session).as_deref(), Some("cancel"));
    session
}

/// Turn 1's recorded `cancel_cause`, read from the Store's row.
fn cancel_cause(daemon: &Daemon, session: &SessionId) -> Option<String> {
    let db = rusqlite::Connection::open(daemon.root.join("state").join("store.sqlite3")).unwrap();
    db.busy_timeout(Duration::from_secs(10)).unwrap();
    db.query_row(
        "SELECT cancel_cause FROM turns WHERE session_id=?1 AND number=1",
        [session.as_str()],
        |row| row.get(0),
    )
    .unwrap()
}

/// via-jm4.35 (C1 §7.6 late row, C2 §4 `turn.late_terminal`), items 1 and
/// 4: on the persistent profile a cancel's force passes unanswered, so the
/// turn ends `unknown` with no retained terminal. The helper's terminal,
/// read while it is retired, revises the turn once: `turn.revised
/// {revision: 1, from_state: unknown, state: completed, evidence:
/// late_terminal}`, `late: true`, and an envelope with `revision: 1`, the
/// vendor's result and the vendor ignoring the cancel (`requested`);
/// `status` and the read report the revision. C1 Q2: a late structured
/// output is validated against the frozen schema, an invalid one revising
/// to `failed(structured_output_invalid)`.
#[test]
fn core_late_terminal_revises_an_unknown_turn() {
    let Some(root) = child(
        "core_late_terminal_revises_an_unknown_turn",
        &scenario(&persistent_with_schema(), &late_scripts()),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = unknown_turn(&daemon, "late", &json!({})).await;
        daemon.release("late");
        until_revised(&daemon, &session).await;
        let revised = revisions(&daemon, &session).await;
        assert_eq!(revised.len(), 1, "{revised:?}");
        let event = &revised[0];
        assert_eq!(
            (
                &event["turn"],
                &event["late"],
                &event["revision"],
                &event["from_state"],
                &event["state"],
                &event["evidence"]
            ),
            (
                &json!(1),
                &json!(true),
                &json!(1),
                &json!("unknown"),
                &json!("completed"),
                &json!("late_terminal")
            ),
            "{event}"
        );
        let envelope = stored(&daemon, &session, 1).await;
        assert_eq!(envelope["revision"], 1, "{envelope}");
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert!(envelope["failure"].is_null(), "{envelope}");
        assert_eq!(envelope["stop_reason"], "end_turn", "{envelope}");
        assert_eq!(envelope["vendor_stop_reason"], "end_turn", "{envelope}");
        assert_eq!(envelope["cancel"]["outcome"], "requested", "{envelope}");
        assert_eq!(envelope["events"]["last_seq"], event["seq"], "{envelope}");
        assert_eq!(
            status_turns(&daemon, &session).await,
            json!([{"n":1,"state":"completed","revision":1}])
        );
        // A revision keeps the cause (runtime §6).
        assert_eq!(cancel_cause(&daemon, &session).as_deref(), Some("cancel"));

        let schema = json!({"type":"object","properties":{"a":{"type":"integer"}},
                            "required":["a"]});
        let session = unknown_turn(&daemon, "invalid", &json!({"output_schema":schema})).await;
        daemon.release("invalid");
        until_revised(&daemon, &session).await;
        let envelope = stored(&daemon, &session, 1).await;
        assert_eq!(envelope["revision"], 1, "{envelope}");
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "structured_output_invalid", "{envelope}");
        assert_eq!(
            envelope["failure"]["data"]["reason"], "invalid",
            "{envelope}"
        );
        assert_eq!(
            envelope["structured_output"],
            json!({"a":"x"}),
            "{envelope}"
        );
        daemon.shutdown().await;
    });
}

/// via-jm4.35 item 2 (C2 §4: only a turn whose end carried no terminal):
/// a turn that retained its terminal is never revised. On the persistent
/// profile the helper reports a second terminal while it is retired; the
/// turn keeps its envelope and revision 0, and no `turn.revised` commits.
/// The session's close waits for the retirement, so the second terminal
/// was read before the events are.
#[test]
fn core_a_turn_that_retained_its_terminal_is_not_revised() {
    let scripts = [script(
        "kept",
        &[
            accepted(1),
            terminal(1, "completed", "end_turn"),
            gate("second"),
            terminal(1, "failed", "error"),
        ],
    )];
    let Some(root) = child(
        "core_a_turn_that_retained_its_terminal_is_not_revised",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("kept", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        daemon.entered("second").await;
        daemon.release("second");
        daemon.close(&session).await;
        assert!(revisions(&daemon, &session).await.is_empty());
        let after = stored(&daemon, &session, 1).await;
        assert_eq!(after, envelope);
        assert_eq!(
            status_turns(&daemon, &session).await,
            json!([{"n":1,"state":"completed","revision":0}])
        );
        daemon.shutdown().await;
    });
}

/// Fix round 1 #8 (C2 §4.1 late observations): a durable observation the
/// retired helper reports before its late terminal is forwarded in decode
/// order and committed `late: true` with its turn, before the revision.
#[test]
fn core_a_retired_helpers_denial_commits_late_before_the_revision() {
    let denial = emit(&json!({"type":"denial","vendor_turn_id":vendor_turn(1),
                              "kind":"command","target":"retired","reason":"policy"}));
    let scripts = [script(
        "late",
        &[
            accepted(1),
            gate("late"),
            denial,
            terminal(1, "completed", "end_turn"),
        ],
    )];
    let Some(root) = child(
        "core_a_retired_helpers_denial_commits_late_before_the_revision",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = unknown_turn(&daemon, "late", &json!({})).await;
        daemon.release("late");
        until_revised(&daemon, &session).await;
        let events = events(&daemon, &session).await;
        let denied: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == "action.denied")
            .collect();
        assert_eq!(denied.len(), 1, "{events:?}");
        assert_eq!(
            (&denied[0]["turn"], &denied[0]["late"], &denied[0]["target"]),
            (&json!(1), &json!(true), &json!("retired")),
            "{events:?}"
        );
        let revised = &revisions(&daemon, &session).await[0];
        assert!(
            denied[0]["seq"].as_u64() < revised["seq"].as_u64(),
            "{events:?}"
        );
        daemon.shutdown().await;
    });
}

/// Fix round 1 #9 (design §7.3): a message the retired helper reports that
/// does not decode is kept in the turn's `undecoded.bin`, as the turn's
/// own reader keeps one; reading stops there, so the terminal after it
/// revises nothing and the committed result stands.
#[test]
fn core_an_undecodable_retired_message_keeps_its_evidence() {
    let malformed = emit(&json!({"type":"terminal","vendor_turn_id":vendor_turn(1)}));
    let scripts = [script(
        "late",
        &[
            accepted(1),
            gate("late"),
            malformed,
            terminal(1, "completed", "end_turn"),
        ],
    )];
    let Some(root) = child(
        "core_an_undecodable_retired_message_keeps_its_evidence",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = unknown_turn(&daemon, "late", &json!({})).await;
        let before = stored(&daemon, &session, 1).await;
        let kept =
            PathBuf::from(before["evidence"]["folder"].as_str().unwrap()).join("undecoded.bin");
        daemon.release("late");
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        while !kept.exists() || daemon.engine.harness_processes().in_use != 0 {
            assert!(
                tokio::time::Instant::now() < by,
                "the retirement kept no undecoded.bin, or never ended"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(revisions(&daemon, &session).await.is_empty());
        assert_eq!(stored(&daemon, &session, 1).await, before);
        daemon.shutdown().await;
    });
}

/// Fix round 4 #1 (C2 §2 health, runtime §7): a failure of the retired
/// helper's output fails the driver's health as soon as its reading met
/// it, while the helper still runs: its message that does not decode is
/// kept, Core's lane sees the failed health (`core.lane.retire`), and the
/// retirement's cleanup (`adapter.fake.retirement_cleaned`) has not
/// ended, as the helper holds at its gate.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_a_retirement_failure_fails_health_before_its_cleanup() {
    let malformed = emit(&json!({"type":"terminal","vendor_turn_id":vendor_turn(1)}));
    let scripts = [script(
        "late",
        &[accepted(1), gate("late"), malformed, gate("hold")],
    )];
    let Some(root) = child(
        "core_a_retirement_failure_fails_health_before_its_cleanup",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    acknowledge(&root, "core.lane.retire", 1);
    acknowledge(&root, "adapter.fake.retirement_cleaned", 1);
    run(async {
        let daemon = Daemon::open(&root);
        let session = unknown_turn(&daemon, "late", &json!({})).await;
        let before = stored(&daemon, &session, 1).await;
        let kept =
            PathBuf::from(before["evidence"]["folder"].as_str().unwrap()).join("undecoded.bin");
        daemon.release("late");
        until_acked(&root, "core.lane.retire", 1).await;
        assert!(kept.exists(), "the undecodable message was kept");
        assert!(
            !root
                .join("points")
                .join("adapter.fake.retirement_cleaned.1.ack")
                .exists(),
            "the cleanup still waits for the helper"
        );
        daemon.entered("hold").await;
        daemon.release("hold");
        until_acked(&root, "adapter.fake.retirement_cleaned", 1).await;
        assert!(revisions(&daemon, &session).await.is_empty());
        daemon.shutdown().await;
    });
}

/// Critical fix r1 #2 (C2 §4: one order across a session's producers):
/// the retired helper's observations follow the turn's own. The turn's
/// acceptance is decoded and handed over, but its delivery is held
/// (`adapter.fake.stamped`) while the cancel's force ends the turn with no
/// terminal and the retired helper's late terminal is read and offered
/// (`adapter.fake.retirement_item`). Released, the acceptance commits
/// first, so the late terminal names a mapped vendor turn and revises it.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_a_late_terminal_follows_the_turns_own_delivery() {
    let scripts = [script(
        "late",
        &[
            accepted(1),
            gate("late"),
            terminal(1, "completed", "end_turn"),
        ],
    )];
    let Some(root) = child(
        "core_a_late_terminal_follows_the_turns_own_delivery",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, "adapter.fake.stamped", "pause");
    acknowledge(&root, "routes.fake.retiring", 1);
    acknowledge(&root, "adapter.fake.retirement_item", 1);
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("late", &json!({})).await;
        until_acked(&root, "adapter.fake.stamped", 1).await;
        daemon.entered("late").await;
        cancel(&daemon, &session, 1, 500).await;
        until_acked(&root, "routes.fake.retiring", 1).await;
        daemon.release("late");
        until_acked(&root, "adapter.fake.retirement_item", 1).await;
        release_point(&root, "adapter.fake.stamped", 1);
        daemon.wait(&session, 1).await;
        until_revised(&daemon, &session).await;
        let envelope = stored(&daemon, &session, 1).await;
        assert_eq!(
            (&envelope["state"], &envelope["revision"]),
            (&json!("completed"), &json!(1)),
            "{envelope}"
        );
        daemon.shutdown().await;
    });
}

/// Every event of `session`, page by page.
#[cfg(feature = "test-failpoints")]
async fn all_events(daemon: &Daemon, session: &SessionId) -> Vec<Value> {
    let mut all = Vec::new();
    loop {
        let after = all
            .last()
            .map_or(0, |event: &Value| event["seq"].as_u64().unwrap());
        let params =
            serde_json::from_value(json!({"session":session,"after":after,"limit":1000})).unwrap();
        let page = daemon.engine.events(params).await.unwrap();
        let page: Value = serde_json::from_str(page.get()).unwrap();
        let events = page["events"].as_array().unwrap().clone();
        if events.is_empty() {
            return all;
        }
        all.extend(events);
    }
}

/// A denial of `target` for turn `turn`'s vendor turn.
fn denial(turn: u32, target: &str) -> Value {
    emit(&json!({"type":"denial","vendor_turn_id":vendor_turn(turn),
                 "kind":"command","target":target,"reason":"policy"}))
}

/// Fix round 2 #1 (C2 §2 health, §4.1; runtime §7): a retirement whose
/// cleanup is unproven fails the driver's health, and the lane closes the
/// driver while the helper's 1,024 denials and its late terminal are
/// still being delivered. The lane drains its channel while the close
/// runs, so the delivery and the close wait for nothing of each other:
/// every denial commits late and the terminal revises the turn.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_a_failed_retirement_delivers_its_observations_while_it_closes() {
    let mut steps = vec![accepted(1), gate("late")];
    steps.extend((0..1_024).map(|n| denial(1, &format!("retired-{n}"))));
    steps.push(terminal(1, "completed", "end_turn"));
    let Some(root) = child(
        "core_a_failed_retirement_delivers_its_observations_while_it_closes",
        &scenario(&persistent(), &[script("late", &steps)]),
        &[("VIA_TEST_FAKE_RETIREMENT_UNCERTAIN", "1")],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = unknown_turn(&daemon, "late", &json!({})).await;
        daemon.release("late");
        // Past the first events page: `until_revised` reads only that one.
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        let events = loop {
            let events = all_events(&daemon, &session).await;
            if events.iter().any(|event| event["type"] == "turn.revised") {
                break events;
            }
            assert!(
                tokio::time::Instant::now() < by,
                "the turn was never revised"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let denied = events
            .iter()
            .filter(|event| event["type"] == "action.denied")
            .filter(|event| event["late"] == true && event["turn"] == 1)
            .count();
        assert_eq!(denied, 1_024);
        assert_eq!(stored(&daemon, &session, 1).await["state"], "completed");
        daemon.shutdown().await;
    });
}

/// Fix round 2 #1, round 3 #3 and #4 (C2 §2 health, §4.1): the lane
/// disposes of what its channel receives while it closes the driver.
/// Core holds its first late item (`core.lane.dispose`), so the session
/// channel fills with the retired helper's denials while its terminal
/// still waits behind them. The retirement's cleanup ends and its
/// unproven cleanup is reported while that delivery is still blocked;
/// once the item is released the failed driver is closed, and its close
/// waits for the delivery while the lane drains: the terminal revises the
/// turn.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_a_closing_lane_drains_what_its_driver_still_delivers() {
    let line = json!({"type":"denial","vendor_turn_id":vendor_turn(1),
                      "kind":"command","target":"t","reason":"policy"})
    .to_string()
        + "\n";
    let steps = [
        accepted(1),
        gate("late"),
        json!({"action":"flood","text":line,"count":RETIRED_DENIALS}),
        terminal(1, "completed", "end_turn"),
    ];
    let Some(root) = child(
        "core_a_closing_lane_drains_what_its_driver_still_delivers",
        &scenario(&persistent(), &[script("late", &steps)]),
        &[("VIA_TEST_FAKE_RETIREMENT_UNCERTAIN", "1")],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, "core.lane.dispose", "pause");
    acknowledge(&root, "adapter.fake.retirement_cleaned", 1);
    acknowledge(&root, "adapter.fake.retirement_delivered", 1);
    acknowledge(&root, "core.lane.close_draining", 1);
    run(async {
        let daemon = Daemon::open(&root);
        let session = unknown_turn(&daemon, "late", &json!({})).await;
        daemon.release("late");
        until_acked(&root, "core.lane.dispose", 1).await;
        // The retirement's cleanup and its reports are published while
        // its delivery is still blocked behind the held item.
        until_acked(&root, "adapter.fake.retirement_cleaned", 1).await;
        let delivered = root
            .join("points")
            .join("adapter.fake.retirement_delivered.1.ack");
        assert!(!delivered.exists(), "the delivery was not blocked");
        fs::write(root.join("points").join("core.lane.dispose.1.release"), b"").unwrap();
        // The failed driver's close starts, and the delivery ends only
        // while the lane drains beside it.
        until_acked(&root, "core.lane.close_draining", 1).await;
        until_acked(&root, "adapter.fake.retirement_delivered", 1).await;
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        let events = loop {
            let events = all_events(&daemon, &session).await;
            if events.iter().any(|event| event["type"] == "turn.revised") {
                break events;
            }
            assert!(
                tokio::time::Instant::now() < by,
                "the turn was never revised"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let denied = events
            .iter()
            .filter(|event| event["type"] == "action.denied")
            .count();
        assert_eq!(denied, RETIRED_DENIALS);
        daemon.shutdown().await;
    });
}

/// Fix round 3 #2, round 4 #4 (C1 §3.6): the driver's close is polled
/// while the lane disposes of an item, so its deadline never waits behind
/// Store work. A close under a 1 s deadline starts; the retired helper's
/// first denial is then held in its disposal (`core.lane.dispose`), and
/// its second at the driver's delivery (`adapter.fake.retirement_item`),
/// so the close's delivery ends at the deadline. The close has then ended
/// and the channel's admission closed while the first is still held
/// (`core.lane.admission_closed`); the second, sent only then, is refused
/// (`adapter.fake.retirement_delivered`) and never committed, while the
/// held one still is.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_a_held_disposal_delays_no_close() {
    let scripts = [script(
        "first",
        &[
            accepted(1),
            terminal(1, "completed", "end_turn"),
            gate("before"),
            denial(1, "held"),
            denial(1, "late"),
        ],
    )];
    let Some(root) = child(
        "core_a_held_disposal_delays_no_close",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, "core.lane.dispose", "pause");
    arm_at(&root, "adapter.fake.retirement_item", 2, "pause");
    acknowledge(&root, "core.lane.close_draining", 1);
    acknowledge(&root, "core.lane.admission_closed", 1);
    acknowledge(&root, "adapter.fake.retirement_delivered", 1);
    run(async {
        let daemon = Daemon::open(&root);
        let session = daemon.spawn("first", &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        daemon.entered("before").await;
        let steps = async {
            until_acked(&root, "core.lane.close_draining", 1).await;
            daemon.release("before");
            until_acked(&root, "core.lane.dispose", 1).await;
            until_acked(&root, "adapter.fake.retirement_item", 2).await;
            // The close ended at its deadline, the first denial still held.
            until_acked(&root, "core.lane.admission_closed", 1).await;
            release_point(&root, "adapter.fake.retirement_item", 2);
            until_acked(&root, "adapter.fake.retirement_delivered", 1).await;
            release_point(&root, "core.lane.dispose", 1);
        };
        let (_closed, ()) = tokio::join!(daemon.close_within(&session, 1_000), steps);
        let events = events(&daemon, &session).await;
        let denied: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == "action.denied")
            .collect();
        assert_eq!(denied.len(), 1, "{events:?}");
        assert_eq!(denied[0]["target"], "held", "{events:?}");
        daemon.shutdown().await;
    });
}

/// The retired helper's denials in
/// [`core_a_closing_lane_drains_what_its_driver_still_delivers`]: one
/// held by Core, the session channel's 1,024 items, and one waiting at
/// the driver, so its terminal waits on Route's hand-over.
#[cfg(feature = "test-failpoints")]
const RETIRED_DENIALS: usize = 1_025;

/// Fix round 2 #4 (C2 §4.1 late observations): the retired helper's
/// durable observations are forwarded until its output ends, not only up
/// to its late terminal: a denial after the terminal commits late too.
#[test]
fn core_a_denial_after_the_late_terminal_commits_late() {
    let scripts = [script(
        "late",
        &[
            accepted(1),
            gate("late"),
            terminal(1, "completed", "end_turn"),
            denial(1, "after"),
        ],
    )];
    let Some(root) = child(
        "core_a_denial_after_the_late_terminal_commits_late",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = unknown_turn(&daemon, "late", &json!({})).await;
        daemon.release("late");
        until_revised(&daemon, &session).await;
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        let denied = loop {
            let events = events(&daemon, &session).await;
            if let Some(denied) = events
                .into_iter()
                .find(|event| event["type"] == "action.denied")
            {
                break denied;
            }
            assert!(
                tokio::time::Instant::now() < by,
                "the denial after the late terminal never committed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(
            (&denied["turn"], &denied["late"], &denied["target"]),
            (&json!(1), &json!(true), &json!("after")),
            "{denied}"
        );
        daemon.shutdown().await;
    });
}

/// Fix round 2 #5 (C2 §2 health, §4.1): the retired helper's messages are
/// checked against the connection's protocol phase as the turn's own are.
/// A second terminal is a protocol failure: the driver's health fails and
/// the lane retires it, releasing its slot, while the revision the first
/// terminal made stands.
#[test]
fn core_a_second_retired_terminal_fails_the_connection() {
    let scripts = [script(
        "late",
        &[
            accepted(1),
            gate("late"),
            terminal(1, "completed", "end_turn"),
            terminal(1, "failed", "error"),
        ],
    )];
    let Some(root) = child(
        "core_a_second_retired_terminal_fails_the_connection",
        &scenario(&persistent(), &scripts),
        &[],
    ) else {
        return;
    };
    run(async {
        let daemon = Daemon::open(&root);
        let session = unknown_turn(&daemon, "late", &json!({})).await;
        daemon.release("late");
        until_revised(&daemon, &session).await;
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        while daemon.engine.harness_processes().in_use != 0 {
            assert!(
                tokio::time::Instant::now() < by,
                "the connection outlived its protocol failure"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let envelope = stored(&daemon, &session, 1).await;
        assert_eq!(
            (&envelope["state"], &envelope["revision"]),
            (&json!("completed"), &json!(1)),
            "{envelope}"
        );
        assert_eq!(revisions(&daemon, &session).await.len(), 1);
        daemon.shutdown().await;
    });
}

/// The hits of failpoint `point` a wrong-token command counted so far.
#[cfg(feature = "test-failpoints")]
fn hits(root: &Path, point: &str) -> u64 {
    let prefix = format!("{point}.");
    fs::read_dir(root.join("points"))
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            name.strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(".refused"))
                .and_then(|number| number.parse::<u64>().ok())
        })
        .max()
        .unwrap_or(0)
}

/// Releases `point`'s paused hit `occurrence`.
#[cfg(feature = "test-failpoints")]
fn release_point(root: &Path, point: &str, occurrence: u64) {
    fs::write(
        root.join("points")
            .join(format!("{point}.{occurrence}.release")),
        b"",
    )
    .unwrap();
}

impl Daemon {
    /// Ends a daemon whose Store failure latched, as its process would:
    /// final shutdown, then every task holding the Engine, so the next
    /// daemon on the same root takes the Store lock.
    #[cfg(feature = "test-failpoints")]
    async fn end_latched(self) {
        let _report = self
            .engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
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

/// Waits until the daemon's Store failure latched.
#[cfg(feature = "test-failpoints")]
async fn until_latched(daemon: &Daemon) {
    let by = tokio::time::Instant::now() + Duration::from_secs(30);
    while !daemon.engine.store_failed() {
        assert!(
            tokio::time::Instant::now() < by,
            "Store failure never latched"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// via-jm4.35 item 3 (C1 §7.6, runtime §7): an uncertain revision commit
/// latches Store failure and is never assumed absent; the restarted daemon
/// reads what is durable. Case `durable`: the commit's reply is lost after
/// it committed (`store.commit.reply_lost`), and the restart reads the
/// first revision. Case `absent`: the commit fails and its rollback is
/// reported failed (`store.commit.revision`, `store.rollback.fail`), and
/// the restart reads the turn `unknown` at revision 0 with no
/// `turn.revised`.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_an_uncertain_revision_latches_and_reconciles_at_restart() {
    const NAME: &str = "core_an_uncertain_revision_latches_and_reconciles_at_restart";
    let scenario = scenario(&persistent(), &late_scripts()[..1]);
    let Some(root) = child(NAME, &scenario, &[]) else {
        child_case(NAME, &scenario, "absent");
        return;
    };
    let durable = case().is_none();
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    // The revision is held before its commit is sent.
    arm(&root, "core.revision.commit", "pause");
    if durable {
        // Counted with a wrong token until the revision's own hit is known.
        let counting = json!({"token":"counting","occurrence":1,"action":"pause"});
        fs::write(
            root.join("points").join("store.commit.reply_lost.json"),
            counting.to_string(),
        )
        .unwrap();
    } else {
        arm(&root, "store.commit.revision", "fail_io");
        arm(&root, "store.rollback.fail", "fail_io");
    }
    run(async {
        let session = {
            let daemon = Daemon::open(&root);
            let session = unknown_turn(&daemon, "late", &json!({})).await;
            daemon.release("late");
            until_acked(&root, "core.revision.commit", 1).await;
            if durable {
                let next = hits(&root, "store.commit.reply_lost") + 1;
                arm_at(&root, "store.commit.reply_lost", next, "fail_io");
            }
            release_point(&root, "core.revision.commit", 1);
            until_latched(&daemon).await;
            daemon.end_latched().await;
            session
        };
        let daemon = Daemon::open(&root);
        daemon.engine.recover().await.unwrap();
        let envelope = stored(&daemon, &session, 1).await;
        let revised = revisions(&daemon, &session).await;
        if durable {
            assert_eq!(envelope["revision"], 1, "{envelope}");
            assert_eq!(envelope["state"], "completed", "{envelope}");
            assert_eq!(revised.len(), 1, "{revised:?}");
            assert_eq!(
                status_turns(&daemon, &session).await,
                json!([{"n":1,"state":"completed","revision":1}])
            );
        } else {
            assert_eq!(envelope["revision"], 0, "{envelope}");
            assert_eq!(envelope["state"], "unknown", "{envelope}");
            assert!(revised.is_empty(), "{revised:?}");
            assert_eq!(
                status_turns(&daemon, &session).await,
                json!([{"n":1,"state":"unknown","revision":0}])
            );
        }
        daemon.shutdown().await;
    });
}

/// via-jm4.35 (C1 §7.6, runtime §7): a revision known not committed is
/// retried once at the same sequence. Case `retried`: the first attempt
/// rolls back and the retry commits, revision 1, nothing latched. Case
/// `not_made`: the retry fails too, so the revision is not made, the turn
/// stays `unknown` at revision 0 with no `turn.revised`; both failures are
/// the turn's, scoped, and nothing latches.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_a_revision_not_committed_is_retried_once() {
    const NAME: &str = "core_a_revision_not_committed_is_retried_once";
    let scenario = scenario(&persistent(), &late_scripts()[..1]);
    let Some(root) = child(NAME, &scenario, &[]) else {
        child_case(NAME, &scenario, "not_made");
        return;
    };
    let retried = case().is_none();
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm_failing(&root, "store.commit.revision", !retried);
    run(async {
        let daemon = Daemon::open(&root);
        let session = unknown_turn(&daemon, "late", &json!({})).await;
        daemon.release("late");
        // The first attempt's hit, and with `persist` the retry's.
        until_acked(&root, "store.commit.revision", if retried { 1 } else { 2 }).await;
        if retried {
            until_revised(&daemon, &session).await;
            let envelope = stored(&daemon, &session, 1).await;
            assert_eq!(envelope["revision"], 1, "{envelope}");
            assert_eq!(envelope["state"], "completed", "{envelope}");
            assert!(!daemon.engine.store_failed());
            daemon.shutdown().await;
            return;
        }
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        let failure = loop {
            let failure = daemon.engine.store_failure_status();
            if failure
                .as_ref()
                .is_some_and(|failure| failure["count"] == 2)
            {
                break failure.unwrap();
            }
            assert!(
                tokio::time::Instant::now() < by,
                "both failures were never recorded: {failure:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(
            (&failure["kind"], &failure["scope"]),
            (&json!("commit_failed"), &json!("turn")),
            "{failure}"
        );
        assert!(!daemon.engine.store_failed());
        let envelope = stored(&daemon, &session, 1).await;
        assert_eq!(envelope["revision"], 0, "{envelope}");
        assert_eq!(envelope["state"], "unknown", "{envelope}");
        assert!(revisions(&daemon, &session).await.is_empty());
        daemon.shutdown().await;
    });
}

// x.3.2 X4: the order's attach instant (I11), Core's `by_order` guard
// (R1) and P7 through Engine (design §5 "Ordering evidence", "Codex under
// Engine"). No ordering claim rests on a time offset: each step waits on
// an acknowledged seam or a progress line, and an instant is compared with
// the wall by causal bounds (`t_s + W <= wall <= t_c + W`).

/// Core's cancel, before its order's publication (pause seam).
#[cfg(feature = "test-failpoints")]
const PUBLISH: &str = "core.cancel.publish";
/// Core's cancel, past its order's attach.
#[cfg(feature = "test-failpoints")]
const ORDERED: &str = "core.cancel.ordered";
/// The driver's turn returned, before disposition.
#[cfg(feature = "test-failpoints")]
const RETURNED: &str = "core.run.returned";
/// The submission clock was taken, before the submission's commit.
#[cfg(feature = "test-failpoints")]
const SUBMIT: &str = "core.submit.before_commit";
/// An accepted Codex turn's wait, before it polls its orders.
#[cfg(feature = "test-failpoints")]
const CODEX_ORDERED: &str = "adapter.codex.ordered";

/// Arms `point` to acknowledge every hit from `occurrence` on.
#[cfg(feature = "test-failpoints")]
fn acknowledge_every(root: &Path, point: &str, occurrence: u64) {
    let command = json!({"token":"conformance-core","occurrence":occurrence,
                         "action":"delay","value":0,"persist":true});
    fs::write(
        root.join("points").join(format!("{point}.json")),
        command.to_string(),
    )
    .unwrap();
}

/// The envelope's `cancel` `{outcome, cleanup}`.
fn stop_pair(envelope: &Value) -> (&Value, &Value) {
    (
        &envelope["cancel"]["outcome"],
        &envelope["cancel"]["cleanup"],
    )
}

/// The persistent profile's `answers` script: the wall's interrupt is
/// answered `interrupted`.
#[cfg(feature = "test-failpoints")]
fn answers() -> Value {
    script(
        "answers",
        &[
            accepted(1),
            expect_interrupt(1),
            terminal(1, "interrupted", "interrupted"),
        ],
    )
}

/// The fake-profile wall.
#[cfg(feature = "test-failpoints")]
const WALL: Duration = Duration::from_millis(2_000);

/// d3 (I11, delayed publication): a cancel held before the wall at
/// `core.cancel.publish` publishes after the wall fired and Route returned
/// `Deadline` with the vendor's acknowledgement; disposition waits for the
/// publication. The order's `attached` is its publication, after the wall,
/// so the result is the wall's, `failed(deadline_wall)` acknowledged and
/// quiescent, never the order's `unknown`.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_cancel_published_after_wall() {
    let Some(root) = child(
        "core_cancel_published_after_wall",
        &scenario(&persistent(), &[answers()]),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, PUBLISH, "pause");
    arm(&root, RETURNED, "pause");
    acknowledge(&root, ORDERED, 1);
    run(async {
        let daemon = Daemon::open(&root);
        let started = tokio::time::Instant::now();
        let wall = json!({"deadlines":{"wall_ms":WALL.as_millis()}});
        let session = daemon.spawn("answers", &wall).await;
        let ((), ()) = tokio::join!(cancel(&daemon, &session, 1, 60_000), async {
            until_acked(&root, PUBLISH, 1).await;
            assert!(
                tokio::time::Instant::now() < started + WALL,
                "the cancel is held before the wall"
            );
            until_acked(&root, RETURNED, 1).await;
            release_point(&root, PUBLISH, 1);
            until_acked(&root, ORDERED, 1).await;
            release_point(&root, RETURNED, 1);
        });
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "deadline_wall", "{envelope}");
        assert_eq!(
            stop_pair(&envelope),
            (&json!("acknowledged"), &json!("quiescent")),
            "{envelope}"
        );
        daemon.shutdown().await;
    });
}

/// d3 (I11, reversed concurrent callers): cancel A is held before the
/// wall; after the wall's `Deadline`, cancel B publishes, then A merges
/// into B's order. A's earlier call never re-dates the order: the result
/// is the wall's, and both replies resolve.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_cancel_reversed_publication() {
    let Some(root) = child(
        "core_cancel_reversed_publication",
        &scenario(&persistent(), &[answers()]),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, PUBLISH, "pause");
    arm(&root, RETURNED, "pause");
    acknowledge_every(&root, ORDERED, 1);
    run(async {
        let daemon = Daemon::open(&root);
        let started = tokio::time::Instant::now();
        let wall = json!({"deadlines":{"wall_ms":WALL.as_millis()}});
        let session = daemon.spawn("answers", &wall).await;
        let ((), ()) = tokio::join!(cancel(&daemon, &session, 1, 60_000), async {
            until_acked(&root, PUBLISH, 1).await;
            assert!(
                tokio::time::Instant::now() < started + WALL,
                "cancel A is held before the wall"
            );
            until_acked(&root, RETURNED, 1).await;
            let ((), ()) = tokio::join!(cancel(&daemon, &session, 1, 60_000), async {
                until_acked(&root, ORDERED, 1).await;
                release_point(&root, PUBLISH, 1);
                until_acked(&root, ORDERED, 2).await;
                release_point(&root, RETURNED, 1);
            });
        });
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "deadline_wall", "{envelope}");
        assert_eq!(
            stop_pair(&envelope),
            (&json!("acknowledged"), &json!("quiescent")),
            "{envelope}"
        );
        daemon.shutdown().await;
    });
}

/// d3 minor (R1 on the private lifecycle Claude shares): an order attached
/// after the private route's wall force, while the turn is still
/// `running`, leaves the wall's result, `failed(deadline_wall)`, with the
/// same `cancel` pair as the same script's wall run without a cancel.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_private_cancel_after_wall() {
    let hangs = [hello("1.0", &["turns"]), accepted(1), hang()];
    let Some(root) = child(
        "core_private_cancel_after_wall",
        &scenario(
            &handshake(),
            &[script("twin", &hangs), script("late", &hangs)],
        ),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm_at(&root, RETURNED, 2, "pause");
    acknowledge(&root, ORDERED, 1);
    run(async {
        let daemon = Daemon::open(&root);
        let wall = json!({"deadlines":{"wall_ms":1_500}});
        let twin = daemon.spawn("twin", &wall).await;
        let twin = daemon.wait(&twin, 1).await;
        assert_eq!(class(&twin), "deadline_wall", "{twin}");
        assert_eq!(twin["cancel"]["outcome"], "forced", "{twin}");
        let session = daemon.spawn("late", &wall).await;
        until_acked(&root, RETURNED, 2).await;
        let ((), ()) = tokio::join!(cancel(&daemon, &session, 1, 60_000), async {
            until_acked(&root, ORDERED, 1).await;
            release_point(&root, RETURNED, 2);
        });
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "deadline_wall", "{envelope}");
        assert_eq!(stop_pair(&envelope), stop_pair(&twin), "{envelope}");
        daemon.shutdown().await;
    });
}

/// A fake deployment no Codex case runs: Core's config requires one.
fn no_fake() -> Value {
    scenario(&json!({}), &[script("unused", &[hang()])])
}

/// The Codex fixtures' first prompt.
const CODEX_PROMPT: &str = "Run the shell command sleep 75. Then say done.";
/// F16c's successor prompt.
const SUCCESSOR: &str = "Ask me one clarifying question before answering.";

/// Spawn members for a Codex case on `case`'s cwd, with wall `wall_ms`.
fn codex_spawn(case: &core_codex::CodexCase, wall_ms: u64) -> Value {
    json!({"harness":"codex","model":"gpt-6-sol","effort":"low",
           "bound":{"mode":"full","extra_write_dirs":[],"network":true},
           "cwd":case.cwd(),"deadlines":{"wall_ms":wall_ms}})
}

/// Sets Codex case `name` up on `replay`.
fn codex_case(root: &Path, name: &str, replay: Value) -> core_codex::CodexCase {
    core_codex::CodexCase::new(root, name, replay, &binary("via-fake-agent"))
}

/// R1, public half (d4 #1, #3): on `c3_wall_interrupt`'s replay the wall
/// fires and the real Codex route's acknowledgement finishes (held at
/// `core.run.returned`); then a cancel attaches while the turn is still
/// `running`. The envelope is the wall's: `failed(deadline_wall)`,
/// acknowledged, with the fixture's wall-only cleanup; never `unknown`.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_codex_cancel_after_wall() {
    const NAME: &str = "c3_wall_interrupt";
    let Some(root) = child("core_codex_cancel_after_wall", &no_fake(), &[]) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, RETURNED, "pause");
    acknowledge(&root, ORDERED, 1);
    let case = codex_case(&root, NAME, core_codex::replay(NAME));
    let cleanup = core_codex::expected(NAME)["turns"][0]["expect"]["cleanup"].clone();
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        let session = daemon.spawn(CODEX_PROMPT, &codex_spawn(&case, 2_000)).await;
        until_acked(&root, RETURNED, 1).await;
        let ((), ()) = tokio::join!(cancel(&daemon, &session, 1, 60_000), async {
            until_acked(&root, ORDERED, 1).await;
            release_point(&root, RETURNED, 1);
        });
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "deadline_wall", "{envelope}");
        assert_eq!(
            stop_pair(&envelope),
            (&json!("acknowledged"), &cleanup),
            "{envelope}"
        );
        daemon.close(&session).await;
        daemon.shutdown().await;
    });
}

/// d2 #1, public half (d4 #1, #2): on `c3_interrupt_uncertain`'s replay
/// the accepted turn's wait is held before it polls its orders; a cancel
/// publishes before the wall (`t_o < t_s + W`); the wait is released once
/// the wall has surely passed (`t_c + W`). The order came first, so the
/// envelope is the order's: `cancelled`, `interrupted`, acknowledged.
#[cfg(feature = "test-failpoints")]
#[test]
fn core_codex_cancel_before_wall_noticed_late() {
    const NAME: &str = "c3_interrupt_uncertain";
    const W: Duration = Duration::from_millis(3_000);
    let Some(root) = child(
        "core_codex_cancel_before_wall_noticed_late",
        &no_fake(),
        &[],
    ) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, SUBMIT, "pause");
    arm(&root, CODEX_ORDERED, "pause");
    acknowledge(&root, ORDERED, 1);
    let case = codex_case(&root, NAME, core_codex::replay(NAME));
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        let started = tokio::time::Instant::now();
        let wall_ms = u64::try_from(W.as_millis()).unwrap();
        let session = daemon
            .spawn(CODEX_PROMPT, &codex_spawn(&case, wall_ms))
            .await;
        until_acked(&root, SUBMIT, 1).await;
        let committed = tokio::time::Instant::now();
        release_point(&root, SUBMIT, 1);
        until_acked(&root, CODEX_ORDERED, 1).await;
        let ((), ()) = tokio::join!(cancel(&daemon, &session, 1, 60_000), async {
            until_acked(&root, ORDERED, 1).await;
            assert!(
                tokio::time::Instant::now() < started + W,
                "the order was published before the wall"
            );
        });
        tokio::time::sleep_until(committed + W).await;
        release_point(&root, CODEX_ORDERED, 1);
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "cancelled", "{envelope}");
        assert_eq!(envelope["stop_reason"], "interrupted", "{envelope}");
        assert_eq!(envelope["cancel"]["outcome"], "acknowledged", "{envelope}");
        daemon.close(&session).await;
        daemon.shutdown().await;
    });
}

/// F16c's copy of `c3_interrupt_uncertain` (design §5 F16c row; steps
/// one-based): a cancel-point gate after original step 22 (copy step
/// 23); with `completion`, a gate after original step 27 (copy step 29)
/// and then the open tool's `item/completed`; then a successor turn shaped
/// like `c1_commentary_usage`'s second (with `successor`); then the
/// original close. The run deadline is 90 s.
fn p7_copy(completion: bool, successor: bool) -> Value {
    let mut replay = core_codex::replay("c3_interrupt_uncertain");
    let original = replay["steps"].as_array().unwrap().clone();
    let shape = core_codex::replay("c1_commentary_usage");
    let second = shape["steps"].as_array().unwrap();
    let gate = json!({"await_signal":{"signal":"SIGUSR1"}});
    let mut steps = original[..22].to_vec();
    steps.push(gate.clone());
    steps.extend_from_slice(&original[22..27]);
    if completion {
        steps.push(gate);
        let line = original[21]["emit"]["line"].as_str().unwrap();
        let mut item: Value = serde_json::from_str(line).unwrap();
        assert_eq!(item["method"], "item/started", "{item}");
        item["method"] = json!("item/completed");
        let params = item["params"].as_object_mut().unwrap();
        let at = params.remove("startedAtMs").unwrap();
        params.insert("completedAtMs".to_owned(), at);
        params["item"]["status"] = json!("completed");
        params["item"]["exitCode"] = json!(0);
        params["item"]["durationMs"] = json!(1_000);
        steps.push(json!({"emit":{"line":item.to_string()}}));
    }
    if successor {
        let mut start = second[29].clone();
        assert_eq!(start["expect"]["line"]["method"], "turn/start", "{start}");
        start["expect"]["line"]["params"]["input"][0]["text"] = json!(SUCCESSOR);
        steps.push(start);
        for step in [31, 33, 44, 45, 46, 49] {
            steps.push(second[step - 1].clone());
        }
    }
    steps.extend_from_slice(&original[27..]);
    replay["steps"] = Value::Array(steps);
    replay["deadline_ms"] = json!(90_000);
    replay
}

/// The session's `status`.
async fn status(daemon: &Daemon, session: &SessionId) -> Value {
    let params = serde_json::from_value(json!({"session":session})).unwrap();
    daemon.engine.status(params).await.unwrap()
}

/// Polls `status` until the running turn's cancel shows `acknowledged`.
async fn until_acknowledged(daemon: &Daemon, session: &SessionId) -> Value {
    let by = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let shown = status(daemon, session).await;
        if shown["active_turn"]["cancel"]["outcome"] == "acknowledged" {
            return shown;
        }
        assert!(
            tokio::time::Instant::now() < by,
            "never acknowledged: {shown}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The codes of `envelope`'s warnings.
#[cfg(feature = "test-failpoints")]
fn warning_codes(envelope: &Value) -> Vec<&str> {
    envelope["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|warning| warning["code"].as_str())
        .collect()
}

/// Asserts `second` was submitted only once `first` had ended.
fn dispatched_after(first: &Value, second: &Value) {
    let ended = first["timestamps"]["ended_at"].as_str().unwrap();
    let submitted = second["timestamps"]["submitted_at"].as_str().unwrap();
    assert!(submitted >= ended, "{first} {second}");
}

/// Turn 2's state in `status` `turns`.
fn second_state(status: &Value) -> &Value {
    status["turns"]
        .as_array()
        .and_then(|turns| turns.iter().find(|turn| turn["n"] == 2))
        .map_or(&Value::Null, |turn| &turn["state"])
}

/// F16c (D7, C1 §3.5 P7, P6): a cancel of the open `sleep 75` tool's turn,
/// under Engine's real 60 s grace. Once the interrupted terminal is
/// retained, `status` shows `{acknowledged, pending, settled_at: null}`
/// while the turn runs, and a queued successor stays `queued`. The tool's
/// completion then settles the turn `cancelled`, acknowledged, quiescent
/// within seconds, and only then does the successor run.
#[test]
fn core_codex_p7_status() {
    const NAME: &str = "core_codex_p7_status";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    let case = codex_case(&root, NAME, p7_copy(true, true));
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        let session = daemon
            .spawn(CODEX_PROMPT, &codex_spawn(&case, 120_000))
            .await;
        let launch = case.at(23).await;
        cancel(&daemon, &session, 1, 60_000).await;
        case.signal(launch);
        let shown = until_acknowledged(&daemon, &session).await;
        let requested_at = shown["active_turn"]["cancel"]["requested_at"].clone();
        assert!(requested_at.is_string(), "{shown}");
        assert_eq!(shown["active_turn"]["state"], "running", "{shown}");
        assert_eq!(
            shown["active_turn"]["cancel"],
            json!({"outcome":"acknowledged","cleanup":"pending",
                   "requested_at":requested_at,"settled_at":null}),
            "{shown}"
        );
        daemon.resume(&session, SUCCESSOR).await;
        for _ in 0..2 {
            let shown = status(&daemon, &session).await;
            assert_eq!(second_state(&shown), "queued", "{shown}");
            assert_eq!(
                shown["active_turn"]["cancel"]["cleanup"], "pending",
                "{shown}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert_eq!(case.at(29).await, launch);
        let signalled = tokio::time::Instant::now();
        case.signal(launch);
        let first = daemon.wait(&session, 1).await;
        assert!(
            signalled.elapsed() < Duration::from_secs(30),
            "settled by the completion, not the grace"
        );
        assert_eq!(first["state"], "cancelled", "{first}");
        assert_eq!(
            stop_pair(&first),
            (&json!("acknowledged"), &json!("quiescent")),
            "{first}"
        );
        let second = daemon.wait(&session, 2).await;
        assert_eq!(second["state"], "completed", "{second}");
        dispatched_after(&first, &second);
        daemon.close(&session).await;
        daemon.shutdown().await;
    });
}

/// F16c (d6 #2): the tool never ends, so the window ends at a short wall
/// `W`, proven before the grace's end: the order is published before the
/// wall (`t_o < t_s + W`), the terminal retained before it (`t_ack < t_s +
/// W`), and `t_c + W < t_s + 60 s`. The turn settles `cancelled`,
/// acknowledged, `uncertain` with `cancel_cleanup_uncertain`; the
/// successor is submitted only after that settlement and carries
/// `predecessor_cleanup_uncertain` (C1 §7.3).
#[cfg(feature = "test-failpoints")]
#[test]
fn core_codex_p7_uncertain_successor() {
    const NAME: &str = "core_codex_p7_uncertain_successor";
    const W: Duration = Duration::from_millis(8_000);
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    arm(&root, SUBMIT, "pause");
    acknowledge(&root, ORDERED, 1);
    let case = codex_case(&root, NAME, p7_copy(false, true));
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        let started = tokio::time::Instant::now();
        let wall_ms = u64::try_from(W.as_millis()).unwrap();
        let session = daemon
            .spawn(CODEX_PROMPT, &codex_spawn(&case, wall_ms))
            .await;
        until_acked(&root, SUBMIT, 1).await;
        let committed = tokio::time::Instant::now();
        release_point(&root, SUBMIT, 1);
        assert!(
            committed + W < started + Duration::from_secs(60),
            "the wall comes before the grace's end"
        );
        let launch = case.at(23).await;
        let ((), ()) = tokio::join!(cancel(&daemon, &session, 1, 60_000), async {
            until_acked(&root, ORDERED, 1).await;
            assert!(
                tokio::time::Instant::now() < started + W,
                "the order was published before the wall"
            );
        });
        case.signal(launch);
        until_acknowledged(&daemon, &session).await;
        assert!(
            tokio::time::Instant::now() < started + W,
            "the terminal was retained before the wall"
        );
        daemon.resume(&session, SUCCESSOR).await;
        let first = daemon.wait(&session, 1).await;
        assert_eq!(first["state"], "cancelled", "{first}");
        assert_eq!(
            stop_pair(&first),
            (&json!("acknowledged"), &json!("uncertain")),
            "{first}"
        );
        assert!(
            warning_codes(&first).contains(&"cancel_cleanup_uncertain"),
            "{first}"
        );
        let second = daemon.wait(&session, 2).await;
        assert_eq!(second["state"], "completed", "{second}");
        dispatched_after(&first, &second);
        assert!(
            warning_codes(&second).contains(&"predecessor_cleanup_uncertain"),
            "{second}"
        );
        daemon.close(&session).await;
        daemon.shutdown().await;
    });
}

/// x.3.2 X4 K5 (`codex_control_races`, F15 without steer: close versus
/// the P7 window; W3 through Engine): a cancel's interrupted terminal is
/// retained with the `sleep 75` tool open under the real 60 s grace;
/// the session's `close` then detaches the draining turn at once. The turn
/// settles `cancelled`, acknowledged, `uncertain`, and the close returns,
/// both long before the grace would end the window.
///
/// Ignored (x.3.2 X4 K5 finding): Core coalesces the close's stop order
/// into the cancel's, keeping the cancel's cause (design §2), so the
/// Codex driver cannot tell the close from the cancel while it drains and
/// the turn waits out the 60 s grace. W3 covers only a close-caused order.
/// Accepted until measured (via-5lr.7): the wait is bounded by the grace,
/// and close already waits for a running turn.
#[test]
#[ignore = "via-5lr.7: a close coalesced into a cancel's order waits out the P7 grace (accepted until measured)"]
fn codex_control_races_close_vs_p7_window() {
    const NAME: &str = "codex_control_races_close_vs_p7_window";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    let case = codex_case(&root, NAME, p7_copy(false, false));
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        let session = daemon
            .spawn(CODEX_PROMPT, &codex_spawn(&case, 120_000))
            .await;
        let launch = case.at(23).await;
        cancel(&daemon, &session, 1, 60_000).await;
        case.signal(launch);
        let shown = until_acknowledged(&daemon, &session).await;
        assert_eq!(
            shown["active_turn"]["cancel"]["cleanup"], "pending",
            "{shown}"
        );
        let closing = tokio::time::Instant::now();
        daemon.close(&session).await;
        let first = daemon.wait(&session, 1).await;
        assert!(
            closing.elapsed() < Duration::from_secs(20),
            "the close detached the window, not the grace"
        );
        assert_eq!(first["state"], "cancelled", "{first}");
        assert_eq!(
            stop_pair(&first),
            (&json!("acknowledged"), &json!("uncertain")),
            "{first}"
        );
        daemon.shutdown().await;
    });
}

/// The Codex fixtures' first turn's prompt in `c1_commentary_usage`.
const C1_PROMPT: &str =
    "Create a file note.txt in this directory with the text OK. Report the result.";

/// Turn 1's gate in `c1_commentary_usage` (one-based): the fixture
/// holds the turn's last three messages behind it.
const C1_GATE: usize = 26;

/// `c1_commentary_usage`'s copy for `codex_bounds_overflow` (steps
/// one-based): turn 1 whole, with its gate ([`C1_GATE`]); turn 2 through
/// its `turn/started` (step 33), then gate `held`, the user message's
/// `item/started` (`X`), gate `burst`, twenty copies of `burst_line`,
/// gate `after`, and the generation's cleanup interrupt of turn 2, due
/// within 2 s; then the server's stdin close.
#[cfg(feature = "test-failpoints")]
struct OverflowCopy {
    replay: Value,
    held: usize,
    burst: usize,
    after: usize,
}

#[cfg(feature = "test-failpoints")]
fn overflow_copy(burst_line: &str) -> OverflowCopy {
    let mut replay = core_codex::replay("c1_commentary_usage");
    let original = replay["steps"].as_array().unwrap().clone();
    assert!(
        original[C1_GATE - 1].get("await_signal").is_some(),
        "step {C1_GATE} is turn 1's gate"
    );
    let mut steps = original[..33].to_vec();
    let line = original[32]["emit"]["line"].as_str().unwrap();
    assert!(
        line.contains("\"turn/started\""),
        "step 33 is turn 2's start: {line}"
    );
    let gate = |steps: &mut Vec<Value>| {
        steps.push(json!({"await_signal":{"signal":"SIGUSR1"}}));
        steps.len()
    };
    let held = gate(&mut steps);
    steps.push(original[33].clone());
    let burst = gate(&mut steps);
    for _ in 0..20 {
        steps.push(json!({"emit":{"line":burst_line}}));
    }
    let after = gate(&mut steps);
    steps.push(
        json!({"expect":{"line":{"method":"turn/interrupt","params":{
        "threadId":"019a0000-0000-7000-8000-000000100001",
        "turnId":"019a0000-0000-7000-8000-000000200002"}},"within_ms":2000}}),
    );
    steps.push(json!({"await_eof":{}}));
    replay["steps"] = Value::Array(steps);
    replay["deadline_ms"] = json!(60_000);
    OverflowCopy {
        replay,
        held,
        burst,
        after,
    }
}

/// The occurrences of `point`'s hits counted so far (each refusal's
/// marker), ascending.
#[cfg(feature = "test-failpoints")]
fn counted(root: &Path, point: &str) -> Vec<u64> {
    let prefix = format!("{point}.");
    let mut counted: Vec<u64> = fs::read_dir(root.join("points"))
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            name.strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(".refused"))
                .and_then(|number| number.parse::<u64>().ok())
        })
        .collect();
    counted.sort_unstable();
    counted
}

/// Waits until `n` hits of `point` were counted; returns the last one's
/// occurrence.
#[cfg(feature = "test-failpoints")]
async fn until_counted(root: &Path, point: &str, n: usize) -> u64 {
    let by = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let counted = counted(root, point);
        if counted.len() >= n {
            assert_eq!(counted.len(), n, "{point} counted {counted:?}");
            return counted[n - 1];
        }
        assert!(
            tokio::time::Instant::now() < by,
            "{point} counted {counted:?} of {n}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The consumer's takes of turn 1's messages before its gate
/// ([`C1_GATE`]): its status line, `turn/started` and the eleven item and
/// usage lines (`account/updated` names no thread; the `Start` and
/// `Reply` markers are no messages, so no take).
#[cfg(feature = "test-failpoints")]
const TAKES_AT_C1_GATE: usize = 13;

/// The consumer's takes before `X`'s in [`overflow_copy`]: turn 1's
/// thirteen, its three after the gate, then turn 2's status line and
/// `turn/started`.
#[cfg(feature = "test-failpoints")]
const TAKES_BEFORE_X: usize = 18;

/// The messages the connection routes in turn 2 of [`overflow_copy`]
/// through its burst: the `turn/start` reply, the status line,
/// `turn/started`, `X` and the twenty burst lines.
#[cfg(feature = "test-failpoints")]
const ROUTED_THROUGH_BURST: usize = 24;

/// `codex_bounds_overflow` (x.3.2 X5, X0 item 10 as the owner simplified
/// it): turn 1 completes; turn 2 starts; its consumer is paused at its
/// take of the user message's `item/started` while twenty `burst_line`s
/// arrive, so the thread's lane drops the seventeenth and turn 2 fails
/// `overflow`. Turn 2's envelope carries exactly one `observations_lost`
/// warning, of generation 1, with an unknown count (`omitted: null`) and
/// `first_unqueued` 1: the record merges by the earliest position (X0
/// item 10), and the generation's quarantine notes its registration's
/// seal, whose idle delivery took no message, so position 1 (the lane's
/// own note is the first dropped line's decode sequence, 44, and the
/// turn's settlement notes 27). Turn 1's envelope, the unaffected turn's,
/// carries none and is unchanged. Returns the session and the warning's
/// data.
///
/// Turn 1's gate is released only once its consumer took every message
/// before it: turn 1 puts eighteen items in the sixteen-message lane (its
/// `Start` and `Reply` markers and sixteen messages), so a consumer that
/// had taken none of its messages by the time the last three arrived
/// overflowed the lane in turn 1 (the 0.1 s flake, reproduced under CPU
/// load: the lane held the `Reply` and fifteen messages and dropped
/// `turn/completed`). Every wait is an exact count, not a quiet period.
#[cfg(feature = "test-failpoints")]
async fn lane_overflow(root: &Path, name: &str, burst_line: &str) -> (SessionId, Value) {
    let copy = overflow_copy(burst_line);
    let case = codex_case(root, name, copy.replay);
    let points = root.join("points");
    count_hits(&points, TAKE);
    let daemon = Daemon::open_with(root, case.config());
    let session = daemon.spawn(C1_PROMPT, &codex_spawn(&case, 60_000)).await;
    let launch = case.at(C1_GATE).await;
    until_counted(root, TAKE, TAKES_AT_C1_GATE).await;
    case.signal(launch);
    let first = daemon.wait(&session, 1).await;
    assert_eq!(first["state"], "completed", "{first}");
    assert!(
        !warning_codes(&first).contains(&"observations_lost"),
        "{first}"
    );
    count_hits(&points, ROUTED);
    daemon.resume(&session, SUCCESSOR).await;
    assert_eq!(case.at(copy.held).await, launch);
    // Turn 2's takes before the gate were made: the next take is `X`'s.
    let take = until_counted(root, TAKE, TAKES_BEFORE_X).await + 1;
    arm_at(root, TAKE, take, "pause");
    case.signal(launch);
    until_acked(root, TAKE, take).await;
    assert_eq!(case.at(copy.burst).await, launch);
    case.signal(launch);
    assert_eq!(case.at(copy.after).await, launch);
    // The connection reached the last burst line, so it routed the
    // nineteen before it, the dropped one among them, before the
    // consumer goes on.
    until_counted(root, ROUTED, ROUTED_THROUGH_BURST).await;
    release_point(root, TAKE, take);
    case.signal(launch);
    let second = daemon.wait(&session, 2).await;
    assert_eq!(second["state"], "failed", "{second}");
    assert_eq!(class(&second), "overflow", "{second}");
    let lost: Vec<&Value> = second["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|warning| warning["code"] == "observations_lost")
        .collect();
    assert_eq!(lost.len(), 1, "{second}");
    let data = lost[0]["data"].clone();
    assert_eq!(data["generation"], 1, "{second}");
    assert_eq!(data["first_unqueued"], 1, "{second}");
    assert_eq!(data["omitted"], Value::Null, "{second}");
    assert_eq!(
        data.as_object().map(serde_json::Map::len),
        Some(4),
        "{second}"
    );
    assert!(lost[0]["message"].is_string(), "{second}");
    // The first turn's committed envelope is unchanged.
    assert_eq!(daemon.wait(&session, 1).await, first);
    daemon.close(&session).await;
    daemon.shutdown().await;
    (session, data)
}

/// The lost lines are turn 2's own thread traffic (thread status lines,
/// which name no turn): the warning names turn 2.
#[cfg(feature = "test-failpoints")]
#[test]
fn codex_bounds_overflow_warns_the_affected_turn() {
    const NAME: &str = "codex_bounds_overflow_warns_the_affected_turn";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    let status = json!({"method": "thread/status/changed",
        "params": {"threadId": "019a0000-0000-7000-8000-000000100001",
                   "status": {"type": "active", "activeFlags": []}}});
    let (session, data) = run(lane_overflow(&root, NAME, &status.to_string()));
    assert_eq!(data["trigger_turn"], format!("{session}/2"), "{data}");
}

/// Critical review x5 (successor): the lost lines are turn 1's late
/// messages (its `thread/tokenUsage/updated`, naming turn 1's vendor
/// turn), arriving while its successor runs. Turn 2 keeps the warning,
/// whose `trigger_turn` names turn 1: the turn the first dropped line was
/// mapped to.
#[cfg(feature = "test-failpoints")]
#[test]
fn codex_bounds_overflow_names_the_predecessor_whose_lines_were_lost() {
    const NAME: &str = "codex_bounds_overflow_names_the_predecessor_whose_lines_were_lost";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    let original = core_codex::replay("c1_commentary_usage")["steps"][26].clone();
    let late = original["emit"]["line"].as_str().unwrap().to_owned();
    assert!(
        late.contains("\"thread/tokenUsage/updated\"")
            && late.contains("019a0000-0000-7000-8000-000000200001"),
        "step 27 is turn 1's usage: {late}"
    );
    let (session, data) = run(lane_overflow(&root, NAME, &late));
    assert_eq!(data["trigger_turn"], format!("{session}/1"), "{data}");
}

/// x.3.2 X5 (via-5lr.6): Codex's admission cap on the JSON-encoded prompt
/// plus the JSON-encoded cwd (C1 §4 `prompt`), so the vendor's
/// `userMessage` echo always fits Wire's 1 MiB message.
const CODEX_PROMPT_MAX: usize = 1_040_384;

/// A prompt whose JSON string encoding, quotes included, is `bytes` long,
/// with an escaped newline, quote and control character in front.
fn encoded_prompt(bytes: usize) -> String {
    let head = "x\n\"\u{1}";
    let mut prompt = head.to_owned();
    prompt.push_str(&"a".repeat(bytes - serde_json::to_string(head).unwrap().len()));
    assert_eq!(serde_json::to_string(&prompt).unwrap().len(), bytes);
    prompt
}

/// [`echo_copy`]'s gate (one-based): [`C1_GATE`], one usage line before
/// it dropped.
const ECHO_GATE: usize = C1_GATE - 1;

/// `c1_commentary_usage`'s first turn with prompt `prompt`, without its
/// two `thread/tokenUsage/updated` lines (one-based steps 22 and 27): its
/// `turn/start` expects it and its two `userMessage` echoes carry it; the
/// close's unsubscribe and the stdin close follow the turn's end. Without
/// them the turn puts sixteen items in its thread's sixteen-message lane
/// (its `Start` and `Reply` markers and fourteen messages), so it never
/// passes the lane's message bound however far its consumer lags; with
/// them, eighteen did under CPU load. (A maximal prompt's two echoes
/// together pass the lane's 1 MiB: they fit only while the consumer takes
/// the first before the second is routed.)
fn echo_copy(prompt: &str) -> Value {
    let mut replay = core_codex::replay("c1_commentary_usage");
    let original = replay["steps"].as_array().unwrap().clone();
    for usage in [22, 27] {
        let line = original[usage - 1]["emit"]["line"].as_str().unwrap();
        assert!(
            line.contains("\"thread/tokenUsage/updated\""),
            "step {usage} is turn 1's usage: {line}"
        );
    }
    let mut steps: Vec<Value> = original[..29]
        .iter()
        .enumerate()
        .filter(|(index, _)| ![21, 26].contains(index))
        .map(|(_, step)| step.clone())
        .collect();
    assert!(steps[ECHO_GATE - 1].get("await_signal").is_some());
    steps.extend_from_slice(&original[49..]);
    assert_eq!(steps[9]["expect"]["line"]["method"], "turn/start");
    steps[9]["expect"]["line"]["params"]["input"][0]["text"] = json!(prompt);
    for at in [14, 15] {
        let mut line: Value =
            serde_json::from_str(steps[at]["emit"]["line"].as_str().unwrap()).unwrap();
        assert_eq!(line["params"]["item"]["type"], "userMessage", "{line}");
        line["params"]["item"]["content"][0]["text"] = json!(prompt);
        steps[at]["emit"]["line"] = json!(line.to_string());
    }
    assert_eq!(
        steps[27]["expect"]["line"]["method"], "thread/unsubscribe",
        "{}",
        steps[27]
    );
    replay["steps"] = Value::Array(steps);
    replay
}

/// Spawn members for a Codex case on `case`'s cwd with `prompt` given as
/// `member` (`prompt` or `prompt_file`).
fn codex_raw(case: &core_codex::CodexCase, member: &str, prompt: &str) -> Value {
    let mut raw = codex_spawn(case, 60_000);
    raw["prompt"] = Value::Null;
    raw.as_object_mut().unwrap().remove("prompt");
    raw[member] = json!(prompt);
    raw["handle"] = json!(HANDLE);
    raw
}

/// The Store's session rows.
fn session_rows(root: &Path) -> i64 {
    let store = rusqlite::Connection::open_with_flags(
        root.join("state").join("store.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    store
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap()
}

/// via-5lr.6 (x.3.2 X5): C1 admits a 16 MiB prompt, but Codex echoes it in
/// one `item/started` line, and a line over Wire's 1 MiB fails the shared
/// connection, every session on it. `codex-app-server` refuses a prompt
/// whose JSON encoding plus the cwd's exceeds [`CODEX_PROMPT_MAX`]:
/// inline or as a `prompt_file`, one byte over is `invalid_params` naming
/// `prompt`, before any receipt or vendor I/O. A prompt that just fits is
/// admitted and completes, its two echoes read whole.
#[test]
fn codex_prompt_echo_cap() {
    const NAME: &str = "codex_prompt_echo_cap";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    // The cap counts the case's cwd, a fixed-length temporary name: a
    // probe case gives its encoded length before the replay is written.
    fs::create_dir_all(root.join("probe").join("state")).unwrap();
    let probe = codex_case(
        &root.join("probe"),
        NAME,
        core_codex::replay("c1_commentary_usage"),
    );
    let cwd = serde_json::to_string(probe.cwd()).unwrap().len();
    drop(probe);
    let fitting = encoded_prompt(CODEX_PROMPT_MAX - cwd);
    let over = encoded_prompt(CODEX_PROMPT_MAX - cwd + 1);
    let file = root.join("over.prompt");
    fs::write(&file, &over).unwrap();
    let case = codex_case(&root, NAME, echo_copy(&fitting));
    assert_eq!(serde_json::to_string(case.cwd()).unwrap().len(), cwd);
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        for (member, prompt) in [
            ("prompt", over.as_str()),
            ("prompt_file", file.to_str().unwrap()),
        ] {
            let raw = codex_raw(&case, member, prompt);
            let params: SpawnParams = serde_json::from_value(raw.clone()).unwrap();
            let refused = daemon.engine.spawn(params, &raw.to_string()).await;
            let error = refused
                .err()
                .unwrap_or_else(|| panic!("{member}: admitted"));
            let data = error.data();
            assert_eq!(data["kind"], "invalid_params", "{member}: {data}");
            assert_eq!(data["field"], "prompt", "{member}: {data}");
            assert_eq!(data["route"], "codex-app-server", "{member}: {data}");
        }
        assert_eq!(case.launches(), 0, "a refused prompt reached no vendor");
        assert_eq!(session_rows(&root), 0, "a refused prompt has no receipt");
        let raw = codex_raw(&case, "prompt", &fitting);
        let params: SpawnParams = serde_json::from_value(raw.clone()).unwrap();
        let session = daemon
            .engine
            .spawn(params, &raw.to_string())
            .await
            .unwrap()
            .enqueued
            .unwrap()
            .0;
        let launch = case.at(ECHO_GATE).await;
        case.signal(launch);
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        daemon.close(&session).await;
        daemon.shutdown().await;
    });
}

/// A Codex session on [`echo_copy`]'s replay of `C1_PROMPT`, served by
/// the fake's launch `launch`: its turn completes, then the session
/// closes.
async fn c1_turn(daemon: &Daemon, case: &core_codex::CodexCase, launch: u64) {
    let session = daemon.spawn(C1_PROMPT, &codex_spawn(case, 60_000)).await;
    case.at_launch(ECHO_GATE, launch).await;
    case.signal(launch);
    let envelope = daemon.wait(&session, 1).await;
    assert_eq!(envelope["state"], "completed", "{envelope}");
    daemon.close(&session).await;
}

/// x.3.2 X5 (X0 item 4): Codex's `CODEX_SQLITE_HOME`, `<state>/vendor/codex`,
/// persists across a daemon restart. The first daemon's server creates
/// it (0700); a file the vendor would keep there is written; after a
/// clean stop the second daemon finds the same directory, its file
/// unchanged, and its own server launches over it, leaving both as they
/// were.
#[test]
fn codex_sqlite_home_persists_across_restart() {
    use std::os::unix::fs::MetadataExt as _;
    const NAME: &str = "codex_sqlite_home_persists_across_restart";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    let case = codex_case(&root, NAME, echo_copy(C1_PROMPT));
    let home = root.join("state").join("vendor").join("codex");
    let kept = home.join("state_5.sqlite");
    let identity = run(async {
        let daemon = Daemon::open_with(&root, case.config());
        c1_turn(&daemon, &case, 1).await;
        let metadata = fs::symlink_metadata(&home).unwrap();
        assert!(metadata.is_dir(), "{}", home.display());
        assert_eq!(metadata.mode() & 0o777, 0o700);
        // via-25f: the first successful handshake marks the home warm.
        let marker = fs::symlink_metadata(home.join(".via-initialized")).unwrap();
        assert!(marker.is_file());
        assert_eq!(marker.mode() & 0o777, 0o600);
        fs::write(&kept, b"vendor state").unwrap();
        daemon.stop().await;
        (metadata.dev(), metadata.ino())
    });
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        let unchanged = || {
            let metadata = fs::symlink_metadata(&home).unwrap();
            assert!(metadata.is_dir(), "{}", home.display());
            assert_eq!((metadata.dev(), metadata.ino()), identity);
            assert_eq!(metadata.mode() & 0o777, 0o700);
            assert_eq!(fs::read(&kept).unwrap(), b"vendor state");
        };
        unchanged();
        c1_turn(&daemon, &case, 2).await;
        assert_eq!(case.launches(), 2, "each daemon launched its own server");
        unchanged();
        daemon.shutdown().await;
    });
}

/// Bead via-20s (live 2026-10-06, Codex 0.160.0): a Codex server that
/// dies during its handshake, before any of the waiting turn was sent,
/// fails that turn definitively: `server_lost` on Host's confirmed exit,
/// never `unknown`. So the turn queued behind it is not cancelled (C1 P6)
/// and runs on the next server.
#[test]
fn codex_handshake_death_fails_and_keeps_the_queue() {
    const NAME: &str = "codex_handshake_death_fails_and_keeps_the_queue";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    let full = echo_copy(C1_PROMPT);
    let initialize = full["steps"][0].clone();
    assert_eq!(initialize["expect"]["line"]["method"], "initialize");
    let mut dying = full.clone();
    // As live: the second server's backfill wait timed out and it exited.
    dying["steps"] = json!([
        initialize,
        {"await_signal": {"signal": "SIGUSR1"}},
        {"exit": {"code": 1, "stderr": "Error: failed to initialize sqlite state runtime\n"}},
    ]);
    let replay = json!({
        "source": format!("{NAME}: c1_commentary_usage, its first server dying at its handshake"),
        "lifetimes": [dying, full],
    });
    let case = codex_case(&root, NAME, replay);
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        let session = daemon.spawn(C1_PROMPT, &codex_spawn(&case, 60_000)).await;
        assert_eq!(case.at(2).await, 1, "the first server holds its handshake");
        daemon.resume(&session, C1_PROMPT).await;
        case.signal(1);
        let failed = daemon.wait(&session, 1).await;
        assert_eq!(failed["state"], "failed", "{failed}");
        assert_eq!(class(&failed), "server_lost", "{failed}");
        assert_eq!(failed["timestamps"]["accepted_at"], Value::Null, "{failed}");
        case.at_launch(ECHO_GATE, 2).await;
        case.signal(2);
        let next = daemon.wait(&session, 2).await;
        assert_eq!(next["state"], "completed", "{next}");
        daemon.close(&session).await;
        daemon.shutdown().await;
    });
}

/// Bead via-20s review #1: once any byte of a turn reached Wire, its
/// later request proven unwritten does not erase that. The first turn's
/// `thread/start` (with its settings) is answered; its `turn/start` is
/// queued and held before its hand-off to Wire (`codex.feeder.queued`
/// paused at the second data input); then the connection task fails
/// (`codex.connection.message` at the next vendor line), the server
/// unconfirmed, so the queued `turn/start` answers `NotWritten`. The turn
/// ends `unknown`, never `failed(submit_failed)`: the vendor already had
/// the turn's thread request.
#[cfg(feature = "test-failpoints")]
#[test]
fn codex_unwritten_turn_start_after_thread_start_is_unknown() {
    const NAME: &str = "codex_unwritten_turn_start_after_thread_start_is_unknown";
    const QUEUED: &str = "codex.feeder.queued";
    const MESSAGE: &str = "codex.connection.message";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    via_store::failpoint::activate(&root.join("points"), "conformance-core").unwrap();
    let full = echo_copy(C1_PROMPT);
    let steps = full["steps"].as_array().unwrap();
    assert_eq!(steps[6]["expect"]["line"]["method"], "thread/start");
    assert_eq!(steps[9]["expect"]["line"]["method"], "turn/start");
    assert!(
        steps[8]["emit"]["line"]
            .as_str()
            .unwrap()
            .contains("\"thread/started\""),
        "step 9 is the thread's start notification"
    );
    assert!(
        steps[4]["emit"]["line"]
            .as_str()
            .unwrap()
            .contains("\"remoteControl/status/changed\""),
        "step 5 is an unrelated notification"
    );
    let mut cut = full.clone();
    let mut held: Vec<Value> = steps[..9].to_vec();
    held.push(json!({"await_signal": {"signal": "SIGUSR1"}}));
    let gate = held.len();
    held.push(steps[4].clone());
    // Never met: the connection fails first, and the server is stopped.
    held.push(steps[9].clone());
    held.push(json!({"await_eof": {}}));
    let line = held
        .iter()
        .filter(|step| step.get("emit").is_some())
        .count();
    cut["steps"] = Value::Array(held);
    // The thread/start is data input 1, the turn/start 2.
    arm_at(&root, QUEUED, 2, "pause");
    arm_at(&root, MESSAGE, u64::try_from(line).unwrap(), "fail_io");
    let case = codex_case(&root, NAME, cut);
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        let session = daemon.spawn(C1_PROMPT, &codex_spawn(&case, 60_000)).await;
        until_acked(&root, QUEUED, 2).await;
        case.at_launch(gate, 1).await;
        case.signal(1);
        let lost = daemon.wait(&session, 1).await;
        assert_eq!(lost["state"], "unknown", "{lost}");
        assert_eq!(lost["failure"], Value::Null, "{lost}");
        release_point(&root, QUEUED, 2);
        daemon.close(&session).await;
        let report = daemon
            .engine
            .shutdown(Deadline::at(
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .await;
        daemon.starter.abort();
        // The one failed task is the connection task this case failed.
        assert_eq!(
            (report.failed_tasks, report.pending_tasks),
            (1, 0),
            "{report:?}"
        );
        assert_eq!(report.unresolved_turns, 0, "{report:?}");
    });
}

/// `codex_rss_leases` (x.3.2 X5, X0 item 9.2): the sessions leased on one
/// server, each with one active turn.
#[cfg(feature = "test-failpoints")]
const LEASES: usize = 32;

/// Maximal status lines of the growth flood: about 270 MiB, so the flood
/// passes 256 MiB.
#[cfg(feature = "test-failpoints")]
const FLOOD_LINES: usize = 272;

/// Maximal `final_answer` lines per session while Core's drain is held:
/// four fill its 4 MiB channel, the fifth is the normalizer's decode in
/// flight, blocked on the channel.
#[cfg(feature = "test-failpoints")]
const FILL_LINES: usize = 5;

/// The Codex route's message cap, LF included (via-5lr.3.5,
/// `via_routes::codex::MESSAGE_BYTES`).
#[cfg(feature = "test-failpoints")]
const CODEX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

/// The Codex server's staging: 4 MiB plus one maximal message
/// (`via_routes::codex::INBOUND`).
#[cfg(feature = "test-failpoints")]
const CODEX_STAGING_BYTES: u64 = 4 * 1024 * 1024 + CODEX_MESSAGE_BYTES;

/// Maximal lines left in the ingress lanes of blocked consumers: twelve
/// of about 1 MiB fill the server's 12 MiB staging.
#[cfg(feature = "test-failpoints")]
const STAGED_LINES: usize = 12;

/// Maximal lines between two of the fake's gates: the test releases the
/// next batch once every line before it was taken, so Wire's staging
/// holds at most one batch (3 MiB of its 12 MiB).
#[cfg(feature = "test-failpoints")]
const BATCH: usize = 3;

/// Core's hold before it handles an observation: each session's drain is
/// paused at its first fill observation, acknowledged, until the test
/// releases it after the measurement.
#[cfg(feature = "test-failpoints")]
const CORE_HOLD: &str = "core.observations.pause";

/// The connection task's hit before it routes each message, counted to
/// prove the staged lines reached their lanes.
#[cfg(feature = "test-failpoints")]
const ROUTED: &str = "codex.connection.message";

/// The consumer's take of a lane item, counted to pace the fake.
#[cfg(feature = "test-failpoints")]
const TAKE: &str = "adapter.codex.consumer_take";

/// A blocked channel send, counted: one per session once its channel is
/// full and its fifth fill line decoded.
#[cfg(feature = "test-failpoints")]
const BLOCKED: &str = "adapter.observation.blocked";

/// Session `k`'s thread and turn IDs; session 0's are the fixture's.
#[cfg(feature = "test-failpoints")]
fn lease_ids(k: usize) -> (String, String) {
    (
        format!("019a0000-0000-7000-8000-{:012}", 100_001 + k),
        format!("019a0000-0000-7000-8000-{:012}", 200_001 + k),
    )
}

/// `codex_rss_leases`' replay and the one-based steps of its gates.
#[cfg(feature = "test-failpoints")]
struct Leases {
    replay: Value,
    /// The gate after each session's start and prompt echo.
    started: Vec<usize>,
    /// The flood's gates, with the flood lines written before each.
    flood: Vec<(usize, usize)>,
    /// The gate after each session's first fill line, where Core's drain
    /// of that session is held.
    held: Vec<usize>,
    /// The rest of the fill's gates, with the fill lines written before
    /// each.
    fill: Vec<(usize, usize)>,
    /// The gate after the staged lines, where the holders are measured.
    measured: usize,
    /// The gate after every session's `turn/completed`, written together.
    completed: usize,
}

/// `c1_commentary_usage`'s handshake, then [`LEASES`] sessions, each
/// started (`thread/start`, `turn/start`) and its prompt captured as `p`
/// and echoed in its `userMessage` (about 1 MiB at the echo cap); then the
/// flood, the fill and the staged lines, every maximal line's text `${p}`;
/// then each turn completes and each session closes, in order.
#[cfg(feature = "test-failpoints")]
fn leases_copy() -> Leases {
    let mut replay = core_codex::replay("c1_commentary_usage");
    let original = replay["steps"].as_array().unwrap().clone();
    let (thread, turn) = lease_ids(0);
    let line_of = |at: usize| original[at]["emit"]["line"].as_str().unwrap().to_owned();
    let for_lease = |line: &str, k: usize| {
        let (t, u) = lease_ids(k);
        line.replace(&thread, &t).replace(&turn, &u)
    };
    assert_eq!(original[6]["expect"]["line"]["method"], "thread/start");
    assert_eq!(original[9]["expect"]["line"]["method"], "turn/start");
    assert!(line_of(14).contains("\"userMessage\""));
    assert!(line_of(28).contains("\"turn/completed\""));
    assert_eq!(
        original[49]["expect"]["line"]["method"],
        "thread/unsubscribe"
    );
    let echo = line_of(14).replace(&serde_json::to_string(C1_PROMPT).unwrap(), "${p}");
    assert!(echo.contains("${p}"), "{echo}");
    let mut steps = original[..6].to_vec();
    let gate = |steps: &mut Vec<Value>| {
        steps.push(json!({"await_signal":{"signal":"SIGUSR1"}}));
        steps.len()
    };
    let emit = |steps: &mut Vec<Value>, line: String| {
        steps.push(json!({"emit":{"line":line}}));
    };
    let mut started = Vec::new();
    for k in 0..LEASES {
        let (t, _) = lease_ids(k);
        steps.push(original[6].clone());
        emit(&mut steps, for_lease(&line_of(7), k));
        let answered = steps.len();
        emit(&mut steps, for_lease(&line_of(8), k));
        steps.push(json!({"expect":{"line":{"method":"turn/start","params":{
            "threadId":t,"cwd":original[9]["expect"]["line"]["params"]["cwd"]}},
            "capture":{"turn1":"/id","p":"/params/input/0/text"},"after_emit":answered}}));
        emit(&mut steps, for_lease(&line_of(10), k));
        emit(&mut steps, for_lease(&line_of(12), k));
        emit(&mut steps, for_lease(&echo, k));
        started.push(gate(&mut steps));
    }
    let status = |k: usize| {
        let (t, _) = lease_ids(k);
        format!(
            r#"{{"method":"thread/status/changed","params":{{"threadId":"{t}","status":{{"type":"active","activeFlags":[]}},"pad":${{p}}}}}}"#
        )
    };
    let mut flood = Vec::new();
    for line in 0..FLOOD_LINES {
        emit(&mut steps, status(line % LEASES));
        if (line + 1) % BATCH == 0 || line + 1 == FLOOD_LINES {
            flood.push((gate(&mut steps), line + 1));
        }
    }
    let mut held = Vec::new();
    let mut fill = Vec::new();
    for line in 0..FILL_LINES * LEASES {
        let (t, u) = lease_ids(line % LEASES);
        let j = line / LEASES;
        emit(
            &mut steps,
            format!(
                r#"{{"method":"item/completed","params":{{"item":{{"type":"agentMessage","id":"fill_{j}","text":${{p}},"phase":"final_answer","memoryCitation":null,"delivery":null,"questions":null}},"threadId":"{t}","turnId":"{u}","completedAtMs":1790000007591}},"emittedAtMs":1790000007591}}"#
            ),
        );
        if line < LEASES {
            held.push(gate(&mut steps));
        } else if (line + 1 - LEASES).is_multiple_of(BATCH) || line + 1 == FILL_LINES * LEASES {
            fill.push((gate(&mut steps), line + 1));
        }
    }
    for k in 0..STAGED_LINES {
        emit(&mut steps, status(k));
    }
    let measured = gate(&mut steps);
    for k in 0..LEASES {
        emit(&mut steps, for_lease(&line_of(28), k));
    }
    let completed = gate(&mut steps);
    for k in 0..LEASES {
        let (t, _) = lease_ids(k);
        steps.push(json!({"expect":{"line":{"method":"thread/unsubscribe",
            "params":{"threadId":t}},"capture":{"close":"/id"}}}));
        steps.push(original[50].clone());
    }
    steps.push(json!({"await_eof":{}}));
    replay["steps"] = Value::Array(steps);
    replay["deadline_ms"] = json!(115_000);
    Leases {
        replay,
        started,
        flood,
        held,
        fill,
        measured,
        completed,
    }
}

/// Counts `point`'s hits without acting on any: a command under another
/// token is refused at every hit, each refusal leaving its marker.
#[cfg(feature = "test-failpoints")]
fn count_hits(points: &Path, point: &str) {
    let command = json!({"token":"counting","occurrence":1,"action":"pause"});
    fs::write(points.join(format!("{point}.json")), command.to_string()).unwrap();
}

/// Waits until the fake is at gate `step` and the consumers took at least
/// `taken` lane items; the caller releases the gate.
#[cfg(feature = "test-failpoints")]
async fn ready(case: &core_codex::CodexCase, root: &Path, step: usize, taken: u64) {
    case.at_launch(step, 1).await;
    let by = tokio::time::Instant::now() + Duration::from_secs(60);
    while hits(root, TAKE) < taken {
        assert!(
            tokio::time::Instant::now() < by,
            "the consumers took {} of {taken} items by gate {step}",
            hits(root, TAKE)
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// A `/proc/<pid>/status` field in KiB.
#[cfg(feature = "test-failpoints")]
fn status_kib(pid: &str, field: &str) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

/// The bytes process `pid` wrote (`/proc/<pid>/io` `wchar`).
#[cfg(feature = "test-failpoints")]
fn written(pid: u32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/io"))
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("wchar:"))
        .and_then(|rest| rest.trim().parse().ok())
        .unwrap_or(0)
}

/// The phases of `codex_rss_leases`, as the sampler records them.
#[cfg(feature = "test-failpoints")]
mod phase {
    pub(super) const SPAWN: u8 = 1;
    pub(super) const FLOOD: u8 = 2;
    pub(super) const HELD: u8 = 3;
    pub(super) const DRAIN: u8 = 4;
}

/// One 10 ms sample: the phase, this process's RSS and the fake's written
/// bytes.
#[cfg(feature = "test-failpoints")]
#[derive(Clone, Copy)]
struct RssSample {
    phase: u8,
    rss_kib: u64,
    written: u64,
}

/// Samples this process's RSS every 10 ms, with the phase and the fake's
/// written bytes (once its pid is known), until `stop`.
#[cfg(feature = "test-failpoints")]
fn rss_sampler(
    phase: Arc<std::sync::atomic::AtomicU8>,
    fake: Arc<std::sync::atomic::AtomicU32>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<Vec<RssSample>> {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let mut samples = Vec::new();
        while !stop.load(Ordering::Acquire) {
            let pid = fake.load(Ordering::Acquire);
            samples.push(RssSample {
                phase: phase.load(Ordering::Acquire),
                rss_kib: status_kib("self", "VmRSS:"),
                written: if pid == 0 { 0 } else { written(pid) },
            });
            std::thread::sleep(Duration::from_millis(10));
        }
        samples
    })
}

/// X0 item 9.2's computed sum, in bytes, from the table's constants: per
/// server, staging 12 MiB, correlation 256 KiB, pending replies 64 KiB,
/// Wire's read buffer 64 KiB and the demux peek of one 8 MiB message; per
/// session, the observation channel 4 MiB, driver controls 64 KiB and the
/// decode allowance (two 8 MiB messages, an escaped string's scratch and
/// its owned copy, + 65,536 nodes × 64 B; review cfix-1 #2); per active
/// turn, the dispatched prompt, which on this route is at most the echo
/// cap (via-5lr.6), not C1's 16 MiB.
#[cfg(feature = "test-failpoints")]
fn leases_sum() -> u64 {
    const MIB: u64 = 1024 * 1024;
    let server = CODEX_STAGING_BYTES + 256 * 1024 + 64 * 1024 + 64 * 1024 + CODEX_MESSAGE_BYTES;
    let session = 4 * MIB + 64 * 1024 + 2 * CODEX_MESSAGE_BYTES + 65_536 * 64;
    let turn = CODEX_PROMPT_MAX as u64;
    server + LEASES as u64 * (session + turn)
}

/// x.3.2 X5 `codex_rss_leases` (X0 item 9.2, runtime §8 F24): one replay
/// server, [`LEASES`] leased sessions each with an active turn whose
/// prompt is at the echo cap. A paced flood of about 270 MiB of maximal
/// thread lines passes through every lane while Core drains (growth
/// below 32 MiB after its first 64 MiB, peak to peak, both windows
/// sampled). Then each session's Core drain is paused, acknowledged, until
/// the test releases it; each channel is filled to its 4 MiB with the
/// fifth maximal message decoded and blocked, and the server's 12 MiB
/// staging filled with lines in blocked lanes. That occupancy is asserted
/// from counted failpoint hits before and after the held sample; then
/// peak RSS less the idle baseline is within [`leases_sum`] plus 25%.
/// Then every drain is released together, every turn completes together,
/// and every session closes. 10 ms sampling; on glibc
/// `MALLOC_ARENA_MAX=2` as F24's proxy. Held to its own nextest slot
/// (`.config/nextest.toml`).
#[cfg(feature = "test-failpoints")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "every phase of one measured scenario, in order"
)]
#[expect(clippy::print_stdout, reason = "the measured numbers are reported")]
fn codex_rss_leases() {
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
    const NAME: &str = "codex_rss_leases";
    const MIB: u64 = 1024 * 1024;
    let mut env = vec![("VIA_TEST_EVENT_STALL_MS", "120000")];
    if cfg!(target_env = "gnu") {
        env.push(("MALLOC_ARENA_MAX", "2"));
    }
    let Some(root) = child(NAME, &no_fake(), &env) else {
        return;
    };
    let started_at = Instant::now();
    let points = root.join("points");
    via_store::failpoint::activate(&points, "conformance-core").unwrap();
    count_hits(&points, TAKE);
    count_hits(&points, BLOCKED);
    count_hits(&points, ROUTED);
    count_hits(&points, CORE_HOLD);
    let leases = leases_copy();
    let case = codex_case(&root, NAME, leases.replay.clone());
    let cwd = serde_json::to_string(case.cwd()).unwrap().len();
    let prompt = encoded_prompt(CODEX_PROMPT_MAX - cwd);
    // A maximal status line's length, LF included, as the fake writes it.
    let maximal = format!(
        r#"{{"method":"thread/status/changed","params":{{"threadId":"{}","status":{{"type":"active","activeFlags":[]}},"pad":{}}}}}"#,
        lease_ids(0).0,
        serde_json::to_string(&prompt).unwrap()
    )
    .len() as u64
        + 1;
    assert!(maximal <= MIB, "{maximal}");
    assert!(
        maximal * STAGED_LINES as u64 <= CODEX_STAGING_BYTES,
        "{maximal}"
    );
    let phase = Arc::new(AtomicU8::new(0));
    let fake = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (baseline, spawned, flood, sampling, (held_for, blocked)) = run(async {
        let daemon = Daemon::open_with(&root, case.config());
        // Elapsed time only: the Engine settles before its baseline.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let baseline = status_kib("self", "VmRSS:");
        let sampling = rss_sampler(Arc::clone(&phase), Arc::clone(&fake), Arc::clone(&stop));
        phase.store(phase::SPAWN, Ordering::Release);
        let mut sessions = Vec::new();
        for (k, step) in leases.started.iter().enumerate() {
            sessions.push(daemon.spawn(&prompt, &codex_spawn(&case, 115_000)).await);
            case.at_launch(*step, 1).await;
            if k == 0 {
                fake.store(case.pid(1), Ordering::Release);
            }
            if k + 1 < LEASES {
                case.signal(1);
            }
        }
        // Every start's traffic is taken before the flood is counted.
        let mut base = hits(&root, TAKE);
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let now = hits(&root, TAKE);
            if now == base {
                break;
            }
            base = now;
        }
        let spawned = status_kib("self", "VmRSS:");
        let from = written(case.pid(1));
        phase.store(phase::FLOOD, Ordering::Release);
        case.signal(1);
        for (step, lines) in &leases.flood {
            ready(&case, &root, *step, base + *lines as u64).await;
            if *lines < FLOOD_LINES {
                case.signal(1);
            }
        }
        let flood = (from, written(case.pid(1)));
        // Core's drain of each session is paused at its first fill
        // observation, one session at a time: the next occurrence of
        // Core's hold is armed, the session's first fill line released,
        // and the pause acknowledged. No other observation reaches Core.
        phase.store(phase::HELD, Ordering::Release);
        let base = hits(&root, TAKE);
        let blocked_from = hits(&root, BLOCKED);
        let core_from = hits(&root, CORE_HOLD);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(hits(&root, CORE_HOLD), core_from, "Core is not idle");
        let paused: Vec<u64> = (1..=LEASES as u64).map(|k| core_from + k).collect();
        for (occurrence, step) in paused.iter().zip(&leases.held) {
            arm_at(&root, CORE_HOLD, *occurrence, "pause");
            case.signal(1);
            until_acked(&root, CORE_HOLD, *occurrence).await;
            case.at_launch(*step, 1).await;
        }
        // Counted again: a later hit of Core's hold would be an
        // observation handled while Core is held.
        count_hits(&points, CORE_HOLD);
        let mut routed_from = 0;
        for (index, (step, lines)) in leases.fill.iter().enumerate() {
            if index == 0 {
                case.signal(1);
            }
            ready(&case, &root, *step, base + *lines as u64).await;
            if index + 1 == leases.fill.len() {
                routed_from = hits(&root, ROUTED);
            }
            case.signal(1);
        }
        case.at_launch(leases.measured, 1).await;
        // The simultaneous occupancy, before the held peak is sampled:
        // the staged lines were routed and none was taken, so they wait in
        // their lanes; every fill line was taken and exactly one send per
        // session blocked, so each channel is full with its fifth line
        // decoded; each session's drain is still paused.
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        while hits(&root, ROUTED) < routed_from + STAGED_LINES as u64 {
            assert!(
                tokio::time::Instant::now() < by,
                "the staged lines were not routed"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let sampled_from = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(
            hits(&root, TAKE),
            base + (FILL_LINES * LEASES) as u64,
            "a staged line was taken, or a fill line was not"
        );
        assert_eq!(
            hits(&root, BLOCKED) - blocked_from,
            LEASES as u64,
            "blocked channel sends during the fill"
        );
        assert_eq!(
            hits(&root, CORE_HOLD),
            core_from,
            "Core took an observation while every drain was held"
        );
        let held_for = sampled_from.elapsed();
        let blocked = hits(&root, BLOCKED) - blocked_from;
        phase.store(phase::DRAIN, Ordering::Release);
        // Every drain is released together and handles its other
        // final-text pieces; then every turn completes together, so 32
        // spilled final texts settle at once, past the Store's 16
        // blob-step slots (bead via-s4s).
        let pieces = via_adapters::final_text_pieces(&prompt).count() as u64;
        let handled = hits(&root, CORE_HOLD) + LEASES as u64 * (pieces * FILL_LINES as u64 - 1);
        for occurrence in &paused {
            release_point(&root, CORE_HOLD, *occurrence);
        }
        let by = tokio::time::Instant::now() + Duration::from_secs(60);
        while hits(&root, CORE_HOLD) < handled {
            assert!(
                tokio::time::Instant::now() < by,
                "Core handled {} of {handled} observations",
                hits(&root, CORE_HOLD)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        case.signal(1);
        for session in &sessions {
            let envelope = daemon.wait(session, 1).await;
            assert_eq!(envelope["state"], "completed", "{envelope}");
        }
        case.at_launch(leases.completed, 1).await;
        case.signal(1);
        for session in &sessions {
            daemon.close(session).await;
        }
        daemon.shutdown().await;
        (baseline, spawned, flood, sampling, (held_for, blocked))
    });
    stop.store(true, Ordering::Release);
    let samples = sampling.join().unwrap();
    let hwm = status_kib("self", "VmHWM:");
    let peak_sampled = samples
        .iter()
        .map(|sample| sample.rss_kib)
        .max()
        .unwrap_or(0);
    let peak_held = samples
        .iter()
        .filter(|sample| sample.phase == phase::HELD)
        .map(|sample| sample.rss_kib)
        .max()
        .unwrap_or(0);
    let peak = hwm.max(peak_sampled);
    // Growth, peak to peak as F24 (`s1_f24_memory.rs`): the highest RSS
    // with 32 MiB <= flooded < 64 MiB against the highest from 64 MiB on,
    // in the flood phase only.
    let flooded = |sample: &RssSample| sample.written.saturating_sub(flood.0);
    let in_flood = || samples.iter().filter(|sample| sample.phase == phase::FLOOD);
    let before_64 = || in_flood().filter(|sample| (32 * MIB..64 * MIB).contains(&flooded(sample)));
    let after_64 = || in_flood().filter(|sample| flooded(sample) >= 64 * MIB);
    let level = before_64().map(|sample| sample.rss_kib).max().unwrap_or(0);
    let after = after_64().map(|sample| sample.rss_kib).max().unwrap_or(0);
    let limit = leases_sum() * 5 / 4 / 1024;
    let metrics = json!({
        "baseline_kib": baseline, "peak_kib": peak, "peak_hwm_kib": hwm,
        "peak_sampled_kib": peak_sampled, "peak_held_kib": peak_held,
        "after_spawn_kib": spawned, "samples": samples.len(),
        "sum_kib": leases_sum() / 1024, "limit_kib": limit,
        "marginal_per_session_kib": spawned.saturating_sub(baseline) / LEASES as u64,
        "marginal_per_held_session_kib": peak_held.saturating_sub(baseline) / LEASES as u64,
        "flood_bytes": flood.1 - flood.0, "flood_samples": in_flood().count(),
        "samples_before_64_mib": before_64().count(),
        "samples_after_64_mib": after_64().count(),
        "held_samples": samples.iter().filter(|sample| sample.phase == phase::HELD).count(),
        "held_sampled_ms": held_for.as_millis(),
        "level_before_64_mib_kib": level,
        "rss_after_64_mib_kib": after,
        "sessions": LEASES, "flood_lines": FLOOD_LINES,
        "fill_lines": FILL_LINES * LEASES, "staged_bytes": maximal * STAGED_LINES as u64,
        "taken": hits(&root, TAKE), "blocked_sends_held": blocked,
        "maximal_line": maximal, "runtime_ms": started_at.elapsed().as_millis(),
        "malloc_arena_max": env::var("MALLOC_ARENA_MAX").ok(),
    });
    println!("codex_rss_leases {metrics}");
    assert!(flood.1 - flood.0 >= 256 * MIB, "{metrics}");
    assert!(
        before_64().count() > 0 && after_64().count() > 0,
        "an empty growth window: {metrics}"
    );
    assert!(
        peak.saturating_sub(baseline) <= limit,
        "peak RSS less the baseline is over 1.25 x the computed sum: {metrics}"
    );
    assert!(
        after.saturating_sub(level) < 32 * 1024,
        "RSS grew 32 MiB or more after the flood's first 64 MiB: {metrics}"
    );
}

mod codex_gaps;
