//! The C2 driver lane (C2 §2, adapter design §3.2, AD3, AD16): a logical
//! session driver whose `run_turn` is the one data lane per submitted turn,
//! with `steer`, `close` and `health` serviceable meanwhile.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::value::RawValue;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::fake::FakeAdapter;
use crate::observation::{AdapterError, ObservationSink, SteerDelivery, TurnEnd, TurnEvidence};
use crate::plan::{Bound, Inherit, VendorOptions};
use crate::{
    CapacityToken, Cleanup, Deadline, DriverFailure, DriverHealth, SessionId, StopCause, StopOrder,
    StopWatch, TurnActivity, TurnNumber, VendorTurnId,
};
use via_routes::{FakeRoute, Retirement, SteerRefused, SteerSender, WireCleanup};

/// The daemon force: `None` until raised, then the instant it was raised.
pub type ForceWatch = watch::Receiver<Option<tokio::time::Instant>>;

/// A session's context, attached at `open_session` or `recover` (C2 §2).
pub struct SessionCx {
    /// The session's observation channel: session-level and turn items.
    pub observations: ObservationSink,
    /// Owns every task the driver starts: a turn's route work and its
    /// process retirement, and the session's idle source.
    pub tracker: TaskTracker,
    /// The session's cancellation: it stops the driver's owned work.
    pub cancel: CancellationToken,
}

/// What `open_session` needs of a session (C2 §2 `SessionSpec`).
#[derive(Clone, Debug)]
pub struct SessionSpec {
    /// The VIA session.
    pub session_id: SessionId,
    /// The resolved model.
    pub model: String,
    /// Session instructions.
    pub instructions: Option<String>,
    /// The bound frozen at spawn.
    pub initial_bound: Option<Bound>,
    /// The session's frozen working directory.
    pub cwd: PathBuf,
    /// Vendor options.
    pub vendor: VendorOptions,
    /// Effective inherited-configuration states (AD13).
    pub inherit: Inherit,
    /// The last confirmed vendor session ID; not verification of a new
    /// connection.
    pub confirmed_vendor_session_id: Option<String>,
    /// Stored for C1 compatibility; no effect.
    pub allow_untested: bool,
}

/// One submitted turn's values (C2 §2 `TurnSpec`).
#[derive(Debug, Default)]
pub struct TurnSpec {
    /// The prompt.
    pub prompt: String,
    /// The turn's effort.
    pub effort: Option<String>,
    /// The turn's bound.
    pub bound: Option<Bound>,
    /// The structured-output schema.
    pub output_schema: Option<Box<RawValue>>,
    /// The step limit.
    pub max_steps: Option<u64>,
    /// Vendor options.
    pub vendor: VendorOptions,
}

/// A live connection pinned against idle retirement for one turn (AD16).
#[derive(Debug)]
pub struct ConnectionPin {
    generation: u64,
}

/// `prepare`'s answer (AD16).
#[derive(Debug)]
pub enum Prepared {
    /// A live connection is pinned: the turn needs no slot.
    Pinned(ConnectionPin),
    /// The turn opens a connection: Core reserves a slot for it.
    NeedsConnection,
}

/// The per-turn context (C2 §2 `TurnCx`).
pub struct TurnCx {
    /// The canonical turn.
    pub turn: TurnNumber,
    /// What `prepare` answered at dispatch.
    pub prepared: Prepared,
    /// The connection slot Core reserved, for `NeedsConnection`.
    pub capacity: Option<CapacityToken>,
    /// The turn's activity clock.
    pub activity: TurnActivity,
    /// The absolute wall deadline.
    pub wall: Deadline,
    /// C1 P7's tool-grace window (60 s).
    pub tool_grace: Duration,
    /// The turn's stop order.
    pub stop: StopWatch,
    /// The daemon force.
    pub force: ForceWatch,
}

/// Steer input for the active turn (C2 §2 `SteerInput`).
#[derive(Debug)]
pub struct SteerInput {
    /// The text.
    pub text: String,
    /// The vendor turn the caller means; another running turn refuses it.
    pub expected_vendor_turn: Option<VendorTurnId>,
}

/// Why steer input was not delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteerError {
    /// The route does not support steer.
    Unsupported,
    /// No turn is accepted and running.
    NoActiveTurn,
    /// The input names another vendor turn than the running one.
    TurnMismatch,
    /// The control lane's eight commands or 64 KiB are taken (C2 §2).
    OverCapacity,
    /// The input was not written whole.
    NotDelivered,
}

/// How a session closes (C2 §2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseMode {
    /// Ends the vendor session politely.
    Graceful,
    /// Stops the session's private group.
    Force,
}

/// A session close's passive report (C2 §2 `CloseReport`).
#[derive(Debug)]
pub struct CloseReport {
    /// The vendor reported that it closed the session.
    pub vendor_closed: bool,
    /// The connection's process exit, when this close stopped one.
    pub process_exit: Option<via_routes::ExitReport>,
    /// Cleanup certainty.
    pub cleanup: Cleanup,
    /// Warnings.
    pub warnings: Vec<crate::Warning>,
    /// Only when this close stopped the server (AD20).
    pub leftovers: Option<crate::observation::LeftoverReport>,
}

/// `recover`'s answer (C2 §2): never a resubmission.
pub enum Recovery {
    /// The session's live connection was rejoined.
    Resumed(Box<SessionDriver>),
    /// Whether a process survives is not established.
    Unknown {
        /// Why.
        reason: String,
    },
    /// Host confirmed every process of the session is gone.
    Dead {
        /// The evidence.
        evidence: String,
    },
}

/// The driver's mutable state; never locked across an await.
#[derive(Default)]
pub(crate) struct DriverState {
    /// The persistent profile's committed slot, held between turns
    /// (decision H1).
    pub(crate) capacity: Option<CapacityToken>,
    /// The persistent connection is live and pinnable.
    pub(crate) live: bool,
    /// The connection generation; each new connection advances it.
    pub(crate) generation: u64,
    /// The running turn, until its logical end.
    pub(crate) active: Option<Active>,
    /// Set once the last turn's process was retired.
    pub(crate) retiring: Option<watch::Receiver<bool>>,
    /// The last turn's retirement facts.
    pub(crate) retirement: Option<Retirement>,
    /// The session's confirmed vendor session ID, which every later
    /// identity must match (C2 §2 Reopen).
    pub(crate) identity: Option<String>,
    /// The vendor reported that it closed the session.
    pub(crate) vendor_closed: bool,
    /// The session was closed.
    pub(crate) closed: bool,
}

/// The running turn's driver-side lanes.
pub(crate) struct Active {
    /// The turn.
    pub(crate) turn: TurnNumber,
    /// Its steer lane.
    pub(crate) steer: SteerSender,
    /// Its driver-side stop order: a session close.
    pub(crate) close: watch::Sender<Option<StopOrder>>,
}

/// Locks the driver's state. No code panics while holding the lock; a
/// poisoned state is still the last consistent one.
pub(crate) fn lock(state: &Mutex<DriverState>) -> MutexGuard<'_, DriverState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Latches the first failure (C2 §2 sticky health); later failures and a
/// closed driver keep what they find.
pub(crate) fn latch(health: &watch::Sender<DriverHealth>, cause: DriverFailure) {
    health.send_if_modified(|health| {
        if matches!(health, DriverHealth::Open) {
            *health = DriverHealth::Failed { first_cause: cause };
            true
        } else {
            false
        }
    });
}

/// One session's driver (C2 §2).
pub struct SessionDriver {
    pub(crate) route: Arc<FakeRoute>,
    /// `None` when no adapter serves the session's harness.
    pub(crate) adapter: Option<Arc<FakeAdapter>>,
    pub(crate) spec: SessionSpec,
    pub(crate) observations: ObservationSink,
    pub(crate) tracker: TaskTracker,
    /// The session's cancellation, also cancelled by this driver's close.
    pub(crate) cancel: CancellationToken,
    pub(crate) health: Arc<watch::Sender<DriverHealth>>,
    pub(crate) state: Arc<Mutex<DriverState>>,
}

impl SessionDriver {
    pub(crate) fn new(
        route: Arc<FakeRoute>,
        adapter: Option<Arc<FakeAdapter>>,
        spec: SessionSpec,
        cx: SessionCx,
    ) -> Self {
        let state = DriverState {
            identity: spec.confirmed_vendor_session_id.clone(),
            ..DriverState::default()
        };
        Self {
            route,
            adapter,
            spec,
            observations: cx.observations,
            tracker: cx.tracker,
            cancel: cx.cancel.child_token(),
            health: Arc::new(watch::Sender::new(DriverHealth::Open)),
            state: Arc::new(Mutex::new(state)),
        }
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, DriverState> {
        lock(&self.state)
    }

    fn persistent(&self) -> bool {
        self.adapter
            .as_ref()
            .is_some_and(|adapter| adapter.profile().persistent)
    }

    /// The ID identity confirmations name for the driver's current
    /// connection generation, the latest it opened (C2 §2 delayed identity:
    /// Core checks the current generation); `None` before the first.
    pub fn connection_id(&self) -> Option<String> {
        let generation = self.state().generation;
        (generation > 0).then(|| crate::fake::connection_id(generation))
    }

    /// AD16: a live persistent connection is pinned; otherwise the turn
    /// opens a new connection and Core reserves a slot for it.
    pub fn prepare(&self) -> Prepared {
        let state = self.state();
        if self.persistent() && state.live && !state.closed {
            Prepared::Pinned(ConnectionPin {
                generation: state.generation,
            })
        } else {
            Prepared::NeedsConnection
        }
    }

    /// Runs one submitted turn to its one result (C2 §4.1).
    pub async fn run_turn(&self, spec: TurnSpec, cx: TurnCx) -> TurnEnd {
        let Some(adapter) = self.adapter.clone() else {
            return rejected(AdapterError::Unavailable);
        };
        crate::fake::run_turn(self, &adapter, spec, cx).await
    }

    /// Takes the turn's connection: a pin must name the live generation
    /// (AD16 rule 4), and a new connection advances the generation and
    /// replaces any earlier one. On the persistent profile the slot stays
    /// with the turn's [`Reservation`] until its handshake succeeded;
    /// otherwise it goes to Host with the process, and is returned.
    pub(crate) fn connect(
        &self,
        prepared: Prepared,
        capacity: Option<CapacityToken>,
    ) -> Result<(u64, Option<CapacityToken>, Reservation), AdapterError> {
        let persistent = self.persistent();
        let mut state = self.state();
        if state.closed {
            return Err(session_gone());
        }
        let reservation = |generation, slot| Reservation {
            state: Arc::clone(&self.state),
            generation,
            slot,
            persistent,
            committed: false,
        };
        match prepared {
            Prepared::Pinned(pin)
                if persistent && state.live && pin.generation == state.generation =>
            {
                Ok((state.generation, None, reservation(state.generation, None)))
            }
            // The pinned connection died before submission: nothing sent.
            Prepared::Pinned(_) => Err(session_gone()),
            Prepared::NeedsConnection => {
                state.generation += 1;
                state.live = false;
                state.vendor_closed = false;
                let generation = state.generation;
                // A new connection replaces the earlier one, whose slot goes.
                let replaced = state.capacity.take();
                drop(state);
                drop(replaced);
                if persistent {
                    Ok((generation, None, reservation(generation, capacity)))
                } else {
                    Ok((generation, capacity, reservation(generation, None)))
                }
            }
        }
    }

    /// The connection of `generation` is gone: its slot is released and
    /// nothing pins it.
    pub(crate) fn disconnect(&self, generation: u64) {
        let released = {
            let mut state = self.state();
            if state.generation != generation {
                return;
            }
            state.live = false;
            state.capacity.take()
        };
        drop(released);
    }

    /// Latches the first failure (C2 §2 sticky health).
    pub(crate) fn fail(&self, cause: DriverFailure) {
        latch(&self.health, cause);
    }

    /// Delivers steer input into the running turn (C2 §2): admitted at
    /// once or refused; delivered once written and reported by the vendor.
    pub async fn steer(&self, input: SteerInput) -> Result<SteerDelivery, SteerError> {
        let delivery = match self
            .adapter
            .as_ref()
            .map(|adapter| &adapter.profile().capabilities.verbs.steer)
        {
            Some(crate::Support::Native) => SteerDelivery::Injected,
            Some(crate::Support::Partial { semantics }) => {
                SteerDelivery::Partial(Cow::Owned(semantics.clone()))
            }
            Some(crate::Support::Unsupported { .. }) | None => return Err(SteerError::Unsupported),
        };
        let lane = self
            .state()
            .active
            .as_ref()
            .map(|active| active.steer.clone());
        let Some(lane) = lane else {
            return Err(SteerError::NoActiveTurn);
        };
        let expected = input
            .expected_vendor_turn
            .map(|turn| turn.as_str().to_owned());
        let answer = lane.send(input.text, expected).map_err(steer_error)?;
        match answer.await {
            Ok(Ok(())) => Ok(delivery),
            Ok(Err(refused)) => Err(steer_error(refused)),
            // The turn ended first.
            Err(_) => Err(SteerError::NoActiveTurn),
        }
    }

    /// Closes the session (C2 §2). A running turn is stopped through its
    /// own stop path: the soft stop, then the force rule at the midpoint
    /// of `deadline` (at once for `Force`), under `deadline`. The report
    /// waits, within `deadline`, for the last process's retirement and
    /// carries only what was established: the vendor's own close, the exit
    /// this close caused and the retirement's cleanup. The persistent
    /// profile's slot is released once that retirement ended: here when it
    /// did by `deadline`, else by the retirement itself. Nothing happens
    /// before the first poll: a close dropped unpolled changes nothing.
    pub fn close(
        &self,
        mode: CloseMode,
        deadline: Deadline,
    ) -> impl Future<Output = CloseReport> + Send + use<> {
        let state = Arc::clone(&self.state);
        let health = Arc::clone(&self.health);
        let cancel = self.cancel.clone();
        async move {
            // The session closes and the stop order is posted together.
            let (stop, retiring) = {
                let mut state = lock(&state);
                state.closed = true;
                let stop = state.active.take().map(|active| active.close);
                (stop, state.retiring.clone())
            };
            let stopped = stop.is_some();
            if let Some(stop) = stop {
                let now = tokio::time::Instant::now();
                let force_at = match mode {
                    CloseMode::Graceful => {
                        now + deadline.instant().saturating_duration_since(now) / 2
                    }
                    CloseMode::Force => now,
                };
                stop.send_replace(Some(StopOrder {
                    cause: StopCause::Close,
                    // Route acts only on the times; Core never sees this order.
                    requested_at: String::new(),
                    force_at: Deadline::at(force_at),
                    close_by: deadline,
                }));
            }
            let settled = match retiring {
                Some(mut retiring) => matches!(
                    tokio::time::timeout_at(deadline.instant(), retiring.wait_for(|done| *done))
                        .await,
                    Ok(Ok(_))
                ),
                // No turn ever ran: nothing to clean up.
                None => true,
            };
            // The driver's own idle work ends with the session.
            cancel.cancel();
            let (released, retirement, vendor_closed) = {
                let mut state = lock(&state);
                state.live = false;
                // A retirement still running keeps the slot until it ends
                // (`Reservation::retired`).
                let released = if settled { state.capacity.take() } else { None };
                (released, state.retirement, state.vendor_closed)
            };
            drop(released);
            health.send_replace(DriverHealth::Closed);
            let quiescent = match retirement {
                Some(retirement) => {
                    !retirement.launched || retirement.cleanup == Some(WireCleanup::Quiescent)
                }
                None => true,
            };
            CloseReport {
                vendor_closed,
                process_exit: retirement
                    .filter(|_| stopped && settled)
                    .and_then(|retirement| retirement.exit),
                cleanup: if settled && quiescent {
                    Cleanup::Quiescent
                } else {
                    Cleanup::Uncertain
                },
                warnings: Vec::new(),
                leftovers: None,
            }
        }
    }

    /// The sticky health lane.
    pub fn health(&self) -> watch::Receiver<DriverHealth> {
        self.health.subscribe()
    }
}

/// Route's refusal as the driver reports it.
fn steer_error(refused: SteerRefused) -> SteerError {
    match refused {
        SteerRefused::NotActive => SteerError::NoActiveTurn,
        SteerRefused::NotWritten => SteerError::NotDelivered,
        SteerRefused::TurnMismatch => SteerError::TurnMismatch,
        SteerRefused::OverCapacity => SteerError::OverCapacity,
    }
}

/// One turn's hold on its connection (AD16), shared by the turn's task and
/// `run_turn`: the persistent profile's slot and pin are committed only
/// once the logical turn kept its server and the Adapter delivered all of
/// it. Its last owner drops it after the process's retirement and the end
/// of `run_turn`, whichever is later; dropped uncommitted, on any failure,
/// a dropped `run_turn` or an unwind, it invalidates the generation and
/// releases the slot then.
pub(crate) struct Reservation {
    state: Arc<Mutex<DriverState>>,
    generation: u64,
    slot: Option<CapacityToken>,
    persistent: bool,
    committed: bool,
}

impl Reservation {
    /// The logical turn kept its server: the slot and the pin are the
    /// session's, unless it closed or was cancelled meanwhile.
    pub(crate) fn commit(&mut self, cancelled: bool) {
        if !self.persistent || cancelled {
            return;
        }
        let mut state = lock(&self.state);
        if state.closed || state.generation != self.generation {
            return;
        }
        if let Some(slot) = self.slot.take() {
            state.capacity = Some(slot);
        }
        state.live = true;
        self.committed = true;
    }

    /// The logical turn ended its connection, or the Adapter could not
    /// deliver it: the generation is invalid now; its slot, the session's
    /// committed one included, goes with the reservation, after the
    /// process's retirement.
    pub(crate) fn release(&mut self) {
        self.invalidate();
    }

    /// Invalidates the generation and takes over its committed slot.
    fn invalidate(&mut self) {
        let committed = {
            let mut state = lock(&self.state);
            if state.generation == self.generation {
                state.live = false;
                state.capacity.take()
            } else {
                None
            }
        };
        if committed.is_some() {
            self.slot = committed;
        }
    }

    /// Records the turn's process retirement for a later close. A session
    /// closed meanwhile releases its committed slot now, the retirement
    /// done (C2 §2 Close).
    pub(crate) fn retired(&self, retirement: Retirement) {
        let released = {
            let mut state = lock(&self.state);
            state.retirement = Some(retirement);
            if state.closed && state.generation == self.generation {
                state.capacity.take()
            } else {
                None
            }
        };
        drop(released);
    }
}

impl Drop for Reservation {
    /// Uncommitted, the generation is invalidated; the slot it holds is
    /// released with the reservation's fields.
    fn drop(&mut self) {
        if !self.committed {
            self.invalidate();
        }
    }
}

/// The session or its pinned connection is gone: nothing launched.
fn session_gone() -> AdapterError {
    AdapterError::Rejected {
        reason: crate::StartRejected::SessionGone,
        evidence: TurnEvidence::no_launch(false),
    }
}

/// A turn rejected before anything ran.
pub(crate) fn rejected(error: AdapterError) -> TurnEnd {
    TurnEnd {
        terminal: None,
        instance: None,
        leftovers: None,
        outcome: Err(error),
    }
}
