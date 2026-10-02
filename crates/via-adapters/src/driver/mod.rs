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

use crate::claude::ClaudeAdapter;
use crate::codex::CodexAdapter;
use crate::fake::FakeAdapter;
use crate::observation::{
    AdapterError, ObservationSink, SteerDelivery, SteerToken, TurnEnd, TurnEvidence,
};
use crate::plan::{Bound, InheritPlan, VendorOptions};
use crate::{
    CapacityToken, Cleanup, Deadline, DriverFailure, DriverHealth, SessionId, StopCause, StopOrder,
    StopWatch, Support, TurnActivity, TurnNumber, VendorTurnId,
};
use via_routes::{Retirement, RouteRuntime, SteerSender, WireCleanup};

mod reservation;
mod steer;
pub(crate) mod turn;

pub(crate) use reservation::Reservation;
use steer::steer_error;
pub(crate) use steer::{SteerEmissions, SteerTurn};

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
    /// The inherited-configuration settings requested at spawn and their
    /// effective states, both frozen (C2 §6.2): the launch recipe applies
    /// the requested settings.
    pub inherit: InheritPlan,
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
    /// The canonical turn the caller selected; the driver refuses the
    /// input unless it is running that turn (C2 §2).
    pub turn: TurnNumber,
    /// Core's token for this input, unique within the session: the
    /// `steer.delivered` observation the driver emits for it carries it
    /// (C2 `SteerInput.token`, critical r2 #2).
    pub token: SteerToken,
    /// The text.
    pub text: String,
    /// The vendor turn the caller means; another running turn refuses it.
    pub expected_vendor_turn: Option<VendorTurnId>,
}

/// Why steer input was not delivered, or not recorded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SteerError {
    /// The route does not support steer.
    Unsupported,
    /// No turn is accepted and running.
    NoActiveTurn,
    /// The input names another vendor turn than the running one.
    TurnMismatch,
    /// The control lane's eight commands or 64 KiB are taken (C2 §2):
    /// nothing was written.
    OverCapacity,
    /// The vendor refused steer in the active turn's current phase
    /// (Codex `activeTurnNotSteerable`): nothing was applied.
    NotSteerable,
    /// Writing the input began, in part or whole, but the vendor never
    /// acknowledged it: whether it was applied is unknown (a write that
    /// failed, or the turn's end first; critical r3 #1, r4 #1).
    NotDelivered,
    /// The vendor took the input whole, as `delivery` says, but its
    /// `steer.delivered` observation could not be emitted (a full
    /// observation queue, or the turn's end, a forced stop included), so no
    /// event records it (critical r2 #3).
    NotRecorded {
        /// How the input reached the vendor.
        delivery: SteerDelivery,
    },
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
    /// The generation a turn holds and may still deliver on
    /// ([`Delivering`]).
    pub(crate) delivering: Option<u64>,
    /// Test builds: the daemon adapter's nth-retirement fault (Sol r3 N10).
    #[cfg(feature = "test-failpoints")]
    pub(crate) retirement_fault: Option<Arc<crate::fake::RetirementFault>>,
}

/// A turn that took the connection and may still deliver (C2 D4): set
/// under the state lock that pinning takes ([`SessionDriver::connect`]),
/// cleared when its `run_turn` returns or is dropped. The persistent idle
/// close never runs meanwhile, so the turn's observations and the idle
/// close's never interleave out of decode order.
pub(crate) struct Delivering(Arc<Mutex<DriverState>>);

impl Drop for Delivering {
    fn drop(&mut self) {
        lock(&self.0).delivering = None;
    }
}

/// The running turn's driver-side lanes.
pub(crate) struct Active {
    /// The turn.
    pub(crate) turn: TurnNumber,
    /// Its steer lane.
    pub(crate) steer: SteerSender,
    /// Its steer callers awaiting their observation's emission.
    pub(crate) emissions: Arc<SteerEmissions>,
    /// Its driver-side stop order: a session close.
    pub(crate) close: watch::Sender<Option<StopOrder>>,
}

impl Active {
    /// The lanes of `turn`, with no steer caller waiting yet.
    pub(crate) fn new(
        turn: TurnNumber,
        steer: SteerSender,
        close: watch::Sender<Option<StopOrder>>,
    ) -> Self {
        Self {
            turn,
            steer,
            close,
            emissions: Arc::default(),
        }
    }
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

/// The adapter a driver runs its turns for (adapter design §6 step 2): the
/// closed set this build compiles in. The harness-neutral driver state
/// stays in [`SessionDriver`]; each arm decides its turn, its connection
/// IDs and its steer support.
pub(crate) enum DriverKind {
    /// The fake test double.
    Fake(Arc<FakeAdapter>),
    /// Claude Code: a stub that runs no turn until via-p98.3.2.
    Claude(#[expect(dead_code, reason = "its turn reads it (via-p98.3.2)")] Arc<ClaudeAdapter>),
    /// Codex: a stub that runs no turn until via-5lr.3.2.
    Codex(#[expect(dead_code, reason = "its turn reads it (via-5lr.3.2)")] Arc<CodexAdapter>),
}

impl DriverKind {
    /// Whether its connection persists between turns (AD16).
    fn persistent(&self) -> bool {
        match self {
            Self::Fake(fake) => fake.profile().persistent,
            Self::Claude(_) | Self::Codex(_) => false,
        }
    }

    /// The adapter version its turns record (AD12); none from a stub.
    fn adapter_version(&self) -> Option<String> {
        match self {
            Self::Fake(fake) => Some(fake.profile().adapter_version.clone()),
            Self::Claude(_) | Self::Codex(_) => None,
        }
    }

    /// Its declared steer support; none from a stub.
    fn steer(&self) -> Option<&Support> {
        match self {
            Self::Fake(fake) => Some(&fake.profile().capabilities.verbs.steer),
            Self::Claude(_) | Self::Codex(_) => None,
        }
    }

    /// The vendor's refusal of every steer admitted into the running turn,
    /// when the adapter declares one: the fake profile's only.
    fn steer_refused(&self, delivery: &SteerDelivery) -> Option<SteerError> {
        match self {
            Self::Fake(fake) => fake
                .profile()
                .steer_refusal
                .map(|refusal| refusal.error(delivery)),
            Self::Claude(_) | Self::Codex(_) => None,
        }
    }

    /// The ID identity confirmations name for connection `generation`.
    fn connection_id(&self, generation: u64) -> Option<String> {
        match self {
            Self::Fake(_) => Some(crate::fake::connection_id(generation)),
            // A stub never connects, so never advances a generation.
            Self::Claude(_) | Self::Codex(_) => None,
        }
    }
}

/// One session's driver (C2 §2).
pub struct SessionDriver {
    /// The Route runtime, which opens every connection.
    pub(crate) runtime: Arc<RouteRuntime>,
    /// `None` when no adapter serves the session's harness.
    pub(crate) kind: Option<DriverKind>,
    pub(crate) spec: SessionSpec,
    pub(crate) observations: ObservationSink,
    pub(crate) tracker: TaskTracker,
    /// The session's cancellation, also cancelled by this driver's close.
    pub(crate) cancel: CancellationToken,
    pub(crate) health: Arc<watch::Sender<DriverHealth>>,
    /// Sticky: a Host journal write no turn reports had an uncertain
    /// outcome ([`Self::journal_uncertain`]).
    pub(crate) journal: Arc<watch::Sender<bool>>,
    pub(crate) state: Arc<Mutex<DriverState>>,
    /// The C2 §4 generation barrier: an idle close holds it from its
    /// generation check until its `VendorClosed` was delivered, and a new
    /// connection takes it to advance the generation. So a new
    /// generation's first observation follows the previous one's last.
    pub(crate) barrier: Arc<tokio::sync::Mutex<()>>,
}

impl SessionDriver {
    pub(crate) fn new(
        runtime: Arc<RouteRuntime>,
        kind: Option<DriverKind>,
        spec: SessionSpec,
        cx: SessionCx,
    ) -> Self {
        let state = DriverState {
            identity: spec.confirmed_vendor_session_id.clone(),
            #[cfg(feature = "test-failpoints")]
            retirement_fault: match &kind {
                Some(DriverKind::Fake(fake)) => fake.retirement_fault.clone(),
                Some(DriverKind::Claude(_) | DriverKind::Codex(_)) | None => None,
            },
            ..DriverState::default()
        };
        Self {
            runtime,
            kind,
            spec,
            observations: cx.observations,
            tracker: cx.tracker,
            cancel: cx.cancel.child_token(),
            health: Arc::new(watch::Sender::new(DriverHealth::Open)),
            journal: Arc::new(watch::Sender::new(false)),
            state: Arc::new(Mutex::new(state)),
            barrier: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, DriverState> {
        lock(&self.state)
    }

    fn persistent(&self) -> bool {
        self.kind.as_ref().is_some_and(DriverKind::persistent)
    }

    /// The running adapter's version (AD12), which each turn the driver
    /// starts records as the session's (C1 §3.3); `None` without one.
    pub fn adapter_version(&self) -> Option<String> {
        self.kind.as_ref().and_then(DriverKind::adapter_version)
    }

    /// The `SessionSpec` the driver was opened with: a test seam, absent
    /// from release builds (critical r2 #11).
    #[cfg(any(test, feature = "test-failpoints"))]
    pub fn spec(&self) -> &SessionSpec {
        &self.spec
    }

    /// Test builds: how many steer callers of the running turn wait for
    /// their observation's emission (critical r2 #1).
    #[cfg(any(test, feature = "test-failpoints"))]
    pub fn steers_waiting(&self) -> usize {
        self.state()
            .active
            .as_ref()
            .map_or(0, |active| active.emissions.len().0)
    }

    /// Test builds: how many of those Route acknowledged, so that each
    /// waits on its observation's emission alone (critical r3 #2).
    #[cfg(any(test, feature = "test-failpoints"))]
    pub fn steers_acknowledged(&self) -> usize {
        self.state()
            .active
            .as_ref()
            .map_or(0, |active| active.emissions.len().1)
    }

    /// The ID identity confirmations name for the driver's current
    /// connection generation, the latest it opened (C2 §2 delayed identity:
    /// Core checks the current generation); `None` before the first.
    pub fn connection_id(&self) -> Option<String> {
        let generation = self.state().generation;
        if generation == 0 {
            return None;
        }
        self.kind
            .as_ref()
            .and_then(|kind| kind.connection_id(generation))
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
        match &self.kind {
            Some(DriverKind::Fake(fake)) => {
                let fake = Arc::clone(fake);
                crate::fake::run_turn(self, &fake, spec, cx).await
            }
            // The vendor stubs run no turn yet (via-p98.3.2, via-5lr.3.2).
            Some(DriverKind::Claude(_) | DriverKind::Codex(_)) | None => {
                rejected(AdapterError::Unavailable)
            }
        }
    }

    /// Takes the turn's connection: a pin must name the live generation
    /// (AD16 rule 4), and a new connection advances the generation and
    /// replaces any earlier one. On the persistent profile the slot stays
    /// with the turn's [`Reservation`] until its handshake succeeded;
    /// otherwise it goes to Host with the process, and is returned. A new
    /// connection first waits for an older generation's idle close in
    /// flight (C2 §4 generation barrier) unless `ordered`, the turn's stop,
    /// force or wall, resolves first: a turn so ordered launches nothing, as
    /// before any launch (Route's entry check), so it offers no observation
    /// for the barrier to order.
    pub(crate) async fn connect(
        &self,
        (prepared, capacity): (Prepared, Option<CapacityToken>),
        ordered: impl Future<Output = ()>,
    ) -> Result<(u64, Option<CapacityToken>, Reservation, Delivering), AdapterError> {
        let _barrier = match prepared {
            Prepared::NeedsConnection => {
                // Test builds: `adapter.connection.barrier_wait` acknowledges
                // a new connection that finds the barrier held.
                #[cfg(feature = "test-failpoints")]
                if self.barrier.try_lock().is_err() {
                    let _ =
                        via_routes::failpoint::hit_async("adapter.connection.barrier_wait").await;
                }
                tokio::select! {
                    barrier = self.barrier.lock() => Some(barrier),
                    () = ordered => None,
                }
            }
            Prepared::Pinned(_) => None,
        };
        let persistent = self.persistent();
        let mut state = self.state();
        if state.closed {
            return Err(session_gone());
        }
        let reservation = |generation, slot| {
            Reservation::new(Arc::clone(&self.state), generation, slot, persistent)
        };
        let delivering = || Delivering(Arc::clone(&self.state));
        match prepared {
            Prepared::Pinned(pin)
                if persistent && state.live && pin.generation == state.generation =>
            {
                state.delivering = Some(state.generation);
                Ok((
                    state.generation,
                    None,
                    reservation(state.generation, None),
                    delivering(),
                ))
            }
            // The pinned connection died before submission: nothing sent.
            Prepared::Pinned(_) => Err(session_gone()),
            Prepared::NeedsConnection => {
                state.generation += 1;
                state.live = false;
                state.vendor_closed = false;
                state.delivering = Some(state.generation);
                let generation = state.generation;
                // A new connection replaces the earlier one, whose slot goes.
                let replaced = state.capacity.take();
                drop(state);
                drop(replaced);
                if persistent {
                    Ok((
                        generation,
                        None,
                        reservation(generation, capacity),
                        delivering(),
                    ))
                } else {
                    Ok((
                        generation,
                        capacity,
                        reservation(generation, None),
                        delivering(),
                    ))
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
    /// Admission checks `input.turn` against the turn the driver runs under
    /// the state lock it enqueues under: another turn is `TurnMismatch`,
    /// none `NoActiveTurn`, so the input never reaches a successor. The
    /// `steer.delivered` observation carrying `input.token` is on the
    /// session channel before this returns `Ok` (C2 `SteerInput.token`). A
    /// delivery the vendor took whose observation the turn could not put
    /// there (a full queue, or the turn's end first, a forced stop
    /// included) is `NotRecorded`. The turn's end, by any path, always
    /// answers (critical r3 #1): before Route's answer, an input the
    /// vendor acknowledged is decided by its report's emission, as above
    /// (critical r5 #1); otherwise one Route never took is `NoActiveTurn`
    /// and one it started writing `NotDelivered`, since the vendor may
    /// have it.
    pub async fn steer(&self, input: SteerInput) -> Result<SteerDelivery, SteerError> {
        let delivery = match self.kind.as_ref().and_then(DriverKind::steer) {
            Some(Support::Native) => SteerDelivery::Injected,
            Some(Support::Partial { semantics }) => {
                SteerDelivery::Partial(Cow::Owned(semantics.clone()))
            }
            Some(Support::Unsupported { .. }) | None => return Err(SteerError::Unsupported),
        };
        let expected = input
            .expected_vendor_turn
            .map(|turn| turn.as_str().to_owned());
        let token = input.token.get();
        // Dropped unanswered, `wait` retires its entry.
        let (wait, answer) = {
            let state = self.state();
            let Some(active) = state.active.as_ref() else {
                return Err(SteerError::NoActiveTurn);
            };
            if active.turn != input.turn {
                return Err(SteerError::TurnMismatch);
            }
            // The adapter's declared vendor refusal (C2 §2 `NotSteerable`).
            if let Some(refused) = self
                .kind
                .as_ref()
                .and_then(|kind| kind.steer_refused(&delivery))
            {
                return Err(refused);
            }
            let wait = active
                .emissions
                .wait(token)
                .ok_or(SteerError::NoActiveTurn)?;
            let answer = active
                .steer
                .send(input.text, expected, token)
                .map_err(steer_error)?;
            (wait, answer)
        };
        // Critical r3 #1: Route's answer, or the turn's end, whichever
        // comes first; an answer already there wins.
        let mut answer = answer;
        let replied = tokio::select! {
            biased;
            replied = &mut answer.reply => replied.ok(),
            () = wait.ended.cancelled() => answer.reply.try_recv().ok(),
        };
        match replied {
            Some(Ok(())) => {}
            // Critical r5 #1: the vendor's acknowledgement decides first,
            // whatever became of the write's answer: its report's emission
            // tells success from `NotRecorded`.
            _ if answer.acknowledged() => {}
            Some(Err(refused)) => return Err(steer_error(refused)),
            // The turn ended unanswered: an input Route started writing
            // may have reached the vendor; one it never took did not.
            None if answer.write_started() => return Err(SteerError::NotDelivered),
            None => return Err(SteerError::NoActiveTurn),
        }
        #[cfg(any(test, feature = "test-failpoints"))]
        if let Some(registry) = wait.registry.upgrade() {
            registry.acknowledged(token);
        }
        if wait.emitted().await {
            Ok(delivery)
        } else {
            Err(SteerError::NotRecorded { delivery })
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

    /// Sticky: true once a Host journal write the driver made outside any
    /// turn's report had an uncertain outcome, as a persistent connection's
    /// retirement after its logical turn ended (critical r1 #4). It is kept
    /// apart from the retirement's cleanup in `RetirementUncertain`, and
    /// from the health lane's first cause, which an earlier failure may
    /// hold: Core latches Store failure on it (runtime §7).
    pub fn journal_uncertain(&self) -> watch::Receiver<bool> {
        self.journal.subscribe()
    }

    /// Test builds only: reports an uncertain Host journal write outside
    /// any turn, as a persistent connection's retirement would.
    #[cfg(feature = "test-failpoints")]
    pub fn report_journal_uncertain(&self) {
        self.journal.send_replace(true);
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
