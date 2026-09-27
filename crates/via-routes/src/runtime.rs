use serde_json::to_vec;
use tokio::{sync::mpsc, time::timeout_at};

use super::{
    ConnectionId, Deadline, FakeMessage, FakeStart, PrivateProcessSpec, RawRef, RouteError,
    RouteMessage, RuntimeConfig, RuntimeResources, SendOutcome, TerminalStatus, TurnNumber,
    WireRecovery, WireShutdown,
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
    ///
    /// Every decoded message, including acceptance, the terminal and late observations
    /// after it, is sent on `observations` in decode order with its synced raw span.
    /// When that channel is full the route waits, bounded by `deadline`.
    pub async fn execute(
        &self,
        connection_id: ConnectionId,
        process: PrivateProcessSpec,
        start: FakeStart,
        observations: mpsc::Sender<RouteMessage>,
        deadline: Deadline,
    ) -> Result<FakeRouteResult, RouteError> {
        let turn = start.turn();
        let mut wire = self
            .wire
            .open_connection(connection_id, process, deadline)
            .await
            .map_err(|_| transport(turn))?;
        let outcome = Box::pin(Self::drive(&mut wire, start, &observations, deadline)).await;
        if outcome.is_err() {
            let _report = wire
                .close(CloseRequest {
                    mode: CloseMode::Force,
                    deadline,
                })
                .await;
            // The group is stopping; keep both tails as raw evidence. The original
            // failure stays authoritative even if this drain cannot finish.
            let _drained = Box::pin(wire.drain_to_eof(deadline)).await;
        }
        outcome
    }

    /// Drains Host controls and reapers before Core releases the Store owner.
    pub async fn shutdown(&self, deadline: Deadline) -> WireShutdown {
        self.wire.shutdown(deadline).await
    }

    /// Returns passive Host recovery facts without exposing a signal handle.
    pub async fn recover(&self, deadline: Deadline) -> Result<Vec<WireRecovery>, WireError> {
        self.wire.recover(deadline).await
    }

    async fn drive(
        wire: &mut WireConnection,
        start: FakeStart,
        observations: &mpsc::Sender<RouteMessage>,
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
        let mut phase = Phase::Submitted;
        let terminal = loop {
            let Some(message) = Box::pin(next_message(wire, turn, deadline)).await? else {
                wire.wait_exit(deadline)
                    .await
                    .map_err(|_| transport(turn))?;
                return Err(RouteError::ProcessExited { turn });
            };
            phase.advance(&message.payload, turn)?;
            let terminal = terminal_evidence(&message);
            forward(observations, message, turn, deadline).await?;
            if let Some(terminal) = terminal {
                break terminal;
            }
        };
        // Terminal is semantic completion, not transport EOF. Half-close input (fake
        // finalization waits on it), then drain both pipes to EOF under the turn deadline:
        // late observations are forwarded and anything that breaks the phase order,
        // such as a second terminal, fails the turn.
        wire.close_input(deadline)
            .await
            .map_err(|_| transport(turn))?;
        while let Some(message) = Box::pin(next_message(wire, turn, deadline)).await? {
            phase.advance(&message.payload, turn)?;
            forward(observations, message, turn, deadline).await?;
        }
        let exit = wire
            .wait_exit(deadline)
            .await
            .map_err(|_| transport(turn))?;
        let cleanup_deadline =
            Deadline::at(tokio::time::Instant::now() + std::time::Duration::from_secs(3));
        let close = wire
            .close(CloseRequest {
                mode: CloseMode::Graceful,
                deadline: cleanup_deadline,
            })
            .await;
        Ok(FakeRouteResult {
            status: terminal.status,
            final_text: terminal.final_text,
            stop_reason: terminal.stop_reason,
            vendor_code: terminal.vendor_code,
            terminal_raw: terminal.raw_ref,
            exit,
            cleanup: close.cleanup,
        })
    }
}

/// Connection-local protocol phase for the only turn on a fake connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Start written; awaiting its paired acceptance.
    Submitted,
    /// Acceptance seen; observations and one terminal may follow.
    Accepted,
    /// Terminal seen; only late observations may follow.
    Terminated,
}

impl Phase {
    /// Checks one decoded message against the phase and advances it.
    fn advance(&mut self, message: &FakeMessage, turn: TurnNumber) -> Result<(), RouteError> {
        match (message, *self) {
            (FakeMessage::Accepted { .. }, Self::Submitted) => *self = Self::Accepted,
            (FakeMessage::Accepted { .. }, Self::Accepted | Self::Terminated) => {
                return Err(protocol(turn, "duplicate fake acceptance"));
            }
            (FakeMessage::Terminal { .. }, Self::Accepted) => *self = Self::Terminated,
            (FakeMessage::Terminal { .. }, Self::Submitted) => {
                return Err(protocol(turn, "fake terminal before acceptance"));
            }
            (FakeMessage::Terminal { .. }, Self::Terminated) => {
                return Err(protocol(turn, "duplicate fake terminal"));
            }
            (FakeMessage::InterruptAck { .. }, _) => {
                return Err(protocol(turn, "unsolicited fake interrupt acknowledgement"));
            }
            (
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::UnknownNotification { .. },
                Self::Submitted,
            ) => return Err(protocol(turn, "fake observation before acceptance")),
            (
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::UnknownNotification { .. },
                Self::Accepted | Self::Terminated,
            ) => {}
        }
        Ok(())
    }
}

/// Terminal fields retained for the route result.
struct TerminalEvidence {
    status: TerminalStatus,
    final_text: String,
    stop_reason: String,
    vendor_code: Option<String>,
    raw_ref: RawRef,
}

fn terminal_evidence(message: &RouteMessage) -> Option<TerminalEvidence> {
    match &message.payload {
        FakeMessage::Terminal {
            status,
            final_text,
            stop_reason,
            vendor_code,
            ..
        } => Some(TerminalEvidence {
            status: *status,
            final_text: final_text.clone(),
            stop_reason: stop_reason.clone(),
            vendor_code: vendor_code.clone(),
            raw_ref: message.raw_ref.clone(),
        }),
        FakeMessage::Accepted { .. }
        | FakeMessage::Text { .. }
        | FakeMessage::ToolStarted { .. }
        | FakeMessage::ToolEnded { .. }
        | FakeMessage::InterruptAck { .. }
        | FakeMessage::UnknownNotification { .. } => None,
    }
}

/// Reads and decodes the next synced frame; `None` means both pipes reached EOF.
async fn next_message(
    wire: &mut WireConnection,
    turn: TurnNumber,
    deadline: Deadline,
) -> Result<Option<RouteMessage>, RouteError> {
    let Some(frame) = wire
        .next_frame(deadline)
        .await
        .map_err(|_| transport(turn))?
    else {
        return Ok(None);
    };
    Ok(Some(RouteMessage {
        payload: FakeMessage::decode(frame.bytes(), turn)?,
        raw_ref: frame.raw_ref().clone(),
    }))
}

/// Waits for observation capacity until the turn deadline; a consumer that neither
/// drains nor stays attached is an overflow, never a silent drop.
async fn forward(
    observations: &mpsc::Sender<RouteMessage>,
    message: RouteMessage,
    turn: TurnNumber,
    deadline: Deadline,
) -> Result<(), RouteError> {
    match timeout_at(deadline.instant(), observations.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(RouteError::Overflow { turn }),
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

#[cfg(test)]
mod tests {
    use super::{FakeMessage, Phase, RouteError, TurnNumber};

    fn decode(json: &str) -> FakeMessage {
        FakeMessage::decode(json.as_bytes(), TurnNumber::try_from(1).unwrap()).unwrap()
    }

    fn detail(phase: &mut Phase, json: &str) -> Option<&'static str> {
        match phase.advance(&decode(json), TurnNumber::try_from(1).unwrap()) {
            Ok(()) => None,
            Err(RouteError::Protocol { detail, .. }) => Some(detail),
            Err(other) => panic!("unexpected route error {other:?}"),
        }
    }

    const ACCEPTED: &str = r#"{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}"#;
    const TERMINAL: &str = r#"{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"","stop_reason":"end_turn"}"#;
    const OBSERVATIONS: [&str; 4] = [
        r#"{"type":"text","vendor_turn_id":"fake-turn-1","text":"hi"}"#,
        r#"{"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t","name":"sh","input_summary":""}"#,
        r#"{"type":"tool_ended","vendor_turn_id":"fake-turn-1","tool_id":"t","status":"completed","output_summary":"","exit_code":null}"#,
        r#"{"type":"later","value":1}"#,
    ];

    #[test]
    fn observations_are_admitted_after_acceptance_and_after_terminal() {
        let mut phase = Phase::Submitted;
        assert_eq!(detail(&mut phase, ACCEPTED), None);
        for message in OBSERVATIONS {
            assert_eq!(detail(&mut phase, message), None);
        }
        assert_eq!(detail(&mut phase, TERMINAL), None);
        assert_eq!(phase, Phase::Terminated);
        for message in OBSERVATIONS {
            assert_eq!(detail(&mut phase, message), None, "late {message}");
        }
    }

    #[test]
    fn order_violations_fail_the_turn() {
        for message in OBSERVATIONS {
            assert_eq!(
                detail(&mut Phase::Submitted, message),
                Some("fake observation before acceptance")
            );
        }
        assert_eq!(
            detail(&mut Phase::Submitted, TERMINAL),
            Some("fake terminal before acceptance")
        );
        assert_eq!(
            detail(&mut Phase::Terminated, TERMINAL),
            Some("duplicate fake terminal")
        );
        for phase in [Phase::Accepted, Phase::Terminated] {
            assert_eq!(
                detail(&mut { phase }, ACCEPTED),
                Some("duplicate fake acceptance")
            );
        }
        let ack = r#"{"type":"interrupt_ack","id":2,"vendor_turn_id":"fake-turn-1"}"#;
        for phase in [Phase::Submitted, Phase::Accepted, Phase::Terminated] {
            assert_eq!(
                detail(&mut { phase }, ack),
                Some("unsolicited fake interrupt acknowledgement")
            );
        }
    }
}
