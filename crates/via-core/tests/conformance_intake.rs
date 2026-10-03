//! S-CORE chunk 5, Core halves (adapter design §7 S-CORE row, §5.1 #3–#39):
//! the intake, receipts, reads and the envelope made generic. Each case
//! runs end to end through Core's public Engine, as daemon main drives it,
//! over the real Store, Route, Wire and Host with the fake agent on its
//! scenario profiles (decisions H1, H2), and asserts receipts, refusals,
//! `status` and the committed envelopes.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    env, fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::{Value, json};
use via_core::{
    AdapterConfig, ApiError, BootstrapEnv, Deadline, DescribeParams, Engine, ModelsParams,
    ResumeParams, SessionId, SpawnParams, StatusParams, SteerParams, WaitParams,
};

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
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

/// Runs `body` on a current-thread runtime.
fn run<F: Future<Output = ()>>(body: F) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body);
}

/// A private test root: State, runtime and sync directories, and scenario
/// files written into it.
struct Root(tempfile::TempDir);

impl Root {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for part in ["state", "runtime", "runtime/anchors", "sync"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.path().join(part))
                .unwrap();
        }
        Self(root)
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// Writes `scenario` as `name` and returns its path.
    fn scenario(&self, name: &str, scenario: &Value) -> PathBuf {
        let path = self.path().join(name);
        fs::write(&path, scenario.to_string()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
}

/// One daemon's Engine on `root`, deployed with the fake scenario at
/// `scenario`, whose session dispatchers run as daemon main runs them.
struct Daemon {
    engine: Arc<Engine>,
    starter: tokio::task::JoinHandle<()>,
    dispatchers: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    root: PathBuf,
}

impl Daemon {
    fn open(root: &Root, scenario: &Path) -> Self {
        let root = root.path();
        let env = BootstrapEnv::from_vars([
            ("VIA_FAKE_AGENT_BINARY", binary("via-fake-agent")),
            ("VIA_FAKE_SCENARIO", scenario.to_path_buf()),
            ("VIA_FAKE_SYNC_DIR", root.join("sync")),
        ]);
        let engine = Engine::open(
            &root.join("state"),
            &root.join("runtime"),
            AdapterConfig::load(env, None).unwrap(),
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

    /// `spawn` with the members `raw` plus the test handle; the receipt.
    async fn try_spawn(&self, raw: &Value) -> Result<Value, ApiError> {
        let mut raw = raw.clone();
        raw["handle"] = json!(HANDLE);
        let params: SpawnParams = serde_json::from_value(raw.clone()).unwrap();
        self.engine
            .spawn(params, &raw.to_string())
            .await
            .map(|receipted| receipted.receipt)
    }

    /// A spawn that must succeed: its session.
    async fn spawn(&self, raw: &Value) -> SessionId {
        let receipt = self.try_spawn(raw).await.unwrap();
        session_of(&receipt)
    }

    /// `resume` of `session` with the members `raw`; the turn receipt.
    async fn try_resume(&self, session: &SessionId, raw: &Value) -> Result<Value, ApiError> {
        let mut raw = raw.clone();
        raw["session"] = json!(session);
        raw["handle"] = json!(HANDLE);
        let params: ResumeParams = serde_json::from_value(raw.clone()).unwrap();
        self.engine
            .resume(params, &raw.to_string())
            .await
            .map(|receipted| receipted.receipt)
    }

    /// `steer` of `session` with the members `raw`.
    async fn try_steer(&self, session: &SessionId, raw: &Value) -> Result<Value, ApiError> {
        let mut raw = raw.clone();
        raw["session"] = json!(session);
        raw["handle"] = json!(HANDLE);
        let params: SteerParams = serde_json::from_value(raw.clone()).unwrap();
        self.engine.steer(params, &raw.to_string()).await
    }

    /// `status` of `session`.
    async fn status(&self, session: &SessionId) -> Value {
        let params: StatusParams = serde_json::from_value(json!({"session":session})).unwrap();
        self.engine.status(params).await.unwrap()
    }

    fn describe(&self, raw: &Value) -> Result<Value, ApiError> {
        let params: DescribeParams = serde_json::from_value(raw.clone()).unwrap();
        self.engine.describe(&params)
    }

    fn models(&self, raw: &Value) -> Value {
        let params: ModelsParams = serde_json::from_value(raw.clone()).unwrap();
        self.engine.models(&params)
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

    /// The session's committed events, in order.
    async fn events(&self, session: &SessionId) -> Vec<Value> {
        let params = serde_json::from_value(json!({"session":session,"limit":1000})).unwrap();
        let page = self.engine.events(params).await.unwrap();
        let page: Value = serde_json::from_str(page.get()).unwrap();
        page["events"].as_array().unwrap().clone()
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

    /// What VIA last wrote first to a fake agent, if any agent read one.
    fn first_input(&self) -> Option<Value> {
        fs::read(self.root.join("sync").join("first-input"))
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
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

fn session_of(receipt: &Value) -> SessionId {
    serde_json::from_value(receipt["session_id"].clone()).unwrap()
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

fn terminal(turn: u32) -> Value {
    emit(
        &json!({"type":"terminal","vendor_turn_id":vendor_turn(turn),
                  "status":"completed","final_text":"done","stop_reason":"end_turn"}),
    )
}

/// A completed terminal of `turn` carrying `output` as its structured output.
fn structured(turn: u32, output: &Value) -> Value {
    emit(
        &json!({"type":"terminal","vendor_turn_id":vendor_turn(turn),
                  "status":"completed","final_text":"done","stop_reason":"end_turn",
                  "structured_output":output}),
    )
}

fn gate(name: &str) -> Value {
    json!({"action":"gate","name":name})
}

/// The script run by the start whose prompt is `prompt`.
fn script(prompt: &str, steps: &[Value]) -> Value {
    json!({"expected_request":{"type":"start","prompt":prompt},"steps":steps})
}

/// A script run by a start that carries at least `expected`.
fn script_expecting(expected: &Value, steps: &[Value]) -> Value {
    let mut request = json!({"type":"start"});
    for (member, value) in expected.as_object().unwrap() {
        request[member] = value.clone();
    }
    json!({"expected_request":request,"steps":steps})
}

/// A `{profile, scripts}` scenario (decision H2).
fn scenario(profile: &Value, scripts: &[Value]) -> Value {
    json!({"profile": profile, "scripts": scripts})
}

/// The default fake capabilities with each `(pointer, value)` replaced.
fn capabilities(changes: &[(&str, Value)]) -> Value {
    let mut capabilities = json!({
        "verbs": {"spawn":{"support":"native"},"resume":{"support":"native"},
                  "steer":{"support":"unsupported","reason":"no steer input"},
                  "cancel":{"support":"native"},"close":{"support":"native"}},
        "params": {"instructions":{"support":"unsupported","reason":"no instructions input"},
                   "output_schema":{"support":"unsupported","reason":"no schema input"},
                   "effort":{"support":"unsupported","reason":"no effort setting"},
                   "max_steps":{"support":"unsupported","reason":"no step limit"}},
        "bounds": [], "network_control": false,
        "recover": {"support":"unsupported","reason":"no recovery"},
        "usage": {"tokens":"turn","cost":"unavailable"}
    });
    for (pointer, value) in changes {
        *capabilities.pointer_mut(pointer).unwrap() = value.clone();
    }
    capabilities
}

fn native() -> Value {
    json!({"support":"native"})
}

/// The C1 §4 bound every per-turn profile here enforces.
fn full_bound() -> Value {
    json!({"mode":"full","extra_write_dirs":[],"network":true})
}

/// A refusal's C1 `kind`, `data.field` and `data.route`.
fn refused(error: &ApiError) -> (String, Value, Value) {
    let data = error.data();
    (
        error.kind.to_owned(),
        data["field"].clone(),
        data["route"].clone(),
    )
}

/// The envelope's or reply's warnings with `code`.
fn with_code<'a>(value: &'a Value, code: &str) -> Vec<&'a Value> {
    value["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|warning| warning["code"] == code)
        .collect()
}

/// (4) C1 §4.1 `require` on a profile whose `cancel` is partial: `cancel`
/// is `missing_capability` naming it and the route, in `describe`'s
/// refusals and as spawn's error; `cancel:partial` is met.
#[test]
fn conformance_intake_require_cancel_partial() {
    let root = Root::new();
    let partial = json!({"support":"partial","semantics":"stops_after_tool"});
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &json!({"capabilities": capabilities(&[("/verbs/cancel", partial)])}),
            &[script("p", &[accepted(1), terminal(1)])],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let strict = daemon
            .describe(&json!({"harness":"fake","model":"fake","require":["cancel"]}))
            .unwrap();
        let refusals = strict["refusals"].as_array().unwrap();
        assert_eq!(refusals.len(), 1, "{strict}");
        assert_eq!(refusals[0]["kind"], "missing_capability", "{strict}");
        assert_eq!(refusals[0]["field"], "cancel", "{strict}");
        assert_eq!(refusals[0]["route"], "fake", "{strict}");
        let lenient = daemon
            .describe(&json!({"harness":"fake","model":"fake","require":["cancel:partial"]}))
            .unwrap();
        assert_eq!(lenient["refusals"], json!([]), "{lenient}");
        assert_eq!(
            lenient["capabilities"]["verbs"]["cancel"]["support"], "partial",
            "{lenient}"
        );

        let error = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"p","require":["cancel"]}))
            .await
            .unwrap_err();
        assert_eq!(
            refused(&error),
            (
                "missing_capability".to_owned(),
                json!("cancel"),
                json!("fake")
            ),
            "{error:?}"
        );
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"p",
                           "require":["cancel:partial"]}))
            .await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        daemon.stop().await;
    });
}

/// (6) AD12 chain across daemons: v1 → v2 → v3, each version declaring
/// only its predecessor compatible. Receipt, envelope and status report
/// the adapter version that ran; a compatible resume advances the
/// session's; a session last run by v1 is refused by v3
/// `harness_unavailable` with `data.reason: "adapter_version"`.
#[test]
fn conformance_intake_adapter_version_chain() {
    let root = Root::new();
    let scripts = [
        script("a", &[accepted(1), terminal(1)]),
        script("b", &[accepted(1), terminal(1)]),
        script("c", &[accepted(2), terminal(2)]),
        script("d", &[accepted(3), terminal(3)]),
    ];
    let version = |name: &str, profile: &Value| root.scenario(name, &scenario(profile, &scripts));
    let v1 = version("v1.json", &json!({"adapter_version":"1"}));
    let v2 = version(
        "v2.json",
        &json!({"adapter_version":"2","compatible":["1"]}),
    );
    let v3 = version(
        "v3.json",
        &json!({"adapter_version":"3","compatible":["2"]}),
    );
    run(async {
        let daemon = Daemon::open(&root, &v1);
        let receipt = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"a"}))
            .await
            .unwrap();
        assert_eq!(receipt["adapter_version"], "1", "{receipt}");
        let chained = session_of(&receipt);
        let envelope = daemon.wait(&chained, 1).await;
        assert_eq!(envelope["adapter_version"], "1", "{envelope}");
        assert_eq!(daemon.status(&chained).await["adapter_version"], "1");
        let left = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"b"}))
            .await;
        assert_eq!(daemon.wait(&left, 1).await["state"], "completed");
        daemon.stop().await;

        let daemon = Daemon::open(&root, &v2);
        assert_eq!(daemon.status(&chained).await["adapter_version"], "1");
        daemon
            .try_resume(&chained, &json!({"prompt":"c"}))
            .await
            .unwrap();
        let envelope = daemon.wait(&chained, 2).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["adapter_version"], "2", "{envelope}");
        assert_eq!(daemon.status(&chained).await["adapter_version"], "2");
        daemon.stop().await;

        let daemon = Daemon::open(&root, &v3);
        daemon
            .try_resume(&chained, &json!({"prompt":"d"}))
            .await
            .unwrap();
        let envelope = daemon.wait(&chained, 3).await;
        assert_eq!(envelope["adapter_version"], "3", "{envelope}");
        assert_eq!(daemon.status(&chained).await["adapter_version"], "3");
        let error = daemon
            .try_resume(&left, &json!({"prompt":"d"}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, "harness_unavailable", "{error:?}");
        assert_eq!(error.data()["reason"], "adapter_version", "{error:?}");
        assert_eq!(daemon.status(&left).await["adapter_version"], "1");
        daemon.stop().await;
    });
}

/// (10) Design §5.2: a spawn naming only a model resolves its harness from
/// the configured catalogs; an alias resolves to the catalogued name, and
/// a model no catalog lists is `unknown_model`.
#[test]
fn conformance_intake_model_only_spawn() {
    let root = Root::new();
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &json!({"models":[{"model":"fake-pro","aliases":["pro"]}]}),
            &[script("p", &[accepted(1), terminal(1)])],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let receipt = daemon
            .try_spawn(&json!({"model":"pro","prompt":"p"}))
            .await
            .unwrap();
        assert_eq!(receipt["route"], "fake", "{receipt}");
        assert_eq!(receipt["effective"]["model"], "fake-pro", "{receipt}");
        let session = session_of(&receipt);
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["harness"], "fake", "{envelope}");
        assert_eq!(
            envelope["model"],
            json!({"requested":"pro","resolved":"fake-pro"}),
            "{envelope}"
        );
        let status = daemon.status(&session).await;
        assert_eq!(status["harness"], "fake", "{status}");
        let error = daemon
            .try_spawn(&json!({"model":"nope","prompt":"p"}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, "unknown_model", "{error:?}");
        assert_eq!(
            daemon.models(&json!({})),
            json!({"models":[{"model":"fake-pro","harness":"fake","aliases":["pro"],
                              "source":"bundled"}]})
        );
        daemon.stop().await;
    });
}

/// Critical r2 #6 (C1 §4, design §6.4): the resolved model is held to the
/// 1 KiB encoded cap, as the requested one is: a short alias resolving to
/// a 2,048-byte model is `invalid_params` naming `model`, and no receipt
/// commits.
#[test]
fn conformance_intake_resolved_model_is_capped() {
    let root = Root::new();
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &json!({"models":[{"model":"m".repeat(2048),"aliases":["pro"]}]}),
            &[],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let error = daemon
            .try_spawn(&json!({"model":"pro","prompt":"p"}))
            .await
            .unwrap_err();
        assert_eq!(
            (error.kind, &error.data()["field"]),
            ("invalid_params", &json!("model")),
            "{error:?}"
        );
        daemon.stop().await;
    });
    let db = rusqlite::Connection::open(root.path().join("state").join("store.sqlite3")).unwrap();
    let sessions: i64 = db
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sessions, 0, "no receipt commits");
}

/// (11) Steer on a native profile (C1 §3.4): a steer naming another turn is
/// `turn_mismatch`; one issued while the turn is submitting is answered
/// once it is accepted, is delivered through the driver and commits
/// `steer.delivered`; one whose turn fails before acceptance, and one on
/// an idle session, are `no_active_turn`.
#[test]
fn conformance_intake_steer_delivered_and_refused() {
    let root = Root::new();
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &json!({"capabilities": capabilities(&[("/verbs/steer", native())])}),
            &[
                script(
                    "steered",
                    &[
                        gate("submitting"),
                        accepted(1),
                        json!({"action":"expect_request","expected":{"type":"steer","id":3}}),
                        emit(&json!({"type":"steer_delivered","id":3,
                                    "vendor_turn_id":vendor_turn(1)})),
                        terminal(1),
                    ],
                ),
                script(
                    "refused",
                    &[gate("refusing"), json!({"action":"exit","code":1})],
                ),
            ],
        ),
    );
    run(async {
        let daemon = Arc::new(Daemon::open(&root, &path));
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"steered"}))
            .await;
        daemon.entered("submitting").await;
        let error = daemon
            .try_steer(&session, &json!({"text":"x","expect_turn":2}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, "turn_mismatch", "{error:?}");
        let steering = {
            let daemon = Arc::clone(&daemon);
            let session = session.clone();
            tokio::spawn(async move {
                daemon
                    .try_steer(&session, &json!({"text":"also","expect_turn":1}))
                    .await
            })
        };
        // Issued while the gate holds the acceptance back, it is answered
        // once the turn is accepted. Its entry into the acceptance wait is
        // not observable from here: the engine's unit tests prove it through
        // the `steer_waiting` hold (Sol r1 #18).
        daemon.release("submitting");
        let reply = steering.await.unwrap().unwrap();
        assert_eq!(
            reply,
            json!({"turn":format!("{session}/1"),"delivery":"injected"})
        );
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        let delivered: Vec<_> = daemon
            .events(&session)
            .await
            .into_iter()
            .filter(|event| event["type"] == "steer.delivered")
            .collect();
        assert_eq!(delivered.len(), 1, "{delivered:?}");
        assert_eq!(delivered[0]["delivery"], "injected");
        assert_eq!(delivered[0]["turn"], 1);

        let error = daemon
            .try_steer(&session, &json!({"text":"idle"}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, "no_active_turn", "{error:?}");

        daemon
            .try_resume(&session, &json!({"prompt":"refused"}))
            .await
            .unwrap();
        daemon.entered("refusing").await;
        let steering = {
            let daemon = Arc::clone(&daemon);
            let session = session.clone();
            tokio::spawn(async move { daemon.try_steer(&session, &json!({"text":"late"})).await })
        };
        daemon.release("refusing");
        let error = steering.await.unwrap().unwrap_err();
        assert_eq!(error.kind, "no_active_turn", "{error:?}");
        assert_eq!(daemon.wait(&session, 2).await["state"], "failed");
        Arc::into_inner(daemon).unwrap().stop().await;
    });
}

/// K2 (via-jm4.36, C1 §3, §3.4): a keyed steer's first answer is its
/// answer. A repeat with the same key and request replays the stored
/// delivery without reaching the driver again: it is answered while the
/// turn still runs, which a resent input could not be (the vendor never
/// acknowledges a second one), and after the turn ended, where a new steer
/// would be `no_active_turn`. The key with another request is
/// `idempotency_conflict`. A keyed refusal is stored too: `no_active_turn`
/// on an idle session replays as such while a later turn runs, and that
/// turn receives nothing. One `steer.delivered` is committed in all.
#[test]
fn conformance_intake_keyed_steer_replays_its_first_answer() {
    let root = Root::new();
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &json!({"capabilities": capabilities(&[("/verbs/steer", native())])}),
            &[
                script(
                    "p",
                    &[
                        accepted(1),
                        json!({"action":"expect_request","expected":{"type":"steer","id":3}}),
                        emit(&json!({"type":"steer_delivered","id":3,
                                    "vendor_turn_id":vendor_turn(1)})),
                        gate("running"),
                        terminal(1),
                    ],
                ),
                script("q", &[accepted(2), gate("second"), terminal(2)]),
            ],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"p"}))
            .await;
        let keyed = json!({"text":"also","op_key":"k-1"});
        let delivered = json!({"turn":format!("{session}/1"),"delivery":"injected"});
        assert_eq!(daemon.try_steer(&session, &keyed).await.unwrap(), delivered);
        daemon.entered("running").await;
        let repeat =
            tokio::time::timeout(Duration::from_secs(10), daemon.try_steer(&session, &keyed))
                .await
                .expect("a replay is answered while the turn runs");
        assert_eq!(repeat.unwrap(), delivered);
        let conflict = daemon
            .try_steer(&session, &json!({"text":"other","op_key":"k-1"}))
            .await
            .unwrap_err();
        assert_eq!(
            (conflict.kind, conflict.kind2),
            ("invalid_params", Some("idempotency_conflict")),
            "{conflict:?}"
        );
        daemon.release("running");
        assert_eq!(daemon.wait(&session, 1).await["state"], "completed");
        assert_eq!(daemon.try_steer(&session, &keyed).await.unwrap(), delivered);

        let idle = json!({"text":"idle","op_key":"k-idle"});
        let refused = daemon.try_steer(&session, &idle).await.unwrap_err();
        assert_eq!(refused.kind, "no_active_turn", "{refused:?}");
        daemon
            .try_resume(&session, &json!({"prompt":"q"}))
            .await
            .unwrap();
        daemon.entered("second").await;
        let replayed =
            tokio::time::timeout(Duration::from_secs(10), daemon.try_steer(&session, &idle))
                .await
                .expect("a stored refusal is answered while the next turn runs")
                .unwrap_err();
        assert_eq!(replayed.kind, "no_active_turn", "{replayed:?}");
        daemon.release("second");
        assert_eq!(daemon.wait(&session, 2).await["state"], "completed");
        let reports: Vec<Value> = daemon
            .events(&session)
            .await
            .into_iter()
            .filter(|event| event["type"] == "steer.delivered")
            .collect();
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0]["turn"], 1);
        daemon.stop().await;
    });
}

/// The schema the structured-output cases freeze.
fn schema() -> Value {
    json!({"type":"object","properties":{"a":{"type":"integer"}},"required":["a"]})
}

/// (12) C1 P5 generic inheritance and clearing: every omitted per-turn
/// value inherits from the latest accepted turn, a queued one included; a
/// null `output_schema` or `max_steps` clears; a null `effort` or `bound`
/// is `invalid_params`; each turn starts with its frozen values.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one session's turns, member by member"
)]
fn conformance_intake_generic_inheritance_and_clearing() {
    let root = Root::new();
    let profile = json!({
        "capabilities": capabilities(&[
            ("/params/effort", native()),
            ("/params/output_schema", native()),
            ("/params/max_steps", native()),
            ("/bounds", json!(["full"])),
        ]),
        "efforts": ["low", "high"],
    });
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &profile,
            &[
                script_expecting(
                    &json!({"prompt":"one","effort":"low","bound":full_bound(),
                            "output_schema":schema(),"max_steps":5}),
                    &[accepted(1), gate("one"), structured(1, &json!({"a":1}))],
                ),
                script_expecting(
                    &json!({"prompt":"two","effort":"high","bound":full_bound(),
                            "output_schema":schema(),"max_steps":5}),
                    &[accepted(2), structured(2, &json!({"a":2}))],
                ),
                script_expecting(
                    &json!({"prompt":"three","effort":"high","bound":full_bound()}),
                    &[accepted(3), terminal(3)],
                ),
            ],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let receipt = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"one",
                               "effort":"low","bound":full_bound(),"output_schema":schema(),
                               "max_steps":5,"deadlines":{"wall_ms":100_000}}))
            .await
            .unwrap();
        assert_eq!(
            receipt["effective"],
            json!({"model":"fake","effort":"low","bound":full_bound(),
                   "deadlines":{"wall_ms":100_000,"idle_ms":600_000},"max_steps":5}),
            "{receipt}"
        );
        let session = session_of(&receipt);
        daemon.entered("one").await;
        for (raw, field) in [
            (json!({"prompt":"x","effort":null}), "effort"),
            (json!({"prompt":"x","bound":null}), "bound"),
        ] {
            let error = daemon.try_resume(&session, &raw).await.unwrap_err();
            assert_eq!(error.kind, "invalid_params", "{raw}: {error:?}");
            assert_eq!(error.data()["field"], field, "{raw}: {error:?}");
        }
        let two = daemon
            .try_resume(&session, &json!({"prompt":"two","effort":"high"}))
            .await
            .unwrap();
        assert_eq!(
            two["effective"],
            json!({"model":"fake","effort":"high","bound":full_bound(),
                   "deadlines":{"wall_ms":100_000,"idle_ms":600_000},"max_steps":5}),
            "{two}"
        );
        // Turn 2 is queued, not started: turn 3 inherits from it.
        let three = daemon
            .try_resume(
                &session,
                &json!({"prompt":"three","output_schema":null,"max_steps":null}),
            )
            .await
            .unwrap();
        assert_eq!(
            three["effective"],
            json!({"model":"fake","effort":"high","bound":full_bound(),
                   "deadlines":{"wall_ms":100_000,"idle_ms":600_000},"max_steps":null}),
            "{three}"
        );
        daemon.release("one");
        let first = daemon.wait(&session, 1).await;
        assert_eq!(first["state"], "completed", "{first}");
        assert_eq!(
            first["bound"],
            json!({"requested":full_bound(),"effective":full_bound(),"inherited":false})
        );
        let second = daemon.wait(&session, 2).await;
        assert_eq!(second["state"], "completed", "{second}");
        assert_eq!(
            second["effort"],
            json!({"requested":"high","resolved":"high"})
        );
        assert_eq!(
            second["bound"],
            json!({"requested":full_bound(),"effective":full_bound(),"inherited":true})
        );
        let third = daemon.wait(&session, 3).await;
        assert_eq!(third["state"], "completed", "{third}");
        // The schema was cleared: no output is expected.
        assert!(
            with_code(&third, "structured_output_missing").is_empty(),
            "{third}"
        );
        let start = daemon.first_input().unwrap();
        assert!(
            start.get("max_steps").is_none() && start.get("output_schema").is_none(),
            "{start}"
        );
        daemon.stop().await;
    });
}

/// (13) The envelope fields of design §5.1 #33 come from the session's
/// plan and the turn's frozen values: harness, the requested and resolved
/// model, effort, bound, the route and the adapter version that ran it,
/// the instance's tested version, the frozen vendor options, and usage
/// under the route's declared token scope. `status` reports the described
/// turn's version, the session's adapter version and no version warning.
#[test]
fn conformance_intake_envelope_fields_from_the_plan() {
    let root = Root::new();
    let profile = json!({
        "adapter_version": "7.7.7",
        "models": [{"model":"fake-pro","aliases":["pro"]}],
        "capabilities": capabilities(&[
            ("/params/effort", native()),
            ("/bounds", json!(["full"])),
            ("/usage/tokens", json!("session_cumulative")),
        ]),
        "efforts": ["high"],
        "handshake": {"checked": ["1.0"], "requires": []},
    });
    let usage = emit(&json!({"type":"usage","vendor_turn_id":vendor_turn(1),
                             "total_tokens":10,"input":9,"output":1,"reasoning_output":0}));
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &profile,
            &[script(
                "p",
                &[
                    json!({"action":"hello","message":{"type":"hello",
                           "vendor_version":"1.0","features":[]}}),
                    accepted(1),
                    usage,
                    terminal(1),
                ],
            )],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let session = daemon
            .spawn(
                &json!({"harness":"fake","model":"pro","prompt":"p","effort":"high",
                           "bound":full_bound(),"vendor":{"fake":{}}}),
            )
            .await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        let expected = json!({
            "harness":"fake",
            "model":{"requested":"pro","resolved":"fake-pro"},
            "effort":{"requested":"high","resolved":"high"},
            "bound":{"requested":full_bound(),"effective":full_bound(),"inherited":false},
            "route":"fake","adapter_version":"7.7.7",
            "vendor_version":"1.0","version_status":"tested",
            "vendor_options":{"fake":{}},
        });
        for (member, value) in expected.as_object().unwrap() {
            assert_eq!(&envelope[member], value, "{member}: {envelope}");
        }
        assert_eq!(envelope["usage"]["scope"], "session_cumulative");
        assert_eq!(envelope["usage"]["total_tokens"], 10);
        assert!(
            with_code(&envelope, "vendor_version_untested").is_empty(),
            "{envelope}"
        );
        let status = daemon.status(&session).await;
        assert_eq!(status["adapter_version"], "7.7.7", "{status}");
        assert_eq!(status["vendor_version"], "1.0", "{status}");
        assert_eq!(status["version_status"], "tested", "{status}");
        assert_eq!(status["model"], "pro", "{status}");
        assert!(
            with_code(&status, "vendor_version_untested").is_empty(),
            "{status}"
        );
        daemon.stop().await;
    });
}

/// (15) #37: Core validates a structured output against the frozen schema
/// (draft 2020-12). Present and invalid → `failed(structured_output_invalid)`
/// with the output kept; requested with no output → the status is kept
/// and the envelope warns `structured_output_missing`; a valid one passes.
/// A schema that is not a valid draft 2020-12 schema is `invalid_params`.
#[test]
fn conformance_intake_structured_output_validation() {
    let root = Root::new();
    let profile = json!({"capabilities": capabilities(&[("/params/output_schema", native())])});
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &profile,
            &[
                script("bad", &[accepted(1), structured(1, &json!({"b":1}))]),
                script("none", &[accepted(2), terminal(2)]),
                script("good", &[accepted(3), structured(3, &json!({"a":3}))]),
            ],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let error = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"bad",
                               "output_schema":{"type":5}}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, "invalid_params", "{error:?}");
        assert_eq!(error.data()["field"], "output_schema", "{error:?}");
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"bad",
                           "output_schema":schema()}))
            .await;
        let bad = daemon.wait(&session, 1).await;
        assert_eq!(bad["state"], "failed", "{bad}");
        assert_eq!(
            bad["failure"]["class"], "structured_output_invalid",
            "{bad}"
        );
        assert_eq!(bad["structured_output"], json!({"b":1}), "{bad}");
        daemon
            .try_resume(&session, &json!({"prompt":"none"}))
            .await
            .unwrap();
        let none = daemon.wait(&session, 2).await;
        assert_eq!(none["state"], "completed", "{none}");
        assert_eq!(
            with_code(&none, "structured_output_missing").len(),
            1,
            "{none}"
        );
        daemon
            .try_resume(&session, &json!({"prompt":"good"}))
            .await
            .unwrap();
        let good = daemon.wait(&session, 3).await;
        assert_eq!(good["state"], "completed", "{good}");
        assert_eq!(good["structured_output"], json!({"a":3}), "{good}");
        assert!(
            with_code(&good, "structured_output_missing").is_empty(),
            "{good}"
        );
        daemon.stop().await;
    });
}

/// Sol r1 #16, #1, #2: every present structured output is validated
/// before it is stored. On a turn that ends other than completed, its
/// state and failure stand and the envelope warns
/// `structured_output_invalid` with `data.reason: "invalid"`; a completed
/// one whose validation reaches the bound fails `structured_output_invalid`
/// with `failure.data.reason: "validation_limit"`, the value kept. A schema
/// declaring another draft is `invalid_params` naming `output_schema`.
#[test]
fn conformance_intake_structured_output_validated_whatever_the_state() {
    let root = Root::new();
    let profile = json!({"capabilities": capabilities(&[("/params/output_schema", native())])});
    let failed = emit(&json!({"type":"terminal","vendor_turn_id":vendor_turn(1),
        "status":"failed","final_text":"","stop_reason":"error","vendor_code":"E1",
        "structured_output":{"b":1}}));
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &profile,
            &[
                script("failed", &[accepted(1), failed]),
                script("limit", &[accepted(2), structured(2, &json!(1))]),
            ],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let draft07 = json!({"$schema":"http://json-schema.org/draft-07/schema#",
                             "dependentRequired":{"a":["b"]}});
        let error = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"failed",
                               "output_schema":draft07}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, "invalid_params", "{error:?}");
        assert_eq!(error.data()["field"], "output_schema", "{error:?}");
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"failed",
                           "output_schema":schema()}))
            .await;
        let failed = daemon.wait(&session, 1).await;
        assert_eq!(failed["state"], "failed", "{failed}");
        assert_ne!(
            failed["failure"]["class"], "structured_output_invalid",
            "{failed}"
        );
        assert_eq!(failed["structured_output"], json!({"b":1}), "{failed}");
        let warned = with_code(&failed, "structured_output_invalid");
        assert_eq!(warned.len(), 1, "{failed}");
        assert_eq!(warned[0]["data"], json!({"reason":"invalid"}), "{failed}");
        // Sol's reference-doubling chain: exponential work for any integer.
        let mut defs = serde_json::Map::new();
        defs.insert("d0".into(), json!({"type":"string"}));
        for i in 1..=40 {
            let previous = json!({"$ref": format!("#/$defs/d{}", i - 1)});
            defs.insert(
                format!("d{i}"),
                json!({"anyOf":[previous.clone(), previous]}),
            );
        }
        daemon
            .try_resume(
                &session,
                &json!({"prompt":"limit","output_schema":{"$ref":"#/$defs/d40","$defs":defs}}),
            )
            .await
            .unwrap();
        let limited = daemon.wait(&session, 2).await;
        assert_eq!(limited["state"], "failed", "{limited}");
        assert_eq!(
            limited["failure"]["class"], "structured_output_invalid",
            "{limited}"
        );
        assert_eq!(
            limited["failure"]["data"],
            json!({"reason":"validation_limit"}),
            "{limited}"
        );
        assert_eq!(limited["structured_output"], json!(1), "{limited}");
        daemon.stop().await;
    });
}

/// (18) AD18 Core half: an effort the plan does not know is
/// `invalid_params` naming it and the route, with nothing launched; one the
/// plan knows but the instance's catalog lacks is `failed(submit_failed)`
/// with `failure.data {reason:"invalid_param", field:"effort"}`, no
/// `vendor_code` and no vendor submission.
#[test]
fn conformance_intake_effort_refused_at_receipt_and_at_submission() {
    let root = Root::new();
    let profile = json!({
        "capabilities": capabilities(&[("/params/effort", native())]),
        "efforts": ["low", "high"],
        "handshake": {"requires": []},
    });
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &profile,
            &[script(
                "catalog",
                &[
                    json!({"action":"hello","message":{"type":"hello","vendor_version":"1.0",
                           "features":[],"efforts":["low"]}}),
                    accepted(1),
                    terminal(1),
                ],
            )],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let error = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"catalog",
                               "effort":"turbo"}))
            .await
            .unwrap_err();
        assert_eq!(
            refused(&error),
            ("invalid_params".to_owned(), json!("effort"), json!("fake")),
            "{error:?}"
        );
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"catalog",
                           "effort":"high"}))
            .await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(envelope["failure"]["class"], "submit_failed", "{envelope}");
        assert_eq!(
            envelope["failure"]["data"],
            json!({"reason":"invalid_param","field":"effort"}),
            "{envelope}"
        );
        assert!(
            envelope["failure"].get("vendor_code").is_none(),
            "{envelope}"
        );
        assert!(daemon.first_input().is_none(), "nothing was submitted");
        daemon.stop().await;
    });
}

/// (19), (21) AD13, AC7: the effective inherited-configuration states are
/// frozen at spawn and shown in `status`; every category whose effective
/// state is not the requested one, in either direction (requested `off`
/// with an unverified switch, requested `on` with no switch and effective
/// `unknown` or a verified `off`), is listed in one
/// `config_switch_unverified` warning on the receipts, in `status` and on
/// every envelope.
#[test]
fn conformance_intake_config_switch_unverified() {
    let root = Root::new();
    let profile = json!({"categories": {
        "hooks": {"off": "unverified"},
        "mcp_servers": {"off": "verified"},
        "plugins": {"on": "none"},
        "skills": {"on": "verified"},
        "agents": {"on": "none", "observed": "off"},
    }});
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &profile,
            &[
                script("one", &[accepted(1), terminal(1)]),
                script("two", &[accepted(2), terminal(2)]),
            ],
        ),
    );
    let categories = json!([
        {"category":"hooks","requested":"off","effective":"unknown"},
        {"category":"plugins","requested":"on","effective":"unknown"},
        {"category":"agents","requested":"on","effective":"off"},
    ]);
    let one_warning = |value: &Value| {
        let found = with_code(value, "config_switch_unverified");
        assert_eq!(found.len(), 1, "{value}");
        assert_eq!(found[0]["data"]["categories"], categories, "{value}");
    };
    run(async {
        let daemon = Daemon::open(&root, &path);
        let receipt = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"one"}))
            .await
            .unwrap();
        one_warning(&receipt);
        let session = session_of(&receipt);
        let first = daemon.wait(&session, 1).await;
        one_warning(&first);
        let status = daemon.status(&session).await;
        assert_eq!(
            status["inherit"],
            json!({"hooks":"unknown","mcp_servers":"off","plugins":"unknown",
                   "skills":"on","agents":"off","instruction_files":"on"}),
            "{status}"
        );
        one_warning(&status);
        let receipt = daemon
            .try_resume(&session, &json!({"prompt":"two"}))
            .await
            .unwrap();
        one_warning(&receipt);
        one_warning(&daemon.wait(&session, 2).await);
        daemon.stop().await;
    });
}

/// A directory under `root` whose path is at most 4096 bytes but over
/// 4 KiB encoded as a JSON string: its names are control characters,
/// each escaped to six bytes.
fn escaped_directory(root: &Path) -> String {
    let name = "\u{1}".repeat(200);
    let mut path = root.join("cwd");
    while path.as_os_str().len() + 1 + name.len() <= 4096 {
        path.push(&name);
    }
    fs::create_dir_all(&path).unwrap();
    path.to_str().unwrap().to_owned()
}

/// C1 §5 (carry-over): a `cwd` over 4 KiB encoded is `invalid_params`
/// naming it at receipt, even when it names an existing directory of at
/// most 4096 bytes; `describe` refuses it the same way.
#[test]
fn conformance_intake_cwd_over_4_kib_encoded_is_refused() {
    let root = Root::new();
    let path = root.scenario(
        "scenario.json",
        &scenario(&json!({}), &[script("p", &[accepted(1), terminal(1)])]),
    );
    let cwd = escaped_directory(root.path());
    assert!(cwd.len() <= 4096);
    assert!(json!(cwd).to_string().len() > 4096);
    run(async {
        let daemon = Daemon::open(&root, &path);
        let error = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"p","cwd":cwd}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, "invalid_params", "{error:?}");
        assert_eq!(error.data()["field"], "cwd", "{error:?}");
        let error = daemon
            .describe(&json!({"harness":"fake","model":"fake","cwd":cwd}))
            .unwrap_err();
        assert_eq!(error.kind, "invalid_params", "{error:?}");
        assert_eq!(error.data()["field"], "cwd", "{error:?}");
        daemon.stop().await;
    });
}

/// Set in the child process the startup-directory case runs in.
const CWD_CHILD: &str = "VIA_INTAKE_CWD_CHILD";

/// Sol r1 #15 (C1 §5): an omitted `cwd` is the daemon's startup
/// directory, held to the same 4 KiB encoded cap: a startup directory over
/// it is `invalid_params` naming `cwd`. The working directory is the
/// process's, so the case changes it only in a child process of its own
/// (critical r1 #11): plain `cargo test` runs this binary's tests as
/// threads of one process.
#[test]
fn conformance_intake_startup_cwd_over_4_kib_encoded_is_refused() {
    const NAME: &str = "conformance_intake_startup_cwd_over_4_kib_encoded_is_refused";
    if env::var_os(CWD_CHILD).is_none() {
        let status = std::process::Command::new(env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(CWD_CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "{NAME} child failed: {status}");
        return;
    }
    let root = Root::new();
    let path = root.scenario(
        "scenario.json",
        &scenario(&json!({}), &[script("p", &[accepted(1), terminal(1)])]),
    );
    let cwd = escaped_directory(root.path());
    assert!(json!(cwd).to_string().len() > 4096);
    env::set_current_dir(&cwd).unwrap();
    run(async {
        let daemon = Daemon::open(&root, &path);
        let error = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"p"}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, "invalid_params", "{error:?}");
        assert_eq!(error.data()["field"], "cwd", "{error:?}");
        daemon.stop().await;
    });
}

/// Sol r1 #5 (C1 §4.1): a route whose `capabilities.verbs.spawn` is
/// unsupported refuses `spawn` `unsupported_verb` before any receipt; a
/// session whose frozen `verbs.resume` is unsupported refuses `resume`
/// the same way, before a receipt and before any driver call.
#[test]
fn conformance_intake_unsupported_spawn_and_resume_verbs() {
    let root = Root::new();
    let unsupported = json!({"support":"unsupported","reason":"no such verb"});
    let no_spawn = root.scenario(
        "no-spawn.json",
        &scenario(
            &json!({"capabilities": capabilities(&[("/verbs/spawn", unsupported.clone())])}),
            &[script("p", &[accepted(1), terminal(1)])],
        ),
    );
    let no_resume = root.scenario(
        "no-resume.json",
        &scenario(
            &json!({"capabilities": capabilities(&[("/verbs/resume", unsupported)])}),
            &[
                script("p", &[accepted(1), terminal(1)]),
                script("again", &[accepted(2), terminal(2)]),
            ],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &no_spawn);
        let error = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"p"}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, "unsupported_verb", "{error:?}");
        assert!(daemon.first_input().is_none(), "no agent was started");
        daemon.stop().await;

        let daemon = Daemon::open(&root, &no_resume);
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"p"}))
            .await;
        assert_eq!(daemon.wait(&session, 1).await["state"], "completed");
        let error = daemon
            .try_resume(&session, &json!({"prompt":"again"}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, "unsupported_verb", "{error:?}");
        let queued: Vec<_> = daemon
            .events(&session)
            .await
            .into_iter()
            .filter(|event| event["type"] == "turn.queued")
            .collect();
        assert_eq!(queued.len(), 1, "no second turn was received: {queued:?}");
        daemon.stop().await;
    });
}

/// Sol r1 #10 (C1 §8.1, §9): a refusal's `harness`, `route` and `verb` are
/// in `data` whenever known, with or without a `data.field`: steer's
/// `unsupported_verb` carries `verb: "steer"` and the session's harness
/// and route; spawn's and resume's carry their verb; a member the route
/// refuses carries its field, harness and route.
#[test]
fn conformance_intake_refusal_metadata() {
    let root = Root::new();
    let unsupported = json!({"support":"unsupported","reason":"no such verb"});
    let no_spawn = root.scenario(
        "no-spawn.json",
        &scenario(
            &json!({"capabilities": capabilities(&[("/verbs/spawn", unsupported.clone())])}),
            &[],
        ),
    );
    let no_resume = root.scenario(
        "no-resume.json",
        &scenario(
            &json!({"capabilities": capabilities(&[("/verbs/resume", unsupported)])}),
            &[script("p", &[gate("running"), accepted(1), terminal(1)])],
        ),
    );
    let metadata = |error: &ApiError| {
        let data = error.data();
        (
            error.kind.to_owned(),
            data["verb"].clone(),
            data["harness"].clone(),
            data["route"].clone(),
        )
    };
    let fake = || json!("fake");
    run(async {
        let daemon = Daemon::open(&root, &no_spawn);
        let error = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"p"}))
            .await
            .unwrap_err();
        assert_eq!(
            metadata(&error),
            (
                "unsupported_verb".to_owned(),
                json!("spawn"),
                fake(),
                fake()
            ),
            "{error:?}"
        );
        daemon.stop().await;

        let daemon = Daemon::open(&root, &no_resume);
        let error = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"p",
                               "instructions":{"text":"be brief"}}))
            .await
            .unwrap_err();
        assert_eq!(error.data()["field"], "instructions", "{error:?}");
        assert_eq!(
            metadata(&error),
            ("invalid_params".to_owned(), Value::Null, fake(), fake()),
            "{error:?}"
        );
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"p"}))
            .await;
        daemon.entered("running").await;
        let error = daemon
            .try_steer(&session, &json!({"text":"x"}))
            .await
            .unwrap_err();
        assert_eq!(
            metadata(&error),
            (
                "unsupported_verb".to_owned(),
                json!("steer"),
                fake(),
                fake()
            ),
            "{error:?}"
        );
        daemon.release("running");
        assert_eq!(daemon.wait(&session, 1).await["state"], "completed");
        let error = daemon
            .try_resume(&session, &json!({"prompt":"again"}))
            .await
            .unwrap_err();
        assert_eq!(
            metadata(&error),
            (
                "unsupported_verb".to_owned(),
                json!("resume"),
                fake(),
                fake()
            ),
            "{error:?}"
        );
        daemon.stop().await;
    });
}

/// Rewrites a session's frozen row in the Store under `root` with `sql`
/// (`?1` is the session), as a corrupting writer would.
fn tamper(root: &Path, session: &SessionId, sql: &str) {
    let db = rusqlite::Connection::open(root.join("state").join("store.sqlite3")).unwrap();
    db.busy_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(db.execute(sql, [session.as_str()]).unwrap(), 1, "{sql}");
}

/// Frozen values that are present, valid JSON, but not what Core froze.
const BAD_PARAMS: &str =
    "UPDATE sessions SET params=json_set(params,'$.allow_untested','yes') WHERE id=?1";
const BAD_CAPABILITIES: &str =
    "UPDATE sessions SET receipt=json_set(receipt,'$.capabilities.usage',7) WHERE id=?1";

/// Sol r1 #14 (T3 §7.3): a queued turn whose session's frozen parameters
/// or capabilities cannot be decoded fails its submission through
/// `commit_submit_failed`, `failed(store)` with no agent I/O, both live
/// (corrupted while it waits behind a running turn) and on restart (the
/// handoff meets it at its session's head).
#[test]
fn conformance_intake_corrupt_frozen_session_values_fail_submission() {
    // Live: turn 2 waits behind turn 1 while its session row is corrupted.
    for corruption in [BAD_PARAMS, BAD_CAPABILITIES] {
        let root = Root::new();
        let path = root.scenario(
            "scenario.json",
            &scenario(
                &json!({}),
                &[
                    script("first", &[gate("hold"), accepted(1), terminal(1)]),
                    script("second", &[accepted(2), terminal(2)]),
                ],
            ),
        );
        run(async {
            let daemon = Daemon::open(&root, &path);
            let session = daemon
                .spawn(&json!({"harness":"fake","model":"fake","prompt":"first"}))
                .await;
            daemon.entered("hold").await;
            daemon
                .try_resume(&session, &json!({"prompt":"second"}))
                .await
                .unwrap();
            tamper(root.path(), &session, corruption);
            daemon.release("hold");
            assert_eq!(daemon.wait(&session, 1).await["state"], "completed");
            let envelope = daemon.wait(&session, 2).await;
            assert_eq!(envelope["state"], "failed", "{corruption}: {envelope}");
            assert_eq!(envelope["failure"]["class"], "store", "{envelope}");
            assert!(
                envelope["timestamps"]["accepted_at"].is_null(),
                "{envelope}"
            );
            daemon.stop().await;
        });
    }
    // Restart: a queued turn left by an Engine that dispatched nothing.
    for corruption in [BAD_PARAMS, BAD_CAPABILITIES] {
        let root = Root::new();
        let path = root.scenario("scenario.json", &scenario(&json!({}), &[]));
        let open = || {
            let env = BootstrapEnv::from_vars([
                ("VIA_FAKE_AGENT_BINARY", binary("via-fake-agent")),
                ("VIA_FAKE_SCENARIO", path.clone()),
                ("VIA_FAKE_SYNC_DIR", root.path().join("sync")),
            ]);
            Engine::open(
                &root.path().join("state"),
                &root.path().join("runtime"),
                AdapterConfig::load(env, None).unwrap(),
                binary("via"),
            )
            .unwrap()
        };
        run(async {
            let engine = open();
            let raw = json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE});
            let (session, _) = engine
                .spawn(
                    serde_json::from_value(raw.clone()).unwrap(),
                    &raw.to_string(),
                )
                .await
                .unwrap()
                .enqueued
                .unwrap();
            drop(engine);
            tamper(root.path(), &session, corruption);
            let engine = open();
            assert_eq!(engine.recover().await.unwrap(), 0);
            let handoff = engine.hand_off_queued().await.unwrap();
            assert_eq!((handoff.enqueued, handoff.failed), (0, 1), "{corruption}");
            let envelope = engine.result(&format!("{session}/1")).await.unwrap();
            let envelope: Value = serde_json::from_str(envelope.get()).unwrap();
            assert_eq!(envelope["state"], "failed", "{envelope}");
            assert_eq!(envelope["failure"]["class"], "store", "{envelope}");
        });
    }
}

/// Sol r1 #3 (C1 §5 `bound`, C2 `RoutePlan.effective_bound`): on a route
/// that normalizes bounds, the requested and effective bounds stay
/// separate. The effective one is in the receipt, the launch input and the
/// envelope; the requested one is the envelope's `requested`, inherited
/// with it. An effective bound over 32 KiB encoded is `invalid_params`
/// naming `bound`.
#[test]
fn conformance_intake_effective_bound_from_the_plan() {
    let root = Root::new();
    let normalized = json!({"mode":"full","extra_write_dirs":["/normalized"],"network":true});
    let capabilities = capabilities(&[("/bounds", json!(["full"]))]);
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &json!({"capabilities": capabilities, "normalized_bound": normalized}),
            &[
                script_expecting(
                    &json!({"prompt":"one","bound":normalized}),
                    &[accepted(1), terminal(1)],
                ),
                script_expecting(
                    &json!({"prompt":"two","bound":normalized}),
                    &[accepted(2), terminal(2)],
                ),
                script_expecting(
                    &json!({"prompt":"three","bound":normalized}),
                    &[accepted(3), terminal(3)],
                ),
            ],
        ),
    );
    let huge = json!({"mode":"full","extra_write_dirs":[format!("/{}", "d".repeat(33 * 1024))],
                      "network":true});
    let oversized = root.scenario(
        "oversized.json",
        &scenario(
            &json!({"capabilities": capabilities, "normalized_bound": huge}),
            &[],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let receipt = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"one",
                               "bound":full_bound()}))
            .await
            .unwrap();
        assert_eq!(receipt["effective"]["bound"], normalized, "{receipt}");
        let session = session_of(&receipt);
        let first = daemon.wait(&session, 1).await;
        assert_eq!(first["state"], "completed", "{first}");
        assert_eq!(
            first["bound"],
            json!({"requested":full_bound(),"effective":normalized,"inherited":false})
        );
        let two = daemon
            .try_resume(&session, &json!({"prompt":"two","bound":full_bound()}))
            .await
            .unwrap();
        assert_eq!(two["effective"]["bound"], normalized, "{two}");
        let second = daemon.wait(&session, 2).await;
        assert_eq!(second["state"], "completed", "{second}");
        assert_eq!(
            second["bound"],
            json!({"requested":full_bound(),"effective":normalized,"inherited":false})
        );
        let three = daemon
            .try_resume(&session, &json!({"prompt":"three"}))
            .await
            .unwrap();
        assert_eq!(three["effective"]["bound"], normalized, "{three}");
        let third = daemon.wait(&session, 3).await;
        assert_eq!(third["state"], "completed", "{third}");
        assert_eq!(
            third["bound"],
            json!({"requested":full_bound(),"effective":normalized,"inherited":true})
        );
        daemon.stop().await;

        let daemon = Daemon::open(&root, &oversized);
        let error = daemon
            .try_spawn(&json!({"harness":"fake","model":"fake","prompt":"p",
                               "bound":full_bound()}))
            .await
            .unwrap_err();
        assert_eq!(
            refused(&error),
            ("invalid_params".to_owned(), json!("bound"), json!("fake")),
            "{error:?}"
        );
        daemon.stop().await;
    });
}

/// Sol r1 #4 (C1 §4 `instructions`): `{path}` names an absolute regular
/// UTF-8 file of at most 1 MiB, read once at the spawn receipt and frozen
/// as its text, which the route then receives as it would `{text}`. A
/// relative path, a directory, a missing, non-UTF-8 or larger file is
/// `invalid_params` naming `instructions`. The path is not stored, and a
/// keyed retry's identity is the copy's content: the same content replays,
/// other content under the key is `idempotency_conflict`.
#[test]
fn conformance_intake_instructions_path() {
    let root = Root::new();
    let profile = json!({"capabilities": capabilities(&[("/params/instructions", native())])});
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &profile,
            &[script_expecting(
                &json!({"prompt":"p","instructions":"be brief"}),
                &[accepted(1), terminal(1)],
            )],
        ),
    );
    let file = root.path().join("instructions.txt");
    fs::write(&file, "be brief").unwrap();
    let file = file.to_str().unwrap().to_owned();
    let not_utf8 = root.path().join("binary.txt");
    fs::write(&not_utf8, [0xff, 0xfe]).unwrap();
    let large = root.path().join("large.txt");
    fs::write(&large, "a".repeat(1024 * 1024 + 1)).unwrap();
    let spawn = |instructions: Value, key: &str| {
        json!({"harness":"fake","model":"fake","prompt":"p",
               "instructions":instructions,"idempotency_key":key})
    };
    run(async {
        let daemon = Daemon::open(&root, &path);
        for (name, refused_path) in [
            ("relative", "instructions.txt".to_owned()),
            ("directory", root.path().to_str().unwrap().to_owned()),
            (
                "missing",
                root.path().join("absent").to_str().unwrap().to_owned(),
            ),
            ("not_utf8", not_utf8.to_str().unwrap().to_owned()),
            ("large", large.to_str().unwrap().to_owned()),
        ] {
            let error = daemon
                .try_spawn(&spawn(json!({"path":refused_path}), name))
                .await
                .unwrap_err();
            assert_eq!(error.kind, "invalid_params", "{name}: {error:?}");
            assert_eq!(error.data()["field"], "instructions", "{name}: {error:?}");
        }
        let receipt = daemon
            .try_spawn(&spawn(json!({"path":file}), "k"))
            .await
            .unwrap();
        let session = session_of(&receipt);
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(
            daemon.first_input().unwrap()["instructions"],
            "be brief",
            "the route received the file's text"
        );
        // The same content under the key replays, rewritten or not.
        fs::write(&file, "be brief").unwrap();
        let replayed = daemon
            .try_spawn(&spawn(json!({"path":file}), "k"))
            .await
            .unwrap();
        assert_eq!(replayed, receipt);
        fs::write(&file, "be verbose").unwrap();
        let error = daemon
            .try_spawn(&spawn(json!({"path":file}), "k"))
            .await
            .unwrap_err();
        assert_eq!(error.data()["kind2"], "idempotency_conflict", "{error:?}");
        daemon.stop().await;
    });
    let db = rusqlite::Connection::open(root.path().join("state").join("store.sqlite3")).unwrap();
    let params: String = db
        .query_row("SELECT params FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert!(
        !params.contains("instructions.txt"),
        "the path was stored: {params}"
    );
    assert!(params.contains("be brief"), "{params}");
}

/// Sol r1 #11 (C1 §3.4, §8.1; C2 §2 `SteerError`): the driver's refusals of
/// an admitted steer map to C1 errors: a full control lane is
/// `admission_refused` with `data.reason: "control_lane_full"`; a vendor
/// that refuses steer in the turn's phase is `steer_failed` with
/// `data.reason: "not_steerable"` and `data.delivery: "none"`; input whose
/// writing began, in part or whole, without the vendor's acknowledgement
/// is `steer_failed` with `data.reason: "not_delivered"` and
/// `data.delivery: "uncertain"`, whose message keeps that uncertainty
/// (critical r1 #13, r4 #1); a delivery the vendor took whose report was not
/// recorded is `steer_failed` with `data.reason: "not_recorded"` and the
/// delivery a success would give (critical r2 #3). None commits
/// `steer.delivered`.
#[test]
fn conformance_intake_steer_error_mapping() {
    for (refusal, code, kind, reason, delivery, message) in [
        (
            "over_capacity",
            -32012,
            "admission_refused",
            "control_lane_full",
            Value::Null,
            "the session's control lane is full",
        ),
        (
            "not_steerable",
            -32021,
            "steer_failed",
            "not_steerable",
            json!("none"),
            "the steer input was not applied",
        ),
        (
            "not_delivered",
            -32021,
            "steer_failed",
            "not_delivered",
            json!("uncertain"),
            "the steer input was not acknowledged by the vendor; whether it was applied is unknown",
        ),
        (
            "not_recorded",
            -32021,
            "steer_failed",
            "not_recorded",
            json!("injected"),
            "the vendor took the steer input, but its steer.delivered event could not be recorded",
        ),
    ] {
        let root = Root::new();
        let path = root.scenario(
            "scenario.json",
            &scenario(
                &json!({"capabilities": capabilities(&[("/verbs/steer", native())]),
                        "steer_refusal": refusal}),
                &[script("p", &[accepted(1), gate("running"), terminal(1)])],
            ),
        );
        run(async {
            let daemon = Daemon::open(&root, &path);
            let session = daemon
                .spawn(&json!({"harness":"fake","model":"fake","prompt":"p"}))
                .await;
            daemon.entered("running").await;
            let error = daemon
                .try_steer(&session, &json!({"text":"x"}))
                .await
                .unwrap_err();
            let data = error.data();
            assert_eq!(
                (
                    error.code,
                    error.kind,
                    &data["reason"],
                    &data["delivery"],
                    error.message
                ),
                (code, kind, &json!(reason), &delivery, message),
                "{refusal}: {data}"
            );
            daemon.release("running");
            assert_eq!(daemon.wait(&session, 1).await["state"], "completed");
            assert!(
                daemon
                    .events(&session)
                    .await
                    .iter()
                    .all(|event| event["type"] != "steer.delivered"),
                "{refusal}"
            );
            daemon.stop().await;
        });
    }
}

/// Sol r1 #13 (C1 §3.7, C2 §4 `turn.accepted`): a running turn's `status`
/// reports its instance's handshake version once the turn is accepted,
/// `null` before; a tested version carries no `vendor_version_untested`
/// warning while the turn runs.
#[test]
fn conformance_intake_running_status_reports_the_accepted_instance() {
    let root = Root::new();
    let profile = json!({"handshake": {"checked": ["1.0"], "requires": []}});
    let path = root.scenario(
        "scenario.json",
        &scenario(
            &profile,
            &[script(
                "p",
                &[
                    json!({"action":"hello","message":{"type":"hello",
                           "vendor_version":"1.0","features":[]}}),
                    gate("submitting"),
                    accepted(1),
                    gate("running"),
                    terminal(1),
                ],
            )],
        ),
    );
    run(async {
        let daemon = Daemon::open(&root, &path);
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"p"}))
            .await;
        daemon.entered("submitting").await;
        let before = daemon.status(&session).await;
        assert_eq!(before["active_turn"]["phase"], "submitting", "{before}");
        assert_eq!(before["vendor_version"], Value::Null, "{before}");
        daemon.release("submitting");
        daemon.entered("running").await;
        // The acceptance is committed once `status` shows it.
        let by = tokio::time::Instant::now() + Duration::from_secs(30);
        let running = loop {
            let status = daemon.status(&session).await;
            if status["active_turn"]["phase"] == "accepted" {
                break status;
            }
            assert!(tokio::time::Instant::now() < by, "never accepted: {status}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert_eq!(running["vendor_version"], "1.0", "{running}");
        assert_eq!(running["version_status"], "tested", "{running}");
        assert!(
            with_code(&running, "vendor_version_untested").is_empty(),
            "{running}"
        );
        daemon.release("running");
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(envelope["vendor_version"], "1.0", "{envelope}");
        let ended = daemon.status(&session).await;
        assert_eq!(ended["version_status"], "tested", "{ended}");
        daemon.stop().await;
    });
}

/// Sol r2 #4 (C2 `check_turn` → `TurnCheck`): a resume turn's effective
/// bound is the one `check_turn` reports against the session's frozen
/// route, not a fresh plan's. A daemon whose model catalog no longer
/// lists the session's model still normalizes and runs the turn.
#[test]
fn conformance_intake_resume_bound_from_check_turn() {
    let root = Root::new();
    let normalized = json!({"mode":"full","extra_write_dirs":["/normalized"],"network":true});
    let profile = |models: Value| {
        json!({"capabilities": capabilities(&[("/bounds", json!(["full"]))]),
               "normalized_bound": normalized, "models": models})
    };
    let scripts = [
        script("one", &[accepted(1), terminal(1)]),
        script_expecting(
            &json!({"prompt":"two","bound":normalized}),
            &[accepted(2), terminal(2)],
        ),
    ];
    let v1 = root.scenario(
        "v1.json",
        &scenario(
            &profile(json!([{"model":"fake-pro","aliases":["pro"]}])),
            &scripts,
        ),
    );
    let v2 = root.scenario(
        "v2.json",
        &scenario(&profile(json!([{"model":"fake-next"}])), &scripts),
    );
    run(async {
        let daemon = Daemon::open(&root, &v1);
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"pro","prompt":"one"}))
            .await;
        assert_eq!(daemon.wait(&session, 1).await["state"], "completed");
        daemon.stop().await;

        let daemon = Daemon::open(&root, &v2);
        let two = daemon
            .try_resume(&session, &json!({"prompt":"two","bound":full_bound()}))
            .await
            .unwrap();
        assert_eq!(two["effective"]["bound"], normalized, "{two}");
        let second = daemon.wait(&session, 2).await;
        assert_eq!(second["state"], "completed", "{second}");
        assert_eq!(
            second["bound"],
            json!({"requested":full_bound(),"effective":normalized,"inherited":false})
        );
        daemon.stop().await;
    });
}

/// Sol r3 #2 (C2 `TurnCheck`, C1 §5 `bound`): a resume that omits the
/// bound inherits the requested one, and its effective bound is the one
/// `check_turn` reports for it now, not the earlier turn's. The session
/// spawned on a daemon whose route normalized `full` to one bound; the
/// daemon that resumes it normalizes it to another. The receipt reports,
/// the start carries and the envelope freezes the new one, with the
/// requested bound kept and `inherited: true`.
#[test]
fn conformance_intake_inherited_bound_from_check_turn() {
    let root = Root::new();
    let earlier = json!({"mode":"full","extra_write_dirs":["/earlier"],"network":true});
    let now = json!({"mode":"full","extra_write_dirs":["/now"],"network":true});
    let profile = |normalized: &Value| {
        json!({"capabilities": capabilities(&[("/bounds", json!(["full"]))]),
               "normalized_bound": normalized})
    };
    let scripts = [
        script_expecting(
            &json!({"prompt":"one","bound":earlier}),
            &[accepted(1), terminal(1)],
        ),
        script_expecting(
            &json!({"prompt":"two","bound":now}),
            &[accepted(2), terminal(2)],
        ),
    ];
    let v1 = root.scenario("v1.json", &scenario(&profile(&earlier), &scripts));
    let v2 = root.scenario("v2.json", &scenario(&profile(&now), &scripts));
    run(async {
        let daemon = Daemon::open(&root, &v1);
        let session = daemon
            .spawn(&json!({"harness":"fake","model":"fake","prompt":"one","bound":full_bound()}))
            .await;
        let first = daemon.wait(&session, 1).await;
        assert_eq!(first["state"], "completed", "{first}");
        assert_eq!(first["bound"]["effective"], earlier, "{first}");
        daemon.stop().await;

        let daemon = Daemon::open(&root, &v2);
        let two = daemon
            .try_resume(&session, &json!({"prompt":"two"}))
            .await
            .unwrap();
        assert_eq!(two["effective"]["bound"], now, "{two}");
        let second = daemon.wait(&session, 2).await;
        assert_eq!(second["state"], "completed", "{second}");
        assert_eq!(
            second["bound"],
            json!({"requested":full_bound(),"effective":now,"inherited":true}),
            "{second}"
        );
        daemon.stop().await;
    });
}
