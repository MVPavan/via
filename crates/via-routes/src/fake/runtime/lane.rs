//! The C2 driver lane of the fake route (adapter design §3.2, AD4, AD7,
//! AD9, decision H1): the retained terminal, the handshake, the steer
//! control, the interrupt acknowledgement, P7's tool-grace wait and the
//! persistent-connection profile emulated over per-turn processes.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use super::super::{
    FakeMessage, Handshake, STEER_ID, TerminalDetails, TerminalStatus, escape_json,
    escaped_text_len, paired_vendor_turn,
};
use super::{FakeRouteResult, Phase, sleep_until_set};
use crate::private::{
    CLEANUP_ALLOWANCE, Failed, Interrupt, Next, Serving, pending, protocol, wire_cause,
};
use crate::steer::{ControlPermit, SteerProgress, SteerRefused, SteerRequest};
use crate::{
    CloseRequest, Deadline, ExitReport, OutboundMessage, Retirement, RouteError, RouteFailure,
    SendOutcome, StopCause, TurnNumber, WireCleanup,
};
use via_wire::{CloseMode, WireCloseReport, WireError, WireMessages, WireSender, WriteBounds};

/// Reported tool items a turn tracks for cleanup (AD9); one more marks the
/// set incomplete, which keeps cleanup `Uncertain`.
const OPEN_TOOLS_MAX: usize = 1024;

/// A steer line's bytes besides its escaped text, for any turn number:
/// `{"type":"steer","id":3,"vendor_turn_id":"fake-turn-N","text":""}` and
/// its newline.
const STEER_OVERHEAD: usize = 96;

/// The encoded bytes of a fake steer line carrying `text`, for the steer
/// lane's budget.
pub(super) fn steer_line_len(text: &str) -> usize {
    escaped_text_len(text).saturating_add(STEER_OVERHEAD)
}

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
    /// Where the persistent profile's retirement streams what its helper
    /// reports meanwhile ([`FakeRetired`]); `None` reads nothing.
    pub retired: Option<FakeRetired>,
}

/// Where the persistent profile's retirement sends what its helper
/// reports while it is retired (C2 §4.1 late observations, §4
/// `turn.late_terminal`).
pub struct FakeRetired {
    /// Each forwarded message in decode order, one at a time, as the
    /// turn's own hop takes them: Route reads no further message while it
    /// is full. A closed receiver ends the forwarding.
    pub items: mpsc::Sender<FakeRetiredItem>,
    /// The failure that ended the reading: a message that does not decode,
    /// kept in `undecoded.bin` as the turn's own reader keeps one, one out
    /// of the connection's protocol phase, or a failed read. The
    /// connection failed.
    pub failure: oneshot::Sender<RouteError>,
    /// The retirement's facts, sent as soon as Host's close of the helper
    /// ended, whatever the reading is doing (fix r3 #3).
    pub cleaned: oneshot::Sender<Retirement>,
}

/// One message [`FakeRetired`] forwards.
pub enum FakeRetiredItem {
    /// A durable message, to be normalized as the turn's own are.
    Durable(super::super::RouteMessage),
    /// The terminal of a logical turn that retained none.
    Terminal(FakeLateTerminal),
}

/// A terminal the persistent profile's helper reported while it was
/// retired, after its logical turn ended with no terminal (C2 §4
/// `turn.late_terminal`).
pub struct FakeLateTerminal {
    /// The vendor turn it names, the turn's own.
    pub vendor_turn_id: String,
    /// The decoded terminal.
    pub terminal: FakeTerminal,
}

/// The steer awaiting its write and the vendor's delivery report.
pub(super) struct SteerPending {
    reply: oneshot::Sender<Result<(), SteerRefused>>,
    progress: Arc<SteerProgress>,
    /// Its share of the control budget.
    _permit: ControlPermit,
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
pub(crate) struct LaneState {
    pub(super) persistent: bool,
    /// The connection's protocol phase; every read message is checked
    /// against it before any fact is recorded.
    pub(super) phase: Phase,
    pub(super) handshake: Option<Vec<String>>,
    pub(super) tool_grace: Duration,
    pub(super) steer: Option<mpsc::Receiver<SteerRequest>>,
    /// The pending steer write, kept pinned while other waits run.
    pub(super) steer_write: Option<via_wire::PendingWrite>,
    /// The steer awaiting its write and the vendor's delivery report.
    pub(super) steer_reply: Option<SteerPending>,
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
    /// An interrupted terminal was read in phase after the interrupt was
    /// sent.
    ack_evidence: bool,
    identity: Option<String>,
    effort: Option<String>,
    /// Where the retirement's messages go ([`Lane::retired`]).
    retired: Option<FakeRetired>,
}

impl LaneState {
    pub(super) fn new(lane: Lane, logical: Option<oneshot::Sender<FakeTurn>>) -> Self {
        Self {
            persistent: lane.persistent,
            phase: Phase::Opening,
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
            ack_evidence: false,
            identity: lane.identity,
            effort: lane.effort,
            retired: lane.retired,
        }
    }

    /// Every reported tool item ended (AD9).
    pub(super) fn tools_settled(&self) -> bool {
        self.open_tools.is_empty() && !self.tools_incomplete
    }

    /// The interrupt, in its final state `interrupt`, was acknowledged.
    fn acknowledged(&self, interrupt: Interrupt) -> bool {
        acknowledges(self.ack_evidence, interrupt)
    }

    /// The facts at the turn's end, the interrupt in its final state.
    pub(super) fn into_facts(self, interrupt: Interrupt) -> Facts {
        let acknowledged = self.acknowledged(interrupt);
        let tools_settled = self.tools_settled();
        Facts {
            acknowledged,
            tools_settled,
            ..self.facts
        }
    }
}

impl LaneState {
    /// Whether [`Self::next_event`] may resolve now: P7's bound, a steer
    /// write, or a steer request while none is pending.
    pub(super) fn has_event(&self) -> bool {
        (self.grace.is_some() && !self.tools_settled())
            || self.steer_write.is_some()
            || self.steer_request_open()
    }

    /// The lane takes a steer request: none is written or awaiting its
    /// report.
    fn steer_request_open(&self) -> bool {
        self.steer.is_some() && self.steer_reply.is_none() && self.steer_write.is_none()
    }

    /// The lane's next control event, biased: (1) C1 P7's bound, once a
    /// reported tool outlived the window (persistent profile); (2) the
    /// pending steer write; (3) a steer request. Cancel-safe.
    pub(super) async fn next_event(&mut self) -> LaneEvent {
        let grace = self.grace.is_some() && !self.tools_settled();
        let writing = self.steer_write.is_some();
        let requesting = self.steer_request_open();
        tokio::select! {
            biased;
            () = sleep_until_set(self.grace), if grace => LaneEvent::GraceExpired,
            written = pending(self.steer_write.as_mut()), if writing => {
                LaneEvent::SteerWritten(written)
            }
            request = steer_request(self.steer.as_mut()), if requesting => {
                LaneEvent::SteerRequest(request)
            }
            else => std::future::pending().await,
        }
    }
}

/// An event of the fake lane's own controls.
pub(crate) enum LaneEvent {
    /// C1 P7: a reported tool outlived the window.
    GraceExpired,
    /// The pending steer write answered.
    SteerWritten(Result<SendOutcome, WireError>),
    /// A steer request, or `None` once the driver's lane closed.
    SteerRequest(Option<SteerRequest>),
}

/// A failure whose emulated server stays (persistent profile).
pub(crate) struct KeepServer;

impl Serving<'_, LaneState> {
    /// The interrupt was acknowledged.
    fn acknowledged(&self) -> bool {
        self.lane.acknowledged(self.interrupt)
    }

    /// Acts on a lane event.
    pub(super) fn on_event(&mut self, event: LaneEvent) -> Result<(), Failed> {
        match event {
            LaneEvent::GraceExpired => {
                self.lane.grace_expired = true;
                Err(RouteError::Deadline { turn: self.turn }.into())
            }
            LaneEvent::SteerWritten(written) => {
                self.steer_written(&written);
                Ok(())
            }
            LaneEvent::SteerRequest(Some(request)) => {
                self.on_steer(request);
                Ok(())
            }
            LaneEvent::SteerRequest(None) => {
                self.lane.steer = None;
                Ok(())
            }
        }
    }

    /// Records the lane facts one decoded, phase-valid message carries: tool
    /// items, the acceptance, the interrupt acknowledgement, the steer
    /// delivery and the vendor identity. Returns whether the message is
    /// handed over: a mismatching identity after the turn's terminal is
    /// recorded and dropped, so it confirms nothing (C2 §2 Reopen).
    pub(super) fn note(&mut self, message: &FakeMessage) -> Result<bool, Failed> {
        let turn = self.turn;
        let terminated = self.terminated;
        let interrupted = self.interrupt != Interrupt::NotSent;
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
                if *status == TerminalStatus::Interrupted && interrupted {
                    lane.ack_evidence = true;
                }
            }
            FakeMessage::SteerDelivered { .. } => {
                let Some(pending) = lane.steer_reply.as_ref().filter(|_| !lane.steer_evidence)
                else {
                    return Err(protocol(turn, "unsolicited fake steer delivery").into());
                };
                // Critical r5 #1: established before the report is handed
                // over, whatever the write's answer.
                pending.progress.mark_acknowledged();
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
        let FakeMessage::Hello(handshake) = &message.0.item.payload else {
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
            self.lane.facts.cause = Some(RouteError::HandshakeRefused { turn, detail: None });
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
        self.hold(message);
        Ok(())
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

    /// Starts one steer write, or answers why not.
    pub(super) fn on_steer(&mut self, request: SteerRequest) {
        let SteerRequest {
            text,
            expected_vendor_turn,
            token,
            reply,
            progress,
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
        progress.mark_write_started();
        self.lane.steer_write = Some(self.sender.write(steer, WriteBounds::CutAt(self.deadline)));
        self.lane.steer_reply = Some(SteerPending {
            reply,
            progress,
            _permit: permit,
        });
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
        if let Some(pending) = self.lane.steer_reply.take() {
            // The steer caller went away: nobody waits for the answer. The
            // control budget returns with `pending`.
            let _ = pending.reply.send(answer);
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
        while !self.acknowledged() {
            let Ok(Next::Message(message)) = self.next(messages).await else {
                break;
            };
            if matches!(message.0.item.payload, FakeMessage::Terminal { .. }) {
                // Its write answers in order before the cutoff, or the
                // turn's failure stands unacknowledged.
                let _settled = self.settle_interrupt().await;
                break;
            }
            self.hold(message);
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
        let cutoff = Deadline::at(self.deadline.instant() + CLEANUP_ALLOWANCE);
        self.deliver_held(cutoff, None)
            .await
            .map_err(Failed::from)?;
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
                Ok(Some(Next::Message(message))) => self.hold(message),
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
        let acknowledged = self.acknowledged();
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
            launch: None,
        }
    }

    /// Sends the logical turn, once.
    pub(super) fn send_logical(&mut self, outcome: Result<FakeRouteResult, RouteFailure>) {
        if let Some(logical) = self.lane.facts.logical.take() {
            let turn = FakeTurn {
                terminal: self.lane.facts.terminal.clone(),
                handshake: self.lane.facts.handshake.clone(),
                acknowledged: self.acknowledged(),
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
    /// once for a session close, then the drain. Meanwhile the helper's
    /// output is read ([`read_retired`]) and its messages go on the lane's
    /// `retired`. The retirement's facts, with the `exit` the turn saw, go
    /// there as soon as Host's close ended, apart from the reading.
    pub(super) async fn retire(
        &mut self,
        sender: &WireSender,
        mut messages: WireMessages,
        (exit, by): (Option<ExitReport>, Deadline),
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
        // Test builds: the logical turn ended and its helper's retirement
        // begins.
        #[cfg(feature = "test-failpoints")]
        let _ = via_wire::failpoint::hit_async("routes.fake.retiring").await;
        // A half-close that failed leaves Host's close below to stop it.
        let _half_closed = sender.close_input(by).await;
        let (reader, cleaned) = match self.lane.retired.take() {
            Some(FakeRetired {
                items,
                failure,
                cleaned,
            }) => (Some((items, failure)), Some(cleaned)),
            None => (None, None),
        };
        let interrupted = self.interrupt != Interrupt::NotSent;
        let reading = (self.turn, &mut self.lane.phase, interrupted);
        let close = async {
            let report = sender.close(CloseRequest { mode, deadline: by }).await;
            if let Some(cleaned) = cleaned {
                // The driver's turn task is gone: nobody takes the facts.
                let _unread = cleaned.send(Retirement::closed(exit, &report));
            }
            report
        };
        let read = async {
            if let Some(reader) = reader {
                read_retired(&mut messages, sender, reading, reader, by).await;
            }
        };
        let (report, ()) = tokio::join!(close, read);
        messages.finish(by).await;
        report
    }
}

/// Reads a retired helper's output for `turn` in decode order until it
/// ends, a failure, `by`, or the driver no longer takes what it forwards
/// (its lane closed). Each message is checked against the connection's
/// `phase` as the turn's own are; the durable ones (denials, declines)
/// and the first terminal, when the logical turn retained none (the only
/// one the phase admits), go on `items`; everything else is
/// dropped, as the logical turn already ended. A message that does not
/// decode is kept in `undecoded.bin`; it, a phase violation and a failed
/// read end the reading with their cause on `failure`. A stop
/// order's wake, the daemon force and `by` end it quietly.
async fn read_retired(
    messages: &mut WireMessages,
    sender: &WireSender,
    (turn, phase, interrupted): (TurnNumber, &mut Phase, bool),
    (items, failure): (mpsc::Sender<FakeRetiredItem>, oneshot::Sender<RouteError>),
    by: Deadline,
) {
    let read = async {
        loop {
            let message = match messages.next_message().await {
                Ok(Some(message)) => message,
                Err(via_wire::WireError::Woken) => continue,
                Ok(None) | Err(via_wire::WireError::Cancelled | via_wire::WireError::Deadline) => {
                    return None;
                }
                Err(error) => return Some(wire_cause::<LaneState>(turn, &error)),
            };
            let payload = match FakeMessage::decode(message.bytes(), turn) {
                Ok(payload) => payload,
                Err(cause) => {
                    let what = format!(
                        "undecodable vendor message: {} bytes",
                        message.bytes().len()
                    );
                    sender.keep_undecoded(message.bytes(), &what).await;
                    return Some(cause);
                }
            };
            if let Err(cause) = phase.advance(&payload, turn, interrupted) {
                return Some(cause);
            }
            let message = super::super::RouteMessage {
                payload,
                steer: None,
            };
            let item = match &message.payload {
                FakeMessage::Denial { .. } | FakeMessage::Decline { .. } => {
                    FakeRetiredItem::Durable(message)
                }
                FakeMessage::Terminal { vendor_turn_id, .. } => {
                    let vendor_turn_id = vendor_turn_id.clone();
                    let Some(terminal) = super::terminal_evidence(&message) else {
                        continue;
                    };
                    FakeRetiredItem::Terminal(FakeLateTerminal {
                        vendor_turn_id,
                        terminal,
                    })
                }
                // Not durable: the logical turn already ended.
                FakeMessage::Accepted { .. }
                | FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::Usage { .. }
                | FakeMessage::Hello(_)
                | FakeMessage::Identity { .. }
                | FakeMessage::SteerDelivered { .. }
                | FakeMessage::VendorClosed { .. }
                | FakeMessage::InterruptAck { .. }
                | FakeMessage::Unknown { .. } => continue,
            };
            if items.send(item).await.is_err() {
                // The lane closed: nothing more is delivered (ruling G1).
                return None;
            }
        }
    };
    if let Ok(Some(cause)) = tokio::time::timeout_at(by.instant(), read).await {
        // The driver's turn task is gone: nobody takes the failure.
        let _unread = failure.send(cause);
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
                launch: failure.launch,
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
