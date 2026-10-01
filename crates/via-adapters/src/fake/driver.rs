//! The fake's driver turn (C2 §2, §4, §4.1; adapter design AD3–AD9):
//! launches the turn's process through the C2 route lane, normalizes each
//! decoded message into session observations in decode order, and builds
//! the turn's one `TurnEnd`.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::FakeAdapter;
use crate::driver::{SessionDriver, TurnCx, TurnSpec, rejected};
use crate::harness::Harness;
use crate::observation::{
    Acceptance, ClassHint, CostReport, Decline, Denial, DenialKind, Identity, InstanceReport,
    Observation, ObservationItem, ObservationSink, ProgressMarks, SteerDelivery, StopReason,
    TurnEnd, TurnError, TurnEvidence, Undelivered, UsageSample, VendorTerminal,
};
use crate::plan::{Refusal, RefusalKind, TurnParams, VersionStatus};
use crate::runtime::{cleanup, event_stall};
use crate::{
    AcceptanceToken, ProcessOwner, RouteError, StartRejected, VendorTerminalStatus, VendorTurnId,
    final_text_pieces,
};
use via_routes::{
    FakeClassHint, FakeDenialKind, FakeMessage, FakeTerminal, FakeTurn, FakeUsage, Lane,
    RouteMessage, TerminalStatus, TurnCause, TurnFailure, TurnStart,
};

/// How the delivery after Route ended went.
enum Rest {
    /// Everything Route handed over reached the session channel.
    Delivered,
    /// A delivery failed: Core stalled or went away.
    Undelivered,
    /// The daemon force ended a delivery that had to wait.
    Forced,
}

/// A pending delivery, polled beside the route (Task 4 design §9).
type Delivery = Pin<Box<dyn Future<Output = Result<(), Undelivered>> + Send>>;

/// Runs one submitted turn (C2 §4.1): per-turn values are checked before
/// anything launches (AD18, C2 §7 item 13); then the turn's process runs
/// through Route while each decoded message is delivered to the session
/// channel. A delivery blocked for the stall bound drops the hop, so Route
/// fails the turn `overflow`, keeping the decoded terminal (AD4).
pub(crate) async fn run_turn(
    driver: &SessionDriver,
    adapter: &FakeAdapter,
    spec: TurnSpec,
    cx: TurnCx,
) -> TurnEnd {
    let route = Harness::Fake.route();
    let params = TurnParams {
        effort: spec.effort.clone(),
        bound: spec.bound.clone(),
        output_schema: spec.output_schema.is_some(),
        max_steps: spec.max_steps,
        vendor: spec.vendor.clone(),
    };
    if let Some(refusal) = adapter.check_turn(route, &params).into_iter().next() {
        return rejected(TurnError::Rejected(start_rejected(refusal)));
    }
    let TurnCx {
        turn,
        prepared,
        capacity,
        activity,
        wall,
        tool_grace,
        stop,
        force,
    } = cx;
    let (generation, capacity) = match driver.connect(prepared, capacity) {
        Ok(connection) => connection,
        Err(error) => return rejected(error),
    };
    let session_id = driver.spec.session_id.clone();
    let owner = ProcessOwner {
        session_id: session_id.clone(),
        turn,
    };
    let Ok(mut process) = adapter.process_spec(owner, &driver.spec.cwd) else {
        return rejected(TurnError::Unavailable);
    };
    // Per-turn profile: Host holds the slot for the group's life. The
    // persistent profile's slot stays with the driver (decision H1).
    process.capacity = capacity;
    let Ok(start) = TurnStart::new(session_id.as_str().to_owned(), turn, spec.prompt) else {
        return rejected(TurnError::Rejected(StartRejected::Protocol(
            "the fake start cannot be built".to_owned(),
        )));
    };
    let profile = adapter.profile();
    let persistent = profile.persistent;
    // Full: a second concurrent steer is refused `Busy`.
    let (steer, steer_lane) = mpsc::channel(1);
    driver.state().steer = Some(steer);
    // Route hands one message at a time: while it is full Route reads no
    // further message.
    let (hop, hop_rx) = mpsc::channel::<RouteMessage>(1);
    let lane = Lane {
        persistent,
        handshake: profile
            .handshake
            .as_ref()
            .map(|handshake| handshake.requires.clone()),
        tool_grace,
        steer: Some(steer_lane),
    };
    let mut normalizer = Normalizer {
        generation,
        confirmed: driver.spec.confirmed_vendor_session_id.clone(),
        vendor_closed: false,
    };
    let route_turn = driver
        .route
        .turn(process, start, hop, (wall, force.clone(), stop), lane);
    let (result, rest) = deliver_beside(
        route_turn,
        hop_rx,
        &mut normalizer,
        &driver.observations,
        &activity,
        force,
    )
    .await;
    driver.state().steer = None;
    let end = turn_end(adapter, turn, result, &rest, persistent);
    let lost = matches!(
        &end.outcome,
        Err(TurnError::Route(TurnFailure {
            cause: TurnCause::ServerLost { .. }
                | TurnCause::Route(RouteError::TransportLost { .. }),
            ..
        }))
    );
    if persistent && (lost || normalizer.vendor_closed) {
        driver.disconnect();
    }
    end
}

/// Polls `route` while delivering what it hands over, then delivers the
/// rest: data deliverable at once still goes under the daemon force.
async fn deliver_beside(
    route: impl Future<Output = FakeTurn>,
    hop_rx: mpsc::Receiver<RouteMessage>,
    normalizer: &mut Normalizer,
    sink: &ObservationSink,
    activity: &crate::TurnActivity,
    mut force: watch::Receiver<Option<tokio::time::Instant>>,
) -> (FakeTurn, Rest) {
    tokio::pin!(route);
    let stall = event_stall();
    let mut hop_rx = Some(hop_rx);
    let mut delivery: Option<Delivery> = None;
    let mut delivered = true;
    let result = loop {
        tokio::select! {
            biased;
            outcome = poll_delivery(delivery.as_mut()), if delivery.is_some() => {
                delivery = None;
                if outcome.is_err() {
                    // Route observes the closed hop as overflow, or as the
                    // force's stop under a force.
                    hop_rx = None;
                    delivered = false;
                }
            }
            message = recv(hop_rx.as_mut()), if delivery.is_none() && hop_rx.is_some() => {
                match message {
                    Some(message) => {
                        let at = tokio::time::Instant::now();
                        activity.record(at);
                        let items = normalizer.items(message, at);
                        delivery = Some(Box::pin(send_all(items, sink.clone(), stall)));
                    }
                    None => hop_rx = None,
                }
            }
            result = &mut route => break result,
        }
    };
    if !delivered {
        return (result, Rest::Undelivered);
    }
    let rest = async {
        if let Some(delivery) = delivery
            && delivery.await.is_err()
        {
            return Rest::Undelivered;
        }
        if let Some(receiver) = hop_rx.as_mut() {
            while let Ok(message) = receiver.try_recv() {
                let at = tokio::time::Instant::now();
                activity.record(at);
                let items = normalizer.items(message, at);
                if send_all(items, sink.clone(), stall).await.is_err() {
                    return Rest::Undelivered;
                }
            }
        }
        Rest::Delivered
    };
    let rest = tokio::select! {
        biased;
        rest = rest => rest,
        () = forced(&mut force) => Rest::Forced,
    };
    (result, rest)
}

/// The turn's one result (C2 §4.1). A Route failure is the first cause;
/// undelivered data fails a success `overflow`, and the daemon force that
/// ended the delivery `force_stopped`; either keeps Route's evidence.
fn turn_end(
    adapter: &FakeAdapter,
    number: crate::TurnNumber,
    turn: FakeTurn,
    rest: &Rest,
    persistent: bool,
) -> TurnEnd {
    let FakeTurn {
        terminal,
        handshake,
        acknowledged,
        outcome,
    } = turn;
    let instance = handshake.map(|handshake| {
        let checked = adapter.profile().handshake.as_ref().is_some_and(|decl| {
            handshake
                .vendor_version
                .as_ref()
                .is_some_and(|version| decl.checked.contains(version))
        });
        InstanceReport {
            vendor_version: handshake.vendor_version,
            version_status: if checked {
                VersionStatus::Tested
            } else {
                VersionStatus::Untested
            },
        }
    });
    let outcome = match outcome {
        Err(failure) => Err(TurnError::Route(failure)),
        Ok(result) => {
            let exit =
                (result.exit.code.is_some() || result.exit.signal.is_some()).then_some(result.exit);
            let cause = match rest {
                Rest::Delivered => None,
                Rest::Undelivered => Some(RouteError::Overflow { turn: number }),
                Rest::Forced => Some(RouteError::ForceStopped { turn: number }),
            };
            match cause {
                None => Ok(TurnEvidence {
                    exit,
                    cleanup: cleanup(result.cleanup),
                    journal_uncertain: result.journal_uncertain,
                }),
                Some(cause) => Err(TurnError::Route(TurnFailure {
                    cause: TurnCause::Route(cause),
                    undecoded: None,
                    exit,
                    launched: true,
                    cleanup: Some(result.cleanup),
                    forced: result.forced,
                    journal_uncertain: result.journal_uncertain,
                    acknowledged,
                    shared: persistent,
                })),
            }
        }
    };
    TurnEnd {
        terminal: terminal.map(vendor_terminal),
        instance,
        leftovers: None,
        outcome,
    }
}

/// Maps the decoded terminal to C2's (AD5, AD6, AD11): the vendor's stop
/// reason is kept verbatim beside its normalized one.
fn vendor_terminal(terminal: FakeTerminal) -> VendorTerminal {
    let details = *terminal.details;
    VendorTerminal {
        at: terminal.at,
        status: match terminal.status {
            TerminalStatus::Completed => VendorTerminalStatus::Completed,
            TerminalStatus::Interrupted => VendorTerminalStatus::Interrupted,
            TerminalStatus::Failed => VendorTerminalStatus::Failed,
        },
        stop_reason: stop_reason(&terminal.stop_reason),
        vendor_stop_reason: terminal.stop_reason,
        vendor_code: terminal.vendor_code,
        class_hint: details.class_hint.map(class_hint),
        detail: details.detail,
        structured_output: details.structured_output,
        steps: details.steps,
        usage: details.usage.map(usage),
        cost: details.cost.map(|cost| CostReport {
            usd: cost.usd,
            scope: cost.scope,
        }),
        vendor: details.vendor,
    }
}

/// The fake's stop reasons; any other is `Other`.
fn stop_reason(vendor: &str) -> StopReason {
    match vendor {
        "end_turn" => StopReason::EndTurn,
        "max_steps" => StopReason::MaxSteps,
        "budget" => StopReason::Budget,
        "refusal" => StopReason::Refusal,
        "interrupted" | "cancelled" => StopReason::Interrupted,
        "error" => StopReason::Error,
        _ => StopReason::Other,
    }
}

fn class_hint(hint: FakeClassHint) -> ClassHint {
    match hint {
        FakeClassHint::Auth => ClassHint::Auth,
        FakeClassHint::RateLimit => ClassHint::RateLimit,
        FakeClassHint::ContextExceeded => ClassHint::ContextExceeded,
        FakeClassHint::BudgetExceeded => ClassHint::BudgetExceeded,
        FakeClassHint::VendorError => ClassHint::VendorError,
        FakeClassHint::Protocol => ClassHint::Protocol,
        FakeClassHint::ResumeMismatch => ClassHint::ResumeMismatch,
    }
}

fn usage(sample: FakeUsage) -> UsageSample {
    UsageSample {
        key: sample.key,
        input: sample.input,
        cached_input: sample.cached_input,
        output: sample.output,
        reasoning_output: sample.reasoning_output,
        total: sample.total,
    }
}

/// A per-turn refusal as the definite rejection it is before submission.
fn start_rejected(refusal: Refusal) -> StartRejected {
    match refusal.kind {
        RefusalKind::BoundUnsupported => StartRejected::BoundUnsupported(refusal.message),
        RefusalKind::InvalidParam { field } => StartRejected::InvalidParam { field },
        RefusalKind::VendorOptionConflict => StartRejected::InvalidParam { field: "vendor" },
        RefusalKind::UnsupportedVerb
        | RefusalKind::HarnessUnavailable
        | RefusalKind::UnknownModel
        | RefusalKind::VersionRefused
        | RefusalKind::MissingCapability { .. } => StartRejected::Protocol(refusal.message),
    }
}

/// Turns decoded messages into observations (C2 §4).
struct Normalizer {
    /// The connection generation, named in identity confirmations.
    generation: u64,
    /// The session's confirmed vendor ID, which a new connection must match.
    confirmed: Option<String>,
    /// The vendor closed its session in this turn.
    vendor_closed: bool,
}

impl Normalizer {
    /// One message's observations: at most one, or the terminal's final
    /// text pieces. Unknown messages, the handshake and the interrupt
    /// acknowledgement move only the activity clock.
    fn items(&mut self, message: RouteMessage, at: tokio::time::Instant) -> Vec<ObservationItem> {
        let item = |vendor_turn: Option<String>, observation| ObservationItem {
            at,
            vendor_turn: vendor_turn.and_then(|id| VendorTurnId::try_from(id).ok()),
            observation,
        };
        // The terminal itself is retained in the turn's end (AD4); its
        // final text goes as pieces.
        if let FakeMessage::Terminal {
            vendor_turn_id,
            final_text,
            ..
        } = message.payload
        {
            return final_text_pieces(&final_text)
                .map(|piece| {
                    item(
                        Some(vendor_turn_id.clone()),
                        Observation::FinalText(piece.to_owned()),
                    )
                })
                .collect();
        }
        self.observation(message.payload)
            .map(|(vendor_turn, observation)| item(vendor_turn, observation))
            .into_iter()
            .collect()
    }

    /// A non-terminal message's observation and vendor turn, if any.
    fn observation(&mut self, payload: FakeMessage) -> Option<(Option<String>, Observation)> {
        let marks = |vendor_turn, marks| Some((Some(vendor_turn), Observation::Progress(marks)));
        match payload {
            FakeMessage::Accepted { vendor_turn_id } => {
                let accepted = Acceptance {
                    // Route admits exactly one acceptance per turn.
                    correlation: AcceptanceToken::FIRST,
                    vendor_turn_id: VendorTurnId::try_from(vendor_turn_id.clone()).ok(),
                };
                Some((Some(vendor_turn_id), Observation::Accepted(accepted)))
            }
            FakeMessage::Text { vendor_turn_id } => marks(
                vendor_turn_id,
                ProgressMarks {
                    model: true,
                    ..ProgressMarks::default()
                },
            ),
            FakeMessage::ToolStarted {
                vendor_turn_id,
                tool_id,
                name,
            } => marks(
                vendor_turn_id,
                ProgressMarks {
                    tools_started: vec![(tool_id, name)],
                    ..ProgressMarks::default()
                },
            ),
            FakeMessage::ToolEnded {
                vendor_turn_id,
                tool_id,
            } => marks(
                vendor_turn_id,
                ProgressMarks {
                    tools_ended: vec![tool_id],
                    ..ProgressMarks::default()
                },
            ),
            FakeMessage::Usage {
                vendor_turn_id,
                sample,
                ..
            } => marks(
                vendor_turn_id,
                ProgressMarks {
                    usage: Some(usage(sample)),
                    ..ProgressMarks::default()
                },
            ),
            FakeMessage::Identity {
                vendor_session_id,
                transcript,
            } => Some((None, self.identity(vendor_session_id, transcript))),
            FakeMessage::Denial {
                vendor_turn_id,
                kind,
                target,
                reason,
            } => Some((
                Some(vendor_turn_id),
                Observation::ActionDenied(Denial {
                    kind: denial_kind(kind),
                    target,
                    reason,
                }),
            )),
            FakeMessage::Decline {
                vendor_turn_id,
                vendor_method,
                summary,
                blocking,
            } => Some((
                Some(vendor_turn_id),
                Observation::RequestDeclined(Decline {
                    vendor_method,
                    summary,
                    blocking,
                }),
            )),
            FakeMessage::SteerDelivered { vendor_turn_id } => Some((
                Some(vendor_turn_id),
                Observation::SteerDelivered(SteerDelivery::Injected),
            )),
            FakeMessage::VendorClosed { reason } => {
                self.vendor_closed = true;
                Some((None, Observation::VendorClosed(reason)))
            }
            FakeMessage::Terminal { .. }
            | FakeMessage::Hello(_)
            | FakeMessage::InterruptAck { .. }
            | FakeMessage::Unknown { .. } => None,
        }
    }

    /// C2 §2 "Reopen": an identity that differs from the session's
    /// confirmed one is `resume.mismatch`, never a replacement.
    fn identity(&self, vendor_session_id: String, transcript: Option<String>) -> Observation {
        match &self.confirmed {
            Some(requested) if *requested != vendor_session_id => Observation::ResumeMismatch {
                requested: requested.clone(),
                returned: vendor_session_id,
            },
            _ => Observation::IdentityConfirmed(Identity {
                vendor_session_id,
                connection_id: format!("fake-{}", self.generation),
                transcript: transcript.map(PathBuf::from),
            }),
        }
    }
}

fn denial_kind(kind: FakeDenialKind) -> DenialKind {
    match kind {
        FakeDenialKind::FileWrite => DenialKind::FileWrite,
        FakeDenialKind::Command => DenialKind::Command,
        FakeDenialKind::Network => DenialKind::Network,
        FakeDenialKind::Other => DenialKind::Other,
    }
}

/// Sends each item in order.
async fn send_all(
    items: Vec<ObservationItem>,
    sink: ObservationSink,
    stall: Duration,
) -> Result<(), Undelivered> {
    for item in items {
        sink.send(item, stall).await?;
    }
    Ok(())
}

async fn poll_delivery(delivery: Option<&mut Delivery>) -> Result<(), Undelivered> {
    match delivery {
        Some(delivery) => delivery.await,
        None => std::future::pending().await,
    }
}

async fn recv(receiver: Option<&mut mpsc::Receiver<RouteMessage>>) -> Option<RouteMessage> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => None,
    }
}

/// Resolves once `force` is set; never when its sender is gone unset.
async fn forced(force: &mut watch::Receiver<Option<tokio::time::Instant>>) {
    if force.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}
