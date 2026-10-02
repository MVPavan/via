//! The C2 driver lane of the fake route (adapter design §3.2, AD4, AD7,
//! AD9, decision H1): the retained terminal, the handshake, the steer
//! control, the interrupt acknowledgement, P7's tool-grace wait and the
//! persistent-connection profile emulated over per-turn processes.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use super::super::{
    FakeMessage, Handshake, STEER_ID, TerminalDetails, TerminalStatus, escape_json,
    escaped_text_len, paired_vendor_turn,
};
use super::{Failed, FakeRouteResult, Next, Serving, protocol, transport};
use crate::{
    CloseRequest, Deadline, ExitReport, OutboundMessage, RouteError, RouteFailure, SendOutcome,
    StopCause, WireCleanup,
};
use via_wire::{CloseMode, WireCloseReport, WireMessages, WireSender};

/// Reported tool items a turn tracks for cleanup (AD9); one more marks the
/// set incomplete, which keeps cleanup `Uncertain`.
const OPEN_TOOLS_MAX: usize = 1024;

/// C2 §2 independent lanes: control commands admitted at once.
pub const CONTROL_COMMANDS: usize = 8;

/// C2 §2 independent lanes: the admitted commands' total encoded bytes.
pub const CONTROL_BYTES: usize = 64 * 1024;

/// A steer line's bytes besides its escaped text, for any turn number:
/// `{"type":"steer","id":3,"vendor_turn_id":"fake-turn-N","text":""}` and
/// its newline.
const STEER_OVERHEAD: usize = 96;

/// What the C2 lane asks of one fake turn beyond S1's inputs.
pub struct Lane {
    /// The persistent-connection profile (decision H1): the turn's process
    /// stands in for a shared server. Its confirmed death is
    /// [`RouteError::ServerLost`], a stdout closed by a live process is
    /// transport loss, the wall sends the interrupt instead of a force
    /// close, a stop order's `force_at` asks for no kill, a natural
    /// terminal ends the turn at once and an interrupted one when its
    /// reported tools ended or at `tool_grace` (C2 §4.1). The process is
    /// then retired apart from the turn.
    pub persistent: bool,
    /// Features the instance must report in its handshake, which Route reads
    /// before the start; `None` on a profile with no handshake (AD7).
    pub handshake: Option<Vec<String>>,
    /// C1 P7's tool-grace window, used on the persistent profile.
    pub tool_grace: Duration,
    /// The driver's steer requests for this turn.
    pub steer: Option<mpsc::Receiver<SteerRequest>>,
    /// The session's confirmed vendor session ID: every identity the turn
    /// reports must match it. With none, the turn's first identity is the
    /// one the rest must match (C2 §2 Reopen).
    pub identity: Option<String>,
    /// The turn's effort: an instance whose handshake reports its catalog
    /// must list it (AD18).
    pub effort: Option<String>,
}

/// One steer input for the running turn, admitted by [`SteerSender`];
/// `reply` answers once the input was written whole and the vendor reported
/// its delivery, or with why it was not delivered. A reply dropped
/// unanswered means the turn ended first.
pub struct SteerRequest {
    /// The steer text.
    pub text: String,
    /// The vendor turn the caller means, if it names one.
    pub expected_vendor_turn: Option<String>,
    /// The caller's token for the request, which Route pairs with the
    /// vendor's delivery report on the hop ([`RouteMessage::steer`]).
    pub token: u64,
    /// The delivery answer.
    pub reply: oneshot::Sender<Result<(), SteerRefused>>,
    /// Set once Route starts writing the input ([`SteerAnswer::write_started`]).
    written: Arc<AtomicBool>,
    /// The request's share of the control budget, returned when it is
    /// dropped.
    permit: ControlPermit,
}

/// One admitted command's share of the control budget (C2 §2): a command
/// slot and its encoded bytes, held until the command is resolved.
pub(super) struct ControlPermit {
    _command: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

/// Why Route did not deliver a steer input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteerRefused {
    /// The turn is not accepted yet, or already ended.
    NotActive,
    /// The input was not written whole.
    NotWritten,
    /// The input names another vendor turn than the running one.
    TurnMismatch,
    /// The control lane's eight commands or 64 KiB are taken (C2 §2).
    OverCapacity,
}

/// The admitting side of a turn's steer lane (C2 §2 independent lanes): at
/// most [`CONTROL_COMMANDS`] requests and [`CONTROL_BYTES`] encoded in
/// total are outstanding, queued or awaiting their answer; anything more is
/// refused before it is enqueued.
#[derive(Clone)]
pub struct SteerSender {
    sender: mpsc::Sender<SteerRequest>,
    commands: Arc<Semaphore>,
    budget: Arc<Semaphore>,
}

/// A turn's steer lane: the admitting sender and Route's receiver.
pub fn steer_lane() -> (SteerSender, mpsc::Receiver<SteerRequest>) {
    let (sender, receiver) = mpsc::channel(CONTROL_COMMANDS);
    let commands = Arc::new(Semaphore::new(CONTROL_COMMANDS));
    let budget = Arc::new(Semaphore::new(CONTROL_BYTES));
    (
        SteerSender {
            sender,
            commands,
            budget,
        },
        receiver,
    )
}

impl SteerSender {
    /// Admits one steer input with its caller's `token`, or refuses it
    /// at once.
    pub fn send(
        &self,
        text: String,
        expected_vendor_turn: Option<String>,
        token: u64,
    ) -> Result<SteerAnswer, SteerRefused> {
        let command = Arc::clone(&self.commands)
            .try_acquire_owned()
            .map_err(|_| SteerRefused::OverCapacity)?;
        let encoded = escaped_text_len(&text).saturating_add(STEER_OVERHEAD);
        // A size past `u32` is past the budget too: both refuse it.
        let bytes = u32::try_from(encoded)
            .ok()
            .and_then(|bytes| Arc::clone(&self.budget).try_acquire_many_owned(bytes).ok())
            .ok_or(SteerRefused::OverCapacity)?;
        let permit = ControlPermit {
            _command: command,
            _bytes: bytes,
        };
        let (reply, answer) = oneshot::channel();
        let written = Arc::new(AtomicBool::new(false));
        let request = SteerRequest {
            text,
            expected_vendor_turn,
            token,
            reply,
            written: Arc::clone(&written),
            permit,
        };
        match self.sender.try_send(request) {
            Ok(()) => Ok(SteerAnswer {
                reply: answer,
                written,
            }),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SteerRefused::OverCapacity),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SteerRefused::NotActive),
        }
    }
}

/// An admitted steer input's answer, as its caller holds it (critical r3
/// #1): Route's reply, and whether Route started writing the input, which
/// tells a caller whose turn ended unanswered whether the input may have
/// reached the vendor.
pub struct SteerAnswer {
    /// Route's reply; dropped unanswered when the turn ended first.
    pub reply: oneshot::Receiver<Result<(), SteerRefused>>,
    written: Arc<AtomicBool>,
}

impl SteerAnswer {
    /// Whether Route started writing the input: it may have been written,
    /// in part or whole. False means it never left the control lane.
    #[must_use]
    pub fn write_started(&self) -> bool {
        self.written.load(Ordering::Acquire)
    }
}

/// The turn's process facts once Route retired it: on the persistent
/// profile the emulated helper's housekeeping close, never a fact of the
/// logical turn (decision H1); otherwise the turn's own close.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Retirement {
    /// A process may have launched.
    pub launched: bool,
    /// Host-confirmed exit, when observed.
    pub exit: Option<ExitReport>,
    /// Cleanup certainty, when Route established one.
    pub cleanup: Option<WireCleanup>,
    /// Host stopped the group while its process was live.
    pub forced: bool,
    /// A Host journal write had an uncertain outcome.
    pub journal_uncertain: bool,
}

impl Retirement {
    /// The facts of Route's S1-shaped result.
    pub(super) fn of(result: &Result<FakeRouteResult, RouteFailure>) -> Self {
        match result {
            Ok(result) => Self {
                launched: true,
                exit: (result.exit.code.is_some() || result.exit.signal.is_some())
                    .then_some(result.exit),
                cleanup: Some(result.cleanup),
                forced: result.forced,
                journal_uncertain: result.journal_uncertain,
            },
            Err(failure) => Self {
                launched: failure.launched,
                exit: failure.exit,
                cleanup: failure.cleanup,
                forced: failure.forced,
                journal_uncertain: failure.journal_uncertain,
            },
        }
    }
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
    pub outcome: Result<FakeRouteResult, RouteFailure>,
    /// The persistent profile's emulated server stays after this turn: its
    /// helper process is retired apart from it (C2 §4.1, decision H1).
    pub server_kept: bool,
    /// An identity that differs from the session's, reported after the
    /// turn's terminal was retained, as `(requested, returned)`: the turn's
    /// result stands and only the connection fails (C2 §2 Reopen).
    pub late_mismatch: Option<(String, String)>,
}

/// What Route learned in a turn besides its S1 result.
#[derive(Default)]
pub(super) struct Facts {
    pub(super) terminal: Option<FakeTerminal>,
    pub(super) handshake: Option<Handshake>,
    /// The lane's own cause, which replaces the S1 protocol failure Route
    /// ended the turn with.
    pub(super) cause: Option<RouteError>,
    /// A mismatching identity after the terminal (C2 §2 Reopen).
    pub(super) late_mismatch: Option<(String, String)>,
    pub(super) acknowledged: bool,
    /// Every reported tool item ended (AD9).
    pub(super) tools_settled: bool,
    /// Where the logical turn goes, until it is sent.
    pub(super) logical: Option<oneshot::Sender<FakeTurn>>,
}

/// The one interrupt's write (design §2 rule 3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Interrupt {
    /// Not sent.
    NotSent,
    /// Enqueued; its write has not answered.
    Queued,
    /// Written whole.
    Written,
    /// Not written whole: the vendor was not asked.
    Failed,
}

/// C2 §7 item 10: an interrupt is acknowledged only by vendor evidence, an
/// interrupted terminal read in phase after it was sent, together with its
/// confirmed write.
pub(super) fn acknowledges(evidence: bool, interrupt: Interrupt) -> bool {
    evidence && interrupt == Interrupt::Written
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
    /// The steer awaiting its write and the vendor's delivery report, with
    /// its share of the control budget.
    pub(super) steer_reply: Option<(oneshot::Sender<Result<(), SteerRefused>>, ControlPermit)>,
    /// The token of the steer written last, which the vendor's delivery
    /// report is paired with on the hop (critical r1 #5).
    pub(super) steer_token: Option<u64>,
    /// The vendor reported the steer's delivery before its write answered.
    steer_evidence: bool,
    pub(super) accepted: bool,
    pub(super) facts: Facts,
    open_tools: BTreeSet<String>,
    tools_incomplete: bool,
    /// P7's bound after an interrupted terminal (persistent profile).
    pub(super) grace: Option<tokio::time::Instant>,
    pub(super) grace_expired: bool,
    /// The one interrupt's write.
    pub(super) interrupt: Interrupt,
    /// An interrupted terminal was read in phase after the interrupt was
    /// sent.
    ack_evidence: bool,
    identity: Option<String>,
    effort: Option<String>,
}

impl LaneState {
    pub(super) fn new(lane: Lane, logical: Option<oneshot::Sender<FakeTurn>>) -> Self {
        Self {
            persistent: lane.persistent,
            handshake: lane.handshake,
            tool_grace: lane.tool_grace,
            steer: lane.steer,
            steer_write: None,
            steer_reply: None,
            steer_token: None,
            steer_evidence: false,
            accepted: false,
            facts: Facts {
                logical,
                ..Facts::default()
            },
            open_tools: BTreeSet::new(),
            tools_incomplete: false,
            grace: None,
            grace_expired: false,
            interrupt: Interrupt::NotSent,
            ack_evidence: false,
            identity: lane.identity,
            effort: lane.effort,
        }
    }

    /// Every reported tool item ended (AD9).
    pub(super) fn tools_settled(&self) -> bool {
        self.open_tools.is_empty() && !self.tools_incomplete
    }

    /// The interrupt was acknowledged.
    pub(super) fn acknowledged(&self) -> bool {
        acknowledges(self.ack_evidence, self.interrupt)
    }

    /// The facts at the turn's end.
    pub(super) fn into_facts(self) -> Facts {
        let acknowledged = self.acknowledged();
        let tools_settled = self.tools_settled();
        Facts {
            acknowledged,
            tools_settled,
            ..self.facts
        }
    }
}

impl Serving<'_> {
    /// Records the lane facts one decoded, phase-valid message carries: tool
    /// items, the acceptance, the interrupt acknowledgement, the steer
    /// delivery and the vendor identity. Returns whether the message is
    /// handed over: a mismatching identity after the turn's terminal is
    /// recorded and dropped, so it confirms nothing (C2 §2 Reopen).
    pub(super) fn note(&mut self, message: &FakeMessage) -> Result<bool, Failed> {
        let turn = self.turn;
        let terminated = self.terminated;
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
                if *status == TerminalStatus::Interrupted && lane.interrupt != Interrupt::NotSent {
                    lane.ack_evidence = true;
                }
            }
            FakeMessage::SteerDelivered { .. } => {
                if lane.steer_reply.is_none() || lane.steer_evidence {
                    return Err(protocol(turn, "unsolicited fake steer delivery").into());
                }
                if lane.steer_write.is_some() {
                    // Answered once its write is confirmed.
                    lane.steer_evidence = true;
                } else {
                    self.resolve_steer(Ok(()));
                }
            }
            FakeMessage::Identity {
                vendor_session_id, ..
            } => match &lane.identity {
                // The turn's terminal stands; the connection fails.
                Some(requested) if requested != vendor_session_id && terminated => {
                    if lane.facts.late_mismatch.is_none() {
                        lane.facts.late_mismatch =
                            Some((requested.clone(), vendor_session_id.clone()));
                    }
                    return Ok(false);
                }
                Some(requested) if requested != vendor_session_id => {
                    lane.facts.cause = Some(RouteError::ResumeMismatch {
                        turn,
                        requested: requested.clone(),
                        returned: vendor_session_id.clone(),
                    });
                    return Err(protocol(turn, "fake identity differs from the session's").into());
                }
                Some(_) => {}
                None => lane.identity = Some(vendor_session_id.clone()),
            },
            FakeMessage::Text { .. }
            | FakeMessage::Usage { .. }
            | FakeMessage::InterruptAck { .. }
            | FakeMessage::Hello(_)
            | FakeMessage::Denial { .. }
            | FakeMessage::Decline { .. }
            | FakeMessage::VendorClosed { .. }
            | FakeMessage::Unknown { .. } => {}
        }
        Ok(true)
    }

    /// AD7: on a profile with a handshake, reads it before the start. A
    /// missing relied-on feature refuses the instance, and a catalog that
    /// lacks the turn's effort rejects the turn (AD18); nothing was sent.
    /// An accepted handshake is forwarded on the hop.
    pub(super) async fn handshake(&mut self, messages: &mut WireMessages) -> Result<(), Failed> {
        let Some(required) = self.lane.handshake.take() else {
            return Ok(());
        };
        let turn = self.turn;
        let message = match self.next(messages).await? {
            Next::Message(message) => message,
            end @ (Next::Eof | Next::Unterminated) => return Err(self.ended(end).await),
        };
        let FakeMessage::Hello(handshake) = &message.payload else {
            return Err(protocol(turn, "fake handshake expected").into());
        };
        let missing = required
            .iter()
            .any(|feature| !handshake.features.contains(feature));
        let uncatalogued = match (&self.lane.effort, &handshake.efforts) {
            (Some(effort), Some(efforts)) => !efforts.contains(effort),
            _ => false,
        };
        self.lane.facts.handshake = Some(handshake.clone());
        if missing {
            self.lane.facts.cause = Some(RouteError::HandshakeRefused { turn });
            return Err(protocol(turn, "fake handshake refused").into());
        }
        if uncatalogued {
            self.lane.facts.cause = Some(RouteError::InvalidParam {
                turn,
                field: "effort",
            });
            return Err(protocol(turn, "fake effort outside the instance catalog").into());
        }
        // C2 §4 `turn.accepted` (Sol r1 #13): an accepted handshake goes on
        // the hop too, ahead of everything the turn reports, so the Adapter
        // knows the instance before its acceptance.
        self.held = Some(message);
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
        if self.lane.interrupt != Interrupt::NotSent {
            return;
        }
        self.lane.interrupt = Interrupt::Queued;
        let interrupt = format!(
            "{{\"type\":\"interrupt\",\"id\":2,\"vendor_turn_id\":\"fake-turn-{}\"}}\n",
            self.turn.get()
        );
        self.pending = Some(self.sender.write(
            OutboundMessage::Interrupt(interrupt.into_bytes()),
            self.deadline,
        ));
    }

    /// The interrupt's write answered. One not written whole asked the
    /// vendor nothing: before the terminal, a stop order's force rule
    /// applies at once and the wall's soft stop ends as transport loss.
    pub(super) fn interrupt_written(
        &mut self,
        outcome: &Result<SendOutcome, via_wire::WireError>,
    ) -> Result<(), Failed> {
        self.pending = None;
        if matches!(outcome, Ok(SendOutcome::Written)) {
            self.lane.interrupt = Interrupt::Written;
            return Ok(());
        }
        self.lane.interrupt = Interrupt::Failed;
        if self.terminated {
            return Ok(());
        }
        let close_by = self
            .signals
            .stop
            .borrow()
            .as_ref()
            .map(|order| order.close_by);
        Err(match close_by {
            Some(close_by) => Failed::stopped(self.turn, close_by),
            None => transport(self.turn).into(),
        })
    }

    /// Serves until the interrupt's write answered.
    async fn settle_interrupt(&mut self) -> Result<(), Failed> {
        let mut never = std::pin::pin!(std::future::pending::<()>());
        while self.lane.interrupt == Interrupt::Queued {
            self.serve_once(never.as_mut()).await?;
        }
        Ok(())
    }

    /// Starts one steer write, or answers why not.
    pub(super) fn on_steer(&mut self, request: SteerRequest) {
        let SteerRequest {
            text,
            expected_vendor_turn,
            token,
            reply,
            written,
            permit,
        } = request;
        let refused = if !self.lane.accepted || self.terminated {
            Some(SteerRefused::NotActive)
        } else if expected_vendor_turn
            .as_deref()
            .is_some_and(|expected| !paired_vendor_turn(expected, self.turn))
        {
            Some(SteerRefused::TurnMismatch)
        } else {
            None
        };
        if let Some(refused) = refused {
            // The steer caller went away: nobody waits for the answer.
            let _ = reply.send(Err(refused));
            return;
        }
        let prefix = format!(
            r#"{{"type":"steer","id":{STEER_ID},"vendor_turn_id":"fake-turn-{}","text":""#,
            self.turn.get()
        );
        let steer = OutboundMessage::Start {
            prefix: prefix.into_bytes(),
            prompt: text,
            suffix: b"\"}\n".to_vec(),
            escape: escape_json,
        };
        written.store(true, Ordering::Release);
        self.lane.steer_write = Some(self.sender.write(steer, self.deadline));
        self.lane.steer_reply = Some((reply, permit));
        self.lane.steer_token = Some(token);
    }

    /// A finished steer write: one not written whole is answered at once,
    /// one written is answered once the vendor reported its delivery.
    pub(super) fn steer_written(&mut self, outcome: &Result<SendOutcome, via_wire::WireError>) {
        self.lane.steer_write = None;
        if !matches!(outcome, Ok(SendOutcome::Written)) {
            self.resolve_steer(Err(SteerRefused::NotWritten));
        } else if self.lane.steer_evidence {
            self.resolve_steer(Ok(()));
        }
    }

    /// Answers the pending steer and returns its control budget.
    fn resolve_steer(&mut self, answer: Result<(), SteerRefused>) {
        if let Some((reply, permit)) = self.lane.steer_reply.take() {
            // The steer caller went away: nobody waits for the answer.
            let _ = reply.send(answer);
            drop(permit);
        }
        self.lane.steer_evidence = false;
    }

    /// AD4 wall expiry on the persistent profile: the cleanup step is the
    /// vendor's soft stop. Route sends the interrupt and reads, phase
    /// checked, until an interrupted terminal acknowledges it or the
    /// `cutoff` (the wall plus 3 s) passes. Messages read meanwhile are
    /// delivered, except a terminal, which is acknowledgement only: the
    /// turn already failed at the wall.
    pub(super) async fn soft_stop(&mut self, messages: &mut WireMessages, cutoff: Deadline) {
        self.deadline = cutoff;
        self.send_interrupt();
        while !self.lane.acknowledged() {
            let Ok(Next::Message(message)) = self.next(messages).await else {
                break;
            };
            if matches!(message.payload, FakeMessage::Terminal { .. }) {
                // Its write answers in order before the cutoff, or the
                // turn's failure stands unacknowledged.
                let _settled = self.settle_interrupt().await;
                break;
            }
            self.held = Some(message);
        }
        // What is not on the hop by the cutoff goes with the turn, which
        // already failed at the wall.
        let _undelivered = self.flush().await;
    }

    /// C2 §4.1 (persistent profile): the logical turn after its terminal.
    /// A natural terminal ends it at once; an interrupted one when every
    /// reported tool ended or at P7's bound, which the helper's end of
    /// stdout does not shorten. Messages read meanwhile, and the held one,
    /// are delivered by the wall plus 3 s. Returns the turn's cleanup.
    pub(super) async fn persistent_end(
        &mut self,
        messages: &mut WireMessages,
        terminal: &FakeTerminal,
    ) -> Result<WireCleanup, Failed> {
        let interrupted = terminal.status == TerminalStatus::Interrupted;
        if interrupted {
            self.await_tools(messages).await?;
        }
        let cutoff = Deadline::at(self.deadline.instant() + super::CLEANUP_ALLOWANCE);
        self.deliver_held(cutoff).await.map_err(Failed::from)?;
        Ok(if !interrupted || self.lane.tools_settled() {
            WireCleanup::Quiescent
        } else {
            WireCleanup::Uncertain
        })
    }

    /// Reads until every reported tool ended or P7's bound passed.
    async fn await_tools(&mut self, messages: &mut WireMessages) -> Result<(), Failed> {
        let Some(bound) = self.lane.grace else {
            return Ok(());
        };
        let mut ended = false;
        while !self.lane.tools_settled() {
            let step = if ended {
                self.serve(tokio::time::sleep_until(bound))
                    .await
                    .map(|()| None)
            } else {
                self.next(messages).await.map(Some)
            };
            match step {
                Ok(Some(Next::Message(message))) => self.held = Some(message),
                // The helper's stdout ended: its reported tools still count
                // until they end or the bound passes (C1 P7).
                Ok(Some(Next::Eof | Next::Unterminated)) => ended = true,
                Ok(None) => break,
                // The bound is at or before the wall.
                Err(failed)
                    if self.lane.grace_expired
                        || matches!(failed.cause, RouteError::Deadline { .. }) =>
                {
                    break;
                }
                Err(failed) => return Err(failed),
            }
        }
        Ok(())
    }

    /// Whether a failure leaves the persistent profile's emulated server
    /// running: a stop order or the wall after submission, with no daemon
    /// force (C2 §4.1).
    pub(super) fn keeps_server(&self, cause: &RouteError) -> bool {
        self.lane.persistent
            && self.submitted
            && self.signals.force.borrow().is_none()
            && matches!(
                cause,
                RouteError::Stopped { .. } | RouteError::Deadline { .. }
            )
    }

    /// The logical failure of a turn whose server stays: no kill, no exit;
    /// cleanup is the reported tools' once acknowledged, else `Uncertain`.
    pub(super) fn kept_failure(&self, cause: RouteError) -> RouteFailure {
        let acknowledged = self.lane.acknowledged();
        RouteFailure {
            cause,
            undecoded: None,
            exit: None,
            launched: true,
            cleanup: Some(if acknowledged && self.lane.tools_settled() {
                WireCleanup::Quiescent
            } else {
                WireCleanup::Uncertain
            }),
            forced: false,
            journal_uncertain: false,
            acknowledged,
            shared: true,
        }
    }

    /// Sends the logical turn, once.
    pub(super) fn send_logical(&mut self, outcome: Result<FakeRouteResult, RouteFailure>) {
        if let Some(logical) = self.lane.facts.logical.take() {
            let turn = FakeTurn {
                terminal: self.lane.facts.terminal.clone(),
                handshake: self.lane.facts.handshake.clone(),
                acknowledged: self.lane.acknowledged(),
                outcome,
                server_kept: true,
                late_mismatch: self.lane.facts.late_mismatch.clone(),
            };
            // The driver's turn was dropped: the retirement that follows is
            // the cleanup it still owns.
            let _unread = logical.send(turn);
        }
    }

    /// Retires the persistent profile's helper apart from the logical turn
    /// (decision H1): input closed, then Host's close under `by`, forced at
    /// once for a session close, then the drain.
    pub(super) async fn retire(
        &mut self,
        sender: &WireSender,
        messages: WireMessages,
        by: Deadline,
    ) -> WireCloseReport {
        let closing = self
            .signals
            .stop
            .borrow()
            .as_ref()
            .is_some_and(|order| order.cause == StopCause::Close);
        let mode = if closing {
            CloseMode::Force
        } else {
            CloseMode::Graceful
        };
        // A half-close that failed leaves Host's close below to stop it.
        let _half_closed = sender.close_input(by).await;
        let report = sender.close(CloseRequest { mode, deadline: by }).await;
        messages.finish(by).await;
        report
    }
}

/// Builds the C2-lane result from Route's S1 result and the lane's facts.
/// On the persistent profile a failure reports the logical connection
/// (decision H1): only the server's death and the daemon force, which stop
/// the server, carry the helper's Host facts.
/// Any other launched failure has no exit and no force, and its cleanup is
/// the reported tool items' (AD9 server-route row); the helper's facts are
/// its retirement's.
pub(super) fn turn_result(
    result: Result<FakeRouteResult, RouteFailure>,
    facts: Facts,
    persistent: bool,
) -> FakeTurn {
    let Facts {
        terminal,
        handshake,
        cause: lane_cause,
        late_mismatch,
        acknowledged,
        tools_settled,
        ..
    } = facts;
    let outcome = match result {
        Ok(result) => Ok(result),
        Err(failure) => {
            let cause = match failure.cause {
                RouteError::Protocol { turn, detail } => {
                    lane_cause.unwrap_or(RouteError::Protocol { turn, detail })
                }
                RouteError::ProcessExited { turn } if persistent => RouteError::ServerLost { turn },
                cause @ (RouteError::TransportLost { .. }
                | RouteError::ProcessExited { .. }
                | RouteError::Overflow { .. }
                | RouteError::Store { .. }
                | RouteError::Stopped { .. }
                | RouteError::Deadline { .. }
                | RouteError::ForceStopped { .. }
                | RouteError::ServerLost { .. }
                | RouteError::HandshakeRefused { .. }
                | RouteError::InvalidParam { .. }
                | RouteError::ResumeMismatch { .. }) => cause,
            };
            // The server's loss and the daemon force end the server itself
            // (Host's own lifecycle): their Host facts are the connection's.
            let logical = persistent
                && failure.launched
                && !matches!(
                    cause,
                    RouteError::ServerLost { .. } | RouteError::ForceStopped { .. }
                );
            let (exit, cleanup, forced) = if logical {
                let cleanup = if tools_settled {
                    WireCleanup::Quiescent
                } else {
                    WireCleanup::Uncertain
                };
                (None, Some(cleanup), false)
            } else {
                (failure.exit, failure.cleanup, failure.forced)
            };
            Err(RouteFailure {
                cause,
                undecoded: failure.undecoded,
                exit,
                launched: failure.launched,
                cleanup,
                forced,
                journal_uncertain: failure.journal_uncertain,
                acknowledged,
                shared: persistent,
            })
        }
    };
    FakeTurn {
        terminal,
        handshake,
        acknowledged,
        outcome,
        server_kept: false,
        late_mismatch,
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
