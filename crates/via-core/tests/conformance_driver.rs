//! C2 conformance kit, run half (adapter design §8 item 2, C2 §7): the
//! adapter halves of the listed items through `SessionDriver::run_turn` over
//! the real Route, Wire and Host, with the fake agent on scenario profiles
//! (decisions H1, H2). The planning half is `conformance.rs`. Route's own
//! crate cannot open a Store, so these run one layer up, in process: the
//! fixture comes from `BootstrapEnv::from_vars`, not the environment.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    env, fs,
    future::Future,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};
use via_adapters::observation::{
    Admitted, Observation, ObservationItem, SteerDelivery, StopReason, TurnEnd, TurnError,
    observation_channel,
};
use via_adapters::{
    AdapterConfig, AdapterSet, AnchorRecovery, BootstrapEnv, Cleanup, Deadline, Inherit,
    OBSERVATION_ITEMS, Prepared, Recovery, RouteError, RuntimeConfig, SessionCx, SessionDriver,
    SessionId, SessionRef, SessionSpec, StartRejected, SteerError, SteerInput, StopCause,
    StopOrder, TurnActivity, TurnCause, TurnCx, TurnFailure, TurnNumber, TurnSpec,
    VendorTerminalStatus, VersionStatus, WireCleanup,
};
use via_store::{ResumeRecord, SpawnRecord, Store};

const SESSION: &str = "s_0123456789ab";
/// The wall of a turn expected to end by itself.
const WALL: Duration = Duration::from_secs(20);
/// The wall of a turn expected to reach it.
const SHORT_WALL: Duration = Duration::from_millis(1500);
/// C1 P7's window, lowered: these turns' tools never end by themselves.
const TOOL_GRACE: Duration = Duration::from_millis(200);

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

fn hello(version: &str, features: &[&str]) -> Value {
    json!({"action":"hello","message":{"type":"hello","vendor_version":version,"features":features}})
}

fn expect_interrupt(turn: u32) -> Value {
    json!({"action":"expect_request",
           "expected":{"type":"interrupt","id":2,"vendor_turn_id":vendor_turn(turn)}})
}

fn script(turn: u32, steps: &[Value]) -> Value {
    json!({"expected_request":{"type":"start","turn":turn},"steps":steps})
}

/// The persistent-connection profile (decision H1).
fn persistent() -> Value {
    json!({"persistent": true})
}

/// A handshake profile: `1.0` is checked; `turns` is relied on.
fn handshake() -> Value {
    json!({"handshake": {"checked": ["1.0"], "requires": ["turns"]}})
}

/// The default capabilities with `steer` and `effort` native.
fn native_capabilities() -> Value {
    json!({
        "verbs": {"spawn":{"support":"native"},"resume":{"support":"native"},
                  "steer":{"support":"native"},"cancel":{"support":"native"},
                  "close":{"support":"native"}},
        "params": {"instructions":{"support":"unsupported","reason":"no instructions input"},
                   "output_schema":{"support":"unsupported","reason":"no schema input"},
                   "effort":{"support":"native"},
                   "max_steps":{"support":"unsupported","reason":"no step limit"}},
        "bounds": [], "network_control": false,
        "recover": {"support":"unsupported","reason":"no recovery"},
        "usage": {"tokens":"turn","cost":"unavailable"}
    })
}

/// A connection slot whose release the test can see.
struct Slot(Arc<AtomicBool>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// One turn's Core-side controls.
struct Controls {
    stop: watch::Sender<Option<StopOrder>>,
    force: watch::Sender<Option<tokio::time::Instant>>,
    /// The slot Core reserved was released.
    released: Arc<AtomicBool>,
}

impl Controls {
    fn released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }

    fn cancel(&self) {
        let far = Deadline::at(tokio::time::Instant::now() + WALL);
        self.stop.send_replace(Some(StopOrder {
            cause: StopCause::Cancel,
            requested_at: "2026-01-01T00:00:00.000Z".to_owned(),
            force_at: far,
            close_by: far,
        }));
    }
}

/// Core's side of turn `turn`: a slot when the turn opens a connection.
fn turn_cx(turn: u32, prepared: Prepared, wall: Duration) -> (TurnCx, Controls) {
    let released = Arc::new(AtomicBool::new(false));
    let capacity = matches!(prepared, Prepared::NeedsConnection)
        .then(|| Box::new(Slot(Arc::clone(&released))) as via_adapters::CapacityToken);
    let (stop, stop_rx) = watch::channel(None);
    let (force, force_rx) = watch::channel(None);
    let now = tokio::time::Instant::now();
    let cx = TurnCx {
        turn: TurnNumber::try_from(turn).unwrap(),
        prepared,
        capacity,
        activity: TurnActivity::new(now),
        wall: Deadline::at(now + wall),
        tool_grace: TOOL_GRACE,
        stop: stop_rx,
        force: force_rx,
    };
    (
        cx,
        Controls {
            stop,
            force,
            released,
        },
    )
}

/// One fake deployment over a private Store, Host and anchor directory.
struct Rig {
    dir: TempDir,
    set: Option<AdapterSet>,
    _store: Store,
    runtime: tokio::runtime::Runtime,
}

impl Rig {
    /// A deployment whose scenario is `{profile, scripts}` (decision H2).
    fn new(profile: &Value, scripts: &[Value]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for part in ["state", "runtime", "sync"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(dir.path().join(part))
                .unwrap();
        }
        let scenario = dir.path().join("scenario.json");
        fs::write(
            &scenario,
            json!({"profile": profile, "scripts": scripts}).to_string(),
        )
        .unwrap();
        fs::set_permissions(&scenario, fs::Permissions::from_mode(0o600)).unwrap();
        let env = BootstrapEnv::from_vars([
            (
                "VIA_FAKE_AGENT_BINARY",
                binary("via-fake-agent").into_os_string(),
            ),
            ("VIA_FAKE_SCENARIO", scenario.into_os_string()),
            (
                "VIA_FAKE_SYNC_DIR",
                dir.path().join("sync").into_os_string(),
            ),
        ]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        runtime
            .block_on(store.client().commit_spawn(SpawnRecord {
                session_id: SessionId::try_from(SESSION).unwrap(),
                handle_hash: [7_u8; 32],
                receipt: json!({"state":"queued"}),
                params: json!({"harness":"fake"}),
                label: None,
                prompt: "hello".into(),
                effective: json!({"deadlines":{"wall_ms":1}}),
                initial_event: json!({"seq":1,"type":"turn.queued","turn":1,"at":"2026-01-01T00:00:00.000Z"}),
            }))
            .unwrap();
        // Route runs only committed turns: the later ones are queued.
        for turn in 2..=3_u32 {
            runtime
                .block_on(store.client().commit_resume(ResumeRecord {
                    session_id: SessionId::try_from(SESSION).unwrap(),
                    turn: TurnNumber::try_from(turn).unwrap(),
                    prompt: "p".into(),
                    effective: json!({"deadlines":{"wall_ms":1}}),
                    event: json!({"seq":turn,"type":"turn.queued","turn":turn,"at":"2026-01-01T00:00:00.000Z"}),
                    operation: None,
                }))
                .unwrap();
        }
        let set = AdapterSet::new(
            AdapterConfig::load(env, None).unwrap(),
            RuntimeConfig {
                anchor_binary: binary("via"),
                anchor_dir: dir.path().join("runtime"),
            },
            store.runtime_resources(),
        )
        .unwrap();
        Self {
            dir,
            set: Some(set),
            _store: store,
            runtime,
        }
    }

    fn set(&self) -> &AdapterSet {
        self.set.as_ref().unwrap()
    }

    fn synced(&self, name: &str) -> bool {
        self.dir.path().join("sync").join(name).exists()
    }

    /// A logical session with its observation channel (AD3: no vendor I/O).
    fn session(&self) -> (SessionDriver, mpsc::Receiver<Admitted>) {
        let (observations, receiver) = observation_channel();
        let spec = SessionSpec {
            session_id: SessionId::try_from(SESSION).unwrap(),
            model: "fake".to_owned(),
            instructions: None,
            initial_bound: None,
            cwd: self.dir.path().to_path_buf(),
            vendor: via_adapters::VendorOptions::new(),
            inherit: Inherit::OD2_DEFAULT,
            confirmed_vendor_session_id: None,
            allow_untested: false,
        };
        let session = SessionRef {
            harness: "fake".to_owned(),
            route: "fake".to_owned(),
            adapter_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let driver = self
            .set()
            .open_session(&session, spec, SessionCx { observations });
        (driver, receiver)
    }

    /// Runs one turn, draining its observations and calling `on` with each.
    fn run(
        &self,
        driver: &SessionDriver,
        receiver: &mut mpsc::Receiver<Admitted>,
        spec: TurnSpec,
        cx: TurnCx,
        mut on: impl FnMut(&ObservationItem),
    ) -> (TurnEnd, Vec<ObservationItem>) {
        self.runtime.block_on(async {
            let run = driver.run_turn(spec, cx);
            tokio::pin!(run);
            let mut items = Vec::new();
            let end = loop {
                tokio::select! {
                    Some(admitted) = receiver.recv() => {
                        on(&admitted.item);
                        items.push(admitted.item);
                    }
                    end = &mut run => break end,
                }
            };
            while let Ok(admitted) = receiver.try_recv() {
                items.push(admitted.item);
            }
            (checked(end), items)
        })
    }
}

impl Drop for Rig {
    /// Every anchor of the deployment is stopped before its directory goes.
    fn drop(&mut self) {
        if let Some(set) = self.set.take() {
            let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(10));
            self.runtime.block_on(set.shutdown(deadline, &[]));
        }
    }
}

fn prompt() -> TurnSpec {
    TurnSpec {
        prompt: "p".to_owned(),
        ..TurnSpec::default()
    }
}

/// (8) No `TurnEnd` carries `Pending`: cleanup is `Quiescent` or `Uncertain`.
fn checked(end: TurnEnd) -> TurnEnd {
    if let Ok(evidence) = &end.outcome {
        assert_ne!(evidence.cleanup, Cleanup::Pending, "{end:?}");
    }
    end
}

fn failure(end: &TurnEnd) -> &TurnFailure {
    let failure = match &end.outcome {
        Err(TurnError::Route(failure)) => Some(failure),
        Ok(_) | Err(TurnError::Rejected(_) | TurnError::Unavailable) => None,
    };
    assert!(failure.is_some(), "expected a route failure: {end:?}");
    failure.unwrap()
}

/// A steer call in flight beside its turn.
type Steer<'a> = Pin<Box<dyn Future<Output = Result<SteerDelivery, SteerError>> + 'a>>;

fn observations(items: &[ObservationItem]) -> Vec<&Observation> {
    items.iter().map(|item| &item.observation).collect()
}

/// (5) AD6: an unknown vendor stop reason (`tool_use`) is `Other`, kept
/// verbatim beside it; a failed `max_steps` terminal is kept whole.
#[test]
fn conformance_stop_reason_other_and_failed_max_steps_kept() {
    let rig = Rig::new(
        &json!({}),
        &[
            script(1, &[accepted(1), terminal(1, "completed", "tool_use")]),
            script(
                2,
                &[
                    accepted(2),
                    emit(&json!({"type":"terminal","vendor_turn_id":vendor_turn(2),
                                 "status":"failed","final_text":"","stop_reason":"max_steps",
                                 "vendor_code":"step_limit","steps":7})),
                ],
            ),
        ],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    let kept = end.terminal.as_ref().unwrap();
    assert_eq!(kept.stop_reason, StopReason::Other);
    assert_eq!(kept.vendor_stop_reason, "tool_use");
    assert!(end.outcome.is_ok(), "{end:?}");

    let (cx, _controls) = turn_cx(2, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    let kept = end.terminal.as_ref().unwrap();
    assert_eq!(kept.status, VendorTerminalStatus::Failed);
    assert_eq!(kept.stop_reason, StopReason::MaxSteps);
    assert_eq!(kept.vendor_code.as_deref(), Some("step_limit"));
    assert_eq!(kept.steps, Some(7));
}

/// (7) AD4: Core never drains the channel; the decoded terminal is retained
/// in the turn's end although the delivery before it stalled (`overflow`).
#[test]
fn conformance_terminal_retained_under_stalled_observations() {
    let text = json!({"type":"text","vendor_turn_id":vendor_turn(1)}).to_string() + "\n";
    let rig = Rig::new(
        &json!({}),
        &[script(
            1,
            &[
                accepted(1),
                json!({"action":"flood","text":text,"count":OBSERVATION_ITEMS}),
                terminal(1, "completed", "end_turn"),
            ],
        )],
    );
    let (driver, _receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let end = checked(rig.runtime.block_on(driver.run_turn(prompt(), cx)));
    let kept = end.terminal.as_ref().unwrap();
    assert_eq!(kept.status, VendorTerminalStatus::Completed);
    assert!(
        matches!(
            failure(&end).cause,
            TurnCause::Route(RouteError::Overflow { .. })
        ),
        "{end:?}"
    );
}

/// (8) C1 P7 (persistent profile): after the interrupted terminal, a tool
/// still open at the window's bound gives `Uncertain`; one that ended
/// before it gives `Quiescent`.
#[test]
fn conformance_open_tool_at_p7_bound_is_uncertain() {
    let tool = |turn: u32| {
        emit(
            &json!({"type":"tool_started","vendor_turn_id":vendor_turn(turn),
                     "tool_id":"t1","name":"bash"}),
        )
    };
    let rig = Rig::new(
        &persistent(),
        &[
            script(
                1,
                &[
                    accepted(1),
                    tool(1),
                    expect_interrupt(1),
                    terminal(1, "interrupted", "interrupted"),
                    json!({"action":"hang"}),
                ],
            ),
            script(
                2,
                &[
                    accepted(2),
                    tool(2),
                    expect_interrupt(2),
                    emit(&json!({"type":"tool_ended","vendor_turn_id":vendor_turn(2),
                                 "tool_id":"t1"})),
                    terminal(2, "interrupted", "interrupted"),
                ],
            ),
        ],
    );
    let (driver, mut receiver) = rig.session();
    for (turn, expected) in [(1, Cleanup::Uncertain), (2, Cleanup::Quiescent)] {
        let (cx, controls) = turn_cx(turn, Prepared::NeedsConnection, WALL);
        let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |item| {
            if matches!(&item.observation, Observation::Progress(marks) if !marks.tools_started.is_empty())
            {
                controls.cancel();
            }
        });
        assert_eq!(
            end.terminal.as_ref().unwrap().status,
            VendorTerminalStatus::Interrupted
        );
        let evidence = end.outcome.as_ref().unwrap();
        assert_eq!(evidence.cleanup, expected, "turn {turn}");
    }
}

/// (11) Steer is delivered on a native profile and refused on one that
/// declares it unsupported; with no turn running it is `NoActiveTurn`.
#[test]
fn conformance_steer_native_delivered_unsupported_refused() {
    let rig = Rig::new(
        &json!({"capabilities": native_capabilities()}),
        &[script(
            1,
            &[
                accepted(1),
                json!({"action":"expect_request","expected":{"type":"steer","id":3}}),
                emit(&json!({"type":"steer_delivered","id":3,"vendor_turn_id":vendor_turn(1)})),
                terminal(1, "completed", "end_turn"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let idle = rig.runtime.block_on(driver.steer(SteerInput {
        text: "early".to_owned(),
    }));
    assert_eq!(idle.unwrap_err(), SteerError::NoActiveTurn);

    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (answer, end, items) = rig.runtime.block_on(async {
        let run = driver.run_turn(prompt(), cx);
        tokio::pin!(run);
        let mut steer: Option<Steer<'_>> = None;
        let mut answer = None;
        let mut items = Vec::new();
        let end = loop {
            tokio::select! {
                Some(admitted) = receiver.recv() => {
                    if matches!(admitted.item.observation, Observation::Accepted(_)) {
                        steer = Some(Box::pin(driver.steer(SteerInput { text: "also \"this\"".to_owned() })));
                    }
                    items.push(admitted.item);
                }
                result = async { steer.as_mut().unwrap().await }, if steer.is_some() => {
                    answer = Some(result);
                    steer = None;
                }
                end = &mut run => break end,
            }
        };
        while let Ok(admitted) = receiver.try_recv() {
            items.push(admitted.item);
        }
        (answer, checked(end), items)
    });
    assert_eq!(answer, Some(Ok(SteerDelivery::Injected)), "{end:?}");
    assert!(end.outcome.is_ok(), "{end:?}");
    assert!(
        observations(&items)
            .iter()
            .any(|observation| matches!(observation, Observation::SteerDelivered(_)))
    );

    let rig = Rig::new(&json!({}), &[]);
    let (driver, _receiver) = rig.session();
    let refused = rig.runtime.block_on(driver.steer(SteerInput {
        text: "no".to_owned(),
    }));
    assert_eq!(refused.unwrap_err(), SteerError::Unsupported);
}

/// (16) AD7: once the handshake was read, `TurnEnd.instance` is set on
/// protocol failure, overflow and the daemon force; before any handshake
/// it is null.
#[test]
fn conformance_instance_set_after_handshake_on_failures() {
    let big = "x".repeat(1024);
    let rig = Rig::new(
        &handshake(),
        &[
            script(
                1,
                &[
                    hello("1.0", &["turns"]),
                    accepted(1),
                    json!({"action":"emit_raw","text":"not json\n"}),
                    json!({"action":"hang"}),
                ],
            ),
            script(
                2,
                &[
                    accepted(2),
                    json!({"action":"flood","text":big,"count":1100}),
                    json!({"action":"hang"}),
                ],
            ),
            script(3, &[accepted(3), json!({"action":"gate","name":"forced"})]),
        ],
    );
    let (driver, mut receiver) = rig.session();
    let mut cause = |turn: u32| {
        let (cx, controls) = turn_cx(turn, driver.prepare(), WALL);
        let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |item| {
            if turn == 3 && matches!(item.observation, Observation::Accepted(_)) {
                controls
                    .force
                    .send_replace(Some(tokio::time::Instant::now()));
            }
        });
        let instance = end.instance.as_ref().unwrap();
        assert_eq!(instance.vendor_version.as_deref(), Some("1.0"));
        assert_eq!(instance.version_status, VersionStatus::Tested);
        failure(&end).cause.clone()
    };
    assert!(matches!(
        cause(1),
        TurnCause::Route(RouteError::Protocol { .. })
    ));
    assert!(matches!(
        cause(2),
        TurnCause::Route(RouteError::Overflow { .. })
    ));
    assert!(matches!(
        cause(3),
        TurnCause::Route(RouteError::ForceStopped { .. })
    ));

    // No handshake ever arrives: the wall ends the turn with none read.
    let rig = Rig::new(&handshake(), &[script(1, &[json!({"action":"hang"})])]);
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), SHORT_WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.instance.is_none(), "{end:?}");
    assert!(matches!(
        failure(&end).cause,
        TurnCause::Route(RouteError::Deadline { .. })
    ));
}

/// (16), (17) Persistent profile: the server's exit before a terminal is
/// `ServerLost`; a live server whose stdout closed is transport loss. Each
/// keeps the handshake's instance and releases the connection's slot.
#[test]
fn conformance_server_lost_versus_transport_lost() {
    let mut profile = persistent();
    profile["handshake"] = handshake()["handshake"].clone();
    let rig = Rig::new(
        &profile,
        &[
            script(
                1,
                &[
                    hello("1.1", &["turns"]),
                    accepted(1),
                    json!({"action":"exit","code":3}),
                ],
            ),
            script(
                2,
                &[accepted(2), json!({"action":"close_stdout","name":"lost"})],
            ),
        ],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    let lost = failure(&end);
    assert!(
        matches!(lost.cause, TurnCause::ServerLost { .. }),
        "{end:?}"
    );
    assert!(lost.shared);
    assert_eq!(
        end.instance.as_ref().unwrap().version_status,
        VersionStatus::Untested
    );
    assert!(controls.released(), "the lost server's slot is released");
    assert!(matches!(driver.prepare(), Prepared::NeedsConnection));

    let (cx, controls) = turn_cx(2, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(
        matches!(
            failure(&end).cause,
            TurnCause::Route(RouteError::TransportLost { .. })
        ),
        "{end:?}"
    );
    assert!(end.instance.is_some());
    assert!(rig.synced("lost.entered"), "the server outlived its stdout");
    assert!(controls.released());
    assert!(matches!(driver.prepare(), Prepared::NeedsConnection));
}

/// (18) AD18 run half: an effort outside the compiled table is rejected
/// `invalid_params(effort)` before anything launches. AD7: a handshake
/// missing a relied-on feature is `HandshakeRefused`; no start was written.
#[test]
fn conformance_invalid_effort_and_handshake_refused_submit_nothing() {
    let rig = Rig::new(
        &json!({"capabilities": native_capabilities(), "efforts": ["low", "high"]}),
        &[script(1, &[json!({"action":"report_pids"})])],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let spec = TurnSpec {
        effort: Some("turbo".to_owned()),
        ..prompt()
    };
    let (end, items) = rig.run(&driver, &mut receiver, spec, cx, |_| {});
    assert!(
        matches!(
            end.outcome,
            Err(TurnError::Rejected(StartRejected::InvalidParam {
                field: "effort"
            }))
        ),
        "{end:?}"
    );
    assert!(end.terminal.is_none() && end.instance.is_none() && items.is_empty());
    assert!(controls.released() && !rig.synced("agent.pid"));

    let rig = Rig::new(
        &json!({"handshake": {"requires": ["turns", "steer"]}}),
        &[script(
            1,
            &[hello("2.0", &["turns"]), json!({"action":"report_pids"})],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(
        matches!(failure(&end).cause, TurnCause::HandshakeRefused { .. }),
        "{end:?}"
    );
    let instance = end.instance.as_ref().unwrap();
    assert_eq!(instance.vendor_version.as_deref(), Some("2.0"));
    assert!(items.is_empty());
    assert!(!rig.synced("agent.pid"), "no start reached the vendor");
}

/// (20) AD4 run half (persistent profile): at the wall, the interrupt is
/// sent before `run_turn` returns; an interrupted terminal after it is
/// reported `acknowledged`, the emulated server never force-stopped. With
/// no answer, it is not acknowledged and cleanup is `Uncertain`.
#[test]
fn conformance_wall_soft_stop_interrupt_acknowledged() {
    let rig = Rig::new(
        &persistent(),
        &[
            script(
                1,
                &[
                    accepted(1),
                    expect_interrupt(1),
                    terminal(1, "interrupted", "interrupted"),
                ],
            ),
            script(2, &[accepted(2), json!({"action":"hang"})]),
        ],
    );
    let (driver, mut receiver) = rig.session();
    for (turn, acknowledged, cleanup) in [
        (1, true, WireCleanup::Quiescent),
        (2, false, WireCleanup::Uncertain),
    ] {
        let (cx, _controls) = turn_cx(turn, driver.prepare(), SHORT_WALL);
        let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
        let failure = failure(&end);
        assert!(
            matches!(failure.cause, TurnCause::Route(RouteError::Deadline { .. })),
            "{end:?}"
        );
        assert_eq!(failure.acknowledged, acknowledged, "turn {turn}");
        assert_eq!(failure.cleanup, Some(cleanup), "turn {turn}");
        assert!(!failure.forced && failure.shared);
        // The acknowledging terminal ends nothing: the turn failed at the wall.
        assert!(end.terminal.is_none());
    }
}

/// Persistent profile (decision H1, AD16): the driver holds the slot
/// between turns and pins the live connection; close releases it, and a
/// turn after close is rejected with nothing sent.
#[test]
fn conformance_persistent_slot_pinned_between_turns() {
    let rig = Rig::new(
        &persistent(),
        &[
            script(1, &[accepted(1), terminal(1, "completed", "end_turn")]),
            script(2, &[accepted(2), terminal(2, "completed", "end_turn")]),
        ],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, first) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    assert!(!first.released(), "the driver holds the slot between turns");
    let pinned = driver.prepare();
    assert!(matches!(pinned, Prepared::Pinned(_)));
    let (cx, _second) = turn_cx(2, pinned, WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    assert!(!first.released());

    let deadline = Deadline::at(tokio::time::Instant::now() + WALL);
    let report = rig
        .runtime
        .block_on(driver.close(via_adapters::CloseMode::Graceful, deadline));
    assert!(report.vendor_closed);
    assert!(first.released(), "close releases the held slot");
    let (cx, _third) = turn_cx(3, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(matches!(
        end.outcome,
        Err(TurnError::Rejected(StartRejected::SessionGone))
    ));
}

/// C2 §4: the vendor closing its session after a turn's terminal is a
/// session-level `VendorClosed` (no vendor turn); the connection is gone,
/// so its slot is released and the next turn opens a new one.
#[test]
fn conformance_vendor_closed_between_turns_is_session_level() {
    let rig = Rig::new(
        &persistent(),
        &[script(
            1,
            &[
                accepted(1),
                terminal(1, "completed", "end_turn"),
                emit(&json!({"type":"vendor_closed","reason":"idle_timeout"})),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    let closed = items
        .iter()
        .find(|item| matches!(&item.observation, Observation::VendorClosed(reason) if reason == "idle_timeout"))
        .unwrap();
    assert!(closed.vendor_turn.is_none(), "session-level");
    assert!(controls.released());
    assert!(matches!(driver.prepare(), Prepared::NeedsConnection));
}

/// C2 §2 Recover: the fake never resumes. Host's proof that every anchor
/// of the session is gone is `Dead`; anything less is `Unknown`.
#[test]
fn conformance_recover_with_host_death_facts_is_dead() {
    let rig = Rig::new(&json!({}), &[]);
    let fact = |cleanup| AnchorRecovery {
        session_id: SessionId::try_from(SESSION).unwrap(),
        anchor_id: "anchor-1".to_owned(),
        generation: "1".to_owned(),
        turn: TurnNumber::try_from(1).unwrap(),
        cleanup,
        forced: false,
    };
    let session = SessionRef {
        harness: "fake".to_owned(),
        route: "fake".to_owned(),
        adapter_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let recover = |facts: &[AnchorRecovery]| {
        let (observations, _receiver) = observation_channel();
        rig.runtime.block_on(
            rig.set()
                .recover(&session, facts, SessionCx { observations }),
        )
    };
    assert!(matches!(
        recover(&[fact(Cleanup::Quiescent)]),
        Recovery::Dead { .. }
    ));
    assert!(matches!(
        recover(&[fact(Cleanup::Quiescent), fact(Cleanup::Uncertain)]),
        Recovery::Unknown { .. }
    ));
    assert!(matches!(recover(&[]), Recovery::Unknown { .. }));
}
