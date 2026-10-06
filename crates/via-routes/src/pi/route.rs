//! The Pi route's private per-turn lifecycle (packet §§2.1, 5–7): one VIA
//! turn on one private `pi --mode rpc` process, over the shared private
//! lifecycle ([`private::turn`]). Route owns the wire order and every
//! check that needs no Pi semantics beyond the records: the handshake's
//! three commands and their checks before any prompt, the one prompt, the
//! command pairing, the decline of every dialog on the control lane, the
//! one abort (paired by the ID it allocates) and its reply's wait after
//! `agent_settled`, and stdin EOF only after them. Identity, acceptance,
//! usage, the terminal mapping and the evidence records are the
//! Adapter's.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, watch};

use super::messages::{
    AssistantEnd, DecodeError, MessageEnd, Record, Response, UiRequest, command, commands_data,
    decode, models_data, prompt_start, state_data, ui_cancel,
};
use crate::private::{
    self, AfterTerminal, Closed, Failed, Hop, Interrupt, Next, PrivateProtocol, Serving, protocol,
    transport, wire_cause,
};
use crate::{
    Deadline, PrivateProcessSpec, Retirement, RouteError, RouteFailure, RouteRuntime, SendOutcome,
    StopSources, StopWatch, StoreFailure, TurnNumber,
};
use via_wire::{
    ExitReport, HostError, OutboundMessage, WireCleanup, WireCloseReport, WireError, WireMessages,
    WireSender, WriteBounds,
};

/// The packet §6 bound on answering a dialog: its cancellation's first
/// byte, or the turn fails closed.
const DECLINE_WITHIN: Duration = Duration::from_secs(5);

/// The decoded records that may wait for room on the hop before Route
/// stops reading (runtime §8: 1,024 messages, with its 4 MiB), so a dialog
/// is still read and declined while the Adapter's observations are
/// blocked (packet §6).
const READ_AHEAD: usize = 1024;

/// The most compactions Route admits before the prompt's `started` reply
/// (picrit #8). Their samples wait in the Adapter until acceptance and
/// emit nothing meanwhile, so the hop's bound does not bound them. Pi
/// compacts at most once before a prompt (E63); 64 leaves room for any
/// retry while keeping the held samples a few KiB.
const HELD_SAMPLES_MAX: usize = 64;

/// The fire-and-forget methods (packet §6): activity. Every other
/// method, a dialog (`select`, `confirm`, `input`, `editor`) or one VIA
/// does not know, is reported once cancelled, and fails closed without an
/// `id`.
const FIRE_AND_FORGET: [&str; 5] = [
    "notify",
    "setStatus",
    "setWidget",
    "setTitle",
    "set_editor_text",
];

/// What the handshake established (packet §2.1 step 3): the facts the
/// Adapter's identity confirmation and inventory need.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HandshakeFacts {
    /// `get_state.sessionFile`, unresolved.
    pub session_file: String,
    /// `get_state.thinkingLevel`.
    pub thinking_level: String,
    /// The `skill:*` names `get_commands` listed.
    pub skills: Vec<String>,
}

/// What hands the Adapter, in decode order.
#[derive(Debug)]
pub enum PiItem {
    /// The handshake's `sessionId` named another session (packet §2.1):
    /// handed on before the turn fails `resume_mismatch`.
    Mismatch {
        /// The ID VIA launched with.
        requested: String,
        /// The one `get_state` reported.
        returned: String,
    },
    /// The prompt's `started` reply (packet §2.1 step 4): acceptance under
    /// the prompt's request ID.
    Started {
        /// The prompt's request ID: the turn's vendor turn ID.
        request_id: String,
        /// The handshake's facts.
        facts: Box<HandshakeFacts>,
    },
    /// A record for the normalizer.
    Record(Record),
    /// A dialog whose cancellation was written whole (packet §6): only now
    /// may it be reported.
    Declined {
        /// Its `method`.
        method: String,
        /// Its `title`, unbounded here; the Adapter bounds it.
        title: Option<String>,
    },
    /// `agent_settled`: the run's end, the turn's terminal.
    Settled,
    /// The prompt's `success:false` reply: a definite rejection, the
    /// turn's terminal.
    Refused,
    /// The abort's paired reply (packet §7.1), success or not.
    AbortAnswered(bool),
}

/// Who the turn runs as and what its handshake must read back (packet
/// §2.1 step 3, §4.5).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PiExpect {
    /// The session ID VIA launched with.
    pub session_id: String,
    /// The resolved model's provider.
    pub provider: String,
    /// The resolved model's ID.
    pub model_id: String,
    /// The requested thinking level, when effort was requested.
    pub thinking: Option<String>,
    /// The `--tools` list.
    pub tools: Vec<String>,
}

/// The one start of a Pi turn: the prompt.
#[derive(Debug)]
pub struct PiStart {
    turn: TurnNumber,
    prompt: String,
}

impl PiStart {
    /// Turn `turn`'s start with `prompt`.
    pub fn new(turn: TurnNumber, prompt: String) -> Self {
        Self { turn, prompt }
    }

    /// The start's turn.
    pub fn turn(&self) -> TurnNumber {
        self.turn
    }
}

/// The process facts of a finalized Pi turn.
#[derive(Clone, Copy, Debug)]
pub struct PiRouteResult {
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

/// The abort's facts at the turn's end (packet §7.1).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AbortFacts {
    /// The abort was written whole.
    pub written: bool,
    /// Its paired reply, when one arrived: its `success`.
    pub answered: Option<bool>,
}

/// A Pi turn's end, as Route hands it to the Adapter.
#[derive(Debug)]
pub struct PiTurn {
    /// The route's outcome: the finalized process, or the first failure
    /// with Route's evidence.
    pub outcome: Result<PiRouteResult, RouteFailure>,
    /// The prompt's write began.
    pub submitted: bool,
    /// The handshake's facts, once it passed.
    pub handshake: Option<HandshakeFacts>,
    /// The last assistant message before `agent_settled`, kept once
    /// `agent_settled` was read (AD4): the terminal, whatever became of its
    /// delivery.
    pub terminal: Option<Box<AssistantEnd>>,
    /// The prompt was refused (`success:false`).
    pub refused: bool,
    /// The abort's facts.
    pub abort: AbortFacts,
}

/// The private Pi route over the daemon's Route runtime.
pub struct PiRoute {
    runtime: Arc<RouteRuntime>,
}

impl PiRoute {
    /// The route over the daemon's Route runtime.
    pub fn new(runtime: Arc<RouteRuntime>) -> Self {
        Self { runtime }
    }

    /// Runs one Pi turn on its own private process, over the shared
    /// private lifecycle (S1 rules 1 to 4, F21): the handshake and its
    /// checks before the prompt, then the run to `agent_settled`; the stop
    /// order's soft stop is the one abort, whose reply is awaited after
    /// `agent_settled` up to the order's `force_at`. Every admitted record
    /// goes on the `hop` in decode order; up to 1,024 records (4 MiB) wait
    /// for room on it. The turn's end goes on `end`. Returns the process's
    /// retirement.
    pub async fn turn(
        &self,
        process: PrivateProcessSpec,
        start: PiStart,
        hop: Hop<PiItem>,
        signals: (
            Deadline,
            watch::Receiver<Option<tokio::time::Instant>>,
            (StopWatch, StopSources),
        ),
        input: (PiExpect, oneshot::Sender<PiTurn>),
    ) -> Retirement {
        private::turn::<PiLane>(&self.runtime, process, start, hop, signals, input).await
    }

    /// Packet §7.4 (R1): whether every earlier Pi launch of `session` is
    /// proven gone, by Host's one non-signalling pass within `deadline`. A
    /// Store read or commit that fails, or outlives the deadline, is the
    /// check's error: the turn's Store failure (decision C-3), never an
    /// unresolved predecessor; the wall itself is `Deadline`.
    pub async fn predecessors_resolved(
        &self,
        session: &crate::SessionId,
        turn: TurnNumber,
        deadline: Deadline,
    ) -> Result<bool, RouteError> {
        self.runtime
            .session_predecessors_resolved(session, deadline)
            .await
            .map_err(|error| predecessor_cause(turn, &error))
    }
}

/// The R1 check's failure cause (packet §7.4).
fn predecessor_cause(turn: TurnNumber, error: &WireError) -> RouteError {
    match error {
        // Every one is a read (a page or the final count), whatever kind
        // the Store gave: a dropped read reply is `UncertainCommit`. An
        // absence proof's uncertain commit is Host's journal error, which
        // `wire_cause` keeps uncertain.
        WireError::Host(HostError::Store(_) | HostError::StoreUnavailable(_)) => {
            RouteError::Store {
                turn,
                kind: StoreFailure::NotCommitted,
            }
        }
        error @ (WireError::Host(_)
        | WireError::Evidence(_)
        | WireError::Io(_)
        | WireError::Deadline
        | WireError::Cancelled
        | WireError::Woken
        | WireError::Acquire { .. }
        | WireError::Message(_)) => wire_cause::<PiLane>(turn, error),
    }
}

/// `agent_settled` or the prompt's refusal was read: the turn's terminal
/// for the lifecycle.
pub(crate) struct Settled;

/// Where a Pi turn stands (packet §2.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Before the prompt's write.
    Handshake,
    /// The prompt's write began; no `started` reply yet.
    Submitted,
    /// The prompt's `started` reply was read: the run is on.
    Started,
    /// `agent_settled` was read.
    Settled,
    /// The prompt was refused (`success:false`).
    Refused,
}

/// One Pi turn's route state.
pub(crate) struct PiLane {
    end: oneshot::Sender<PiTurn>,
    expect: PiExpect,
    /// Commands awaiting their reply, by request ID.
    pending: Vec<(String, &'static str)>,
    state: Option<super::messages::StateData>,
    models: Option<Vec<(String, String)>>,
    commands: Option<Vec<String>>,
    facts: Option<HandshakeFacts>,
    phase: Phase,
    /// The last assistant message since `started`.
    assistant: Option<Box<AssistantEnd>>,
    abort_answered: Option<bool>,
    /// Compactions admitted before `started` ([`HELD_SAMPLES_MAX`]).
    held: usize,
}

/// A dialog VIA could not cancel, or cannot be answered: the turn fails
/// closed (packet §6), reporting no decline.
const UNANSWERED: &str = "an extension dialog VIA could not cancel";

impl PiLane {
    fn send(self, interrupt: Interrupt, outcome: Result<PiRouteResult, RouteFailure>) {
        // Every exit's one rule (picrit #3): an unanswered marker is never
        // retained.
        let terminal = (self.phase == Phase::Settled && !self.unanswered_marker(interrupt))
            .then_some(self.assistant)
            .flatten();
        // The driver's turn was dropped: nobody reads the end.
        let _unread = self.end.send(PiTurn {
            outcome,
            submitted: self.phase != Phase::Handshake,
            handshake: self.facts,
            terminal,
            refused: self.phase == Phase::Refused,
            abort: AbortFacts {
                written: interrupt == Interrupt::Written,
                answered: self.abort_answered,
            },
        });
    }

    /// A fresh request ID for `kind`, awaiting its reply.
    fn request(&mut self, turn: TurnNumber, kind: &'static str) -> String {
        let id = format!("via-{}-{kind}", turn.get());
        self.pending.push((id.clone(), kind));
        id
    }

    /// Pairs `reply` with the command VIA sent under its ID (packet §5.1):
    /// an unknown ID, a second reply, another command or `parse` is
    /// protocol.
    fn pair(&mut self, reply: &Response) -> Result<&'static str, &'static str> {
        let at = reply
            .id
            .as_ref()
            .and_then(|id| self.pending.iter().position(|(sent, _)| sent == id))
            .ok_or("a reply to no outstanding VIA command")?;
        let (_, kind) = self.pending.remove(at);
        if reply.command != kind {
            return Err("a reply naming another command than VIA sent");
        }
        Ok(kind)
    }

    /// One handshake reply, kept for the checks.
    fn handshake_reply(&mut self, reply: &Response) -> Result<(), &'static str> {
        let kind = self.pair(reply)?;
        let data = reply
            .data
            .as_ref()
            .filter(|_| reply.success)
            .ok_or("a handshake command failed or replied without its data")?;
        let malformed = "a malformed handshake reply";
        match kind {
            "get_state" => self.state = Some(state_data(data).ok_or(malformed)?),
            "get_available_models" => self.models = Some(models_data(data).ok_or(malformed)?),
            "get_commands" => self.commands = Some(commands_data(data).ok_or(malformed)?),
            _ => return Err("a reply to no outstanding VIA command"),
        }
        Ok(())
    }

    fn handshake_read(&self) -> bool {
        self.state.is_some() && self.models.is_some() && self.commands.is_some()
    }

    /// Declines dialog `request` on the control lane at once: its first
    /// byte by [`DECLINE_WITHIN`] or the turn's deadline, whichever is
    /// first. A dialog is reported once its cancellation was written
    /// whole; a fire-and-forget request with an ID is answered as activity.
    async fn decline(
        serving: &mut Serving<'_, Self>,
        request: UiRequest,
    ) -> Result<Option<PiItem>, Failed> {
        let turn = serving.turn;
        let reported = !FIRE_AND_FORGET.contains(&request.method.as_str());
        let Some(id) = request.id else {
            // An unanswered dialog blocks the run indefinitely (E44).
            if reported {
                return Err(protocol(turn, "an extension dialog without an id").into());
            }
            return Ok(Some(PiItem::Record(Record::Activity)));
        };
        let by = Deadline::at(
            serving
                .deadline
                .instant()
                .min(tokio::time::Instant::now() + DECLINE_WITHIN),
        );
        let write = serving.sender.write(
            OutboundMessage::Control(ui_cancel(&id)),
            WriteBounds::CutAt(by),
        );
        match serving.serve(write).await? {
            Ok(SendOutcome::Written) if reported => Ok(Some(PiItem::Declined {
                method: request.method,
                title: request.title,
            })),
            Ok(SendOutcome::Written) => Ok(Some(PiItem::Record(Record::Activity))),
            Ok(_) | Err(_) => Err(protocol(turn, UNANSWERED).into()),
        }
    }

    /// The prompt's or the abort's reply, after the handshake.
    fn reply(&mut self, turn: TurnNumber, reply: &Response) -> Result<PiItem, Failed> {
        let kind = self.pair(reply).map_err(|why| protocol(turn, why))?;
        match kind {
            "prompt" if !reply.success => {
                self.phase = Phase::Refused;
                Ok(PiItem::Refused)
            }
            "prompt" => {
                let disposition = reply
                    .data
                    .as_ref()
                    .and_then(|data| data.get("disposition"))
                    .and_then(serde_json::Value::as_str);
                if disposition != Some("started") {
                    // `handled` and `queued` cannot occur on an idle
                    // fresh process under `-ne -np` (E40).
                    return Err(protocol(turn, "a prompt reply other than started").into());
                }
                self.phase = Phase::Started;
                Ok(PiItem::Started {
                    request_id: reply.id.clone().unwrap_or_default(),
                    facts: Box::new(self.facts.clone().unwrap_or_default()),
                })
            }
            "abort" => {
                self.abort_answered = Some(reply.success);
                Ok(PiItem::AbortAnswered(reply.success))
            }
            _ => Err(protocol(turn, "a reply to no outstanding VIA command").into()),
        }
    }

    /// Packet §2.1 step 3's checks, in order, once every reply was read.
    fn check(&mut self, turn: TurnNumber) -> Result<(), Result<PiItem, RouteError>> {
        let (Some(state), Some(models), Some(commands)) =
            (self.state.take(), self.models.take(), self.commands.take())
        else {
            return Err(Err(protocol(turn, "a handshake reply missing")));
        };
        if state.session_id != self.expect.session_id {
            return Err(Ok(PiItem::Mismatch {
                requested: self.expect.session_id.clone(),
                returned: state.session_id,
            }));
        }
        let model = (self.expect.provider.as_str(), self.expect.model_id.as_str());
        if (state.provider.as_str(), state.model_id.as_str()) != model
            || !models
                .iter()
                .any(|(provider, id)| (provider.as_str(), id.as_str()) == model)
        {
            return Err(Err(RouteError::InvalidParam {
                turn,
                field: "model",
            }));
        }
        if let Some(level) = &self.expect.thinking
            && *level != state.thinking_level
        {
            return Err(Err(RouteError::InvalidParam {
                turn,
                field: "effort",
            }));
        }
        if commands.iter().any(|name| !name.starts_with("skill:")) {
            return Err(Err(RouteError::HandshakeRefused {
                turn,
                detail: Some(
                    "get_commands listed a command other than skill:* (-ne or -np did not hold)"
                        .to_owned(),
                ),
            }));
        }
        self.facts = Some(HandshakeFacts {
            session_file: state.session_file,
            thinking_level: state.thinking_level,
            skills: commands,
        });
        Ok(())
    }

    /// Whether the kept terminal carries a cancellation marker (packet
    /// §7.1): `aborted`, or `error` with exactly "This operation was
    /// aborted".
    fn marker(&self) -> bool {
        self.assistant.as_deref().is_some_and(is_marker)
    }

    /// Packet §7.1 step 3: the settled run's terminal is a marker and the
    /// abort VIA sent (`interrupt`) got no reply. It is not retained, on
    /// any exit; the order that sent the abort decides the turn.
    fn unanswered_marker(&self, interrupt: Interrupt) -> bool {
        self.phase == Phase::Settled
            && self.abort_answered.is_none()
            && matches!(interrupt, Interrupt::Queued | Interrupt::Written)
            && self.marker()
    }
}

/// Packet §7.1's cancellation marker on an assistant message.
#[must_use]
pub fn is_marker(message: &AssistantEnd) -> bool {
    message.stop_reason == "aborted"
        || (message.stop_reason == "error"
            && message.error_message.as_deref() == Some("This operation was aborted"))
}

/// The handshake's commands, written at once (Pi buffers them, E01).
async fn send_commands(serving: &mut Serving<'_, PiLane>) -> Result<(), Failed> {
    let turn = serving.turn;
    let deadline = serving.deadline;
    let writes: Vec<_> = ["get_state", "get_available_models", "get_commands"]
        .into_iter()
        .map(|kind| {
            let id = serving.lane.request(turn, kind);
            serving.sender.write(
                OutboundMessage::Control(command(&id, kind)),
                WriteBounds::CutAt(deadline),
            )
        })
        .collect();
    let written = serving
        .serve(async {
            let mut outcomes = Vec::with_capacity(writes.len());
            for write in writes {
                outcomes.push(write.await);
            }
            outcomes
        })
        .await?;
    for outcome in written {
        match outcome {
            Ok(SendOutcome::Written) => {}
            Ok(_) => return Err(transport(turn).into()),
            Err(error) => return Err(wire_cause::<PiLane>(turn, &error).into()),
        }
    }
    Ok(())
}

/// Reads the handshake's three replies, nothing handed on: any other run
/// record before the prompt is protocol; unknown records are activity.
async fn read_replies(
    serving: &mut Serving<'_, PiLane>,
    messages: &mut WireMessages,
) -> Result<(), Failed> {
    let turn = serving.turn;
    while !serving.lane.handshake_read() {
        let message = match serving.serve(messages.next_message()).await? {
            Ok(Some(message)) => message,
            Ok(None) => return Err(serving.ended(Next::Eof).await),
            Err(WireError::Woken) => continue,
            Err(WireError::Message(via_wire::WireFailure::UnterminatedMessage)) => {
                return Err(serving.ended(Next::Unterminated).await);
            }
            Err(error) => return Err(wire_cause::<PiLane>(turn, &error).into()),
        };
        let record = match PiLane::decode(message.bytes(), turn) {
            Ok(record) => record,
            Err(cause) => {
                let what = format!(
                    "undecodable vendor message: {} bytes",
                    message.bytes().len()
                );
                serving.sender.keep_undecoded(message.bytes(), &what).await;
                return Err(cause.into());
            }
        };
        match record {
            Record::Response(reply) => serving
                .lane
                .handshake_reply(&reply)
                .map_err(|why| protocol(turn, why))?,
            Record::Activity => {}
            Record::UiRequest(_)
            | Record::Lifecycle
            | Record::Settled
            | Record::MessageStart(_)
            | Record::MessageEnd(_)
            | Record::MessageUpdate { .. }
            | Record::ToolStart { .. }
            | Record::ToolEnd { .. }
            | Record::CompactionEnd(_)
            | Record::UsageHidden => {
                return Err(protocol(turn, "a run record before the prompt").into());
            }
        }
    }
    Ok(())
}

/// Pi's RPC over the private lifecycle.
impl PrivateProtocol for PiLane {
    type Input = (PiExpect, oneshot::Sender<PiTurn>);
    type Start = PiStart;
    type Payload = Record;
    type Message = PiItem;
    type Terminal = Settled;
    type Result = PiRouteResult;
    type Kept = Infallible;
    type Keep = Infallible;
    type Event = Infallible;

    const READ_AHEAD: usize = READ_AHEAD;
    const UNTERMINATED: &'static str = "vendor stdout ended inside a record";

    fn open((expect, end): Self::Input) -> Self {
        Self {
            end,
            expect,
            pending: Vec::new(),
            state: None,
            models: None,
            commands: None,
            facts: None,
            phase: Phase::Handshake,
            assistant: None,
            abort_answered: None,
            held: 0,
        }
    }

    fn unopened(input: Self::Input, outcome: Result<PiRouteResult, RouteFailure>) {
        Self::open(input).send(Interrupt::NotSent, outcome);
    }

    fn finish(self, interrupt: Interrupt, outcome: Result<PiRouteResult, RouteFailure>) {
        self.send(interrupt, outcome);
    }

    fn evidence(result: &PiRouteResult) -> Closed {
        Closed {
            exit: result.exit,
            cleanup: result.cleanup,
            journal_uncertain: result.journal_uncertain,
            forced: result.forced,
        }
    }

    fn result(_terminal: Settled, exit: ExitReport, close: &WireCloseReport) -> PiRouteResult {
        PiRouteResult {
            exit,
            cleanup: close.cleanup,
            journal_uncertain: close.journal_uncertain,
            forced: close.forced,
        }
    }

    fn turn_of(start: &PiStart) -> TurnNumber {
        start.turn
    }

    /// Packet §2.1 step 4: exactly one prompt, under the ID the handshake
    /// allocated last ([`PiLane::request`]).
    fn start_message(start: PiStart) -> Result<OutboundMessage, RouteError> {
        let id = format!("via-{}-prompt", start.turn.get());
        Ok(prompt_start(&id, start.prompt))
    }

    fn submitted(&mut self) {
        self.phase = Phase::Submitted;
    }

    fn decode(bytes: &[u8], turn: TurnNumber) -> Result<Record, RouteError> {
        decode(bytes).map_err(|error| {
            protocol(
                turn,
                match error {
                    DecodeError::NotTyped => "a vendor line that is not a typed record",
                    DecodeError::Malformed(what) => what,
                    DecodeError::Limits => "a vendor record past the JSON structure limits",
                },
            )
        })
    }

    /// The phase rules Route keeps (packet §5.1): replies paired; before
    /// `started`, a lifecycle, message or tool record is protocol and
    /// compaction is admitted, at most [`HELD_SAMPLES_MAX`] times; one `agent_settled`, only after an
    /// assistant message that is not a tool request; every dialog with an
    /// ID cancelled at once, one without an ID protocol.
    async fn admit(
        serving: &mut Serving<'_, Self>,
        record: Record,
    ) -> Result<Option<PiItem>, Failed> {
        let turn = serving.turn;
        let lane = &mut serving.lane;
        let item = match record {
            Record::Response(reply) => lane.reply(turn, &reply)?,
            Record::UiRequest(request) => return Self::decline(serving, request).await,
            Record::CompactionEnd(_) | Record::UsageHidden
                if !matches!(lane.phase, Phase::Started | Phase::Settled) =>
            {
                lane.held += 1;
                if lane.held > HELD_SAMPLES_MAX {
                    return Err(protocol(turn, "too many compactions before started").into());
                }
                PiItem::Record(record)
            }
            Record::CompactionEnd(_) | Record::UsageHidden | Record::Activity => {
                PiItem::Record(record)
            }
            _ if !matches!(lane.phase, Phase::Started | Phase::Settled) => {
                return Err(
                    protocol(turn, "a run record before the prompt's started reply").into(),
                );
            }
            Record::Settled => {
                if lane.phase == Phase::Settled {
                    return Err(protocol(turn, "a second agent_settled").into());
                }
                match lane.assistant.as_deref() {
                    None => {
                        return Err(
                            protocol(turn, "agent_settled with no assistant message").into()
                        );
                    }
                    Some(last) if last.stop_reason == "toolUse" => {
                        return Err(protocol(
                            turn,
                            "a toolUse terminal: no terminating tool exists",
                        )
                        .into());
                    }
                    Some(_) => {}
                }
                lane.phase = Phase::Settled;
                PiItem::Settled
            }
            Record::MessageEnd(MessageEnd::Assistant(message)) => {
                if lane.phase == Phase::Settled {
                    return Err(protocol(turn, "an assistant message after agent_settled").into());
                }
                lane.assistant = Some(message.clone());
                PiItem::Record(Record::MessageEnd(MessageEnd::Assistant(message)))
            }
            Record::MessageEnd(MessageEnd::System(patch)) => {
                let tools = &lane.expect.tools;
                if patch.tools_added.iter().any(|name| !tools.contains(name))
                    || patch.tools_removed.iter().any(|name| tools.contains(name))
                {
                    return Err(protocol(
                        turn,
                        "a system patch whose tool loadout differs from --tools",
                    )
                    .into());
                }
                PiItem::Record(Record::MessageEnd(MessageEnd::System(patch)))
            }
            record @ (Record::Lifecycle
            | Record::MessageStart(_)
            | Record::MessageEnd(MessageEnd::Other)
            | Record::MessageUpdate { .. }
            | Record::ToolStart { .. }
            | Record::ToolEnd { .. }) => PiItem::Record(record),
        };
        Ok(Some(item))
    }

    fn terminal(message: &PiItem) -> Option<Settled> {
        matches!(message, PiItem::Settled | PiItem::Refused).then_some(Settled)
    }

    /// Kept at admission, where the record is owned.
    fn retain(_serving: &mut Serving<'_, Self>, _terminal: &Settled) {}

    /// Packet §7.1 step 3 on every path that stops reading after the
    /// terminal: an unanswered marker is dropped, and the order that sent
    /// the abort decides the turn ([`Serving::interrupted`]).
    fn unanswered(serving: &mut Serving<'_, Self>) -> Option<Failed> {
        if !serving.lane.unanswered_marker(serving.interrupt) {
            return None;
        }
        serving.lane.assistant = None;
        Some(serving.interrupted())
    }

    /// Packet §2.1 step 3: the three commands, their replies, and every
    /// check before the prompt. A mismatched session is handed on before
    /// the turn fails.
    async fn handshake(
        serving: &mut Serving<'_, Self>,
        messages: &mut WireMessages,
    ) -> Result<(), Failed> {
        let turn = serving.turn;
        send_commands(serving).await?;
        read_replies(serving, messages).await?;
        match serving.lane.check(turn) {
            Ok(()) => {
                // The prompt's reply pairs with the ID `start_message`
                // writes.
                serving.lane.request(turn, "prompt");
                Ok(())
            }
            Err(Ok(mismatch)) => {
                let PiItem::Mismatch {
                    requested,
                    returned,
                } = &mismatch
                else {
                    return Err(protocol(turn, "a handshake check failed").into());
                };
                let cause = RouteError::ResumeMismatch {
                    turn,
                    requested: requested.clone(),
                    returned: returned.clone(),
                };
                let made = serving.hop.made(mismatch);
                serving.hold((made, 0));
                serving.flush().await?;
                Err(cause.into())
            }
            Err(Err(cause)) => Err(cause.into()),
        }
    }

    /// Packet §7.1: the abort under an ID Route allocates.
    fn interrupt(serving: &mut Serving<'_, Self>) -> OutboundMessage {
        let turn = serving.turn;
        let id = serving.lane.request(turn, "abort");
        OutboundMessage::Interrupt(command(&id, "abort"))
    }

    fn bounded_exit(&self) -> bool {
        false
    }

    /// C2 A1: the stall aborts the run, which kills Pi's tool groups.
    fn interrupts_on_stall(&self) -> bool {
        true
    }

    /// Packet §7.1 step 3: an abort sent before `agent_settled` keeps
    /// stdin open until its reply, or the active cutoff
    /// ([`Serving::cutoff`]: the stop order's `force_at`, else the
    /// Adapter's stall's, else the wall); with no reply by then, a marker
    /// terminal is not retained and the order that sent the abort decides
    /// the turn ([`Self::unanswered`]), however the wait ended (the
    /// cutoff, Pi's exit, or a failure, which stays the turn's cause). An
    /// ordinary terminal is retained. Then stdin EOF (packet §7.2).
    async fn after_terminal(
        serving: &mut Serving<'_, Self>,
        messages: &mut WireMessages,
        terminal: Settled,
    ) -> Result<AfterTerminal<Self>, Failed> {
        let (bound, _) = serving.cutoff();
        while serving.lane.phase != Phase::Refused
            && serving.lane.abort_answered.is_none()
            && matches!(serving.interrupt, Interrupt::Queued | Interrupt::Written)
        {
            let failed =
                match tokio::time::timeout_at(bound.instant(), serving.next(messages)).await {
                    Ok(Ok(Next::Message(message))) => {
                        serving.hold(message);
                        continue;
                    }
                    Ok(Ok(Next::Eof | Next::Unterminated)) | Err(_) => None,
                    Ok(Err(failed)) => Some(failed),
                };
            // No reply will come. A marker is not retained: the order's
            // row (C1 §7.6).
            if let Some(stopped) = Self::unanswered(serving) {
                return Err(failed.unwrap_or(stopped));
            }
            if let Some(failed) = failed {
                return Err(failed);
            }
            break;
        }
        Ok(AfterTerminal::Finalize(terminal))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "uninhabited: no Pi turn keeps a server, and std::future::ready of it is unreachable code"
    )]
    async fn kept(
        _serving: &mut Serving<'_, Self>,
        _sender: &WireSender,
        _messages: WireMessages,
        kept: Infallible,
    ) -> Result<PiRouteResult, RouteFailure> {
        match kept {}
    }

    fn keeps_server(_serving: &Serving<'_, Self>, _cause: &RouteError) -> Option<Infallible> {
        None
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "uninhabited: no Pi turn keeps a server, and std::future::ready of it is unreachable code"
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Packet §7.4, decision C-3 (review r1 #2): a failed or late read is
    /// the turn's Store failure, not committed, whatever kind the Store
    /// reported (a dropped read reply is `UncertainCommit`); only an
    /// absence proof whose commit is uncertain is `uncertain`.
    #[test]
    fn predecessor_failures_keep_reads_apart_from_uncertain_commits() {
        let turn = TurnNumber::try_from(3).unwrap();
        let kind = |error: HostError| {
            if let RouteError::Store { kind, .. } = predecessor_cause(turn, &WireError::Host(error))
            {
                Some(kind)
            } else {
                None
            }
        };
        for read in [
            via_wire::StoreFailureKind::UncertainCommit,
            via_wire::StoreFailureKind::Write,
            via_wire::StoreFailureKind::Quota,
        ] {
            assert_eq!(
                kind(HostError::StoreUnavailable(read)),
                Some(StoreFailure::NotCommitted),
                "{read:?}"
            );
        }
        assert_eq!(
            kind(HostError::Store("session anchor read timed out")),
            Some(StoreFailure::NotCommitted)
        );
        assert_eq!(
            kind(HostError::Journal {
                site: via_wire::JournalSite::Absence,
                uncertain: false,
            }),
            Some(StoreFailure::NotCommitted)
        );
        assert_eq!(
            kind(HostError::Journal {
                site: via_wire::JournalSite::Absence,
                uncertain: true,
            }),
            Some(StoreFailure::Uncertain)
        );
        assert!(matches!(
            predecessor_cause(turn, &WireError::Host(HostError::Deadline)),
            RouteError::Deadline { .. }
        ));
    }

    fn lane() -> PiLane {
        let (end, _rx) = oneshot::channel();
        PiLane::open((
            PiExpect {
                session_id: "s".to_owned(),
                provider: "openai".to_owned(),
                model_id: "m".to_owned(),
                thinking: None,
                tools: vec!["read".to_owned()],
            },
            end,
        ))
    }

    fn reply(id: &str, command: &str) -> Response {
        Response {
            id: Some(id.to_owned()),
            command: command.to_owned(),
            success: true,
            data: None,
        }
    }

    /// Packet §5.1: a reply pairs once, by ID and command.
    #[test]
    fn replies_pair_by_id_and_command() {
        let turn = TurnNumber::try_from(3).unwrap();
        let mut lane = lane();
        let id = lane.request(turn, "abort");
        assert_eq!(id, "via-3-abort");
        assert!(lane.pair(&reply(&id, "prompt")).is_err(), "another command");
        let id = lane.request(turn, "abort");
        assert_eq!(lane.pair(&reply(&id, "abort")), Ok("abort"));
        assert!(lane.pair(&reply(&id, "abort")).is_err(), "a second reply");
        assert!(lane.pair(&reply("x", "parse")).is_err(), "parse");
    }

    /// Packet §7.1: only the two markers.
    #[test]
    fn markers_are_exact() {
        let end = |stop: &str, error: Option<&str>| AssistantEnd {
            text: Vec::new(),
            stop_reason: stop.to_owned(),
            error_message: error.map(str::to_owned),
            usage: super::super::messages::Usage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning: None,
                total: 0,
                cost: 0.0,
            },
        };
        assert!(is_marker(&end("aborted", None)));
        assert!(is_marker(&end("error", Some("This operation was aborted"))));
        assert!(!is_marker(&end(
            "error",
            Some("This operation was aborted.")
        )));
        assert!(!is_marker(&end("pending", None)));
        assert!(!is_marker(&end("stop", None)));
    }
}
