//! x.3.2 X3 C2: the registration's single consumer against its lane's
//! markers, schedule by schedule (design §10). Each test drives one
//! registration's consumer through its lane as the connection task would
//! push it: `Start` and `Reply` markers, and the thread's messages with
//! their decode positions and read instants.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use via_routes::WireCleanup;
use via_routes::codex::{
    BoundedBytes, LANE_BYTES, Lane, LaneEnd, LaneItem, MESSAGE_BYTES, RequestId, Routed,
    ServerRequest, VendorMessage,
};

use super::super::driver::{RetireGuard, with_undecoded};
use super::{
    Admission, CONTRADICTED, Delivery, Drained, Evidence, Folders, LossRecord, Losses, Normalizing,
    ObservationLoss, Registration, ServerEvidence, StartCx, Stop, UNDECODED, UNKNOWN,
};
use crate::driver::DriverState;
use crate::observation::{Admitted, Observation, ObservationItem, SessionCap, observation_channel};
use crate::{
    AdapterError, DriverFailure, DriverHealth, RouteError, RouteFailure, TurnActivity, TurnEnd,
    TurnNumber, VendorTerminalStatus,
};

const THREAD: &str = "thread-1";
/// Turn 2's vendor turn.
const B: &str = "vendor-b";
/// Turn 1's vendor turn.
const T: &str = "vendor-t";

fn turn(number: u32) -> TurnNumber {
    TurnNumber::try_from(number).unwrap()
}

/// Keeps nothing: the unit tests' server folder.
struct NoEvidence;

impl ServerEvidence for NoEvidence {
    fn keep<'a>(
        &'a self,
        _bytes: &'a [u8],
        _what: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn overflow(&self) {}
}

/// A turn admitted on the registration whose `Start` was pushed.
struct Started {
    delivery: Arc<Delivery>,
    activity: TurnActivity,
    /// The turn's stop report, as Core reads it (x.3.2 X4 D7).
    stop_ack: watch::Receiver<bool>,
    _admission: Admission,
}

/// Admits turn `number` on `registration` and pushes its `Start` on
/// `lane`, with `credit`, as the driver does.
fn started(
    (registration, lane): (&Arc<Registration>, &Lane),
    number: u32,
    credit: crate::observation::Charge,
) -> Started {
    let delivery = Delivery::new(4);
    let activity = TurnActivity::new(Instant::now());
    let admission = registration
        .admit(turn(number), Arc::new(|| {}), &delivery)
        .unwrap();
    let stop_ack = crate::StopAck::new();
    let acknowledged = stop_ack.subscribe();
    let cx = StartCx {
        delivery: Arc::clone(&delivery),
        activity: activity.clone(),
        schema: false,
        instance: None,
        correlation: Arc::new(OnceLock::new()),
        credit,
        stop_ack,
        // No order, wall or grace ends anything here.
        cutoffs: crate::codex::driver::Cutoffs::unbounded(
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_secs(3600),
        ),
    };
    assert!(lane.push_start(turn(number), activity.decode_watermark(), Box::new(cx)));
    Started {
        delivery,
        activity,
        stop_ack: acknowledged,
        _admission: admission,
    }
}

/// One registration, its lane and its running consumer, as the driver
/// and the connection task see them; turn 2 is the session's latest.
struct Fixture {
    registration: Arc<Registration>,
    lane: Arc<Lane>,
    health: Arc<watch::Sender<DriverHealth>>,
    losses: Arc<Mutex<Losses>>,
    cancel: CancellationToken,
    state: Arc<Mutex<DriverState>>,
    cap: SessionCap,
    received: mpsc::Receiver<Admitted>,
    _consumer: JoinHandle<()>,
}

impl Fixture {
    fn new() -> Self {
        let (sink, received) = observation_channel();
        let cap = SessionCap::new(&sink);
        let registration = Registration::new(4, cap.clone());
        let lane = Arc::new(Lane::default());
        let health = Arc::new(watch::Sender::new(DriverHealth::Open));
        let losses = Arc::new(Mutex::new(Losses {
            record: None,
            latest: Some(turn(2)),
            ..Losses::default()
        }));
        let cancel = CancellationToken::new();
        let consumer = Normalizing::new(
            (Arc::clone(&registration), Arc::clone(&lane)),
            (
                sink,
                Evidence {
                    server: Arc::new(NoEvidence),
                    earlier: Folders::default(),
                },
            ),
            (cancel.clone(), Arc::clone(&health)),
            LossRecord {
                losses: Arc::clone(&losses),
                generation: 3,
            },
        );
        Self {
            registration,
            lane,
            health,
            losses,
            cancel,
            state: Arc::default(),
            cap,
            received,
            _consumer: tokio::spawn(consumer.run()),
        }
    }

    /// Maps turn `number` to Core, as its close does.
    fn mapped(&self, number: u32) {
        let mut ledger = self.registration.ledger();
        let credit = self.credit();
        ledger.map(turn(number));
        ledger.close(turn(number), credit);
    }

    fn credit(&self) -> crate::observation::Charge {
        let slot = self.cap.credit_slot(1024).unwrap();
        self.cap.try_charge(slot).ok().unwrap()
    }

    /// Pushes turn `number`'s `Start`, as its `turn/start` is handed to
    /// Wire.
    fn start(&self, number: u32) -> Started {
        started((&self.registration, &self.lane), number, self.credit())
    }

    /// Pushes turn `number`'s `Reply`, read now, accepting `accepted`;
    /// whether it was contradicted, and when it was read.
    fn reply(&self, number: u32, accepted: Option<&str>) -> (bool, Instant) {
        let at = Instant::now();
        let (contradicted, pushed) =
            self.lane
                .push_reply(turn(number), at, accepted.map(str::to_owned));
        assert!(pushed, "the reply fits");
        (contradicted, at)
    }

    fn push(&self, item: LaneItem) {
        assert!(self.lane.push(item, 64), "the lane takes it");
    }

    /// Lets the consumer run until it waits.
    async fn settle(&self) {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }

    /// What went out so far.
    fn observed(&mut self) -> Vec<ObservationItem> {
        std::iter::from_fn(|| self.received.try_recv().ok())
            .map(|admitted| admitted.item)
            .collect()
    }

    fn record(&self) -> Option<ObservationLoss> {
        self.losses.lock().unwrap().record
    }

    /// The driver's retirement guard.
    fn guard(&self) -> RetireGuard {
        RetireGuard {
            registration: Arc::clone(&self.registration),
            lane: Arc::clone(&self.lane),
            loss: LossRecord {
                losses: Arc::clone(&self.losses),
                generation: 3,
            },
            state: Arc::clone(&self.state),
        }
    }

    /// The cleanup retirement folded into the driver's facts.
    fn folded(&self) -> Option<WireCleanup> {
        self.state
            .lock()
            .unwrap()
            .retirement
            .and_then(|retirement| retirement.cleanup)
    }

    /// The close's detach after its cutoff: the barrier by 200 ms, the
    /// seal and the close's loss, then retirement. The outcome.
    async fn detach(&self) -> Option<Drained> {
        let by = Instant::now() + Duration::from_millis(200);
        let drained = self.registration.drain(by).await;
        let sealed = self.registration.seal();
        let position = self.registration.floor(sealed.position);
        if self
            .losses
            .lock()
            .unwrap()
            .note_close((&self.lane, 3), drained, (&sealed, position))
        {
            self.registration.mark_incomplete();
        }
        drop(self.guard());
        drained
    }
}

/// A routed message `line` at `seq`, naming vendor turn `named`, mapped to
/// `owner` when the connection had, read now.
fn routed(line: &Value, seq: u64, (named, owner): (Option<&str>, Option<u32>)) -> Routed {
    let bytes = (line.to_string() + "\n").into_bytes();
    Routed {
        staged: VendorMessage::new(BoundedBytes::try_from_message(bytes).unwrap()),
        seq,
        turn: named.map(str::to_owned),
        owner: owner.map(turn),
        at: Instant::now(),
        mark: None,
    }
}

fn message(line: &Value, seq: u64, named: (Option<&str>, Option<u32>)) -> LaneItem {
    LaneItem::Message(routed(line, seq, named))
}

/// A tool item `id` of vendor turn `named` started.
fn tool_started(named: &str, id: &str) -> Value {
    json!({"method": "item/started", "params": {"threadId": THREAD, "turnId": named,
        "item": {"type": "commandExecution", "id": id, "command": "sleep 1",
            "cwd": "/w", "commandActions": [], "status": "inProgress"}}})
}

/// Vendor turn `named`'s command item `id` completed.
fn tool_completed(named: &str, id: &str) -> Value {
    json!({"method": "item/completed", "params": {"threadId": THREAD, "turnId": named,
        "item": {"type": "commandExecution", "id": id, "command": "sleep 1",
            "cwd": "/w", "commandActions": [], "status": "completed", "exitCode": 0}}})
}

/// Vendor turn `named`'s command item completed `declined`.
fn denial(named: &str) -> Value {
    denied(named, "item-denied")
}

/// Vendor turn `named`'s command item `id` completed `declined`.
fn denied(named: &str, id: &str) -> Value {
    json!({"method": "item/completed", "params": {"threadId": THREAD, "turnId": named,
        "item": {"type": "commandExecution", "id": id, "command": "rm -rf build",
            "cwd": "/w", "commandActions": [], "status": "declined"}}})
}

/// Vendor turn `named`'s model output.
fn delta(named: &str) -> Value {
    json!({"method": "item/agentMessage/delta", "params": {"threadId": THREAD,
        "turnId": named, "itemId": "msg-1", "delta": "hi"}})
}

/// Vendor turn `named`'s terminal, with `status`.
fn completed(named: &str, status: &str) -> Value {
    json!({"method": "turn/completed", "params": {"threadId": THREAD,
        "turn": {"id": named, "items": [], "status": status}}})
}

/// The approval request `id` of vendor turn `named`'s item `item`.
fn approval(id: i64, named: &str, item: &str) -> Value {
    json!({"id": id, "method": "item/commandExecution/requestApproval",
        "params": {"threadId": THREAD, "turnId": named, "itemId": item}})
}

/// Approval request `id`'s placeholder at `seq`, its decline written
/// whole.
fn declined(id: i64, seq: u64, named: (Option<&str>, Option<u32>)) -> LaneItem {
    let routed = routed(&approval(id, named.0.unwrap_or(B), "item-1"), seq, named);
    let decoded_at = routed.at;
    LaneItem::Declined {
        routed,
        decoded_at,
        written: watch::channel(Some(true)).1,
    }
}

/// Each observation's kind, vendor turn and instant.
fn kinds(observed: &[ObservationItem]) -> Vec<(&'static str, Option<String>, Instant)> {
    observed
        .iter()
        .map(|item| {
            let kind = match &item.observation {
                Observation::Accepted(_) => "accepted",
                Observation::Progress(_) => "progress",
                Observation::ActionDenied(_) => "denied",
                Observation::RequestDeclined(_) => "declined",
                Observation::FinalText(_) => "final_text",
                Observation::IdentityConfirmed(_)
                | Observation::SteerDelivered { .. }
                | Observation::Warning(_)
                | Observation::VendorClosed(_)
                | Observation::ResumeMismatch { .. }
                | Observation::LateTerminal(_) => "other",
            };
            let named = item
                .vendor_turn
                .as_ref()
                .map(|turn| turn.as_str().to_owned());
            (kind, named, item.at)
        })
        .collect()
}

/// `at` never decreases on the channel.
fn monotone(observed: &[ObservationItem]) -> bool {
    observed.windows(2).all(|pair| pair[0].at <= pair[1].at)
}

#[cfg(feature = "test-failpoints")]
/// Pauses the consumer at `point`'s first hit; the directory arms it.
fn pause_at(point: &str) -> tempfile::TempDir {
    pause_at_hit(point, 1)
}

#[cfg(feature = "test-failpoints")]
/// Pauses at `point`'s `hit`th hit; the directory arms it.
fn pause_at_hit(point: &str, hit: u32) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    const TOKEN: &str = "codex-consumer-tests";
    let points = tempfile::tempdir().unwrap();
    std::fs::set_permissions(points.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let command = json!({"token": TOKEN, "occurrence": hit, "action": "pause"});
    std::fs::write(
        points.path().join(format!("{point}.json")),
        command.to_string(),
    )
    .unwrap();
    via_routes::failpoint::activate(points.path(), TOKEN).unwrap();
    points
}

#[cfg(feature = "test-failpoints")]
/// Waits until the consumer paused at `point`.
async fn reached(points: &tempfile::TempDir, point: &str) {
    reached_hit(points, point, 1).await;
}

#[cfg(feature = "test-failpoints")]
/// Waits until `point` paused at its `hit`th hit.
async fn reached_hit(points: &tempfile::TempDir, point: &str, hit: u32) {
    let ack = points.path().join(format!("{point}.{hit}.ack"));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ack.exists() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

#[cfg(feature = "test-failpoints")]
fn release(points: &tempfile::TempDir, point: &str) {
    release_hit(points, point, 1);
}

#[cfg(feature = "test-failpoints")]
fn release_hit(points: &tempfile::TempDir, point: &str, hit: u32) {
    std::fs::write(points.path().join(format!("{point}.{hit}.release")), b"").unwrap();
}

/// x.3.2 X3 S3 (r8 #5): after `Start(B)`, earlier turn A's late decline
/// is read at t1, then B's reply at t2. A's `vendor.request_declined`
/// (late, at t1) precedes B's `turn.accepted` (at t2), `at` never
/// decreases, and B's frontier reaches the reply's position.
#[tokio::test]
async fn s3_an_earlier_turns_decline_goes_out_before_the_acceptance() {
    let mut fixture = Fixture::new();
    fixture.mapped(1);
    let b = fixture.start(2);
    fixture.push(declined(90, 5, (Some(T), Some(1))));
    // Both are queued before the consumer runs: the reply cannot overtake.
    std::thread::sleep(Duration::from_millis(5));
    let (_, t2) = fixture.reply(2, Some(B));
    fixture.settle().await;
    let observed = fixture.observed();
    let kinds = kinds(&observed);
    assert_eq!(
        kinds
            .iter()
            .map(|(kind, named, _)| (*kind, named.as_deref()))
            .collect::<Vec<_>>(),
        [("declined", Some(T)), ("accepted", Some(B))]
    );
    assert!(kinds[0].2 < t2, "the decline at its own read instant");
    assert_eq!(kinds[1].2, t2, "the acceptance at the reply's");
    assert!(monotone(&observed));
    assert_eq!(b.activity.delivered(), 2, "through the reply's position");
}

/// x.3.2 X3 S3b (i) (r10 #1): after `Start(B)`, turn 1 (never mapped)
/// has a late denial read at n, then B's reply at n+1, then B's progress
/// at n+2. Turn 1's item gives no event and is its loss; B's acceptance
/// and progress are delivered; B's frontier stays below n.
#[tokio::test]
async fn s3b_a_pending_turns_fence_stops_at_a_loss() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.push(message(&denial(T), 5, (Some(T), Some(1))));
    fixture.reply(2, Some(B));
    fixture.push(message(&delta(B), 7, (Some(B), Some(2))));
    fixture.settle().await;
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect();
    assert_eq!(kinds, ["accepted", "progress"]);
    assert_eq!(
        fixture.record(),
        Some(ObservationLoss {
            trigger: turn(1),
            generation: 3,
            first_unqueued: 5,
            omitted: 1,
        })
    );
    assert_eq!(b.activity.delivered(), 0, "the frontier stays below n");
    assert!(
        fixture.registration.incomplete(),
        "a message recorded as loss leaves continuity unproven (§6.6)"
    );
}

/// x.3.2 X3 S7, the consumer's side: a placeholder routed before the cut
/// is in the prefix: its decline goes out late, and the close's outcome
/// is `Cut`.
#[tokio::test]
async fn s7_a_placeholder_before_the_cut_is_delivered() {
    let mut fixture = Fixture::new();
    fixture.mapped(1);
    fixture.push(declined(90, 5, (Some(T), Some(1))));
    fixture.lane.end(LaneEnd::Closed);
    assert_eq!(fixture.detach().await, Some(Drained::Cut));
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, named, _)| (kind, named))
        .collect();
    assert_eq!(kinds, [("declined", Some(T.to_owned()))]);
    assert_eq!(fixture.record(), None);
    assert_eq!(fixture.folded(), None, "nothing open, nothing unproven");
}

/// x.3.2 X3 S8 (r9 #1, #2; owner's r10 ruling): B's own tool start read
/// at n before B's reply (n+1) is retained, under its lane charge, with
/// no event and no frontier; after the acceptance (at the reply's
/// instant) it goes out restamped `max(t_n, t_r)`, enters the ledger, and
/// B's interrupted terminal at n+2 leaves its cleanup open. No failure,
/// no loss.
#[tokio::test]
async fn s8_an_early_tool_start_is_retained_then_released() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), None)));
    fixture.settle().await;
    assert!(fixture.observed().is_empty(), "retained: no event");
    assert_eq!(b.activity.delivered(), 0);
    assert_eq!(fixture.lane.charged().0, 1, "still charged to the lane");
    assert!(!fixture.registration.ledger().tools_open(turn(2)));
    tokio::time::sleep(Duration::from_millis(5)).await;
    let (contradicted, t_r) = fixture.reply(2, Some(B));
    assert!(contradicted, "an unmapped item was read under the start");
    fixture.settle().await;
    let observed = fixture.observed();
    let kinds = kinds(&observed);
    assert_eq!(
        kinds
            .iter()
            .map(|(kind, _, at)| (*kind, *at))
            .collect::<Vec<_>>(),
        [("accepted", t_r), ("progress", t_r)]
    );
    let Observation::Progress(marks) = &observed[1].observation else {
        panic!("progress");
    };
    assert_eq!(
        marks.tools_started,
        [("tool-b".to_owned(), "commandExecution".to_owned())]
    );
    assert!(fixture.registration.ledger().tools_open(turn(2)));
    assert_eq!(
        b.activity.delivered(),
        2,
        "through the reply, after the release"
    );
    assert_eq!(fixture.lane.charged().0, 0);
    fixture.push(message(&completed(B, "interrupted"), 7, (Some(B), Some(2))));
    fixture.settle().await;
    assert_eq!(b.activity.delivered(), 3);
    let sealed = b.delivery.seal();
    assert_eq!(
        sealed.terminal.map(|retained| retained.terminal.status),
        Some(VendorTerminalStatus::Interrupted)
    );
    assert!(sealed.tools_open, "the released tool start is open");
    assert_eq!(fixture.registration.failure(), None);
    assert_eq!(fixture.record(), None);
}

/// x.3.2 X3 S8, the frontier: while the retained item is released, B's
/// frontier has not passed it, though the acceptance (at the reply's
/// later position) went out.
#[cfg(feature = "test-failpoints")]
#[tokio::test]
async fn s8_the_frontier_waits_for_the_release() {
    const POINT: &str = "adapter.codex.early_handoff";
    let points = pause_at(POINT);
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), None)));
    fixture.reply(2, Some(B));
    reached(&points, POINT).await;
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect();
    assert_eq!(kinds, ["accepted"]);
    assert_eq!(b.activity.delivered(), 0, "not past the retained item");
    release(&points, POINT);
    fixture.settle().await;
    assert_eq!(b.activity.delivered(), 2);
}

/// x.3.2 X3 S8, seal variant (a): B's wall seals `Pending(B)` after its
/// item was retained: no event, a loss record for B from n, continuity
/// unproven, and retirement folds `Uncertain`.
#[tokio::test]
async fn s8a_a_pending_seal_loses_the_retained_item() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), None)));
    fixture.settle().await;
    assert_eq!(
        fixture.registration.outstanding(),
        Some(5),
        "the item is retained"
    );
    b.delivery.seal();
    fixture.settle().await;
    fixture.reply(2, Some(B));
    fixture.settle().await;
    assert!(fixture.observed().is_empty());
    assert_eq!(
        fixture.record(),
        Some(ObservationLoss {
            trigger: turn(2),
            generation: 3,
            first_unqueued: 5,
            omitted: 1,
        })
    );
    assert!(fixture.registration.incomplete());
    assert_eq!(fixture.lane.charged().0, 0, "its charge dropped");
    drop(fixture.guard());
    assert_eq!(fixture.folded(), Some(WireCleanup::Uncertain));
}

/// x.3.2 X3 S8, seal variant (b): B seals first, then its item arrives
/// while no turn is held (`unanswered` = B): it can never be mapped, so
/// it is B's loss, and continuity is unproven.
#[tokio::test]
async fn s8b_an_item_after_a_pending_seal_is_loss() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.settle().await;
    b.delivery.seal();
    fixture.settle().await;
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), None)));
    fixture.settle().await;
    assert!(fixture.observed().is_empty());
    assert_eq!(
        fixture.record(),
        Some(ObservationLoss {
            trigger: turn(2),
            generation: 3,
            first_unqueued: 5,
            omitted: 1,
        })
    );
    assert!(fixture.registration.incomplete());
    drop(fixture.guard());
    assert_eq!(fixture.folded(), Some(WireCleanup::Uncertain));
}

/// Turn 3's admission at the start gate, then its `Start`, as the driver
/// would push it: resolves once it was pushed.
fn successor(fixture: &Fixture) -> JoinHandle<Started> {
    let (registration, lane) = (Arc::clone(&fixture.registration), Arc::clone(&fixture.lane));
    let credit = fixture.credit();
    tokio::spawn(async move {
        lane.start_gate().await;
        started((&registration, &lane), 3, credit)
    })
}

/// x.3.2 X3 S8, successor variant (a) (r10 #2): B is written and sealed
/// in `Pending` before any reply; C waits at the start gate while
/// `Reply(B)` is unpopped. B's unmapped item, then `Reply(B)`: the item is
/// B's loss, the gate opens and C runs.
#[tokio::test]
async fn s8_successor_a_waits_for_the_reply() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.settle().await;
    b.delivery.seal();
    let c = successor(&fixture);
    fixture.settle().await;
    assert!(!c.is_finished(), "C waits at the start gate");
    assert_eq!(fixture.lane.open_start(), Some(turn(2)));
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), None)));
    fixture.reply(2, Some(B));
    let c = tokio::time::timeout(Duration::from_secs(5), c)
        .await
        .unwrap()
        .unwrap();
    fixture.reply(3, Some("vendor-c"));
    fixture.settle().await;
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, named, _)| (kind, named))
        .collect();
    assert_eq!(kinds, [("accepted", Some("vendor-c".to_owned()))]);
    assert_eq!(
        fixture
            .record()
            .map(|record| (record.trigger, record.first_unqueued, record.omitted)),
        Some((turn(2), 5, 1))
    );
    assert!(fixture.registration.incomplete());
    drop(c);
}

/// x.3.2 X3 S8, successor variant (b): `Reply(B)` first opens the gate
/// as it is popped, and C runs; B's later traffic is B's through the
/// connection's mapping (the late path: B was never mapped to Core, so it
/// is B's loss).
#[tokio::test]
async fn s8_successor_b_runs_once_the_reply_is_taken() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.settle().await;
    b.delivery.seal();
    let c = successor(&fixture);
    fixture.settle().await;
    assert!(!c.is_finished());
    fixture.reply(2, Some(B));
    tokio::time::timeout(Duration::from_secs(5), c)
        .await
        .unwrap()
        .unwrap();
    fixture.push(message(&denial(B), 6, (Some(B), Some(2))));
    fixture.reply(3, Some("vendor-c"));
    fixture.settle().await;
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, named, _)| (kind, named))
        .collect();
    assert_eq!(kinds, [("accepted", Some("vendor-c".to_owned()))]);
    assert_eq!(
        fixture
            .record()
            .map(|record| (record.trigger, record.first_unqueued)),
        Some((turn(2), 6))
    );
}

/// x.3.2 X3 S9 (r9 #1): B's own terminal read at n before its reply is
/// retained, then released after `turn.accepted` and retained as B's
/// first terminal, restamped to the reply's instant; a second terminal
/// cannot replace it, and B's frontier stayed below n until the release.
#[tokio::test]
async fn s9_an_early_terminal_is_the_turns_first() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.push(message(&completed(B, "completed"), 5, (Some(B), None)));
    fixture.settle().await;
    assert_eq!(b.activity.delivered(), 0);
    assert!(!b.delivery.decided());
    tokio::time::sleep(Duration::from_millis(5)).await;
    let (_, t_r) = fixture.reply(2, Some(B));
    fixture.push(message(&completed(B, "failed"), 7, (Some(B), Some(2))));
    fixture.settle().await;
    assert!(b.delivery.decided());
    let observed = fixture.observed();
    let kinds: Vec<_> = kinds(&observed)
        .into_iter()
        .map(|(kind, _, at)| (kind, at))
        .collect();
    assert_eq!(kinds, [("accepted", t_r)]);
    let sealed = b.delivery.seal();
    let terminal = sealed.terminal.map(|retained| retained.terminal).unwrap();
    assert_eq!(terminal.status, VendorTerminalStatus::Completed);
    assert_eq!(terminal.at, t_r, "restamped: never before the acceptance");
    fixture.settle().await;
    assert!(fixture.observed().is_empty(), "the second gives nothing");
}

/// The 16 unmapped items of B the lane takes before its reply.
fn sixteen(fixture: &Fixture) {
    for n in 0..16 {
        fixture.push(message(
            &tool_started(B, &format!("tool-{n}")),
            5 + n,
            (Some(B), None),
        ));
    }
}

/// x.3.2 X3 S12 (owner's r10 ruling): retained items keep their lane
/// charges. After `Start(B)` was taken, 16 B items are retained and still
/// charged; the 17th overflows the lane and the generation fails
/// `overflow`, with no acceptance, continuity unproven, the loss from the
/// first retained position and B's frontier never there.
#[tokio::test]
async fn s12_retention_overflows_the_lane() {
    for reply in [false, true] {
        let mut fixture = Fixture::new();
        let b = fixture.start(2);
        fixture.settle().await;
        sixteen(&fixture);
        fixture.settle().await;
        assert_eq!(fixture.lane.charged().0, 16, "retained, still charged");
        if reply {
            // The variant: `Reply(B)` does not fit.
            let (_, pushed) = fixture
                .lane
                .push_reply(turn(2), Instant::now(), Some(B.into()));
            assert!(!pushed);
        } else {
            let item = message(&tool_started(B, "tool-16"), 21, (Some(B), None));
            assert!(!fixture.lane.push(item, 64), "the 17th overflows");
        }
        fixture.settle().await;
        assert_eq!(
            fixture.registration.failure(),
            Some(DriverFailure::Route(RouteError::Overflow { turn: turn(2) })),
            "reply: {reply}"
        );
        assert!(fixture.observed().is_empty(), "no acceptance");
        assert!(fixture.registration.incomplete());
        assert_eq!(
            fixture
                .record()
                .map(|record| (record.first_unqueued, record.omitted)),
            Some((5, UNKNOWN))
        );
        assert_eq!(b.activity.delivered(), 0);
        assert!(matches!(b.delivery.seal().stop, Some(Stop::Generation)));
        drop(fixture.guard());
        assert_eq!(fixture.folded(), Some(WireCleanup::Uncertain));
    }
}

/// x.3.2 X3 S12 (c) (r11 #3): an overflow beside a retained terminal. B's
/// terminal and 14 more items are retained; the reply fits; the terminal
/// is released and retained, the 14 frozen behind it, still charged. Two
/// more items fit, the third overflows: B keeps its result, and the tail
/// is the loss, from its first position.
#[tokio::test]
async fn s12c_an_overflow_beside_a_retained_terminal() {
    let fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.settle().await;
    fixture.push(message(&completed(B, "completed"), 5, (Some(B), None)));
    for n in 0..14 {
        fixture.push(message(
            &tool_started(B, &format!("tool-{n}")),
            6 + n,
            (Some(B), None),
        ));
    }
    fixture.reply(2, Some(B));
    fixture.settle().await;
    assert!(b.delivery.decided());
    assert_eq!(fixture.lane.charged().0, 14, "the frozen tail");
    for seq in [20, 21] {
        fixture.push(message(&delta(B), seq, (Some(B), Some(2))));
    }
    let third = message(&delta(B), 22, (Some(B), Some(2)));
    assert!(!fixture.lane.push(third, 64));
    fixture.settle().await;
    assert!(fixture.lane.overflowed_now());
    assert_eq!(
        fixture.registration.failure(),
        Some(DriverFailure::Route(RouteError::Overflow { turn: turn(2) }))
    );
    let sealed = b.delivery.seal();
    assert_eq!(
        sealed.terminal.map(|retained| retained.terminal.status),
        Some(VendorTerminalStatus::Completed),
        "the retained result stands"
    );
    // From the earliest of the registration's seal and the tail's first
    // position: never past the tail.
    let record = fixture.record().unwrap();
    assert!(record.first_unqueued <= 6, "{record:?}");
    assert_eq!(record.omitted, UNKNOWN);
    assert!(fixture.registration.incomplete());
}

/// x.3.2 X3 S12 (d) (r12 #2): the retained entry's 64 B grow at
/// retention. With the item's bytes at `LANE_BYTES - 63` the growth does
/// not fit: the lane ends `Overflow` at once, with no further ingress, and
/// the generation fails with the loss from this entry. At `LANE_BYTES -
/// 64` it fits and the item is retained.
#[tokio::test]
async fn s12d_retention_growth_meets_the_byte_bound() {
    for (bytes, fits) in [(LANE_BYTES - 64, true), (LANE_BYTES - 63, false)] {
        let fixture = Fixture::new();
        fixture.start(2);
        fixture.settle().await;
        let item = message(&tool_started(B, "tool-b"), 5, (Some(B), None));
        assert!(fixture.lane.push(item, bytes));
        fixture.settle().await;
        assert_eq!(fixture.lane.overflowed_now(), !fits, "{bytes}");
        if fits {
            assert_eq!(fixture.lane.charged(), (1, LANE_BYTES));
            assert_eq!(fixture.registration.failure(), None);
        } else {
            assert_eq!(
                fixture.registration.failure(),
                Some(DriverFailure::Route(RouteError::Overflow { turn: turn(2) }))
            );
            assert_eq!(
                fixture.record().map(|record| record.first_unqueued),
                Some(5)
            );
            assert!(fixture.registration.incomplete());
        }
    }
}

/// Review cfix-1 #1: a maximal message (`MESSAGE_BYTES`) read before its
/// turn's `turn/start` reply is retained with its 64 B growth, and the
/// reply marker still fits: the lane's room covers its bookkeeping. The
/// item is released to the turn; nothing fails.
#[tokio::test]
async fn a_maximal_early_message_is_retained_and_released() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.settle().await;
    let item = message(&tool_started(B, "tool-b"), 5, (Some(B), None));
    assert!(fixture.lane.push(item, MESSAGE_BYTES), "the lane admits it");
    fixture.settle().await;
    assert!(
        !fixture.lane.overflowed_now(),
        "retention grew within the lane"
    );
    assert_eq!(fixture.lane.charged(), (1, MESSAGE_BYTES + 64));
    let (contradicted, _) = fixture.reply(2, Some(B));
    assert!(contradicted);
    fixture.settle().await;
    assert_eq!(b.activity.delivered(), 2, "released through the reply");
    assert_eq!(fixture.registration.failure(), None);
    assert_eq!(fixture.record(), None);
    assert!(!fixture.observed().is_empty());
}

/// Critical re-review x5 r3: whoever observes a lane overflow first, the
/// loss record names the lane's overflow owner. The review's
/// interleaving: B's early item, `LANE_BYTES - 63` bytes, waits untaken;
/// turn 1's late message (its vendor turn mapped to turn 1) is refused
/// for its bytes, and the connection's overflow handler has not run (here
/// it never does). B's consumer then observes the overflow, its item's
/// retention growth failing too, and fails the generation: the record
/// names turn 1, though turn 2 is the latest. In the other order,
/// retention growth overflows the lane first and turn 1's message is
/// refused after it: the loss is the running turn's own, turn 2.
#[tokio::test]
async fn the_overflow_owner_is_the_trigger_whoever_observes_it() {
    for drop_first in [true, false] {
        let fixture = Fixture::new();
        fixture.start(2);
        fixture.settle().await;
        let late = || message(&delta(T), 6, (Some(T), Some(1)));
        let early = message(&tool_started(B, "tool-b"), 5, (Some(B), None));
        assert!(fixture.lane.push(early, LANE_BYTES - 63));
        if drop_first {
            assert!(!fixture.lane.push(late(), 64), "it does not fit");
        }
        fixture.settle().await;
        assert!(fixture.lane.overflowed_now());
        if !drop_first {
            assert!(!fixture.lane.push(late(), 64), "the lane ended");
        }
        let (trigger, owner) = if drop_first {
            (turn(1), Some(turn(1)))
        } else {
            (turn(2), None)
        };
        assert_eq!(
            fixture.record().map(|record| record.trigger),
            Some(trigger),
            "drop first: {drop_first}"
        );
        assert_eq!(fixture.lane.overflow_owner(), owner);
        assert_eq!(
            fixture.registration.failure(),
            Some(DriverFailure::Route(RouteError::Overflow { turn: turn(2) }))
        );
    }
}

/// x.3.2 X3 S13 (packet lines 144–145), the consumer's side: an item
/// naming an unmapped turn is retained, then B's start is refused: the
/// refusal is contradicted, so the generation fails `protocol`, and the
/// loss starts at the retained position.
#[tokio::test]
async fn s13_a_contradicted_refusal_fails_the_generation() {
    let fixture = Fixture::new();
    fixture.start(2);
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), None)));
    let (contradicted, _) = fixture.reply(2, None);
    assert!(contradicted);
    fixture.settle().await;
    assert_eq!(
        fixture.registration.failure(),
        Some(DriverFailure::Route(RouteError::Protocol {
            turn: turn(2),
            detail: CONTRADICTED,
        }))
    );
    assert_eq!(
        fixture
            .record()
            .map(|record| (record.first_unqueued, record.omitted)),
        Some((5, UNKNOWN))
    );
    assert!(fixture.registration.incomplete());
}

/// x.3.2 X3 S14's common prefix (r11 #1): after `Start(B)`, B's tool
/// start is retained at 5; mapped turn 1's denial at 6 goes out late under
/// the registration's seal, which moves to 7. No reply has come.
async fn s14_prefix(fixture: &mut Fixture) -> Started {
    fixture.mapped(1);
    let b = fixture.start(2);
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), None)));
    fixture.push(message(&denial(T), 6, (Some(T), Some(1))));
    fixture.settle().await;
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect();
    assert_eq!(kinds, ["denied"]);
    assert_eq!(fixture.registration.outstanding(), Some(5));
    b
}

/// The loss record's first position and count.
fn lost_from(fixture: &Fixture) -> Option<(u64, u64)> {
    fixture
        .record()
        .map(|record| (record.first_unqueued, record.omitted))
}

/// x.3.2 X3 S14 (a): the close meets `early` non-empty. Taking the lane's
/// end first, the consumer publishes `Unproven(Closed)`, never `Cut`; with
/// B's seal first, B's retained item is its loss and the cut is then
/// `Cut`. Either way the loss starts at 5, not 7, continuity is unproven
/// and retirement folds `Uncertain`, though the ledger holds no open
/// tool.
#[tokio::test]
async fn s14a_a_close_with_a_retained_item_proves_no_prefix() {
    for seal_first in [false, true] {
        let mut fixture = Fixture::new();
        let b = s14_prefix(&mut fixture).await;
        if seal_first {
            b.delivery.seal();
        }
        fixture.lane.end(LaneEnd::Closed);
        if !seal_first {
            fixture.settle().await;
            b.delivery.seal();
        }
        let drained = fixture.detach().await;
        let expected = if seal_first {
            Drained::Cut
        } else {
            Drained::Unproven(LaneEnd::Closed)
        };
        assert_eq!(drained, Some(expected));
        assert_eq!(lost_from(&fixture).map(|(first, _)| first), Some(5));
        assert!(fixture.registration.incomplete());
        assert!(!fixture.registration.ledger().has_open());
        assert_eq!(fixture.folded(), Some(WireCleanup::Uncertain));
    }
}

/// x.3.2 X3 S14 (b): the driver is dropped without close: retirement's
/// unknown loss starts at the retained item (5), not at the seal (7).
#[tokio::test]
async fn s14b_a_drop_floors_the_loss_at_the_retained_item() {
    let mut fixture = Fixture::new();
    s14_prefix(&mut fixture).await;
    drop(fixture.guard());
    assert_eq!(lost_from(&fixture), Some((5, UNKNOWN)));
    assert!(fixture.registration.incomplete());
    assert_eq!(fixture.folded(), Some(WireCleanup::Uncertain));
}

/// x.3.2 X3 S14 (c): a close whose consumer was cancelled, or whose cut
/// never came by the deadline, records its unknown loss from 5.
#[tokio::test]
async fn s14c_a_cancelled_or_late_close_floors_the_loss() {
    for cancelled in [true, false] {
        let mut fixture = Fixture::new();
        s14_prefix(&mut fixture).await;
        if cancelled {
            fixture.cancel.cancel();
            fixture.settle().await;
        }
        let drained = fixture.detach().await;
        let expected = cancelled.then_some(Drained::ConsumerCancelled);
        assert_eq!(drained, expected);
        assert_eq!(lost_from(&fixture), Some((5, UNKNOWN)));
        assert!(fixture.registration.incomplete());
        assert_eq!(fixture.folded(), Some(WireCleanup::Uncertain));
    }
}

/// x.3.2 X3 S14 (d): `Reply(B)` is taken and the release pauses at the
/// item's take with `releasing` = 5; the driver is dropped: the loss
/// starts at 5.
#[cfg(feature = "test-failpoints")]
#[tokio::test]
async fn s14d_a_drop_mid_release_floors_the_loss() {
    const POINT: &str = "adapter.codex.consumer_take";
    let points = pause_at(POINT);
    let mut fixture = Fixture::new();
    s14_prefix(&mut fixture).await;
    fixture.reply(2, Some(B));
    reached(&points, POINT).await;
    assert_eq!(fixture.registration.outstanding(), Some(5));
    drop(fixture.guard());
    assert_eq!(lost_from(&fixture), Some((5, UNKNOWN)));
    assert_eq!(fixture.folded(), Some(WireCleanup::Uncertain));
    release(&points, POINT);
}

/// x.3.2 X3 S14 (e) (r12 #1): the handoff pauses between the entry's pop
/// and the publication; with `second`, another entry at 8 is still
/// queued. The driver is dropped while paused: the loss starts at 5.
#[cfg(feature = "test-failpoints")]
async fn s14e_a_drop_mid_handoff(second: bool) {
    const POINT: &str = "adapter.codex.early_handoff";
    let points = pause_at(POINT);
    let mut fixture = Fixture::new();
    s14_prefix(&mut fixture).await;
    if second {
        fixture.push(message(&tool_started(B, "tool-c"), 8, (Some(B), None)));
    }
    fixture.reply(2, Some(B));
    reached(&points, POINT).await;
    drop(fixture.guard());
    assert_eq!(lost_from(&fixture), Some((5, UNKNOWN)));
    assert_eq!(fixture.folded(), Some(WireCleanup::Uncertain));
    release(&points, POINT);
}

#[cfg(feature = "test-failpoints")]
#[tokio::test]
async fn s14e_a_drop_mid_handoff_of_the_last_entry() {
    s14e_a_drop_mid_handoff(false).await;
}

#[cfg(feature = "test-failpoints")]
#[tokio::test]
async fn s14e_a_drop_mid_handoff_before_a_second_entry() {
    s14e_a_drop_mid_handoff(true).await;
}

/// x.3.2 X3 S15 (r11 #2): before `Reply(B)`, B's terminal at 5 (t1), a
/// decline naming B at 6 (t2) and an item naming an unrelated vendor turn
/// at 7. The terminal is released and retained; the decline and the
/// unrelated item stay frozen, bound at the acceptance (B and `at` = t3;
/// unseen). After B's seal the tail is handled with no turn held: B's
/// decline goes out late at t3, attributed to B, and is noted in the
/// ledger; the unrelated item gives nothing, no loss and no gap.
#[tokio::test]
async fn s15_a_frozen_tail_goes_out_by_its_binding() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.push(message(&completed(B, "completed"), 5, (Some(B), None)));
    fixture.push(declined(91, 6, (Some(B), None)));
    fixture.push(message(&delta("vendor-x"), 7, (Some("vendor-x"), None)));
    tokio::time::sleep(Duration::from_millis(5)).await;
    let (_, t3) = fixture.reply(2, Some(B));
    fixture.settle().await;
    assert!(b.delivery.decided());
    assert_eq!(fixture.lane.charged().0, 2, "the tail, frozen and charged");
    let sealed = b.delivery.seal();
    assert_eq!(
        sealed.terminal.map(|retained| retained.terminal.status),
        Some(VendorTerminalStatus::Completed)
    );
    fixture.settle().await;
    let observed = fixture.observed();
    let kinds = kinds(&observed);
    assert_eq!(
        kinds,
        [
            ("accepted", Some(B.to_owned()), t3),
            ("declined", Some(B.to_owned()), t3)
        ]
    );
    assert!(monotone(&observed));
    let request = ServerRequest {
        id: RequestId::Int(91),
        method: "item/commandExecution/requestApproval".to_owned(),
        thread_id: Some(THREAD.to_owned()),
        turn_id: Some(B.to_owned()),
        item_id: Some("item-1".to_owned()),
    };
    assert_eq!(
        fixture
            .registration
            .ledger()
            .wants_decline(turn(2), &request),
        None,
        "declined by VIA, in the ledger"
    );
    assert_eq!(fixture.record(), None);
    assert_eq!(fixture.lane.charged().0, 0);
}

/// x.3.2 X3 r5 #1: the consumer is held at the top of its loop while B's
/// start, reply and terminal arrive; released, it takes them in order and
/// B ends with its terminal.
#[cfg(feature = "test-failpoints")]
#[tokio::test]
async fn r5_1_traffic_behind_the_idle_check_is_the_turns() {
    const POINT: &str = "adapter.codex.idle_check";
    let points = pause_at(POINT);
    let mut fixture = Fixture::new();
    reached(&points, POINT).await;
    let b = fixture.start(2);
    fixture.reply(2, Some(B));
    fixture.push(message(&completed(B, "completed"), 6, (Some(B), Some(2))));
    release(&points, POINT);
    fixture.settle().await;
    assert!(b.delivery.decided());
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect();
    assert_eq!(kinds, ["accepted"]);
    assert_eq!(
        b.delivery
            .seal()
            .terminal
            .map(|retained| retained.terminal.status),
        Some(VendorTerminalStatus::Completed)
    );
}

/// x.3.2 X3 r5 #6: B's seal refuses B's denial at its take: B closes,
/// and the same item is handled again with no turn held: B's late denial
/// goes out under the registration's seal.
#[cfg(feature = "test-failpoints")]
#[tokio::test]
async fn r5_6_a_refused_take_is_handled_again() {
    const POINT: &str = "adapter.codex.consumer_take";
    let points = pause_at(POINT);
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.reply(2, Some(B));
    fixture.push(message(&denial(B), 6, (Some(B), Some(2))));
    reached(&points, POINT).await;
    b.delivery.seal();
    release(&points, POINT);
    fixture.settle().await;
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, named, _)| (kind, named))
        .collect();
    assert_eq!(
        kinds,
        [
            ("accepted", Some(B.to_owned())),
            ("denied", Some(B.to_owned()))
        ]
    );
    assert_eq!(b.activity.delivered(), 1, "the refused item counts nothing");
}

/// x.3.2 X3 r5 #6, §3.5: an item that did not go out whole counts nothing
/// toward the frontier: B's message that does not decode fails the
/// generation (its turn's delivery stops, unsealed), and B's frontier
/// stays at the reply's position.
#[tokio::test]
async fn r5_6_an_item_not_whole_holds_the_frontier() {
    let fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.reply(2, Some(B));
    fixture.settle().await;
    assert_eq!(b.activity.delivered(), 1, "through the reply");
    fixture.push(message(&json!({"undecoded": B}), 6, (Some(B), Some(2))));
    fixture.settle().await;
    assert!(matches!(
        fixture.registration.failure(),
        Some(DriverFailure::Route(RouteError::Protocol { .. }))
    ));
    assert_eq!(b.activity.delivered(), 1, "never through the lost item");
}

/// x.3.2 X3 r6 #5, §3.6: a retained terminal freezes the lane: mapped
/// turn 1's denial behind it is not taken until B seals, then goes out
/// late.
#[tokio::test]
async fn r6_5_a_retained_terminal_freezes_the_lane() {
    let mut fixture = Fixture::new();
    fixture.mapped(1);
    let b = fixture.start(2);
    fixture.reply(2, Some(B));
    fixture.push(message(&completed(B, "completed"), 6, (Some(B), Some(2))));
    fixture.push(message(&denial(T), 7, (Some(T), Some(1))));
    fixture.settle().await;
    assert!(b.delivery.decided());
    assert_eq!(fixture.lane.front_seq(), Some(7), "not taken");
    assert_eq!(fixture.observed().len(), 1);
    b.delivery.seal();
    fixture.settle().await;
    assert_eq!(fixture.lane.front_seq(), None);
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, named, _)| (kind, named))
        .collect();
    assert_eq!(kinds, [("denied", Some(T.to_owned()))]);
}

/// x.3.2 X3 r7 #7, §3.5: the frontier is published under the seal's lock:
/// a report before the seal publishes, none after it.
#[test]
fn r7_7_no_report_follows_a_seal() {
    let delivery = Delivery::new(4);
    let activity = TurnActivity::new(Instant::now());
    assert!(delivery.report(&activity, 2));
    assert_eq!(activity.delivered(), 2);
    delivery.seal();
    assert!(!delivery.report(&activity, 3));
    assert_eq!(activity.delivered(), 2);
}

/// Sol code r1 #9, r2 #4, design §10: a seal racing a report waits for
/// its publication. With the report paused between its seal check and its
/// publication, a seal on another thread, acknowledged as it is about to
/// take the seal's lock, blocks until the report is released; then the
/// report has published.
#[cfg(feature = "test-failpoints")]
#[test]
fn r1_9_a_racing_seal_waits_for_the_report() {
    use std::os::unix::fs::PermissionsExt;
    const TOKEN: &str = "codex-consumer-tests";
    const REPORT: &str = "adapter.codex.report";
    const SEAL: &str = "adapter.codex.seal";
    let points = tempfile::tempdir().unwrap();
    std::fs::set_permissions(points.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    for (point, mut command) in [
        (REPORT, json!({"action": "pause"})),
        (SEAL, json!({"action": "delay", "value": 0})),
    ] {
        command["token"] = json!(TOKEN);
        command["occurrence"] = json!(1);
        std::fs::write(
            points.path().join(format!("{point}.json")),
            command.to_string(),
        )
        .unwrap();
    }
    via_routes::failpoint::activate(points.path(), TOKEN).unwrap();
    let acknowledged = |point: &str| {
        let ack = points.path().join(format!("{point}.1.ack"));
        let by = std::time::Instant::now() + Duration::from_secs(5);
        while !ack.exists() {
            assert!(std::time::Instant::now() < by, "{point} was never reached");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    let delivery = Delivery::new(4);
    let activity = TurnActivity::new(Instant::now());
    let reporter = {
        let (delivery, activity) = (Arc::clone(&delivery), activity.clone());
        std::thread::spawn(move || delivery.report(&activity, 2))
    };
    acknowledged(REPORT);
    let (done, waited) = std::sync::mpsc::channel();
    let sealer = {
        let delivery = Arc::clone(&delivery);
        std::thread::spawn(move || {
            delivery.seal();
            done.send(()).unwrap();
        })
    };
    acknowledged(SEAL);
    assert!(
        waited.recv_timeout(Duration::from_millis(200)).is_err(),
        "the seal waits for the paused report"
    );
    release_hit(&points, REPORT, 1);
    assert!(reporter.join().unwrap(), "the report published");
    sealer.join().unwrap();
    assert_eq!(activity.delivered(), 2);
}

/// x.3.2 X3 §4.4: a generation failure drops the markers the lane still
/// holds and their credit with them, and the consumer ends
/// `ConsumerFailed`.
#[tokio::test]
async fn failed_disposal_drops_markers_and_credit() {
    let fixture = Fixture::new();
    let held = fixture.cap.held().0;
    fixture.mapped(1);
    let b = fixture.start(2);
    assert_eq!(fixture.cap.held().0, held + 2, "a range and B's credit");
    let protocol = DriverFailure::Route(RouteError::Protocol {
        turn: turn(2),
        detail: CONTRADICTED,
    });
    let loss = LossRecord {
        losses: Arc::clone(&fixture.losses),
        generation: 3,
    };
    fixture
        .registration
        .fail(&protocol, (&fixture.health, &fixture.lane, &loss));
    let drained = fixture
        .registration
        .drain(Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(drained, Some(Drained::ConsumerFailed));
    assert_eq!(fixture.cap.held().0, held + 1, "B's credit is released");
    assert!(b.delivery.decided(), "the failure stopped admitted B");
}

/// Sol code r1 #1, x.3.2 X3 §3.2: a lane that overflowed while B is
/// pending fails the generation before B's queued `Reply` is taken. With
/// the consumer held after `Start(B)`, B's early item and a fitting
/// `Reply(B)` are queued, then the lane overflows: no acceptance goes out
/// and B's frontier stays.
#[cfg(feature = "test-failpoints")]
#[tokio::test]
async fn r1_1_an_overflow_precedes_a_pending_reply() {
    const POINT: &str = "adapter.codex.idle_check";
    let points = pause_at_hit(POINT, 2);
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    reached_hit(&points, POINT, 2).await;
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), None)));
    fixture.reply(2, Some(B));
    let mut seq = 7;
    while fixture
        .lane
        .push(message(&delta(B), seq, (Some(B), None)), 64)
    {
        seq += 1;
    }
    assert!(fixture.lane.overflowed_now());
    release_hit(&points, POINT, 2);
    fixture.settle().await;
    assert_eq!(
        fixture.registration.failure(),
        Some(DriverFailure::Route(RouteError::Overflow { turn: turn(2) }))
    );
    assert!(fixture.observed().is_empty(), "no acceptance");
    assert_eq!(b.activity.delivered(), 0);
}

/// Sol code r1 #4, X0 item 5: a malformed thread-level message fails the
/// generation `protocol`; once its evidence is kept, running B's reported
/// failure names the shared connection's evidence and its byte count. A
/// protocol failure of another cause names none.
#[tokio::test]
async fn r1_4_a_malformed_message_is_named_by_the_failure() {
    let fixture = Fixture::new();
    let _b = fixture.start(2);
    fixture.reply(2, Some(B));
    // Past the full decode's nesting bound (64), within the peek's.
    let line = json!({"method": "thread/status/changed", "params": {"threadId": THREAD,
        "status": {"type": "idle"},
        "nested": serde_json::from_str::<Value>(&format!("{}{}", "[".repeat(80), "]".repeat(80))).unwrap()}});
    let bytes = line.to_string().len() + 1;
    fixture.push(message(&line, 5, (None, None)));
    fixture.settle().await;
    let detail = UNDECODED;
    assert_eq!(
        fixture.registration.failure(),
        Some(DriverFailure::Route(RouteError::Protocol {
            turn: turn(2),
            detail
        }))
    );
    let reported = |detail| TurnEnd {
        loss: None,
        aggregate: None,
        terminal: None,
        instance: None,
        leftovers: None,
        outcome: Err(AdapterError::Route(RouteFailure {
            cause: RouteError::Protocol {
                turn: turn(2),
                detail,
            },
            undecoded: None,
            exit: None,
            launched: true,
            cleanup: Some(WireCleanup::Uncertain),
            forced: false,
            journal_uncertain: false,
            acknowledged: false,
            shared: true,
            launch: None,
        })),
    };
    let undecoded = |end: TurnEnd| match end.outcome {
        Err(AdapterError::Route(failure)) => failure.undecoded,
        _ => panic!("a route failure"),
    };
    let end = with_undecoded(reported(detail), &fixture.registration).await;
    assert_eq!(
        undecoded(end),
        Some(format!(
            "{bytes} bytes kept as the shared connection's evidence"
        ))
    );
    let end = with_undecoded(reported(CONTRADICTED), &fixture.registration).await;
    assert_eq!(undecoded(end), None);
}

/// Takes `items` through the consumer in lane-sized batches, draining
/// what goes out.
async fn pump(fixture: &mut Fixture, items: impl IntoIterator<Item = LaneItem>) {
    let mut items = items.into_iter().peekable();
    while items.peek().is_some() {
        for item in items.by_ref().take(8) {
            fixture.push(item);
        }
        while fixture.lane.charged().0 > 0 {
            tokio::task::yield_now().await;
            fixture.observed();
        }
    }
    fixture.settle().await;
}

/// Sol code r2 #1, x.3.2 X3 §3.2: an overflow preempts B's acceptance
/// in flight. B's completed terminal is retained while pending; its
/// `Reply` is taken and `Accepted` waits for sink capacity; the lane then
/// overflows, and only after does capacity free. The generation fails
/// `overflow`: nothing is mapped or released, and B retains no terminal.
#[tokio::test]
async fn r2_1_an_overflow_preempts_a_pending_acceptance() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.push(message(&completed(B, "completed"), 5, (Some(B), None)));
    fixture.settle().await;
    assert_eq!(
        fixture.registration.outstanding(),
        Some(5),
        "the terminal is retained"
    );
    let full = fixture.cap.fill_budget().unwrap();
    assert_eq!(fixture.lane.open_start(), Some(turn(2)));
    fixture.reply(2, Some(B));
    // Taking B's `Reply` opens the start gate: the acceptance is in flight.
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.lane.open_start().is_some() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the reply is taken");
    fixture.settle().await;
    assert!(
        fixture.lane.charged().0 > 0,
        "the retained terminal keeps its charge"
    );
    let mut seq = 7;
    while fixture
        .lane
        .push(message(&delta(B), seq, (Some(B), None)), 64)
    {
        seq += 1;
    }
    assert!(fixture.lane.overflowed_now());
    fixture.settle().await;
    drop(full);
    fixture.settle().await;
    assert_eq!(
        fixture.registration.failure(),
        Some(DriverFailure::Route(RouteError::Overflow { turn: turn(2) }))
    );
    assert!(fixture.observed().is_empty(), "no acceptance");
    assert!(b.delivery.seal().terminal.is_none(), "no terminal");
    assert_eq!(b.activity.delivered(), 0);
}

/// Sol code r2 #2, x.3.2 X3 §3.2: a retained item the registration's seal
/// refuses is recorded as loss from its own position before it stops
/// being outstanding. B's terminal (4) and a denial (5) are retained while
/// pending; turn 1's later message (7) goes out under the registration's
/// seal; B is accepted, its terminal freezes the tail, and B seals. The
/// denial, released as B's late observation, then waits for its entry's
/// bytes (held at its idle seam while the cap is filled); the
/// registration is sealed past it; once the bytes free, its take is
/// refused. The loss starts at 5 and continuity is unproven.
#[cfg(feature = "test-failpoints")]
#[tokio::test]
async fn r2_2_a_refused_release_records_its_loss() {
    const POINT: &str = "adapter.codex.idle_item";
    let points = pause_at_hit(POINT, 2);
    let fixture = Fixture::new();
    fixture.mapped(1);
    let b = fixture.start(2);
    fixture.push(message(&completed(B, "completed"), 4, (Some(B), None)));
    fixture.push(message(&denied(B, "tool-x"), 5, (Some(B), None)));
    fixture.push(message(&delta(T), 7, (Some(T), Some(1))));
    fixture.settle().await;
    assert_eq!(fixture.registration.outstanding(), Some(4));
    fixture.reply(2, Some(B));
    fixture.settle().await;
    assert_eq!(
        fixture.registration.outstanding(),
        Some(5),
        "the tail is frozen"
    );
    b.delivery.seal();
    reached_hit(&points, POINT, 2).await;
    let full = fixture.cap.fill_budget().unwrap();
    release_hit(&points, POINT, 2);
    fixture.settle().await;
    assert_eq!(
        fixture.registration.outstanding(),
        Some(5),
        "the denial waits"
    );
    let sealed = fixture.registration.seal();
    assert!(
        sealed.position > 5,
        "sealed past the denial: {}",
        sealed.position
    );
    drop(full);
    fixture.settle().await;
    assert_eq!(fixture.registration.outstanding(), None);
    let record = fixture.record().expect("the denial's loss");
    assert!(
        record.first_unqueued <= 5,
        "loss from {}",
        record.first_unqueued
    );
    assert!(fixture.registration.incomplete());
}

/// Sol code r1 #6, x.3.2 X3 §6.2: at a full cap (B's credit and 1,023
/// open tools), B's declined completion of an open tool judges its denial
/// in that tool's entry: no further slot, no overflow, the denial out.
#[tokio::test]
async fn r1_6_a_denial_at_a_full_cap_keeps_its_entry() {
    let mut fixture = Fixture::new();
    let _b = fixture.start(2);
    fixture.reply(2, Some(B));
    fixture.settle().await;
    let tools = (0..1023u64).map(|n| {
        message(
            &tool_started(B, &format!("t{n}")),
            10 + n,
            (Some(B), Some(2)),
        )
    });
    pump(&mut fixture, tools).await;
    assert_eq!(fixture.cap.held().0, 1024, "the cap is full");
    fixture.push(message(&denied(B, "t0"), 2000, (Some(B), Some(2))));
    fixture.settle().await;
    assert_eq!(fixture.registration.failure(), None);
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect();
    assert_eq!(
        kinds,
        ["progress", "denied"],
        "its completion, then its denial"
    );
    assert_eq!(fixture.cap.held().0, 1024);
}

/// Sol code r1 #6, x.3.2 X3 §6.2: the charge reserved for a declined
/// completion's denial entry is held through the whole metadata update:
/// a competing waiter queued behind it never takes it between the
/// completion and the denial.
#[tokio::test]
async fn r1_6_a_reserved_charge_reaches_its_denial() {
    let mut fixture = Fixture::new();
    let _b = fixture.start(2);
    fixture.reply(2, Some(B));
    fixture.settle().await;
    fixture.observed();
    let mut full = fixture.cap.fill_budget().unwrap();
    fixture.push(message(&denied(B, "tool-b"), 6, (Some(B), Some(2))));
    fixture.settle().await;
    let cap = fixture.cap.clone();
    let competitor = tokio::spawn(async move {
        let slot = cap.slot("tool-b".len()).unwrap();
        cap.charge(slot).await.is_some()
    });
    fixture.settle().await;
    // The entry's bytes: its key and 64 B.
    drop(full.split("tool-b".len() + 64).unwrap());
    fixture.settle().await;
    assert_eq!(fixture.registration.failure(), None);
    assert!(!competitor.is_finished(), "the competitor still waits");
    drop(full);
    fixture.settle().await;
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect();
    assert_eq!(
        kinds,
        ["progress", "denied"],
        "its completion, then its denial"
    );
    assert!(competitor.await.unwrap());
}

/// Sol code r1 #5, x.3.2 X3 §6.2: one absolute stall deadline covers a
/// message's ledger wait and its sink waits. B's tool start waits 300 ms
/// for its entry's bytes, then for the sink's: the generation fails at
/// the message's deadline (stall 400 ms), not a fresh one after the
/// ledger wait.
#[cfg(feature = "test-failpoints")]
#[test]
fn r1_5_one_stall_deadline_covers_a_message() {
    const NAME: &str = "codex::delivery::consumer_tests::r1_5_one_stall_deadline_covers_a_message";
    if std::env::var_os("VIA_TEST_EVENT_STALL_MS").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env("VIA_TEST_EVENT_STALL_MS", "400")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut fixture = Fixture::new();
        let _b = fixture.start(2);
        fixture.reply(2, Some(B));
        fixture.settle().await;
        fixture.observed();
        let mut full = fixture.cap.fill_budget().unwrap();
        let started = Instant::now();
        fixture.push(message(&tool_started(B, "tool-b"), 6, (Some(B), Some(2))));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fixture.registration.failure(), None, "the ledger waits");
        drop(full.split("tool-b".len() + 64).unwrap());
        let failed = tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.registration.failure().is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Instant::now()
        })
        .await
        .unwrap();
        let elapsed = failed - started;
        assert!(
            elapsed >= Duration::from_millis(390) && elapsed < Duration::from_millis(600),
            "the message failed {elapsed:?} after its read"
        );
        drop(full);
    });
}

/// Vendor turn `named`'s per-call usage sample of `tokens` input tokens.
#[cfg(feature = "test-failpoints")]
fn token_usage(named: &str, tokens: u64) -> Value {
    let last = json!({"totalTokens": tokens, "inputTokens": tokens, "cachedInputTokens": 0,
        "outputTokens": 0, "reasoningOutputTokens": 0});
    json!({"method": "thread/tokenUsage/updated", "params": {"threadId": THREAD,
        "turnId": named, "tokenUsage": {"total": last, "last": last}}})
}

/// C2 §5 (picrit round 4): a usage sample lost while an interrupted
/// terminal drains leaves the turn unaccounted. B delivers a 100-token
/// sample, then its interrupted terminal is retained with a tool open;
/// a second sample read while it drains stalls in the full sink (stall
/// 400 ms), so the registration fails and the seal is partial. The
/// retained terminal does not make the delivered sum the turn's: the
/// predicate is false. Before the fix the retained terminal exempted the
/// partial delivery.
#[cfg(feature = "test-failpoints")]
#[test]
fn a_sample_lost_while_draining_unaccounts_the_turn() {
    const NAME: &str =
        "codex::delivery::consumer_tests::a_sample_lost_while_draining_unaccounts_the_turn";
    if std::env::var_os("VIA_TEST_EVENT_STALL_MS").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env("VIA_TEST_EVENT_STALL_MS", "400")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut fixture = Fixture::new();
        let b = fixture.start(2);
        fixture.reply(2, Some(B));
        fixture.push(message(&token_usage(B, 100), 5, (Some(B), Some(2))));
        fixture.push(message(&tool_started(B, "tool-b"), 6, (Some(B), Some(2))));
        fixture.push(message(&completed(B, "interrupted"), 7, (Some(B), Some(2))));
        fixture.settle().await;
        assert!(b.delivery.draining().is_some(), "the terminal drains");
        fixture.observed();
        let full = fixture.cap.fill_budget().unwrap();
        fixture.push(message(&token_usage(B, 50), 8, (Some(B), Some(2))));
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.registration.failure().is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let sealed = b.delivery.seal();
        assert!(sealed.terminal.is_some(), "the terminal stays retained");
        assert!(
            !super::super::driver::accounted(
                super::super::driver::Cut::Decided,
                &sealed,
                (&fixture.lane, &fixture.registration),
                None,
            ),
            "a lost sample left the delivered sum standing as the turn's usage"
        );
        drop(full);
    });
}

/// C2 §5 (picrit round 7): a rejection after the last message the turn's
/// delivery took is exempt only when delivery decided. B's interrupted
/// terminal (6) drains with its tool open; the connection then rejected
/// message 7. Cut at the P7 bound (`Grace`) or by a close (`Detach`), the
/// delivery never finished, so the sum is unaccounted; with no rejection
/// the same seal is accounted, and a decided delivery keeps the
/// exemption. Before the fix `Grace` and `Detach` were accounted too.
#[tokio::test]
async fn a_rejection_during_p7_unaccounts_an_undecided_cut() {
    use super::super::driver::{Cut, accounted};
    let fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.reply(2, Some(B));
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), Some(2))));
    fixture.push(message(&completed(B, "interrupted"), 6, (Some(B), Some(2))));
    fixture.settle().await;
    assert!(b.delivery.draining().is_some(), "the terminal drains");
    let sealed = b.delivery.seal();
    assert!(sealed.terminal.is_some() && !sealed.partial);
    assert_eq!(sealed.last_seq, 6);
    let rest = (&*fixture.lane, &*fixture.registration);
    for cut in [Cut::Grace, Cut::Detach] {
        assert!(accounted(cut, &sealed, rest, None), "{cut:?}: whole");
        assert!(
            !accounted(cut, &sealed, rest, Some(7)),
            "{cut:?}: a rejection while P7 accounted left the sum standing"
        );
    }
    assert!(accounted(Cut::Decided, &sealed, rest, Some(7)));
    assert!(!accounted(Cut::Decided, &sealed, rest, Some(6)));
}

/// A message of B's, mapped, read now: the item and its read instant.
fn of_b(line: &Value, seq: u64, owner: Option<u32>) -> (LaneItem, Instant) {
    let routed = routed(line, seq, (Some(B), owner));
    let at = routed.at;
    (LaneItem::Message(routed), at)
}

/// x.3.2 X4 D4.1 (W1, live): B's interrupted terminal, read with its tool
/// open, drains: it is retained with its original decode instant, not
/// decided, and the stop report is written. A second terminal read while
/// it drains is handled whole (the frontier passes it) with nothing
/// emitted or retained: the first stays.
#[tokio::test]
async fn w1_a_duplicate_terminal_while_draining_is_suppressed() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.reply(2, Some(B));
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), Some(2))));
    let (first, t0) = of_b(&completed(B, "interrupted"), 6, Some(2));
    fixture.push(first);
    fixture.settle().await;
    assert!(!b.delivery.decided(), "draining: the window is open");
    assert_eq!(b.delivery.draining(), Some(t0));
    assert!(
        *b.stop_ack.borrow(),
        "vendor evidence acknowledged the stop"
    );
    let before = b.activity.delivered();
    let seen = fixture.observed().len();
    tokio::time::sleep(Duration::from_millis(5)).await;
    fixture.push(message(&completed(B, "completed"), 7, (Some(B), Some(2))));
    fixture.settle().await;
    assert_eq!(b.activity.delivered(), before + 1, "handled whole");
    assert_eq!(
        fixture.observed().len(),
        0,
        "nothing emitted ({seen} before)"
    );
    assert_eq!(b.delivery.draining(), Some(t0), "still the first's window");
    let sealed = b.delivery.seal();
    let terminal = sealed.terminal.map(|retained| retained.terminal).unwrap();
    assert_eq!(terminal.status, VendorTerminalStatus::Interrupted);
    assert_eq!(sealed.decoded_at, Some(t0));
    assert!(sealed.tools_open);
    assert_eq!(fixture.registration.failure(), None);
}

/// x.3.2 X4 D4.1 (W1 from the early tail; W7, the consumer's half): B's
/// tool start, interrupted terminal and a second terminal, read before
/// its reply, are retained and released at the acceptance: the first
/// terminal drains, observed at the reply's instant but keeping its
/// original decode instant for P7; the second is suppressed, and the
/// release goes on past it.
#[tokio::test]
async fn w1_w7_an_early_terminal_drains_from_its_decode() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), None)));
    let (first, t0) = of_b(&completed(B, "interrupted"), 6, None);
    fixture.push(first);
    fixture.push(message(&completed(B, "completed"), 7, (Some(B), None)));
    fixture.settle().await;
    assert!(!*b.stop_ack.borrow(), "retained: not yet the turn's");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (_, t_r) = fixture.reply(2, Some(B));
    fixture.settle().await;
    assert!(t0 < t_r);
    assert!(!b.delivery.decided());
    assert_eq!(
        b.delivery.draining(),
        Some(t0),
        "P7 from the decode, not t_r"
    );
    assert!(*b.stop_ack.borrow());
    assert_eq!(fixture.lane.charged().0, 0, "the whole tail was released");
    let kinds: Vec<_> = kinds(&fixture.observed())
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect();
    assert_eq!(kinds, ["accepted", "progress"]);
    let sealed = b.delivery.seal();
    let terminal = sealed.terminal.map(|retained| retained.terminal).unwrap();
    assert_eq!(terminal.status, VendorTerminalStatus::Interrupted);
    assert_eq!(terminal.at, t_r, "observed at the acceptance (X3)");
    assert_eq!(sealed.decoded_at, Some(t0));
}

/// x.3.2 X4 D4.1: the draining turn's window closes when its tool ends:
/// a completion of another item does not end it; the matching one does,
/// and then the terminal decides with no tool open, frozen as `Retained`
/// (a later message is not taken).
#[tokio::test]
async fn a_draining_turn_decides_when_its_tools_end() {
    let mut fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.reply(2, Some(B));
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), Some(2))));
    fixture.push(message(&completed(B, "interrupted"), 6, (Some(B), Some(2))));
    fixture.settle().await;
    fixture.push(message(&tool_completed(B, "tool-x"), 7, (Some(B), Some(2))));
    fixture.settle().await;
    assert!(!b.delivery.decided(), "a wrong ID ends nothing");
    fixture.push(message(&tool_completed(B, "tool-b"), 8, (Some(B), Some(2))));
    fixture.settle().await;
    assert!(b.delivery.decided(), "the tools ended");
    assert_eq!(b.delivery.draining(), None);
    let ended: Vec<_> = fixture
        .observed()
        .into_iter()
        .filter_map(|item| {
            if let Observation::Progress(marks) = item.observation {
                Some(marks.tools_ended)
            } else {
                None
            }
        })
        .flatten()
        .collect();
    assert!(ended.contains(&"tool-b".to_owned()), "{ended:?}");
    fixture.push(message(&delta(B), 9, (Some(B), Some(2))));
    fixture.settle().await;
    assert!(
        fixture.observed().is_empty(),
        "retained: the lane is frozen"
    );
    let sealed = b.delivery.seal();
    assert!(!sealed.tools_open);
    assert_eq!(
        sealed.terminal.map(|retained| retained.terminal.status),
        Some(VendorTerminalStatus::Interrupted)
    );
}

/// x.3.2 X4 D4.1: a completed or failed terminal never drains, an open
/// tool or not; an interrupted one with no tool open decides at once.
/// Each interrupted one writes the stop report; the others do not.
#[tokio::test]
async fn only_an_interrupted_terminal_with_a_tool_open_drains() {
    for (status, tool, drains) in [
        ("completed", true, false),
        ("failed", true, false),
        ("interrupted", false, false),
        ("interrupted", true, true),
    ] {
        let fixture = Fixture::new();
        let b = fixture.start(2);
        fixture.reply(2, Some(B));
        if tool {
            fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), Some(2))));
        }
        fixture.push(message(&completed(B, status), 6, (Some(B), Some(2))));
        fixture.settle().await;
        assert_eq!(b.delivery.decided(), !drains, "{status} {tool}");
        assert_eq!(b.delivery.draining().is_some(), drains, "{status} {tool}");
        assert_eq!(*b.stop_ack.borrow(), status == "interrupted", "{status}");
    }
}

/// x.3.2 X4 W8: a driver dropped while its turn drains retires at once:
/// the registration is retired, the open tool folds `Uncertain`, and the
/// consumer ends; nothing waits for the P7 window.
#[tokio::test]
async fn w8_a_driver_dropped_while_draining_retires_at_once() {
    let fixture = Fixture::new();
    let b = fixture.start(2);
    fixture.reply(2, Some(B));
    fixture.push(message(&tool_started(B, "tool-b"), 5, (Some(B), Some(2))));
    fixture.push(message(&completed(B, "interrupted"), 6, (Some(B), Some(2))));
    fixture.settle().await;
    assert!(b.delivery.draining().is_some());
    drop(fixture.guard());
    assert!(fixture.registration.retired());
    assert_eq!(fixture.folded(), Some(WireCleanup::Uncertain));
    let Fixture {
        _consumer: consumer,
        ..
    } = fixture;
    tokio::time::timeout(Duration::from_secs(1), consumer)
        .await
        .expect("the consumer ends at once")
        .unwrap();
}
