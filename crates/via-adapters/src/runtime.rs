use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    time::timeout_at,
};

use crate::{
    AcceptanceToken, Cleanup, ConnectionId, Deadline, FakeAcceptanceObservation, FakeConfig,
    FakeObservation, FakeTerminalEvidence, MAX_OBSERVATION_BYTES, Observation, ProcessOwner,
    RouteError, RouteFailure, RuntimeConfig, RuntimeResources, SessionId, TurnNumber,
    VendorTerminalStatus, VendorTurnId,
};
use via_routes::{
    FakeMessage, FakeRoute, FakeRouteResult, FakeStart, RouteMessage, TerminalStatus, WireRecovery,
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
    /// Lower protocol or process boundary failed, with its typed cause and evidence.
    #[error("fake route failed: {0}")]
    Route(#[from] RouteFailure),
    /// Lower runtime could not initialize.
    #[error("adapter runtime failed: {0}")]
    Open(#[from] via_routes::WireError),
    /// Fake launch settings are unavailable.
    #[error("fake route is unavailable")]
    Unavailable,
    /// A typed fake request or observation could not be represented.
    #[error("fake protocol identity is invalid")]
    Protocol,
}

impl AdapterError {
    /// Durable Store state could not be read or written; other failures leave
    /// evidence unproven without making Store unusable.
    pub fn is_store_failure(&self) -> bool {
        matches!(self, Self::Open(error) if error.is_store_failure())
    }
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
    /// Host stopped the group while its vendor was live (Host force evidence).
    pub forced: bool,
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

    /// Runs one submitted fake turn and forwards every observation to Core in
    /// decode order. When Core's channel is full this waits, bounded by `deadline`;
    /// if Core cannot take an observation, the Route receiver is dropped so Route
    /// fails the turn as overflow and still performs its cleanup and drain.
    /// `force` set force-closes the turn through Route (C2 Close(Force)).
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is a distinct input of the one turn"
    )]
    pub async fn execute(
        &self,
        session_id: SessionId,
        turn: TurnNumber,
        connection_id: ConnectionId,
        prompt: String,
        observations: mpsc::Sender<FakeObservation>,
        deadline: Deadline,
        force: watch::Receiver<bool>,
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
        // Full: Route waits for capacity under the turn deadline while this loop
        // forwards to Core, which drains until the route finishes.
        let (route_tx, route_rx) = mpsc::channel::<RouteMessage>(64);
        let route = self.route.execute(
            connection_id,
            process,
            start,
            route_tx,
            deadline,
            force.clone(),
        );
        let mut force = force;
        tokio::pin!(route);
        let mut route_rx = Some(route_rx);
        loop {
            tokio::select! {
                Some(message) = recv(route_rx.as_mut()) => {
                    if deliver(message, &observations, deadline, &mut force).await.is_err() {
                        // Route observes the closed channel as overflow, or
                        // after a force stops forwarding and force-closes.
                        route_rx = None;
                    }
                }
                result = &mut route => {
                    let mut delivered = true;
                    if let Some(receiver) = route_rx.as_mut() {
                        while let Ok(message) = receiver.try_recv() {
                            if deliver(message, &observations, deadline, &mut force).await.is_err() {
                                delivered = false;
                                break;
                            }
                        }
                    }
                    // A route failure is the first cause; undelivered data fails a success.
                    return match result {
                        Ok(result) if delivered => Ok(normalize_terminal(result)),
                        Ok(result) => Err(AdapterError::Route(RouteFailure {
                            cause: RouteError::Overflow { turn },
                            evidence: None,
                            exit: Some(result.exit),
                            raw_incomplete: false,
                            launched: true,
                            cleanup: None,
                            forced: false,
                        })),
                        Err(failure) => Err(AdapterError::Route(failure)),
                    };
                }
            }
        }
    }

    /// Drains lower process owners before Store shutdown and returns passive facts.
    pub async fn shutdown(
        &self,
        deadline: Deadline,
        turns: &[(SessionId, TurnNumber)],
    ) -> FakeShutdown {
        let report = self.route.shutdown(deadline, turns).await;
        FakeShutdown {
            recovery: report
                .recovery
                .into_iter()
                .map(|turn| FakeTurnRecovery {
                    session_id: turn.owner_session,
                    turn: turn.owner_turn,
                    cleanup: match turn.cleanup {
                        via_routes::WireCleanup::Quiescent => Cleanup::Quiescent,
                        via_routes::WireCleanup::Uncertain => Cleanup::Uncertain,
                    },
                    forced: turn.forced,
                })
                .collect(),
            anchors: report.anchors,
            uncertain_anchors: report.uncertain_anchors,
            pending_tasks: report.pending_tasks,
            failed_tasks: report.failed_tasks,
            failure: report.failure,
        }
    }

    /// Recovers one page of committed anchors, up to `limit` after the
    /// `after` id, without giving Core process signalling authority.
    pub async fn recover_page(
        &self,
        after: Option<String>,
        limit: u32,
        deadline: Deadline,
    ) -> Result<Vec<FakeRecovery>, AdapterError> {
        self.route
            .recover_page(after, limit, deadline)
            .await
            .map(|reports| reports.into_iter().map(normalize_recovery).collect())
            .map_err(AdapterError::Open)
    }
}

/// Resolves once `force` is set; never when its sender is gone unset.
async fn forced(force: &mut watch::Receiver<bool>) {
    if force.wait_for(|force| *force).await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn recv(receiver: Option<&mut mpsc::Receiver<RouteMessage>>) -> Option<RouteMessage> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => None,
    }
}

/// Normalizes one Route message and waits, bounded by `deadline`, for Core to
/// take each resulting observation. The terminal travels in the route result.
/// A force ends the wait, so Route is polled into its force close and drain.
async fn deliver(
    message: RouteMessage,
    observations: &mpsc::Sender<FakeObservation>,
    deadline: Deadline,
    force: &mut watch::Receiver<bool>,
) -> Result<(), ()> {
    for observation in normalize(message)? {
        let sent = tokio::select! {
            // Capacity first: a draining Core still commits frames already read.
            biased;
            sent = timeout_at(deadline.instant(), observations.send(observation)) => sent,
            () = forced(force) => return Err(()),
        };
        match sent {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => return Err(()),
        }
    }
    Ok(())
}

/// Maps one decoded fake message to C2 observations; oversized text is split.
fn normalize(message: RouteMessage) -> Result<Vec<FakeObservation>, ()> {
    let raw_ref = message.raw_ref;
    let data = |observation| FakeObservation::Data {
        observation,
        raw_ref: raw_ref.clone(),
    };
    Ok(match message.payload {
        // Route admits exactly one acceptance per turn.
        FakeMessage::Accepted { vendor_turn_id } => {
            vec![FakeObservation::Accepted(FakeAcceptanceObservation {
                correlation: AcceptanceToken::try_from(1).map_err(|_| ())?,
                vendor_turn_id: VendorTurnId::try_from(vendor_turn_id).map_err(|_| ())?,
                raw_ref: raw_ref.clone(),
            })]
        }
        FakeMessage::Text { text, .. } => split_text(&text)
            .into_iter()
            .map(|text| data(Observation::AssistantText { text }))
            .collect(),
        FakeMessage::ToolStarted {
            tool_id,
            name,
            input_summary,
            ..
        } => vec![data(Observation::ToolStarted {
            tool_id,
            name,
            input_summary,
        })],
        FakeMessage::ToolEnded {
            tool_id,
            status,
            output_summary,
            exit_code,
            ..
        } => vec![data(Observation::ToolEnded {
            tool_id,
            status,
            output_summary,
            exit_code,
        })],
        FakeMessage::UnknownNotification {
            vendor_type,
            raw_payload,
            truncated,
        } => vec![data(Observation::VendorOther {
            vendor_type,
            payload: raw_payload,
            truncated,
        })],
        // Route rejects interrupt acknowledgements; the terminal is the route result.
        FakeMessage::Terminal { .. } | FakeMessage::InterruptAck { .. } => Vec::new(),
    })
}

/// Encoded bytes of an `assistant.text` payload other than its text:
/// `{"text":"","final":false}`.
const TEXT_PAYLOAD_OVERHEAD: usize = 25;

/// Splits text in order at UTF-8 boundaries so that each piece's encoded
/// `assistant.text` payload stays within C2's 256 KiB bound.
fn split_text(text: &str) -> Vec<String> {
    let budget = MAX_OBSERVATION_BYTES - TEXT_PAYLOAD_OVERHEAD;
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut encoded = 0;
    for (index, character) in text.char_indices() {
        let width = escaped_len(character);
        if encoded + width > budget {
            pieces.push(text[start..index].to_owned());
            start = index;
            encoded = 0;
        }
        encoded += width;
    }
    if start < text.len() || pieces.is_empty() {
        pieces.push(text[start..].to_owned());
    }
    pieces
}

/// Bytes `serde_json` writes for one character inside a JSON string.
fn escaped_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        '\0'..='\u{1f}' => 6,
        _ => character.len_utf8(),
    }
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
        forced: report.forced,
    }
}

/// Passive per-turn shutdown recovery facts.
pub struct FakeTurnRecovery {
    /// Owning VIA session.
    pub session_id: SessionId,
    /// Owning turn.
    pub turn: TurnNumber,
    /// Quiescent only when every anchor of the turn was proved absent.
    pub cleanup: Cleanup,
    /// Host stopped a group of the turn while its vendor was live.
    pub forced: bool,
}

/// Passive shutdown status; no Host operation or signal handle escapes Adapter.
pub struct FakeShutdown {
    /// Per-turn recovery facts for the requested turns.
    pub recovery: Vec<FakeTurnRecovery>,
    /// Committed anchors reconciled.
    pub anchors: usize,
    /// Reconciled anchors without positive absence proof.
    pub uncertain_anchors: usize,
    /// Host tasks still pending at the shutdown deadline.
    pub pending_tasks: usize,
    /// Host tasks that panicked, were cancelled or failed their child wait.
    pub failed_tasks: usize,
    /// Bounded description of the deadline, Store or recovery failure, if any.
    pub failure: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{MAX_OBSERVATION_BYTES, split_text};

    /// Encoded bytes of the `assistant.text` payload Core commits for one piece.
    fn encoded(text: &str) -> usize {
        serde_json::to_vec(&serde_json::json!({"text":text,"final":false}))
            .unwrap()
            .len()
    }

    #[test]
    fn text_splits_in_order_within_the_encoded_payload_bound() {
        let cases = [
            String::new(),
            "short".to_owned(),
            "é".repeat(140_000),
            "a".repeat(MAX_OBSERVATION_BYTES),
            "\u{1}\"\n😀".repeat(40_000),
        ];
        for text in cases {
            let pieces = split_text(&text);
            assert_eq!(pieces.concat(), text);
            assert!(!pieces.is_empty());
            for piece in &pieces {
                assert!(
                    encoded(piece) <= MAX_OBSERVATION_BYTES,
                    "{}",
                    encoded(piece)
                );
            }
            // Greedy: every piece but the last is filled to within one character.
            for piece in &pieces[..pieces.len() - 1] {
                assert!(encoded(piece) + 6 > MAX_OBSERVATION_BYTES);
            }
        }
    }
}
