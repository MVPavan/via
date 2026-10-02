use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};

use super::{FakeMessage, RouteMessage, TerminalStatus, TurnStart};
use crate::private::{
    self, AfterTerminal, Closed, Failed, Interrupt, PrivateProtocol, Serving, cleanup_deadline,
    protocol,
};
use crate::steer::{SteerRequest, SteerSender};
use crate::{
    Deadline, PrivateProcessSpec, Retirement, RouteError, RouteFailure, RouteRuntime, StopSources,
    StopWatch, TurnNumber,
};
use lane::{Facts, KeepServer, LaneEvent, LaneState, turn_result};
use via_wire::{
    ExitReport, OutboundMessage, WireCleanup, WireCloseReport, WireMessages, WireSender,
};

mod lane;

pub use lane::{FakeLateTerminal, FakeRetired, FakeRetiredItem, FakeTerminal, FakeTurn, Lane};

/// Final fake protocol evidence, including independently confirmed process exit.
#[derive(Clone, Debug)]
pub struct FakeRouteResult {
    /// Vendor terminal status.
    pub status: TerminalStatus,
    /// Vendor stop reason retained verbatim.
    pub stop_reason: String,
    /// Optional vendor failure code.
    pub vendor_code: Option<String>,
    /// Host-confirmed vendor exit after stdin was half-closed; both fields
    /// are `None` when the wall deadline passed during finalization before
    /// an exit was confirmed.
    pub exit: ExitReport,
    /// Group cleanup certainty after the vendor's confirmed exit.
    pub cleanup: via_wire::WireCleanup,
    /// A Host journal write in the turn's cleanup had an uncertain outcome:
    /// the daemon must latch (design §7.2 row 12).
    pub journal_uncertain: bool,
    /// Host stopped the group while its vendor was live (Host force
    /// evidence), as on a late terminal's force close.
    pub forced: bool,
}

/// One-start state machine of the private fake route, over the shared
/// Route runtime that opens its connections.
pub struct FakeRoute {
    runtime: Arc<RouteRuntime>,
}

impl FakeRoute {
    /// The fake route over the daemon's Route runtime.
    pub fn new(runtime: Arc<RouteRuntime>) -> Self {
        Self { runtime }
    }

    /// A fake turn's steer lane: each input is sized as the fake's steer
    /// line encodes it.
    pub fn steer_lane() -> (SteerSender, mpsc::Receiver<SteerRequest>) {
        crate::steer_lane(lane::steer_line_len)
    }

    /// Sends one prompt after durable submission and awaits the paired
    /// terminal and real exit (adapter design §3.2), with the `lane`'s
    /// handshake, steer control, interrupt acknowledgement and persistent
    /// profile, over the private per-turn lifecycle ([`private::turn`]).
    ///
    /// Every decoded message, including acceptance, the terminal and late
    /// observations after it, is sent on the `hop` in decode order. While
    /// the hop is full Route reads no further message, and a closed hop
    /// (the Adapter's stall, C2 A1) fails the turn as overflow. Every wait
    /// keeps the controls serviced (Task 4 design §9). On any failure the
    /// private group is force-closed and the connection finished under a
    /// separate cleanup bound.
    ///
    /// `force` set fails the turn [`RouteError::ForceStopped`]: before launch
    /// nothing starts; after it, the same cleanup follows. The daemon force
    /// overrides a stop order.
    ///
    /// `stop` is the turn's stop order (design §2): set before ARM, nothing
    /// launches; after ARM but before the start message, the group is
    /// force-closed at once; after it, one interrupt is sent, a terminal still
    /// ends the turn normally, and at `force_at` without one the group is
    /// force-closed under `close_by`. Either stop is
    /// [`RouteError::Stopped`]. `sources` reports an order set where a
    /// relayed `stop` has not caught up yet; the entry check and the
    /// launch gate read it too.
    ///
    /// The logical turn, with the terminal and handshake retained on every
    /// outcome (AD4, AD7), goes on `logical` when it ends: on the persistent
    /// profile that can be before its process is retired (C2 §4.1).
    /// Returns the process's retirement.
    pub async fn turn(
        &self,
        process: PrivateProcessSpec,
        start: TurnStart,
        hop: mpsc::Sender<RouteMessage>,
        signals: (
            Deadline,
            watch::Receiver<Option<tokio::time::Instant>>,
            (StopWatch, StopSources),
        ),
        lane: Lane,
        logical: oneshot::Sender<FakeTurn>,
    ) -> Retirement {
        private::turn::<LaneState>(&self.runtime, process, start, hop, signals, (lane, logical))
            .await
    }
}

/// The fake's protocol over the private lifecycle: its typed messages, its
/// connection phase, the C2 lane's handshake, steer and identity facts,
/// and the persistent-connection profile (decision H1).
impl PrivateProtocol for LaneState {
    type Input = (Lane, oneshot::Sender<FakeTurn>);
    type Start = TurnStart;
    type Payload = FakeMessage;
    type Message = RouteMessage;
    type Terminal = FakeTerminal;
    type Result = FakeRouteResult;
    type Kept = (FakeTerminal, WireCleanup);
    type Keep = KeepServer;
    type Event = LaneEvent;

    /// While one message waits for the hop, Route reads no further one.
    const READ_AHEAD: usize = 1;
    const UNTERMINATED: &'static str = "fake stdout ended inside a message";

    fn open((lane, logical): Self::Input) -> Self {
        Self::new(lane, Some(logical))
    }

    fn unopened((lane, logical): Self::Input, result: Result<FakeRouteResult, RouteFailure>) {
        let facts = Facts::default();
        // The driver's turn was dropped: nobody reads the logical turn.
        let _unread = logical.send(turn_result(result, facts, lane.persistent));
    }

    fn finish(self, interrupt: Interrupt, result: Result<FakeRouteResult, RouteFailure>) {
        let persistent = self.persistent;
        let mut facts = self.into_facts(interrupt);
        if let Some(logical) = facts.logical.take() {
            // The driver's turn was dropped: nobody reads the logical turn.
            let _unread = logical.send(turn_result(result, facts, persistent));
        }
    }

    fn evidence(result: &FakeRouteResult) -> Closed {
        Closed {
            exit: result.exit,
            cleanup: result.cleanup,
            journal_uncertain: result.journal_uncertain,
            forced: result.forced,
        }
    }

    fn result(
        terminal: FakeTerminal,
        exit: ExitReport,
        close: &WireCloseReport,
    ) -> FakeRouteResult {
        terminal.result(exit, close)
    }

    fn turn_of(start: &TurnStart) -> TurnNumber {
        start.turn()
    }

    fn start_message(start: TurnStart) -> Result<OutboundMessage, RouteError> {
        start.into_message()
    }

    fn submitted(&mut self) {
        self.phase = Phase::Submitted;
    }

    fn decode(bytes: &[u8], turn: TurnNumber) -> Result<FakeMessage, RouteError> {
        FakeMessage::decode(bytes, turn)
    }

    /// The phase check, then the lane facts (`note`): a written steer's
    /// delivery report carries its token.
    fn admit<'s>(
        serving: &'s mut Serving<'_, Self>,
        payload: FakeMessage,
    ) -> impl Future<Output = Result<Option<RouteMessage>, Failed>> + Send + 's {
        let turn = serving.turn;
        let admitted = (|| {
            let interrupted = serving.interrupt != Interrupt::NotSent;
            serving
                .lane
                .phase
                .advance(&payload, turn, interrupted)
                .map_err(Failed::from)?;
            // The written steer a delivery report answers, taken
            // before `note` may resolve it.
            let steer = matches!(payload, FakeMessage::SteerDelivered { .. })
                .then(|| serving.lane.steer_token.take())
                .flatten();
            if !serving.note(&payload)? {
                return Ok(None);
            }
            Ok(Some(RouteMessage { payload, steer }))
        })();
        std::future::ready(admitted)
    }

    fn terminal(message: &RouteMessage) -> Option<FakeTerminal> {
        terminal_evidence(message)
    }

    fn retain(serving: &mut Serving<'_, Self>, terminal: &FakeTerminal) {
        serving.retain(terminal);
    }

    fn handshake<'s>(
        serving: &'s mut Serving<'_, Self>,
        messages: &'s mut WireMessages,
    ) -> impl Future<Output = Result<(), Failed>> + Send + 's {
        serving.handshake(messages)
    }

    fn interrupt(serving: &mut Serving<'_, Self>) -> OutboundMessage {
        let interrupt = format!(
            "{{\"type\":\"interrupt\",\"id\":2,\"vendor_turn_id\":\"fake-turn-{}\"}}\n",
            serving.turn.get()
        );
        OutboundMessage::Interrupt(interrupt.into_bytes())
    }

    fn bounded_exit(&self) -> bool {
        self.persistent
    }

    /// The persistent profile keeps its emulated server, and its stall
    /// fails the turn at once; the per-turn profile interrupts (C2 A1).
    fn interrupts_on_stall(&self) -> bool {
        !self.persistent
    }

    /// The persistent profile's logical turn ends at its terminal, with
    /// its cleanup (C2 §4.1); the server stays.
    async fn after_terminal(
        serving: &mut Serving<'_, Self>,
        messages: &mut WireMessages,
        terminal: FakeTerminal,
    ) -> Result<AfterTerminal<Self>, Failed> {
        if serving.lane.persistent {
            let cleanup = serving.persistent_end(messages, &terminal).await?;
            return Ok(AfterTerminal::Kept((terminal, cleanup)));
        }
        Ok(AfterTerminal::Finalize(terminal))
    }

    /// C2 §4.1 (persistent profile): the logical turn ended and the
    /// emulated server stays; its helper process is retired apart from
    /// the turn (decision H1).
    async fn kept(
        serving: &mut Serving<'_, Self>,
        sender: &WireSender,
        messages: WireMessages,
        (terminal, cleanup): (FakeTerminal, WireCleanup),
    ) -> Result<FakeRouteResult, RouteFailure> {
        serving.send_logical(Ok(FakeRouteResult {
            status: terminal.status,
            stop_reason: terminal.stop_reason.clone(),
            vendor_code: terminal.vendor_code.clone(),
            exit: ExitReport {
                code: None,
                signal: None,
            },
            cleanup,
            journal_uncertain: false,
            forced: false,
        }));
        let report = serving
            .retire(sender, messages, (None, cleanup_deadline()))
            .await;
        let exit = report.vendor_exit.unwrap_or(ExitReport {
            code: None,
            signal: None,
        });
        Ok(terminal.result(exit, &report))
    }

    fn keeps_server(serving: &Serving<'_, Self>, cause: &RouteError) -> Option<KeepServer> {
        serving.keeps_server(cause).then_some(KeepServer)
    }

    /// AD4 (persistent profile): at the wall, the cleanup step is the
    /// vendor's soft stop; a stop order's `force_at` asks for no kill
    /// (C2 §4.1). The logical turn ends; the helper is retired after.
    async fn keep(
        serving: &mut Serving<'_, Self>,
        (sender, mut messages): (&WireSender, WireMessages),
        KeepServer: KeepServer,
        (failed, cleanup): (Failed, Deadline),
    ) -> RouteFailure {
        if matches!(failed.cause, RouteError::Deadline { .. }) {
            serving.soft_stop(&mut messages, cleanup).await;
        }
        serving.send_logical(Err(serving.kept_failure(failed.cause.clone())));
        let report = serving
            .retire(sender, messages, (failed.exit, cleanup))
            .await;
        RouteFailure {
            cause: failed.cause,
            undecoded: sender.take_undecoded(),
            exit: failed.exit.or(report.vendor_exit),
            launched: true,
            cleanup: Some(report.cleanup),
            forced: report.forced,
            journal_uncertain: report.journal_uncertain,
            acknowledged: false,
            shared: false,
        }
    }

    fn has_event(&self) -> bool {
        self.has_event()
    }

    fn event(&mut self) -> impl Future<Output = LaneEvent> + Send + '_ {
        self.next_event()
    }

    fn on_event(serving: &mut Serving<'_, Self>, event: LaneEvent) -> Result<(), Failed> {
        serving.on_event(event)
    }
}

/// Connection-local protocol phase for the only turn on a fake connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Before the start: only the handshake and session-level messages.
    Opening,
    /// Start written; awaiting its paired acceptance.
    Submitted,
    /// Acceptance seen; observations and one terminal may follow.
    Accepted,
    /// Terminal seen; only late observations may follow.
    Terminated,
}

impl Phase {
    /// Checks one decoded message against the phase and advances it. An
    /// `interrupt_ack` is control evidence once Route sent its interrupt;
    /// an unknown message is activity only, in every phase (C2 A1).
    fn advance(
        &mut self,
        message: &FakeMessage,
        turn: TurnNumber,
        interrupted: bool,
    ) -> Result<(), RouteError> {
        match (message, *self) {
            (FakeMessage::InterruptAck { .. }, _) if interrupted => {}
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
            // The handshake is read before the start, never after (AD7).
            (FakeMessage::Hello(_), Self::Submitted | Self::Accepted | Self::Terminated) => {
                return Err(protocol(turn, "unexpected fake handshake"));
            }
            // Session-level and unknown messages may come at any time (C2
            // §4, A1).
            (FakeMessage::Hello(_), Self::Opening)
            | (
                FakeMessage::Identity { .. }
                | FakeMessage::VendorClosed { .. }
                | FakeMessage::Unknown { .. },
                _,
            )
            | (
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::Usage { .. }
                | FakeMessage::Denial { .. }
                | FakeMessage::Decline { .. }
                | FakeMessage::SteerDelivered { .. },
                Self::Accepted | Self::Terminated,
            ) => {}
            (
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::Usage { .. }
                | FakeMessage::Denial { .. }
                | FakeMessage::Decline { .. }
                | FakeMessage::SteerDelivered { .. },
                Self::Submitted,
            ) => return Err(protocol(turn, "fake observation before acceptance")),
            (
                FakeMessage::Accepted { .. }
                | FakeMessage::Terminal { .. }
                | FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::Usage { .. }
                | FakeMessage::Denial { .. }
                | FakeMessage::Decline { .. }
                | FakeMessage::SteerDelivered { .. },
                Self::Opening,
            ) => return Err(protocol(turn, "fake message before the start")),
        }
        Ok(())
    }
}

/// The terminal fields go into the route result. The final text is not
/// among them: the Adapter sends it as `final_text` observations from the
/// terminal message itself (Task 4 design §2.3).
impl FakeTerminal {
    fn result(self, exit: ExitReport, close: &WireCloseReport) -> FakeRouteResult {
        FakeRouteResult {
            status: self.status,
            stop_reason: self.stop_reason,
            vendor_code: self.vendor_code,
            exit,
            cleanup: close.cleanup,
            journal_uncertain: close.journal_uncertain,
            forced: close.forced,
        }
    }
}

fn terminal_evidence(message: &RouteMessage) -> Option<FakeTerminal> {
    match &message.payload {
        FakeMessage::Terminal {
            status,
            stop_reason,
            vendor_code,
            details,
            ..
        } => Some(FakeTerminal {
            at: tokio::time::Instant::now(),
            status: *status,
            stop_reason: stop_reason.clone(),
            vendor_code: vendor_code.clone(),
            details: details.clone(),
        }),
        FakeMessage::Accepted { .. }
        | FakeMessage::Text { .. }
        | FakeMessage::ToolStarted { .. }
        | FakeMessage::ToolEnded { .. }
        | FakeMessage::Usage { .. }
        | FakeMessage::InterruptAck { .. }
        | FakeMessage::Hello(_)
        | FakeMessage::Identity { .. }
        | FakeMessage::Denial { .. }
        | FakeMessage::Decline { .. }
        | FakeMessage::SteerDelivered { .. }
        | FakeMessage::VendorClosed { .. }
        | FakeMessage::Unknown { .. } => None,
    }
}

/// Resolves at `at`; never when it is unset.
async fn sleep_until_set(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::lane::acknowledges;
    use super::{FakeMessage, Interrupt, LaneState, Phase, RouteError, TurnNumber};
    use crate::StoreFailure;
    use via_wire::{HostError, WireError, WireFailure};

    /// The fake's cause for a Wire failure.
    fn wire_cause(turn: TurnNumber, error: &WireError) -> RouteError {
        crate::private::wire_cause::<LaneState>(turn, error)
    }

    fn decode(json: &str) -> FakeMessage {
        FakeMessage::decode(json.as_bytes(), TurnNumber::try_from(1).unwrap()).unwrap()
    }

    fn detail(phase: &mut Phase, json: &str) -> Option<&'static str> {
        match phase.advance(&decode(json), TurnNumber::try_from(1).unwrap(), false) {
            Ok(()) => None,
            Err(RouteError::Protocol { detail, .. }) => Some(detail),
            Err(other) => panic!("unexpected route error {other:?}"),
        }
    }

    const ACCEPTED: &str = r#"{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}"#;
    const TERMINAL: &str = r#"{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"","stop_reason":"end_turn"}"#;
    const OBSERVATIONS: [&str; 5] = [
        r#"{"type":"text","vendor_turn_id":"fake-turn-1","text":"hi"}"#,
        r#"{"type":"usage","vendor_turn_id":"fake-turn-1","total_tokens":7}"#,
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

    /// C2 A1 rule 6: an unknown notification produces no observation and
    /// is admitted in every phase, before acceptance too.
    #[test]
    fn an_unknown_message_is_admitted_in_every_phase() {
        for phase in [Phase::Submitted, Phase::Accepted, Phase::Terminated] {
            let mut phase = { phase };
            assert_eq!(detail(&mut phase, OBSERVATIONS[4]), None, "{phase:?}");
        }
    }

    #[test]
    fn order_violations_fail_the_turn() {
        // The unknown message (the last) is admitted in every phase.
        for message in &OBSERVATIONS[..4] {
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

    fn refused(input: &[u8]) -> Option<&'static str> {
        match FakeMessage::decode(input, TurnNumber::try_from(1).unwrap()) {
            Ok(_) => None,
            Err(RouteError::Protocol { detail, .. }) => Some(detail),
            Err(other) => panic!("unexpected route error {other:?}"),
        }
    }

    /// Task 4 design §2.2: what the fake keeps, and rules 1 and 3 plus
    /// UTF-8. Skipped fields are never decoded, whatever their type.
    #[test]
    fn decode_keeps_the_marks_and_bounds_short_fields() {
        let tool = |name: &str| {
            format!(
                r#"{{"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t","name":"{name}","input_summary":{{"x":[1]}}}}"#
            )
        };
        let kept = tool(&"n".repeat(1024));
        assert!(matches!(
            decode(&kept),
            FakeMessage::ToolStarted { ref name, .. } if name.len() == 1024
        ));
        assert_eq!(
            refused(tool(&"n".repeat(1025)).as_bytes()),
            Some("fake short field exceeds 1 KiB")
        );
        assert!(matches!(
            decode(r#"{"type":"text","vendor_turn_id":"fake-turn-1","text":1}"#),
            FakeMessage::Text { .. }
        ));
        assert!(matches!(
            decode(r#"{"type":"usage","vendor_turn_id":"fake-turn-1","total_tokens":9}"#),
            FakeMessage::Usage {
                total_tokens: 9,
                ..
            }
        ));
        let tag = |length: usize| format!(r#"{{"type":"{}","v":1}}"#, "u".repeat(length));
        assert!(matches!(
            decode(&tag(256)),
            FakeMessage::Unknown { ref vendor_type } if vendor_type.len() == 256
        ));
        assert_eq!(
            refused(tag(257).as_bytes()),
            Some("fake type tag exceeds 256 bytes")
        );
        let deep = format!(
            r#"{{"type":"text","vendor_turn_id":"fake-turn-1","x":{}{}}}"#,
            "[".repeat(64),
            "]".repeat(64)
        );
        assert_eq!(
            refused(deep.as_bytes()),
            Some("fake message exceeds JSON structure limits")
        );
        assert_eq!(
            refused(b"{\"type\":\"text\",\"vendor_turn_id\":\"fake-turn-1\",\"x\":\"\xff\"}"),
            Some("fake message is not UTF-8")
        );
    }

    /// C2 §7 item 10: vendor evidence acknowledges the interrupt only with
    /// its write confirmed: not while the write is pending, nor after it
    /// failed.
    #[test]
    fn an_acknowledgement_needs_the_confirmed_interrupt_write() {
        assert!(acknowledges(true, Interrupt::Written));
        for interrupt in [Interrupt::NotSent, Interrupt::Queued, Interrupt::Failed] {
            assert!(!acknowledges(true, interrupt), "{interrupt:?}");
        }
        assert!(!acknowledges(false, Interrupt::Written));
    }

    /// Design §2 rule 3: once Route sent its interrupt, an `interrupt_ack`
    /// is control evidence in any phase, not a protocol error.
    #[test]
    fn an_interrupt_ack_is_admitted_once_an_interrupt_was_sent() {
        let turn = TurnNumber::try_from(1).unwrap();
        let ack = decode(r#"{"type":"interrupt_ack","id":2,"vendor_turn_id":"fake-turn-1"}"#);
        for phase in [Phase::Submitted, Phase::Accepted, Phase::Terminated] {
            let mut phase = { phase };
            assert_eq!(phase.advance(&ack, turn, true), Ok(()));
        }
    }

    /// Task 4 design §7.2: a folder or `stderr.log` that was not created is
    /// a non-latching Store failure; Host journal failures report their
    /// outcome [r5.5]; C1 §8.2: a message over the cap is `overflow`.
    #[test]
    fn store_failures_keep_their_kind() {
        let turn = TurnNumber::try_from(1).unwrap();
        for error in [
            WireError::Evidence(std::io::Error::from(std::io::ErrorKind::AlreadyExists)),
            WireError::Host(HostError::Evidence(std::io::Error::from(
                std::io::ErrorKind::AlreadyExists,
            ))),
        ] {
            assert_eq!(
                wire_cause(turn, &error),
                RouteError::Store {
                    turn,
                    kind: StoreFailure::Evidence
                }
            );
        }
        assert!(StoreFailure::WriterLost.latches() && !StoreFailure::Evidence.latches());
        for (uncertain, kind) in [
            (false, StoreFailure::NotCommitted),
            (true, StoreFailure::Uncertain),
        ] {
            let cause = wire_cause(
                turn,
                &WireError::Host(HostError::Journal {
                    site: via_wire::JournalSite::VendorFacts,
                    uncertain,
                }),
            );
            assert_eq!(cause, RouteError::Store { turn, kind });
        }
        assert_eq!(
            wire_cause(turn, &WireError::Message(WireFailure::MessageTooLarge)),
            RouteError::Overflow { turn }
        );
    }
}
