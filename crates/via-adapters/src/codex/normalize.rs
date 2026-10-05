//! Codex events as C2 observations (vendors/codex.md §3, §5; C2 §4, §6.2):
//! the handshake version, a `model/list` page, the no-grant declines and
//! one turn's notifications.

use serde_json::json;
use std::collections::{HashMap, hash_map};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::value::RawValue;
use tokio::time::Instant;
use via_routes::codex::{
    CodexErrorInfo, DeclineTable, Item, ItemKind, JsonTextError, Model, ModelListResult,
    Notification, ServerRequest, TokenBreakdown, Turn, TurnError, TurnStatus, json_text,
};

use super::plan::CHECKED;
use crate::observation::{
    Charge, ClassHint, Decline, Denial, DenialKind, Observation, ProgressMarks, SessionCap,
    StopReason, UsageSample, VendorTerminal,
};
use crate::plan::VersionStatus;
use crate::{TurnNumber, VendorTerminalStatus, final_text_pieces};

/// The prefix VIA's own `clientInfo.name` puts on `userAgent`.
const USER_AGENT_PREFIX: &str = "via/";

/// A failure detail's bound, in bytes.
const DETAIL_MAX: usize = 1024;

/// The only agent-message phase whose text is the turn's answer.
const FINAL_ANSWER: &str = "final_answer";

/// The six server requests VIA answers with a no-grant body (packet §4,
/// `codex_never_ask`); each body validates against its 0.157.1 response
/// schema. Every other request gets `-32601`.
pub(crate) const DECLINES: DeclineTable = DeclineTable::new(&[
    (
        "item/commandExecution/requestApproval",
        r#"{"decision":"decline"}"#,
    ),
    (
        "item/fileChange/requestApproval",
        r#"{"decision":"decline"}"#,
    ),
    ("item/permissions/requestApproval", r#"{"permissions":{}}"#),
    ("item/tool/requestUserInput", r#"{"answers":{}}"#),
    (
        "mcpServer/elicitation/request",
        r#"{"action":"decline","content":null}"#,
    ),
    ("item/tool/call", r#"{"contentItems":[],"success":false}"#),
]);

/// The instance version in an `initialize` `userAgent` (C2 §5 OD1): the
/// text after VIA's `via/` prefix, up to the first space. Any other shape
/// is no version.
pub(crate) fn instance_version(user_agent: &str) -> Option<&str> {
    let rest = user_agent.strip_prefix(USER_AGENT_PREFIX)?;
    let version = rest.split(' ').next().unwrap_or_default();
    (!version.is_empty()).then_some(version)
}

/// Whether `version` is in the adapter's `checked` set.
pub(crate) fn version_status(version: &str) -> VersionStatus {
    if CHECKED.contains(&version) {
        VersionStatus::Tested
    } else {
        VersionStatus::Untested
    }
}

/// One `model/list` page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatalogPage {
    /// The page's models, in the vendor's order.
    pub(crate) models: Vec<DiscoveredModel>,
    /// The cursor of the next page; `None` on the last.
    pub(crate) next_cursor: Option<String>,
}

/// A model as `model/list` advertises it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiscoveredModel {
    /// The model name `thread/start` takes.
    pub(crate) model: String,
    /// Its supported reasoning efforts, vendor values.
    pub(crate) efforts: Vec<String>,
    /// Hidden from the vendor's own picker.
    pub(crate) hidden: bool,
    /// The vendor's default model.
    pub(crate) default: bool,
}

impl DiscoveredModel {
    /// Whether the model advertises `effort`.
    pub(crate) fn supports(&self, effort: &str) -> bool {
        self.efforts.iter().any(|known| known == effort)
    }
}

/// The models and next cursor of one `model/list` reply.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Routes follows the pages; the driver maps each model"
    )
)]
pub(crate) fn catalog_page(page: ModelListResult) -> CatalogPage {
    CatalogPage {
        models: page.data.iter().map(discovered).collect(),
        next_cursor: page.next_cursor,
    }
}

/// A `model/list` model as the adapter keeps it.
pub(crate) fn discovered(model: &Model) -> DiscoveredModel {
    DiscoveredModel {
        model: model.model.clone(),
        efforts: model
            .supported_reasoning_efforts
            .iter()
            .map(|option| option.reasoning_effort.clone())
            .collect(),
        hidden: model.hidden,
        default: model.is_default,
    }
}

/// `vendor.request_declined` for a request [`DECLINES`] answers. The
/// summary is fixed text: request parameters never reach the journal.
pub(crate) fn decline(request: &ServerRequest) -> Decline {
    Decline {
        vendor_method: request.method.clone(),
        summary: "VIA grants no approval, input or tool call".to_owned(),
        blocking: true,
    }
}

/// What a turn's structured output is when the turn ends (packet §3: the
/// nonempty final text is the output), as the C2 amendment of review r1
/// types `VendorTerminal.structured_output`; until that join lands here,
/// the normalizer returns it beside the terminal.
#[derive(Debug)]
pub(crate) enum StructuredOutput {
    /// No schema was requested.
    NotRequested,
    /// A schema was requested and the final text is absent or empty.
    Missing,
    /// The final text, parsed.
    Json(Box<RawValue>),
    /// A schema was requested and the final text is not JSON.
    NotJson,
    /// A schema was requested and the final text passed the 4 MiB
    /// retention bound or the JSON structure limits (`validation_limit`).
    OverLimit,
}

impl PartialEq for StructuredOutput {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Json(left), Self::Json(right)) => left.get() == right.get(),
            (Self::NotRequested, Self::NotRequested)
            | (Self::Missing, Self::Missing)
            | (Self::NotJson, Self::NotJson)
            | (Self::OverLimit, Self::OverLimit) => true,
            (
                Self::NotRequested
                | Self::Missing
                | Self::Json(_)
                | Self::NotJson
                | Self::OverLimit,
                _,
            ) => false,
        }
    }
}

/// What one notification of the turn amounts to.
#[derive(Debug)]
pub(crate) enum Step {
    /// Observations to send.
    Observations(Vec<Observation>),
    /// The turn's one vendor terminal, with its structured output.
    Terminal {
        /// The terminal.
        terminal: Box<VendorTerminal>,
        /// The structured output, when one was requested.
        structured: StructuredOutput,
    },
    /// Vendor activity with nothing to report.
    Activity,
}

/// The most structured-output text a turn retains (review r1 #1): past
/// it the turn's output is `OverLimit`. An E2E measurement item, not a
/// qualified figure.
const STRUCTURED_MAX: usize = 4 * 1024 * 1024;

/// Why the normalizer cannot take a notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NormalizeError {
    /// A known notification contradicts the protocol, or counts pass
    /// their range.
    Protocol(&'static str),
    /// An item ID could not be admitted to its bounded set: the driver
    /// routes it through the health path as the per-thread ingress
    /// overflow. The normalizer takes nothing after it.
    Overflow,
}

/// The reason of every vendor denial (C1 Q9, as the Claude adapter words it).
const DENIAL_REASON: &str = "denied by the vendor's permission policy";

/// The final-answer text retained for structured output.
enum Answer {
    /// Text so far, within [`STRUCTURED_MAX`].
    Text(String),
    /// The text passed [`STRUCTURED_MAX`] and was dropped.
    OverLimit,
}

/// The turn's cache-write input tokens over its samples (review r1 #9):
/// unavailable once any sample lacks the count, never reported as 0.
#[derive(Clone, Copy)]
enum CacheWrite {
    /// No sample yet.
    None,
    /// Every sample had the count; their sum.
    Sum(u64),
    /// A sample lacked it.
    Unavailable,
}

/// What the session keeps of one item of one turn (x.3.2 X3 fix r4 #2,
/// #3; X3 §6.1).
#[derive(Clone, Copy, Default)]
struct Suppressed {
    /// A tool item started and not completed.
    open: bool,
    /// VIA declined its approval request: its `declined` status is no
    /// vendor denial.
    declined_by_via: bool,
    /// Its vendor denial was reported: it is not reported again.
    denied: bool,
}

/// One ledger entry: its facts and its charge (x.3.2 X3 §6.2).
struct Entry {
    facts: Suppressed,
    _charge: Charge,
}

/// Turns mapped to Core, adjacent numbers from `first` to `last` (x.3.2
/// X3 §3.4), with one charge: the credit of the turn that opened it, once
/// that turn closed.
struct Range {
    first: TurnNumber,
    last: TurnNumber,
    charge: Option<Charge>,
}

/// One session's item metadata on a registration (packet §5; x.3.2 X3
/// fix r4 #2, #3; X3 §6): open tools and the suppression table, keyed by
/// owning turn and item ID, and the turns mapped to Core, kept for as long
/// as the registration lives. Each entry and each range is charged to the
/// session's cap and observation budget (§6.2). An open tool's entry
/// survives its turn's settlement until the tool completes. A full cap is
/// an overflow, for good: nothing continues with inaccurate correlation,
/// and nothing is forgotten silently.
pub(crate) struct Metadata {
    cap: SessionCap,
    exhausted: bool,
    /// The registration retired (x.3.2 X3 §6.5): nothing commits again.
    retired: bool,
    suppressed: HashMap<(TurnNumber, String), Entry>,
    mapped: Vec<Range>,
    /// The charge the consumer reserved for the message it handles.
    staged: Option<Charge>,
}

/// A registration's metadata, in registration storage (x.3.2 X3 §6.1):
/// its consumer and the running turn's normalizer read and write it
/// there, never across an await.
pub(crate) type Ledger = Arc<Mutex<Metadata>>;

/// An empty ledger charged to `cap`.
pub(crate) fn ledger_on(cap: SessionCap) -> Ledger {
    Arc::new(Mutex::new(Metadata {
        cap,
        exhausted: false,
        retired: false,
        suppressed: HashMap::new(),
        mapped: Vec::new(),
        staged: None,
    }))
}

/// The ledger's metadata, locked.
pub(crate) fn ledger(ledger: &Ledger) -> MutexGuard<'_, Metadata> {
    // Each update is a few assignments: consistent across a panic.
    ledger.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The denial class of a command or file-change item the vendor declined.
fn denied_kind(item: &Item) -> Option<DenialKind> {
    let kind = match item.kind {
        ItemKind::CommandExecution => DenialKind::Command,
        ItemKind::FileChange => DenialKind::FileWrite,
        ItemKind::UserMessage
        | ItemKind::AgentMessage
        | ItemKind::Reasoning
        | ItemKind::McpToolCall
        | ItemKind::DynamicToolCall
        | ItemKind::CollabAgentToolCall
        | ItemKind::WebSearch
        | ItemKind::ImageGeneration
        | ItemKind::Sleep
        | ItemKind::Other(_) => return None,
    };
    (item.status.as_deref() == Some("declined")).then_some(kind)
}

impl Metadata {
    /// x.3.2 X3 §6.3, in every state: turn `turn`'s tool item started is
    /// open (charged once); completed, it is no longer, and its entry is
    /// released unless it is marked or its denial is still to be judged in
    /// it (§6.2: the entry's charge carries the denial).
    pub(crate) fn track(
        &mut self,
        turn: TurnNumber,
        notification: &Notification,
    ) -> Result<(), NormalizeError> {
        let (Notification::ItemStarted(event) | Notification::ItemCompleted(event)) = notification
        else {
            return Ok(());
        };
        let item = &event.item;
        if !item.kind.is_tool() {
            return Ok(());
        }
        if matches!(notification, Notification::ItemStarted(_)) {
            self.entry(turn, &item.id)?.open = true;
            return Ok(());
        }
        let key = (turn, item.id.clone());
        if let Some(entry) = self.suppressed.get_mut(&key) {
            entry.facts.open = false;
            if !entry.facts.declined_by_via && !entry.facts.denied && denied_kind(item).is_none() {
                self.suppressed.remove(&key);
            }
        }
        Ok(())
    }

    /// x.3.2 X3 §6.2: the key bytes of the entry turn `turn`'s
    /// `notification` may insert, when it holds none to keep: the consumer
    /// reserves its charge first ([`Self::stage`]).
    pub(crate) fn wants(&self, turn: TurnNumber, notification: &Notification) -> Option<usize> {
        let (Notification::ItemStarted(event) | Notification::ItemCompleted(event)) = notification
        else {
            return None;
        };
        let item = &event.item;
        let held = self
            .suppressed
            .get(&(turn, item.id.clone()))
            .map(|entry| entry.facts);
        let wanted = if matches!(notification, Notification::ItemStarted(_)) {
            item.kind.is_tool() && held.is_none()
        } else {
            // A denial is judged in the item's entry, inserted only when
            // it has none.
            denied_kind(item).is_some() && held.is_none()
        };
        wanted.then_some(item.id.len())
    }

    /// [`Self::wants`] for a request [`DECLINES`] answered for turn `turn`.
    pub(crate) fn wants_decline(&self, turn: TurnNumber, request: &ServerRequest) -> Option<usize> {
        request
            .item_id
            .as_ref()
            .filter(|item| !self.suppressed.contains_key(&(turn, (*item).clone())))
            .map(String::len)
    }

    /// Holds `charge` for the next entry the message being handled
    /// inserts; [`Self::unstage`] releases it if none did.
    pub(crate) fn stage(&mut self, charge: Charge) {
        self.staged = Some(charge);
    }

    /// Releases a staged charge no entry took.
    pub(crate) fn unstage(&mut self) {
        self.staged = None;
    }

    /// The cap the entries are charged to.
    pub(crate) fn cap(&self) -> &SessionCap {
        &self.cap
    }

    /// Exhausted for good: the cap had no slot.
    pub(crate) fn exhaust(&mut self) {
        self.exhausted = true;
    }

    /// x.3.2 X3 §3.4: turn `turn`'s `Accepted` went out, so it is mapped
    /// to Core. An exactly adjacent range extends; otherwise `turn` opens
    /// a range, charged at its close ([`Self::close`]).
    pub(crate) fn map(&mut self, turn: TurnNumber) {
        if self.retired {
            return;
        }
        let adjacent = self
            .mapped
            .iter_mut()
            .find(|range| range.last.get().checked_add(1) == Some(turn.get()));
        match adjacent {
            Some(range) => range.last = turn,
            None => self.mapped.push(Range {
                first: turn,
                last: turn,
                charge: None,
            }),
        }
    }

    /// Turn `turn` closed: its `credit` becomes the charge of the range
    /// it opened, else is released (x.3.2 X3 §3.4, r8 #6).
    pub(crate) fn close(&mut self, turn: TurnNumber, credit: Charge) {
        if let Some(range) = self
            .mapped
            .iter_mut()
            .find(|range| range.first == turn && range.charge.is_none())
        {
            range.charge = Some(credit);
        }
    }

    /// Whether turn `turn` was mapped to Core.
    pub(crate) fn mapped(&self, turn: TurnNumber) -> bool {
        self.mapped
            .iter()
            .any(|range| range.first <= turn && turn <= range.last)
    }

    /// Whether a tool item of turn `turn` started and has not completed.
    pub(crate) fn tools_open(&self, turn: TurnNumber) -> bool {
        self.suppressed
            .iter()
            .any(|((owner, _), entry)| *owner == turn && entry.facts.open)
    }

    /// Whether any tool item of any turn is open.
    pub(crate) fn has_open(&self) -> bool {
        self.suppressed.values().any(|entry| entry.facts.open)
    }

    /// x.3.2 X3 §6.5 step 5: the registration retired. Its entries, its
    /// ranges and a staged charge are released, and nothing commits again.
    pub(crate) fn retire(&mut self) {
        self.retired = true;
        self.suppressed.clear();
        self.mapped.clear();
        self.staged = None;
    }

    /// The entries charged.
    #[cfg(test)]
    pub(crate) fn entries(&self) -> usize {
        self.suppressed.len()
    }

    /// Item `item` of turn `turn`'s facts, charged once: by the staged
    /// charge, else by one taken now.
    fn entry(&mut self, turn: TurnNumber, item: &str) -> Result<&mut Suppressed, NormalizeError> {
        if self.exhausted || self.retired {
            return Err(NormalizeError::Overflow);
        }
        match self.suppressed.entry((turn, item.to_owned())) {
            hash_map::Entry::Occupied(entry) => Ok(&mut entry.into_mut().facts),
            hash_map::Entry::Vacant(vacant) => {
                let charge = if let Some(charge) = self.staged.take() {
                    charge
                } else {
                    let Some(slot) = self.cap.slot(item.len()) else {
                        self.exhausted = true;
                        return Err(NormalizeError::Overflow);
                    };
                    self.cap
                        .try_charge(slot)
                        .map_err(|_| NormalizeError::Overflow)?
                };
                Ok(&mut vacant
                    .insert(Entry {
                        facts: Suppressed::default(),
                        _charge: charge,
                    })
                    .facts)
            }
        }
    }

    /// Records a request [`DECLINES`] answered for turn `turn`, so the
    /// item's declined status is not reported as a vendor denial (C2:
    /// VIA's own decline is `vendor.request_declined` only).
    pub(crate) fn note_decline(
        &mut self,
        turn: TurnNumber,
        request: &ServerRequest,
    ) -> Result<(), NormalizeError> {
        if self.exhausted {
            return Err(NormalizeError::Overflow);
        }
        if let Some(item) = &request.item_id {
            self.entry(turn, item)?.declined_by_via = true;
        }
        Ok(())
    }

    /// Turn `turn`'s notification after it ended (x.3.2 X3 fix r2 #3, r4
    /// #2): the denial of a tool item it completed declined, unless VIA
    /// declined the item or it was reported.
    pub(crate) fn late_denial(
        &mut self,
        turn: TurnNumber,
        notification: &Notification,
    ) -> Result<Option<Denial>, NormalizeError> {
        if self.exhausted {
            return Err(NormalizeError::Overflow);
        }
        let Notification::ItemCompleted(event) = notification else {
            return Ok(None);
        };
        self.denial(turn, &event.item)
    }

    /// A command or file-change item of turn `turn` the vendor declined,
    /// once per item, unless VIA's own decline caused it.
    fn denial(&mut self, turn: TurnNumber, item: &Item) -> Result<Option<Denial>, NormalizeError> {
        let Some(kind) = denied_kind(item) else {
            return Ok(None);
        };
        let suppressed = self
            .suppressed
            .get(&(turn, item.id.clone()))
            .map(|entry| entry.facts)
            .unwrap_or_default();
        if suppressed.declined_by_via || suppressed.denied {
            return Ok(None);
        }
        self.entry(turn, &item.id)?.denied = true;
        Ok(Some(Denial {
            kind,
            target: item.target.clone().unwrap_or_default(),
            reason: DENIAL_REASON.to_owned(),
        }))
    }
}

/// One turn's normalizer: it keeps the structured-output text and the
/// thread's usage; the open tools and the denials so far are in its
/// registration's ledger.
pub(crate) struct TurnNormalizer {
    /// Whether the turn requested an output schema.
    schema: bool,
    /// The final-answer text, retained only with a schema.
    answer: Answer,
    /// The thread's latest cumulative usage.
    total: Option<TokenBreakdown>,
    /// The turn's cache-write input tokens.
    cache_write: CacheWrite,
    /// The model's context window, as last reported.
    window: Option<u64>,
    /// The turn the normalizer is for.
    turn: TurnNumber,
    /// Its registration's metadata.
    ledger: Ledger,
    /// An ID overflowed: the normalizer takes nothing more.
    overflowed: bool,
}

impl TurnNormalizer {
    /// A normalizer of a session's only turn, with its own ledger.
    #[cfg(test)]
    pub(crate) fn new(schema: bool) -> Self {
        Self::on(
            schema,
            TurnNumber::try_from(1).unwrap_or_else(|_| unreachable!()),
            ledger_on(SessionCap::new(
                &crate::observation::observation_channel().0,
            )),
        )
    }

    /// Turn `turn`'s normalizer over its registration's `ledger`.
    pub(crate) fn on(schema: bool, turn: TurnNumber, ledger: Ledger) -> Self {
        Self {
            schema,
            answer: Answer::Text(String::new()),
            total: None,
            cache_write: CacheWrite::None,
            window: None,
            turn,
            ledger,
            overflowed: false,
        }
    }

    /// Its registration's metadata, locked.
    #[cfg(test)]
    pub(crate) fn ledger(&self) -> MutexGuard<'_, Metadata> {
        ledger(&self.ledger)
    }

    /// The step `notification` is; an error for a known notification that
    /// contradicts the protocol, counts past their range, or an ID past
    /// its bounds (then for good).
    pub(crate) fn observe(
        &mut self,
        notification: &Notification,
        at: Instant,
    ) -> Result<Step, NormalizeError> {
        if self.overflowed {
            return Err(NormalizeError::Overflow);
        }
        let step = self.step(notification, at);
        self.overflowed = matches!(step, Err(NormalizeError::Overflow));
        step
    }

    fn step(&mut self, notification: &Notification, at: Instant) -> Result<Step, NormalizeError> {
        let progress =
            |marks: ProgressMarks| Step::Observations(vec![Observation::Progress(marks)]);
        Ok(match notification {
            Notification::ItemStarted(event) => {
                Self::item_started(&event.item).map_or(Step::Activity, progress)
            }
            Notification::ItemCompleted(event) => self.item_completed(&event.item)?,
            Notification::AgentMessageDelta(_) | Notification::ReasoningDelta(_) => {
                progress(ProgressMarks {
                    model: true,
                    ..ProgressMarks::default()
                })
            }
            Notification::TokenUsage(event) => {
                let last = event.usage.last;
                self.cache_write = match (self.cache_write, last.cache_write_input_tokens) {
                    (CacheWrite::Unavailable, _) | (_, None) => CacheWrite::Unavailable,
                    (CacheWrite::None, Some(count)) => CacheWrite::Sum(count),
                    (CacheWrite::Sum(sum), Some(count)) => {
                        CacheWrite::Sum(sum.checked_add(count).ok_or(NormalizeError::Protocol(
                            "cache-write tokens past their range",
                        ))?)
                    }
                };
                self.total = Some(event.usage.total);
                self.window = event.usage.model_context_window.or(self.window);
                progress(ProgressMarks {
                    usage: Some(sample(&last)),
                    ..ProgressMarks::default()
                })
            }
            Notification::TurnCompleted(event) => self.terminal(&event.turn, at)?,
            Notification::TurnStarted(_)
            | Notification::Error(_)
            | Notification::ThreadStatusChanged { .. }
            | Notification::ThreadClosed { .. }
            | Notification::Unknown { .. } => Step::Activity,
        })
    }

    /// Bytes of final text retained for structured output.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the memory measurement reads it (x.3.2 X5)")
    )]
    pub(crate) fn retained(&self) -> usize {
        match &self.answer {
            Answer::Text(text) => text.len(),
            Answer::OverLimit => 0,
        }
    }

    /// The marks of an item's start: model output, and a tool's ID and type.
    fn item_started(item: &Item) -> Option<ProgressMarks> {
        let tools_started = if item.kind.is_tool() {
            vec![(item.id.clone(), item.kind.as_str().to_owned())]
        } else if matches!(item.kind, ItemKind::AgentMessage | ItemKind::Reasoning) {
            Vec::new()
        } else {
            return None;
        };
        Some(ProgressMarks {
            model: true,
            tools_started,
            ..ProgressMarks::default()
        })
    }

    fn item_completed(&mut self, item: &Item) -> Result<Step, NormalizeError> {
        if item.kind.is_tool() {
            let mut observations = vec![Observation::Progress(ProgressMarks {
                tools_ended: vec![item.id.clone()],
                ..ProgressMarks::default()
            })];
            observations.extend(
                ledger(&self.ledger)
                    .denial(self.turn, item)?
                    .map(Observation::ActionDenied),
            );
            return Ok(Step::Observations(observations));
        }
        let (ItemKind::AgentMessage, Some(FINAL_ANSWER), Some(text)) =
            (&item.kind, item.phase.as_deref(), item.text.as_deref())
        else {
            return Ok(Step::Activity);
        };
        if self.schema {
            self.retain(text);
        }
        Ok(Step::Observations(
            final_text_pieces(text)
                .map(|piece| Observation::FinalText(piece.to_owned()))
                .collect(),
        ))
    }

    /// Appends `text` to the structured-output text, within its bound.
    fn retain(&mut self, text: &str) {
        if let Answer::Text(answer) = &mut self.answer {
            if answer.len().saturating_add(text.len()) > STRUCTURED_MAX {
                self.answer = Answer::OverLimit;
            } else {
                answer.push_str(text);
            }
        }
    }

    fn terminal(&mut self, turn: &Turn, at: Instant) -> Result<Step, NormalizeError> {
        let terminal = vendor_terminal(turn, at, self.vendor())?;
        Ok(Step::Terminal {
            terminal: Box::new(terminal),
            structured: self.structured(),
        })
    }

    fn structured(&mut self) -> StructuredOutput {
        if !self.schema {
            return StructuredOutput::NotRequested;
        }
        match std::mem::replace(&mut self.answer, Answer::Text(String::new())) {
            Answer::OverLimit => StructuredOutput::OverLimit,
            Answer::Text(text) if text.is_empty() => StructuredOutput::Missing,
            Answer::Text(text) => match json_text(&text) {
                Ok(json) => StructuredOutput::Json(json),
                Err(JsonTextError::NotJson) => StructuredOutput::NotJson,
                Err(JsonTextError::Limits) => StructuredOutput::OverLimit,
            },
        }
    }

    /// The envelope's `vendor` member: the thread's cumulative usage, the
    /// turn's cache-write tokens and the context window, none of which C1
    /// usage carries.
    fn vendor(&self) -> Option<Box<RawValue>> {
        let total = self.total?;
        let mut vendor = json!({
            "total": {
                "totalTokens": total.total_tokens,
                "inputTokens": total.input_tokens,
                "cachedInputTokens": total.cached_input_tokens,
                "outputTokens": total.output_tokens,
                "reasoningOutputTokens": total.reasoning_output_tokens,
            },
        });
        if let CacheWrite::Sum(cache_write) = self.cache_write {
            vendor["cacheWriteInputTokens"] = cache_write.into();
        }
        if let Some(cache_write) = total.cache_write_input_tokens {
            vendor["total"]["cacheWriteInputTokens"] = cache_write.into();
        }
        if let Some(window) = self.window {
            vendor["modelContextWindow"] = window.into();
        }
        serde_json::value::to_raw_value(&vendor).ok()
    }
}

/// The C2 terminal of `turn/completed`'s `turn`, read at `at`, with the
/// envelope's `vendor` member: a running turn's, or an ended turn's late
/// one (X4 code review r2 #2), which carries no usage of its own.
pub(crate) fn vendor_terminal(
    turn: &Turn,
    at: Instant,
    vendor: Option<Box<RawValue>>,
) -> Result<VendorTerminal, NormalizeError> {
    let (status, stop_reason, vendor_stop_reason) = match turn.status {
        TurnStatus::Completed => (
            VendorTerminalStatus::Completed,
            StopReason::EndTurn,
            "completed",
        ),
        TurnStatus::Interrupted => (
            VendorTerminalStatus::Interrupted,
            StopReason::Interrupted,
            "interrupted",
        ),
        TurnStatus::Failed => (VendorTerminalStatus::Failed, StopReason::Error, "failed"),
        TurnStatus::InProgress => {
            return Err(NormalizeError::Protocol(
                "turn/completed with an inProgress turn",
            ));
        }
    };
    let failed = status == VendorTerminalStatus::Failed;
    let error = turn.error.as_ref();
    let info = error.and_then(|error| error.codex_error_info.as_ref());
    Ok(VendorTerminal {
        at,
        status,
        stop_reason,
        vendor_stop_reason: vendor_stop_reason.to_owned(),
        vendor_code: info.map(|info| info.kind.clone()),
        class_hint: failed.then(|| class_hint(info)),
        detail: error.map(detail),
        structured_output: None,
        structured_output_unparsed: None,
        steps: None,
        usage: None,
        cost: None,
        vendor,
    })
}

/// One model call's usage, keyless: Codex `last` samples add (AD6).
fn sample(last: &TokenBreakdown) -> UsageSample {
    UsageSample {
        key: None,
        input: Some(last.input_tokens),
        cached_input: Some(last.cached_input_tokens),
        output: Some(last.output_tokens),
        reasoning_output: Some(last.reasoning_output_tokens),
        total: Some(last.total_tokens),
    }
}

/// C2 §6.2's Codex row: any HTTP 401 or 403 is `auth`; otherwise by code.
fn class_hint(info: Option<&CodexErrorInfo>) -> ClassHint {
    let Some(info) = info else {
        return ClassHint::VendorError;
    };
    if matches!(info.http_status, Some(401 | 403)) {
        return ClassHint::Auth;
    }
    match info.kind.as_str() {
        "rateLimitExceeded" => ClassHint::RateLimit,
        "unauthorized" => ClassHint::Auth,
        "contextWindowExceeded" => ClassHint::ContextExceeded,
        "usageLimitExceeded" | "sessionBudgetExceeded" => ClassHint::BudgetExceeded,
        _ => ClassHint::VendorError,
    }
}

/// The error's message, bounded at a character boundary.
fn detail(error: &TurnError) -> String {
    let message = &error.message;
    let mut end = message.len().min(DETAIL_MAX);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}
