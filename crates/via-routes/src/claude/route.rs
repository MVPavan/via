//! The Claude Code route's private per-turn lifecycle (packet §§2, 5–7;
//! adapter design §6): one VIA turn on one private `claude -p` process,
//! over the shared private lifecycle ([`private::turn`]). Route owns the
//! wire order: the one user line, an immediate decline of every control
//! request on the control lane, the one interrupt (paired by the ID it
//! allocates), stdin EOF after the result, and the phase rules that need
//! no Claude semantics. Identity, acceptance, the handshake check and the
//! terminal mapping are the Adapter's normalizer.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use super::messages::{
    ControlRequest, DecodeError, Message, ResultMessage, control_decline, decode,
    interrupt_request, user_start,
};
use crate::private::{
    self, AfterTerminal, Closed, Failed, Interrupt, PrivateProtocol, Serving, protocol,
};
use crate::{
    Deadline, PrivateProcessSpec, Retirement, RouteError, RouteFailure, RouteRuntime, SendOutcome,
    StopSources, StopWatch, TurnNumber,
};
use via_wire::{
    ExitReport, OutboundMessage, WireCleanup, WireCloseReport, WireMessages, WireSender,
};

/// The packet §6 bound on answering a control request: its decline's
/// first byte, or the turn fails closed.
const DECLINE_WITHIN: Duration = Duration::from_secs(5);

/// The decoded messages that may wait for room on the hop before Route
/// stops reading (runtime §8: 1,024 messages of route data, with its
/// 4 MiB). Reading ahead keeps the control lane answered while the
/// Adapter's observations are blocked (packet §6).
const READ_AHEAD: usize = 1024;

/// What the hop hands the Adapter, in decode order.
#[derive(Debug)]
pub enum ClaudeItem {
    /// A decoded message, for the normalizer.
    Message(Message),
    /// A control request whose decline was written whole (packet §6, Q9):
    /// only now may it be reported and its call's denial suppressed.
    Declined(ControlRequest),
    /// VIA enqueued its one interrupt under this request ID, before any
    /// later message was read: its receipt pairs with it (packet §7).
    InterruptSent(String),
}

/// The one start of a Claude turn: the prompt as one user line.
#[derive(Debug)]
pub struct ClaudeStart {
    turn: TurnNumber,
    prompt: String,
}

impl ClaudeStart {
    /// Turn `turn`'s start with `prompt`.
    pub fn new(turn: TurnNumber, prompt: String) -> Self {
        Self { turn, prompt }
    }

    /// The start's turn.
    pub fn turn(&self) -> TurnNumber {
        self.turn
    }
}

/// The process facts of a finalized Claude turn.
#[derive(Clone, Copy, Debug)]
pub struct ClaudeRouteResult {
    /// Host-confirmed vendor exit after stdin EOF; both fields `None` when
    /// none was confirmed (the late path).
    pub exit: ExitReport,
    /// Group cleanup certainty.
    pub cleanup: WireCleanup,
    /// A Host journal write in the cleanup had an uncertain outcome.
    pub journal_uncertain: bool,
    /// Host stopped the group while its vendor was live.
    pub forced: bool,
}

/// A Claude turn's end, as Route hands it to the Adapter.
#[derive(Debug)]
pub struct ClaudeTurn {
    /// The route's outcome: the finalized process, or the first failure
    /// with Route's evidence.
    pub outcome: Result<ClaudeRouteResult, RouteFailure>,
    /// The first `result` Route read, kept whatever became of its delivery
    /// (AD4): the Adapter normalizes it when the hop never handed it on.
    pub result: Option<Box<ResultMessage>>,
}

/// The private Claude Code route over the daemon's Route runtime.
pub struct ClaudeRoute {
    runtime: Arc<RouteRuntime>,
}

impl ClaudeRoute {
    /// The route over the daemon's Route runtime.
    pub fn new(runtime: Arc<RouteRuntime>) -> Self {
        Self { runtime }
    }

    /// Runs one Claude turn on its own private process, over the shared
    /// private lifecycle (S1 rules 1 to 4, F21): the stop order, the
    /// daemon force and the wall act as on every private route, the stop
    /// order's soft stop being the one interrupt control request. Every
    /// admitted message and the interrupt's ID go on the `hop` in decode
    /// order; up to 1,024 messages (4 MiB) wait for room on it, so a
    /// control request is still read and declined at once while the
    /// Adapter's observations are blocked. The turn's end goes on `end`.
    /// Returns the process's retirement.
    pub async fn turn(
        &self,
        process: PrivateProcessSpec,
        start: ClaudeStart,
        hop: mpsc::Sender<ClaudeItem>,
        signals: (
            Deadline,
            watch::Receiver<Option<tokio::time::Instant>>,
            (StopWatch, StopSources),
        ),
        end: oneshot::Sender<ClaudeTurn>,
    ) -> Retirement {
        private::turn::<ClaudeLane>(&self.runtime, process, start, hop, signals, end).await
    }
}

/// The `result` was read: the turn's terminal for the lifecycle.
pub(crate) struct ResultRead;

/// One Claude turn's route state.
pub(crate) struct ClaudeLane {
    end: oneshot::Sender<ClaudeTurn>,
    /// The first `result`, kept (AD4).
    result: Option<Box<ResultMessage>>,
}

/// The decline of `request` was not written whole, or cannot be written:
/// the turn fails closed (packet §6), reporting no decline.
const UNANSWERED: &str = "a control request VIA could not decline";

impl ClaudeLane {
    fn send(self, outcome: Result<ClaudeRouteResult, RouteFailure>) {
        // The driver's turn was dropped: nobody reads the end.
        let _unread = self.end.send(ClaudeTurn {
            outcome,
            result: self.result,
        });
    }

    /// Declines `request` on the control lane at once: its first byte by
    /// [`DECLINE_WITHIN`] or the turn's deadline, whichever is first.
    async fn decline(
        serving: &mut Serving<'_, Self>,
        request: ControlRequest,
    ) -> Result<Option<ClaudeItem>, Failed> {
        let turn = serving.turn;
        let by = Deadline::at(
            serving
                .deadline
                .instant()
                .min(tokio::time::Instant::now() + DECLINE_WITHIN),
        );
        let write = serving.sender.write(
            OutboundMessage::Control(control_decline(&request.request_id)),
            by,
        );
        match serving.serve(write).await? {
            Ok(SendOutcome::Written) => Ok(Some(ClaudeItem::Declined(request))),
            Ok(_) | Err(_) => Err(protocol(turn, UNANSWERED).into()),
        }
    }
}

/// Claude Code's protocol over the private lifecycle.
impl PrivateProtocol for ClaudeLane {
    type Input = oneshot::Sender<ClaudeTurn>;
    type Start = ClaudeStart;
    type Payload = Message;
    type Message = ClaudeItem;
    type Terminal = ResultRead;
    type Result = ClaudeRouteResult;
    type Kept = Infallible;
    type Keep = Infallible;
    type Event = Infallible;

    const READ_AHEAD: usize = READ_AHEAD;
    const UNTERMINATED: &'static str = "vendor stdout ended inside a message";

    fn open(end: Self::Input) -> Self {
        Self { end, result: None }
    }

    fn unopened(end: Self::Input, outcome: Result<ClaudeRouteResult, RouteFailure>) {
        Self::open(end).send(outcome);
    }

    fn finish(self, _interrupt: Interrupt, outcome: Result<ClaudeRouteResult, RouteFailure>) {
        self.send(outcome);
    }

    fn evidence(result: &ClaudeRouteResult) -> Closed {
        Closed {
            exit: result.exit,
            cleanup: result.cleanup,
            journal_uncertain: result.journal_uncertain,
            forced: result.forced,
        }
    }

    fn result(
        _terminal: ResultRead,
        exit: ExitReport,
        close: &WireCloseReport,
    ) -> ClaudeRouteResult {
        ClaudeRouteResult {
            exit,
            cleanup: close.cleanup,
            journal_uncertain: close.journal_uncertain,
            forced: close.forced,
        }
    }

    fn turn_of(start: &ClaudeStart) -> TurnNumber {
        start.turn
    }

    /// Packet §5: exactly one user line.
    fn start_message(start: ClaudeStart) -> Result<OutboundMessage, RouteError> {
        Ok(user_start(start.prompt))
    }

    fn submitted(&mut self) {}

    fn decode(bytes: &[u8], turn: TurnNumber) -> Result<Message, RouteError> {
        decode(bytes).map_err(|error| {
            protocol(
                turn,
                match error {
                    DecodeError::NotTyped => "a vendor line that is not a typed message",
                    DecodeError::Malformed(_) => "a malformed vendor message",
                    DecodeError::Limits => "a vendor message past the JSON structure limits",
                },
            )
        })
    }

    /// The phase rules Route keeps: after the result, a second result, an
    /// init or a control request (stdin is closed: it cannot be declined)
    /// fails the turn `protocol`. Before it, a control request is declined
    /// at once, and handed on only once its decline was written whole.
    async fn admit(
        serving: &mut Serving<'_, Self>,
        message: Message,
    ) -> Result<Option<ClaudeItem>, Failed> {
        let turn = serving.turn;
        let read = serving.lane.result.is_some();
        match message {
            Message::Result(_) if read => Err(protocol(turn, "a second result").into()),
            Message::Init(_) if read => Err(protocol(turn, "an init after the result").into()),
            Message::ControlRequest(_) if read => Err(protocol(turn, UNANSWERED).into()),
            Message::ControlRequest(request) => Self::decline(serving, request).await,
            Message::Result(result) => {
                serving.lane.result = Some(Box::new((*result).clone()));
                Ok(Some(ClaudeItem::Message(Message::Result(result))))
            }
            message @ (Message::Init(_)
            | Message::PermissionDenied(_)
            | Message::Assistant(_)
            | Message::User(_)
            | Message::ControlResponse(_)
            | Message::Unknown { .. }) => Ok(Some(ClaudeItem::Message(message))),
        }
    }

    fn terminal(message: &ClaudeItem) -> Option<ResultRead> {
        matches!(message, ClaudeItem::Message(Message::Result(_))).then_some(ResultRead)
    }

    /// Kept at admission, where the message is owned.
    fn retain(_serving: &mut Serving<'_, Self>, _terminal: &ResultRead) {}

    /// Claude's init follows the prompt line (packet §3): the Adapter's
    /// normalizer checks it, so nothing is read before the start.
    fn handshake<'s>(
        _serving: &'s mut Serving<'_, Self>,
        _messages: &'s mut WireMessages,
    ) -> impl Future<Output = Result<(), Failed>> + Send + 's {
        std::future::ready(Ok(()))
    }

    /// Packet §7: the interrupt control request under an ID Route
    /// allocates, handed to the Adapter before any later message.
    fn interrupt(serving: &mut Serving<'_, Self>) -> OutboundMessage {
        let id = format!("via-interrupt-{}", serving.turn.get());
        let bytes = interrupt_request(&id);
        serving.hold((ClaudeItem::InterruptSent(id), 0));
        OutboundMessage::Interrupt(bytes)
    }

    fn bounded_exit(&self) -> bool {
        false
    }

    /// Per turn: stdin EOF after the result, then S1's close (AD19).
    fn after_terminal<'s>(
        _serving: &'s mut Serving<'_, Self>,
        _messages: &'s mut WireMessages,
        terminal: ResultRead,
    ) -> impl Future<Output = Result<AfterTerminal<Self>, Failed>> + Send + 's {
        std::future::ready(Ok(AfterTerminal::Finalize(terminal)))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "uninhabited: no Claude turn keeps a server, and std::future::ready of it is unreachable code"
    )]
    async fn kept(
        _serving: &mut Serving<'_, Self>,
        _sender: &WireSender,
        _messages: WireMessages,
        kept: Infallible,
    ) -> Result<ClaudeRouteResult, RouteFailure> {
        match kept {}
    }

    fn keeps_server(_serving: &Serving<'_, Self>, _cause: &RouteError) -> Option<Infallible> {
        None
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "uninhabited: no Claude turn keeps a server, and std::future::ready of it is unreachable code"
    )]
    async fn keep(
        _serving: &mut Serving<'_, Self>,
        _connection: (&WireSender, WireMessages),
        keep: Infallible,
        _failure: (Failed, Deadline),
    ) -> RouteFailure {
        match keep {}
    }

    fn has_event(&self) -> bool {
        false
    }

    fn event(&mut self) -> impl Future<Output = Infallible> + Send + '_ {
        std::future::pending()
    }

    fn on_event(_serving: &mut Serving<'_, Self>, event: Infallible) -> Result<(), Failed> {
        match event {}
    }
}
