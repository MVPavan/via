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
    AdapterError, Admitted, Observation, ObservationItem, SteerDelivery, SteerReceipt, StopReason,
    TurnEnd, observation_channel,
};
use via_adapters::{
    AdapterConfig, AdapterSet, AnchorRecovery, BootstrapEnv, CancellationToken, Cleanup, CloseMode,
    Deadline, DriverFailure, DriverHealth, Inherit, InheritPlan, OBSERVATION_ITEMS, Prepared,
    Recovery, RouteError, RouteFailure, RuntimeConfig, SessionCx, SessionDriver, SessionId,
    SessionRef, SessionSpec, StartRejected, SteerError, SteerInput, StopCause, StopOrder,
    TaskTracker, TurnActivity, TurnCx, TurnNumber, TurnSpec, VendorTerminalStatus, VendorTurnId,
    VersionStatus, WireCleanup,
};
use via_store::{ResumeRecord, SpawnRecord, Store};

const SESSION: &str = "s_0123456789ab";
/// The wall of a turn expected to end by itself.
const WALL: Duration = Duration::from_secs(20);
/// The wall of a turn expected to reach it.
const SHORT_WALL: Duration = Duration::from_millis(1500);
/// C1 P7's window, lowered: these turns' tools never end by themselves.
const TOOL_GRACE: Duration = Duration::from_millis(200);
/// Turns the Store holds, all queued after the first.
const TURNS: u32 = 6;
/// A bound on a test's wait for a fixture event.
const FIXTURE_WAIT: Duration = Duration::from_secs(10);

/// What a turn's drain loop has seen, for side actions keyed on it.
#[derive(Clone, Copy, Debug, Default)]
struct Seen {
    accepted: bool,
    tool: bool,
}

impl Seen {
    fn note(&mut self, observation: &Observation) {
        if matches!(observation, Observation::Accepted(_)) {
            self.accepted = true;
        } else if let Observation::Progress(marks) = observation
            && !marks.tools_started.is_empty()
        {
            self.tool = true;
        }
    }
}

/// The failpoint token of [`Rig::points`].
#[cfg(feature = "test-failpoints")]
const POINTS_TOKEN: &str = "conformance-driver";

/// Arms `point`'s first hit to be acknowledged only. A `value` command at
/// a point that reads none continues at once, without yielding: by the
/// time its task is next idle, it waits past the point.
#[cfg(feature = "test-failpoints")]
fn acknowledge(points: &std::path::Path, point: &str) {
    let command = json!({"token":POINTS_TOKEN,"occurrence":1,"action":"value","value":0});
    fs::write(points.join(format!("{point}.json")), command.to_string()).unwrap();
}

/// Waits until `path` exists, within [`FIXTURE_WAIT`].
async fn until_file(path: PathBuf) {
    let by = tokio::time::Instant::now() + FIXTURE_WAIT;
    while !path.exists() {
        assert!(
            tokio::time::Instant::now() < by,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits until `seen` holds, within [`FIXTURE_WAIT`].
async fn until_seen(seen: &mut watch::Receiver<Seen>, what: fn(&Seen) -> bool) {
    let waited = tokio::time::timeout(FIXTURE_WAIT, seen.wait_for(what)).await;
    assert!(matches!(waited, Ok(Ok(_))), "never seen");
}

/// Releases the fake agent's gate `name`.
fn release(sync: &std::path::Path, name: &str) {
    fs::write(sync.join(format!("{name}.release")), b"").unwrap();
}

fn raw(text: &str) -> Value {
    json!({"action":"emit_raw","text":text})
}

fn hang() -> Value {
    json!({"action":"hang"})
}

fn gate(name: &str) -> Value {
    json!({"action":"gate","name":name})
}

fn hold(name: &str) -> Value {
    json!({"action":"hold_stdin","name":name})
}

/// One line over Wire's 1 MiB message cap: `overflow`.
fn oversized() -> Value {
    json!({"action":"flood","text":"x".repeat(1024),"count":1100})
}

fn tool_started(turn: u32) -> Value {
    emit(
        &json!({"type":"tool_started","vendor_turn_id":vendor_turn(turn),
                 "tool_id":"t1","name":"bash"}),
    )
}

fn tool_ended(turn: u32) -> Value {
    emit(&json!({"type":"tool_ended","vendor_turn_id":vendor_turn(turn),"tool_id":"t1"}))
}

fn identity(id: &str) -> Value {
    emit(&json!({"type":"identity","vendor_session_id":id}))
}

/// A stop order whose `force_at` is `after` from now.
fn order(after: Duration) -> StopOrder {
    let force_at = Deadline::at(tokio::time::Instant::now() + after);
    StopOrder {
        cause: StopCause::Cancel,
        requested_at: "2026-01-01T00:00:00.000Z".to_owned(),
        force_at,
        close_by: Deadline::at(force_at.instant() + Duration::from_secs(3)),
    }
}

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
    turn_cx_with(turn, prepared, wall, TOOL_GRACE)
}

/// [`turn_cx`] with C1 P7's window `tool_grace`.
fn turn_cx_with(
    turn: u32,
    prepared: Prepared,
    wall: Duration,
    tool_grace: Duration,
) -> (TurnCx, Controls) {
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
        tool_grace,
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
    /// The sessions' owned tasks (C2 §2 `SessionCx`).
    tracker: TaskTracker,
    /// The sessions' cancellation.
    cancel: CancellationToken,
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
        for turn in 2..=TURNS {
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
            tracker: TaskTracker::new(),
            cancel: CancellationToken::new(),
        }
    }

    fn set(&self) -> &AdapterSet {
        self.set.as_ref().unwrap()
    }

    fn synced(&self, name: &str) -> bool {
        self.sync().join(name).exists()
    }

    /// The fake agent's synchronization directory.
    fn sync(&self) -> PathBuf {
        self.dir.path().join("sync")
    }

    /// This process's failpoint directory, the controller activated on it
    /// (once per process: each test runs in its own).
    #[cfg(feature = "test-failpoints")]
    fn points(&self) -> PathBuf {
        let points = self.dir.path().join("points");
        fs::DirBuilder::new().mode(0o700).create(&points).unwrap();
        via_store::failpoint::activate(&points, POINTS_TOKEN).unwrap();
        points
    }

    /// A logical session with its observation channel (AD3: no vendor I/O).
    fn session(&self) -> (SessionDriver, mpsc::Receiver<Admitted>) {
        self.session_with(|_| {})
    }

    /// [`Self::session`] with `edit` applied to its spec.
    fn session_with(
        &self,
        edit: impl FnOnce(&mut SessionSpec),
    ) -> (SessionDriver, mpsc::Receiver<Admitted>) {
        let (observations, receiver) = observation_channel();
        let mut spec = SessionSpec {
            session_id: SessionId::try_from(SESSION).unwrap(),
            model: "fake".to_owned(),
            instructions: None,
            initial_bound: None,
            cwd: self.dir.path().to_path_buf(),
            vendor: via_adapters::VendorOptions::new(),
            inherit: InheritPlan {
                requested: Inherit::OD2_DEFAULT,
                effective: Inherit::OD2_DEFAULT,
            },
            confirmed_vendor_session_id: None,
            allow_untested: false,
        };
        edit(&mut spec);
        let session = SessionRef {
            harness: "fake".to_owned(),
            route: "fake".to_owned(),
            adapter_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let driver = self
            .set()
            .open_session(&session, spec, self.session_cx(observations));
        (driver, receiver)
    }

    /// The session context Core attaches.
    fn session_cx(&self, observations: via_adapters::observation::ObservationSink) -> SessionCx {
        SessionCx {
            observations,
            tracker: self.tracker.clone(),
            cancel: self.cancel.clone(),
        }
    }

    /// Runs one turn beside `side`, which may act on what the drain loop
    /// has seen; returns both results.
    fn run_beside<T, F: Future<Output = T>>(
        &self,
        driver: &SessionDriver,
        receiver: &mut mpsc::Receiver<Admitted>,
        (spec, cx): (TurnSpec, TurnCx),
        side: impl FnOnce(watch::Receiver<Seen>) -> F,
    ) -> (TurnEnd, Vec<ObservationItem>, T) {
        self.runtime.block_on(async {
            let (seen_tx, seen_rx) = watch::channel(Seen::default());
            let run = async {
                let run = driver.run_turn(spec, cx);
                tokio::pin!(run);
                let mut items = Vec::new();
                let end = loop {
                    tokio::select! {
                        Some(admitted) = receiver.recv() => {
                            seen_tx.send_modify(|seen| seen.note(&admitted.item.observation));
                            items.push(admitted.item);
                        }
                        end = &mut run => break end,
                    }
                };
                while let Ok(admitted) = receiver.try_recv() {
                    items.push(admitted.item);
                }
                (checked(end), items)
            };
            let ((end, items), out) = tokio::join!(run, side(seen_rx));
            (end, items, out)
        })
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
        // The sessions' owned work ends first, within a bound.
        self.cancel.cancel();
        self.tracker.close();
        let joined = self
            .runtime
            .block_on(async { tokio::time::timeout(FIXTURE_WAIT, self.tracker.wait()).await });
        assert!(joined.is_ok(), "owned tasks outlived the rig");
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

fn failure(end: &TurnEnd) -> &RouteFailure {
    let failure = if let Err(AdapterError::Route(failure)) = &end.outcome {
        Some(failure)
    } else {
        None
    };
    assert!(failure.is_some(), "expected a route failure: {end:?}");
    failure.unwrap()
}

/// A steer call in flight beside its turn.
type Steer<'a> = Pin<Box<dyn Future<Output = Result<SteerReceipt, SteerError>> + 'a>>;

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

/// In the parent, runs test `name` again in a child with the stall bound
/// lowered to 250 ms, which test-failpoint builds honour (others keep
/// 10 s), and returns true once it passed; in the child, returns false.
fn rerun_with_short_stall(name: &str) -> bool {
    const CHILD: &str = "VIA_CONFORMANCE_STALL_CHILD";
    if env::var_os(CHILD).is_some() {
        return false;
    }
    let status = std::process::Command::new(env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, "1")
        .env("VIA_TEST_EVENT_STALL_MS", "250")
        .status()
        .unwrap();
    assert!(status.success(), "child failed: {status}");
    true
}

/// A flood of `count` model marks for `turn`, in two halves: the second
/// waits at gate `flood` until [`release_flood`] saw the first in the
/// channel. Wire's 1024-message queue then never holds a channel's worth,
/// whatever Route's reading pace under load (a full queue is `overflow`).
fn staged_flood(turn: u32, count: usize) -> Vec<Value> {
    let text = json!({"type":"text","vendor_turn_id":vendor_turn(turn)}).to_string() + "\n";
    vec![
        json!({"action":"flood","text":text,"count":count / 2}),
        gate("flood"),
        json!({"action":"flood","text":text,"count":count - count / 2}),
    ]
}

/// Releases gate `flood` once the undrained `receiver` holds `items`.
async fn release_flood(sync: &std::path::Path, receiver: &mpsc::Receiver<Admitted>, items: usize) {
    let by = tokio::time::Instant::now() + FIXTURE_WAIT;
    while receiver.len() < items {
        assert!(
            tokio::time::Instant::now() < by,
            "the flood's first half never arrived"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    release(sync, "flood");
}

/// (7) AD4: Core never drains the channel; the decoded terminal is retained
/// in the turn's end although the delivery before it stalled (`overflow`).
/// It runs again in a child with the stall bound lowered to 250 ms, which
/// test-failpoint builds honour (others keep 10 s).
#[test]
fn conformance_terminal_retained_under_stalled_observations() {
    if rerun_with_short_stall("conformance_terminal_retained_under_stalled_observations") {
        return;
    }
    let steps = [
        vec![accepted(1)],
        staged_flood(1, OBSERVATION_ITEMS),
        vec![terminal(1, "completed", "end_turn")],
    ]
    .concat();
    let rig = Rig::new(&json!({}), &[script(1, &steps)]);
    let (driver, receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let sync = rig.sync();
    let end = checked(rig.runtime.block_on(async {
        let release = release_flood(&sync, &receiver, 1 + OBSERVATION_ITEMS / 2);
        tokio::join!(driver.run_turn(prompt(), cx), release).0
    }));
    let kept = end.terminal.as_ref().unwrap();
    assert_eq!(kept.status, VendorTerminalStatus::Completed);
    assert!(
        matches!(failure(&end).cause, RouteError::Overflow { .. }),
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
        turn: TurnNumber::try_from(1).unwrap(),
        text: "early".to_owned(),
        expected_vendor_turn: None,
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
                        steer = Some(Box::pin(driver.steer(SteerInput { turn: TurnNumber::try_from(1).unwrap(), text: "also \"this\"".to_owned(), expected_vendor_turn: Some(VendorTurnId::try_from(vendor_turn(1)).unwrap()) })));
                    }
                    items.push(admitted.item);
                }
                result = async { steer.as_mut().unwrap().await }, if steer.is_some() => {
                    // C2 §2 `SteerReceipt` (critical r1 #5): the observation
                    // carrying the receipt's token was emitted first.
                    while let Ok(admitted) = receiver.try_recv() {
                        items.push(admitted.item);
                    }
                    if let Ok(receipt) = &result {
                        assert!(items.iter().any(|item| matches!(
                            &item.observation,
                            Observation::SteerDelivered { token, .. } if *token == receipt.token
                        )));
                    }
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
    let answer = answer.map(|answer| answer.map(|receipt| receipt.delivery));
    assert_eq!(answer, Some(Ok(SteerDelivery::Injected)), "{end:?}");
    assert!(end.outcome.is_ok(), "{end:?}");
    assert!(
        observations(&items)
            .iter()
            .any(|observation| matches!(observation, Observation::SteerDelivered { .. }))
    );

    let rig = Rig::new(&json!({}), &[]);
    let (driver, _receiver) = rig.session();
    let refused = rig.runtime.block_on(driver.steer(SteerInput {
        turn: TurnNumber::try_from(1).unwrap(),
        text: "no".to_owned(),
        expected_vendor_turn: None,
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
    assert!(matches!(cause(1), RouteError::Protocol { .. }));
    assert!(matches!(cause(2), RouteError::Overflow { .. }));
    assert!(matches!(cause(3), RouteError::ForceStopped { .. }));

    // No handshake ever arrives: the wall ends the turn with none read.
    let rig = Rig::new(&handshake(), &[script(1, &[json!({"action":"hang"})])]);
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), SHORT_WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.instance.is_none(), "{end:?}");
    assert!(matches!(failure(&end).cause, RouteError::Deadline { .. }));
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
        matches!(lost.cause, RouteError::ServerLost { .. }),
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
        matches!(failure(&end).cause, RouteError::TransportLost { .. }),
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
            Err(AdapterError::Rejected {
                reason: StartRejected::InvalidParam { field: "effort" },
                ..
            })
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
        matches!(failure(&end).cause, RouteError::HandshakeRefused { .. }),
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
            matches!(failure.cause, RouteError::Deadline { .. }),
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
    // No vendor evidence of a close: the fake's server never said so.
    assert!(!report.vendor_closed);
    assert!(first.released(), "close releases the held slot");
    let (cx, _third) = turn_cx(3, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(matches!(
        end.outcome,
        Err(AdapterError::Rejected {
            reason: StartRejected::SessionGone,
            ..
        })
    ));
}

/// Sol r2 #8 (C2 §4 `turn.accepted`, C1 §3.7): on a persistent
/// connection every acceptance carries the connection's handshake. The
/// second turn runs on the pinned connection whose handshake was read for
/// the first; its acceptance still reports `1.0`, tested. The fake stands
/// in for the server with one process per turn, each emitting the
/// scenario's one handshake, which Route reads before each start.
#[test]
fn conformance_persistent_acceptances_carry_the_connection_handshake() {
    let mut profile = persistent();
    profile["handshake"] = handshake()["handshake"].clone();
    let rig = Rig::new(
        &profile,
        &[
            script(
                1,
                &[
                    hello("1.0", &["turns"]),
                    accepted(1),
                    terminal(1, "completed", "end_turn"),
                ],
            ),
            script(2, &[accepted(2), terminal(2, "completed", "end_turn")]),
        ],
    );
    let instance = |items: &[ObservationItem]| {
        items.iter().find_map(|item| {
            if let Observation::Accepted(acceptance) = &item.observation {
                Some(acceptance.instance.clone())
            } else {
                None
            }
        })
    };
    let (driver, mut receiver) = rig.session();
    let (cx, _first) = turn_cx(1, driver.prepare(), WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    let first = instance(&items).expect("turn 1 was accepted").unwrap();
    assert_eq!(
        (first.vendor_version.as_deref(), first.version_status),
        (Some("1.0"), VersionStatus::Tested)
    );
    let pinned = driver.prepare();
    assert!(matches!(pinned, Prepared::Pinned(_)));
    let (cx, _second) = turn_cx(2, pinned, WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    assert_eq!(
        instance(&items).expect("turn 2 was accepted"),
        Some(first),
        "turn 2's acceptance carries the connection's handshake"
    );
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
                .recover(&session, facts, rig.session_cx(observations)),
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

/// The wall of a turn that a close or a stop must end first.
const CLOSE_WALL: Duration = Duration::from_secs(4);
/// A close's deadline.
const CLOSE_WITHIN: Duration = Duration::from_secs(2);

/// When a close-during-turn case closes.
#[derive(Clone, Copy)]
enum Moment {
    /// Once the fake agent entered gate (or hold) `name`.
    Entered(&'static str),
    /// Once a tool start was observed.
    Tool,
}

/// C2 §2 Close: closing the session while turn 1 runs `steps` drives that
/// turn's own stop path, on the per-turn and the persistent profile. The
/// slot stays held until cleanup settled, and the report carries only
/// established facts. With `no_input`, nothing reached the vendor.
fn close_during(steps: &[Value], moment: Moment, no_input: bool, mode: CloseMode) {
    for persistent in [false, true] {
        let mut profile = handshake();
        profile["persistent"] = json!(persistent);
        let rig = Rig::new(&profile, &[script(1, steps)]);
        let (driver, mut receiver) = rig.session();
        let (cx, controls) = turn_cx(1, driver.prepare(), CLOSE_WALL);
        let sync = rig.sync();
        let driver_ref = &driver;
        let released = Arc::clone(&controls.released);
        let (end, _, (held, report)) = rig.run_beside(
            &driver,
            &mut receiver,
            (prompt(), cx),
            |mut seen| async move {
                match moment {
                    Moment::Entered(name) => until_file(sync.join(format!("{name}.entered"))).await,
                    Moment::Tool => until_seen(&mut seen, |seen| seen.tool).await,
                }
                let held = !released.load(Ordering::SeqCst);
                let deadline = Deadline::at(tokio::time::Instant::now() + CLOSE_WITHIN);
                (held, driver_ref.close(mode, deadline).await)
            },
        );
        let case = format!("persistent={persistent}");
        assert!(held, "{case}: the slot is held while the turn runs");
        assert!(
            matches!(failure(&end).cause, RouteError::Stopped { .. }),
            "{case}: {end:?}"
        );
        assert!(
            !report.vendor_closed,
            "{case}: no vendor evidence of a close"
        );
        assert_eq!(report.cleanup, Cleanup::Quiescent, "{case}: {report:?}");
        assert!(report.process_exit.is_some(), "{case}: {report:?}");
        assert!(controls.released(), "{case}: released once cleanup settled");
        assert_eq!(*driver.health().borrow(), DriverHealth::Closed, "{case}");
        if no_input {
            assert!(!rig.synced("first-input"), "{case}: nothing was written");
        }
    }
}

#[test]
fn close_while_awaiting_the_handshake_stops_the_turn_without_input() {
    close_during(
        &[
            hold("h"),
            hello("1.0", &["turns"]),
            accepted(1),
            terminal(1, "completed", "end_turn"),
        ],
        Moment::Entered("h"),
        true,
        CloseMode::Graceful,
    );
}

#[test]
fn close_while_awaiting_acceptance_stops_the_turn() {
    close_during(
        &[hello("1.0", &["turns"]), gate("wait")],
        Moment::Entered("wait"),
        false,
        CloseMode::Graceful,
    );
}

/// A tool open in turn 1, on the C2 §2 Close cases.
fn open_tool_steps() -> [Value; 4] {
    [
        hello("1.0", &["turns"]),
        accepted(1),
        tool_started(1),
        gate("tool"),
    ]
}

#[test]
fn close_during_an_open_tool_stops_the_turn() {
    close_during(&open_tool_steps(), Moment::Tool, false, CloseMode::Graceful);
}

/// C2 §2 Close(Force): the same stop path, forced at once.
#[test]
fn force_close_during_an_open_tool_stops_the_turn() {
    close_during(&open_tool_steps(), Moment::Tool, false, CloseMode::Force);
}

/// C2 §2 Close with no active turn: cleanup is the last helper's
/// retirement, `vendor_closed` needs vendor evidence, and no process exit
/// belongs to this close.
#[test]
fn an_idle_close_reports_the_last_retirement_and_no_vendor_close() {
    let rig = Rig::new(
        &json!({}),
        &[script(
            1,
            &[accepted(1), terminal(1, "completed", "end_turn")],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    let deadline = Deadline::at(tokio::time::Instant::now() + CLOSE_WITHIN);
    let report = rig
        .runtime
        .block_on(driver.close(CloseMode::Graceful, deadline));
    assert!(!report.vendor_closed, "{report:?}");
    assert_eq!(report.cleanup, Cleanup::Quiescent, "{report:?}");
    assert!(report.process_exit.is_none(), "{report:?}");
}

/// AD16: a persistent generation that failed (protocol, overflow, the
/// daemon force) releases its slot and leaves nothing pinnable.
#[test]
fn failed_persistent_generations_release_their_slot() {
    let rig = Rig::new(
        &persistent(),
        &[
            script(1, &[accepted(1), raw("not json\n"), hang()]),
            script(2, &[accepted(2), oversized(), hang()]),
            script(3, &[accepted(3), gate("forced")]),
        ],
    );
    let (driver, mut receiver) = rig.session();
    for turn in 1..=3 {
        let (cx, controls) = turn_cx(turn, driver.prepare(), WALL);
        let force = &controls.force;
        let (end, _, ()) = rig.run_beside(
            &driver,
            &mut receiver,
            (prompt(), cx),
            |mut seen| async move {
                if turn == 3 {
                    until_seen(&mut seen, |seen| seen.accepted).await;
                    force.send_replace(Some(tokio::time::Instant::now()));
                }
            },
        );
        assert!(end.outcome.is_err(), "turn {turn}: {end:?}");
        assert!(controls.released(), "turn {turn}: slot released");
        assert!(
            matches!(driver.prepare(), Prepared::NeedsConnection),
            "turn {turn}: nothing pinnable"
        );
    }
}

/// AD16: a persistent turn whose acquisition failed, or whose handshake
/// was refused, releases its slot and leaves nothing pinnable.
#[test]
fn unlaunched_and_refused_persistent_generations_release_their_slot() {
    let rig = Rig::new(
        &persistent(),
        &[script(
            1,
            &[accepted(1), terminal(1, "completed", "end_turn")],
        )],
    );
    let missing = rig.sync().join("missing");
    let (driver, mut receiver) = rig.session_with(|spec| spec.cwd = missing);
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_err(), "{end:?}");
    assert!(controls.released(), "acquisition failure releases the slot");
    assert!(matches!(driver.prepare(), Prepared::NeedsConnection));

    let mut profile = persistent();
    profile["handshake"] = json!({"requires": ["turns", "steer"]});
    let rig = Rig::new(&profile, &[script(1, &[hello("1.0", &["turns"])])]);
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(
        matches!(failure(&end).cause, RouteError::HandshakeRefused { .. }),
        "{end:?}"
    );
    assert!(controls.released(), "a refused handshake releases the slot");
    assert!(matches!(driver.prepare(), Prepared::NeedsConnection));
}

/// A dropped `run_turn` future leaves the launched turn's cleanup owned:
/// it completes, then the slot is released and nothing is pinnable.
#[test]
fn a_dropped_turn_keeps_its_cleanup_owned_then_releases_the_slot() {
    let rig = Rig::new(&persistent(), &[script(1, &[accepted(1), gate("held")])]);
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let ended_early = rig.runtime.block_on(async {
        let run = driver.run_turn(prompt(), cx);
        tokio::pin!(run);
        loop {
            tokio::select! {
                Some(admitted) = receiver.recv() => {
                    if matches!(admitted.item.observation, Observation::Accepted(_)) {
                        break None;
                    }
                }
                end = &mut run => break Some(end),
            }
        }
    });
    assert!(ended_early.is_none(), "{ended_early:?}");
    rig.runtime.block_on(async {
        let by = tokio::time::Instant::now() + FIXTURE_WAIT;
        while !controls.released() && tokio::time::Instant::now() < by {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    assert!(controls.released(), "the owned cleanup released the slot");
    assert!(matches!(driver.prepare(), Prepared::NeedsConnection));
}

/// Design §2 rule 2: a stop order that arrives while Route awaits the
/// handshake closes the turn before submission: no interrupt and no start
/// reach the vendor.
#[test]
fn a_stop_during_the_handshake_writes_no_start() {
    let rig = Rig::new(
        &handshake(),
        &[script(
            1,
            &[
                hold("h"),
                hello("1.0", &["turns"]),
                accepted(1),
                terminal(1, "completed", "end_turn"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), CLOSE_WALL);
    let sync = rig.sync();
    let stop = &controls.stop;
    let (end, _, ()) = rig.run_beside(&driver, &mut receiver, (prompt(), cx), |_| async move {
        until_file(sync.join("h.entered")).await;
        stop.send_replace(Some(order(Duration::from_secs(10))));
        release(&sync, "h");
    });
    assert!(
        matches!(failure(&end).cause, RouteError::Stopped { .. }),
        "{end:?}"
    );
    assert!(!rig.synced("first-input"), "nothing reached the vendor");
}

/// AD4 (persistent profile): a `Completed` or `Failed` terminal returns at
/// once; the helper's exit is not awaited and is not a turn fact.
#[test]
fn a_persistent_natural_terminal_returns_at_once() {
    let rig = Rig::new(
        &persistent(),
        &[
            script(
                1,
                &[
                    accepted(1),
                    terminal(1, "completed", "end_turn"),
                    gate("after1"),
                ],
            ),
            script(
                2,
                &[accepted(2), terminal(2, "failed", "error"), gate("after2")],
            ),
        ],
    );
    let (driver, mut receiver) = rig.session();
    for turn in 1..=2 {
        let (cx, _controls) = turn_cx(turn, driver.prepare(), CLOSE_WALL);
        let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
        let evidence = end.outcome.as_ref().unwrap();
        assert!(evidence.exit.is_none(), "turn {turn}: {end:?}");
        assert_eq!(evidence.cleanup, Cleanup::Quiescent, "turn {turn}");
        assert!(!rig.synced(&format!("after{turn}.released")));
    }
}

/// AD4/P7 (persistent profile): a tool ending after the acknowledging
/// terminal returns the turn at once, `Quiescent`; the helper is retired
/// separately.
#[test]
fn a_tool_ending_after_the_acknowledgement_returns_the_persistent_turn() {
    let rig = Rig::new(
        &persistent(),
        &[script(
            1,
            &[
                accepted(1),
                tool_started(1),
                expect_interrupt(1),
                terminal(1, "interrupted", "interrupted"),
                tool_ended(1),
                gate("after"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx_with(1, driver.prepare(), CLOSE_WALL, Duration::from_secs(60));
    let stop = &controls.stop;
    let (end, _, ()) = rig.run_beside(
        &driver,
        &mut receiver,
        (prompt(), cx),
        |mut seen| async move {
            until_seen(&mut seen, |seen| seen.tool).await;
            stop.send_replace(Some(order(Duration::from_secs(30))));
        },
    );
    let evidence = end.outcome.as_ref().unwrap();
    assert_eq!(evidence.cleanup, Cleanup::Quiescent, "{end:?}");
    assert!(evidence.exit.is_none(), "{end:?}");
    assert_eq!(
        end.terminal.as_ref().unwrap().status,
        VendorTerminalStatus::Interrupted
    );
}

/// AD4/P7 (persistent profile): the helper's exit with a tool still open
/// does not end the wait early; the turn returns at the P7 bound,
/// `Uncertain`.
#[test]
fn a_helper_exit_does_not_end_the_p7_wait() {
    let grace = Duration::from_secs(1);
    let rig = Rig::new(
        &persistent(),
        &[script(
            1,
            &[
                accepted(1),
                tool_started(1),
                expect_interrupt(1),
                terminal(1, "interrupted", "interrupted"),
                json!({"action":"exit","code":0}),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx_with(1, driver.prepare(), CLOSE_WALL, grace);
    let stop = &controls.stop;
    let (end, _, ()) = rig.run_beside(
        &driver,
        &mut receiver,
        (prompt(), cx),
        |mut seen| async move {
            until_seen(&mut seen, |seen| seen.tool).await;
            stop.send_replace(Some(order(Duration::from_secs(30))));
        },
    );
    let returned = tokio::time::Instant::now();
    let at = end.terminal.as_ref().unwrap().at;
    assert!(
        returned >= at + grace,
        "returned {:?} after the terminal",
        returned - at
    );
    assert_eq!(
        end.outcome.as_ref().unwrap().cleanup,
        Cleanup::Uncertain,
        "{end:?}"
    );
}

/// AD4 one wall cutoff (persistent profile): with Core never draining, the
/// soft stop, the helper's retirement and the remaining delivery all end by
/// the wall plus 3 s; no step starts a fresh budget.
#[test]
fn the_persistent_wall_path_ends_by_one_cutoff_with_a_stalled_consumer() {
    let steps = [
        vec![accepted(1)],
        staged_flood(1, OBSERVATION_ITEMS),
        vec![hang()],
    ]
    .concat();
    let rig = Rig::new(&persistent(), &[script(1, &steps)]);
    let (driver, receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), SHORT_WALL);
    let cutoff = cx.wall.instant() + Duration::from_secs(3);
    let sync = rig.sync();
    let end = checked(rig.runtime.block_on(async {
        let release = release_flood(&sync, &receiver, 1 + OBSERVATION_ITEMS / 2);
        tokio::join!(driver.run_turn(prompt(), cx), release).0
    }));
    let returned = tokio::time::Instant::now();
    assert!(
        matches!(failure(&end).cause, RouteError::Deadline { .. }),
        "{end:?}"
    );
    assert!(
        returned <= cutoff + Duration::from_millis(500),
        "returned {:?} past the cutoff",
        returned.saturating_duration_since(cutoff)
    );
}

/// C2 §7 item 10: an interrupted terminal that breaks the phase order (here
/// before acceptance) fails the turn and is no acknowledgement.
#[test]
fn an_interrupted_terminal_before_acceptance_is_not_an_acknowledgement() {
    let rig = Rig::new(
        &json!({}),
        &[script(
            1,
            &[
                json!({"action":"report_pids"}),
                expect_interrupt(1),
                terminal(1, "interrupted", "interrupted"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let sync = rig.sync();
    let stop = &controls.stop;
    let (end, _, ()) = rig.run_beside(&driver, &mut receiver, (prompt(), cx), |_| async move {
        until_file(sync.join("agent.pid")).await;
        stop.send_replace(Some(order(Duration::from_secs(10))));
    });
    let failure = failure(&end);
    assert!(
        matches!(failure.cause, RouteError::Protocol { .. }),
        "{end:?}"
    );
    assert!(!failure.acknowledged, "{end:?}");
}

/// AD4 (persistent profile): the wall's soft stop validates the phase as
/// the turn does: an interrupted terminal before acceptance is no
/// acknowledgement.
#[test]
fn the_wall_soft_stop_validates_the_terminal_phase() {
    let rig = Rig::new(
        &persistent(),
        &[script(
            1,
            &[
                expect_interrupt(1),
                terminal(1, "interrupted", "interrupted"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), SHORT_WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    let failure = failure(&end);
    assert!(
        matches!(failure.cause, RouteError::Deadline { .. }),
        "{end:?}"
    );
    assert!(!failure.acknowledged, "{end:?}");
}

/// AD9 no-launch row (persistent profile): a stop order before launch
/// keeps its no-launch evidence; nothing is rewritten.
#[test]
fn a_persistent_stop_before_launch_keeps_its_no_launch_evidence() {
    let rig = Rig::new(
        &persistent(),
        &[script(
            1,
            &[accepted(1), terminal(1, "completed", "end_turn")],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    controls
        .stop
        .send_replace(Some(order(Duration::from_secs(10))));
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    let failure = failure(&end);
    assert!(
        matches!(failure.cause, RouteError::Stopped { .. }),
        "{end:?}"
    );
    assert!(!failure.launched && !failure.forced, "{end:?}");
    assert_eq!(failure.cleanup, None, "{end:?}");
}

/// C2 §4.1 (persistent profile): an order's `force_at` passing without an
/// acknowledgement asks for no kill: the logical turn is unforced,
/// unacknowledged and `Uncertain`, with no process exit.
#[test]
fn a_shared_stop_unacknowledged_at_force_at_requests_no_kill() {
    let rig = Rig::new(&persistent(), &[script(1, &[accepted(1), gate("never")])]);
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let stop = &controls.stop;
    let (end, _, ()) = rig.run_beside(
        &driver,
        &mut receiver,
        (prompt(), cx),
        |mut seen| async move {
            until_seen(&mut seen, |seen| seen.accepted).await;
            stop.send_replace(Some(order(Duration::from_millis(500))));
        },
    );
    let failure = failure(&end);
    assert!(
        matches!(failure.cause, RouteError::Stopped { .. }),
        "{end:?}"
    );
    assert!(!failure.forced && !failure.acknowledged && failure.shared);
    assert_eq!(failure.cleanup, Some(WireCleanup::Uncertain));
    assert!(failure.exit.is_none(), "{end:?}");
}

/// C2 §2 health: the first protocol, transport or overflow failure latches
/// `Failed` with its cause; a later failure does not replace it.
#[test]
fn health_latches_the_first_failure() {
    let rig = Rig::new(
        &json!({}),
        &[
            script(1, &[accepted(1), raw("not json\n"), hang()]),
            script(2, &[accepted(2), oversized(), hang()]),
        ],
    );
    let (driver, mut receiver) = rig.session();
    let health = driver.health();
    assert_eq!(*health.borrow(), DriverHealth::Open);
    for turn in 1..=2 {
        let (cx, _controls) = turn_cx(turn, driver.prepare(), WALL);
        let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
        assert!(end.outcome.is_err(), "{end:?}");
        assert!(
            matches!(
                *health.borrow(),
                DriverHealth::Failed {
                    first_cause: DriverFailure::Route(RouteError::Protocol { .. })
                }
            ),
            "turn {turn}: {:?}",
            *health.borrow()
        );
    }
}

/// C2 §2 Reopen: turn 1 confirmed `v1`; turn 2's connection returns `v2`:
/// `resume_mismatch` before acceptance, and `v1` is not replaced.
#[test]
fn a_later_turn_returning_another_identity_fails_resume_mismatch() {
    let rig = Rig::new(
        &json!({}),
        &[
            script(
                1,
                &[
                    identity("v1"),
                    accepted(1),
                    terminal(1, "completed", "end_turn"),
                ],
            ),
            script(
                2,
                &[
                    identity("v2"),
                    accepted(2),
                    terminal(2, "completed", "end_turn"),
                ],
            ),
        ],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    assert!(observations(&items).iter().any(|observation| matches!(
        observation,
        Observation::IdentityConfirmed(identity) if identity.vendor_session_id == "v1"
    )));
    let (cx, _controls) = turn_cx(2, driver.prepare(), WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert_resume_mismatch(&end, &items);
}

/// C2 §2: every identity in a turn is checked against the first
/// confirmation; a second, different one fails `resume_mismatch`.
#[test]
fn two_identities_in_one_turn_fail_resume_mismatch() {
    let rig = Rig::new(
        &json!({}),
        &[script(
            1,
            &[
                identity("v1"),
                identity("v2"),
                accepted(1),
                terminal(1, "completed", "end_turn"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert_resume_mismatch(&end, &items);
}

/// C2 §2 Reopen, third case (spec amendment r3): a mismatching identity
/// after the turn's retained terminal does not fail the turn. The turn
/// keeps its result; the mismatch is reported on the session channel and
/// latches health `ResumeMismatch`, which ends the connection.
#[test]
fn a_mismatch_after_the_terminal_keeps_the_turn_and_latches_health() {
    let rig = Rig::new(
        &json!({}),
        &[script(
            1,
            &[
                identity("v1"),
                accepted(1),
                terminal(1, "completed", "end_turn"),
                identity("v2"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "the turn keeps its result: {end:?}");
    let kept = end.terminal.as_ref().unwrap();
    assert_eq!(kept.status, VendorTerminalStatus::Completed);
    let observed = observations(&items);
    assert!(
        observed.iter().any(|observation| matches!(
            observation,
            Observation::ResumeMismatch { requested, returned } if requested == "v1" && returned == "v2"
        )),
        "{observed:?}"
    );
    assert!(
        observed
            .iter()
            .any(|observation| matches!(observation, Observation::Accepted(_))),
        "{observed:?}"
    );
    assert_eq!(
        format!("{:?}", *driver.health().borrow()),
        "Failed { first_cause: ResumeMismatch }"
    );
}

/// C2 §2 Reopen, second case (Sol r1 F13, Sol r2 F13): a mismatching
/// identity after the turn's acceptance, before any terminal is retained,
/// fails the turn with the typed `AdapterError::ResumeMismatch`, whose
/// evidence is the per-turn process's: its confirmed exit, cleanup proved
/// `Quiescent` from its group's absence, and a complete Host journal. The
/// later terminal is not retained.
#[test]
fn a_mismatch_after_acceptance_fails_typed_with_the_process_evidence() {
    let rig = Rig::new(
        &json!({}),
        &[script(
            1,
            &[
                identity("v1"),
                accepted(1),
                identity("v2"),
                terminal(1, "completed", "end_turn"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    let Err(AdapterError::ResumeMismatch { evidence }) = &end.outcome else {
        panic!("a typed resume mismatch: {end:?}");
    };
    let exit = evidence.exit.expect("the process's confirmed exit");
    assert!(exit.code.is_some() || exit.signal.is_some(), "{exit:?}");
    assert_eq!(evidence.cleanup, Cleanup::Quiescent, "{evidence:?}");
    assert!(!evidence.journal_uncertain, "{evidence:?}");
    assert!(end.terminal.is_none(), "{end:?}");
    let observed = observations(&items);
    assert!(
        observed
            .iter()
            .any(|observation| matches!(observation, Observation::Accepted(_))),
        "{observed:?}"
    );
    assert!(
        observed.iter().any(|observation| matches!(
            observation,
            Observation::ResumeMismatch { requested, returned } if requested == "v1" && returned == "v2"
        )),
        "{observed:?}"
    );
}

/// `v2` against a confirmed `v1`: a mismatch observation, a failed turn,
/// no acceptance and no confirmation of `v2`.
fn assert_resume_mismatch(end: &TurnEnd, items: &[ObservationItem]) {
    let observed = observations(items);
    assert!(end.outcome.is_err(), "{end:?}");
    assert!(
        observed.iter().any(|observation| matches!(
            observation,
            Observation::ResumeMismatch { requested, returned } if requested == "v1" && returned == "v2"
        )),
        "{observed:?}"
    );
    assert!(
        !observed.iter().any(
            |observation| matches!(observation, Observation::Accepted(_))
                || matches!(
                    observation,
                    Observation::IdentityConfirmed(identity) if identity.vendor_session_id == "v2"
                )
        ),
        "{observed:?}"
    );
}

/// C2 §4 between turns (persistent profile, decision H1): the emulated
/// server's idle close arrives on the session channel only after the turn
/// returned, releases the slot and invalidates the pin taken before it.
#[test]
fn an_idle_vendor_close_releases_the_slot_and_invalidates_the_pin() {
    let mut profile = persistent();
    profile["idle_close"] = json!({"after_turn": 1, "gate": "idle", "reason": "idle_timeout"});
    let rig = Rig::new(
        &profile,
        &[
            script(1, &[accepted(1), terminal(1, "completed", "end_turn")]),
            script(2, &[accepted(2), terminal(2, "completed", "end_turn")]),
        ],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    assert!(
        !observations(&items)
            .iter()
            .any(|observation| matches!(observation, Observation::VendorClosed(_)))
    );
    let pin = driver.prepare();
    assert!(matches!(pin, Prepared::Pinned(_)));
    assert!(!controls.released());
    release(&rig.sync(), "idle");
    let closed = rig
        .runtime
        .block_on(async { tokio::time::timeout(FIXTURE_WAIT, receiver.recv()).await });
    let closed = closed.unwrap().unwrap().item;
    assert!(
        matches!(&closed.observation, Observation::VendorClosed(reason) if reason == "idle_timeout"),
        "{closed:?}"
    );
    assert!(closed.vendor_turn.is_none(), "session-level");
    assert!(controls.released(), "the closed server's slot is released");
    assert!(matches!(driver.prepare(), Prepared::NeedsConnection));
    let (cx, _controls) = turn_cx(2, pin, WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(
        matches!(
            end.outcome,
            Err(AdapterError::Rejected {
                reason: StartRejected::SessionGone,
                ..
            })
        ),
        "{end:?}"
    );
}

/// A script whose start must carry `expected` (the start's fields).
fn script_expecting(expected: &Value, steps: &[Value]) -> Value {
    json!({"expected_request": expected, "steps": steps})
}

/// Adapter design §3.2: the C2 start carries the supported effective
/// values, and a generation's first turn also the session model and
/// instructions; a pinned later turn carries only its own values.
#[test]
fn effective_values_reach_the_fake_start() {
    let mut capabilities = native_capabilities();
    capabilities["params"]["max_steps"] = json!({"support":"native"});
    capabilities["params"]["instructions"] = json!({"support":"native"});
    let rig = Rig::new(
        &json!({"capabilities": capabilities, "efforts": ["low", "high"], "persistent": true}),
        &[
            script_expecting(
                &json!({"type":"start","turn":1,"effort":"high","max_steps":5,
                        "model":"fake","instructions":"be brief"}),
                &[accepted(1), terminal(1, "completed", "end_turn")],
            ),
            script_expecting(
                &json!({"type":"start","turn":2,"effort":"low"}),
                &[accepted(2), terminal(2, "completed", "end_turn")],
            ),
        ],
    );
    let (driver, mut receiver) =
        rig.session_with(|spec| spec.instructions = Some("be brief".to_owned()));
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let spec = TurnSpec {
        effort: Some("high".to_owned()),
        max_steps: Some(5),
        ..prompt()
    };
    let (end, _) = rig.run(&driver, &mut receiver, spec, cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    let (cx, _controls) = turn_cx(2, driver.prepare(), WALL);
    let spec = TurnSpec {
        effort: Some("low".to_owned()),
        ..prompt()
    };
    let (end, _) = rig.run(&driver, &mut receiver, spec, cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    let start: Value =
        serde_json::from_slice(&fs::read(rig.sync().join("first-input")).unwrap()).unwrap();
    assert!(
        start.get("model").is_none() && start.get("instructions").is_none(),
        "{start}"
    );
}

/// AD18 run half: planning accepts `high`, but the instance's handshake
/// catalog lacks it: `Rejected(InvalidParam{effort})`, with no submission.
#[test]
fn a_catalog_only_effort_mismatch_is_rejected_without_submission() {
    let rig = Rig::new(
        &json!({"capabilities": native_capabilities(), "efforts": ["low", "high"],
                "handshake": {"requires": []}}),
        &[script(
            1,
            &[
                json!({"action":"hello","message":{"type":"hello","vendor_version":"1.0",
                                                   "features":[],"efforts":["low"]}}),
                accepted(1),
                terminal(1, "completed", "end_turn"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let spec = TurnSpec {
        effort: Some("high".to_owned()),
        ..prompt()
    };
    let (end, _) = rig.run(&driver, &mut receiver, spec, cx, |_| {});
    assert!(
        matches!(
            end.outcome,
            Err(AdapterError::Rejected {
                reason: StartRejected::InvalidParam { field: "effort" },
                ..
            })
        ),
        "{end:?}"
    );
    assert!(end.instance.is_some(), "{end:?}");
    assert!(!rig.synced("first-input"), "nothing was submitted");
}

/// C2 A1 rule 6: an unknown notification before acceptance produces no
/// observation and does not fail the turn.
#[test]
fn an_unknown_message_before_acceptance_is_activity_only() {
    let rig = Rig::new(
        &json!({}),
        &[script(
            1,
            &[
                emit(&json!({"type":"vendor_status","phase":"warming"})),
                accepted(1),
                terminal(1, "completed", "end_turn"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, items) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    assert!(matches!(
        observations(&items).first(),
        Some(Observation::Accepted(_))
    ));
}

/// C2 §2 `SessionCx`: cancelling the session stops the work the driver
/// owns; the running turn ends, every owned task is joined, and the slot
/// is released.
#[test]
fn session_cancellation_stops_owned_work() {
    let rig = Rig::new(&persistent(), &[script(1, &[accepted(1), gate("held")])]);
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let cancel = rig.cancel.clone();
    let (end, _, ()) = rig.run_beside(
        &driver,
        &mut receiver,
        (prompt(), cx),
        |mut seen| async move {
            until_seen(&mut seen, |seen| seen.accepted).await;
            cancel.cancel();
        },
    );
    assert!(end.outcome.is_err(), "{end:?}");
    rig.tracker.close();
    let joined = rig
        .runtime
        .block_on(async { tokio::time::timeout(FIXTURE_WAIT, rig.tracker.wait()).await });
    assert!(joined.is_ok(), "every owned task was joined");
    assert!(controls.released());
}

/// Runs turn 1 (accepted, then gate `g`, then a terminal) and steers once
/// it is accepted, then releases the gate; returns the steer's answer.
fn steer_once(
    profile: &Value,
    input: impl FnOnce() -> SteerInput,
) -> (Result<SteerReceipt, SteerError>, Vec<ObservationItem>) {
    let rig = Rig::new(
        profile,
        &[script(
            1,
            &[accepted(1), gate("g"), terminal(1, "completed", "end_turn")],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let sync = rig.sync();
    let driver_ref = &driver;
    let (end, items, answer) = rig.run_beside(
        &driver,
        &mut receiver,
        (prompt(), cx),
        |mut seen| async move {
            until_seen(&mut seen, |seen| seen.accepted).await;
            let answer = driver_ref.steer(input()).await;
            release(&sync, "g");
            answer
        },
    );
    assert!(end.outcome.is_ok(), "{end:?}");
    (answer, items)
}

/// C2 §2 `SteerInput.expected_vendor_turn`: a steer naming another vendor
/// turn is refused `TurnMismatch`; nothing is written.
#[test]
fn a_steer_naming_another_vendor_turn_is_refused() {
    let (answer, _) = steer_once(&json!({"capabilities": native_capabilities()}), || {
        SteerInput {
            turn: TurnNumber::try_from(1).unwrap(),
            text: "more".to_owned(),
            expected_vendor_turn: Some(VendorTurnId::try_from("fake-turn-9".to_owned()).unwrap()),
        }
    });
    assert_eq!(answer, Err(SteerError::TurnMismatch));
}

/// C2 §2 independent lanes: a steer past the 64 KiB control budget is
/// refused explicitly before it is enqueued.
#[test]
fn an_over_budget_steer_is_refused_before_enqueue() {
    let (answer, _) = steer_once(&json!({"capabilities": native_capabilities()}), || {
        SteerInput {
            turn: TurnNumber::try_from(1).unwrap(),
            text: "s".repeat(65 * 1024),
            expected_vendor_turn: None,
        }
    });
    assert_eq!(answer, Err(SteerError::OverCapacity));
}

/// C2 §2 `SteerDelivery::Partial`: a profile declaring partial steer
/// reports its semantics, in the answer and the observation.
#[test]
fn a_partial_steer_profile_reports_its_semantics() {
    let mut capabilities = native_capabilities();
    capabilities["verbs"]["steer"] = json!({"support":"partial","semantics":"after_tool"});
    let rig = Rig::new(
        &json!({"capabilities": capabilities}),
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
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let driver_ref = &driver;
    let (end, items, answer) = rig.run_beside(
        &driver,
        &mut receiver,
        (prompt(), cx),
        |mut seen| async move {
            until_seen(&mut seen, |seen| seen.accepted).await;
            driver_ref
                .steer(SteerInput {
                    turn: TurnNumber::try_from(1).unwrap(),
                    text: "more".to_owned(),
                    expected_vendor_turn: None,
                })
                .await
        },
    );
    assert!(end.outcome.is_ok(), "{end:?}");
    let partial = SteerDelivery::Partial("after_tool".into());
    let receipt = answer.unwrap();
    assert_eq!(receipt.delivery, partial);
    assert!(observations(&items).iter().any(|observation| matches!(
        observation,
        Observation::SteerDelivered { delivery, token }
            if *delivery == partial && *token == receipt.token
    )));
}

/// Polls `steer` once: its answer if it has one at once.
async fn poll_once(steer: &mut Steer<'_>) -> Option<Result<SteerReceipt, SteerError>> {
    tokio::select! {
        biased;
        answer = steer.as_mut() => Some(answer),
        () = std::future::ready(()) => None,
    }
}

/// Waits, within `within`, until `health` is `Failed`; returns what it is.
async fn until_failed(
    health: &mut watch::Receiver<DriverHealth>,
    within: Duration,
) -> DriverHealth {
    // A timeout leaves the health as it was, which the caller asserts on.
    drop(
        tokio::time::timeout(
            within,
            health.wait_for(|health| matches!(health, DriverHealth::Failed { .. })),
        )
        .await,
    );
    health.borrow().clone()
}

/// C2 §2 Close (persistent profile): a close whose deadline passes before
/// the helper's retirement reports `Uncertain`, and the slot stays held
/// until the retirement ended with the helper gone.
#[test]
fn a_close_past_its_deadline_holds_the_slot_until_the_helper_retired() {
    let rig = Rig::new(
        &persistent(),
        &[script(
            1,
            &[
                json!({"action":"report_pids"}),
                accepted(1),
                terminal(1, "completed", "end_turn"),
                gate("after"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_millis(300));
    let report = rig
        .runtime
        .block_on(driver.close(CloseMode::Graceful, deadline));
    assert_eq!(report.cleanup, Cleanup::Uncertain, "{report:?}");
    assert!(
        !controls.released(),
        "the slot outlives a close whose deadline passed"
    );
    let pid = fs::read_to_string(rig.sync().join("agent.pid")).unwrap();
    rig.runtime.block_on(async {
        let by = tokio::time::Instant::now() + FIXTURE_WAIT;
        while !controls.released() && tokio::time::Instant::now() < by {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    assert!(controls.released(), "released once the helper retired");
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the helper is gone"
    );
}

/// AD16 (persistent profile): the reservation commits only after the
/// Adapter's final delivery. A logical success whose remaining delivery
/// overflows leaves nothing pinnable, and the slot is released.
#[test]
fn a_persistent_success_whose_delivery_overflows_keeps_no_pin() {
    let steps = [
        vec![accepted(1)],
        staged_flood(1, OBSERVATION_ITEMS),
        vec![terminal(1, "completed", "end_turn")],
    ]
    .concat();
    let rig = Rig::new(&persistent(), &[script(1, &steps)]);
    // Core never drains the channel.
    let (driver, receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), SHORT_WALL);
    let sync = rig.sync();
    let end = checked(rig.runtime.block_on(async {
        let release = release_flood(&sync, &receiver, 1 + OBSERVATION_ITEMS / 2);
        tokio::join!(driver.run_turn(prompt(), cx), release).0
    }));
    assert!(
        matches!(failure(&end).cause, RouteError::Overflow { .. }),
        "{end:?}"
    );
    assert!(
        matches!(driver.prepare(), Prepared::NeedsConnection),
        "no live pin after a delivery overflow"
    );
    rig.runtime.block_on(async {
        let by = tokio::time::Instant::now() + FIXTURE_WAIT;
        while !controls.released() && tokio::time::Instant::now() < by {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    assert!(controls.released(), "the slot is released");
}

/// C2 §4.1, AD9 (persistent profile): a protocol failure reports the
/// logical connection's facts only. The helper's housekeeping kill is no
/// exit and no force, and cleanup comes from the reported tool items: one
/// still open is `Uncertain`.
#[test]
fn a_persistent_protocol_failure_reports_logical_facts_only() {
    let rig = Rig::new(
        &persistent(),
        &[script(
            1,
            &[accepted(1), tool_started(1), raw("not json\n"), hang()],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    let failure = failure(&end);
    assert!(
        matches!(failure.cause, RouteError::Protocol { .. }),
        "{end:?}"
    );
    assert!(failure.exit.is_none(), "no helper exit: {end:?}");
    assert!(!failure.forced, "no helper force: {end:?}");
    assert_eq!(failure.cleanup, Some(WireCleanup::Uncertain), "{end:?}");
}

/// C2 §2 health: a failure is published when it is detected, not after the
/// turn's delivery: with Core stalled, health is `Failed` long before the
/// delivery's stall bound.
#[test]
fn health_fails_at_detection_with_a_stalled_consumer() {
    let steps = [
        vec![accepted(1)],
        staged_flood(1, OBSERVATION_ITEMS),
        vec![raw("not json\n"), hang()],
    ]
    .concat();
    let rig = Rig::new(&json!({}), &[script(1, &steps)]);
    // Core never drains the channel.
    let (driver, receiver) = rig.session();
    let mut health = driver.health();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let sync = rig.sync();
    let seen = rig.runtime.block_on(async {
        let run = driver.run_turn(prompt(), cx);
        tokio::pin!(run);
        let failed = async {
            release_flood(&sync, &receiver, 1 + OBSERVATION_ITEMS / 2).await;
            until_failed(&mut health, Duration::from_secs(5)).await
        };
        tokio::select! {
            end = &mut run => panic!("the turn ended first: {end:?}"),
            seen = failed => seen,
        }
    });
    assert!(
        matches!(
            seen,
            DriverHealth::Failed {
                first_cause: DriverFailure::Route(RouteError::Protocol { .. })
            }
        ),
        "{seen:?}"
    );
}

/// C2 §2 health: dropping the `run_turn` future before its result latches
/// its own first cause, `TurnAbandoned`; the overflow Route then sees on
/// the closed hop does not replace it.
#[test]
fn health_fails_after_the_turn_future_was_dropped() {
    let rig = Rig::new(&json!({}), &[script(1, &[accepted(1), gate("held")])]);
    let (driver, mut receiver) = rig.session();
    let mut health = driver.health();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let seen = rig.runtime.block_on(async {
        {
            let run = driver.run_turn(prompt(), cx);
            tokio::pin!(run);
            loop {
                tokio::select! {
                    Some(admitted) = receiver.recv() => {
                        if matches!(admitted.item.observation, Observation::Accepted(_)) {
                            break;
                        }
                    }
                    end = &mut run => panic!("the turn ended first: {end:?}"),
                }
            }
        }
        until_failed(&mut health, FIXTURE_WAIT).await
    });
    assert_eq!(format!("{seen:?}"), "Failed { first_cause: TurnAbandoned }");
}

/// C2 §2 health: the persistent server's loss and a resume mismatch each
/// latch `Failed` with their cause.
#[test]
fn server_loss_and_resume_mismatch_latch_health() {
    let rig = Rig::new(
        &persistent(),
        &[script(1, &[accepted(1), json!({"action":"exit","code":3})])],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(
        matches!(failure(&end).cause, RouteError::ServerLost { .. }),
        "{end:?}"
    );
    assert_eq!(
        format!("{:?}", *driver.health().borrow()),
        "Failed { first_cause: ServerLost }"
    );

    let rig = Rig::new(
        &json!({}),
        &[script(
            1,
            &[
                identity("v1"),
                identity("v2"),
                accepted(1),
                terminal(1, "completed", "end_turn"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_err(), "{end:?}");
    assert_eq!(
        format!("{:?}", *driver.health().borrow()),
        "Failed { first_cause: ResumeMismatch }"
    );
}

/// C2 §2 independent lanes: at most eight control commands are
/// outstanding. With one steer written and awaiting the vendor's report and
/// seven queued, a ninth is refused `OverCapacity`.
#[test]
fn a_ninth_outstanding_steer_is_refused() {
    let rig = Rig::new(
        &json!({"capabilities": native_capabilities()}),
        &[script(
            1,
            &[
                accepted(1),
                json!({"action":"expect_request","expected":{"type":"steer","id":3}}),
                gate("held"),
                terminal(1, "completed", "end_turn"),
            ],
        )],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let sync = rig.sync();
    let driver_ref = &driver;
    let steer = move || -> Steer<'_> {
        Box::pin(driver_ref.steer(SteerInput {
            turn: TurnNumber::try_from(1).unwrap(),
            text: "more".to_owned(),
            expected_vendor_turn: None,
        }))
    };
    let (end, _, (queued, ninth)) = rig.run_beside(
        &driver,
        &mut receiver,
        (prompt(), cx),
        |mut seen| async move {
            until_seen(&mut seen, |seen| seen.accepted).await;
            let mut first = steer();
            assert!(poll_once(&mut first).await.is_none(), "admitted");
            until_file(sync.join("held.entered")).await;
            let mut queued = Vec::new();
            for _ in 0..7 {
                let mut next = steer();
                queued.push(poll_once(&mut next).await);
            }
            let mut ninth = steer();
            let ninth = poll_once(&mut ninth).await;
            release(&sync, "held");
            (queued, ninth)
        },
    );
    assert!(end.outcome.is_ok(), "{end:?}");
    assert!(queued.iter().all(Option::is_none), "{queued:?}");
    assert_eq!(ninth, Some(Err(SteerError::OverCapacity)));
}

/// C2 §4 between turns (persistent profile): an idle close whose
/// session-level `VendorClosed` the stalled channel cannot take latches
/// `Failed{overflow}`; the slot is still released and the pin invalidated.
#[test]
fn an_idle_close_the_stalled_channel_cannot_take_latches_overflow() {
    if rerun_with_short_stall("an_idle_close_the_stalled_channel_cannot_take_latches_overflow") {
        return;
    }
    let mut profile = persistent();
    profile["idle_close"] = json!({"after_turn": 1, "gate": "idle", "reason": "idle_timeout"});
    // The accepted mark, 1022 model marks and the final text: a full channel.
    let steps = [
        vec![accepted(1)],
        staged_flood(1, OBSERVATION_ITEMS - 2),
        vec![terminal(1, "completed", "end_turn")],
    ]
    .concat();
    let rig = Rig::new(&profile, &[script(1, &steps)]);
    // Core never drains the channel.
    let (driver, receiver) = rig.session();
    let mut health = driver.health();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let sync = rig.sync();
    let end = checked(rig.runtime.block_on(async {
        let release = release_flood(&sync, &receiver, 1 + (OBSERVATION_ITEMS - 2) / 2);
        tokio::join!(driver.run_turn(prompt(), cx), release).0
    }));
    assert!(end.outcome.is_ok(), "{end:?}");
    assert_eq!(receiver.len(), OBSERVATION_ITEMS, "the channel is full");
    assert!(matches!(driver.prepare(), Prepared::Pinned(_)));
    release(&rig.sync(), "idle");
    let seen = rig
        .runtime
        .block_on(until_failed(&mut health, Duration::from_secs(15)));
    assert_eq!(
        seen,
        DriverHealth::Failed {
            first_cause: DriverFailure::ObservationOverflow
        }
    );
    assert!(controls.released(), "the closed server's slot is released");
    assert!(matches!(driver.prepare(), Prepared::NeedsConnection));
}

/// A persistent session whose turn 1 filled the channel and whose idle
/// close then ran: its `VendorClosed` waits on the full channel, and the
/// next turn needs a new connection. Turn 2 runs `second`.
fn idle_close_in_flight(second: &[Value]) -> (Rig, SessionDriver, mpsc::Receiver<Admitted>) {
    let mut profile = persistent();
    profile["idle_close"] = json!({"after_turn": 1, "gate": "idle", "reason": "idle_timeout"});
    // The accepted mark, 1022 model marks and the final text: a full channel.
    let steps = [
        vec![accepted(1)],
        staged_flood(1, OBSERVATION_ITEMS - 2),
        vec![terminal(1, "completed", "end_turn")],
    ]
    .concat();
    let rig = Rig::new(&profile, &[script(1, &steps), script(2, second)]);
    let (driver, receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let sync = rig.sync();
    let end = checked(rig.runtime.block_on(async {
        let release = release_flood(&sync, &receiver, 1 + (OBSERVATION_ITEMS - 2) / 2);
        tokio::join!(driver.run_turn(prompt(), cx), release).0
    }));
    assert!(end.outcome.is_ok(), "{end:?}");
    assert_eq!(receiver.len(), OBSERVATION_ITEMS, "the channel is full");
    assert!(matches!(driver.prepare(), Prepared::Pinned(_)));
    assert!(!controls.released());
    release(&sync, "idle");
    // The slot goes before the close's item is offered: once it is released
    // the `VendorClosed` waits on the full channel.
    rig.runtime.block_on(async {
        let by = tokio::time::Instant::now() + FIXTURE_WAIT;
        while !controls.released() {
            assert!(tokio::time::Instant::now() < by, "the idle close never ran");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    assert!(matches!(driver.prepare(), Prepared::NeedsConnection));
    (rig, driver, receiver)
}

/// C2 §4 generation barrier (persistent profile): a driver admits a new
/// generation's first observation only after the previous generation's
/// last. The old generation's idle `VendorClosed` is in flight, blocked on
/// the full channel, when the next turn opens a new connection. Nothing is
/// drained until that turn is acknowledged either waiting on the barrier
/// (`adapter.connection.barrier_wait`) or offering its first observation
/// to the full channel (`adapter.observation.blocked`); draining then
/// reads every old item before the new generation's first. A driver
/// without the barrier always reaches the second, and fails.
#[cfg(feature = "test-failpoints")]
#[test]
fn a_new_generation_is_admitted_only_after_the_old_generations_traffic() {
    let (rig, driver, mut receiver) =
        idle_close_in_flight(&[accepted(2), terminal(2, "completed", "end_turn")]);
    // Activated once the old close is parked: occurrences count from here.
    let points = rig.points();
    for point in [
        "adapter.connection.barrier_wait",
        "adapter.observation.blocked",
    ] {
        acknowledge(&points, point);
    }
    let prepared = driver.prepare();
    assert!(matches!(prepared, Prepared::NeedsConnection));
    let (cx, _controls) = turn_cx(2, prepared, WALL);
    let (end, items) = rig.runtime.block_on(async {
        let run = driver.run_turn(prompt(), cx);
        tokio::pin!(run);
        let by = tokio::time::Instant::now() + FIXTURE_WAIT;
        while ![
            "adapter.connection.barrier_wait",
            "adapter.observation.blocked",
        ]
        .iter()
        .any(|point| points.join(format!("{point}.1.ack")).exists())
        {
            assert!(tokio::time::Instant::now() < by, "turn 2 never waited");
            tokio::select! {
                end = &mut run => panic!("turn 2 ended undelivered: {end:?}"),
                () = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
        let mut items = Vec::new();
        let end = loop {
            tokio::select! {
                Some(admitted) = receiver.recv() => items.push(admitted.item),
                end = &mut run => break end,
            }
        };
        while let Ok(admitted) = receiver.try_recv() {
            items.push(admitted.item);
        }
        (checked(end), items)
    });
    assert!(end.outcome.is_ok(), "{end:?}");
    let new = |item: &ObservationItem| {
        item.vendor_turn
            .as_ref()
            .is_some_and(|id| id.as_str() == vendor_turn(2))
    };
    let first_new = items.iter().position(new).unwrap();
    let old = &items[..first_new];
    assert!(
        old.len() == OBSERVATION_ITEMS + 1
            && matches!(
                old[OBSERVATION_ITEMS].observation,
                Observation::VendorClosed(_)
            ),
        "the old generation's close was not read before the new generation's first item: {:?}",
        items[OBSERVATION_ITEMS - 1..]
            .iter()
            .map(|item| (&item.vendor_turn, &item.observation))
            .collect::<Vec<_>>()
    );
    assert!(
        items[first_new..].iter().all(new),
        "no old item after the new generation's first"
    );
}

/// C2 D4 (persistent profile): a session's observations reach the channel
/// in decode order across all of the driver's producers, `at` never
/// earlier than the previous one's. A pinned turn's progress is stamped and
/// held before its delivery (`adapter.fake.stamped`) while the scenario's
/// idle close is released and decides (`adapter.fake.idle_decided`): the
/// idle close never runs during a turn, so nothing overtakes the progress.
#[cfg(feature = "test-failpoints")]
#[test]
fn a_pinned_turns_stamped_progress_is_not_overtaken_by_the_idle_close() {
    let mut profile = persistent();
    profile["idle_close"] = json!({"after_turn": 1, "gate": "idle", "reason": "idle_timeout"});
    let text = emit(&json!({"type":"text","vendor_turn_id":vendor_turn(2)}));
    let rig = Rig::new(
        &profile,
        &[
            script(1, &[accepted(1), terminal(1, "completed", "end_turn")]),
            script(
                2,
                &[accepted(2), text, terminal(2, "completed", "end_turn")],
            ),
        ],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _first) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    // Counted from here: turn 2's second message, its progress, is held.
    let points = rig.points();
    let held = json!({"token":POINTS_TOKEN,"occurrence":2,"action":"pause"});
    fs::write(points.join("adapter.fake.stamped.json"), held.to_string()).unwrap();
    acknowledge(&points, "adapter.fake.idle_decided");
    let pin = driver.prepare();
    assert!(matches!(pin, Prepared::Pinned(_)));
    let (cx, _second) = turn_cx(2, pin, WALL);
    let sync = rig.sync();
    let (end, items, ()) = rig.run_beside(&driver, &mut receiver, (prompt(), cx), |_| {
        let points = points.clone();
        async move {
            until_file(points.join("adapter.fake.stamped.2.ack")).await;
            release(&sync, "idle");
            until_file(points.join("adapter.fake.idle_decided.1.ack")).await;
            fs::write(points.join("adapter.fake.stamped.2.release"), b"").unwrap();
        }
    });
    assert!(
        items.windows(2).all(|pair| pair[0].at <= pair[1].at),
        "out of decode order: {:?}",
        items
            .iter()
            .map(|item| (&item.observation, item.at))
            .collect::<Vec<_>>()
    );
    assert!(end.outcome.is_ok(), "{end:?}");
}

/// C2 §4 generation barrier: a turn waiting on it has not launched, so a
/// stop, the daemon force or its wall (turn `wall`) ends it there as before
/// any launch, promptly and well within the stall bound that holds the old
/// close. The old close is still read, and nothing of the ended turn.
fn a_turn_waiting_on_the_barrier_ends_on(
    (wall, order): (Duration, fn(&Controls)),
    cause: fn(&RouteError) -> bool,
) {
    let (rig, driver, mut receiver) =
        idle_close_in_flight(&[accepted(2), terminal(2, "completed", "end_turn")]);
    let (cx, controls) = turn_cx(2, driver.prepare(), wall);
    let (end, waited) = rig.runtime.block_on(async {
        let run = driver.run_turn(prompt(), cx);
        tokio::pin!(run);
        // Polled first: the turn reaches the barrier and waits there.
        tokio::select! {
            biased;
            end = &mut run => panic!("turn 2 did not wait on the barrier: {end:?}"),
            () = tokio::task::yield_now() => {}
        }
        let sent = tokio::time::Instant::now();
        order(&controls);
        let end = tokio::time::timeout(Duration::from_secs(2), &mut run).await;
        assert!(end.is_ok(), "the order did not end the waiting turn");
        let end = end.unwrap();
        (checked(end), sent.elapsed())
    });
    let failure = failure(&end);
    assert!(cause(&failure.cause), "{end:?}");
    assert!(!failure.launched && !failure.forced, "{end:?}");
    assert_eq!(failure.cleanup, None, "{end:?}");
    assert!(waited < Duration::from_secs(1), "{waited:?}");
    let items = rig.runtime.block_on(async {
        let mut items = Vec::new();
        while items.len() <= OBSERVATION_ITEMS {
            let admitted = tokio::time::timeout(FIXTURE_WAIT, receiver.recv()).await;
            items.push(admitted.unwrap().unwrap().item);
        }
        items
    });
    assert!(
        matches!(
            items[OBSERVATION_ITEMS].observation,
            Observation::VendorClosed(_)
        ),
        "{:?}",
        items[OBSERVATION_ITEMS]
    );
    assert!(receiver.try_recv().is_err(), "nothing of the stopped turn");
}

#[test]
fn a_stop_ends_a_turn_waiting_on_the_generation_barrier() {
    a_turn_waiting_on_the_barrier_ends_on(
        (WALL, |controls| {
            controls
                .stop
                .send_replace(Some(order(Duration::from_secs(10))));
        }),
        |cause| matches!(cause, RouteError::Stopped { .. }),
    );
}

#[test]
fn the_daemon_force_ends_a_turn_waiting_on_the_generation_barrier() {
    a_turn_waiting_on_the_barrier_ends_on(
        (WALL, |controls| {
            controls
                .force
                .send_replace(Some(tokio::time::Instant::now()));
        }),
        |cause| matches!(cause, RouteError::ForceStopped { .. }),
    );
}

/// C2 §4.1 wall path: a wall passing while the turn waits on the barrier
/// ends it unlaunched with the deadline, long before the old close's stall.
#[test]
fn the_wall_ends_a_turn_waiting_on_the_generation_barrier() {
    a_turn_waiting_on_the_barrier_ends_on((Duration::from_millis(300), |_| {}), |cause| {
        matches!(cause, RouteError::Deadline { .. })
    });
}

/// C2 §2 Close: a `close()` dropped before it was polled leaves the driver
/// untouched; a later close stops the running turn.
#[test]
fn a_dropped_unpolled_close_leaves_the_driver_open() {
    let rig = Rig::new(&json!({}), &[script(1, &[accepted(1), gate("held")])]);
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), CLOSE_WALL);
    let driver_ref = &driver;
    let (end, _, (open, report)) = rig.run_beside(
        &driver,
        &mut receiver,
        (prompt(), cx),
        |mut seen| async move {
            until_seen(&mut seen, |seen| seen.accepted).await;
            let deadline = Deadline::at(tokio::time::Instant::now() + CLOSE_WITHIN);
            drop(driver_ref.close(CloseMode::Graceful, deadline));
            let open = *driver_ref.health().borrow() == DriverHealth::Open;
            let deadline = Deadline::at(tokio::time::Instant::now() + CLOSE_WITHIN);
            (open, driver_ref.close(CloseMode::Graceful, deadline).await)
        },
    );
    assert!(open, "the dropped close changed nothing");
    assert!(
        matches!(failure(&end).cause, RouteError::Stopped { .. }),
        "{end:?}"
    );
    assert_eq!(report.cleanup, Cleanup::Quiescent, "{report:?}");
    assert!(report.process_exit.is_some(), "{report:?}");
}

/// C2 §2 Close(Force), §4.1 (persistent profile): the daemon force stops the
/// shared server through Host's own lifecycle, so its Host facts are the
/// logical connection's, as for the server's loss: forced, with its exit.
#[test]
fn a_persistent_daemon_force_reports_host_facts() {
    let rig = Rig::new(&persistent(), &[script(1, &[accepted(1), gate("forced")])]);
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let force = &controls.force;
    let (end, _, ()) = rig.run_beside(
        &driver,
        &mut receiver,
        (prompt(), cx),
        |mut seen| async move {
            until_seen(&mut seen, |seen| seen.accepted).await;
            force.send_replace(Some(tokio::time::Instant::now()));
        },
    );
    let failure = failure(&end);
    assert!(
        matches!(failure.cause, RouteError::ForceStopped { .. }),
        "{end:?}"
    );
    assert!(failure.forced, "{end:?}");
    assert!(failure.exit.is_some(), "{end:?}");
}

/// AD16 (persistent profile): a pinned turn 2 whose final delivery fails
/// invalidates the pin at once, but the committed slot stays with its
/// helper's retirement until the helper is gone.
#[test]
fn a_pinned_turn_whose_delivery_fails_holds_the_slot_until_retirement() {
    if rerun_with_short_stall("a_pinned_turn_whose_delivery_fails_holds_the_slot_until_retirement")
    {
        return;
    }
    let second = [
        vec![json!({"action":"report_pids"}), accepted(2)],
        staged_flood(2, OBSERVATION_ITEMS),
        vec![terminal(2, "completed", "end_turn"), gate("after")],
    ]
    .concat();
    let rig = Rig::new(
        &persistent(),
        &[
            script(1, &[accepted(1), terminal(1, "completed", "end_turn")]),
            script(2, &second),
        ],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    let pinned = driver.prepare();
    assert!(matches!(pinned, Prepared::Pinned(_)));
    // Core stops draining: turn 2's delivery stalls.
    let (cx, _second) = turn_cx(2, pinned, WALL);
    let sync = rig.sync();
    let end = checked(rig.runtime.block_on(async {
        let release = release_flood(&sync, &receiver, 1 + OBSERVATION_ITEMS / 2);
        tokio::join!(driver.run_turn(prompt(), cx), release).0
    }));
    assert!(
        matches!(failure(&end).cause, RouteError::Overflow { .. }),
        "{end:?}"
    );
    assert!(
        matches!(driver.prepare(), Prepared::NeedsConnection),
        "the pin is invalid at once"
    );
    let pid = fs::read_to_string(rig.sync().join("agent.pid")).unwrap();
    let helper = PathBuf::from(format!("/proc/{pid}"));
    // Only the lowered stall fails the delivery while the helper still
    // retires; with 10 s it has long retired.
    if cfg!(feature = "test-failpoints") {
        assert!(helper.exists(), "the helper is still retiring");
    }
    let released = controls.released();
    assert!(
        !released || !helper.exists(),
        "the slot stays while the helper lives"
    );
    rig.runtime.block_on(async {
        let by = tokio::time::Instant::now() + FIXTURE_WAIT;
        while !controls.released() && tokio::time::Instant::now() < by {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    assert!(controls.released(), "released once the helper retired");
    assert!(!helper.exists(), "the helper is gone");
}

/// Sol r1 #8 (C2 §2 `SteerInput.turn`): a steer selected for turn 1 that
/// reaches the driver only once turn 1 ended and turn 2 runs, accepted,
/// is refused `TurnMismatch` at the driver's control-lane admission, even
/// naming no vendor turn: turn 2's agent never receives it. With no turn
/// running it is `NoActiveTurn`.
#[test]
fn a_steer_for_an_ended_turn_never_reaches_its_successor() {
    let rig = Rig::new(
        &json!({"capabilities": native_capabilities()}),
        &[
            script(1, &[accepted(1), terminal(1, "completed", "end_turn")]),
            script(
                2,
                &[accepted(2), gate("g"), terminal(2, "completed", "end_turn")],
            ),
        ],
    );
    let (driver, mut receiver) = rig.session();
    let (cx, _controls) = turn_cx(1, driver.prepare(), WALL);
    let (end, _) = rig.run(&driver, &mut receiver, prompt(), cx, |_| {});
    assert!(end.outcome.is_ok(), "{end:?}");
    let selected = || SteerInput {
        turn: TurnNumber::try_from(1).unwrap(),
        text: "for turn 1".to_owned(),
        expected_vendor_turn: None,
    };
    let idle = rig.runtime.block_on(driver.steer(selected()));
    assert_eq!(idle.unwrap_err(), SteerError::NoActiveTurn);
    let (cx, _controls) = turn_cx(2, driver.prepare(), WALL);
    let sync = rig.sync();
    let driver_ref = &driver;
    let (end, items, answer) = rig.run_beside(
        &driver,
        &mut receiver,
        (prompt(), cx),
        |mut seen| async move {
            until_seen(&mut seen, |seen| seen.accepted).await;
            let answer = driver_ref.steer(selected()).await;
            release(&sync, "g");
            answer
        },
    );
    assert_eq!(answer, Err(SteerError::TurnMismatch), "{end:?}");
    assert!(end.outcome.is_ok(), "{end:?}");
    assert!(
        !observations(&items)
            .iter()
            .any(|observation| matches!(observation, Observation::SteerDelivered { .. })),
        "the successor took no steer"
    );
}
