//! One Codex registration's delivery (x.3.2 X0 items 5, 8.2, 10, 11,
//! 12.5, 13.2; X3 §1–§6): the single consumer that takes the
//! registration's ingress lane in decode order across its turns, each
//! turn's `DeliverySeal`, and the registration's own seal for what goes
//! out under no turn.
//!
//! The consumer runs on the session's tracker under `crash_on_panic`: a
//! panic in it is a VIA bug and aborts the daemon. It lives from the
//! thread's registration until the lane's end, the generation's failure,
//! the registration's seal or the session's cancellation. It is the only
//! reader of the lane and the registration's only sink producer, so its
//! outputs follow decode order and `at` never decreases (C2 §4).
//!
//! Every fact that decides whose lane traffic is travels in the lane: a
//! turn's `Start` marker, pushed as its `turn/start` is handed to Wire,
//! holds the turn (`Pending`), and the turn's `Reply` marker, at the
//! reply's decode position, accepts it (`Running`) or refuses it. A
//! message naming an unmapped turn read while a turn is pending may be
//! that turn's early traffic: it is retained, under its lane charge, and
//! released after the acceptance, restamped; it is loss if the turn can
//! never be mapped. A turn's terminal is retained in its seal's slot,
//! never sent, and freezes the lane until the turn seals. An earlier
//! turn's message takes the late path under the registration's seal,
//! judged by the session's ledger; the terminal of a turn that sealed
//! with none, or one its seal judged past the turn's cut, is the turn's
//! late terminal (C2 §4 `turn.late_terminal`). A message that does not decode, a
//! protocol error, a stall or an overflow fails the generation (§4).
//!
//! The running turn never waits on the consumer. It waits for its seal's
//! decision beside its own orders, and seals at whatever cutoff comes
//! first: every later output finds the seal and is refused. The seal
//! reports the first message it may have left undelivered, a conservative
//! lower bound, floored by the consumer's outstanding retained position.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use via_routes::codex::{
    Connection, ConnectionFailure, DECLINE_DEADLINE, ENTRY_BYTES, Incoming, Lane, LaneCharge,
    LaneEnd, LaneEvent, LaneItem, Mark, Notification, Reply, Routed, ServerRequest, Start,
    TurnFolder, decode,
};

use super::normalize::{
    self, Ledger, Metadata, NormalizeError, Step, StructuredOutput, TurnNormalizer, ledger,
    ledger_on,
};
use crate::driver::latch;
use crate::observation::{
    Acceptance, Charge, InstanceReport, Observation, ObservationItem, ObservationSink, Reserved,
    SessionCap, Undelivered, VendorTerminal, admitted,
};
use crate::runtime::event_stall;
use crate::{
    AcceptanceToken, DriverFailure, DriverHealth, RouteError, StopAck, TurnActivity, TurnNumber,
    VendorTerminalStatus, VendorTurnId,
};

/// `omitted` when the count of lost messages is unknown or saturated
/// (X0 item 10).
pub(crate) const UNKNOWN: u64 = u64::MAX;

/// The detail of a refusal the lane contradicts (x.3.2 X3 §3.2, packet
/// lines 144–145).
pub(crate) const CONTRADICTED: &str = "refusal contradicted by started-turn evidence";

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

    /// What a close of generation `generation` leaves lost by its outcome
    /// `drained` (x.3.2 X3 §5.4; `None`: the barrier timed out), from
    /// `position`, its seal's position floored by the outstanding one: a
    /// cut prefix only a message its seal found delivered in part; a
    /// cancelled or timed-out consumer everything from there, count
    /// unknown. A lane end, a failure or an unproven end noted its own.
    /// Whether continuity is unproven.
    pub(crate) fn note_close(
        &mut self,
        generation: u64,
        drained: Option<Drained>,
        (sealed, position): (&Sealed, u64),
    ) -> bool {
        match drained {
            Some(Drained::Cut) => {
                if sealed.partial {
                    self.note(generation, position, UNKNOWN);
                }
                false
            }
            Some(Drained::LaneEnded(_) | Drained::Unproven(_) | Drained::ConsumerFailed) => false,
            Some(Drained::ConsumerCancelled) | None => {
                self.note(generation, position, UNKNOWN);
                true
            }
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
    /// The retained terminal's original decode instant (x.3.2 X4 D4.1),
    /// not its observation time.
    pub(crate) decoded_at: Option<Instant>,
    /// The decode instant of the message that ended a draining
    /// terminal's tools (X4 code review r1 #1): settlement judges it
    /// against the P7 window, whenever the driver ran.
    pub(crate) drained_at: Option<Instant>,
    /// X4 code review r2 #2: the retained terminal was decoded past the
    /// turn's cut: it is not the turn's, and goes out as its late
    /// terminal once the consumer closes the turn.
    pub(crate) late: bool,
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
    /// The terminal's original decode instant.
    decoded_at: Option<Instant>,
    /// x.3.2 X4 D4.1: the terminal is an interrupted one retained with a
    /// tool open; the turn's P7 window is open until the tools end.
    draining: bool,
    /// The decode instant of the message that ended the draining
    /// terminal's tools.
    drained_at: Option<Instant>,
    /// A terminal the seal judged late: the turn's late terminal.
    late: Option<Retained>,
    tools_open: bool,
    stop: Option<Stop>,
}

impl Seal {
    /// The first message a seal now would leave undelivered.
    fn next(&self) -> u64 {
        if self.complete {
            self.current.saturating_add(1)
        } else {
            self.current
        }
    }
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
                decoded_at: None,
                draining: false,
                drained_at: None,
                late: None,
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
    /// sealed, with its original decode instant; it completes its message.
    /// A `draining` one opens the turn's P7 window (x.3.2 X4 D4.1).
    fn retain(
        &self,
        retained: Retained,
        (tools_open, draining): (bool, bool),
        decoded_at: Instant,
    ) -> bool {
        let mut seal = self.lock();
        if seal.sealed.is_some() {
            return false;
        }
        seal.terminal = Some(retained);
        seal.decoded_at = Some(decoded_at);
        seal.draining = draining;
        seal.complete = true;
        seal.tools_open = tools_open;
        drop(seal);
        self.changed.notify_one();
        true
    }

    /// x.3.2 X4 D4.1: the draining turn's tools all ended, by the message
    /// decoded at `decoded_at`; its P7 window closes and the retained
    /// terminal decides.
    fn drained(&self, decoded_at: Instant) {
        let mut seal = self.lock();
        if !seal.draining {
            return;
        }
        seal.draining = false;
        seal.drained_at = Some(decoded_at);
        drop(seal);
        self.changed.notify_one();
    }

    /// The draining terminal's original decode instant, while its P7
    /// window is open (x.3.2 X4 D4.3).
    pub(crate) fn draining(&self) -> Option<Instant> {
        let seal = self.lock();
        seal.decoded_at
            .filter(|_| seal.draining && seal.sealed.is_none())
    }

    /// The terminal the seal judged late, once.
    fn take_late(&self) -> Option<Retained> {
        self.lock().late.take()
    }

    /// Records why delivery stopped, unless sealed.
    pub(crate) fn stop(&self, stop: Stop) {
        let mut seal = self.lock();
        if seal.sealed.is_some() || seal.stop.is_some() {
            return;
        }
        seal.stop = Some(stop);
        drop(seal);
        self.changed.notify_one();
    }

    /// x.3.2 X3 §3.5: publishes `through` as the position through which
    /// every message of the turn's fence went out whole, in the seal's
    /// critical section, so a seal either precedes the check (nothing is
    /// published) or follows the publication. Whether it was published.
    fn report(&self, activity: &TurnActivity, through: u64) -> bool {
        let seal = self.lock();
        if seal.sealed.is_some() {
            return false;
        }
        report_seam();
        activity.delivered_through(through);
        true
    }

    /// The first message a seal now would leave undelivered, or the
    /// sealed position.
    fn position(&self) -> u64 {
        let seal = self.lock();
        seal.sealed.unwrap_or_else(|| seal.next())
    }

    /// Whether the turn's delivery reached a decision: why it stopped, or
    /// its terminal once no P7 window is open (x.3.2 X4 I2).
    pub(crate) fn decided(&self) -> bool {
        let seal = self.lock();
        seal.stop.is_some() || (seal.terminal.is_some() && !seal.draining)
    }

    /// Resolves at the next decision change (a change since the last wait
    /// is kept).
    pub(crate) async fn changed(&self) {
        self.changed.notified().await;
    }

    /// Seals delivery: nothing more goes out. The position is fixed by
    /// the first call; the slots are taken once.
    pub(crate) fn seal(&self) -> Sealed {
        self.seal_cut(|_| false)
    }

    /// [`Self::seal`], judging a retained terminal by its decode instant
    /// (X4 code review r2 #2): one `late` decides is kept for the
    /// consumer, which sends it as the turn's late terminal once it closes
    /// the turn, in its own order ([`Self::take_late`]).
    pub(crate) fn seal_cut(&self, late: impl FnOnce(Instant) -> bool) -> Sealed {
        seal_seam();
        let mut seal = self.lock();
        let next = seal.next();
        let position = *seal.sealed.get_or_insert(next);
        let mut terminal = seal.terminal.take();
        if let (Some(_), Some(decoded_at)) = (&terminal, seal.decoded_at)
            && late(decoded_at)
        {
            seal.late = terminal.take();
        }
        let sealed = Sealed {
            position,
            partial: !seal.complete,
            late: seal.late.is_some(),
            terminal,
            decoded_at: seal.decoded_at,
            drained_at: seal.drained_at,
            tools_open: seal.tools_open,
            stop: seal.stop.take(),
        };
        drop(seal);
        self.sealed.cancel();
        sealed
    }
}

/// Whose a lane message is, in the consumer's state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Owner {
    /// The held turn, running.
    This,
    /// An earlier VIA turn of the session, or the held turn once closed.
    Earlier(TurnNumber),
    /// A vendor turn the connection never mapped here.
    Unknown,
    /// No turn: thread-level traffic.
    Thread,
}

/// The protocol failure a malformed message of the thread latches.
pub(crate) const UNDECODED: &str = "a message of the session's thread did not decode";

/// How a message's handling ended.
enum Handled {
    /// It was handled; whether it went out whole.
    Done(bool),
    /// The held turn's seal refused it at its start: nothing of it went
    /// out, and it is handled again with the turn closed (x.3.2 X3 §3.2).
    Refused(LaneItem),
    /// The registration's seal refused it at its start: nothing of it
    /// went out, ever.
    Sealed,
}

/// The evidence folder of each turn a generation ran, by turn: the
/// driver files a turn's before its start.
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

/// What a turn's `Start` marker carries to its registration's consumer
/// (x.3.2 X3 §2.1): the turn's delivery, what its normalizer and its
/// acceptance need, and its credit (§3.4). Its evidence folder is in the
/// generation's folders, by turn.
pub(crate) struct StartCx {
    pub(crate) delivery: Arc<Delivery>,
    pub(crate) activity: TurnActivity,
    pub(crate) schema: bool,
    pub(crate) instance: Option<InstanceReport>,
    /// The start request's correlation, set as the request is encoded,
    /// before its `Start` can be pushed.
    pub(crate) correlation: Arc<OnceLock<AcceptanceToken>>,
    pub(crate) credit: Charge,
    /// The turn's stop report (x.3.2 X4 D7): written once its interrupted
    /// terminal is retained.
    pub(crate) stop_ack: StopAck,
}

/// How the consumer ended (x.3.2 X3 §5.4); the first outcome stays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Drained {
    /// The close's cut was taken, with nothing retained or releasing.
    Cut,
    /// The lane ended otherwise, with nothing retained or releasing.
    LaneEnded(LaneEnd),
    /// The lane ended with a retained or releasing item: its loss noted.
    Unproven(LaneEnd),
    /// The generation failed; its loss noted (§4).
    ConsumerFailed,
    /// The consumer returned without its lane's end.
    ConsumerCancelled,
}

impl Drained {
    /// Whether the outcome proves the prefix disposed of.
    fn positive(self) -> bool {
        matches!(self, Self::Cut | Self::LaneEnded(_))
    }
}

#[derive(Default)]
struct Slot {
    /// The generation's failure, first wins (x.3.2 X3 §4.2).
    failed: Option<DriverFailure>,
    /// Retired (§6.5): no admission, wait or ledger commit follows.
    retired: bool,
    /// Continuity is unproven: retirement folds `Uncertain` (§6.6).
    incomplete: bool,
    /// The consumer's published outstanding position (§3.2; its rule is
    /// at `Normalizing::outstanding`).
    outstanding: Option<u64>,
    /// The admitted turns, which the generation's failure stops (§4.2).
    admitted: BTreeMap<TurnNumber, AdmittedTurn>,
}

/// An admitted turn: what cancels its writes, and its delivery.
#[derive(Clone)]
struct AdmittedTurn {
    cancel: Arc<dyn Fn() + Send + Sync>,
    delivery: Arc<Delivery>,
}

/// A turn admitted on its registration (x.3.2 X3 §4.2 step 2): it leaves
/// the admitted set as this drops.
pub(crate) struct Admission {
    registration: Arc<Registration>,
    turn: TurnNumber,
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.registration.slot().admitted.remove(&self.turn);
    }
}

/// One registration's state shared by its driver and its consumer (X0
/// items 8.2, 13.2; x.3.2 X3 §3.1): the failure latch, the admitted turns,
/// the session's metadata (§6.1), the consumer's outcome and outstanding
/// position, and the seal of what goes out under no turn.
pub(crate) struct Registration {
    slot: Mutex<Slot>,
    /// The session's metadata: open tools, the suppression table and the
    /// mapped ranges.
    ledger: Ledger,
    /// The seal of what goes out under no turn: sealed at a close, at the
    /// generation's failure, or when the registration is released.
    idle: Arc<Delivery>,
    /// How the consumer ended, once it did.
    drained: watch::Sender<Option<Drained>>,
    /// Cancelled at the generation's failure (x.3.2 X3 §4.2).
    failing: CancellationToken,
    /// Cancelled at retirement (§6.5).
    retiring: CancellationToken,
    /// Where the malformed message that failed the generation was kept
    /// (X0 item 5), noted after the failure so its I/O never delays it.
    undecoded: watch::Sender<Note>,
}

/// A malformed message's evidence note (x.3.2 X3 fix r1 #4).
enum Note {
    Absent,
    Keeping,
    Kept(Option<String>),
}

impl Registration {
    /// A registration whose last message before it is `before`, its
    /// metadata charged to the session's `cap`.
    pub(crate) fn new(before: u64, cap: SessionCap) -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(Slot::default()),
            ledger: ledger_on(cap),
            idle: Delivery::new(before),
            drained: watch::channel(None).0,
            failing: CancellationToken::new(),
            retiring: CancellationToken::new(),
            undecoded: watch::channel(Note::Absent).0,
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

    /// The close's delivery barrier (X0 item 8.2; x.3.2 X3 §5.4), once the
    /// close was posted: how the consumer ended, by `by`; `None` when it
    /// had not. The caller seals next.
    pub(crate) async fn drain(&self, by: Instant) -> Option<Drained> {
        let mut drained = self.drained.subscribe();
        let ended = tokio::time::timeout_at(by, drained.wait_for(Option::is_some)).await;
        ended
            .ok()
            .and_then(|ended| ended.ok().and_then(|drained| *drained))
    }

    /// Records the consumer's outcome; the first stays.
    fn drained(&self, outcome: Drained) {
        self.drained.send_if_modified(|drained| {
            let unset = drained.is_none();
            drained.get_or_insert(outcome);
            unset
        });
    }

    /// x.3.2 X3 §4.2 step 2: admits turn `turn`, whose writes `cancel`
    /// cancels and whose `delivery` the generation's failure stops;
    /// refused once the generation failed or the registration retired.
    pub(crate) fn admit(
        self: &Arc<Self>,
        turn: TurnNumber,
        cancel: Arc<dyn Fn() + Send + Sync>,
        delivery: &Arc<Delivery>,
    ) -> Option<Admission> {
        let mut slot = self.slot();
        if slot.failed.is_some() || slot.retired {
            return None;
        }
        slot.admitted.insert(
            turn,
            AdmittedTurn {
                cancel,
                delivery: Arc::clone(delivery),
            },
        );
        Some(Admission {
            registration: Arc::clone(self),
            turn,
        })
    }

    /// Notes the malformed message's evidence: `Keeping` from `Absent`,
    /// then `Kept` from `Keeping`; the first message's stays.
    fn note(&self, note: Note) {
        self.undecoded.send_if_modified(|current| {
            let next = matches!(
                (&*current, &note),
                (Note::Absent, Note::Keeping) | (Note::Keeping, Note::Kept(_))
            );
            if next {
                *current = note;
            }
            next
        });
    }

    /// The evidence note of the malformed message that failed the
    /// generation, once kept: `None` when none was or it was not kept.
    pub(crate) async fn undecoded(&self) -> Option<String> {
        let mut note = self.undecoded.subscribe();
        match note
            .wait_for(|note| !matches!(note, Note::Keeping))
            .await
            .as_deref()
        {
            Ok(Note::Kept(note)) => note.clone(),
            _ => None,
        }
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
    pub(crate) fn mark_incomplete(&self) {
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

    pub(crate) fn outstanding(&self) -> Option<u64> {
        self.slot().outstanding
    }

    fn publish(&self, outstanding: Option<u64>) {
        self.slot().outstanding = outstanding;
    }

    /// x.3.2 X3 §4.2: a loss snapshot from a seal's `position`, floored
    /// by the outstanding position (r11 #1).
    pub(crate) fn floor(&self, position: u64) -> u64 {
        self.outstanding()
            .map_or(position, |outstanding| outstanding.min(position))
    }

    /// x.3.2 X3 §4.1, §4.2: the generation fails with `cause`, first wins
    /// and never after retirement, before any await: `failed` and
    /// `incomplete` are set and the admitted turns taken in one section;
    /// the driver's health latches; the lane ends; each admitted turn's
    /// delivery stops (a retained terminal stays) and its writes are
    /// cancelled; then the registration is sealed and the loss noted from
    /// its seal, floored by the outstanding position, count unknown.
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
            turn.delivery.stop(Stop::Generation);
            (turn.cancel)();
        }
        self.failing.cancel();
        let sealed = self.seal();
        losses(&loss.losses).note(loss.generation, self.floor(sealed.position), UNKNOWN);
    }

    /// x.3.2 X3 §6.5: retires the registration once, in one section (a
    /// later call does nothing). It is sealed; unless the consumer proved
    /// the prefix disposed of (`Cut` or `LaneEnded`), the loss is noted
    /// from its seal, floored by the outstanding position, count unknown,
    /// and continuity is unproven. Then `retired` refuses every later
    /// admission, wait and commit, and ends the consumer; `fold` runs when
    /// a tool is open or continuity is unproven, never inferring
    /// quiescence from what survived; then the ledger and its ranges are
    /// released.
    pub(crate) fn retire(&self, loss: &LossRecord, fold: impl FnOnce()) {
        let mut slot = self.slot();
        if slot.retired {
            return;
        }
        let sealed = self.seal();
        let positive = self.drained.borrow().is_some_and(Drained::positive);
        if !positive {
            let position = slot.outstanding.map_or(sealed.position, |outstanding| {
                outstanding.min(sealed.position)
            });
            losses(&loss.losses).note(loss.generation, position, UNKNOWN);
            slot.incomplete = true;
        }
        slot.retired = true;
        self.retiring.cancel();
        if slot.incomplete || self.ledger().has_open() {
            fold();
        }
        self.ledger().retire();
    }

    /// Seals what goes out under no turn: the consumer then returns. The
    /// first call fixes the position.
    pub(crate) fn seal(&self) -> Sealed {
        self.idle.seal()
    }
}

/// Marks a registration's consumer returned, however: cancelled unless it
/// recorded how its lane ended first.
struct Returned(Arc<Registration>);

impl Drop for Returned {
    fn drop(&mut self) {
        // Returning is no evidence the prefix was taken (x.3.2 X3 r7 #5).
        self.0.drained(Drained::ConsumerCancelled);
        self.0.note(Note::Kept(None));
    }
}

/// The driver's loss record and the generation it names (x.3.2 X3 fix r4
/// #6).
pub(crate) struct LossRecord {
    pub(crate) losses: Arc<Mutex<Losses>>,
    pub(crate) generation: u64,
}

/// Where the held turn is (x.3.2 X3 §3.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Its `Start` was taken, its `Reply` not yet.
    Pending,
    /// Accepted: its `Accepted` went out.
    Running,
    /// x.3.2 X4 D4.1: its interrupted terminal is retained with a tool
    /// open; its messages are still taken until the tools end (then
    /// `Retained`) or it seals.
    Draining,
    /// Its terminal is retained: nothing is taken until it seals.
    Retained,
}

impl Phase {
    /// Accepted and still taking its messages.
    fn taking(self) -> bool {
        matches!(self, Self::Running | Self::Draining)
    }
}

/// The turn the consumer holds, from its `Start` until it seals.
struct Held {
    turn: TurnNumber,
    /// Its decode fence.
    fence: u64,
    cx: StartCx,
    phase: Phase,
    /// Its vendor turn ID, once accepted.
    accepted: Option<String>,
    normalizer: Option<TurnNormalizer>,
    /// The position last published under its fence.
    reported: u64,
}

/// A retained item's binding at its turn's acceptance (x.3.2 X3 §3.2, r11
/// #2): whose it is (`None`: no turn's) and its stamped instant.
#[derive(Clone, Copy, Debug)]
struct Bound {
    owner: Option<TurnNumber>,
    at: Instant,
}

/// An item retained while its turn was pending, under its own lane charge
/// (x.3.2 X3 §2.3, §3.2).
struct Early {
    item: LaneItem,
    /// Its decode sequence.
    seq: u64,
    /// Its position under the held turn's fence.
    mark: Option<Mark>,
    /// Its decode instant.
    at: Instant,
    charge: LaneCharge,
    bound: Option<Bound>,
}

/// What woke the consumer.
enum Wake {
    /// The session's cancellation, or the registration's seal.
    Ended,
    /// The generation failed.
    Failed,
    /// The lane overflowed.
    Overflow,
    /// The held turn's seal.
    Sealed,
    /// The lane's next item, or its end.
    Lane(LaneEvent),
}

/// One registration's consumer (X0 item 13.2; x.3.2 X3 §3): the lane's
/// only reader and the registration's only sink producer, across the
/// registration's turns.
pub(crate) struct Normalizing {
    registration: Arc<Registration>,
    lane: Arc<Lane>,
    sink: ObservationSink,
    evidence: Evidence,
    cancel: CancellationToken,
    health: Arc<watch::Sender<DriverHealth>>,
    loss: LossRecord,
    held: Option<Held>,
    /// The turn whose `Start` was taken and whose `Reply` was not.
    unanswered: Option<TurnNumber>,
    /// The held turn's retained early items, in decode order.
    early: VecDeque<Early>,
    /// The retained item being released: its decode sequence and mark.
    releasing: Option<(u64, Option<Mark>)>,
    /// The first position of the live fence whose observations were lost.
    gap: Option<u64>,
    /// The highest position of the live fence taken and handled whole.
    disposed: u64,
    /// x.3.2 X3 §6.2: the stall deadline of the message being handled,
    /// one for its ledger wait and all of its sink waits.
    stall_by: Instant,
    /// X4 code review r2 #2 (C2 §4 `turn.late_terminal`): the accepted
    /// turns that sealed with no terminal and have sent no late one: only
    /// these send their terminal, once. One entry per such turn of the
    /// registration.
    bare: Vec<TurnNumber>,
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
            held: None,
            unanswered: None,
            early: VecDeque::new(),
            releasing: None,
            gap: None,
            disposed: 0,
            stall_by: Instant::now(),
            bare: Vec::new(),
        }
    }

    /// Takes the registration's lane, turn after turn, until it ends.
    pub(crate) async fn run(mut self) {
        let _returned = Returned(Arc::clone(&self.registration));
        loop {
            idle_check_seam().await;
            // A bound tail is handled before the lane is taken again.
            if self.held.is_none() && !self.early.is_empty() {
                self.release().await;
                if !self.early.is_empty() {
                    // Released only in part: the consumer ends.
                    return self.ended();
                }
            }
            match self.wake().await {
                Wake::Ended => return,
                Wake::Failed => return self.dispose_failed(),
                Wake::Overflow => {
                    self.overflow();
                    return self.dispose_failed();
                }
                Wake::Sealed => self.close_sealed().await,
                Wake::Lane(LaneEvent::Item(item, charge)) => self.item(*item, charge).await,
                Wake::Lane(LaneEvent::End(end)) => return self.end(end),
            }
        }
    }

    /// Waits on the consumer's cutoffs and the lane: a retained terminal
    /// freezes the lane. A pending turn's lane fails at its overflow before
    /// anything more is taken (§3.2: no `Reply` past it); otherwise what
    /// the lane took before an overflow is taken in order up to its end (a
    /// running turn's terminal among it is retained, x.3.2 X3 fix r3 #2),
    /// and a frozen lane's overflow fails the generation at once.
    async fn wake(&self) -> Wake {
        let held = self
            .held
            .as_ref()
            .map(|held| (Arc::clone(&held.cx.delivery), held.phase));
        let frozen = held
            .as_ref()
            .is_some_and(|(_, phase)| *phase == Phase::Retained);
        let pending = held
            .as_ref()
            .is_some_and(|(_, phase)| *phase == Phase::Pending);
        let sealed = async {
            match &held {
                Some((delivery, _)) => delivery.sealed.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Wake::Ended,
            () = self.registration.failing.cancelled() => Wake::Failed,
            () = self.registration.idle.sealed.cancelled() => Wake::Ended,
            () = sealed => Wake::Sealed,
            () = self.lane.overflowed(), if pending => Wake::Overflow,
            event = self.lane.next(), if !frozen => Wake::Lane(event),
            () = self.lane.overflowed() => Wake::Overflow,
        }
    }

    /// The turn a failure names: the held one, else the session's latest.
    fn latest(&self) -> Option<TurnNumber> {
        self.held
            .as_ref()
            .map(|held| held.turn)
            .or_else(|| losses(&self.loss.losses).latest)
    }

    /// x.3.2 X3 §4.1: the generation fails with `cause` (first wins).
    fn fail(&self, cause: &DriverFailure) {
        self.registration
            .fail(cause, (&self.health, &self.lane, &self.loss));
    }

    fn overflow(&self) {
        let cause = self
            .latest()
            .map_or(DriverFailure::ObservationOverflow, |turn| {
                DriverFailure::Route(RouteError::Overflow { turn })
            });
        self.fail(&cause);
    }

    fn protocol(&self, detail: &'static str) {
        let cause = self
            .latest()
            .map_or(DriverFailure::ObservationOverflow, |turn| {
                DriverFailure::Route(RouteError::Protocol { turn, detail })
            });
        self.fail(&cause);
    }

    /// The consumer returns with a retained item undelivered: its loss is
    /// noted from the outstanding position, and continuity is unproven.
    fn ended(&mut self) {
        if let Some(first) = self.outstanding() {
            losses(&self.loss.losses).note(self.loss.generation, first, UNKNOWN);
            self.registration.mark_incomplete();
        }
        self.drop_early();
    }

    async fn item(&mut self, item: LaneItem, charge: LaneCharge) {
        self.stall_by = Instant::now() + event_stall();
        match item {
            LaneItem::Start(start) => {
                drop(charge);
                self.start(start);
            }
            LaneItem::Reply(reply) => {
                drop(charge);
                self.reply(reply).await;
            }
            item @ (LaneItem::Message(_) | LaneItem::Declined { .. }) => {
                self.data(item, charge).await;
            }
        }
    }

    /// `Start(B)`: B is held, pending; its fence is the live one.
    fn start(&mut self, Start { turn, fence, cx }: Start) {
        let Ok(cx) = cx.downcast::<StartCx>() else {
            // Only the driver pushes a `Start`, with its context.
            return;
        };
        if self.held.is_some() {
            self.close();
        }
        // The start gate leaves an earlier start unanswered only when it
        // never reached the vendor, or the lane ended.
        self.unanswered = Some(turn);
        self.held = Some(Held {
            turn,
            fence,
            cx: *cx,
            phase: Phase::Pending,
            accepted: None,
            normalizer: None,
            reported: 0,
        });
        self.gap = None;
        self.disposed = 0;
    }

    /// A `turn/start` reply in decode order (x.3.2 X3 §3.2).
    async fn reply(&mut self, reply: Reply) {
        let pending = self
            .held
            .as_ref()
            .is_some_and(|held| held.turn == reply.turn && held.phase == Phase::Pending);
        if self.unanswered == Some(reply.turn) {
            self.unanswered = None;
        }
        if !pending {
            // An abandoned or refused start's.
            return self.handled(reply.mark, true);
        }
        match reply.accepted {
            Some(accepted) => self.accepted(accepted, reply.at, reply.mark).await,
            // The vendor refused, uncontradicted: B is never mapped.
            None if self.early.is_empty() => {
                self.handled(reply.mark, true);
                self.close();
            }
            // Packet lines 144–145: possible started-turn evidence
            // contradicts the refusal.
            None => self.fail(&DriverFailure::Route(RouteError::Protocol {
                turn: reply.turn,
                detail: CONTRADICTED,
            })),
        }
    }

    /// `Reply(B)` accepted it: `Accepted` goes out under B's seal at the
    /// reply's read instant, which alone maps B (§3.4); its retained items
    /// are bound and released.
    async fn accepted(&mut self, accepted: String, at: Instant, mark: Option<Mark>) {
        let Some(held) = self.held.as_ref() else {
            return;
        };
        let acceptance = Acceptance {
            correlation: held
                .cx
                .correlation
                .get()
                .copied()
                .unwrap_or(AcceptanceToken::FIRST),
            vendor_turn_id: VendorTurnId::try_from(accepted.clone()).ok(),
            instance: held.cx.instance.clone(),
        };
        let delivery = Arc::clone(&held.cx.delivery);
        let sent = self
            .output(
                &delivery,
                &accepted,
                Observation::Accepted(acceptance),
                (None, at),
            )
            .await;
        if !sent {
            // B sealed (its seal disposes of its retained items), or the
            // generation failed.
            return;
        }
        let ledger = Arc::clone(&self.registration.ledger);
        let Some(held) = self.held.as_mut() else {
            return;
        };
        self.registration.ledger().map(held.turn);
        held.normalizer = Some(TurnNormalizer::on(held.cx.schema, held.turn, ledger));
        held.phase = Phase::Running;
        let turn = held.turn;
        for entry in &mut self.early {
            let named = entry
                .item
                .routed()
                .and_then(|routed| routed.turn.as_deref());
            entry.bound = Some(Bound {
                owner: (named == Some(accepted.as_str())).then_some(turn),
                at: entry.at.max(at),
            });
        }
        held.accepted = Some(accepted);
        self.handled(mark, true);
        self.release().await;
    }

    /// A message or a placeholder.
    async fn data(&mut self, item: LaneItem, charge: LaneCharge) {
        let Some(routed) = item.routed() else {
            return;
        };
        let (seq, mark) = (routed.seq, routed.mark);
        let unmapped = routed.turn.is_some() && routed.owner.is_none();
        let phase = self.held.as_ref().map(|held| held.phase);
        if unmapped && phase == Some(Phase::Pending) {
            return self.retain(item, charge);
        }
        drop(charge);
        if unmapped
            && phase.is_none()
            && let Some(turn) = self.unanswered
        {
            // Its turn sealed pending: it can never be mapped.
            losses(&self.loss.losses).note_turn(turn, self.loss.generation, seq, 1);
            self.registration.mark_incomplete();
            return;
        }
        let whole = self.deliver(item, None).await;
        self.handled(mark, whole.unwrap_or(false));
    }

    /// Handles `item` as its routing names it, or as `bound` binds it (a
    /// retained item's turn and instant): whether it went out whole,
    /// `None` when the registration's seal refused it. The held turn's
    /// seal refusing it at its start closes that turn, and it is handled
    /// again.
    async fn deliver(
        &mut self,
        mut item: LaneItem,
        bound: Option<(TurnNumber, Instant)>,
    ) -> Option<bool> {
        loop {
            let owner = match (bound, item.routed()) {
                (Some((turn, _)), _) => match &self.held {
                    Some(held) if held.turn == turn => Owner::This,
                    Some(_) | None => Owner::Earlier(turn),
                },
                (None, Some(routed)) => self.owner(routed),
                (None, None) => return Some(true),
            };
            match self.handle(item, owner, bound.map(|(_, at)| at)).await {
                Handled::Done(whole) => return Some(whole),
                Handled::Sealed => return None,
                Handled::Refused(refused) => {
                    self.close();
                    item = refused;
                }
            }
        }
    }

    /// The early rule (x.3.2 X3 §3.2): retained, under its lane charge,
    /// grown by its entry's 64 B without waiting; a growth that does not
    /// fit ends the lane `Overflow`, which fails the generation.
    fn retain(&mut self, item: LaneItem, mut charge: LaneCharge) {
        let Some(routed) = item.routed() else {
            return;
        };
        let (seq, mark, at) = (routed.seq, routed.mark, routed.at);
        self.lane.try_grow(&mut charge, ENTRY_BYTES);
        self.early.push_back(Early {
            item,
            seq,
            mark,
            at,
            charge,
            bound: None,
        });
        self.publish();
    }

    /// Releases the bound retained items in order, by their binding only
    /// (x.3.2 X3 §3.2): to the running turn, or, once it closed, as its
    /// late observations; an item of no turn is disposed of. Each handoff
    /// keeps the outstanding rule ([`Self::outstanding`]). A retained
    /// terminal freezes the rest; a sealed turn leaves it to its close.
    async fn release(&mut self) {
        loop {
            let Some(front) = self.early.front() else {
                return;
            };
            let open = match &self.held {
                Some(held) => held.phase.taking() && !held.cx.delivery.sealed.is_cancelled(),
                None => true,
            };
            if !open || self.registration.idle.sealed.is_cancelled() {
                return;
            }
            self.releasing = Some((front.seq, front.mark));
            let Some(entry) = self.early.pop_front() else {
                return;
            };
            handoff_seam().await;
            self.publish();
            self.stall_by = Instant::now() + event_stall();
            // Its lane charge is held until it went out; one of no turn
            // is disposed of, whole.
            let Early {
                item,
                seq,
                charge: _charge,
                bound,
                ..
            } = entry;
            let whole = match bound {
                Some(Bound {
                    owner: Some(turn),
                    at,
                }) => {
                    let whole = self.deliver(item, Some((turn, at))).await;
                    // Refused by the registration's seal: its loss is
                    // recorded before it stops being outstanding (§3.2).
                    if whole.is_none() {
                        losses(&self.loss.losses).note_turn(turn, self.loss.generation, seq, 1);
                        self.registration.mark_incomplete();
                    }
                    whole.unwrap_or(false)
                }
                Some(Bound { owner: None, .. }) | None => true,
            };
            let mark = self.releasing.and_then(|(_, mark)| mark);
            self.releasing = self.early.front().map(|next| (next.seq, next.mark));
            self.publish();
            self.handled(mark, whole);
        }
    }

    /// The held turn sealed: it closes, then a terminal its seal judged
    /// late goes out as its late terminal (X4 code review r2 #2; C2 §4.1
    /// "Late observations"), attributed to its vendor turn, at now, after
    /// everything the consumer sent before.
    async fn close_sealed(&mut self) {
        let late = self.held.as_ref().and_then(|held| {
            let retained = held.cx.delivery.take_late()?;
            Some((held.accepted.clone().unwrap_or_default(), retained))
        });
        if let Some(held) = self.held.as_ref()
            && late.is_none()
            && held.phase == Phase::Running
        {
            self.bare.push(held.turn);
        }
        self.close();
        if let Some((turn, retained)) = late {
            let Retained {
                mut terminal,
                structured,
            } = retained;
            (
                terminal.structured_output,
                terminal.structured_output_unparsed,
            ) = super::driver::carried(structured);
            let idle = Arc::clone(&self.registration.idle);
            let late = Observation::LateTerminal(terminal);
            self.output(&idle, &turn, late, (None, Instant::now()))
                .await;
        }
    }

    /// The held turn sealed (or closes): a pending turn is never mapped,
    /// so its retained items are loss; a running one's normalizer and
    /// vendor ID go, and its credit goes to the ledger's ranges (§3.3).
    fn close(&mut self) {
        let Some(held) = self.held.take() else {
            return;
        };
        if held.phase == Phase::Pending && !self.early.is_empty() {
            self.lose_early(held.turn);
        }
        self.registration.ledger().close(held.turn, held.cx.credit);
        self.gap = None;
        self.disposed = 0;
    }

    /// x.3.2 X3 §3.2: turn `turn` can never be mapped: its retained items
    /// are its loss, from the outstanding position, and continuity is
    /// unproven.
    fn lose_early(&mut self, turn: TurnNumber) {
        if let Some(first) = self.outstanding() {
            let count = u64::try_from(self.early.len()).unwrap_or(UNKNOWN);
            losses(&self.loss.losses).note_turn(turn, self.loss.generation, first, count);
            self.registration.mark_incomplete();
        }
        self.drop_early();
    }

    fn drop_early(&mut self) {
        self.early.clear();
        self.releasing = None;
        self.publish();
    }

    /// x.3.2 X3 §3.2 (r11 #1, r12 #1): the outstanding position, the
    /// earliest retained or releasing message; the one bound every loss
    /// snapshot (a seal's floor, retirement, the consumer's end) is floored
    /// by, so no loss starts past an item that has not gone out whole. An
    /// entry is `releasing` before it leaves `early` and stays so until it
    /// went out whole or its loss was recorded; the registration's copy is
    /// republished after every change of either.
    fn outstanding(&self) -> Option<u64> {
        let releasing = self.releasing.map(|(seq, _)| seq);
        let front = self.early.front().map(|entry| entry.seq);
        match (releasing, front) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn publish(&self) {
        self.registration.publish(self.outstanding());
    }

    /// The lane's end (x.3.2 X3 §3.2, r11 #1): another end first stops the
    /// held turn. With nothing outstanding the prefix was disposed of
    /// (`Cut`, `LaneEnded`); otherwise its loss is noted from the
    /// outstanding position and the end is `Unproven`.
    fn end(&mut self, end: LaneEnd) {
        if end == LaneEnd::Overflow {
            self.overflow();
        }
        if matches!(end, LaneEnd::Quarantined | LaneEnd::Overflow) {
            return self.dispose_failed();
        }
        if end != LaneEnd::Closed
            && let Some(held) = &self.held
        {
            held.cx.delivery.stop(Stop::Lane(end));
        }
        let drained = match self.outstanding() {
            None if end == LaneEnd::Closed => Drained::Cut,
            None => Drained::LaneEnded(end),
            Some(first) => {
                let position = first.min(self.registration.idle.position());
                losses(&self.loss.losses).note(self.loss.generation, position, UNKNOWN);
                self.registration.mark_incomplete();
                self.drop_early();
                Drained::Unproven(end)
            }
        };
        self.registration.drained(drained);
    }

    /// x.3.2 X3 §4.4: after the generation's failure, what the lane still
    /// holds up to its end is disposed of without waiting, and nothing
    /// goes out: markers and retained items are dropped (their loss is
    /// the failure's), a tool item or a decline updates the ledger with a
    /// charge taken at once; one the ledger cannot retain leaves
    /// continuity unproven and is lost.
    fn dispose_failed(&mut self) {
        self.held = None;
        self.drop_early();
        while let Some(LaneEvent::Item(item, _charge)) = self.lane.try_next() {
            let Some(routed) = item.routed() else {
                continue;
            };
            let (owner, seq) = (self.owner(routed), routed.seq);
            let parsed = parse(&item);
            let charged = match self.wanted(owner, &parsed) {
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
            let kept = charged && self.tracked(owner, &parsed).is_ok();
            self.registration.ledger().unstage();
            if !kept {
                self.registration.mark_incomplete();
                losses(&self.loss.losses).note(self.loss.generation, seq, UNKNOWN);
            }
        }
        self.registration.drained(Drained::ConsumerFailed);
    }

    /// Whose `routed` is, in the consumer's state.
    fn owner(&self, routed: &Routed) -> Owner {
        let running = self
            .held
            .as_ref()
            .filter(|held| held.phase != Phase::Pending);
        match (routed.owner, routed.turn.as_deref()) {
            (Some(owner), _) if running.is_some_and(|held| held.turn == owner) => Owner::This,
            (Some(owner), _) => Owner::Earlier(owner),
            (None, Some(turn))
                if running.is_some_and(|held| held.accepted.as_deref() == Some(turn)) =>
            {
                Owner::This
            }
            (None, Some(_)) => Owner::Unknown,
            (None, None) => Owner::Thread,
        }
    }

    /// The seal a message of `owner` goes out under: the running turn's
    /// for its own, thread-level or unseen traffic; the registration's for
    /// the late path and while no turn runs.
    fn delivery_for(&self, owner: Owner) -> Arc<Delivery> {
        match (&self.held, owner) {
            (Some(held), Owner::This | Owner::Thread | Owner::Unknown)
                if held.phase != Phase::Pending =>
            {
                Arc::clone(&held.cx.delivery)
            }
            _ => Arc::clone(&self.registration.idle),
        }
    }

    /// x.3.2 X3 §3.5: the item at `mark` was handled, whole or not. Under
    /// the live fence a whole one is disposed of; one that is not is loss
    /// (the gap). Then the frontier is reported.
    fn handled(&mut self, mark: Option<Mark>, whole: bool) {
        let live = self.held.as_ref().map(|held| held.fence);
        if let Some(mark) = mark.filter(|mark| Some(mark.fence) == live) {
            if whole {
                self.disposed = self.disposed.max(mark.seq);
            } else {
                self.gap = Some(self.gap.map_or(mark.seq, |gap| gap.min(mark.seq)));
            }
        }
        self.report();
    }

    /// x.3.2 X3 §3.5: publishes the live fence's frontier, `min(disposed,
    /// outstanding - 1, gap - 1)`, when it advances, under the held turn's
    /// seal: never past a retained or releasing item, or a loss.
    fn report(&mut self) {
        let outstanding = {
            let releasing = self.releasing.and_then(|(_, mark)| mark);
            let front = self.early.front().and_then(|entry| entry.mark);
            match (releasing, front) {
                (Some(a), Some(b)) => Some(a.seq.min(b.seq)),
                (a, b) => a.or(b).map(|mark| mark.seq),
            }
        };
        let Some(held) = self.held.as_mut() else {
            return;
        };
        let mut through = self.disposed;
        if let Some(outstanding) = outstanding {
            through = through.min(outstanding.saturating_sub(1));
        }
        if let Some(gap) = self.gap {
            through = through.min(gap.saturating_sub(1));
        }
        if through > held.reported && held.cx.delivery.report(&held.cx.activity, through) {
            held.reported = through;
        }
    }

    /// One message or placeholder of `owner`, decoded now; its staging
    /// permit goes with it. Under the held turn's seal it is taken first
    /// (x.3.2 X3 §3.2: refused, it is handled again with the turn closed).
    /// The ledger is updated before any output, in every state, whatever
    /// goes out (§6.3), once the charge of an entry it may insert was
    /// reserved (§6.2); its output goes out at its read instant or `at`,
    /// a retained item's stamp.
    async fn handle(&mut self, item: LaneItem, owner: Owner, at: Option<Instant>) -> Handled {
        let Some(routed) = item.routed() else {
            return Handled::Done(true);
        };
        let seq = routed.seq;
        let decoded_at = routed.at;
        let at = at.unwrap_or(decoded_at);
        let parsed = parse(&item);
        let delivery = self.delivery_for(owner);
        let idle = Arc::ptr_eq(&delivery, &self.registration.idle);
        if idle {
            idle_seam().await;
        } else {
            take_seam().await;
            if !delivery.take(seq) {
                return Handled::Refused(item);
            }
        }
        if let Some(key) = self.wanted(owner, &parsed)
            && !self.stage(key, seq).await
        {
            return Handled::Done(false);
        }
        // A staged charge is held through the message's whole metadata
        // update, its denial included (§6.2), then released if unused.
        let _unstage = Unstage(Arc::clone(&self.registration));
        let tracked = self.tracked(owner, &parsed);
        if idle && !delivery.take(seq) {
            return Handled::Sealed;
        }
        if tracked.is_err() {
            self.overflow();
            return Handled::Done(false);
        }
        match (item, parsed) {
            (item, Parsed::Malformed) => {
                let bytes = item.routed().map(|routed| routed.staged.bytes().to_vec());
                self.malformed(owner, &bytes.unwrap_or_default()).await;
            }
            (LaneItem::Message(routed), Parsed::Notification(notification)) => {
                let named = routed.turn;
                drop(routed.staged);
                match (owner, named) {
                    (Owner::Earlier(earlier), Some(turn)) => {
                        self.late(
                            &delivery,
                            &notification,
                            (earlier, &turn),
                            (at, seq, routed.mark),
                        )
                        .await;
                    }
                    _ => {
                        self.notification(&delivery, owner, &notification, (at, decoded_at))
                            .await;
                    }
                }
            }
            (
                LaneItem::Declined {
                    routed,
                    decoded_at,
                    written,
                },
                Parsed::Request(request),
            ) => {
                drop(routed.staged);
                self.declined(
                    &delivery,
                    owner,
                    &request,
                    (decoded_at, at),
                    (seq, routed.mark),
                    written,
                )
                .await;
            }
            (_, Parsed::Notification(_) | Parsed::Request(_)) => {
                delivery.complete(self.tools_open());
            }
        }
        self.quiesced(decoded_at);
        Handled::Done(delivery.whole(seq))
    }

    /// x.3.2 X4 D4.1: a draining turn whose tools all ended in the ledger,
    /// by the message decoded at `decoded_at`, leaves its P7 window: its
    /// terminal decides, and it is `Retained`.
    fn quiesced(&mut self, decoded_at: Instant) {
        let draining = self
            .held
            .as_ref()
            .is_some_and(|held| held.phase == Phase::Draining);
        if !draining || self.tools_open() {
            return;
        }
        if let Some(held) = self.held.as_mut() {
            held.cx.delivery.drained(decoded_at);
            held.phase = Phase::Retained;
        }
    }

    /// The key bytes of the ledger entry `parsed` of `owner` may insert
    /// (x.3.2 X3 §6.2).
    fn wanted(&self, owner: Owner, parsed: &Parsed) -> Option<usize> {
        let turn = self.entry_turn(owner)?;
        let ledger = self.registration.ledger();
        match parsed {
            Parsed::Request(request) => ledger.wants_decline(turn, request),
            Parsed::Notification(notification) => ledger.wants(turn, notification),
            Parsed::Malformed => None,
        }
    }

    /// The ledger's update for `parsed` of `owner` (x.3.2 X3 §6.3).
    fn tracked(&self, owner: Owner, parsed: &Parsed) -> Result<(), NormalizeError> {
        let Some(turn) = self.entry_turn(owner) else {
            return Ok(());
        };
        let mut ledger = self.registration.ledger();
        match parsed {
            Parsed::Request(request) => ledger.note_decline(turn, request),
            Parsed::Notification(notification) => ledger.track(turn, notification),
            Parsed::Malformed => Ok(()),
        }
    }

    /// The turn whose ledger entry a message of `owner` may update: the
    /// running turn's (its own or a thread-level message it judges), or
    /// an earlier turn's by the connection's mapping.
    fn entry_turn(&self, owner: Owner) -> Option<TurnNumber> {
        let running = self
            .held
            .as_ref()
            .filter(|held| held.phase != Phase::Pending);
        match (owner, running) {
            (Owner::This | Owner::Thread, Some(held)) => Some(held.turn),
            (Owner::Earlier(turn), _) => Some(turn),
            (Owner::This | Owner::Thread | Owner::Unknown, _) => None,
        }
    }

    /// Whether the running turn has a tool open, in the ledger.
    fn tools_open(&self) -> bool {
        self.held.as_ref().is_some_and(|held| {
            held.phase != Phase::Pending && self.registration.ledger().tools_open(held.turn)
        })
    }

    /// x.3.2 X3 §6.2: reserves the charge of the ledger entry the message
    /// may insert, before the ledger is updated. The cap slot is taken at
    /// once: a full cap fails the connection `overflow` (§6.4). The budget
    /// bytes are awaited by the message's stall deadline (then it fails
    /// `overflow`), or until the generation fails, the registration
    /// retires or the session is cancelled. False when the wait ended
    /// without its charge: continuity is unproven and message `seq` is
    /// lost.
    async fn stage(&mut self, key: usize, seq: u64) -> bool {
        let cap = self.registration.ledger().cap().clone();
        let Some(slot) = cap.slot(key) else {
            self.registration.ledger().exhaust();
            self.evidence.server.overflow();
            self.overflow();
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
        // Err(true): the generation fails; Err(false): the consumer ends.
        let charged = tokio::select! {
            biased;
            _ = health.wait_for(|health| matches!(health, DriverHealth::Failed { .. })) => {
                Err(true)
            }
            () = registration.failed() => Err(false),
            () = registration.retiring.cancelled() => Err(false),
            () = self.cancel.cancelled() => Err(false),
            charged = tokio::time::timeout_at(self.stall_by, cap.charge(slot)) => {
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
            Err(failing) => {
                self.registration.mark_incomplete();
                losses(&self.loss.losses).note(self.loss.generation, seq, UNKNOWN);
                if failing {
                    self.overflow();
                }
                false
            }
        }
    }

    /// x.3.2 X3 §3.4, §3.5: an observation of earlier turn `turn`, never
    /// mapped to Core, is lost, not relabeled: the loss is noted for that
    /// turn, continuity is unproven (§6.6), and a live fence it was read
    /// under reports nothing from its position on (the gap).
    fn lose(&mut self, delivery: &Delivery, turn: TurnNumber, (seq, mark): (u64, Option<Mark>)) {
        losses(&self.loss.losses).note_turn(turn, self.loss.generation, seq, 1);
        self.registration.mark_incomplete();
        let live = self.held.as_ref().map(|held| held.fence);
        if let Some(mark) = mark.filter(|mark| Some(mark.fence) == live) {
            self.gap = Some(self.gap.map_or(mark.seq, |gap| gap.min(mark.seq)));
        }
        delivery.complete(self.tools_open());
    }

    /// X0 item 5 steps 5 and 6, x.3.2 X3 §4.1: the generation fails
    /// `protocol` first, before any await; then the evidence goes to the
    /// turn the message names, else to the server folder, and its note to
    /// the registration for the failure's report (fix r1 #4).
    async fn malformed(&mut self, owner: Owner, bytes: &[u8]) {
        self.registration.note(Note::Keeping);
        self.protocol(UNDECODED);
        let named = match owner {
            Owner::This => self
                .held
                .as_ref()
                .map(|held| (held.turn, "the turn's message")),
            Owner::Earlier(turn) => Some((turn, "an earlier turn's message")),
            Owner::Unknown | Owner::Thread => None,
        };
        let folder = named.and_then(|(turn, what)| {
            let folders = self
                .evidence
                .earlier
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            folders.get(&turn).map(|folder| (Arc::clone(folder), what))
        });
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
        self.registration.note(Note::Kept(undecoded));
    }

    /// A decoded notification: the running turn's, or thread-level, is
    /// normalized under its seal; anything else gives nothing.
    async fn notification(
        &mut self,
        delivery: &Arc<Delivery>,
        owner: Owner,
        notification: &Notification,
        (at, decoded_at): (Instant, Instant),
    ) {
        let running = self
            .held
            .as_mut()
            .filter(|held| held.phase.taking() && matches!(owner, Owner::This | Owner::Thread));
        let Some(held) = running else {
            delivery.complete(self.tools_open());
            return;
        };
        // Its read instant, not now (C2 §4): time in the lane moves
        // nothing.
        held.cx.activity.record(at);
        // x.3.2 X4 I1: the turn's first terminal is never replaced; a
        // later one, live or from the early tail, is handled whole with
        // nothing emitted or retained, whatever the normalizer would make
        // of it.
        if held.phase == Phase::Draining && matches!(notification, Notification::TurnCompleted(_)) {
            delivery.complete(self.tools_open());
            return;
        }
        let step = match held.normalizer.as_mut() {
            Some(normalizer) => normalizer.observe(notification, at),
            None => Ok(Step::Activity),
        };
        let step = match step {
            Ok(step) => step,
            Err(NormalizeError::Protocol(detail)) => {
                return self.protocol(detail);
            }
            Err(NormalizeError::Overflow) => {
                return self.overflow();
            }
        };
        let tools_open = self.tools_open();
        let accepted = self
            .held
            .as_ref()
            .and_then(|held| held.accepted.clone())
            .unwrap_or_default();
        match step {
            Step::Activity => {
                delivery.complete(tools_open);
            }
            Step::Observations(observations) => {
                let count = observations.len();
                if count == 0 {
                    delivery.complete(tools_open);
                    return;
                }
                for (index, observation) in observations.into_iter().enumerate() {
                    let last = (index + 1 == count).then_some(tools_open);
                    if !self
                        .output(delivery, &accepted, observation, (last, at))
                        .await
                    {
                        return;
                    }
                }
            }
            Step::Terminal {
                terminal,
                structured,
            } => {
                // x.3.2 X4 D4.1: an interrupted terminal with a tool open
                // drains (its P7 window), timed from its original decode.
                let interrupted = terminal.status == VendorTerminalStatus::Interrupted;
                let draining = interrupted && tools_open;
                let retained = Retained {
                    terminal: *terminal,
                    structured,
                };
                if delivery.retain(retained, (tools_open, draining), decoded_at)
                    && let Some(held) = self.held.as_mut()
                {
                    held.phase = if draining {
                        Phase::Draining
                    } else {
                        Phase::Retained
                    };
                    // D7: vendor evidence acknowledged the stop; reported
                    // outside the Delivery lock.
                    if interrupted {
                        held.cx.stop_ack.acknowledged();
                    }
                }
            }
        }
    }

    /// An earlier turn's notification (x.3.2 X3 fix r2 #3, r4 #2): a
    /// denial of one of its items is that turn's late observation, named
    /// by its vendor turn `turn` (Core records it `late`, the turn's
    /// envelope unchanged), judged against the session's suppression
    /// table, under the registration's seal; anything else of it gives
    /// nothing.
    async fn late(
        &mut self,
        delivery: &Arc<Delivery>,
        notification: &Notification,
        (earlier, turn): (TurnNumber, &str),
        (at, seq, mark): (Instant, u64, Option<Mark>),
    ) {
        // X4 code review r2 #2 (C2 §4.1 "Late observations"): the
        // terminal of a turn that ended with none is its late terminal,
        // sent once.
        if let Notification::TurnCompleted(event) = notification
            && let Some(index) = self.bare.iter().position(|bare| *bare == earlier)
            && let Ok(terminal) = normalize::vendor_terminal(&event.turn, at, None)
        {
            self.bare.swap_remove(index);
            let late = Observation::LateTerminal(terminal);
            self.output(delivery, turn, late, (Some(false), at)).await;
            return;
        }
        let denial = self
            .registration
            .ledger()
            .late_denial(earlier, notification);
        let Ok(denial) = denial else {
            return self.overflow();
        };
        match denial {
            Some(_) if !self.registration.ledger().mapped(earlier) => {
                self.lose(delivery, earlier, (seq, mark));
            }
            Some(denial) => {
                let denied = Observation::ActionDenied(denial);
                self.output(delivery, turn, denied, (Some(false), at)).await;
            }
            None => {
                delivery.complete(false);
            }
        }
    }

    /// X0 item 11: a placeholder of the running turn is reported once its
    /// reply was written whole by `decoded_at + 5 s`; an earlier turn's
    /// likewise, as that turn's late observation (x.3.2 X3 fix r2 #3, r4
    /// #2; [`Self::handle`] noted it in the session's suppression table);
    /// one naming another turn, or none, gives nothing. It goes out at
    /// `at`: its decode instant, or a retained item's stamp.
    async fn declined(
        &mut self,
        delivery: &Arc<Delivery>,
        owner: Owner,
        request: &ServerRequest,
        (decoded_at, at): (Instant, Instant),
        (seq, mark): (u64, Option<Mark>),
        mut written: watch::Receiver<Option<bool>>,
    ) {
        let running = self.held.as_ref().filter(|held| held.phase.taking());
        let named = match (owner, request.turn_id.as_deref(), running) {
            (Owner::This, _, Some(held)) => {
                held.cx.activity.record(at);
                held.accepted.clone().unwrap_or_default()
            }
            (Owner::Earlier(_), Some(turn), _) => turn.to_owned(),
            (Owner::This | Owner::Earlier(_) | Owner::Unknown | Owner::Thread, _, _) => {
                delivery.complete(self.tools_open());
                return;
            }
        };
        let whole = tokio::select! {
            biased;
            () = delivery.sealed.cancelled() => return,
            outcome = tokio::time::timeout_at(
                decoded_at + DECLINE_DEADLINE,
                written.wait_for(Option::is_some),
            ) => outcome.is_ok_and(|outcome| outcome.is_ok_and(|outcome| *outcome == Some(true))),
        };
        if !whole {
            delivery.complete(self.tools_open());
            return;
        }
        if let Owner::Earlier(earlier) = owner
            && !self.registration.ledger().mapped(earlier)
        {
            return self.lose(delivery, earlier, (seq, mark));
        }
        let declined = Observation::RequestDeclined(normalize::decline(request));
        let tools_open = self.tools_open();
        self.output(delivery, &named, declined, (Some(tools_open), at))
            .await;
    }

    /// Hands one observation naming vendor turn `turn` to the sink under
    /// `delivery`'s seal: its room reserved outside the seal, the send
    /// made under it. `last` (with the open tools) completes the message.
    /// A wait past the message's stall deadline latches the observation
    /// overflow and fails the generation, as the lane's overflow does while
    /// the turn is pending; false once nothing more goes out.
    async fn output(
        &mut self,
        delivery: &Arc<Delivery>,
        turn: &str,
        observation: Observation,
        (last, at): (Option<bool>, Instant),
    ) -> bool {
        let item = ObservationItem {
            at,
            vendor_turn: VendorTurnId::try_from(turn.to_owned()).ok(),
            observation,
        };
        // A pending turn's acceptance in flight fails at the lane's
        // overflow (§3.2), as a pending turn's next item would.
        let pending = self
            .held
            .as_ref()
            .is_some_and(|held| held.phase == Phase::Pending);
        let sent = {
            let reserved = tokio::select! {
                biased;
                () = delivery.sealed.cancelled() => return false,
                () = self.lane.overflowed(), if pending => {
                    self.overflow();
                    return false;
                }
                reserved = tokio::time::timeout_at(
                    self.stall_by,
                    self.sink.reserve(&item, self.stall_by.saturating_duration_since(Instant::now())),
                ) => reserved.unwrap_or(Err(Undelivered::Stalled)),
            };
            reserved.map(|reserved| delivery.send(reserved, item, last))
        };
        match sent {
            Ok(true) => {}
            Ok(false) => return false,
            Err(_) => {
                latch(&self.health, DriverFailure::ObservationOverflow);
                self.overflow();
                return false;
            }
        }
        admitted().await;
        true
    }
}

/// Releases a staged charge no entry took, when dropped.
struct Unstage(Arc<Registration>);

impl Drop for Unstage {
    fn drop(&mut self) {
        self.0.ledger().unstage();
    }
}

/// A lane item's full decode, at consumption.
enum Parsed {
    Notification(Notification),
    Request(ServerRequest),
    Malformed,
}

/// Decodes a message or a placeholder's request (x.3.2 X3 §5.3: a
/// placeholder is decoded only here, where its failure fails only this
/// generation).
fn parse(item: &LaneItem) -> Parsed {
    let Some(routed) = item.routed() else {
        return Parsed::Malformed;
    };
    match (item, decode(routed.staged.bytes())) {
        (LaneItem::Message(_), Ok(Incoming::Notification(notification))) => {
            Parsed::Notification(notification)
        }
        (LaneItem::Declined { .. }, Ok(Incoming::Request(request))) => Parsed::Request(request),
        _ => Parsed::Malformed,
    }
}

/// Test builds: a seam between a report's seal check and its
/// publication (Sol code r1 #9), where a test holds the report while a
/// seal races it.
fn report_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit("adapter.codex.report");
    }
}

/// Test builds: a marker as a seal is about to take the seal's lock (Sol
/// code r2 #4), which a test awaits before releasing a held report.
fn seal_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit("adapter.codex.seal");
    }
}

/// Test builds: a seam at the top of the consumer's loop (x.3.2 X3 r5
/// #1), where a test holds it while a turn's traffic arrives.
#[cfg_attr(
    not(feature = "test-failpoints"),
    expect(clippy::unused_async, reason = "only test builds wait at the seam")
)]
async fn idle_check_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit_async("adapter.codex.idle_check").await;
    }
}

/// Test builds: a seam between an item's pop and its take under the
/// registration's seal (x.3.2 X3 fix r3 #3, r6 #3), where a test seals the
/// registration.
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

/// Test builds: a seam before an item's take under the held turn's seal
/// (x.3.2 X3 r5 #6), where a test seals the turn.
#[cfg_attr(
    not(feature = "test-failpoints"),
    expect(clippy::unused_async, reason = "only test builds wait at the seam")
)]
async fn take_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit_async("adapter.codex.consumer_take").await;
    }
}

/// Test builds: a seam between a retained entry's leaving `early` and its
/// publication (x.3.2 X3 S14 (e)).
#[cfg_attr(
    not(feature = "test-failpoints"),
    expect(clippy::unused_async, reason = "only test builds wait at the seam")
)]
async fn handoff_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit_async("adapter.codex.early_handoff").await;
    }
}

#[cfg(test)]
#[path = "consumer_tests.rs"]
mod consumer_tests;

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
        Delivery, Drained, Evidence, Folders, LossRecord, Losses, Normalizing, ObservationLoss,
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
        assert!(delivery.retain(terminal(), (true, false), tokio::time::Instant::now()));
        assert!(delivery.decided());
        let sealed = delivery.seal();
        assert!(sealed.terminal.is_some());
        assert!(sealed.tools_open);
        assert_eq!(sealed.position, 2);

        let late = Delivery::new(0);
        assert!(late.take(1));
        let sealed = late.seal();
        assert!(sealed.terminal.is_none());
        assert!(!late.retain(terminal(), (false, false), tokio::time::Instant::now()));
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
            detail: super::UNDECODED,
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
                &Delivery::new(4),
            )
            .unwrap();
        let delivery = Delivery::new(4);
        let third = run
            .registration
            .admit(turn(3), Arc::new(|| {}), &delivery)
            .unwrap();
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
            run.registration
                .admit(turn(4), Arc::new(|| {}), &Delivery::new(4))
                .is_none(),
            "nothing is admitted on a failed generation"
        );
        // Its note waits for the held keep (fix r1 #4) and resolves empty
        // once the consumer is gone.
        let note = tokio::time::timeout(Duration::from_millis(50), run.registration.undecoded());
        assert!(note.await.is_err(), "the note waits for its keep");
        drop((second, third));
        consumer.abort();
        let _aborted = consumer.await;
        assert_eq!(run.registration.undecoded().await, None);
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
            assert_eq!(drained, None);
            run.registration.seal();
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
        assert_eq!(drained, Some(Drained::ConsumerCancelled));
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
        assert_eq!(drained, Some(Drained::Cut));
        run.registration.seal();
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

    /// X0 item 8.2 (x.3.2 X3 fix r4 #4, §5.4): a close whose consumer
    /// took the cut still records the loss of a message its seal found
    /// delivered in part; a whole cut records nothing, and what the cutoff
    /// dropped is diagnostics, not loss. A cancelled or timed-out consumer
    /// loses everything from the position, count unknown, and leaves
    /// continuity unproven.
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
        let sealed = partial.seal();
        assert!(!losses.note_close(2, Some(Drained::Cut), (&sealed, sealed.position)));
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
        let sealed = whole.seal();
        assert!(!kept.note_close(2, Some(Drained::Cut), (&sealed, 11)));
        assert!(kept.record.is_none());
        for drained in [
            Some(Drained::LaneEnded(LaneEnd::Retired)),
            Some(Drained::Unproven(LaneEnd::Closed)),
            Some(Drained::ConsumerFailed),
        ] {
            assert!(!kept.note_close(2, drained, (&sealed, 11)));
            assert!(kept.record.is_none(), "{drained:?} noted its own");
        }
        for drained in [Some(Drained::ConsumerCancelled), None] {
            let mut lost = Losses {
                record: None,
                latest: Some(turn(1)),
            };
            assert!(lost.note_close(2, drained, (&sealed, 7)));
            assert_eq!(
                lost.record
                    .map(|record| (record.first_unqueued, record.omitted)),
                Some((7, UNKNOWN))
            );
        }
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
