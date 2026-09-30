//! Per-session dispatch state (C1 §7.3): the session's queue of receipted,
//! unsubmitted turns with their claims (design §3.1), the running turn's
//! phase and stop-order sender (§2), the close order (§4) and whether its one
//! dispatcher task exists. Whether the queue head may run is decided from
//! durable state (`Engine::decide`), never remembered here.
//!
//! Slot state is a `std` mutex: never held across an `.await`, taken under
//! `admission` and `sessions`, never the reverse (design §1).

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use serde_json::Value;
use tokio::sync::{Notify, watch};
use via_adapters::{StopCause, StopOrder};
use via_store::{CancelCause, CloseIntent};

use super::journal::Head;
use super::lock;
use super::progress::{Progress, ProgressDelta};
use crate::api::{CloseMode, rfc3339};
use crate::{ApiError, Deadline, TurnNumber};
/// Most queued turns one session holds (C1 P6), also enforced by Store.
pub(super) const SESSION_QUEUE_LIMIT: u32 = via_store::SESSION_QUEUE_LIMIT;

/// Most queued turns the daemon holds across sessions (runtime §8); also the
/// dispatcher-start channel's capacity.
pub(super) const DAEMON_QUEUE_LIMIT: usize = 128;

/// Active private connections daemon-wide (runtime §8, design §11).
pub(super) const CONNECTION_SLOTS: usize = 4;

/// First delay after a failed Store read or while waiting on an unowned predecessor.
const RETRY_MIN: Duration = Duration::from_millis(250);

/// Longest delay between retries.
const RETRY_MAX: Duration = Duration::from_secs(5);

/// The cleanup allowance after a stop's work deadline (runtime §5.2).
pub(super) const CLOSE_ALLOWANCE: Duration = Duration::from_secs(3);

/// Grace an idle-deadline stop gives the vendor before force (design §2).
const IDLE_GRACE: Duration = Duration::from_secs(10);

/// Whether the session's dispatcher task exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Dispatcher {
    None,
    /// Requested from daemon main, not yet running.
    Starting,
    Live,
}

/// Who owns a queued turn's cancellation (design §3.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Owner {
    /// One `cancel` request, from `Waiting` only.
    Request,
    /// The dispatcher: a rollback with an order, the close pass or force.
    Dispatcher,
}

/// A queue entry's claim (design §3.1). Claims move only under slot state,
/// and only the claim owner writes the turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Claim {
    Waiting,
    /// The dispatcher reserved a connection slot and grants and submits it.
    Claimed,
    Cancelling(Owner),
}

/// How a queued turn's cancellation ended, published to every caller that
/// joined it (design §3.1 "one owner per cancellation").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueuedOutcome {
    /// Durably cancelled.
    Committed,
    /// A read before the commit failed: nothing was written.
    ReadFailed,
    /// The commit did not happen.
    NotCommitted,
    /// The commit may have happened: the daemon latches.
    Uncertain,
}

/// A stop order's acknowledgement by the run loop (design §2 durability).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Ack {
    /// `cancel.requested` committed with this `requested_at`.
    Requested(String),
    /// `cancel.requested` could not be recorded.
    Failed,
}

/// A submitted-or-claimed turn's stop channels. Dropping it closes both
/// watches, which waiters observe as the turn leaving the run loop.
pub(super) struct TurnStop {
    order: watch::Sender<Option<StopOrder>>,
    ack: watch::Sender<Option<Ack>>,
}

impl TurnStop {
    fn new() -> Self {
        Self {
            order: watch::Sender::new(None),
            ack: watch::Sender::new(None),
        }
    }

    /// Attaches `order`, or coalesces it into the one already attached: the
    /// first `requested_at` and cause stay, a `store` cause overrides, a
    /// `protocol` cause overrides any but `store`, and
    /// the earlier `force_at` and `close_by` win (design §2).
    fn attach(&self, order: StopOrder) {
        self.order.send_modify(|current| match current {
            Some(existing) => {
                if order.cause == StopCause::Store
                    || (order.cause == StopCause::Protocol && existing.cause != StopCause::Store)
                {
                    existing.cause = order.cause;
                }
                if order.force_at.instant() < existing.force_at.instant() {
                    existing.force_at = order.force_at;
                }
                if order.close_by.instant() < existing.close_by.instant() {
                    existing.close_by = order.close_by;
                }
            }
            None => *current = Some(order),
        });
    }
}

/// What a stop order asks for, before the turn's wall deadline caps it
/// (design §2's table).
#[derive(Clone, Copy, Debug)]
pub(super) enum StopSpec {
    /// A caller `cancel` with its grace.
    Cancel { force_after: Duration },
    /// A session `close`.
    Close {
        mode: CloseMode,
        deadline: tokio::time::Instant,
    },
    /// Core's idle deadline.
    Idle,
    /// The turn's own write did not commit (design §7.2 row 5).
    Store,
    /// Core refused the vendor's evidence (review r1): stops as `Store`.
    Protocol,
}

impl StopSpec {
    /// The order at `now`, capped by the turn's wall deadline once it is
    /// known (a claimed turn has none yet: it never launches under an order).
    pub(super) fn order(
        self,
        requested_at: String,
        now: tokio::time::Instant,
        wall: Option<tokio::time::Instant>,
    ) -> StopOrder {
        let cap = |at: tokio::time::Instant| wall.map_or(at, |wall| at.min(wall));
        let (cause, force_at, close_by) = match self {
            Self::Cancel { force_after } => {
                let force_at = cap(now + force_after);
                (StopCause::Cancel, force_at, force_at + CLOSE_ALLOWANCE)
            }
            Self::Idle => {
                let force_at = cap(now + IDLE_GRACE);
                (
                    StopCause::IdleDeadline,
                    force_at,
                    force_at + CLOSE_ALLOWANCE,
                )
            }
            Self::Store | Self::Protocol => {
                let close_by = now + CLOSE_ALLOWANCE;
                let close_by = wall.map_or(close_by, |wall| close_by.min(wall + CLOSE_ALLOWANCE));
                let cause = if matches!(self, Self::Store) {
                    StopCause::Store
                } else {
                    StopCause::Protocol
                };
                (cause, now, close_by.max(now))
            }
            Self::Close { mode, deadline } => {
                let force_at = match mode {
                    CloseMode::Graceful => cap(deadline
                        .checked_sub(CLOSE_ALLOWANCE)
                        .unwrap_or(now)
                        .max(now)),
                    CloseMode::Force => now,
                };
                let close_by = wall.map_or(deadline, |wall| deadline.min(wall + CLOSE_ALLOWANCE));
                (StopCause::Close, force_at, close_by.max(force_at))
            }
        };
        StopOrder {
            cause,
            requested_at,
            force_at: Deadline::at(force_at),
            close_by: Deadline::at(close_by),
        }
    }
}

/// The reply every waiter of one close attempt gets (design §1 close watch).
pub(super) type CloseReply = Result<Value, ApiError>;

/// One close attempt's watch: `None` until the dispatcher publishes. It is
/// retained by every handle, so a caller that subscribes after the
/// publication still reads the outcome [r5.8].
pub(super) type CloseWatch = watch::Sender<Option<CloseReply>>;

/// A session's close order (design §4 step 7).
pub(super) struct CloseOrder {
    pub(super) mode: CloseMode,
    pub(super) deadline: tokio::time::Instant,
    /// Wall time of the close's acceptance: its cancellations' `requested_at`.
    pub(super) requested_at: String,
    /// A keyed close's intent, recorded with `Closed`.
    pub(super) operation: Option<CloseIntent>,
    watch: CloseWatch,
}

impl CloseOrder {
    pub(super) fn new(
        mode: CloseMode,
        deadline: tokio::time::Instant,
        operation: Option<CloseIntent>,
    ) -> Self {
        Self {
            mode,
            deadline,
            requested_at: rfc3339(std::time::SystemTime::now()),
            operation,
            watch: watch::Sender::new(None),
        }
    }

    fn spec(&self) -> StopSpec {
        StopSpec::Close {
            mode: self.mode,
            deadline: self.deadline,
        }
    }
}

/// The close pass's view of the close order.
pub(super) struct CloseTask {
    pub(super) deadline: tokio::time::Instant,
    pub(super) requested_at: String,
    pub(super) operation: Option<CloseIntent>,
}

/// One queued turn and its claim.
struct Entry {
    turn: TurnNumber,
    claim: Claim,
    /// While `Claimed`: the turn's stop channels (design §2 delivery).
    stop: Option<TurnStop>,
    /// A dispatcher-owned cancellation's cause and `requested_at`.
    cause: Option<(CancelCause, String)>,
    /// The current cancellation's outcome; replaced when it rolls back.
    outcome: watch::Sender<Option<QueuedOutcome>>,
}

impl Entry {
    fn new(turn: TurnNumber) -> Self {
        Self {
            turn,
            claim: Claim::Waiting,
            stop: None,
            cause: None,
            outcome: watch::Sender::new(None),
        }
    }
}

/// The submitted turn the dispatcher runs inline.
struct Running {
    turn: TurnNumber,
    /// Adapter's `execute` returned: no order is sent any more [r1.4].
    settling: bool,
    stop: TurnStop,
    /// The turn's wall deadline, from the submission clock [r1.11].
    wall: tokio::time::Instant,
    /// The published progress (Task 4 design §2.4).
    progress: Progress,
}

/// Mutable dispatch state of one session.
struct State {
    /// Receipted turns without a confirmed submission, in number order.
    queue: VecDeque<Entry>,
    dispatcher: Dispatcher,
    running: Option<Running>,
    close: Option<CloseOrder>,
}

impl State {
    fn entry(&mut self, turn: TurnNumber) -> Option<&mut Entry> {
        self.queue.iter_mut().find(|entry| entry.turn == turn)
    }

    /// Requests the dispatcher when none exists; true when one must start.
    fn start(&mut self) -> bool {
        let start = self.dispatcher == Dispatcher::None;
        if start {
            self.dispatcher = Dispatcher::Starting;
        }
        start
    }
}

/// What a `cancel` found for its turn, under slot state (design §3).
pub(super) enum CancelStep {
    /// The turn was `Waiting`: the caller now owns its cancellation.
    Queued,
    /// An order was attached to the `Claimed` or running turn, or coalesced;
    /// the receiver reports its acknowledgement, and closes on the drop.
    Ordered(watch::Receiver<Option<Ack>>),
    /// The turn is settling: no order; the receiver closes on the drop.
    Settling(watch::Receiver<Option<Ack>>),
    /// Another owner is cancelling the queued turn: its outcome.
    Joined(watch::Receiver<Option<QueuedOutcome>>),
    /// Not queued, claimed or running here.
    Absent,
}

/// What the dispatcher found at the queue head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Front {
    Empty,
    /// A close order is set; the close pass runs first (design §4).
    Closing,
    Turn(TurnNumber, Claim),
}

/// What the close pass or force does next with the queue.
pub(super) enum Sweep {
    /// Cancel this dispatcher-owned turn, with its cause.
    Cancel(TurnNumber, Option<(CancelCause, String)>),
    /// Only request-owned cancellations are left: wait for a wake.
    Wait,
    /// The queue is empty.
    Done,
}

/// One session's queue, dispatcher wake and the event head every writer of
/// it shares. A clone of `head` is a writer lease: the slot is not retired
/// while anyone else holds one.
pub(super) struct Slot {
    pub(super) head: Arc<Head>,
    state: StdMutex<State>,
    wake: Notify,
}

impl Slot {
    pub(super) fn new(head: Arc<Head>) -> Arc<Self> {
        Arc::new(Self {
            head,
            state: StdMutex::new(State {
                queue: VecDeque::new(),
                dispatcher: Dispatcher::None,
                running: None,
                close: None,
            }),
            wake: Notify::new(),
        })
    }

    /// Adds a receipted turn `Waiting`, in number order, and wakes the
    /// dispatcher; true when no dispatcher exists and one must be started.
    pub(super) fn enqueue(&self, turn: TurnNumber) -> bool {
        let start = {
            let mut state = lock(&self.state);
            let at = state.queue.partition_point(|queued| queued.turn < turn);
            if state.queue.get(at).is_none_or(|entry| entry.turn != turn) {
                state.queue.insert(at, Entry::new(turn));
            }
            state.start()
        };
        self.wake();
        start
    }

    /// Wakes the dispatcher and every slot waiter; wakes before a wait
    /// coalesce into one.
    pub(super) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Waits for the next wake.
    pub(super) async fn woken(&self) {
        self.wake.notified().await;
    }

    /// The dispatcher task is running.
    pub(super) fn live(&self) {
        lock(&self.state).dispatcher = Dispatcher::Live;
    }

    /// The dispatcher's next concern: the close order first, else the head.
    pub(super) fn front(&self) -> Front {
        let state = lock(&self.state);
        if state.close.is_some() {
            return Front::Closing;
        }
        state
            .queue
            .front()
            .map_or(Front::Empty, |entry| Front::Turn(entry.turn, entry.claim))
    }

    /// Whether `turn` is still the `Waiting` head with no close order: the
    /// capacity wait's re-check (design §3.1).
    pub(super) fn waiting_head(&self, turn: TurnNumber) -> bool {
        let state = lock(&self.state);
        state.close.is_none()
            && state
                .queue
                .front()
                .is_some_and(|entry| entry.turn == turn && entry.claim == Claim::Waiting)
    }

    /// Claims the `Waiting` head for dispatch, unless a close order is set.
    /// Wakes: a claim change.
    pub(super) fn claim(&self, turn: TurnNumber) -> bool {
        let claimed = {
            let mut state = lock(&self.state);
            let open = state.close.is_none();
            match state.queue.front_mut() {
                Some(entry) if open && entry.turn == turn && entry.claim == Claim::Waiting => {
                    entry.claim = Claim::Claimed;
                    entry.stop = Some(TurnStop::new());
                    true
                }
                _ => false,
            }
        };
        if claimed {
            self.wake();
        }
        claimed
    }

    /// Whether a close order is set: the claim step's close check.
    pub(super) fn closing(&self) -> bool {
        lock(&self.state).close.is_some()
    }

    /// Rolls a `Claimed` turn back (design §3.1): to `Cancelling{dispatcher}`
    /// when a cancel order is attached, else to `Waiting`, where a close
    /// order's pass cancels it. Dropping its stop channels releases any
    /// waiter, which re-reads the claim. Wakes: a claim change.
    pub(super) fn rollback(&self, turn: TurnNumber) {
        {
            let mut state = lock(&self.state);
            if let Some(entry) = state.entry(turn)
                && entry.claim == Claim::Claimed
            {
                let order = entry
                    .stop
                    .take()
                    .and_then(|stop| stop.order.borrow().clone());
                match order {
                    Some(order) if order.cause == StopCause::Cancel => {
                        entry.claim = Claim::Cancelling(Owner::Dispatcher);
                        entry.cause = Some((CancelCause::Cancel, order.requested_at));
                    }
                    _ => entry.claim = Claim::Waiting,
                }
            }
        }
        self.wake();
    }

    /// The `Claimed → running` transition (design §2): the turn leaves the
    /// queue and its stop channels, with any attached order, move to the run
    /// loop. Returns Route's order receiver and the run loop's own.
    pub(super) fn start_running(
        &self,
        turn: TurnNumber,
        wall: tokio::time::Instant,
        progress: Progress,
    ) -> (
        watch::Receiver<Option<StopOrder>>,
        watch::Receiver<Option<StopOrder>>,
    ) {
        let mut state = lock(&self.state);
        let position = state.queue.iter().position(|entry| entry.turn == turn);
        let stop = position
            .and_then(|at| state.queue.remove(at))
            .and_then(|mut entry| entry.stop.take())
            .unwrap_or_else(TurnStop::new);
        let receivers = (stop.order.subscribe(), stop.order.subscribe());
        state.running = Some(Running {
            turn,
            settling: false,
            stop,
            wall,
            progress,
        });
        receivers
    }

    /// Applies one step-tracker delta to the running turn's published
    /// progress (Task 4 design §2.4), under the slot state mutex.
    pub(super) fn publish_progress(&self, turn: TurnNumber, delta: &ProgressDelta) {
        let mut state = lock(&self.state);
        if let Some(running) = state
            .running
            .as_mut()
            .filter(|running| running.turn == turn)
        {
            running.progress.apply(delta);
        }
    }

    /// A copy of the running turn's published progress when it is `turn`'s
    /// (Task 4 design §4.2), taken under the slot state mutex without a wait.
    pub(super) fn progress(&self, turn: u32) -> Option<Progress> {
        lock(&self.state)
            .running
            .as_ref()
            .filter(|running| running.progress.turn() == turn)
            .map(|running| running.progress.clone())
    }

    /// Issues the idle deadline's order (design §5) unless the turn is
    /// settling or already has an order: the timer disarms once any order
    /// exists, so it never shortens a cancel's or a close's `force_at`. The
    /// check and the attach are one transition under the slot state, which
    /// every other order's attach also takes [s2-r1.1]. Wakes: an order
    /// attached.
    pub(super) fn idle_order(&self, turn: TurnNumber, now: tokio::time::Instant) {
        let issued = {
            let state = lock(&self.state);
            match state
                .running
                .as_ref()
                .filter(|running| running.turn == turn && !running.settling)
            {
                Some(running) if running.stop.order.borrow().is_none() => {
                    let requested_at = rfc3339(std::time::SystemTime::now());
                    running.stop.attach(StopSpec::Idle.order(
                        requested_at,
                        now,
                        Some(running.wall),
                    ));
                    true
                }
                _ => false,
            }
        };
        if issued {
            self.wake();
        }
    }

    /// Sends the running turn an order with cause `store`, or upgrades its
    /// order to it (design §7.2 row 5): force at once, closing by
    /// `min(now + 3 s, wall + 3 s)`. A settling turn gets none: its
    /// disposition reads the turn's first failure. One transition under the
    /// slot state, like every other attach [s2-r1.1]. Wakes: an order
    /// attached.
    pub(super) fn store_order(&self, turn: TurnNumber, now: tokio::time::Instant) {
        self.failure_order(turn, now, StopSpec::Store);
    }

    /// As [`Self::store_order`], with cause `protocol`: Core refused the
    /// turn's vendor evidence (review r1).
    pub(super) fn protocol_order(&self, turn: TurnNumber, now: tokio::time::Instant) {
        self.failure_order(turn, now, StopSpec::Protocol);
    }

    fn failure_order(&self, turn: TurnNumber, now: tokio::time::Instant, spec: StopSpec) {
        let issued = {
            let state = lock(&self.state);
            match state
                .running
                .as_ref()
                .filter(|running| running.turn == turn && !running.settling)
            {
                Some(running) => {
                    let requested_at = rfc3339(std::time::SystemTime::now());
                    running
                        .stop
                        .attach(spec.order(requested_at, now, Some(running.wall)));
                    true
                }
                None => false,
            }
        };
        if issued {
            self.wake();
        }
    }

    /// Publishes the run loop's acknowledgement of the turn's order.
    pub(super) fn acknowledge(&self, turn: TurnNumber, ack: Ack) {
        let state = lock(&self.state);
        if let Some(running) = state
            .running
            .as_ref()
            .filter(|running| running.turn == turn)
        {
            running.stop.ack.send_replace(Some(ack));
        }
    }

    /// Marks the running turn `settling` once `execute` returned, and returns
    /// its order, if any [r1.4].
    pub(super) fn settle(&self, turn: TurnNumber) -> Option<StopOrder> {
        let mut state = lock(&self.state);
        let running = state
            .running
            .as_mut()
            .filter(|running| running.turn == turn)?;
        running.settling = true;
        running.stop.order.borrow().clone()
    }

    /// The run loop is done with the turn: its stop channels drop, which
    /// waiters observe. Wakes: the drop.
    pub(super) fn finish_running(&self, turn: TurnNumber) {
        {
            let mut state = lock(&self.state);
            if state
                .running
                .as_ref()
                .is_some_and(|running| running.turn == turn)
            {
                state.running = None;
            }
        }
        self.wake();
    }

    /// Whether the session has a queue entry (`Waiting`, `Claimed` or
    /// `Cancelling`) or a running or settling turn, read under one slot
    /// state lock, so no queue-to-running move falls between two reads
    /// (the force set, design §6.3 [O3]).
    pub(super) fn unfinished(&self) -> bool {
        let state = lock(&self.state);
        !state.queue.is_empty() || state.running.is_some()
    }

    /// The session's running turn, if any.
    pub(super) fn running_turn(&self) -> Option<TurnNumber> {
        lock(&self.state)
            .running
            .as_ref()
            .map(|running| running.turn)
    }

    /// A `cancel`'s step for `turn` under slot state (design §3.1, §3.3):
    /// takes a `Waiting` turn, attaches `spec`'s order to a claimed or running
    /// one, and joins any other cancellation. Wakes: a claim change or an
    /// order attached.
    pub(super) fn cancel_step(
        &self,
        turn: TurnNumber,
        spec: StopSpec,
        now: tokio::time::Instant,
    ) -> CancelStep {
        let step = {
            let mut state = lock(&self.state);
            if let Some(running) = state
                .running
                .as_ref()
                .filter(|running| running.turn == turn)
            {
                if running.settling {
                    CancelStep::Settling(running.stop.ack.subscribe())
                } else {
                    let requested_at = rfc3339(std::time::SystemTime::now());
                    running
                        .stop
                        .attach(spec.order(requested_at, now, Some(running.wall)));
                    CancelStep::Ordered(running.stop.ack.subscribe())
                }
            } else {
                match state.entry(turn) {
                    None => CancelStep::Absent,
                    Some(entry) => match entry.claim {
                        Claim::Waiting => {
                            entry.claim = Claim::Cancelling(Owner::Request);
                            entry.outcome = watch::Sender::new(None);
                            CancelStep::Queued
                        }
                        Claim::Claimed => match &entry.stop {
                            Some(stop) => {
                                let requested_at = rfc3339(std::time::SystemTime::now());
                                stop.attach(spec.order(requested_at, now, None));
                                CancelStep::Ordered(stop.ack.subscribe())
                            }
                            None => CancelStep::Absent,
                        },
                        Claim::Cancelling(_) => CancelStep::Joined(entry.outcome.subscribe()),
                    },
                }
            }
        };
        if !matches!(step, CancelStep::Absent | CancelStep::Joined(_)) {
            self.wake();
        }
        step
    }

    /// Ends a cancellation that did not commit (design §3.2): a request's
    /// read failure or not-committed commit rolls back to `Waiting`; an
    /// uncertain one stays `Cancelling`, as does any dispatcher-owned one.
    /// Publishes `outcome` to every joined caller. Wakes: a claim change.
    pub(super) fn cancel_failed(&self, turn: TurnNumber, outcome: QueuedOutcome) {
        {
            let mut state = lock(&self.state);
            if let Some(entry) = state.entry(turn) {
                entry.outcome.send_replace(Some(outcome));
                let rollback = entry.claim == Claim::Cancelling(Owner::Request)
                    && outcome != QueuedOutcome::Uncertain;
                if rollback {
                    entry.claim = Claim::Waiting;
                    entry.outcome = watch::Sender::new(None);
                }
            }
        }
        self.wake();
    }

    /// A dispatcher-owned cancellation's read failed (design §7.3
    /// [r1.13]): every caller joined so far gets a plain `store_error`. The
    /// claim is kept, and a caller joining later waits for the retry. The
    /// slot mutex alone. No wake: the claim is unchanged.
    pub(super) fn read_failed(&self, turn: TurnNumber) {
        let mut state = lock(&self.state);
        if let Some(entry) = state.entry(turn)
            && entry.claim == Claim::Cancelling(Owner::Dispatcher)
        {
            entry.outcome.send_replace(Some(QueuedOutcome::ReadFailed));
            entry.outcome = watch::Sender::new(None);
        }
    }

    /// Takes the dispatcher's next cancellation for the close pass or force
    /// (design §3.1, §4 step 1): the first `Waiting` turn becomes
    /// `Cancelling{dispatcher}` with `cause`; one already dispatcher-owned is
    /// retried. Request-owned cancellations are waited for. Wakes: a claim
    /// change.
    pub(super) fn sweep(&self, cause: Option<&(CancelCause, String)>) -> Sweep {
        let sweep = {
            let mut state = lock(&self.state);
            let mut waiting = false;
            let mut next = Sweep::Done;
            for entry in &mut state.queue {
                match entry.claim {
                    Claim::Waiting => {
                        entry.claim = Claim::Cancelling(Owner::Dispatcher);
                        entry.cause = cause.cloned();
                        entry.outcome = watch::Sender::new(None);
                        next = Sweep::Cancel(entry.turn, entry.cause.clone());
                        break;
                    }
                    Claim::Cancelling(Owner::Dispatcher) => {
                        next = Sweep::Cancel(entry.turn, entry.cause.clone());
                        break;
                    }
                    Claim::Cancelling(Owner::Request) | Claim::Claimed => waiting = true,
                }
            }
            if waiting && matches!(next, Sweep::Done) {
                Sweep::Wait
            } else {
                next
            }
        };
        if matches!(sweep, Sweep::Cancel(..)) {
            self.wake();
        }
        sweep
    }

    /// Whether the dispatcher owns a cancellation of `turn`
    /// (`Cancelling{dispatcher}`): its read streak resolves it (design §7.3).
    pub(super) fn dispatcher_cancelling(&self, turn: TurnNumber) -> bool {
        lock(&self.state)
            .entry(turn)
            .is_some_and(|entry| entry.claim == Claim::Cancelling(Owner::Dispatcher))
    }

    /// Makes a `Waiting` head dispatcher-owned for a P6 cancellation.
    pub(super) fn own(&self, turn: TurnNumber) -> bool {
        let mut state = lock(&self.state);
        match state.entry(turn) {
            Some(entry) if entry.claim == Claim::Waiting => {
                entry.claim = Claim::Cancelling(Owner::Dispatcher);
                entry.outcome = watch::Sender::new(None);
                true
            }
            Some(entry) => entry.claim == Claim::Cancelling(Owner::Dispatcher),
            None => false,
        }
    }

    /// Test-only: callers waiting on `turn`'s stop acknowledgement while it
    /// is claimed, and on its current cancellation's outcome.
    #[cfg(test)]
    pub(super) fn watchers(&self, turn: TurnNumber) -> (usize, usize) {
        lock(&self.state).entry(turn).map_or((0, 0), |entry| {
            (
                entry
                    .stop
                    .as_ref()
                    .map_or(0, |stop| stop.ack.receiver_count()),
                entry.outcome.receiver_count(),
            )
        })
    }

    /// The cause of a dispatcher-owned cancellation.
    pub(super) fn cause(&self, turn: TurnNumber) -> Option<(CancelCause, String)> {
        lock(&self.state)
            .entry(turn)
            .and_then(|entry| entry.cause.clone())
    }

    /// Every queued turn, oldest first.
    pub(super) fn queued(&self) -> Vec<TurnNumber> {
        lock(&self.state)
            .queue
            .iter()
            .map(|entry| entry.turn)
            .collect()
    }

    /// Removes a turn that left the queue durably terminal (cancelled, or
    /// failed without agent I/O, design §7.2 row 2), publishing `committed`
    /// to its joined callers. Wakes: the pop.
    pub(super) fn pop(&self, turn: TurnNumber) {
        {
            let mut state = lock(&self.state);
            if let Some(at) = state.queue.iter().position(|entry| entry.turn == turn)
                && let Some(entry) = state.queue.remove(at)
            {
                entry.outcome.send_replace(Some(QueuedOutcome::Committed));
            }
        }
        self.wake();
    }

    /// Sets the close order (design §4 step 7) and attaches a `close` stop
    /// order to a claimed or running turn [r1.1]. Returns the attempt's
    /// watch and whether a dispatcher must be started (amendment A2).
    /// Wakes: a close order set.
    pub(super) fn set_close(
        &self,
        order: CloseOrder,
        now: tokio::time::Instant,
    ) -> (CloseWatch, bool) {
        let result = {
            let mut state = lock(&self.state);
            let spec = order.spec();
            let requested_at = order.requested_at.clone();
            if let Some(running) = state.running.as_ref().filter(|running| !running.settling) {
                running
                    .stop
                    .attach(spec.order(requested_at.clone(), now, Some(running.wall)));
            }
            for entry in &state.queue {
                if let (Claim::Claimed, Some(stop)) = (entry.claim, &entry.stop) {
                    stop.attach(spec.order(requested_at.clone(), now, None));
                }
            }
            let watch = order.watch.clone();
            state.close = Some(order);
            (watch, state.start())
        };
        self.wake();
        result
    }

    /// The close attempt in progress, if any: its watch (design §4 steps 2
    /// and 5).
    pub(super) fn close_watch(&self) -> Option<CloseWatch> {
        lock(&self.state)
            .close
            .as_ref()
            .map(|close| close.watch.clone())
    }

    /// A second close with `mode: force` escalates the attempt to force now
    /// (design §4 step 5). Wakes: an order attached.
    pub(super) fn escalate_close(&self, now: tokio::time::Instant) {
        {
            let mut state = lock(&self.state);
            let Some(close) = state.close.as_mut() else {
                return;
            };
            close.mode = CloseMode::Force;
            let spec = close.spec();
            let requested_at = close.requested_at.clone();
            if let Some(running) = state.running.as_ref().filter(|running| !running.settling) {
                running
                    .stop
                    .attach(spec.order(requested_at.clone(), now, Some(running.wall)));
            }
            for entry in &state.queue {
                if let (Claim::Claimed, Some(stop)) = (entry.claim, &entry.stop) {
                    stop.attach(spec.order(requested_at.clone(), now, None));
                }
            }
        }
        self.wake();
    }

    /// The close pass's copy of the close order.
    pub(super) fn close_task(&self) -> Option<CloseTask> {
        lock(&self.state).close.as_ref().map(|close| CloseTask {
            deadline: close.deadline,
            requested_at: close.requested_at.clone(),
            operation: close.operation.clone(),
        })
    }

    /// Publishes the close attempt's reply and clears the order, under slot
    /// state (design §1 close watch): a later caller finds no attempt in
    /// progress. Wakes: the order cleared.
    pub(super) fn finish_close(&self, reply: CloseReply) {
        {
            let mut state = lock(&self.state);
            if let Some(close) = state.close.take() {
                close.watch.send_replace(Some(reply));
            }
        }
        self.wake();
    }

    /// Marks the dispatcher gone if nothing is queued, running or closing;
    /// the caller holds `admission`. Returns whether it exited.
    pub(super) fn exit(&self) -> bool {
        let mut state = lock(&self.state);
        if !state.queue.is_empty() || state.running.is_some() || state.close.is_some() {
            return false;
        }
        state.dispatcher = Dispatcher::None;
        true
    }

    /// No queued turn, close order or dispatcher: the slot may be retired.
    pub(super) fn idle(&self) -> bool {
        let state = lock(&self.state);
        state.queue.is_empty() && state.close.is_none() && state.dispatcher == Dispatcher::None
    }

    /// Marks the dispatcher gone after a force stop or a Store failure,
    /// whatever it leaves queued: nothing restarts it, since receipts are refused.
    pub(super) fn stop(&self) {
        lock(&self.state).dispatcher = Dispatcher::None;
    }

    /// A dispatcher was requested but has not run.
    pub(super) fn starting(&self) -> bool {
        lock(&self.state).dispatcher == Dispatcher::Starting
    }

    /// Whether only the slot itself holds its head: no writer lease is out.
    pub(super) fn unleased(&self) -> bool {
        Arc::strong_count(&self.head) == 1
    }
}

/// Exponential retry delay for Store reads: 250 ms doubling to 5 s, reset
/// by a successful step.
pub(super) struct Backoff(Duration);

impl Backoff {
    pub(super) fn new() -> Self {
        Self(RETRY_MIN)
    }

    /// The delay to wait now; the next one doubles up to the cap.
    pub(super) fn next(&mut self) -> Duration {
        let delay = self.0;
        self.0 = (self.0 * 2).min(RETRY_MAX);
        delay
    }

    pub(super) fn reset(&mut self) {
        self.0 = RETRY_MIN;
    }
}
