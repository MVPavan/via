use thiserror::Error;
use tokio::sync::mpsc;

use crate::{
    AcceptanceToken, Cleanup, ConnectionId, Deadline, FakeAcceptanceObservation, FakeConfig,
    FakeTerminalEvidence, ProcessOwner, RuntimeConfig, RuntimeResources, SessionId, TurnNumber,
    VendorTerminalStatus, VendorTurnId,
};
use via_routes::{
    FakeMessage, FakeRoute, FakeRouteResult, FakeStart, RouteError, RouteMessage, TerminalStatus,
    WireRecovery,
};

/// Immutable fake deployment and Host paths supplied at daemon bootstrap.
pub struct AdapterRuntimeConfig {
    /// Opaque Wire deployment paths forwarded unopened through Route.
    pub runtime: RuntimeConfig,
    /// Fake-only fixture launch configuration.
    pub fake: FakeConfig,
}

/// Adapter construction or fake-drive failure without handle or prompt text.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// Lower protocol or process boundary failed.
    #[error("fake route failed: {0}")]
    Route(#[from] RouteError),
    /// Lower runtime could not initialize.
    #[error("adapter runtime failed: {0}")]
    Open(#[from] via_routes::WireError),
    /// Fake launch settings are unavailable.
    #[error("fake route is unavailable")]
    Unavailable,
    /// A typed fake request or observation could not be represented.
    #[error("fake protocol identity is invalid")]
    Protocol,
    /// The reserved acceptance observation slot was not drained.
    #[error("fake acceptance observation overflow")]
    ObservationOverflow,
}

/// Passive recovery facts for Core's later crash reconciliation.
pub struct FakeRecovery {
    /// Owning VIA session.
    pub session_id: SessionId,
    /// Opaque committed anchor identifier.
    pub anchor_id: String,
    /// Opaque launch generation.
    pub generation: String,
    /// Owning turn.
    pub turn: TurnNumber,
    /// Cleanup certainty under Host's validated group.
    pub cleanup: Cleanup,
}

/// Fake Adapter with an opaque Route/Wire runtime and immutable fixture policy.
pub struct AdapterRuntime {
    route: FakeRoute,
    fake: FakeConfig,
}

impl AdapterRuntime {
    /// Forwards the unopened Store resource bundle to Route and Wire.
    pub fn new(
        config: AdapterRuntimeConfig,
        resources: RuntimeResources,
    ) -> Result<Self, AdapterError> {
        let route = FakeRoute::new(config.runtime, resources)?;
        Ok(Self {
            route,
            fake: config.fake,
        })
    }

    /// Whether this daemon has the explicitly configured fake executable and fixture.
    pub fn fake_available(&self) -> bool {
        self.fake.is_available()
    }

    /// Runs one submitted fake turn; acceptance travels on a reserved small control slot.
    pub async fn execute(
        &self,
        session_id: SessionId,
        turn: TurnNumber,
        connection_id: ConnectionId,
        prompt: String,
        acceptance: mpsc::Sender<FakeAcceptanceObservation>,
        deadline: Deadline,
    ) -> Result<FakeTerminalEvidence, AdapterError> {
        let owner = ProcessOwner {
            session_id: session_id.clone(),
            turn,
        };
        let process = self
            .fake
            .process_spec(owner)
            .map_err(|_| AdapterError::Unavailable)?;
        let start = FakeStart::new(session_id.as_str().to_owned(), turn, prompt)
            .map_err(|_| AdapterError::Protocol)?;
        // Full: Route waits for capacity under the turn deadline, so this loop keeps
        // draining until the route finishes.
        let (route_tx, mut route_rx) = mpsc::channel::<RouteMessage>(64);
        let route = self
            .route
            .execute(connection_id, process, start, route_tx, deadline);
        tokio::pin!(route);
        loop {
            tokio::select! {
                Some(message) = route_rx.recv() => {
                    forward_observation(message, &acceptance)?;
                }
                result = &mut route => {
                    while let Ok(message) = route_rx.try_recv() {
                        forward_observation(message, &acceptance)?;
                    }
                    return result.map(normalize_terminal).map_err(AdapterError::Route);
                }
            }
        }
    }

    /// Drains lower process owners before Store shutdown and returns passive facts.
    pub async fn shutdown(&self, deadline: Deadline) -> FakeShutdown {
        let report = self.route.shutdown(deadline).await;
        FakeShutdown {
            recovery: report
                .recovery
                .into_iter()
                .map(normalize_recovery)
                .collect(),
            pending_tasks: report.pending_tasks,
            failed_tasks: report.failed_tasks,
            failure: report.failure,
        }
    }

    /// Recovers committed anchors without giving Core process signalling authority.
    pub async fn recover(&self, deadline: Deadline) -> Result<Vec<FakeRecovery>, AdapterError> {
        self.route
            .recover(deadline)
            .await
            .map(|reports| reports.into_iter().map(normalize_recovery).collect())
            .map_err(AdapterError::Open)
    }
}

/// Forwards acceptance to Core. Core has no path for other observations yet, so they
/// are dropped here after Route recorded their raw spans; the terminal arrives in the
/// route result.
fn forward_observation(
    message: RouteMessage,
    sender: &mpsc::Sender<FakeAcceptanceObservation>,
) -> Result<(), AdapterError> {
    // Route admits exactly one acceptance per turn.
    let FakeMessage::Accepted { vendor_turn_id } = message.payload else {
        return Ok(());
    };
    let correlation = AcceptanceToken::try_from(1).map_err(|_| AdapterError::Protocol)?;
    let vendor_turn_id =
        VendorTurnId::try_from(vendor_turn_id).map_err(|_| AdapterError::Protocol)?;
    sender
        .try_send(FakeAcceptanceObservation {
            correlation,
            vendor_turn_id,
            raw_ref: message.raw_ref,
        })
        .map_err(|_| AdapterError::ObservationOverflow)
}

fn normalize_terminal(result: FakeRouteResult) -> FakeTerminalEvidence {
    let status = match result.status {
        TerminalStatus::Completed => VendorTerminalStatus::Completed,
        TerminalStatus::Interrupted => VendorTerminalStatus::Interrupted,
        TerminalStatus::Failed => VendorTerminalStatus::Failed,
    };
    FakeTerminalEvidence {
        status,
        final_text: result.final_text,
        stop_reason: result.stop_reason,
        vendor_code: result.vendor_code,
        terminal_raw: result.terminal_raw,
        exit: result.exit,
        cleanup: match result.cleanup {
            via_routes::WireCleanup::Quiescent => Cleanup::Quiescent,
            via_routes::WireCleanup::Uncertain => Cleanup::Uncertain,
        },
    }
}

fn normalize_recovery(report: WireRecovery) -> FakeRecovery {
    FakeRecovery {
        session_id: report.owner_session,
        anchor_id: report.anchor_id,
        generation: report.generation,
        turn: report.owner_turn,
        cleanup: match report.cleanup {
            via_routes::WireCleanup::Quiescent => Cleanup::Quiescent,
            via_routes::WireCleanup::Uncertain => Cleanup::Uncertain,
        },
    }
}

/// Passive shutdown status; no Host operation or signal handle escapes Adapter.
pub struct FakeShutdown {
    /// Recovery facts for every committed anchor.
    pub recovery: Vec<FakeRecovery>,
    /// Host tasks still pending at the shutdown deadline.
    pub pending_tasks: usize,
    /// Host tasks that panicked, were cancelled or failed their child wait.
    pub failed_tasks: usize,
    /// Bounded description of the deadline, Store or recovery failure, if any.
    pub failure: Option<String>,
}
