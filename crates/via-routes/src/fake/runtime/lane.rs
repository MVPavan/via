//! The C2 driver lane of the fake route (adapter design §3.2, AD4, AD7,
//! AD9, decision H1): the retained terminal, the handshake, the steer
//! control, the interrupt acknowledgement, P7's tool-grace wait and the
//! persistent-connection profile emulated over per-turn processes.

use std::collections::BTreeSet;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use super::super::{
    FakeMessage, Handshake, STEER_ID, TerminalDetails, TerminalStatus, escape_json,
};
use super::{Failed, FakeRouteResult, Next, Serving, cleanup_deadline, protocol};
use crate::{
    ExitReport, OutboundMessage, RouteError, RouteFailure, SendOutcome, TurnNumber, WireCleanup,
};
use via_wire::WireMessages;

/// Reported tool items a turn tracks for cleanup (AD9); one more marks the
/// set incomplete, which keeps cleanup `Uncertain`.
const OPEN_TOOLS_MAX: usize = 1024;

/// What the C2 lane asks of one fake turn beyond S1's inputs.
pub struct Lane {
    /// The persistent-connection profile (decision H1): the turn's process
    /// stands in for a shared server. Its confirmed death is
    /// [`TurnCause::ServerLost`], a stdout closed by a live process is
    /// transport loss, the wall sends the interrupt instead of a force close,
    /// an interrupted terminal waits for reported tools up to `tool_grace`,
    /// and the emulated server is never reported force-stopped.
    pub persistent: bool,
    /// Features the instance must report in its handshake, which Route reads
    /// before the start; `None` on a profile with no handshake (AD7).
    pub handshake: Option<Vec<String>>,
    /// C1 P7's tool-grace window, used on the persistent profile.
    pub tool_grace: Duration,
    /// The driver's steer requests for this turn.
    pub steer: Option<mpsc::Receiver<SteerRequest>>,
}

impl Lane {
    /// S1's per-turn lane: no handshake, no steer.
    pub(super) fn legacy() -> Self {
        Self {
            persistent: false,
            handshake: None,
            tool_grace: Duration::ZERO,
            steer: None,
        }
    }
}

/// One steer input for the running turn; `reply` answers once the vendor
/// reported its delivery, or with why it was not delivered. A reply dropped
/// unanswered means the turn ended first.
pub struct SteerRequest {
    /// The steer text.
    pub text: String,
    /// The delivery answer.
    pub reply: oneshot::Sender<Result<(), SteerRefused>>,
}

/// Why Route did not deliver a steer input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteerRefused {
    /// The turn is not accepted yet, or already ended.
    NotActive,
    /// The input was not written whole.
    NotWritten,
}

/// The decoded vendor terminal, retained even when the turn then fails
/// (AD4).
#[derive(Clone, Debug)]
pub struct FakeTerminal {
    /// When Route decoded it.
    pub at: tokio::time::Instant,
    /// Vendor status.
    pub status: TerminalStatus,
    /// Vendor stop reason, verbatim.
    pub stop_reason: String,
    /// Vendor failure code.
    pub vendor_code: Option<String>,
    /// The C2 terminal fields.
    pub details: Box<TerminalDetails>,
}

/// A C2-lane route cause: S1's, or one only this lane reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnCause {
    /// An S1 route cause.
    Route(RouteError),
    /// Host confirmed the persistent connection's server died before any
    /// terminal (C1 §7.6 `server_lost`).
    ServerLost {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// The handshake lacked a feature VIA relies on; the start was not
    /// written (AD7 `handshake_refused`).
    HandshakeRefused {
        /// Affected turn.
        turn: TurnNumber,
    },
}

/// A failed C2-lane turn: its cause and the evidence Route holds, plus the
/// two facts Core's stop outcome needs (AD4 Core handoff).
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a distinct, independent fact of the evidence"
)]
pub struct TurnFailure {
    /// First cause.
    pub cause: TurnCause,
    /// Where an undecodable message was kept, or why not.
    pub undecoded: Option<String>,
    /// Host-confirmed vendor exit, when observed.
    pub exit: Option<ExitReport>,
    /// A vendor may have launched.
    pub launched: bool,
    /// Cleanup certainty, when Route established one.
    pub cleanup: Option<WireCleanup>,
    /// Host stopped the group while its vendor was live; never on the
    /// persistent profile's stops, which leave a shared server running.
    pub forced: bool,
    /// A Host journal write had an uncertain outcome: the daemon latches.
    pub journal_uncertain: bool,
    /// The vendor acknowledged Route's interrupt with an interrupted
    /// terminal within the cutoff.
    pub acknowledged: bool,
    /// The connection is a persistent server (the persistent profile).
    pub shared: bool,
}

/// One C2-lane turn's result (C2 §4.1): the retained terminal and handshake
/// on every outcome.
pub struct FakeTurn {
    /// The terminal decoded before any wall failure.
    pub terminal: Option<FakeTerminal>,
    /// The handshake, once read.
    pub handshake: Option<Handshake>,
    /// The vendor acknowledged Route's interrupt.
    pub acknowledged: bool,
    /// Process and cleanup facts, or the typed failure.
    pub outcome: Result<FakeRouteResult, TurnFailure>,
}

/// What Route learned in a turn besides its S1 result.
#[derive(Default)]
pub(super) struct Facts {
    pub(super) terminal: Option<FakeTerminal>,
    pub(super) handshake: Option<Handshake>,
    pub(super) refused: bool,
    pub(super) acknowledged: bool,
    pub(super) tools_settled: bool,
}

/// The lane's state inside [`Serving`].
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a distinct, independent fact of the turn"
)]
pub(super) struct LaneState {
    pub(super) persistent: bool,
    pub(super) handshake: Option<Vec<String>>,
    pub(super) tool_grace: Duration,
    pub(super) steer: Option<mpsc::Receiver<SteerRequest>>,
    /// The pending steer write, kept pinned while other waits run.
    pub(super) steer_write: Option<via_wire::PendingWrite>,
    /// The steer awaiting the vendor's delivery report.
    pub(super) steer_reply: Option<oneshot::Sender<Result<(), SteerRefused>>>,
    pub(super) accepted: bool,
    pub(super) facts: Facts,
    open_tools: BTreeSet<String>,
    tools_incomplete: bool,
    /// P7's bound after an interrupted terminal (persistent profile).
    pub(super) grace: Option<tokio::time::Instant>,
    pub(super) grace_expired: bool,
}

impl LaneState {
    pub(super) fn new(lane: Lane) -> Self {
        Self {
            persistent: lane.persistent,
            handshake: lane.handshake,
            tool_grace: lane.tool_grace,
            steer: lane.steer,
            steer_write: None,
            steer_reply: None,
            accepted: false,
            facts: Facts::default(),
            open_tools: BTreeSet::new(),
            tools_incomplete: false,
            grace: None,
            grace_expired: false,
        }
    }

    /// Every reported tool item ended (AD9).
    pub(super) fn tools_settled(&self) -> bool {
        self.open_tools.is_empty() && !self.tools_incomplete
    }

    /// The facts at the turn's end.
    pub(super) fn into_facts(self) -> Facts {
        let settled = self.tools_settled();
        Facts {
            tools_settled: settled,
            ..self.facts
        }
    }
}

impl Serving<'_> {
    /// Records the lane facts one decoded message carries: tool items, the
    /// acceptance, the interrupt acknowledgement and the steer delivery.
    pub(super) fn note(&mut self, message: &FakeMessage) -> Result<(), Failed> {
        let lane = &mut self.lane;
        match message {
            FakeMessage::Accepted { .. } => lane.accepted = true,
            FakeMessage::ToolStarted { tool_id, .. } => {
                if lane.open_tools.len() < OPEN_TOOLS_MAX {
                    lane.open_tools.insert(tool_id.clone());
                } else {
                    lane.tools_incomplete = true;
                }
            }
            FakeMessage::ToolEnded { tool_id, .. } => {
                lane.open_tools.remove(tool_id);
            }
            FakeMessage::Terminal { status, .. } => {
                // C2 §7 item 10: only an interrupted terminal after the
                // interrupt acknowledges it.
                if self.interrupted && *status == TerminalStatus::Interrupted {
                    lane.facts.acknowledged = true;
                }
            }
            FakeMessage::SteerDelivered { .. } => match lane.steer_reply.take() {
                Some(reply) => {
                    let _ = reply.send(Ok(()));
                }
                None => {
                    return Err(protocol(self.turn, "unsolicited fake steer delivery").into());
                }
            },
            FakeMessage::Text { .. }
            | FakeMessage::Usage { .. }
            | FakeMessage::InterruptAck { .. }
            | FakeMessage::Hello(_)
            | FakeMessage::Identity { .. }
            | FakeMessage::Denial { .. }
            | FakeMessage::Decline { .. }
            | FakeMessage::VendorClosed { .. }
            | FakeMessage::Unknown { .. } => {}
        }
        Ok(())
    }

    /// AD7: on a profile with a handshake, reads it before the start. A
    /// missing relied-on feature refuses the instance; nothing was sent.
    pub(super) async fn handshake(&mut self, messages: &mut WireMessages) -> Result<(), Failed> {
        let Some(required) = self.lane.handshake.take() else {
            return Ok(());
        };
        let turn = self.turn;
        let message = match self.next(messages).await? {
            Next::Message(message) => message,
            end @ (Next::Eof | Next::Unterminated) => return Err(self.ended(end).await),
        };
        let FakeMessage::Hello(handshake) = message.payload else {
            return Err(protocol(turn, "fake handshake expected").into());
        };
        let missing = required
            .iter()
            .any(|feature| !handshake.features.contains(feature));
        self.lane.facts.handshake = Some(handshake);
        if missing {
            self.lane.facts.refused = true;
            return Err(protocol(turn, "fake handshake refused").into());
        }
        Ok(())
    }

    /// The end of stdout before a terminal: a Host-confirmed exit is
    /// `ProcessExited` (the persistent profile's server loss), anything else
    /// as [`Self::exit_before_terminal`] decides. The persistent profile
    /// waits for the exit only for the cleanup allowance: a live process
    /// that closed stdout is transport loss.
    pub(super) async fn ended(&mut self, end: Next) -> Failed {
        let bounded = matches!(end, Next::Unterminated) || self.lane.persistent;
        let exit = match self.exit_before_terminal(bounded).await {
            Ok(exit) => exit,
            Err(failed) => return failed,
        };
        if let Err(failed) = self.after_terminal() {
            return failed;
        }
        Failed {
            cause: RouteError::ProcessExited { turn: self.turn },
            exit: Some(exit),
            close_by: None,
        }
    }

    /// Keeps the decoded terminal (AD4) and, on the persistent profile,
    /// arms P7's bound after an interrupted one (C1 P7, AD4).
    pub(super) fn retain(&mut self, terminal: &FakeTerminal) {
        if self.lane.persistent && terminal.status == TerminalStatus::Interrupted {
            let bound = terminal.at + self.lane.tool_grace;
            self.lane.grace = Some(bound.min(self.deadline.instant()));
        }
        self.lane.facts.terminal = Some(terminal.clone());
    }

    /// Enqueues the one interrupt (design §2 rule 3).
    pub(super) fn send_interrupt(&mut self) {
        if self.interrupted {
            return;
        }
        self.interrupted = true;
        let interrupt = format!(
            "{{\"type\":\"interrupt\",\"id\":2,\"vendor_turn_id\":\"fake-turn-{}\"}}\n",
            self.turn.get()
        );
        self.pending = Some(self.sender.write(
            OutboundMessage::Interrupt(interrupt.into_bytes()),
            self.deadline,
        ));
    }

    /// Starts one steer write, or answers why not.
    pub(super) fn on_steer(&mut self, request: SteerRequest) {
        if !self.lane.accepted || self.terminated {
            let _ = request.reply.send(Err(SteerRefused::NotActive));
            return;
        }
        let prefix = format!(
            r#"{{"type":"steer","id":{STEER_ID},"vendor_turn_id":"fake-turn-{}","text":""#,
            self.turn.get()
        );
        let steer = OutboundMessage::Start {
            prefix: prefix.into_bytes(),
            prompt: request.text,
            suffix: b"\"}\n".to_vec(),
            escape: escape_json,
        };
        self.lane.steer_write = Some(self.sender.write(steer, self.deadline));
        self.lane.steer_reply = Some(request.reply);
    }

    /// A finished steer write: one not written whole is answered at once.
    pub(super) fn steer_written(&mut self, outcome: &Result<SendOutcome, via_wire::WireError>) {
        self.lane.steer_write = None;
        if !matches!(outcome, Ok(SendOutcome::Written))
            && let Some(reply) = self.lane.steer_reply.take()
        {
            let _ = reply.send(Err(SteerRefused::NotWritten));
        }
    }

    /// AD4 wall expiry on the persistent profile: the cleanup step is the
    /// vendor's soft stop. Route sends the interrupt and reads until an
    /// interrupted terminal acknowledges it, or the cleanup bound passes.
    /// Messages read meanwhile are delivered, except that terminal, which
    /// is acknowledgement only: the turn already failed at the wall.
    pub(super) async fn soft_stop(&mut self, messages: &mut WireMessages) {
        self.deadline = cleanup_deadline();
        self.send_interrupt();
        while !self.lane.facts.acknowledged {
            let Ok(Next::Message(message)) = self.next(messages).await else {
                break;
            };
            if !matches!(message.payload, FakeMessage::Terminal { .. }) {
                self.held = Some(message);
            }
        }
        let _ = self.flush().await;
    }
}

/// Builds the C2-lane result from Route's S1 result and the lane's facts.
pub(super) fn turn_result(
    result: Result<FakeRouteResult, RouteFailure>,
    facts: Facts,
    persistent: bool,
) -> FakeTurn {
    let settled = if facts.tools_settled {
        WireCleanup::Quiescent
    } else {
        WireCleanup::Uncertain
    };
    let outcome = match result {
        Ok(mut result) => {
            if persistent {
                // AD9 server row: the reported tool items decide, and the
                // emulated server was never stopped.
                result.cleanup = settled;
                result.forced = false;
            }
            Ok(result)
        }
        Err(failure) => {
            let cause = match failure.cause {
                RouteError::Protocol { turn, .. } if facts.refused => {
                    TurnCause::HandshakeRefused { turn }
                }
                RouteError::ProcessExited { turn } if persistent => TurnCause::ServerLost { turn },
                cause @ (RouteError::Protocol { .. }
                | RouteError::TransportLost { .. }
                | RouteError::ProcessExited { .. }
                | RouteError::Overflow { .. }
                | RouteError::Store { .. }
                | RouteError::Stopped { .. }
                | RouteError::Deadline { .. }
                | RouteError::ForceStopped { .. }) => TurnCause::Route(cause),
            };
            // Server death and the daemon force keep Host's evidence. Any
            // other stop of the emulated server never stopped it: cleanup is
            // the reported tools' once the interrupt was acknowledged (AD9).
            let host_evidence = matches!(
                cause,
                TurnCause::ServerLost { .. } | TurnCause::Route(RouteError::ForceStopped { .. })
            );
            let (cleanup, forced) = if persistent && !host_evidence {
                let cleanup = if facts.acknowledged {
                    settled
                } else {
                    WireCleanup::Uncertain
                };
                (Some(cleanup), false)
            } else {
                (failure.cleanup, failure.forced)
            };
            Err(TurnFailure {
                cause,
                undecoded: failure.undecoded,
                exit: failure.exit,
                launched: failure.launched,
                cleanup,
                forced,
                journal_uncertain: failure.journal_uncertain,
                acknowledged: facts.acknowledged,
                shared: persistent,
            })
        }
    };
    FakeTurn {
        terminal: facts.terminal,
        handshake: facts.handshake,
        acknowledged: facts.acknowledged,
        outcome,
    }
}

/// Resolves on the next steer request; never without a receiver.
pub(super) async fn steer_request(
    receiver: Option<&mut mpsc::Receiver<SteerRequest>>,
) -> Option<SteerRequest> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}
