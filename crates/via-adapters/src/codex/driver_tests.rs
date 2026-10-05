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

/// One running turn and its order senders.
struct Running {
    task: JoinHandle<TurnEnd>,
    _stop: watch::Sender<Option<StopOrder>>,
    _force: watch::Sender<Option<Instant>>,
}

/// Starts turn `number` of `driver` as Core would, with what it prepared.
fn run(driver: &Arc<SessionDriver>, number: u32, prepared: Prepared) -> Running {
    let now = Instant::now();
    let (stop, stop_rx) = watch::channel(None);
    let (force, force_rx) = watch::channel(None);
    let capacity =
        matches!(prepared, Prepared::NeedsConnection).then(|| Box::new(()) as crate::CapacityToken);
    let cx = TurnCx {
        turn: TurnNumber::try_from(number).unwrap(),
        prepared,
        capacity,
        activity: TurnActivity::new(now),
        wall: Deadline::at(now + Duration::from_secs(600)),
        tool_grace: Duration::from_secs(60),
        stop: stop_rx,
        force: force_rx,
        stop_ack: crate::StopAck::new(),
    };
    let spec = TurnSpec {
        prompt: "hi".to_owned(),
        effort: Some("low".to_owned()),
        bound: Some(full()),
        ..TurnSpec::default()
    };
    let driver = Arc::clone(driver);
    Running {
        task: tokio::spawn(async move { driver.run_turn(spec, cx).await }),
        _stop: stop,
        _force: force,
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
