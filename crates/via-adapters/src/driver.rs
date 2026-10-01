//! The C2 driver lane (C2 §2, adapter design §3.2, AD3, AD16): a logical
//! session driver whose `run_turn` is the one data lane per submitted turn,
//! with `steer`, `close` and `health` serviceable meanwhile.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::value::RawValue;
use tokio::sync::{mpsc, oneshot, watch};

use crate::fake::FakeAdapter;
use crate::observation::{ObservationSink, SteerDelivery, TurnEnd, TurnError};
use crate::plan::{Bound, Inherit, VendorOptions};
use crate::{
    CapacityToken, Cleanup, Deadline, DriverHealth, SessionId, StopWatch, TurnActivity, TurnNumber,
};
use via_routes::{FakeRoute, SteerRefused, SteerRequest};

/// The daemon force: `None` until raised, then the instant it was raised.
pub type ForceWatch = watch::Receiver<Option<tokio::time::Instant>>;

/// A session's context, attached at `open_session` or `recover` (C2 §2).
/// The fake driver spawns no task of its own, so it takes no task tracker
/// or cancellation token yet.
pub struct SessionCx {
    /// The session's observation channel: session-level and turn items.
    pub observations: ObservationSink,
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
}

/// Why steer input was not delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteerError {
    /// The route does not support steer.
    Unsupported,
    /// No turn is accepted and running.
    NoActiveTurn,
    /// Another steer is still being delivered.
    Busy,
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
    /// The vendor session was closed.
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
    /// The persistent profile's slot, held between turns (decision H1).
    pub(crate) capacity: Option<CapacityToken>,
    /// The persistent connection is live.
    pub(crate) live: bool,
    /// The connection generation; each new connection advances it.
    pub(crate) generation: u64,
    /// The running turn's steer lane.
    pub(crate) steer: Option<mpsc::Sender<SteerRequest>>,
    /// The session was closed.
    pub(crate) closed: bool,
}

/// One session's driver (C2 §2).
pub struct SessionDriver {
    pub(crate) route: Arc<FakeRoute>,
    /// `None` when no adapter serves the session's harness.
    pub(crate) adapter: Option<Arc<FakeAdapter>>,
    pub(crate) spec: SessionSpec,
    pub(crate) observations: ObservationSink,
    health: watch::Sender<DriverHealth>,
    state: Mutex<DriverState>,
}

impl SessionDriver {
    pub(crate) fn new(
        route: Arc<FakeRoute>,
        adapter: Option<Arc<FakeAdapter>>,
        spec: SessionSpec,
        cx: SessionCx,
    ) -> Self {
        Self {
            route,
            adapter,
            spec,
            observations: cx.observations,
            health: watch::Sender::new(DriverHealth::Open),
            state: Mutex::new(DriverState::default()),
        }
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, DriverState> {
        // No code panics while holding the lock; a poisoned state is still
        // the last consistent one.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn persistent(&self) -> bool {
        self.adapter
            .as_ref()
            .is_some_and(|adapter| adapter.profile().persistent)
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
            return rejected(TurnError::Unavailable);
        };
        crate::fake::run_turn(self, &adapter, spec, cx).await
    }

    /// Takes the turn's connection: a pin must name the live generation
    /// (AD16 rule 4), and a new connection advances the generation. On the
    /// persistent profile the slot stays with the driver; otherwise it goes
    /// to Host with the process, which is returned.
    pub(crate) fn connect(
        &self,
        prepared: Prepared,
        capacity: Option<CapacityToken>,
    ) -> Result<(u64, Option<CapacityToken>), TurnError> {
        let persistent = self.persistent();
        let mut state = self.state();
        if state.closed {
            return Err(TurnError::Rejected(crate::StartRejected::SessionGone));
        }
        match prepared {
            Prepared::Pinned(pin)
                if persistent && state.live && pin.generation == state.generation =>
            {
                Ok((state.generation, None))
            }
            // The pinned connection died before submission: nothing sent.
            Prepared::Pinned(_) => Err(TurnError::Rejected(crate::StartRejected::SessionGone)),
            Prepared::NeedsConnection => {
                state.generation += 1;
                if persistent {
                    state.capacity = capacity;
                    state.live = true;
                    Ok((state.generation, None))
                } else {
                    Ok((state.generation, capacity))
                }
            }
        }
    }

    /// The persistent connection is gone: its slot is released.
    pub(crate) fn disconnect(&self) {
        let mut state = self.state();
        state.live = false;
        state.capacity = None;
    }

    /// Delivers steer input into the running turn (C2 §2).
    pub async fn steer(&self, input: SteerInput) -> Result<SteerDelivery, SteerError> {
        let supported = self.adapter.as_ref().is_some_and(|adapter| {
            !matches!(
                adapter.profile().capabilities.verbs.steer,
                crate::Support::Unsupported { .. }
            )
        });
        if !supported {
            return Err(SteerError::Unsupported);
        }
        let Some(lane) = self.state().steer.clone() else {
            return Err(SteerError::NoActiveTurn);
        };
        let (reply, answer) = oneshot::channel();
        let request = SteerRequest {
            text: input.text,
            reply,
        };
        match lane.try_send(request) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(SteerError::Busy),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(SteerError::NoActiveTurn),
        }
        match answer.await {
            Ok(Ok(())) => Ok(SteerDelivery::Injected),
            Ok(Err(SteerRefused::NotWritten)) => Err(SteerError::NotDelivered),
            // Not accepted yet, or the turn ended first.
            Ok(Err(SteerRefused::NotActive)) | Err(_) => Err(SteerError::NoActiveTurn),
        }
    }

    /// Closes the session (C2 §2). The fake has no connection between its
    /// turns' processes: the persistent profile's held slot is released,
    /// its idle retirement.
    pub fn close(
        &self,
        _mode: CloseMode,
        _deadline: Deadline,
    ) -> impl Future<Output = CloseReport> + Send + use<> {
        {
            let mut state = self.state();
            state.closed = true;
            state.live = false;
            state.capacity = None;
            state.steer = None;
        }
        self.health.send_replace(DriverHealth::Closed);
        std::future::ready(CloseReport {
            vendor_closed: true,
            process_exit: None,
            cleanup: Cleanup::Quiescent,
            warnings: Vec::new(),
            leftovers: None,
        })
    }

    /// The sticky health lane.
    pub fn health(&self) -> watch::Receiver<DriverHealth> {
        self.health.subscribe()
    }
}

/// A turn rejected before anything ran.
pub(crate) fn rejected(error: TurnError) -> TurnEnd {
    TurnEnd {
        terminal: None,
        instance: None,
        leftovers: None,
        outcome: Err(error),
    }
}
