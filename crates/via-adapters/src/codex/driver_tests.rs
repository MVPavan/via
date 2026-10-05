//! The Codex driver over a scripted server (x.3.2 X4 §5 "Scripted Codex
//! server"): the driver's own launch through the registry's launch job,
//! answered on the vendor's ends of test pipes. These cover the reattach
//! fence's driver wait (D3): a resume of a thread waits while another
//! generation's unsubscribe or resume of it is outstanding, and a wait cut
//! by the connection's failure keeps that failure's class.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use via_routes::codex::testing::{TestRuntime, VendorEnds, model};

use crate::observation::{Admitted, TurnEnd, observation_channel};
use crate::plan::{AdapterSet, Bound, Inherit, InheritPlan, SessionRef};
use crate::{
    AdapterConfig, AdapterError, BootstrapEnv, BoundMode, CancellationToken, Deadline, Prepared,
    RouteError, SessionCx, SessionDriver, SessionId, SessionSpec, StopOrder, TaskTracker,
    TurnActivity, TurnCx, TurnNumber, TurnSpec,
};

const USER_AGENT: &str = "via/0.159.2 (Linux 6.0.0; x86_64) unknown (via; 0.0.0)";
const MODEL: &str = "gpt-6-sol";
const CWD: &str = "/work/project";
const THREAD: &str = "019a0000-0000-7000-8000-000000100001";

/// The adapter set over a test runtime, with the Codex binary configured
/// (never run: the registry's launch job takes the scripted pipes).
struct Rig {
    set: AdapterSet,
    /// Dropped after the set, so the Store outlives the driver's and the
    /// registry supervisor's tasks.
    _runtime: TestRuntime,
}

impl Rig {
    fn new() -> Self {
        let runtime = TestRuntime::new();
        let (config, resources) = runtime.parts();
        let binary = std::env::current_exe().unwrap();
        let harnesses = serde_json::value::RawValue::from_string(
            json!({"codex": {"binary": binary}}).to_string(),
        )
        .unwrap();
        let env = BootstrapEnv::from_vars(std::iter::empty::<(&str, &str)>());
        let adapters = AdapterConfig::load(env, Some(&harnesses)).unwrap();
        let set = AdapterSet::new(adapters, config, resources).unwrap();
        Self {
            set,
            _runtime: runtime,
        }
    }

    /// Queues the next server launch's scripted pipes: the vendor's ends.
    fn script(&self) -> VendorEnds {
        self.set.codex.as_ref().unwrap().servers().script().0
    }

    /// A driver of session `s_000000000001` whose identity confirmed
    /// [`THREAD`]: its first open is that thread's resume. Each call is a
    /// new driver of the same session, as Core opens a failed one's
    /// successor.
    fn driver(&self) -> (Arc<SessionDriver>, tokio::sync::mpsc::Receiver<Admitted>) {
        let (driver, observations, _cancel) = self.session();
        (driver, observations)
    }

    /// [`Self::driver`], with the session's cancellation.
    fn session(
        &self,
    ) -> (
        Arc<SessionDriver>,
        tokio::sync::mpsc::Receiver<Admitted>,
        CancellationToken,
    ) {
        let spec = SessionSpec {
            session_id: SessionId::try_from("s_000000000001").unwrap(),
            model: MODEL.to_owned(),
            instructions: None,
            initial_bound: Some(full()),
            cwd: CWD.into(),
            vendor: std::collections::BTreeMap::default(),
            inherit: InheritPlan {
                requested: Inherit::OD2_DEFAULT,
                effective: Inherit::OD2_DEFAULT,
            },
            confirmed_vendor_session_id: Some(THREAD.to_owned()),
            allow_untested: false,
        };
        let session = SessionRef {
            harness: "codex".to_owned(),
            route: "codex-app-server".to_owned(),
            adapter_version: super::ADAPTER_VERSION.to_owned(),
        };
        let (observations, receiver) = observation_channel();
        let cancel = CancellationToken::new();
        let cx = SessionCx {
            observations,
            tracker: TaskTracker::new(),
            cancel: cancel.clone(),
        };
        (
            Arc::new(self.set.open_session(&session, spec, cx)),
            receiver,
            cancel,
        )
    }
}

fn full() -> Bound {
    Bound {
        mode: BoundMode::Full,
        extra_write_dirs: Vec::new(),
        network: true,
    }
}

/// One running turn, its order senders and its stop report.
struct Running {
    /// The turn's end and the instant `run_turn` returned.
    task: JoinHandle<(TurnEnd, Instant)>,
    stop: watch::Sender<Option<StopOrder>>,
    _force: watch::Sender<Option<Instant>>,
    /// Core's view of the turn's stop report (x.3.2 X4 D7).
    acknowledged: watch::Receiver<bool>,
}

/// Starts turn `number` of `driver` as Core would, with what it prepared.
fn run(driver: &Arc<SessionDriver>, number: u32, prepared: Prepared) -> Running {
    run_timed(
        driver,
        (number, prepared),
        (Duration::from_secs(600), Duration::from_secs(60)),
    )
}

/// [`run`] with the turn's `wall` (from now) and P7 `tool_grace`.
fn run_timed(
    driver: &Arc<SessionDriver>,
    (number, prepared): (u32, Prepared),
    (wall, tool_grace): (Duration, Duration),
) -> Running {
    let now = Instant::now();
    let (stop, stop_rx) = watch::channel(None);
    let (force, force_rx) = watch::channel(None);
    let capacity =
        matches!(prepared, Prepared::NeedsConnection).then(|| Box::new(()) as crate::CapacityToken);
    let stop_ack = crate::StopAck::new();
    let acknowledged = stop_ack.subscribe();
    let cx = TurnCx {
        turn: TurnNumber::try_from(number).unwrap(),
        prepared,
        capacity,
        activity: TurnActivity::new(now),
        wall: Deadline::at(now + wall),
        tool_grace,
        stop: stop_rx,
        force: force_rx,
        stop_ack,
    };
    let spec = TurnSpec {
        prompt: "hi".to_owned(),
        effort: Some("low".to_owned()),
        bound: Some(full()),
        ..TurnSpec::default()
    };
    let driver = Arc::clone(driver);
    Running {
        task: tokio::spawn(async move {
            let end = driver.run_turn(spec, cx).await;
            (end, Instant::now())
        }),
        stop,
        _force: force,
        acknowledged,
    }
}

/// The vendor's reply opening [`THREAD`] with the session's settings.
fn thread_reply(id: &Value) -> Value {
    json!({"id": id, "result": {"thread": {"id": THREAD}, "model": MODEL, "cwd": CWD,
        "approvalPolicy": "never", "approvalsReviewer": "user",
        "sandbox": {"type": "dangerFullAccess"}}})
}

/// The next line VIA wrote with `method`, skipping up to four others.
async fn read_method(vendor: &mut VendorEnds, method: &str) -> Value {
    for _ in 0..5 {
        let line = vendor.read().await;
        if line["method"] == method {
            return line;
        }
    }
    panic!("VIA never wrote {method}");
}

/// The turn's end, within 5 s.
async fn ended(running: Running) -> TurnEnd {
    tokio::time::timeout(Duration::from_secs(5), running.task)
        .await
        .expect("the turn ended")
        .unwrap()
        .0
}

/// Drops the turn's `run_turn` future (abandoned, as a dropped Core task
/// would), and waits until it is gone.
async fn abandon(running: Running) {
    running.task.abort();
    assert!(running.task.await.unwrap_err().is_cancelled());
}

/// Driver A's turn: launches the server, resumes [`THREAD`] (answered) and
/// starts its turn (accepted); then it is abandoned, which posts the
/// thread's unsubscribe and closes its lane. What it returns keeps the
/// server live (a pin) and the unsubscribe's request.
async fn abandoned_after_accept(rig: &Rig, vendor: &mut VendorEnds) -> (Prepared, Value) {
    let (a, _observations) = rig.driver();
    let first = run(&a, 1, a.prepare());
    vendor.handshake(USER_AGENT, &[model(MODEL)]).await;
    let resume = read_method(vendor, "thread/resume").await;
    assert_eq!(resume["params"]["threadId"], THREAD);
    vendor.emit(&thread_reply(&resume["id"])).await;
    let start = read_method(vendor, "turn/start").await;
    vendor
        .emit(&json!({"id": start["id"], "result": {"turn": {"id": "turn-a", "status": "inProgress"}}}))
        .await;
    // The pin keeps the server live once A's lease goes.
    let keep = a.prepare();
    assert!(matches!(keep, Prepared::Pinned(_)), "A's server is live");
    abandon(first).await;
    let unsubscribe = read_method(vendor, "thread/unsubscribe").await;
    assert_eq!(unsubscribe["params"]["threadId"], THREAD);
    (keep, unsubscribe)
}

/// D3 (`reattach_waits_for_unsubscribe`): generation A's unsubscribe of
/// the thread is outstanding, its lane already closed; B's resume of the
/// thread on the same connection waits, writing nothing, until the
/// unsubscribe's reply, then resumes it.
#[tokio::test]
async fn reattach_waits_for_unsubscribe() {
    let rig = Rig::new();
    let mut vendor = rig.script();
    let (_keep, unsubscribe) = abandoned_after_accept(&rig, &mut vendor).await;
    let (b, _observations) = rig.driver();
    let prepared = b.prepare();
    assert!(
        matches!(prepared, Prepared::Pinned(_)),
        "B joins A's server"
    );
    let _second = run(&b, 2, prepared);
    assert!(
        vendor.silent(Duration::from_millis(300)).await,
        "no resume crosses the outstanding unsubscribe"
    );
    vendor
        .emit(&json!({"id": unsubscribe["id"], "result": {"status": "unsubscribed"}}))
        .await;
    let resume = read_method(&mut vendor, "thread/resume").await;
    assert_eq!(resume["params"]["threadId"], THREAD);
}

/// D3 (`fence_wait_keeps_failure_class`): B waits on the fence when the
/// connection fails `protocol` (an undecodable line): B's turn fails
/// `protocol`, with nothing launched.
#[tokio::test]
async fn fence_wait_keeps_failure_class() {
    let rig = Rig::new();
    let mut vendor = rig.script();
    let (_keep, _unsubscribe) = abandoned_after_accept(&rig, &mut vendor).await;
    let (b, _observations) = rig.driver();
    let second = run(&b, 2, b.prepare());
    assert!(vendor.silent(Duration::from_millis(100)).await, "B waits");
    vendor.emit_raw(b"not json\n").await;
    let end = ended(second).await;
    match end.outcome {
        Err(AdapterError::Route(failure)) => {
            assert!(
                matches!(failure.cause, RouteError::Protocol { .. }),
                "{:?}",
                failure.cause
            );
            assert!(!failure.launched, "nothing of B's turn was written");
        }
        other => panic!("not the connection's protocol failure: {other:?}"),
    }
}

/// W4: the connection fails while B's resume waits on the fence (the
/// server's stdout ends): B's turn fails with the connection's loss, its
/// transport class kept, never the session's end.
#[tokio::test]
async fn w4_connection_fails_during_fence_wait() {
    let rig = Rig::new();
    let mut vendor = rig.script();
    let (_keep, _unsubscribe) = abandoned_after_accept(&rig, &mut vendor).await;
    let (b, _observations) = rig.driver();
    let second = run(&b, 2, b.prepare());
    assert!(vendor.silent(Duration::from_millis(100)).await, "B waits");
    drop(vendor);
    let end = ended(second).await;
    match end.outcome {
        Err(AdapterError::Route(failure)) => {
            assert!(
                matches!(failure.cause, RouteError::TransportLost { .. }),
                "{:?}",
                failure.cause
            );
            assert!(!failure.launched);
        }
        other => panic!("not the connection's loss: {other:?}"),
    }
}

/// D3 (`abandoned_resume_keeps_fence`, d2 #2): generation A writes its
/// resume of the thread and is abandoned before the reply; B's resume
/// waits, writing nothing; A's reply arrives (no waiter: counted
/// abandoned), the fence clears and B writes its resume.
#[tokio::test]
async fn abandoned_resume_keeps_fence() {
    let rig = Rig::new();
    let mut vendor = rig.script();
    let (a, _a_observations) = rig.driver();
    let first = run(&a, 1, a.prepare());
    vendor.handshake(USER_AGENT, &[model(MODEL)]).await;
    let resume_a = read_method(&mut vendor, "thread/resume").await;
    let keep = a.prepare();
    let Prepared::Pinned(pin) = &keep else {
        panic!("A's server is live");
    };
    let (connection, _) = pin.server.as_ref().unwrap().live().unwrap();
    abandon(first).await;
    let (b, _b_observations) = rig.driver();
    let _second = run(&b, 2, b.prepare());
    assert!(
        vendor.silent(Duration::from_millis(300)).await,
        "no resume crosses A's unresolved one"
    );
    vendor.emit(&thread_reply(&resume_a["id"])).await;
    let resume_b = read_method(&mut vendor, "thread/resume").await;
    assert_eq!(resume_b["params"]["threadId"], THREAD);
    assert_ne!(resume_b["id"], resume_a["id"]);
    assert_eq!(connection.counts().abandoned, 1, "A's reply had no waiter");
}

// x.3.2 X4 D4: the P7 window, the wall and provenance, over the scripted
// server under paused time: every instant below is exact.

/// The accepted turn's vendor ID.
const TURN_A: &str = "019a0000-0000-7000-8000-000000200001";
/// Its command item.
const EXEC: &str = "exec-019a0000-0000-7000-8000-000000400004";

/// The late terminals a session reported: each one's vendor turn and
/// status.
type LateSeen = Arc<std::sync::Mutex<Vec<(Option<String>, crate::VendorTerminalStatus)>>>;

/// Reads the session's observations as they come, so its sink never
/// stalls while time is advanced; keeps its late terminals.
fn drained(mut receiver: tokio::sync::mpsc::Receiver<Admitted>) -> (JoinHandle<()>, LateSeen) {
    let late = LateSeen::default();
    let seen = Arc::clone(&late);
    let task = tokio::spawn(async move {
        while let Some(admitted) = receiver.recv().await {
            if let crate::Observation::LateTerminal(terminal) = admitted.item.observation {
                let vendor_turn = admitted.item.vendor_turn.map(|id| id.as_str().to_owned());
                seen.lock().unwrap().push((vendor_turn, terminal.status));
            }
        }
    });
    (task, late)
}

/// Turn A's command item `id`, started.
fn tool_started(id: &str) -> Value {
    json!({"method": "item/started", "params": {"threadId": THREAD, "turnId": TURN_A,
        "item": {"type": "commandExecution", "id": id, "command": "sleep 75",
            "cwd": CWD, "commandActions": [], "status": "inProgress"}}})
}

/// Turn A's command item `id`, ended.
fn tool_completed(id: &str) -> Value {
    json!({"method": "item/completed", "params": {"threadId": THREAD, "turnId": TURN_A,
        "item": {"type": "commandExecution", "id": id, "command": "sleep 75",
            "cwd": CWD, "commandActions": [], "status": "completed", "exitCode": 0}}})
}

/// Turn A's terminal with `status`.
fn terminal(status: &str) -> Value {
    json!({"method": "turn/completed", "params": {"threadId": THREAD,
        "turn": {"id": TURN_A, "items": [], "status": status}}})
}

/// A stop order with `cause`, published now, forcing at once and closing
/// by `close_by` (Core's cancel, capped as the case needs).
fn order(cause: crate::StopCause, close_by: Instant) -> StopOrder {
    let now = Instant::now();
    StopOrder {
        cause,
        requested_at: String::new(),
        attached: now,
        force_at: Deadline::at(now),
        close_by: Deadline::at(close_by),
    }
}

/// One accepted turn over a fresh scripted server.
struct Turn1 {
    vendor: VendorEnds,
    running: Running,
    /// When the turn started: its wall is measured from here.
    started: Instant,
    driver: Arc<SessionDriver>,
    /// The session's cancellation.
    #[cfg_attr(
        not(feature = "test-failpoints"),
        expect(dead_code, reason = "only the held-wait cases cancel the session")
    )]
    cancel: CancellationToken,
    /// The session's late terminals.
    late: LateSeen,
    _observations: JoinHandle<()>,
}

/// Turn 1 of a new driver, under `wall` and `tool_grace`: launched,
/// resumed, started and accepted as [`TURN_A`]; with `tool`, its command
/// [`EXEC`] started.
async fn accepted(rig: &Rig, timing: (Duration, Duration), tool: bool) -> Turn1 {
    let mut vendor = rig.script();
    let (driver, observations, cancel) = rig.session();
    let (observations, late) = drained(observations);
    let started = Instant::now();
    let running = run_timed(&driver, (1, driver.prepare()), timing);
    vendor.handshake(USER_AGENT, &[model(MODEL)]).await;
    let resume = read_method(&mut vendor, "thread/resume").await;
    vendor.emit(&thread_reply(&resume["id"])).await;
    let start = read_method(&mut vendor, "turn/start").await;
    vendor
        .emit(
            &json!({"id": start["id"], "result": {"turn": {"id": TURN_A, "status": "inProgress"}}}),
        )
        .await;
    if tool {
        vendor.emit(&tool_started(EXEC)).await;
    }
    Turn1 {
        vendor,
        running,
        started,
        driver,
        cancel,
        late,
        _observations: observations,
    }
}

impl Turn1 {
    /// Core's cancel (closing by `close_by`), its `turn/interrupt` read and
    /// answered; then the interrupted terminal, decoded now, which the
    /// stop report shows. Returns its decode instant `T0`.
    async fn interrupted(&mut self, close_by: Duration) -> Instant {
        self.cancel(close_by).await;
        self.decode_interrupted().await
    }

    /// Core's cancel, closing by `close_by` from now; its `turn/interrupt`
    /// read and answered.
    async fn cancel(&mut self, close_by: Duration) {
        let now = Instant::now();
        self.running
            .stop
            .send_replace(Some(order(crate::StopCause::Cancel, now + close_by)));
        self.interrupt_answered().await;
    }

    /// VIA's `turn/interrupt` of turn A, read and answered.
    async fn interrupt_answered(&mut self) {
        let interrupt = read_method(&mut self.vendor, "turn/interrupt").await;
        assert_eq!(interrupt["params"]["turnId"], TURN_A);
        self.vendor
            .emit(&json!({"id": interrupt["id"], "result": {}}))
            .await;
    }

    /// The interrupted terminal, decoded now and reported; its instant.
    async fn decode_interrupted(&mut self) -> Instant {
        let t0 = Instant::now();
        self.vendor.emit(&terminal("interrupted")).await;
        self.running
            .acknowledged
            .wait_for(|acknowledged| *acknowledged)
            .await
            .unwrap();
        assert_eq!(Instant::now(), t0, "decoded and reported at T0");
        t0
    }

    /// Whether `run_turn` is still pending once the clock reaches `at`.
    async fn pending_at(&self, at: Instant) -> bool {
        tokio::time::sleep_until(at).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        !self.running.task.is_finished()
    }

    /// The turn's end and the instant it returned; the vendor's ends and
    /// the driver, kept open.
    async fn end(self) -> (TurnEnd, Instant, (VendorEnds, Arc<SessionDriver>)) {
        let (end, at) = tokio::time::timeout(Duration::from_secs(600), self.running.task)
            .await
            .expect("the turn ended")
            .unwrap();
        (end, at, (self.vendor, self.driver))
    }
}

/// The late terminals of turn A the session reported within a second.
async fn late_reported(late: &LateSeen) -> Vec<crate::VendorTerminalStatus> {
    let by = Instant::now() + Duration::from_secs(1);
    loop {
        let reported: Vec<_> = late
            .lock()
            .unwrap()
            .iter()
            .filter(|(turn, _)| turn.as_deref() == Some(TURN_A))
            .map(|(_, status)| *status)
            .collect();
        if !reported.is_empty() || Instant::now() >= by {
            return reported;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// The terminal's status and the cleanup of an `Ok` end.
fn kept(end: &TurnEnd) -> (Option<crate::VendorTerminalStatus>, Option<crate::Cleanup>) {
    (
        end.terminal.as_ref().map(|terminal| terminal.status),
        end.outcome.as_ref().ok().map(|evidence| evidence.cleanup),
    )
}

/// F16a case 1 (`codex_cleanup_window_grace`), with cases 7 and 8: an
/// interrupted terminal decoded at `T0` with its tool open; `run_turn` is
/// pending at `T0 + 59.999 s`, long past the order's `close_by` (`T0 + 3
/// s`), and returns at `T0 + 60 s` exactly, keeping the terminal, cleanup
/// `Uncertain`. The stop report reads `true` from `T0`. A completion after
/// settlement changes nothing, and the shared server was never asked to
/// stop: its stdin stays open.
#[tokio::test(start_paused = true)]
async fn codex_cleanup_window_grace() {
    let rig = Rig::new();
    let mut turn = accepted(
        &rig,
        (Duration::from_secs(600), Duration::from_secs(60)),
        true,
    )
    .await;
    let t0 = turn.interrupted(Duration::from_secs(3)).await;
    assert!(turn.pending_at(t0 + Duration::from_secs(3)).await);
    assert!(turn.pending_at(t0 + Duration::from_millis(59_999)).await);
    let (end, at, (mut vendor, _driver)) = turn.end().await;
    assert_eq!(at, t0 + Duration::from_secs(60));
    assert_eq!(
        kept(&end),
        (
            Some(crate::VendorTerminalStatus::Interrupted),
            Some(crate::Cleanup::Uncertain)
        )
    );
    vendor.emit(&tool_completed(EXEC)).await;
    assert!(
        vendor.silent(Duration::from_secs(5)).await,
        "no stdin close: the shared server is never stopped"
    );
}

/// The standard timing: a far wall, C1's 60 s grace.
const FAR: (Duration, Duration) = (Duration::from_secs(600), Duration::from_secs(60));

/// F16a case 2: the tool's matching completion at `T0 + 20 s` ends the
/// window then, `Quiescent`.
#[tokio::test(start_paused = true)]
async fn codex_cleanup_window_tools_end() {
    let rig = Rig::new();
    let mut turn = accepted(&rig, FAR, true).await;
    let t0 = turn.interrupted(Duration::from_secs(3)).await;
    assert!(turn.pending_at(t0 + Duration::from_secs(20)).await);
    turn.vendor.emit(&tool_completed(EXEC)).await;
    let (end, at, _kept) = turn.end().await;
    assert_eq!(at, t0 + Duration::from_secs(20));
    assert_eq!(
        kept(&end),
        (
            Some(crate::VendorTerminalStatus::Interrupted),
            Some(crate::Cleanup::Quiescent)
        )
    );
}

/// F16a case 3: another item's completion at `T0 + 20 s` ends nothing:
/// the window still ends at `T0 + 60 s`.
#[tokio::test(start_paused = true)]
async fn codex_cleanup_window_wrong_id() {
    let rig = Rig::new();
    let mut turn = accepted(&rig, FAR, true).await;
    let t0 = turn.interrupted(Duration::from_secs(3)).await;
    assert!(turn.pending_at(t0 + Duration::from_secs(20)).await);
    turn.vendor.emit(&tool_completed("exec-other")).await;
    assert!(turn.pending_at(t0 + Duration::from_millis(59_999)).await);
    let (end, at, _kept) = turn.end().await;
    assert_eq!(at, t0 + Duration::from_secs(60));
    assert_eq!(kept(&end).1, Some(crate::Cleanup::Uncertain));
}

/// F16a case 4: a second order at `T0 + 30 s` neither extends nor
/// shortens the window.
#[tokio::test(start_paused = true)]
async fn codex_cleanup_window_second_order() {
    let rig = Rig::new();
    let mut turn = accepted(&rig, FAR, true).await;
    let t0 = turn.interrupted(Duration::from_secs(3)).await;
    assert!(turn.pending_at(t0 + Duration::from_secs(30)).await);
    let again = order(crate::StopCause::Cancel, t0 + Duration::from_secs(31));
    turn.running.stop.send_replace(Some(again));
    assert!(turn.pending_at(t0 + Duration::from_millis(59_999)).await);
    let (end, at, _kept) = turn.end().await;
    assert_eq!(at, t0 + Duration::from_secs(60));
    assert_eq!(kept(&end).1, Some(crate::Cleanup::Uncertain));
}

/// F16a case 5: the wall at `T0 + 1 s` caps the window: pending at `T0 +
/// 0.999 s`, returned at `T0 + 1 s`, `Uncertain`, with the order row's
/// `Ok` (the terminal was decoded before the wall).
#[tokio::test(start_paused = true)]
async fn codex_cleanup_window_wall() {
    let rig = Rig::new();
    let wall = Duration::from_secs(1);
    let mut turn = accepted(&rig, (wall, Duration::from_secs(60)), true).await;
    let t0 = turn.interrupted(Duration::from_secs(3)).await;
    assert_eq!(t0, turn.started, "the wall is at T0 + 1 s");
    assert!(turn.pending_at(t0 + Duration::from_millis(999)).await);
    let (end, at, _kept) = turn.end().await;
    assert_eq!(at, t0 + wall);
    assert_eq!(
        kept(&end),
        (
            Some(crate::VendorTerminalStatus::Interrupted),
            Some(crate::Cleanup::Uncertain)
        )
    );
}

/// F16a case 6 (zero budget): the interrupted terminal is decoded at the
/// wall itself, under an order attached before it: the window is empty,
/// so `run_turn` returns at once, keeping the terminal under the order's
/// row (`Ok`, `Uncertain`).
#[tokio::test(start_paused = true)]
async fn codex_cleanup_window_zero_budget() {
    let rig = Rig::new();
    let wall = Duration::from_secs(5);
    let mut turn = accepted(&rig, (wall, Duration::from_secs(60)), true).await;
    turn.cancel(Duration::from_secs(10)).await;
    tokio::time::sleep_until(turn.started + wall).await;
    let t0 = turn.decode_interrupted().await;
    assert_eq!(t0, turn.started + wall);
    let (end, at, _kept) = turn.end().await;
    assert_eq!(at, t0);
    assert_eq!(
        kept(&end),
        (
            Some(crate::VendorTerminalStatus::Interrupted),
            Some(crate::Cleanup::Uncertain)
        )
    );
}

/// W2: the tool ends at the grace instant itself: `run_turn` returns
/// then, never before, with the terminal, `Uncertain`.
#[tokio::test(start_paused = true)]
async fn w2_tool_ends_at_the_grace_instant() {
    let rig = Rig::new();
    let mut turn = accepted(&rig, FAR, true).await;
    let t0 = turn.interrupted(Duration::from_secs(3)).await;
    assert!(turn.pending_at(t0 + Duration::from_millis(59_999)).await);
    tokio::time::sleep_until(t0 + Duration::from_secs(60)).await;
    turn.vendor.emit(&tool_completed(EXEC)).await;
    let (end, at, _kept) = turn.end().await;
    assert_eq!(at, t0 + Duration::from_secs(60));
    // The window's end is exclusive (X4 code review r1 #1).
    assert_eq!(
        kept(&end),
        (
            Some(crate::VendorTerminalStatus::Interrupted),
            Some(crate::Cleanup::Uncertain)
        )
    );
}

/// W3 (Q8): a close while the turn drains detaches it at once, keeping
/// the terminal, `Uncertain`: Core's close order, and the driver's own
/// close.
#[tokio::test(start_paused = true)]
async fn w3_close_during_draining_detaches() {
    for core in [true, false] {
        let rig = Rig::new();
        let mut turn = accepted(&rig, FAR, true).await;
        let t0 = turn.interrupted(Duration::from_secs(3)).await;
        assert!(turn.pending_at(t0 + Duration::from_secs(10)).await);
        let _close = if core {
            let close = order(crate::StopCause::Close, t0 + Duration::from_secs(13));
            turn.running.stop.send_replace(Some(close));
            None
        } else {
            let deadline = Deadline::at(t0 + Duration::from_secs(40));
            Some(tokio::spawn(
                turn.driver.close(crate::CloseMode::Graceful, deadline),
            ))
        };
        let (end, at, _kept) = turn.end().await;
        assert_eq!(at, t0 + Duration::from_secs(10), "core: {core}");
        assert_eq!(
            kept(&end),
            (
                Some(crate::VendorTerminalStatus::Interrupted),
                Some(crate::Cleanup::Uncertain)
            ),
            "core: {core}"
        );
    }
}

/// X4 code review r2 #2, on time: Core's cancel closes by `t + 3 s` and
/// the turn ends there, `Stopped`, with no terminal; the interrupted
/// terminal decoded at `t + 5 s` reaches Core as the turn's late terminal
/// (C2 §4.1 "Late observations").
#[tokio::test(start_paused = true)]
async fn late_terminal_after_an_on_time_cut() {
    let rig = Rig::new();
    let mut turn = accepted(&rig, FAR, false).await;
    let t = Instant::now();
    turn.cancel(Duration::from_secs(3)).await;
    tokio::time::sleep_until(t + Duration::from_secs(5)).await;
    turn.vendor.emit(&terminal("interrupted")).await;
    let late = Arc::clone(&turn.late);
    let (end, at, _kept) = turn.end().await;
    assert_eq!(at, t + Duration::from_secs(3));
    assert!(end.terminal.is_none());
    assert_eq!(
        late_reported(&late).await,
        [crate::VendorTerminalStatus::Interrupted]
    );
}

/// The `Deadline` failure of an end, with its acknowledged and shared
/// facts.
fn deadline(end: &TurnEnd) -> (bool, bool) {
    match &end.outcome {
        Err(AdapterError::Route(failure)) => {
            assert!(
                matches!(failure.cause, RouteError::Deadline { .. }),
                "{:?}",
                failure.cause
            );
            (failure.acknowledged, failure.shared)
        }
        other => panic!("not the wall's deadline: {other:?}"),
    }
}

/// W6a (Sol d1 (a)): `turn/start` is unanswered at the wall, whose
/// interrupt waits on its reply; the reply and the interrupted terminal
/// come within the wall's 3 s cleanup bound. The wall's `Deadline`,
/// acknowledged and shared, keeps the terminal.
#[tokio::test(start_paused = true)]
async fn w6a_unanswered_start_at_the_wall() {
    let rig = Rig::new();
    let mut vendor = rig.script();
    let (driver, observations) = rig.driver();
    let _observations = drained(observations).0;
    let wall = Instant::now() + Duration::from_secs(5);
    let running = run_timed(
        &driver,
        (1, driver.prepare()),
        (Duration::from_secs(5), Duration::from_secs(60)),
    );
    vendor.handshake(USER_AGENT, &[model(MODEL)]).await;
    let resume = read_method(&mut vendor, "thread/resume").await;
    vendor.emit(&thread_reply(&resume["id"])).await;
    let start = read_method(&mut vendor, "turn/start").await;
    tokio::time::sleep_until(wall + Duration::from_secs(1)).await;
    vendor
        .emit(
            &json!({"id": start["id"], "result": {"turn": {"id": TURN_A, "status": "inProgress"}}}),
        )
        .await;
    let interrupt = read_method(&mut vendor, "turn/interrupt").await;
    vendor
        .emit(&json!({"id": interrupt["id"], "result": {}}))
        .await;
    tokio::time::sleep_until(wall + Duration::from_millis(1500)).await;
    vendor.emit(&terminal("interrupted")).await;
    let (end, _at) = running.task.await.unwrap();
    assert_eq!(deadline(&end), (true, true));
    assert_eq!(
        end.terminal.map(|terminal| terminal.status),
        Some(crate::VendorTerminalStatus::Interrupted)
    );
}

/// W6c: a terminal decoded at the wall instant itself is not before the
/// wall: with no order, the wall's `Deadline` keeps it (a completed
/// terminal acknowledges no stop).
#[tokio::test(start_paused = true)]
async fn w6c_terminal_decoded_at_the_wall() {
    let rig = Rig::new();
    let wall = Duration::from_secs(5);
    let mut turn = accepted(&rig, (wall, Duration::from_secs(60)), false).await;
    let at_wall = turn.started + wall;
    tokio::time::sleep_until(at_wall).await;
    turn.vendor.emit(&terminal("completed")).await;
    let (end, at, _kept) = turn.end().await;
    assert_eq!(at, at_wall, "settled at the wall, at the terminal");
    assert_eq!(deadline(&end), (false, true));
    assert_eq!(
        end.terminal.map(|terminal| terminal.status),
        Some(crate::VendorTerminalStatus::Completed)
    );
}

/// Test builds: the next turn's wait held at `adapter.codex.ordered`,
/// before it first polls its orders: a driver delayed while its consumer
/// runs on.
#[cfg(feature = "test-failpoints")]
struct HeldWait(tempfile::TempDir);

#[cfg(feature = "test-failpoints")]
impl HeldWait {
    const POINT: &str = "adapter.codex.ordered";

    /// Arms the seam's first hit.
    fn arm() -> Self {
        use std::os::unix::fs::PermissionsExt;
        const TOKEN: &str = "codex-driver-tests";
        let points = tempfile::tempdir().unwrap();
        std::fs::set_permissions(points.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = json!({"token": TOKEN, "occurrence": 1, "action": "pause"});
        std::fs::write(
            points.path().join(format!("{}.json", Self::POINT)),
            command.to_string(),
        )
        .unwrap();
        via_routes::failpoint::activate(points.path(), TOKEN).unwrap();
        Self(points)
    }

    /// Resolves once the wait is held.
    async fn reached(&self) {
        let ack = self.0.path().join(format!("{}.1.ack", Self::POINT));
        while !ack.exists() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Lets the wait go on.
    fn release(&self) {
        let release = self.0.path().join(format!("{}.1.release", Self::POINT));
        std::fs::write(release, b"").unwrap();
    }
}

/// W6b (Sol d1 (b)): the interrupted terminal is retained after the wall
/// while the turn's wait has not yet polled its orders (held at
/// `adapter.codex.ordered`); the wait finds the delivery decided at once,
/// and settlement's own note records the wall: `Deadline`, acknowledged.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn w6b_terminal_retained_after_the_wall_before_any_poll() {
    let held = HeldWait::arm();
    let rig = Rig::new();
    let wall = Duration::from_secs(5);
    let mut turn = accepted(&rig, (wall, Duration::from_secs(60)), false).await;
    held.reached().await;
    tokio::time::sleep_until(turn.started + wall + Duration::from_secs(1)).await;
    turn.vendor.emit(&terminal("interrupted")).await;
    turn.running
        .acknowledged
        .wait_for(|acknowledged| *acknowledged)
        .await
        .unwrap();
    held.release();
    let (end, _at, _kept) = turn.end().await;
    assert_eq!(deadline(&end), (true, true));
    assert_eq!(
        end.terminal.map(|terminal| terminal.status),
        Some(crate::VendorTerminalStatus::Interrupted)
    );
}

/// The cleanup a route failure reports.
#[cfg(feature = "test-failpoints")]
fn failure_cleanup(end: &TurnEnd) -> Option<crate::WireCleanup> {
    match &end.outcome {
        Err(AdapterError::Route(failure)) => failure.cleanup,
        other => panic!("not a route failure: {other:?}"),
    }
}

/// X4 code review r1 #1: the P7 window is judged on decode instants, not
/// on when the driver runs. The interrupted terminal is decoded at `T0`
/// with its tool open while the turn's wait is held (each case is its own
/// test: the failpoint controller is armed once per process); the tool's
/// completion is decoded at `T0 + completion`, and the wait resumes at
/// `T0 + 62 s`, past the window's end at `T0 + 60 s`.
#[cfg(feature = "test-failpoints")]
async fn p7_completion_case(completion: Duration, cleanup: crate::Cleanup) {
    let held = HeldWait::arm();
    let rig = Rig::new();
    let mut turn = accepted(&rig, FAR, true).await;
    held.reached().await;
    let t0 = turn.decode_interrupted().await;
    tokio::time::sleep_until(t0 + completion).await;
    turn.vendor.emit(&tool_completed(EXEC)).await;
    tokio::time::sleep_until(t0 + Duration::from_secs(62)).await;
    held.release();
    let (end, _at, _kept) = turn.end().await;
    assert_eq!(
        kept(&end),
        (
            Some(crate::VendorTerminalStatus::Interrupted),
            Some(cleanup)
        ),
        "completion at T0 + {completion:?}"
    );
}

/// r1 #1: a completion decoded after the window (`T0 + 61 s`) proves
/// nothing: `Uncertain`, as an undelayed driver's grace cut gives.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn p7_completion_after_the_window_is_uncertain() {
    p7_completion_case(Duration::from_secs(61), crate::Cleanup::Uncertain).await;
}

/// r1 #1: a completion decoded at the window's end itself is not within
/// it (the end, like the wall, is exclusive).
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn p7_completion_at_the_window_end_is_uncertain() {
    p7_completion_case(Duration::from_secs(60), crate::Cleanup::Uncertain).await;
}

/// r1 #1: a completion decoded within the window (`T0 + 59 s`) proves
/// quiescence, however late the wait resumes.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn p7_completion_within_the_window_is_quiescent() {
    p7_completion_case(Duration::from_secs(59), crate::Cleanup::Quiescent).await;
}

/// X4 code review r1 #2 (C2 §4.1 "One wall cutoff"): with no order, the
/// wall at `T5` and the turn's wait held until `T10`, an interrupted
/// terminal is decoded at `T5 + decoded`.
#[cfg(feature = "test-failpoints")]
async fn wall_cutoff_case(decoded: Duration, within: bool) {
    let wall = Duration::from_secs(5);
    let held = HeldWait::arm();
    let rig = Rig::new();
    let mut turn = accepted(&rig, (wall, Duration::from_secs(60)), false).await;
    held.reached().await;
    tokio::time::sleep_until(turn.started + wall + decoded).await;
    turn.decode_interrupted().await;
    tokio::time::sleep_until(turn.started + wall + Duration::from_secs(5)).await;
    held.release();
    let late = Arc::clone(&turn.late);
    let (end, _at, _kept) = turn.end().await;
    assert_eq!(deadline(&end), (within, true), "acknowledged");
    assert_eq!(end.terminal.is_some(), within, "terminal kept");
    if !within {
        assert_ne!(
            failure_cleanup(&end),
            Some(crate::WireCleanup::Quiescent),
            "an unproven stop"
        );
        // X4 code review r2 #2: late evidence, never lost.
        assert_eq!(
            late_reported(&late).await,
            [crate::VendorTerminalStatus::Interrupted]
        );
    }
}

/// r1 #2: decoded at `T9`, past the wall's cleanup bound `T8`, the
/// terminal is late only: the wall's `Deadline` keeps no terminal,
/// unacknowledged, its cleanup unproven, as an undelayed driver's cut at
/// `T8` gives.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn wall_terminal_after_the_cleanup_bound_is_late() {
    wall_cutoff_case(Duration::from_secs(4), false).await;
}

/// r1 #2: decoded at the bound `T8` itself, the terminal is within it:
/// kept, acknowledged.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn wall_terminal_at_the_cleanup_bound_is_kept() {
    wall_cutoff_case(Duration::from_secs(3), true).await;
}

/// The cause of a route failure.
#[cfg(feature = "test-failpoints")]
fn failure_cause(end: &TurnEnd) -> &RouteError {
    match &end.outcome {
        Err(AdapterError::Route(failure)) => &failure.cause,
        other => panic!("not a route failure: {other:?}"),
    }
}

/// How the turn's stop is ordered in an order-cutoff case.
#[cfg(feature = "test-failpoints")]
#[derive(Clone, Copy)]
enum Ordered {
    /// Core's cancel order.
    Cancel,
    /// The driver's own close (its relay order, dated inside its send).
    Close,
    /// The session's cancellation, with no order: its ending is due
    /// `CLEANUP_ALLOWANCE` after the wait notices it.
    Session,
}

/// X4 code review r1 concern 1 (C2 §4.1 "Two deadlines"): an order's
/// `close_by` bounds the wait for the terminal, judged on decode
/// instants. With the turn's wait held, the stop is ordered at `t`,
/// closing by `t + 3 s`, and the interrupted terminal (no tool open) is
/// decoded at `t + decoded`; the wait resumes at `t + 6 s`.
#[cfg(feature = "test-failpoints")]
async fn order_cutoff_case(ordered: Ordered, decoded: Duration) -> (TurnEnd, LateSeen) {
    let held = HeldWait::arm();
    let rig = Rig::new();
    let mut turn = accepted(&rig, FAR, false).await;
    held.reached().await;
    let t = Instant::now();
    let close_by = t + Duration::from_secs(3);
    let _close = match ordered {
        Ordered::Cancel => {
            let cancel = order(crate::StopCause::Cancel, close_by);
            turn.running.stop.send_replace(Some(cancel));
            None
        }
        Ordered::Close => Some(tokio::spawn(
            turn.driver
                .close(crate::CloseMode::Graceful, Deadline::at(close_by)),
        )),
        Ordered::Session => {
            turn.cancel.cancel();
            None
        }
    };
    tokio::time::sleep_until(t + decoded).await;
    match ordered {
        Ordered::Cancel => {
            turn.decode_interrupted().await;
        }
        // The close detached the session at its deadline, and the
        // session's cancellation ended its consumer: the terminal is read
        // by no turn, and no stop report is left to show it.
        Ordered::Close | Ordered::Session => turn.vendor.emit(&terminal("interrupted")).await,
    }
    tokio::time::sleep_until(t + Duration::from_secs(6)).await;
    held.release();
    let late = Arc::clone(&turn.late);
    (turn.end().await.0, late)
}

/// Concern 1: a terminal decoded at `t + 5 s`, after Core's cancel order's
/// `close_by`, is late only: the end an undelayed wait's cut at `close_by`
/// gives (`Stopped`, no terminal, unacknowledged, cleanup unproven).
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn order_terminal_after_close_by_is_late() {
    let (end, late) = order_cutoff_case(Ordered::Cancel, Duration::from_secs(5)).await;
    assert!(
        matches!(failure_cause(&end), RouteError::Stopped { .. }),
        "{:?}",
        end.outcome
    );
    assert!(end.terminal.is_none(), "late only");
    assert_eq!(failure_cleanup(&end), None, "cleanup unproven");
    // X4 code review r2 #2: late evidence, never lost.
    assert_eq!(
        late_reported(&late).await,
        [crate::VendorTerminalStatus::Interrupted]
    );
}

/// Concern 1: a terminal decoded at `t + 2 s`, within the cancel order's
/// `close_by`, is kept, however late the wait resumes.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn order_terminal_before_close_by_is_kept() {
    let (end, late) = order_cutoff_case(Ordered::Cancel, Duration::from_secs(2)).await;
    assert_eq!(
        kept(&end),
        (
            Some(crate::VendorTerminalStatus::Interrupted),
            Some(crate::Cleanup::Quiescent)
        )
    );
    assert!(late_reported(&late).await.is_empty(), "kept, not late");
}

/// Concern 1: a terminal decoded at the cancel order's `close_by` itself
/// is within it (as at the wall's cleanup bound): kept.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn order_terminal_at_close_by_is_kept() {
    let (end, late) = order_cutoff_case(Ordered::Cancel, Duration::from_secs(3)).await;
    assert_eq!(kept(&end).0, Some(crate::VendorTerminalStatus::Interrupted));
    assert!(late_reported(&late).await.is_empty(), "kept, not late");
}

/// X4 code review r2 #1 (Sol's schedule): the driver's close relays an
/// order at `T0` closing by `T10`; Core's cancel attaches at `T1` closing
/// by `T4`; the interrupted terminal is decoded at `T5`. The cut is the
/// earliest `close_by` of the orders attached before the decode, `T4`,
/// whenever the turn's wait resumes (`resume` after `T0`): `Stopped`, no
/// terminal. (Whether the late terminal then reaches Core is the close's
/// own cutoff, C2 §4.1: the closing session detaches once the turn ends.)
/// Each resume point is its own test: the failpoint controller is armed
/// once per process.
#[cfg(feature = "test-failpoints")]
async fn two_orders_case(resume: Duration) {
    let held = HeldWait::arm();
    let rig = Rig::new();
    let mut turn = accepted(&rig, FAR, false).await;
    held.reached().await;
    let t0 = Instant::now();
    let _close = tokio::spawn(turn.driver.close(
        crate::CloseMode::Graceful,
        Deadline::at(t0 + Duration::from_secs(10)),
    ));
    let mut released = false;
    let mut release_by = |at: Duration, held: &HeldWait| {
        if !released && resume <= at {
            released = true;
            held.release();
        }
    };
    for (at, step) in [
        (Duration::from_secs(1), 1),
        (Duration::from_secs(5), 5),
        (Duration::from_secs(6), 6),
    ] {
        if resume < at {
            tokio::time::sleep_until(t0 + resume).await;
            release_by(resume, &held);
        }
        tokio::time::sleep_until(t0 + at).await;
        match step {
            1 => {
                let cancel = order(crate::StopCause::Cancel, t0 + Duration::from_secs(4));
                turn.running.stop.send_replace(Some(cancel));
            }
            5 => turn.vendor.emit(&terminal("interrupted")).await,
            _ => release_by(at, &held),
        }
    }
    let (end, at, _kept) = turn.end().await;
    if resume < Duration::from_secs(4) {
        assert_eq!(
            at,
            t0 + Duration::from_secs(4),
            "a resumed wait cuts at T4, read again at the second order"
        );
    }
    assert!(
        matches!(failure_cause(&end), RouteError::Stopped { .. }),
        "resumed at T0 + {resume:?}: {:?}",
        end.outcome
    );
    assert!(end.terminal.is_none(), "resumed at T0 + {resume:?}");
}

/// r2 #1: resumed between the two orders' attachments.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn two_orders_cut_resumed_between_attachments() {
    two_orders_case(Duration::from_millis(500)).await;
}

/// r2 #1: resumed after both orders, before the earlier `close_by`.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn two_orders_cut_resumed_before_close_by() {
    two_orders_case(Duration::from_secs(2)).await;
}

/// r2 #1: resumed after the terminal's decode.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn two_orders_cut_resumed_after_the_terminal() {
    two_orders_case(Duration::from_secs(6)).await;
}

/// Concern 1: the driver's own close relays an order dated inside its
/// send (`attached`), closing by the close's deadline, where it also
/// detaches the session; a terminal decoded after that is late only, as
/// under Core's order.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn close_relay_terminal_after_close_by_is_late() {
    let (end, _late) = order_cutoff_case(Ordered::Close, Duration::from_secs(5)).await;
    assert!(
        matches!(failure_cause(&end), RouteError::Stopped { .. }),
        "{:?}",
        end.outcome
    );
    assert!(end.terminal.is_none(), "late only");
}

/// Concern 1 (the `soon()` ending): the session's cancellation at `t`,
/// with no order, while the wait is held. Its ending, `CLEANUP_ALLOWANCE`
/// after the wait notices it, never admits a terminal decoded after the
/// cancellation: the cancellation ends the registration's consumer, so a
/// terminal decoded at `t + 5 s` is read by no turn, and the held wait
/// resumes to `Stopped` with no terminal.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn session_cancel_admits_no_later_terminal() {
    let (end, _late) = order_cutoff_case(Ordered::Session, Duration::from_secs(5)).await;
    assert!(
        matches!(failure_cause(&end), RouteError::Stopped { .. }),
        "{:?}",
        end.outcome
    );
    assert!(end.terminal.is_none(), "late only");
}

/// The orders of a turn whose wall is `wall`, and their senders.
fn orders_at(
    wall: Instant,
) -> (
    super::driver::Orders,
    watch::Sender<Option<StopOrder>>,
    watch::Sender<Option<StopOrder>>,
    CancellationToken,
) {
    let (stop, stop_rx) = watch::channel(None);
    let (close, close_rx) = watch::channel(None);
    let cancel = CancellationToken::new();
    let orders = super::driver::Orders::new(
        (stop_rx, close_rx),
        (Deadline::at(wall), Duration::from_secs(60)),
        cancel.clone(),
    );
    (orders, stop, close, cancel)
}

/// An order attached at `attached`, capped at `wall`, as Core publishes
/// a cancel then (its close by the wall's cleanup bound).
fn capped(attached: Instant, wall: Instant) -> StopOrder {
    StopOrder {
        cause: crate::StopCause::Cancel,
        requested_at: String::new(),
        attached,
        force_at: Deadline::at(wall),
        close_by: Deadline::at(wall + Duration::from_secs(3)),
    }
}

/// W6d (d2 #1): the record moves only earlier: noted after the wall with
/// no order, it is `Wall@wall`; an order attached at `wall - 1 s` (capped)
/// becomes visible later, and the record moves to `Stopped@attached`. A
/// later candidate never moves it back.
#[tokio::test(start_paused = true)]
async fn w6d_the_record_moves_earlier_only() {
    use super::driver::{EndCause, Provenance};
    let wall = Instant::now() + Duration::from_secs(10);
    let (mut orders, stop, _close, cancel) = orders_at(wall);
    orders.note(Instant::now());
    assert_eq!(orders.first, None, "nothing attested before the wall");
    tokio::time::advance(Duration::from_secs(11)).await;
    orders.note(Instant::now());
    let at_wall = Provenance {
        cause: EndCause::Wall,
        at: wall,
    };
    assert_eq!(orders.first, Some(at_wall));
    let attached = wall - Duration::from_secs(1);
    stop.send_replace(Some(capped(attached, wall)));
    orders.note(Instant::now());
    let stopped = Provenance {
        cause: EndCause::Stopped,
        at: attached,
    };
    assert_eq!(orders.first, Some(stopped));
    cancel.cancel();
    stop.send_replace(Some(capped(wall + Duration::from_secs(1), wall)));
    orders.note(Instant::now());
    assert_eq!(orders.first, Some(stopped), "never later");
}

/// W6e: an order attached exactly at the wall is the wall's (`Stopped`
/// needs `attached < wall`); one attached just before is the order's.
#[tokio::test(start_paused = true)]
async fn w6e_an_order_attached_at_the_wall_is_the_walls() {
    use super::driver::{EndCause, Provenance};
    let wall = Instant::now() + Duration::from_secs(10);
    let (mut orders, stop, _close, _cancel) = orders_at(wall);
    stop.send_replace(Some(capped(wall, wall)));
    // Read before the clock reaches the wall: the order proves it passed.
    orders.note(Instant::now());
    assert_eq!(
        orders.first,
        Some(Provenance {
            cause: EndCause::Wall,
            at: wall,
        })
    );
    let (mut orders, stop, _close, _cancel) = orders_at(wall);
    let before = wall - Duration::from_nanos(1);
    stop.send_replace(Some(capped(before, wall)));
    tokio::time::advance(Duration::from_secs(11)).await;
    orders.note(Instant::now());
    assert_eq!(
        orders.first,
        Some(Provenance {
            cause: EndCause::Stopped,
            at: before,
        })
    );
}

/// The session's cancellation attests a stop at its observation: before
/// the wall it is `Stopped`, and the driver's close order is read beside
/// Core's.
#[tokio::test(start_paused = true)]
async fn provenance_reads_the_cancel_and_the_close() {
    use super::driver::{EndCause, Provenance};
    let wall = Instant::now() + Duration::from_secs(10);
    let (mut orders, _stop, _close, cancel) = orders_at(wall);
    tokio::time::advance(Duration::from_secs(2)).await;
    cancel.cancel();
    let now = Instant::now();
    orders.note(now);
    assert_eq!(
        orders.first,
        Some(Provenance {
            cause: EndCause::Stopped,
            at: now,
        })
    );
    let (mut orders, _stop, close, _cancel) = orders_at(wall);
    let attached = Instant::now();
    close.send_replace(Some(capped(attached, wall)));
    orders.note(attached);
    assert_eq!(
        orders.first,
        Some(Provenance {
            cause: EndCause::Stopped,
            at: attached,
        })
    );
}

/// x.3.2 X4 K5 (`codex_control_races`, F15 without steer: an interrupt
/// versus the turn's own terminal): Core's cancel posts the interrupt,
/// and the vendor finishes the turn, `completed`, before it handles it;
/// the interrupt's reply comes after. The turn ends with its terminal,
/// cleanup not at issue and no stop acknowledged (no `interrupted`
/// terminal); the late reply pairs and fails nothing: the thread's next
/// turn is accepted and completes.
#[tokio::test(start_paused = true)]
async fn codex_control_races_interrupt_vs_terminal() {
    let rig = Rig::new();
    let mut turn = accepted(
        &rig,
        (Duration::from_secs(600), Duration::from_secs(60)),
        false,
    )
    .await;
    let now = Instant::now();
    turn.running.stop.send_replace(Some(order(
        crate::StopCause::Cancel,
        now + Duration::from_secs(3),
    )));
    let interrupt = read_method(&mut turn.vendor, "turn/interrupt").await;
    assert_eq!(interrupt["params"]["turnId"], TURN_A);
    turn.vendor.emit(&terminal("completed")).await;
    turn.vendor
        .emit(&json!({"id": interrupt["id"], "result": {}}))
        .await;
    let acknowledged = turn.running.acknowledged.clone();
    let (end, _, (mut vendor, driver)) = turn.end().await;
    assert_eq!(
        end.terminal.as_ref().map(|terminal| terminal.status),
        Some(crate::VendorTerminalStatus::Completed)
    );
    assert!(end.outcome.is_ok(), "{:?}", end.outcome);
    assert!(!*acknowledged.borrow(), "no interrupted terminal");
    let second = run(&driver, 2, driver.prepare());
    let start = read_method(&mut vendor, "turn/start").await;
    vendor
        .emit(&json!({"id": start["id"], "result": {"turn": {"id": "turn-2", "status": "inProgress"}}}))
        .await;
    vendor
        .emit(
            &json!({"method": "turn/completed", "params": {"threadId": THREAD,
            "turn": {"id": "turn-2", "items": [], "status": "completed"}}}),
        )
        .await;
    let end = ended(second).await;
    assert_eq!(
        end.terminal.as_ref().map(|terminal| terminal.status),
        Some(crate::VendorTerminalStatus::Completed)
    );
    assert!(end.outcome.is_ok(), "{:?}", end.outcome);
}
