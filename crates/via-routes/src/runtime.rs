use serde_json::to_vec;
use tokio::sync::mpsc;

use super::{
    ConnectionId, Deadline, FakeMessage, FakeStart, PrivateProcessSpec, RawRef, RouteError,
    RuntimeConfig, RuntimeResources, SendOutcome, TerminalStatus, TurnNumber, WireRecovery,
    WireShutdown,
};
use via_wire::{CloseMode, CloseRequest, ExitReport, WireConnection, WireError, WireRuntime};

/// Final fake protocol evidence, including independently confirmed process exit.
pub struct FakeRouteResult {
    /// Vendor terminal status.
    pub status: TerminalStatus,
    /// Authoritative final text.
    pub final_text: String,
    /// Vendor stop reason retained verbatim.
    pub stop_reason: String,
    /// Optional vendor failure code.
    pub vendor_code: Option<String>,
    /// Raw reference to the terminal frame.
    pub terminal_raw: RawRef,
    /// Host-confirmed vendor exit after stdin was half-closed.
    pub exit: ExitReport,
    /// Group cleanup certainty after the vendor's confirmed exit.
    pub cleanup: via_wire::WireCleanup,
}

/// Paired acceptance evidence emitted before subsequent fake observations.
pub struct RouteAcceptance {
    /// Vendor-scoped turn identity confirmed by the fake.
    pub vendor_turn_id: String,
    /// Synced raw frame containing the matching acceptance.
    pub raw_ref: RawRef,
}

/// One-start state machine and opaque Wire runtime for the private fake route.
pub struct FakeRoute {
    wire: WireRuntime,
}

impl FakeRoute {
    /// Forwards the unopened resources to Wire's sole bootstrap split.
    pub fn new(config: RuntimeConfig, resources: RuntimeResources) -> Result<Self, WireError> {
        let wire = WireRuntime::new(config, resources)?;
        Ok(Self { wire })
    }

    /// Sends one prompt after durable submission and awaits paired terminal and real exit.
    pub async fn execute(
        &self,
        connection_id: ConnectionId,
        process: PrivateProcessSpec,
        start: FakeStart,
        acceptance: mpsc::Sender<RouteAcceptance>,
        deadline: Deadline,
    ) -> Result<FakeRouteResult, RouteError> {
        let turn = start.turn();
        let mut wire = self
            .wire
            .open_connection(connection_id, process, deadline)
            .await
            .map_err(|_| transport(turn))?;
        let outcome = Box::pin(Self::drive(&mut wire, start, acceptance, deadline)).await;
        if outcome.is_err() {
            let _report = wire
                .close(CloseRequest {
                    mode: CloseMode::Force,
                    deadline,
                })
                .await;
        }
        outcome
    }

    /// Drains Host controls and reapers before Core releases the Store owner.
    pub async fn shutdown(&self, deadline: Deadline) -> Result<WireShutdown, WireError> {
        self.wire.shutdown(deadline).await
    }

    /// Returns passive Host recovery facts without exposing a signal handle.
    pub async fn recover(&self, deadline: Deadline) -> Result<Vec<WireRecovery>, WireError> {
        self.wire.recover(deadline).await
    }

    async fn drive(
        wire: &mut WireConnection,
        start: FakeStart,
        acceptance: mpsc::Sender<RouteAcceptance>,
        deadline: Deadline,
    ) -> Result<FakeRouteResult, RouteError> {
        let turn = start.turn();
        let mut bytes = to_vec(&start).map_err(|_| protocol(turn, "cannot encode fake start"))?;
        bytes.push(b'\n');
        if wire
            .write_frame(&bytes, deadline)
            .await
            .map_err(|_| transport(turn))?
            != SendOutcome::Written
        {
            return Err(transport(turn));
        }
        let mut accepted = false;
        loop {
            let Some(frame) = Box::pin(wire.next_frame(deadline))
                .await
                .map_err(|_| transport(turn))?
            else {
                wire.wait_exit(deadline)
                    .await
                    .map_err(|_| transport(turn))?;
                return Err(RouteError::ProcessExited { turn });
            };
            let message = FakeMessage::decode(frame.bytes(), turn)?;
            match message {
                FakeMessage::Accepted { vendor_turn_id } if !accepted => {
                    accepted = true;
                    acceptance
                        .try_send(RouteAcceptance {
                            vendor_turn_id,
                            raw_ref: frame.raw_ref().clone(),
                        })
                        .map_err(|_| RouteError::Overflow { turn })?;
                }
                FakeMessage::Accepted { .. } => {
                    return Err(protocol(turn, "duplicate fake acceptance"));
                }
                FakeMessage::Terminal {
                    status,
                    final_text,
                    stop_reason,
                    vendor_code,
                    ..
                } if accepted => {
                    // Fake finalization waits on EOF; release stdin before waiting on process exit.
                    wire.close_input(deadline)
                        .await
                        .map_err(|_| transport(turn))?;
                    // Terminal is semantic completion, not transport EOF. Drain both pipes
                    // and reject any later stdout frame before accepting the exit.
                    let trailing = Box::pin(wire.next_frame(deadline))
                        .await
                        .map_err(|_| transport(turn))?;
                    if let Some(trailing) = trailing {
                        let _message = FakeMessage::decode(trailing.bytes(), turn)?;
                        return Err(protocol(turn, "fake observation after terminal"));
                    }
                    let exit = wire
                        .wait_exit(deadline)
                        .await
                        .map_err(|_| transport(turn))?;
                    let cleanup_deadline = Deadline::at(
                        tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                    );
                    let close = wire
                        .close(CloseRequest {
                            mode: CloseMode::Graceful,
                            deadline: cleanup_deadline,
                        })
                        .await;
                    return Ok(FakeRouteResult {
                        status,
                        final_text,
                        stop_reason,
                        vendor_code,
                        terminal_raw: frame.raw_ref().clone(),
                        exit,
                        cleanup: close.cleanup,
                    });
                }
                FakeMessage::Terminal { .. } => {
                    return Err(protocol(turn, "fake terminal before acceptance"));
                }
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::UnknownNotification { .. }
                    if accepted => {}
                FakeMessage::InterruptAck { .. } => {
                    return Err(protocol(turn, "unsolicited fake interrupt acknowledgement"));
                }
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::UnknownNotification { .. } => {
                    return Err(protocol(turn, "fake observation before acceptance"));
                }
            }
        }
    }
}

fn protocol(turn: TurnNumber, detail: &'static str) -> RouteError {
    RouteError::Protocol { turn, detail }
}

fn transport(turn: TurnNumber) -> RouteError {
    RouteError::TransportLost {
        turn,
        evidence: None,
    }
}
