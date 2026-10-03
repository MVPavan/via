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
    Connection, ConnectionFailure, DECLINE_DEADLINE, Incoming, Lane, LaneEnd, LaneEvent, LaneItem,
    Mark, Notification, Routed, ServerRequest, TurnFolder, decode,
};

use super::normalize::{
    self, Ledger, Metadata, NormalizeError, Step, StructuredOutput, TurnNormalizer, ledger,
    ledger_on,
};
use crate::driver::latch;
use crate::observation::{
    Acceptance, Charge, Observation, ObservationItem, ObservationSink, Reserved, SessionCap,
    VendorTerminal, admitted,
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
        if let Some(trigger) = self.latest {
            self.note_turn(trigger, generation, first_unqueued, omitted);
        }
    }

    /// [`Self::note`] for a loss turn `trigger` affected (x.3.2 X3 §3.5):
    /// a new record names it.
    pub(crate) fn note_turn(
        &mut self,
        trigger: TurnNumber,
        generation: u64,
        first_unqueued: u64,
        omitted: u64,
    ) {
        if let Some(record) = self.record.as_mut() {
            record.first_unqueued = record.first_unqueued.min(first_unqueued);
            record.omitted = if record.omitted == UNKNOWN || omitted == UNKNOWN {
                UNKNOWN
            } else {
                record.omitted.saturating_add(omitted)
            };
            return;
        }
        self.record = Some(ObservationLoss {
            trigger,
            generation,
            first_unqueued,
            omitted,
        });
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
    /// The registration's generation failed, with the registration's
    /// cause (x.3.2 X3 §4.2 step 4).
    Generation,
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

    /// The session's metadata cap is exhausted: the connection fails
    /// `overflow` (x.3.2 X3 §6.4), which every session on it sees.
    fn overflow(&self);
}

impl ServerEvidence for Connection {
    fn keep<'a>(
        &'a self,
        bytes: &'a [u8],
        what: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(self.keep_undecoded(bytes, what))
    }

    fn overflow(&self) {
        self.fail(ConnectionFailure::Overflow);
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
    /// The turn's credit (x.3.2 X3 §3.4), reserved at its admission.
    pub(crate) credit: Charge,
}

#[derive(Default)]
struct Slot {
    fenced: Option<Fenced>,
    accepted: Option<Current>,
    /// The generation's failure, first wins (x.3.2 X3 §4.2).
    failed: Option<DriverFailure>,
    /// Retired (§6.5): no admission, wait or ledger commit follows.
    retired: bool,
    /// Continuity is unproven: retirement folds `Uncertain` (§6.6).
    incomplete: bool,
    /// The close's barrier took the whole prefix (§12, F3's bridge).
    disposed: bool,
    /// The admitted turns, which the generation's failure stops (§4.2).
    admitted: BTreeMap<TurnNumber, AdmittedTurn>,
}

/// An admitted turn: what cancels its writes, and its delivery once
/// accepted.
#[derive(Clone)]
struct AdmittedTurn {
    cancel: Arc<dyn Fn() + Send + Sync>,
    delivery: Option<Arc<Delivery>>,
}

/// A turn admitted on its registration (x.3.2 X3 §4.2 step 2): it leaves
/// the admitted set as this drops.
pub(crate) struct Admission {
    registration: Arc<Registration>,
    turn: TurnNumber,
}

impl Admission {
    /// The turn's delivery, once accepted: the generation's failure stops
    /// it, at once if it already failed.
    pub(crate) fn deliver(&self, delivery: &Arc<Delivery>) {
        let mut slot = self.registration.slot();
        if slot.failed.is_some() {
            drop(slot);
            delivery.stop(Stop::Generation);
        } else if let Some(admitted) = slot.admitted.get_mut(&self.turn) {
            admitted.delivery = Some(Arc::clone(delivery));
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.registration.slot().admitted.remove(&self.turn);
    }
}

/// One registration's state shared by its driver and its normalizer (X0
/// items 8.2, 13.2; x.3.2 X3 fix r3 #3, #4): the turn that fenced the
/// lane, the accepted turn handed over, the close's delivery barrier, the
/// seal of what goes out while no turn runs, and the session's metadata
/// (X3 §6.1).
pub(crate) struct Registration {
    slot: Mutex<Slot>,
    /// The session's metadata: open tools and the suppression table.
    ledger: Ledger,
    /// Wakes the normalizer when the slot changes.
    changed: Notify,
    /// The seal of what goes out while no turn runs: sealed at the close's
    /// delivery barrier, at an idle failure, or when the registration is
    /// released.
    idle: Arc<Delivery>,
    /// `Some(true)` once the normalizer took everything of an ended lane;
    /// `Some(false)` once it returned without (x.3.2 X3 r7 #5).
    drained: watch::Sender<Option<bool>>,
    /// Cancelled at the generation's failure (x.3.2 X3 §4.2).
    failing: CancellationToken,
    /// Cancelled at retirement (§6.5).
    retiring: CancellationToken,
}

impl Registration {
    /// A registration whose last message before it is `before`, its
    /// metadata charged to the session's `cap`.
    pub(crate) fn new(before: u64, cap: SessionCap) -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(Slot::default()),
            ledger: ledger_on(cap),
            changed: Notify::new(),
            idle: Delivery::new(before),
            drained: watch::channel(None).0,
            failing: CancellationToken::new(),
            retiring: CancellationToken::new(),
        })
    }

    fn slot(&self) -> MutexGuard<'_, Slot> {
        // Each edit is one assignment: consistent across a panic.
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The session's metadata, locked; never held across an await.
    fn ledger(&self) -> MutexGuard<'_, Metadata> {
        ledger(&self.ledger)
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
        tokio::time::timeout_at(by, drained.wait_for(Option::is_some))
            .await
            .is_ok_and(|drained| drained.is_ok_and(|drained| *drained == Some(true)))
    }

    /// x.3.2 X3 §12 (F3's bridge): what the close's barrier returned; only
    /// `true` is positive evidence that the prefix was disposed of.
    pub(crate) fn drained_prefix(&self, drained: bool) {
        self.slot().disposed = drained;
    }

    /// x.3.2 X3 §4.2 step 2: admits turn `turn`, whose writes `cancel`
    /// cancels; refused once the generation failed or the registration
    /// retired.
    pub(crate) fn admit(
        self: &Arc<Self>,
        turn: TurnNumber,
        cancel: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<Admission> {
        let mut slot = self.slot();
        if slot.failed.is_some() || slot.retired {
            return None;
        }
        slot.admitted.insert(
            turn,
            AdmittedTurn {
                cancel,
                delivery: None,
            },
        );
        Some(Admission {
            registration: Arc::clone(self),
            turn,
        })
    }

    /// The generation's failure, once it failed.
    pub(crate) fn failure(&self) -> Option<DriverFailure> {
        self.slot().failed.clone()
    }

    /// Resolves once the generation failed.
    pub(crate) async fn failed(&self) {
        self.failing.cancelled().await;
    }

    /// Resolves once the generation failed or the registration retired.
    pub(crate) async fn gone(&self) {
        tokio::select! {
            () = self.failing.cancelled() => {}
            () = self.retiring.cancelled() => {}
        }
    }

    /// Continuity is unproven (x.3.2 X3 §6.6).
    fn mark_incomplete(&self) {
        self.slot().incomplete = true;
    }

    /// Whether continuity is unproven.
    #[cfg(test)]
    pub(crate) fn incomplete(&self) -> bool {
        self.slot().incomplete
    }

    /// Whether the registration retired.
    #[cfg(test)]
    pub(crate) fn retired(&self) -> bool {
        self.slot().retired
    }

    /// x.3.2 X3 §4.1, §4.2: the generation fails with `cause`, first wins
    /// and never after retirement, before any await: `failed` and
    /// `incomplete` are set and the admitted turns taken in one section;
    /// the driver's health latches; the lane ends; each admitted turn's
    /// delivery stops (a retained terminal stays) and its writes are
    /// cancelled; then the registration is sealed and the loss noted from
    /// its seal, count unknown.
    pub(crate) fn fail(
        &self,
        cause: &DriverFailure,
        (health, lane, loss): (&watch::Sender<DriverHealth>, &Lane, &LossRecord),
    ) {
        let admitted: Vec<AdmittedTurn> = {
            let mut slot = self.slot();
            if slot.failed.is_some() || slot.retired {
                return;
            }
            slot.failed = Some(cause.clone());
            slot.incomplete = true;
            slot.admitted.values().cloned().collect()
        };
        latch(health, cause.clone());
        lane.end(LaneEnd::Quarantined);
        for turn in admitted {
            if let Some(delivery) = &turn.delivery {
                delivery.stop(Stop::Generation);
            }
            (turn.cancel)();
        }
        self.failing.cancel();
        let sealed = self.seal();
        losses(&loss.losses).note(loss.generation, sealed.position, UNKNOWN);
    }

    /// x.3.2 X3 §6.5: retires the registration once, in one section (a
    /// later call does nothing). It is sealed; unless the close's barrier
    /// took the whole prefix, the loss is noted from its seal, count
    /// unknown, and continuity is unproven. Then `retired` refuses every
    /// later admission, wait and commit, and ends the consumer; `fold`
    /// runs when a tool is open or continuity is unproven, never inferring
    /// quiescence from what survived; then the ledger and its ranges are
    /// released.
    pub(crate) fn retire(&self, loss: &LossRecord, fold: impl FnOnce()) {
        let mut slot = self.slot();
        if slot.retired {
            return;
        }
        let sealed = self.seal();
        if !slot.disposed {
            losses(&loss.losses).note(loss.generation, sealed.position, UNKNOWN);
            slot.incomplete = true;
        }
        slot.retired = true;
        self.retiring.cancel();
        if slot.incomplete || self.ledger().has_open() {
            fold();
        }
        self.ledger().retire();
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

/// Marks a registration's normalizer returned, however: drained only if
/// it took the lane's end first.
struct Returned(Arc<Registration>);

impl Drop for Returned {
    fn drop(&mut self) {
        // Returning is no evidence the prefix was taken (x.3.2 X3 r7 #5).
        self.0.drained.send_if_modified(|drained| {
            let unset = drained.is_none();
            drained.get_or_insert(false);
            unset
        });
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
    /// Its credit, held while its vendor ID is (x.3.2 X3 §3.4, r8 #6).
    credit: Charge,
}

/// Delivered positions under one turn's fence, reported in order.
struct Marks {
    fence: u64,
    /// Delivered positions past the first one not yet delivered: only the
    /// acceptance's, while messages read before its reply are delivered
    /// after it.
    ahead: BTreeSet<u64>,
    /// The first position read under the fence whose observations were
    /// lost (x.3.2 X3 §3.5, r10 #1): no report reaches it.
    gap: Option<u64>,
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
                        self.dispose_failed();
                        return false;
                    }
                }
                Some(LaneEvent::End(end)) => {
                    self.registration.drained.send_replace(Some(true));
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
    /// the generation fails (X3 §4.2): the driver's health latches at
    /// once, an admitted turn's `turn/start` is cancelled, what goes out
    /// is sealed and the rest joins the loss record.
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
        self.registration
            .fail(&cause, (&self.health, &self.lane, &self.loss));
    }

    /// x.3.2 X3 §4.4: after the generation's failure, what the lane still
    /// holds up to its end is disposed of without waiting, and nothing
    /// goes out. A tool item or a decline updates the ledger with a charge
    /// taken at once; one the ledger cannot retain leaves continuity
    /// unproven and is lost.
    fn dispose_failed(&mut self) {
        while let Some(LaneEvent::Item(item)) = self.lane.try_next() {
            let owner = self.owner(item.routed());
            let decoded = match &*item {
                LaneItem::Message(routed) => Some(decode(routed.staged.bytes())),
                LaneItem::Declined { .. } => None,
            };
            let notification = match &decoded {
                Some(Ok(Incoming::Notification(notification))) => Some(notification),
                Some(Ok(Incoming::Request(_) | Incoming::Response(_)) | Err(_)) | None => None,
            };
            let charged = match self.wanted(owner, &item, notification) {
                None => true,
                Some(key) => {
                    let cap = self.registration.ledger().cap().clone();
                    match cap.slot(key) {
                        None => {
                            self.registration.ledger().exhaust();
                            self.evidence.server.overflow();
                            false
                        }
                        Some(slot) => cap
                            .try_charge(slot)
                            .map(|charge| self.registration.ledger().stage(charge))
                            .is_ok(),
                    }
                }
            };
            let kept = charged && self.tracked(owner, &item, notification).is_ok();
            self.registration.ledger().unstage();
            if !kept {
                self.registration.mark_incomplete();
                losses(&self.loss.losses).note(self.loss.generation, item.routed().seq, UNKNOWN);
            }
        }
    }

    /// Delivers the accepted turn until its terminal, its seal or the
    /// registration's end. False once the registration's delivery ended.
    async fn turn(&mut self, current: Current) -> bool {
        let Current {
            delivery,
            turn,
            accepted,
            acceptance,
            folder,
            activity,
            schema,
            credit,
        } = current;
        let sealed = Arc::clone(&delivery);
        self.running = Some(Running {
            delivery,
            turn,
            accepted,
            folder,
            activity,
            normalizer: TurnNormalizer::on(schema, turn, Arc::clone(&self.registration.ledger)),
            credit,
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
            self.finished = Some(running.turn);
            self.registration
                .ledger()
                .close(running.turn, running.credit);
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
        // Only a successful `Accepted` send maps the turn (x.3.2 X3 §3.4).
        if let Some(running) = &self.running {
            self.registration.ledger().map(running.turn);
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

    /// Whether the running turn has a tool open, in the ledger.
    fn tools_open(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|running| self.registration.ledger().tools_open(running.turn))
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

    /// Reports position `mark` delivered under its fence, in order, while
    /// that fence's turn runs (x.3.2 critical r2 #2, runtime §8): a stale
    /// message counts for no turn.
    fn mark_delivered(&mut self, mark: Mark) {
        let Some(fenced) = self.live(mark) else {
            return;
        };
        let marks = self.marks(mark.fence);
        if marks.gap.is_some_and(|gap| mark.seq >= gap) {
            return;
        }
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

    /// The turn that fenced `mark`, while its fence is live.
    fn live(&self, mark: Mark) -> Option<Fenced> {
        let fenced = self.registration.slot().fenced.clone();
        fenced.filter(|fenced| {
            fenced.fence == mark.fence && !fenced.finished && self.finished != Some(fenced.turn)
        })
    }

    /// The marks of fence `fence`, fresh for a new fence.
    fn marks(&mut self, fence: u64) -> &mut Marks {
        let marks = self.marks.get_or_insert_with(|| Marks {
            fence,
            ahead: BTreeSet::new(),
            gap: None,
        });
        if marks.fence != fence {
            *marks = Marks {
                fence,
                ahead: BTreeSet::new(),
                gap: None,
            };
        }
        marks
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
        self.registration.ledger().unstage();
        if let Some(mark) = mark
            && self.delivery().whole(seq)
        {
            self.mark_delivered(mark);
        }
        flow
    }

    /// [`Self::message`]'s handling: the item's observations, at the
    /// instant the connection read it. The ledger is updated first, in
    /// every state, whatever goes out (x.3.2 X3 §6.3), once the charge of
    /// an entry it may insert was reserved (§6.2).
    async fn handle(&mut self, item: LaneItem) -> Flow {
        let owner = self.owner(item.routed());
        let decoded = match &item {
            LaneItem::Message(routed) => Some(decode(routed.staged.bytes())),
            LaneItem::Declined { .. } => None,
        };
        let notification = match &decoded {
            Some(Ok(Incoming::Notification(notification))) => Some(notification),
            Some(Ok(Incoming::Request(_) | Incoming::Response(_)) | Err(_)) | None => None,
        };
        if let Some(key) = self.wanted(owner, &item, notification)
            && !self.stage(key, item.routed().seq).await
        {
            return Flow::Done;
        }
        let tracked = self.tracked(owner, &item, notification);
        if !self.delivery().take(item.routed().seq) {
            return Flow::Done;
        }
        if tracked.is_err() {
            self.stop(Stop::Overflow);
            return Flow::Done;
        }
        let (at, seq, mark) = (item.routed().at, item.routed().seq, item.routed().mark);
        match item {
            LaneItem::Message(routed) => {
                let notification = match decoded {
                    Some(Ok(Incoming::Notification(notification))) => notification,
                    Some(Ok(Incoming::Request(_) | Incoming::Response(_)) | Err(_)) | None => {
                        return self.malformed(owner, routed.staged.bytes()).await;
                    }
                };
                let named = routed.turn;
                drop(routed.staged);
                if let (Owner::Earlier(earlier), Some(turn)) = (owner, named) {
                    return self
                        .late(&notification, (earlier, &turn), (at, seq, mark))
                        .await;
                }
                self.notification(owner, &notification, at).await
            }
            LaneItem::Declined {
                routed,
                request,
                decoded_at,
                written,
            } => {
                let flow = self
                    .declined(owner, &request, (decoded_at, seq, mark), written)
                    .await;
                drop(routed);
                flow
            }
        }
    }

    /// The key bytes of the ledger entry `item` of `owner` may insert
    /// (x.3.2 X3 §6.2).
    fn wanted(
        &self,
        owner: Owner,
        item: &LaneItem,
        notification: Option<&Notification>,
    ) -> Option<usize> {
        let turn = self.entry_turn(owner)?;
        let ledger = self.registration.ledger();
        match (item, notification) {
            (LaneItem::Declined { request, .. }, _) => ledger.wants_decline(turn, request),
            (LaneItem::Message(_), Some(notification)) => ledger.wants(turn, notification),
            (LaneItem::Message(_), None) => None,
        }
    }

    /// The ledger's update for `item` of `owner` (x.3.2 X3 §6.3).
    fn tracked(
        &self,
        owner: Owner,
        item: &LaneItem,
        notification: Option<&Notification>,
    ) -> Result<(), NormalizeError> {
        match (item, notification) {
            (LaneItem::Declined { request, .. }, _) => {
                self.track(owner, |ledger, turn| ledger.note_decline(turn, request))
            }
            (LaneItem::Message(_), Some(notification)) => {
                self.track(owner, |ledger, turn| ledger.track(turn, notification))
            }
            (LaneItem::Message(_), None) => Ok(()),
        }
    }

    /// The turn whose ledger entry a message of `owner` may insert: the
    /// running turn's (its own or a thread-level message the normalizer
    /// judges), or an earlier turn's by the connection's mapping.
    fn entry_turn(&self, owner: Owner) -> Option<TurnNumber> {
        match (owner, &self.running) {
            (Owner::This | Owner::Thread, Some(running)) => Some(running.turn),
            (Owner::Earlier(turn), _) => Some(turn),
            (Owner::This | Owner::Thread | Owner::Unknown, _) => None,
        }
    }

    /// x.3.2 X3 §6.2 (F2): reserves the charge of the ledger entry the
    /// message may insert, before the ledger is updated. The cap slot is
    /// taken at once: a full cap fails the connection `overflow` (§6.4).
    /// The budget bytes are awaited within the stall bound, or until the
    /// driver's health or the generation fails (the overflow path), the
    /// registration retires or the session is cancelled (F3). False once
    /// delivery stopped: a wait that ended without its charge leaves
    /// continuity unproven and loses message `seq`.
    async fn stage(&mut self, key: usize, seq: u64) -> bool {
        let cap = self.registration.ledger().cap().clone();
        let Some(slot) = cap.slot(key) else {
            self.registration.ledger().exhaust();
            self.evidence.server.overflow();
            self.stop(Stop::Overflow);
            return false;
        };
        let slot = match cap.try_charge(slot) {
            Ok(charge) => {
                self.registration.ledger().stage(charge);
                return true;
            }
            Err(slot) => slot,
        };
        let mut health = self.health.subscribe();
        let registration = Arc::clone(&self.registration);
        // Err(true): the overflow path; Err(false): the consumer ends.
        let charged = tokio::select! {
            biased;
            _ = health.wait_for(|health| matches!(health, DriverHealth::Failed { .. })) => {
                Err(true)
            }
            () = registration.failed() => Err(true),
            () = registration.retiring.cancelled() => Err(false),
            () = self.cancel.cancelled() => Err(false),
            charged = tokio::time::timeout(event_stall(), cap.charge(slot)) => {
                if charged.is_err() {
                    latch(&self.health, DriverFailure::ObservationOverflow);
                }
                charged.ok().flatten().ok_or(true)
            }
        };
        match charged {
            Ok(charge) => {
                self.registration.ledger().stage(charge);
                true
            }
            Err(overflow) => {
                self.registration.mark_incomplete();
                losses(&self.loss.losses).note(self.loss.generation, seq, UNKNOWN);
                if overflow {
                    self.stop(Stop::Overflow);
                }
                false
            }
        }
    }

    /// x.3.2 X3 §3.4, §3.5: an observation of earlier turn `turn`, never
    /// mapped to Core, is lost, not relabeled: the loss is noted for that
    /// turn, and a live fence it was read under reports nothing from its
    /// position on (the gap).
    fn lose(&mut self, turn: TurnNumber, seq: u64, mark: Option<Mark>) -> Flow {
        losses(&self.loss.losses).note_turn(turn, self.loss.generation, seq, 1);
        if let Some(mark) = mark.filter(|mark| self.live(*mark).is_some()) {
            let marks = self.marks(mark.fence);
            marks.gap = Some(marks.gap.map_or(mark.seq, |gap| gap.min(mark.seq)));
        }
        flow(self.complete())
    }

    /// Updates the ledger for the turn that owns a message, when known:
    /// the running turn's, or an earlier turn's by the connection's
    /// mapping (x.3.2 X3 §6.3).
    fn track(
        &self,
        owner: Owner,
        update: impl FnOnce(&mut Metadata, TurnNumber) -> Result<(), NormalizeError>,
    ) -> Result<(), NormalizeError> {
        let turn = match (owner, &self.running) {
            (Owner::This, Some(running)) => running.turn,
            (Owner::Earlier(turn), _) => turn,
            (Owner::This | Owner::Unknown | Owner::Thread, _) => return Ok(()),
        };
        update(&mut self.registration.ledger(), turn)
    }

    /// X0 item 5 steps 5 and 6: the evidence goes to the turn the message
    /// names, else to the server folder; the generation fails `protocol`.
    async fn malformed(&mut self, owner: Owner, bytes: &[u8]) -> Flow {
        let detail = "a message of the session's thread did not decode";
        // While no turn runs the generation fails before the evidence is
        // kept (x.3.2 X3 §4.4, r6 #1, r5 #5).
        if self.running.is_none() {
            self.fail_idle(&Stop::Protocol {
                detail,
                undecoded: None,
            });
        }
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
        self.stop(Stop::Protocol { detail, undecoded });
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
        (at, seq, mark): (Instant, u64, Option<Mark>),
    ) -> Flow {
        let denial = self
            .registration
            .ledger()
            .late_denial(earlier, notification);
        let Ok(denial) = denial else {
            self.stop(Stop::Overflow);
            return Flow::Done;
        };
        match denial {
            Some(_) if !self.registration.ledger().mapped(earlier) => self.lose(earlier, seq, mark),
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
    /// likewise, as that turn's late observation (x.3.2 X3 fix r2 #3, r4
    /// #2; [`Self::handle`] noted it in the session's suppression table);
    /// one naming another turn, or none, gives nothing.
    async fn declined(
        &mut self,
        owner: Owner,
        request: &ServerRequest,
        (decoded_at, seq, mark): (Instant, u64, Option<Mark>),
        mut written: watch::Receiver<Option<bool>>,
    ) -> Flow {
        let named = match (owner, request.turn_id.as_deref(), self.running.as_ref()) {
            (Owner::This, _, Some(running)) => {
                running.activity.record(decoded_at);
                running.accepted.clone()
            }
            (Owner::Earlier(_), Some(turn), _) => turn.to_owned(),
            (Owner::This | Owner::Earlier(_) | Owner::Unknown | Owner::Thread, _, _) => {
                return flow(self.complete());
            }
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
        if let Owner::Earlier(earlier) = owner
            && !self.registration.ledger().mapped(earlier)
        {
            return self.lose(earlier, seq, mark);
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

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;
    use via_routes::codex::{BoundedBytes, Lane, LaneEnd, LaneItem, Routed, VendorMessage};

    use super::super::driver::RetireGuard;
    use super::{
        Delivery, Evidence, Folders, LossRecord, Losses, Normalizing, ObservationLoss,
        Registration, Retained, ServerEvidence, Stop, UNKNOWN,
    };
    use crate::driver::DriverState;
    use crate::observation::{
        Admitted, Observation, ObservationBudget, ObservationItem, ObservationSink, SessionCap,
        StopReason, VendorTerminal, observation_channel, observation_channel_in,
    };
    use crate::runtime::OBSERVATION_BYTES;
    use crate::{DriverFailure, DriverHealth, RouteError, TurnNumber, VendorTerminalStatus};
    use via_routes::WireCleanup;

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

        fn overflow(&self) {}
    }

    /// A unit test's connection: whether its overflow was latched.
    #[derive(Default)]
    struct Connection(AtomicBool);

    impl ServerEvidence for Connection {
        fn keep<'a>(
            &'a self,
            _bytes: &'a [u8],
            _what: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }

        fn overflow(&self) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// A cap on a budget of its own.
    fn cap() -> SessionCap {
        SessionCap::new(&observation_channel().0)
    }

    /// A registration whose last message before it is 4, on its own cap.
    fn fresh() -> Arc<Registration> {
        Registration::new(4, cap())
    }

    /// Maps turn `turn` of `registration` to Core, charged as its close
    /// charges a new range.
    fn mapped(registration: &Registration, turn: TurnNumber) {
        let mut ledger = registration.ledger();
        let cap = ledger.cap().clone();
        let credit = cap.credit_slot(1024).map(|slot| cap.try_charge(slot));
        let Some(Ok(credit)) = credit else {
            panic!("no credit");
        };
        ledger.map(turn);
        ledger.close(turn, credit);
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

    /// Message `seq`: turn `owner`'s command item `tool-1` with `method`
    /// (`item/started` or `item/completed`, successful), as the lane holds
    /// it after that turn ended.
    fn late_tool(seq: u64, owner: TurnNumber, method: &str) -> LaneItem {
        let status = if method == "item/started" {
            "inProgress"
        } else {
            "completed"
        };
        let line = serde_json::json!({"method": method, "params": {
            "threadId": "thread-1", "turnId": "vendor-1",
            "item": {"type": "commandExecution", "id": "tool-1", "command": "sleep 1",
                "cwd": "/w", "commandActions": [], "status": status}}})
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

    /// The normalizer of `registration` over `lane`, its observations
    /// taken by nobody.
    fn idle_normalizer(registration: &Arc<Registration>, lane: &Arc<Lane>) -> Normalizing {
        let (sink, _received) = observation_channel();
        Normalizing::new(
            (Arc::clone(registration), Arc::clone(lane)),
            (
                sink,
                Evidence {
                    server: Arc::new(NoEvidence),
                    earlier: Folders::default(),
                },
            ),
            (
                CancellationToken::new(),
                Arc::new(watch::Sender::new(DriverHealth::Open)),
            ),
            LossRecord {
                losses: Arc::new(Mutex::new(Losses::default())),
                generation: 1,
            },
        )
    }

    /// x.3.2 X3 r6 #3 and r5 #2 (release by completion), C1: while no
    /// turn runs, an earlier turn's late tool start opens its entry in the
    /// registration's ledger, and its late successful completion releases
    /// it.
    #[tokio::test]
    async fn late_tool_items_update_the_ledger() {
        let registration = fresh();
        let lane = Arc::new(Lane::default());
        assert!(lane.push(late_tool(5, turn(1), "item/started"), 64));
        lane.end(LaneEnd::Closed);
        tokio::time::timeout(
            Duration::from_secs(5),
            idle_normalizer(&registration, &lane).run(),
        )
        .await
        .unwrap();
        assert!(registration.ledger().tools_open(turn(1)));
        assert_eq!(registration.ledger().entries(), 1);

        let registration = fresh();
        let lane = Arc::new(Lane::default());
        assert!(lane.push(late_tool(5, turn(1), "item/started"), 64));
        assert!(lane.push(late_tool(6, turn(1), "item/completed"), 64));
        lane.end(LaneEnd::Closed);
        tokio::time::timeout(
            Duration::from_secs(5),
            idle_normalizer(&registration, &lane).run(),
        )
        .await
        .unwrap();
        assert!(!registration.ledger().tools_open(turn(1)));
        assert_eq!(registration.ledger().entries(), 0, "the entry is released");
    }

    /// x.3.2 X3 r6 #3, C1: the ledger is updated in every state, before
    /// any output decision. Turn 1's late tool start is held at the idle
    /// seam, between its pop and its take, while the registration is
    /// sealed: the take is refused, and the tool is open in the ledger all
    /// the same.
    #[cfg(feature = "test-failpoints")]
    #[tokio::test]
    async fn a_refused_take_still_opens_the_tool() {
        use std::os::unix::fs::PermissionsExt;
        const POINT: &str = "adapter.codex.idle_item";
        const TOKEN: &str = "codex-delivery-tests";
        let points = tempfile::tempdir().unwrap();
        std::fs::set_permissions(points.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = serde_json::json!({"token": TOKEN, "occurrence": 1, "action": "pause"});
        std::fs::write(
            points.path().join(format!("{POINT}.json")),
            command.to_string(),
        )
        .unwrap();
        via_routes::failpoint::activate(points.path(), TOKEN).unwrap();

        let registration = fresh();
        let lane = Arc::new(Lane::default());
        assert!(lane.push(late_tool(5, turn(1), "item/started"), 64));
        let run = tokio::spawn(idle_normalizer(&registration, &lane).run());
        let ack = points.path().join(format!("{POINT}.1.ack"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ack.exists() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        registration.seal();
        std::fs::write(points.path().join(format!("{POINT}.1.release")), b"").unwrap();
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap();
        assert!(registration.ledger().tools_open(turn(1)));
    }

    /// A normalizer run's fixture: its parts, and what a test reads after.
    struct Run {
        registration: Arc<Registration>,
        lane: Arc<Lane>,
        connection: Arc<Connection>,
        health: Arc<watch::Sender<DriverHealth>>,
        losses: Arc<Mutex<Losses>>,
        /// The session's cancellation.
        cancel: CancellationToken,
        /// The driver's cleanup facts, which retirement folds into.
        state: Arc<Mutex<DriverState>>,
    }

    impl Run {
        /// Turn 2 the latest, its registration on `cap`.
        fn new(cap: SessionCap) -> Self {
            Self {
                registration: Registration::new(4, cap),
                lane: Arc::new(Lane::default()),
                connection: Arc::default(),
                health: Arc::new(watch::Sender::new(DriverHealth::Open)),
                losses: Arc::new(Mutex::new(Losses {
                    record: None,
                    latest: Some(turn(2)),
                })),
                cancel: CancellationToken::new(),
                state: Arc::default(),
            }
        }

        /// The driver's retirement guard of the registration.
        fn guard(&self) -> RetireGuard {
            RetireGuard {
                registration: Arc::clone(&self.registration),
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

        /// Runs the registration's normalizer on `sink` until it returns.
        async fn run(&self, sink: ObservationSink) {
            let normalizing = Normalizing::new(
                (Arc::clone(&self.registration), Arc::clone(&self.lane)),
                (
                    sink,
                    Evidence {
                        server: Arc::clone(&self.connection) as Arc<dyn ServerEvidence>,
                        earlier: Folders::default(),
                    },
                ),
                (self.cancel.clone(), Arc::clone(&self.health)),
                LossRecord {
                    losses: Arc::clone(&self.losses),
                    generation: 3,
                },
            );
            tokio::time::timeout(Duration::from_secs(5), normalizing.run())
                .await
                .unwrap();
        }

        fn record(&self) -> Option<ObservationLoss> {
            self.losses.lock().unwrap().record
        }

        fn overflowed(&self) -> bool {
            self.connection.0.load(Ordering::Acquire)
        }
    }

    /// x.3.2 X3 fix r4 #5, #6: a late denial the sink refuses while no
    /// turn runs fails the generation at once: the driver's health latches
    /// and the loss is recorded from that message, its count unknown.
    /// x.3.2 X3 r6 #8 (F2): an ordinary sink stall fails only its own
    /// session, never the shared connection.
    #[tokio::test]
    async fn idle_sink_failure_records_its_loss() {
        let (sink, received) = observation_channel();
        drop(received);
        let run = Run::new(cap());
        mapped(&run.registration, turn(1));
        assert!(run.lane.push(late_denial(5, turn(1)), 64));
        run.run(sink).await;
        assert_eq!(
            *run.health.borrow(),
            DriverHealth::Failed {
                first_cause: DriverFailure::ObservationOverflow
            }
        );
        assert_eq!(
            run.record(),
            Some(ObservationLoss {
                trigger: turn(2),
                generation: 3,
                first_unqueued: 5,
                omitted: UNKNOWN,
            })
        );
        assert!(!run.overflowed(), "the connection is not failed");
    }

    /// x.3.2 X3 S2 and §3.4 (F2): "mapped to Core" is positive evidence. A
    /// late denial naming turn 1, which the connection maps but whose
    /// `Accepted` never went out, gives no observation and nothing
    /// session-level: it is turn 1's loss, counted, and the generation
    /// lives on.
    #[tokio::test]
    async fn an_unmapped_turns_late_denial_is_its_loss() {
        let (sink, mut received) = observation_channel();
        let run = Run::new(cap());
        assert!(run.lane.push(late_denial(5, turn(1)), 64));
        run.lane.end(LaneEnd::Closed);
        run.run(sink).await;
        assert!(received.try_recv().is_err(), "no observation");
        assert_eq!(*run.health.borrow(), DriverHealth::Open);
        assert_eq!(
            run.record(),
            Some(ObservationLoss {
                trigger: turn(1),
                generation: 3,
                first_unqueued: 5,
                omitted: 1,
            })
        );

        // Mapped, the same denial is turn 1's late observation.
        let (sink, mut received) = observation_channel();
        let run = Run::new(cap());
        mapped(&run.registration, turn(1));
        assert!(run.lane.push(late_denial(5, turn(1)), 64));
        run.lane.end(LaneEnd::Closed);
        run.run(sink).await;
        let Ok(Admitted { item, .. }) = received.try_recv() else {
            panic!("no observation");
        };
        assert!(matches!(item.observation, Observation::ActionDenied(_)));
        assert_eq!(run.record(), None);
    }

    /// x.3.2 X3 S6 (F2): ledger entries and mapped ranges are one count on
    /// the session's cap. 1,023 entries and one range fit; the next entry
    /// fails the connection `overflow` (every session on it sees the
    /// failure), and the session's health with it. Released entries return
    /// to the shared count.
    #[tokio::test]
    async fn the_cap_counts_entries_and_ranges_together() {
        let cap = cap();
        let run = Run::new(cap.clone());
        let decline = |n: usize| super::ServerRequest {
            id: via_routes::codex::RequestId::Int(1),
            method: "item/commandExecution/requestApproval".to_owned(),
            thread_id: None,
            turn_id: None,
            item_id: Some(format!("c{n}")),
        };
        for n in 0..1023 {
            run.registration
                .ledger()
                .note_decline(turn(1), &decline(n))
                .unwrap();
        }
        mapped(&run.registration, turn(1));
        assert_eq!(cap.held().0, 1024, "1,023 entries and one range fit");
        assert!(cap.slot(0).is_none(), "the next range has no slot");

        assert!(run.lane.push(late_tool(5, turn(1), "item/started"), 64));
        let (sink, _received) = observation_channel();
        run.run(sink).await;
        assert!(run.overflowed(), "the next entry fails the connection");
        assert!(matches!(*run.health.borrow(), DriverHealth::Failed { .. }));
        assert!(!run.registration.ledger().tools_open(turn(1)));
        assert_eq!(cap.held().0, 1024);

        drop(run);
        assert_eq!(cap.held(), (0, 0), "released with the registration");
        let cap = super::super::normalize::ledger_on(cap);
        assert_eq!(
            super::super::normalize::ledger(&cap).note_decline(turn(1), &decline(0)),
            Ok(()),
            "a released slot is taken again"
        );
    }

    /// x.3.2 X3 §6.2 (F2): a ledger wait ends at the existing failure
    /// signal. With the session's budget full, turn 1's late tool start
    /// waits for its entry's bytes; the driver's health fails, and the
    /// wait ends without a commit: the overflow path, the loss noted from
    /// that message, the ledger unchanged.
    #[tokio::test]
    async fn a_ledger_wait_ends_at_the_drivers_failure() {
        let budget = ObservationBudget::new();
        let (sink, _received) = observation_channel_in(&budget);
        let cap = SessionCap::new(&sink);
        let full = cap.fill_budget().unwrap();
        assert_eq!(budget.available(), 0);
        let run = Run::new(cap.clone());
        assert!(run.lane.push(late_tool(5, turn(1), "item/started"), 64));
        let health = Arc::clone(&run.health);
        let failing = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            crate::driver::latch(&health, DriverFailure::TurnAbandoned);
        });
        run.run(sink).await;
        failing.await.unwrap();
        assert!(!run.registration.ledger().tools_open(turn(1)));
        assert_eq!(run.registration.ledger().entries(), 0);
        assert_eq!(cap.held(), (0, 0), "the wait's slot is released");
        assert_eq!(
            run.record().map(|record| record.first_unqueued),
            Some(5),
            "the loss is noted from the message"
        );
        assert!(!run.overflowed());
        drop(full);
        assert_eq!(budget.available(), OBSERVATION_BYTES);
    }

    /// x.3.2 X3 r7 #3, r8 #6 (F2): a turn's credit outlives compression. A
    /// turn that extends a range keeps its credit charged while it holds
    /// its vendor ID, and releases it at its close; a non-adjacent turn's
    /// credit becomes its new range's charge.
    #[test]
    fn a_credit_outlives_compression() {
        let cap = cap();
        let registration = Registration::new(4, cap.clone());
        let credit = || {
            cap.credit_slot(1024)
                .map(|slot| cap.try_charge(slot).ok().unwrap())
        };
        mapped(&registration, turn(1));
        assert_eq!(cap.held().0, 1);

        let second = credit().unwrap();
        registration.ledger().map(turn(2));
        assert!(registration.ledger().mapped(turn(2)));
        assert_eq!(cap.held().0, 2, "charged while turn 2 holds its ID");
        registration.ledger().close(turn(2), second);
        assert_eq!(cap.held().0, 1, "released at the close: turn 2 extended");

        let fourth = credit().unwrap();
        registration.ledger().map(turn(4));
        registration.ledger().close(turn(4), fourth);
        assert_eq!(cap.held().0, 2, "turn 4's credit charges its range");
        assert!(!registration.ledger().mapped(turn(3)), "a hole stays");
        assert!(registration.ledger().mapped(turn(4)));
    }

    /// x.3.2 X3 r5 #3 (F2): ledger entries are charged inside the
    /// session's observation budget, their key bytes plus 64 B each, and
    /// share its cap: 600 open tools of turn 1 and 424 of turn 2 fill it,
    /// the next one overflows, and completions return their bytes.
    #[test]
    fn ledger_entries_hold_budget_permits() {
        let budget = ObservationBudget::new();
        let (sink, _received) = observation_channel_in(&budget);
        let cap = SessionCap::new(&sink);
        let ledger = super::super::normalize::ledger_on(cap.clone());
        let tool = |method: &str, id: &str| {
            let status = if method == "item/started" {
                "inProgress"
            } else {
                "completed"
            };
            let line = serde_json::json!({"method": method, "params": {
                "threadId": "thread-1", "turnId": "vendor-1",
                "item": {"type": "commandExecution", "id": id, "command": "sleep 1",
                    "cwd": "/w", "commandActions": [], "status": status}}});
            match via_routes::codex::decode(line.to_string().as_bytes()) {
                Ok(via_routes::codex::Incoming::Notification(notification)) => notification,
                _ => panic!("not a notification"),
            }
        };
        let id = |turn: u32, n: usize| format!("t{turn}-{n:04}");
        let mut charged = 0;
        for (number, count) in [(1, 600), (2, 424)] {
            for n in 0..count {
                super::super::normalize::ledger(&ledger)
                    .track(turn(number), &tool("item/started", &id(number, n)))
                    .unwrap();
                charged += id(number, n).len() + 64;
            }
        }
        assert_eq!(budget.available(), OBSERVATION_BYTES - charged);
        assert_eq!(cap.held().0, 1024);
        assert!(
            super::super::normalize::ledger(&ledger)
                .track(turn(2), &tool("item/started", &id(2, 424)))
                .is_err(),
            "the 1,025th entry overflows"
        );
        for n in 0..600 {
            super::super::normalize::ledger(&ledger)
                .track(turn(1), &tool("item/completed", &id(1, n)))
                .unwrap();
        }
        assert_eq!(
            budget.available(),
            OBSERVATION_BYTES - 424 * (id(2, 0).len() + 64)
        );
        drop(ledger);
        assert_eq!(budget.available(), OBSERVATION_BYTES);
        assert_eq!(cap.held(), (0, 0));
    }

    /// Message `seq`: a thread message that does not decode.
    fn malformed(seq: u64) -> LaneItem {
        LaneItem::Message(Routed {
            staged: VendorMessage::new(
                BoundedBytes::try_from_message(b"{not json}\n".to_vec()).unwrap(),
            ),
            seq,
            turn: None,
            owner: None,
            at: tokio::time::Instant::now(),
            mark: None,
        })
    }

    /// The failure while no turn runs: a malformed thread message.
    fn protocol() -> DriverFailure {
        DriverFailure::Route(RouteError::Protocol {
            turn: turn(2),
            detail: "a message of the session's thread did not decode",
        })
    }

    /// Keeps nothing, never finishing: a unit test's server folder whose
    /// write is held.
    struct Held;

    impl ServerEvidence for Held {
        fn keep<'a>(
            &'a self,
            _bytes: &'a [u8],
            _what: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(std::future::pending())
        }

        fn overflow(&self) {}
    }

    /// x.3.2 X3 r6 #1, r5 #5 (F3): the generation's failure stops every
    /// admitted turn before any await. While no turn runs, a malformed
    /// thread message fails the generation, its evidence kept only after:
    /// with that keep held, the driver's health is latched, the admitted
    /// turn 2's writes are cancelled, accepted turn 3's delivery is
    /// stopped, and no turn is admitted again.
    #[tokio::test]
    async fn the_failure_stops_admitted_turns_before_any_await() {
        let run = Run::new(cap());
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let second = run
            .registration
            .admit(
                turn(2),
                Arc::new(move || flag.store(true, Ordering::Release)),
            )
            .unwrap();
        let third = run.registration.admit(turn(3), Arc::new(|| {})).unwrap();
        let delivery = Delivery::new(4);
        third.deliver(&delivery);
        assert!(run.lane.push(malformed(5), 64));
        let (sink, _received) = observation_channel();
        let normalizing = Normalizing::new(
            (Arc::clone(&run.registration), Arc::clone(&run.lane)),
            (
                sink,
                Evidence {
                    server: Arc::new(Held),
                    earlier: Folders::default(),
                },
            ),
            (run.cancel.clone(), Arc::clone(&run.health)),
            LossRecord {
                losses: Arc::clone(&run.losses),
                generation: 3,
            },
        );
        let consumer = tokio::spawn(normalizing.run());
        let mut health = run.health.subscribe();
        tokio::time::timeout(
            Duration::from_secs(5),
            health.wait_for(|health| matches!(health, DriverHealth::Failed { .. })),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!consumer.is_finished(), "the evidence keep is held");
        assert_eq!(
            *run.health.borrow(),
            DriverHealth::Failed {
                first_cause: protocol()
            }
        );
        assert!(
            cancelled.load(Ordering::Acquire),
            "turn 2's start is cancelled"
        );
        assert!(matches!(delivery.seal().stop, Some(Stop::Generation)));
        assert_eq!(run.registration.failure(), Some(protocol()));
        assert!(run.registration.incomplete());
        assert_eq!(run.lane.ended(), Some(LaneEnd::Quarantined));
        assert!(
            run.registration.admit(turn(4), Arc::new(|| {})).is_none(),
            "nothing is admitted on a failed generation"
        );
        drop((second, third));
        consumer.abort();
    }

    /// x.3.2 X3 S5 (F3): a driver dropped without close retires its
    /// registration synchronously, evidence before the fold. The consumer
    /// waits for the bytes of turn 1's late tool start; as the driver's
    /// guard drops, the loss (from the registration's seal, count unknown)
    /// and `incomplete` are noted, the fold is `Uncertain` and the
    /// registration is retired, all at once. Capacity returns after: no
    /// commit lands.
    #[tokio::test]
    async fn a_dropped_driver_retires_before_a_late_commit() {
        let cap = cap();
        let full = cap.fill_budget().unwrap();
        let run = Run::new(cap.clone());
        assert!(run.lane.push(late_tool(5, turn(1), "item/started"), 64));
        let (sink, _received) = observation_channel();
        let dropped = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(run.registration.ledger().entries(), 0, "the wait holds");
            drop(run.guard());
            assert_eq!(
                run.record(),
                Some(ObservationLoss {
                    trigger: turn(2),
                    generation: 3,
                    first_unqueued: 5,
                    omitted: UNKNOWN,
                })
            );
            assert!(run.registration.incomplete());
            assert!(run.registration.retired());
            assert_eq!(run.folded(), Some(WireCleanup::Uncertain));
            drop(full);
        };
        tokio::join!(run.run(sink), dropped);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(run.registration.ledger().entries(), 0, "no commit lands");
        assert_eq!(cap.held(), (0, 0));
    }

    /// x.3.2 X3 r7 #6 (F3): a close that expires while the consumer waits
    /// for a late tool start's bytes leaves continuity unproven. The
    /// barrier returns false, so retirement notes the loss and `incomplete`
    /// and folds `Uncertain`, although the surviving ledger has no open
    /// tool.
    #[tokio::test]
    async fn an_expired_close_folds_uncertain() {
        let cap = cap();
        let full = cap.fill_budget().unwrap();
        let run = Run::new(cap);
        assert!(run.lane.push(late_tool(5, turn(1), "item/started"), 64));
        let (sink, _received) = observation_channel();
        let closed = async {
            run.lane.end(LaneEnd::Closed);
            let by = tokio::time::Instant::now() + Duration::from_millis(100);
            let drained = run.registration.drain(by).await;
            assert!(!drained);
            run.registration.seal();
            run.registration.drained_prefix(drained);
            assert!(!run.registration.ledger().has_open());
            drop(run.guard());
        };
        tokio::join!(run.run(sink), closed);
        assert!(run.registration.incomplete());
        assert_eq!(run.folded(), Some(WireCleanup::Uncertain));
        assert!(run.record().is_some());
        drop(full);
    }

    /// x.3.2 X3 r7 #5 (F3): a consumer cancelled before it took a queued
    /// decline gives no evidence that the prefix was disposed of: the
    /// close's barrier returns false, and retirement notes the loss and
    /// folds `Uncertain`.
    #[tokio::test]
    async fn a_cancelled_consumer_proves_no_prefix() {
        let run = Run::new(cap());
        mapped(&run.registration, turn(1));
        assert!(run.lane.push(late_denial(5, turn(1)), 64));
        run.lane.end(LaneEnd::Closed);
        run.cancel.cancel();
        let (sink, _received) = observation_channel();
        run.run(sink).await;
        assert_eq!(run.lane.front_seq(), Some(5), "the decline was not taken");
        let drained = run
            .registration
            .drain(tokio::time::Instant::now() + Duration::from_millis(100))
            .await;
        assert!(!drained);
        run.registration.drained_prefix(drained);
        drop(run.guard());
        assert_eq!(
            run.record()
                .map(|record| (record.first_unqueued, record.omitted)),
            Some((5, UNKNOWN))
        );
        assert_eq!(run.folded(), Some(WireCleanup::Uncertain));
    }

    /// x.3.2 X3 S10, r6 #6, r5 #2 (release by retirement) (F3): the ledger
    /// is registration storage until retirement, not the consumer's. Turn
    /// 1's late tool start opens its entry; the consumer takes the lane's
    /// end and returns, and the entry stays charged. The close's barrier
    /// proves the prefix disposed of, so retirement notes no loss, but it
    /// reads the open tool and folds `Uncertain`; then the entry is
    /// released, while the registration still lives.
    #[tokio::test]
    async fn retirement_folds_the_open_tool_and_releases_the_ledger() {
        let cap = cap();
        let run = Run::new(cap.clone());
        assert!(run.lane.push(late_tool(5, turn(1), "item/started"), 64));
        run.lane.end(LaneEnd::Closed);
        let (sink, _received) = observation_channel();
        run.run(sink).await;
        assert!(run.registration.ledger().tools_open(turn(1)));
        assert_eq!(cap.held().0, 1, "kept after the consumer returned");
        let drained = run
            .registration
            .drain(tokio::time::Instant::now() + Duration::from_millis(100))
            .await;
        assert!(drained);
        run.registration.seal();
        run.registration.drained_prefix(drained);
        drop(run.guard());
        assert_eq!(run.record(), None, "no loss is noted");
        assert!(!run.registration.incomplete());
        assert_eq!(run.folded(), Some(WireCleanup::Uncertain));
        assert_eq!(cap.held(), (0, 0), "released at retirement");
        assert!(!run.registration.ledger().tools_open(turn(1)));
    }

    /// x.3.2 X3 r7 #4 (F3): disposal after the generation's failure never
    /// waits and never commits after retirement. With the budget full, a
    /// malformed thread message fails the generation, and the tool start
    /// behind it cannot be retained: the consumer returns at once,
    /// `incomplete` set and the loss noted. Retired, then given capacity,
    /// the registration takes nothing. With room, disposal does retain
    /// the tool start.
    #[tokio::test]
    async fn failed_disposal_takes_only_room_at_hand() {
        let shared = cap();
        let full = shared.fill_budget().unwrap();
        let run = Run::new(shared.clone());
        assert!(run.lane.push(malformed(5), 64));
        assert!(run.lane.push(late_tool(6, turn(1), "item/started"), 64));
        let (sink, _received) = observation_channel();
        run.run(sink).await;
        assert_eq!(run.registration.failure(), Some(protocol()));
        assert_eq!(run.lane.front_seq(), None, "disposed of to the lane's end");
        assert!(run.registration.incomplete());
        assert_eq!(
            run.record()
                .map(|record| (record.first_unqueued, record.omitted)),
            Some((5, UNKNOWN))
        );
        drop(run.guard());
        drop(full);
        assert_eq!(run.registration.ledger().entries(), 0);
        assert_eq!(shared.held(), (0, 0));
        assert_eq!(run.folded(), Some(WireCleanup::Uncertain));

        let run = Run::new(cap());
        assert!(run.lane.push(malformed(5), 64));
        assert!(run.lane.push(late_tool(6, turn(1), "item/started"), 64));
        let (sink, _received) = observation_channel();
        run.run(sink).await;
        assert!(run.registration.ledger().tools_open(turn(1)));
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
