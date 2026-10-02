//! One Codex registration's delivery (x.3.2 X0 items 5, 8.2, 10, 11,
//! 12.5, 13.2): the normalizer task that takes the registration's ingress
//! lane in decode order across its turns, each turn's `DeliverySeal`, and
//! the registration's own seal for what goes out while no turn runs.
//!
//! The normalizer runs on the session's tracker under `crash_on_panic`: a
//! panic in it is a VIA bug and aborts the daemon. It lives from the
//! thread's registration until its close's delivery barrier, the
//! registration's release or the session's cancellation (x.3.2 X3 fix r3
//! #3). It decodes each raw message as it consumes it, so the message's
//! staging permit is held until then. While a turn runs, a message of
//! that turn is normalized and its observations are handed to the C2
//! sink; the turn's terminal is retained in the seal's slot, never sent.
//! An earlier turn's denial or decline is that turn's late observation,
//! named by its vendor turn and judged against that turn's own history
//! (fix r3 #4), whether or not a turn runs; while none runs, only such
//! messages are taken, the rest waits for the next turn. A message of no
//! known turn gives nothing; one that does not decode keeps its evidence
//! in the folder of the turn its correlation names, else in the server's,
//! and fails the generation `protocol`. A decline is reported only for the
//! turn it names, once its reply was written whole.
//!
//! The running turn never waits on the normalizer. It waits for the
//! seal's decision (the retained terminal, or why delivery stopped)
//! beside its own orders, and seals at whatever cutoff comes first: every
//! later output finds the seal and is refused, so nothing of the turn
//! reaches Core after `run_turn` returned. The seal reports the first
//! message it may have left undelivered, a conservative lower bound.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use via_routes::codex::{
    Connection, DECLINE_DEADLINE, Incoming, Lane, LaneEnd, LaneEvent, LaneItem, Mark, Notification,
    Routed, ServerRequest, TurnFolder, decode,
};

use super::normalize::{self, Metadata, NormalizeError, Step, StructuredOutput, TurnNormalizer};
use crate::driver::latch;
use crate::observation::{
    Acceptance, Observation, ObservationItem, ObservationSink, Reserved, VendorTerminal, admitted,
};
use crate::runtime::event_stall;
use crate::{DriverFailure, DriverHealth, RouteError, TurnActivity, TurnNumber, VendorTurnId};

/// `omitted` when the count of lost messages is unknown or saturated
/// (X0 item 10).
pub(crate) const UNKNOWN: u64 = u64::MAX;

/// The driver's sticky loss record (X0 item 10). C2 has no carrier for it
/// yet (`TurnEnd.loss`, `CloseReport.loss` and Core's `record_loss` are
/// x.3.2 X5's), so the driver holds it for diagnostics and tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObservationLoss {
    /// The turn the first loss affected.
    pub(crate) trigger: TurnNumber,
    /// Its connection generation.
    pub(crate) generation: u64,
    /// No message of the generation before it was lost.
    pub(crate) first_unqueued: u64,
    /// How many were lost; [`UNKNOWN`] when not known.
    pub(crate) omitted: u64,
}

/// The driver's loss facts, under a leaf lock: the record, and the
/// session's latest turn, which a new record names.
#[derive(Default)]
pub(crate) struct Losses {
    pub(crate) record: Option<ObservationLoss>,
    pub(crate) latest: Option<TurnNumber>,
}

impl Losses {
    /// Installs a loss of generation `generation` from `first_unqueued`
    /// on, or merges it into the record held (item 10's rule: the
    /// earliest position, the counts added or unknown, the trigger and
    /// generation kept).
    pub(crate) fn note(&mut self, generation: u64, first_unqueued: u64, omitted: u64) {
        if let Some(record) = self.record.as_mut() {
            record.first_unqueued = record.first_unqueued.min(first_unqueued);
            record.omitted = if record.omitted == UNKNOWN || omitted == UNKNOWN {
                UNKNOWN
            } else {
                record.omitted.saturating_add(omitted)
            };
            return;
        }
        if let Some(trigger) = self.latest {
            self.record = Some(ObservationLoss {
                trigger,
                generation,
                first_unqueued,
                omitted,
            });
        }
    }
}

impl Losses {
    /// What a close's seal of generation `generation` left (X0 item 8.2;
    /// x.3.2 X3 fix r4 #4, #6): a prefix the barrier did not finish, or
    /// a message delivered only in part, is lost from the seal's
    /// position, its count unknown; the `dropped` messages the cutoff
    /// refused are lost, counted.
    pub(crate) fn note_close(
        &mut self,
        generation: u64,
        drained: bool,
        sealed: &Sealed,
        dropped: u64,
    ) {
        if !drained || sealed.partial {
            self.note(generation, sealed.position, UNKNOWN);
        }
        if dropped > 0 {
            self.note(generation, sealed.position, dropped);
        }
    }
}

/// Locks `losses`; each edit is one assignment.
pub(crate) fn losses(losses: &Mutex<Losses>) -> MutexGuard<'_, Losses> {
    losses.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The turn's retained terminal, never sent to the sink (C2 §4).
#[derive(Debug)]
pub(crate) struct Retained {
    pub(crate) terminal: VendorTerminal,
    pub(crate) structured: StructuredOutput,
}

/// Why delivery stopped before the turn's terminal.
#[derive(Debug)]
pub(crate) enum Stop {
    /// The lane ended after every message routed before its end.
    Lane(LaneEnd),
    /// A message of the generation contradicts the protocol or does not
    /// decode (X0 item 5 steps 5, 6): where its evidence was kept.
    Protocol {
        detail: &'static str,
        undecoded: Option<String>,
    },
    /// An ID past the normalizer's bounds, or the C2 sink stalled.
    Overflow,
}

/// What a seal found.
#[derive(Debug)]
pub(crate) struct Sealed {
    /// The first message the seal may have left undelivered.
    pub(crate) position: u64,
    /// The message being delivered was delivered only in part.
    pub(crate) partial: bool,
    pub(crate) terminal: Option<Retained>,
    pub(crate) tools_open: bool,
    pub(crate) stop: Option<Stop>,
}

struct Seal {
    sealed: Option<u64>,
    /// The decode sequence of the message being, or last, delivered.
    current: u64,
    /// Every output of `current` went out.
    complete: bool,
    terminal: Option<Retained>,
    tools_open: bool,
    stop: Option<Stop>,
}

/// One turn's `DeliverySeal` and decision slot (X0 item 13.2).
pub(crate) struct Delivery {
    seal: Mutex<Seal>,
    /// Wakes the turn when the decision changes.
    changed: Notify,
    /// Cancelled at the seal: a waiting output gives up at once.
    sealed: CancellationToken,
}

impl Delivery {
    /// A delivery whose last delivered message is `before`.
    pub(crate) fn new(before: u64) -> Arc<Self> {
        Arc::new(Self {
            seal: Mutex::new(Seal {
                sealed: None,
                current: before,
                complete: true,
                terminal: None,
                tools_open: false,
                stop: None,
            }),
            changed: Notify::new(),
            sealed: CancellationToken::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Seal> {
        // Each section is a few assignments: consistent across a panic.
        self.seal.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts delivering message `seq`; false once sealed.
    fn take(&self, seq: u64) -> bool {
        let mut seal = self.lock();
        if seal.sealed.is_some() {
            return false;
        }
        seal.current = seq;
        seal.complete = false;
        true
    }

    /// Whether message `seq` went out whole: its observations, or its
    /// retained terminal, were all handed on.
    fn whole(&self, seq: u64) -> bool {
        let seal = self.lock();
        seal.current == seq && seal.complete
    }

    /// The message went out whole with no (further) output.
    fn complete(&self, tools_open: bool) -> bool {
        let mut seal = self.lock();
        if seal.sealed.is_some() {
            return false;
        }
        seal.complete = true;
        seal.tools_open = tools_open;
        true
    }

    /// Sends `item` into its reserved room unless sealed; `last` completes
    /// the message.
    fn send(&self, reserved: Reserved<'_>, item: ObservationItem, last: Option<bool>) -> bool {
        let mut seal = self.lock();
        if seal.sealed.is_some() {
            return false;
        }
        reserved.send(item);
        if let Some(tools_open) = last {
            seal.complete = true;
            seal.tools_open = tools_open;
        }
        true
    }

    /// Publishes the turn's terminal into the retained slot unless
    /// sealed; it completes its message.
    fn retain(&self, retained: Retained, tools_open: bool) -> bool {
        let mut seal = self.lock();
        if seal.sealed.is_some() {
            return false;
        }
        seal.terminal = Some(retained);
        seal.complete = true;
        seal.tools_open = tools_open;
        drop(seal);
        self.changed.notify_one();
        true
    }

    /// Records why delivery stopped, unless sealed.
    fn stop(&self, stop: Stop) {
        let mut seal = self.lock();
        if seal.sealed.is_some() || seal.stop.is_some() {
            return;
        }
        seal.stop = Some(stop);
        drop(seal);
        self.changed.notify_one();
    }

    /// Whether the turn's delivery reached a decision: its terminal, or
    /// why it stopped.
    pub(crate) fn decided(&self) -> bool {
        let seal = self.lock();
        seal.terminal.is_some() || seal.stop.is_some()
    }

    /// Resolves at the next decision change (a change since the last wait
    /// is kept).
    pub(crate) async fn changed(&self) {
        self.changed.notified().await;
    }

    /// Seals delivery: nothing more goes out. The position is fixed by
    /// the first call; the slots are taken once.
    pub(crate) fn seal(&self) -> Sealed {
        let mut seal = self.lock();
        let next = if seal.complete {
            seal.current.saturating_add(1)
        } else {
            seal.current
        };
        let position = *seal.sealed.get_or_insert(next);
        let sealed = Sealed {
            position,
            partial: !seal.complete,
            terminal: seal.terminal.take(),
            tools_open: seal.tools_open,
            stop: seal.stop.take(),
        };
        drop(seal);
        self.sealed.cancel();
        sealed
    }
}

/// Where a malformed message's evidence goes and which turn it names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Owner {
    /// The running turn.
    This,
    /// An earlier VIA turn of the session.
    Earlier(TurnNumber),
    /// A vendor turn the connection never mapped here.
    Unknown,
    /// No turn: thread-level traffic.
    Thread,
}

/// What the normalizer does after a message.
enum Flow {
    Next,
    Done,
}

/// [`Flow::Next`] while delivery is open.
fn flow(open: bool) -> Flow {
    if open { Flow::Next } else { Flow::Done }
}

/// The evidence folder of each turn a generation ran, by turn.
pub(crate) type Folders = Arc<Mutex<BTreeMap<TurnNumber, Arc<TurnFolder>>>>;

/// Where a malformed message's evidence is kept (X0 item 5): the running
/// turn's folder, an earlier turn's of the generation, or the server
/// folder.
pub(crate) struct Evidence {
    pub(crate) server: Arc<dyn ServerEvidence>,
    pub(crate) earlier: Folders,
}

/// Where a message no turn owns keeps its evidence: the shared
/// connection's server folder (X0 item 5).
pub(crate) trait ServerEvidence: Send + Sync {
    /// Keeps `bytes` as the server folder's undecoded message.
    fn keep<'a>(
        &'a self,
        bytes: &'a [u8],
        what: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

impl ServerEvidence for Connection {
    fn keep<'a>(
        &'a self,
        bytes: &'a [u8],
        what: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(self.keep_undecoded(bytes, what))
    }
}

/// The latest turn that fenced the registration's lane.
#[derive(Clone)]
struct Fenced {
    turn: TurnNumber,
    fence: u64,
    activity: TurnActivity,
    /// The turn's `run_started` returned: nothing more runs under it.
    finished: bool,
}

/// An accepted turn, handed to its registration's normalizer.
pub(crate) struct Current {
    pub(crate) delivery: Arc<Delivery>,
    pub(crate) turn: TurnNumber,
    pub(crate) accepted: String,
    /// The acceptance, with when the connection read its reply and the
    /// reply's position under the turn's fence (x.3.2 X3 fix r2 #10).
    pub(crate) acceptance: (Acceptance, Instant, Option<Mark>),
    pub(crate) folder: Arc<TurnFolder>,
    pub(crate) activity: TurnActivity,
    pub(crate) schema: bool,
}

#[derive(Default)]
struct Slot {
    fenced: Option<Fenced>,
    accepted: Option<Current>,
}

/// One registration's state shared by its driver and its normalizer (X0
/// items 8.2, 13.2; x.3.2 X3 fix r3 #3, #4): the turn that fenced the
/// lane, the accepted turn handed over, the close's delivery barrier, and
/// the seal of what goes out while no turn runs.
pub(crate) struct Registration {
    slot: Mutex<Slot>,
    /// Wakes the normalizer when the slot changes.
    changed: Notify,
    /// The seal of what goes out while no turn runs: sealed at the close's
    /// delivery barrier, at an idle failure, or when the registration is
    /// released.
    idle: Arc<Delivery>,
    /// True once the normalizer took everything of an ended lane, or
    /// returned.
    drained: watch::Sender<bool>,
}

impl Registration {
    /// A registration whose last message before it is `before`.
    pub(crate) fn new(before: u64) -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(Slot::default()),
            changed: Notify::new(),
            idle: Delivery::new(before),
            drained: watch::channel(false).0,
        })
    }

    fn slot(&self) -> MutexGuard<'_, Slot> {
        // Each edit is one assignment: consistent across a panic.
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fences `lane` for turn `turn` (x.3.2 critical r2 #2, runtime §8),
    /// under the slot, so the normalizer never reads a fence position the
    /// slot does not name. From now until the returned guard drops a turn
    /// runs: the normalizer leaves the lane to it (x.3.2 X3 fix r4 #1).
    pub(crate) fn fence(
        self: &Arc<Self>,
        lane: &Lane,
        turn: TurnNumber,
        activity: &TurnActivity,
    ) -> TurnFence {
        let mut slot = self.slot();
        let fence = lane.fence(activity.decode_watermark());
        slot.fenced = Some(Fenced {
            turn,
            fence,
            activity: activity.clone(),
            finished: false,
        });
        TurnFence {
            registration: Arc::clone(self),
            turn,
        }
    }

    /// Hands the accepted turn to the normalizer.
    pub(crate) fn accept(&self, current: Current) {
        self.slot().accepted = Some(current);
        self.changed.notify_one();
    }

    /// The close's delivery barrier (X0 item 8.2), once the caller ended
    /// the lane at its cutoff: true once the normalizer took the admitted
    /// prefix, by `by`. The caller seals next.
    pub(crate) async fn drain(&self, by: Instant) -> bool {
        let mut drained = self.drained.subscribe();
        tokio::time::timeout_at(by, drained.wait_for(|drained| *drained))
            .await
            .is_ok_and(|drained| drained.is_ok())
    }

    /// Seals what goes out while no turn runs: the normalizer then
    /// returns. The first call fixes the position.
    pub(crate) fn seal(&self) -> Sealed {
        self.idle.seal()
    }
}

/// A turn that fenced its registration's lane; dropped once its
/// `run_started` returned, accepted or not.
pub(crate) struct TurnFence {
    registration: Arc<Registration>,
    turn: TurnNumber,
}

impl Drop for TurnFence {
    fn drop(&mut self) {
        let mut slot = self.registration.slot();
        if let Some(fenced) = slot
            .fenced
            .as_mut()
            .filter(|fenced| fenced.turn == self.turn)
        {
            fenced.finished = true;
        }
        drop(slot);
        self.registration.changed.notify_one();
    }
}

/// Marks a registration drained when its normalizer returns, however.
struct Returned(Arc<Registration>);

impl Drop for Returned {
    fn drop(&mut self) {
        self.0.drained.send_replace(true);
    }
}

/// The running turn's delivery state.
struct Running {
    delivery: Arc<Delivery>,
    turn: TurnNumber,
    accepted: String,
    folder: Arc<TurnFolder>,
    activity: TurnActivity,
    normalizer: TurnNormalizer,
}

/// Delivered positions under one turn's fence, reported in order.
struct Marks {
    fence: u64,
    /// Delivered positions past the first one not yet delivered: only the
    /// acceptance's, while messages read before its reply are delivered
    /// after it.
    ahead: BTreeSet<u64>,
}

/// The driver's loss record and the generation it names (x.3.2 X3 fix r4
/// #6).
pub(crate) struct LossRecord {
    pub(crate) losses: Arc<Mutex<Losses>>,
    pub(crate) generation: u64,
}

/// One registration's normalizer task (X0 item 13.2; x.3.2 X3 fix r3 #3,
/// #4, r4 #1–#6): it lives across the registration's turns until its lane
/// ends at a close's cutoff, the registration's seal or the session's
/// cancellation. While a turn runs it delivers the lane under that turn's
/// seal; while none runs it takes every message in order under the
/// registration's seal: an earlier turn's request or denial is that
/// turn's late observation, judged against the session's suppression
/// table, and the rest gives nothing. A failure while no turn runs
/// latches the driver's health at once and records the loss.
pub(crate) struct Normalizing {
    registration: Arc<Registration>,
    lane: Arc<Lane>,
    sink: ObservationSink,
    evidence: Evidence,
    cancel: CancellationToken,
    health: Arc<watch::Sender<DriverHealth>>,
    loss: LossRecord,
    running: Option<Running>,
    /// The session's metadata while no turn runs (a running turn holds
    /// it).
    metadata: Metadata,
    /// The last turn whose delivery ended: its fence counts nothing more.
    finished: Option<TurnNumber>,
    marks: Option<Marks>,
    /// Why delivery stopped while no turn ran.
    failed: Option<Stop>,
}

impl Normalizing {
    pub(crate) fn new(
        (registration, lane): (Arc<Registration>, Arc<Lane>),
        (sink, evidence): (ObservationSink, Evidence),
        (cancel, health): (CancellationToken, Arc<watch::Sender<DriverHealth>>),
        loss: LossRecord,
    ) -> Self {
        Self {
            registration,
            lane,
            sink,
            evidence,
            cancel,
            health,
            loss,
            running: None,
            metadata: Metadata::default(),
            finished: None,
            marks: None,
            failed: None,
        }
    }

    /// Delivers the registration's lane, turn after turn.
    pub(crate) async fn run(mut self) {
        let _returned = Returned(Arc::clone(&self.registration));
        loop {
            let accepted = self.registration.slot().accepted.take();
            let open = match accepted {
                Some(current) => self.turn(current).await,
                None => self.idle().await,
            };
            if !open {
                return;
            }
        }
    }

    /// Whether the registration's delivery ended.
    fn ended(&self) -> bool {
        self.cancel.is_cancelled() || self.registration.idle.sealed.is_cancelled()
    }

    /// While no turn runs, takes every message in order (x.3.2 X3 fix r4
    /// #1); while a turn runs but is not yet accepted, leaves the lane to
    /// it. Returns true once a turn is accepted, false once the
    /// registration's delivery ended. A lane that ended is left to the
    /// next turn, once everything before its end was taken.
    async fn idle(&mut self) -> bool {
        loop {
            if self.ended() {
                return false;
            }
            let running = {
                let slot = self.registration.slot();
                if slot.accepted.is_some() {
                    return true;
                }
                slot.fenced
                    .as_ref()
                    .is_some_and(|fenced| !fenced.finished && self.finished != Some(fenced.turn))
            };
            let event = if running {
                None
            } else {
                self.lane.try_next_if(|_| true)
            };
            match event {
                Some(LaneEvent::Item(item)) => {
                    idle_seam().await;
                    self.message(*item).await;
                    if let Some(stop) = self.failed.take() {
                        self.fail_idle(&stop);
                        return false;
                    }
                }
                Some(LaneEvent::End(end)) => {
                    self.registration.drained.send_replace(true);
                    if end == LaneEnd::Closed {
                        return false;
                    }
                    self.wait().await;
                }
                None => self.wait().await,
            }
        }
    }

    /// Waits for the lane or the slot to change, or the registration's
    /// delivery to end.
    async fn wait(&self) {
        tokio::select! {
            biased;
            () = self.registration.idle.sealed.cancelled() => {}
            () = self.cancel.cancelled() => {}
            () = self.registration.changed.notified() => {}
            () = self.lane.pushed() => {}
        }
    }

    /// X0 item 5 and item 10 while no turn runs (x.3.2 X3 fix r4 #5, #6):
    /// the failure latches the driver's health at once, so no later
    /// `turn/start` is written on the generation; what goes out is sealed
    /// and the rest joins the loss record.
    fn fail_idle(&self, stop: &Stop) {
        let latest = losses(&self.loss.losses).latest;
        let cause = match (stop, latest) {
            (Stop::Protocol { detail, .. }, Some(turn)) => {
                DriverFailure::Route(RouteError::Protocol { turn, detail })
            }
            (Stop::Overflow | Stop::Lane(LaneEnd::Overflow), Some(turn)) => {
                DriverFailure::Route(RouteError::Overflow { turn })
            }
            _ => DriverFailure::ObservationOverflow,
        };
        latch(&self.health, cause);
        let sealed = self.registration.seal();
        losses(&self.loss.losses).note(self.loss.generation, sealed.position, UNKNOWN);
    }

    /// Delivers the accepted turn until its terminal, its seal or the
    /// registration's end; once it is sealed the session's metadata comes
    /// back. False once the registration's delivery ended.
    async fn turn(&mut self, current: Current) -> bool {
        let Current {
            delivery,
            turn,
            accepted,
            acceptance,
            folder,
            activity,
            schema,
        } = current;
        let sealed = Arc::clone(&delivery);
        let metadata = std::mem::take(&mut self.metadata);
        self.running = Some(Running {
            delivery,
            turn,
            accepted,
            folder,
            activity,
            normalizer: TurnNormalizer::on(schema, turn, metadata),
        });
        let mut open = self.deliver_turn(acceptance).await;
        if open {
            open = tokio::select! {
                biased;
                () = self.cancel.cancelled() => false,
                () = self.registration.idle.sealed.cancelled() => false,
                () = sealed.sealed.cancelled() => true,
            };
        }
        if let Some(running) = self.running.take() {
            self.metadata = running.normalizer.finish();
            self.finished = Some(running.turn);
        }
        open
    }

    /// The running turn's acceptance, then its lane, until its delivery
    /// stops (true) or the registration's ends (false).
    async fn deliver_turn(
        &mut self,
        (acceptance, at, mark): (Acceptance, Instant, Option<Mark>),
    ) -> bool {
        if !self
            .output(Observation::Accepted(acceptance), None, at)
            .await
        {
            return true;
        }
        // Messages of the fence read before the reply go out after it:
        // its position counts once they did.
        if let Some(mark) = mark {
            self.mark_delivered(mark);
        }
        let delivery = Arc::clone(self.delivery());
        loop {
            let event = tokio::select! {
                biased;
                () = delivery.sealed.cancelled() => return true,
                () = self.cancel.cancelled() => return false,
                () = self.registration.idle.sealed.cancelled() => return false,
                event = self.lane.next() => event,
            };
            let item = match event {
                LaneEvent::Item(item) => *item,
                LaneEvent::End(end) => {
                    self.stop(Stop::Lane(end));
                    return true;
                }
            };
            if matches!(self.message(item).await, Flow::Done) {
                return true;
            }
        }
    }

    /// The seal outputs go under: the running turn's, else the
    /// registration's.
    fn delivery(&self) -> &Arc<Delivery> {
        self.running
            .as_ref()
            .map_or(&self.registration.idle, |running| &running.delivery)
    }

    fn tools_open(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|running| running.normalizer.tools_open())
    }

    /// The message went out whole with no (further) output.
    fn complete(&self) -> bool {
        self.delivery().complete(self.tools_open())
    }

    /// Delivery stops: the running turn's, else the registration's.
    fn stop(&mut self, stop: Stop) {
        match &self.running {
            Some(running) => running.delivery.stop(stop),
            None => {
                self.failed.get_or_insert(stop);
            }
        }
    }

    /// The session's metadata: the running turn holds it.
    fn metadata(&mut self) -> &mut Metadata {
        match self.running.as_mut() {
            Some(running) => running.normalizer.metadata(),
            None => &mut self.metadata,
        }
    }

    /// Reports position `mark` delivered under its fence, in order, while
    /// that fence's turn runs (x.3.2 critical r2 #2, runtime §8): a stale
    /// message counts for no turn.
    fn mark_delivered(&mut self, mark: Mark) {
        let fenced = self.registration.slot().fenced.clone();
        let Some(fenced) = fenced.filter(|fenced| {
            fenced.fence == mark.fence && !fenced.finished && self.finished != Some(fenced.turn)
        }) else {
            return;
        };
        if self
            .marks
            .as_ref()
            .is_none_or(|marks| marks.fence != mark.fence)
        {
            self.marks = Some(Marks {
                fence: mark.fence,
                ahead: BTreeSet::new(),
            });
        }
        let Some(marks) = self.marks.as_mut() else {
            return;
        };
        marks.ahead.insert(mark.seq);
        while let Some(&next) = marks.ahead.first() {
            if next > fenced.activity.delivered().saturating_add(1) {
                break;
            }
            marks.ahead.pop_first();
            fenced.activity.delivered_through(next);
        }
        // Only the acceptance's position waits ahead: the lane is left to
        // the running turn until its acceptance went out, and a message
        // not delivered whole ends the turn's delivery.
        debug_assert!(
            marks.ahead.len() <= 1,
            "fence positions ahead: {:?}",
            marks.ahead
        );
    }

    fn owner(&self, routed: &Routed) -> Owner {
        let running = self.running.as_ref();
        match (routed.owner, routed.turn.as_deref()) {
            (Some(owner), _) if running.is_some_and(|running| running.turn == owner) => Owner::This,
            (Some(owner), _) => Owner::Earlier(owner),
            (None, Some(turn)) if running.is_some_and(|running| running.accepted == turn) => {
                Owner::This
            }
            (None, Some(_)) => Owner::Unknown,
            (None, None) => Owner::Thread,
        }
    }

    /// One lane item, decoded now; its staging permit goes with it. Once
    /// it went out whole, its fence counts it delivered (x.3.2 critical r2
    /// #2, runtime §8).
    async fn message(&mut self, item: LaneItem) -> Flow {
        let (seq, mark) = (item.routed().seq, item.routed().mark);
        let flow = self.handle(item).await;
        if let Some(mark) = mark
            && self.delivery().whole(seq)
        {
            self.mark_delivered(mark);
        }
        flow
    }

    /// [`Self::message`]'s handling: the item's observations, at the
    /// instant the connection read it.
    async fn handle(&mut self, item: LaneItem) -> Flow {
        let owner = self.owner(item.routed());
        if !self.delivery().take(item.routed().seq) {
            return Flow::Done;
        }
        let at = item.routed().at;
        match item {
            LaneItem::Message(routed) => {
                let notification = match decode(routed.staged.bytes()) {
                    Ok(Incoming::Notification(notification)) => notification,
                    Ok(Incoming::Request(_) | Incoming::Response(_)) | Err(_) => {
                        return self.malformed(owner, routed.staged.bytes()).await;
                    }
                };
                let named = routed.turn;
                drop(routed.staged);
                if let (Owner::Earlier(earlier), Some(turn)) = (owner, named) {
                    return self.late(&notification, (earlier, &turn), at).await;
                }
                self.notification(owner, &notification, at).await
            }
            LaneItem::Declined {
                routed,
                request,
                decoded_at,
                written,
            } => {
                let flow = self.declined(owner, &request, decoded_at, written).await;
                drop(routed);
                flow
            }
        }
    }

    /// X0 item 5 steps 5 and 6: the evidence goes to the turn the message
    /// names, else to the server folder; the generation fails `protocol`.
    async fn malformed(&mut self, owner: Owner, bytes: &[u8]) -> Flow {
        let folder = match owner {
            Owner::This => self
                .running
                .as_ref()
                .map(|running| (Arc::clone(&running.folder), "the turn's message")),
            Owner::Earlier(turn) => self
                .evidence
                .earlier
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&turn)
                .cloned()
                .map(|folder| (folder, "an earlier turn's message")),
            Owner::Unknown | Owner::Thread => None,
        };
        let undecoded = match (owner, folder) {
            (Owner::This | Owner::Earlier(_), Some((folder, what))) => {
                folder.keep_undecoded(bytes, what).await;
                folder.take_undecoded()
            }
            (Owner::This | Owner::Earlier(_), None) => None,
            (Owner::Unknown | Owner::Thread, _) => {
                self.evidence
                    .server
                    .keep(bytes, "the shared connection's message")
                    .await;
                Some(format!(
                    "{} bytes kept as the shared connection's evidence",
                    bytes.len()
                ))
            }
        };
        self.stop(Stop::Protocol {
            detail: "a message of the session's thread did not decode",
            undecoded,
        });
        Flow::Done
    }

    /// A decoded notification: the running turn's, or thread-level, is
    /// normalized; another turn's, or one while no turn runs, gives
    /// nothing.
    async fn notification(
        &mut self,
        owner: Owner,
        notification: &Notification,
        at: Instant,
    ) -> Flow {
        let step = match self.running.as_mut() {
            Some(running) if matches!(owner, Owner::This | Owner::Thread) => {
                // Its read instant, not now (C2 §4): time in the lane
                // moves nothing.
                running.activity.record(at);
                running.normalizer.observe(notification, at)
            }
            Some(_) | None => return flow(self.complete()),
        };
        let step = match step {
            Ok(step) => step,
            Err(NormalizeError::Protocol(detail)) => {
                self.stop(Stop::Protocol {
                    detail,
                    undecoded: None,
                });
                return Flow::Done;
            }
            Err(NormalizeError::Overflow) => {
                self.stop(Stop::Overflow);
                return Flow::Done;
            }
        };
        let tools_open = self.tools_open();
        match step {
            Step::Activity => flow(self.complete()),
            Step::Observations(observations) => {
                let count = observations.len();
                if count == 0 {
                    return flow(self.complete());
                }
                for (index, observation) in observations.into_iter().enumerate() {
                    let last = (index + 1 == count).then_some(tools_open);
                    if !self.output(observation, last, at).await {
                        return Flow::Done;
                    }
                }
                Flow::Next
            }
            Step::Terminal {
                terminal,
                structured,
            } => {
                let retained = Retained {
                    terminal: *terminal,
                    structured,
                };
                self.delivery().retain(retained, tools_open);
                Flow::Done
            }
        }
    }

    /// An earlier turn's notification (x.3.2 X3 fix r2 #3, r4 #2): a
    /// denial of one of its items is that turn's late observation, named
    /// by its vendor turn `turn` (Core records it `late`, the turn's
    /// envelope unchanged), judged against the session's suppression
    /// table; anything else of it gives nothing.
    async fn late(
        &mut self,
        notification: &Notification,
        (earlier, turn): (TurnNumber, &str),
        at: Instant,
    ) -> Flow {
        let Ok(denial) = self.metadata().late_denial(earlier, notification) else {
            self.stop(Stop::Overflow);
            return Flow::Done;
        };
        match denial {
            Some(denial) => {
                let denied = Observation::ActionDenied(denial);
                let tools_open = self.tools_open();
                flow(self.output_as(turn, denied, Some(tools_open), at).await)
            }
            None => flow(self.complete()),
        }
    }

    /// X0 item 11: a placeholder of the running turn is reported once its
    /// reply was written whole by `decoded_at + 5 s`; an earlier turn's
    /// likewise, as that turn's late observation, noted in the session's
    /// suppression table (x.3.2 X3 fix r2 #3, r4 #2); one naming another
    /// turn, or none, gives nothing.
    async fn declined(
        &mut self,
        owner: Owner,
        request: &ServerRequest,
        decoded_at: Instant,
        mut written: watch::Receiver<Option<bool>>,
    ) -> Flow {
        let noted = match (owner, request.turn_id.as_deref(), self.running.as_mut()) {
            (Owner::This, _, Some(running)) => {
                running.activity.record(decoded_at);
                running
                    .normalizer
                    .note_decline(request)
                    .map(|()| running.accepted.clone())
            }
            (Owner::Earlier(earlier), Some(turn), _) => {
                let turn = turn.to_owned();
                self.metadata()
                    .note_decline(earlier, request)
                    .map(|()| turn)
            }
            (Owner::This | Owner::Earlier(_) | Owner::Unknown | Owner::Thread, _, _) => {
                return flow(self.complete());
            }
        };
        let Ok(named) = noted else {
            self.stop(Stop::Overflow);
            return Flow::Done;
        };
        let delivery = Arc::clone(self.delivery());
        let whole = tokio::select! {
            biased;
            () = delivery.sealed.cancelled() => return Flow::Done,
            outcome = tokio::time::timeout_at(
                decoded_at + DECLINE_DEADLINE,
                written.wait_for(Option::is_some),
            ) => outcome.is_ok_and(|outcome| outcome.is_ok_and(|outcome| *outcome == Some(true))),
        };
        if !whole {
            return flow(self.complete());
        }
        let declined = Observation::RequestDeclined(normalize::decline(request));
        let tools_open = self.tools_open();
        flow(
            self.output_as(&named, declined, Some(tools_open), decoded_at)
                .await,
        )
    }

    /// Hands one observation of the running turn to the sink: its room
    /// reserved outside the seal, the send made under it. `last` (with the
    /// open tools) completes the message. A stall latches the observation
    /// overflow and stops delivery; false once nothing more goes out.
    async fn output(&mut self, observation: Observation, last: Option<bool>, at: Instant) -> bool {
        let Some(accepted) = self
            .running
            .as_ref()
            .map(|running| running.accepted.clone())
        else {
            return false;
        };
        self.output_as(&accepted, observation, last, at).await
    }

    /// [`Self::output`] naming vendor turn `turn`: the running turn's, or
    /// an earlier turn's for its late observation.
    async fn output_as(
        &mut self,
        turn: &str,
        observation: Observation,
        last: Option<bool>,
        at: Instant,
    ) -> bool {
        let item = ObservationItem {
            at,
            vendor_turn: VendorTurnId::try_from(turn.to_owned()).ok(),
            observation,
        };
        let delivery = Arc::clone(self.delivery());
        let sent = {
            let reserved = tokio::select! {
                biased;
                () = delivery.sealed.cancelled() => return false,
                reserved = self.sink.reserve(&item, event_stall()) => reserved,
            };
            reserved.map(|reserved| delivery.send(reserved, item, last))
        };
        match sent {
            Ok(true) => {}
            Ok(false) => return false,
            Err(_) => {
                latch(&self.health, DriverFailure::ObservationOverflow);
                self.stop(Stop::Overflow);
                return false;
            }
        }
        admitted().await;
        true
    }
}

/// Test builds: a seam where an idle registration's normalizer holds a
/// message it took (x.3.2 X3 fix r3 #3): a close's delivery barrier must
/// then carry it, and its cutoff stops what comes after.
#[cfg_attr(
    not(feature = "test-failpoints"),
    expect(clippy::unused_async, reason = "only test builds wait at the seam")
)]
async fn idle_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit_async("adapter.codex.idle_item").await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use std::sync::{Arc, Mutex};

    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;
    use via_routes::codex::{BoundedBytes, Lane, LaneItem, Routed, VendorMessage};

    use super::{
        Delivery, Evidence, Folders, LossRecord, Losses, Normalizing, ObservationLoss,
        Registration, Retained, ServerEvidence, UNKNOWN,
    };
    use crate::DriverHealth;
    use crate::TurnNumber;
    use crate::VendorTerminalStatus;
    use crate::observation::{
        Observation, ObservationItem, ObservationSink, StopReason, VendorTerminal,
        observation_channel,
    };

    fn item(text: &str) -> ObservationItem {
        ObservationItem {
            at: tokio::time::Instant::now(),
            vendor_turn: None,
            observation: Observation::FinalText(text.to_owned()),
        }
    }

    async fn send(delivery: &Delivery, sink: &ObservationSink, text: &str, last: bool) -> bool {
        let item = item(text);
        let reserved = sink.reserve(&item, Duration::from_secs(1)).await.unwrap();
        delivery.send(reserved, item, last.then_some(false))
    }

    fn terminal() -> Retained {
        Retained {
            terminal: VendorTerminal {
                at: tokio::time::Instant::now(),
                status: VendorTerminalStatus::Completed,
                stop_reason: StopReason::EndTurn,
                vendor_stop_reason: "completed".to_owned(),
                vendor_code: None,
                class_hint: None,
                detail: None,
                structured_output: None,
                structured_output_unparsed: None,
                steps: None,
                usage: None,
                cost: None,
                vendor: None,
            },
            structured: super::StructuredOutput::NotRequested,
        }
    }

    /// X0 item 13.2 (R9-2): a seal right after a message's final send
    /// reports the next position, and a second seal the same one.
    #[tokio::test]
    async fn seal_right_after_final_send_is_stable() {
        let (sink, mut received) = observation_channel();
        let delivery = Delivery::new(10);
        assert!(delivery.take(11));
        assert!(send(&delivery, &sink, "whole", true).await);
        let sealed = delivery.seal();
        assert_eq!((sealed.position, sealed.partial), (12, false));
        assert_eq!(delivery.seal().position, 12);
        assert!(received.try_recv().is_ok());
    }

    /// X0 item 13.2 (R8-4): sealed after the first of a message's two
    /// observations, the message is the first undelivered one; a message
    /// with no output before it does not move the position past it, and
    /// nothing more goes out.
    #[tokio::test]
    async fn seal_between_observations_of_one_message() {
        let (sink, mut received) = observation_channel();
        let delivery = Delivery::new(10);
        assert!(delivery.take(11));
        assert!(delivery.complete(false));
        assert!(delivery.take(12));
        assert!(send(&delivery, &sink, "first", false).await);
        let sealed = delivery.seal();
        assert_eq!((sealed.position, sealed.partial), (12, true));
        assert!(!send(&delivery, &sink, "second", true).await);
        assert!(!delivery.take(13));
        let delivered: Vec<_> = std::iter::from_fn(|| received.try_recv().ok()).collect();
        assert_eq!(delivered.len(), 1);
    }

    /// X0 item 13.2 (R8-3, R9-1): a terminal retained before the seal is
    /// the seal's; one published after it is refused, and decides nothing.
    #[test]
    fn retained_terminal_before_the_seal_only() {
        let delivery = Delivery::new(0);
        assert!(delivery.take(1));
        assert!(delivery.retain(terminal(), true));
        assert!(delivery.decided());
        let sealed = delivery.seal();
        assert!(sealed.terminal.is_some());
        assert!(sealed.tools_open);
        assert_eq!(sealed.position, 2);

        let late = Delivery::new(0);
        assert!(late.take(1));
        let sealed = late.seal();
        assert!(sealed.terminal.is_none());
        assert!(!late.retain(terminal(), false));
        assert!(!late.decided());
        assert!(late.seal().terminal.is_none());
    }

    /// X0 item 13.2: a full sink's wait gives up at the seal (the
    /// normalizer's output selects on it), and the room it waited for is
    /// never used.
    #[tokio::test]
    async fn seal_ends_an_output_waiting_on_a_full_sink() {
        let (sink, mut received) = observation_channel();
        let mut filled = 0_usize;
        loop {
            let item = item("fill");
            match tokio::time::timeout(
                Duration::from_millis(1),
                sink.reserve(&item, Duration::from_secs(10)),
            )
            .await
            {
                Ok(Ok(reserved)) => {
                    reserved.send(item);
                    filled += 1;
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert!(filled > 0);
        let delivery = Delivery::new(0);
        assert!(delivery.take(1));
        let waiting = {
            let delivery = &delivery;
            let sink = &sink;
            async move {
                let item = item("blocked");
                tokio::select! {
                    biased;
                    () = delivery.sealed.cancelled() => false,
                    reserved = sink.reserve(&item, Duration::from_secs(10)) => {
                        delivery.send(reserved.unwrap(), item, Some(false))
                    }
                }
            }
        };
        let sealing = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            delivery.seal()
        };
        let (sent, sealed) = tokio::join!(waiting, sealing);
        assert!(!sent);
        assert_eq!(sealed.position, 1);
        let drained: usize = std::iter::from_fn(|| received.try_recv().ok()).count();
        assert_eq!(drained, filled);
        assert!(received.try_recv().is_err());
    }

    /// Keeps nothing: a unit test's server folder.
    struct NoEvidence;

    impl ServerEvidence for NoEvidence {
        fn keep<'a>(
            &'a self,
            _bytes: &'a [u8],
            _what: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
    }

    fn turn(number: u32) -> TurnNumber {
        TurnNumber::try_from(number).unwrap()
    }

    /// Message `seq`: turn `owner`'s command item completed `declined`,
    /// as the lane holds it after that turn ended.
    fn late_denial(seq: u64, owner: TurnNumber) -> LaneItem {
        let line = serde_json::json!({"method": "item/completed", "params": {
            "threadId": "thread-1", "turnId": "vendor-1",
            "item": {"type": "commandExecution", "id": "item-1", "command": "rm -rf build",
                "cwd": "/w", "commandActions": [], "status": "declined"}}})
        .to_string()
            + "\n";
        LaneItem::Message(Routed {
            staged: VendorMessage::new(BoundedBytes::try_from_message(line.into_bytes()).unwrap()),
            seq,
            turn: Some("vendor-1".to_owned()),
            owner: Some(owner),
            at: tokio::time::Instant::now(),
            mark: None,
        })
    }

    /// x.3.2 X3 fix r4 #5, #6: a late denial the sink refuses while no
    /// turn runs fails the generation at once: the driver's health latches
    /// and the loss is recorded from that message, its count unknown.
    #[tokio::test]
    async fn idle_sink_failure_records_its_loss() {
        let (sink, received) = observation_channel();
        drop(received);
        let lane = Arc::new(Lane::default());
        assert!(lane.push(late_denial(5, turn(1)), 64));
        let losses = Arc::new(Mutex::new(Losses {
            record: None,
            latest: Some(turn(2)),
        }));
        let health = Arc::new(watch::Sender::new(DriverHealth::Open));
        let normalizing = Normalizing::new(
            (Registration::new(4), Arc::clone(&lane)),
            (
                sink,
                Evidence {
                    server: Arc::new(NoEvidence),
                    earlier: Folders::default(),
                },
            ),
            (CancellationToken::new(), Arc::clone(&health)),
            LossRecord {
                losses: Arc::clone(&losses),
                generation: 3,
            },
        );
        tokio::time::timeout(Duration::from_secs(5), normalizing.run())
            .await
            .unwrap();
        assert!(matches!(*health.borrow(), DriverHealth::Failed { .. }));
        assert_eq!(
            losses.lock().unwrap().record,
            Some(ObservationLoss {
                trigger: turn(2),
                generation: 3,
                first_unqueued: 5,
                omitted: UNKNOWN,
            })
        );
    }

    /// X0 item 8.2 (x.3.2 X3 fix r4 #4, #6): a close whose barrier
    /// drained the lane still records the loss of a message its seal
    /// found delivered in part, and counts what the cutoff dropped; a
    /// whole, drained close records nothing.
    #[tokio::test]
    async fn close_records_a_partial_seal() {
        let (sink, _received) = observation_channel();
        let partial = Delivery::new(10);
        assert!(partial.take(11));
        assert!(send(&partial, &sink, "first of two", false).await);
        let mut losses = Losses {
            record: None,
            latest: Some(turn(1)),
        };
        losses.note_close(2, true, &partial.seal(), 0);
        assert_eq!(
            losses
                .record
                .map(|record| (record.first_unqueued, record.omitted)),
            Some((11, UNKNOWN))
        );

        let whole = Delivery::new(10);
        let mut kept = Losses {
            record: None,
            latest: Some(turn(1)),
        };
        kept.note_close(2, true, &whole.seal(), 0);
        assert!(kept.record.is_none());
        kept.note_close(2, true, &whole.seal(), 3);
        assert_eq!(
            kept.record
                .map(|record| (record.first_unqueued, record.omitted)),
            Some((11, 3))
        );
    }

    /// X0 item 10 (R6-8): a later loss keeps the record's trigger and
    /// generation, the earliest position and an unknown count.
    #[test]
    fn successive_losses_keep_earliest_sequence() {
        let mut losses = Losses {
            record: None,
            latest: Some(TurnNumber::try_from(1).unwrap()),
        };
        losses.note(3, 101, UNKNOWN);
        losses.latest = Some(TurnNumber::try_from(2).unwrap());
        losses.note(3, 50, 4);
        assert_eq!(
            losses.record,
            Some(ObservationLoss {
                trigger: TurnNumber::try_from(1).unwrap(),
                generation: 3,
                first_unqueued: 50,
                omitted: UNKNOWN,
            })
        );
        let mut counted = Losses {
            record: None,
            latest: Some(TurnNumber::try_from(1).unwrap()),
        };
        counted.note(1, 7, 2);
        counted.note(1, 9, 3);
        assert_eq!(
            counted
                .record
                .map(|record| (record.first_unqueued, record.omitted)),
            Some((7, 5))
        );
    }
}
