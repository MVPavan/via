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
        let cx = SessionCx {
            observations,
            tracker: TaskTracker::new(),
            cancel: CancellationToken::new(),
        };
        (
            Arc::new(self.set.open_session(&session, spec, cx)),
            receiver,
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

/// Reads the session's observations as they come, so its sink never
/// stalls while time is advanced.
fn drained(mut receiver: tokio::sync::mpsc::Receiver<Admitted>) -> JoinHandle<()> {
    tokio::spawn(async move { while receiver.recv().await.is_some() {} })
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
    _observations: JoinHandle<()>,
}

/// Turn 1 of a new driver, under `wall` and `tool_grace`: launched,
/// resumed, started and accepted as [`TURN_A`]; with `tool`, its command
/// [`EXEC`] started.
async fn accepted(rig: &Rig, timing: (Duration, Duration), tool: bool) -> Turn1 {
    let mut vendor = rig.script();
    let (driver, observations) = rig.driver();
    let observations = drained(observations);
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
/// then, never before, with the terminal.
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
    assert_eq!(kept(&end).0, Some(crate::VendorTerminalStatus::Interrupted));
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
    let _observations = drained(observations);
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

/// W6b (Sol d1 (b)): the interrupted terminal is retained after the wall
/// while the turn's wait has not yet polled its orders (held at
/// `adapter.codex.ordered`); the wait finds the delivery decided at once,
/// and settlement's own note records the wall: `Deadline`, acknowledged.
#[cfg(feature = "test-failpoints")]
#[tokio::test(start_paused = true)]
async fn w6b_terminal_retained_after_the_wall_before_any_poll() {
    use std::os::unix::fs::PermissionsExt;
    const POINT: &str = "adapter.codex.ordered";
    const TOKEN: &str = "codex-driver-tests";
    let points = tempfile::tempdir().unwrap();
    std::fs::set_permissions(points.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let command = json!({"token": TOKEN, "occurrence": 1, "action": "pause"});
    std::fs::write(
        points.path().join(format!("{POINT}.json")),
        command.to_string(),
    )
    .unwrap();
    via_routes::failpoint::activate(points.path(), TOKEN).unwrap();
    let rig = Rig::new();
    let wall = Duration::from_secs(5);
    let mut turn = accepted(&rig, (wall, Duration::from_secs(60)), false).await;
    let ack = points.path().join(format!("{POINT}.1.ack"));
    while !ack.exists() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    tokio::time::sleep_until(turn.started + wall + Duration::from_secs(1)).await;
    turn.vendor.emit(&terminal("interrupted")).await;
    turn.running
        .acknowledged
        .wait_for(|acknowledged| *acknowledged)
        .await
        .unwrap();
    std::fs::write(points.path().join(format!("{POINT}.1.release")), b"").unwrap();
    let (end, _at, _kept) = turn.end().await;
    assert_eq!(deadline(&end), (true, true));
    assert_eq!(
        end.terminal.map(|terminal| terminal.status),
        Some(crate::VendorTerminalStatus::Interrupted)
    );
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
