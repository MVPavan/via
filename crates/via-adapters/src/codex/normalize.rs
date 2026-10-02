//! Codex events as C2 observations (vendors/codex.md §3, §5; C2 §4, §6.2):
//! the handshake version, a `model/list` page, the no-grant declines and
//! one turn's notifications.

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the server driver uses it (x.3.2 X2, X3)")
)]

use serde_json::json;
use std::collections::HashSet;

use serde_json::value::RawValue;
use tokio::time::Instant;
use via_routes::codex::{
    CodexErrorInfo, DeclineTable, Item, ItemKind, JsonTextError, ModelListResult, Notification,
    ServerRequest, TokenBreakdown, Turn, TurnError, TurnStatus, json_text,
};

use super::plan::CHECKED;
use crate::observation::{
    ClassHint, Decline, Denial, DenialKind, Observation, ProgressMarks, StopReason, UsageSample,
    VendorTerminal,
};
use crate::plan::VersionStatus;
use crate::{VendorTerminalStatus, final_text_pieces};

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
pub(crate) fn catalog_page(page: ModelListResult) -> CatalogPage {
    CatalogPage {
        models: page
            .data
            .into_iter()
            .map(|model| DiscoveredModel {
                model: model.model,
                efforts: model
                    .supported_reasoning_efforts
                    .into_iter()
                    .map(|option| option.reasoning_effort)
                    .collect(),
                hidden: model.hidden,
                default: model.is_default,
            })
            .collect(),
        next_cursor: page.next_cursor,
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

/// The most item IDs each per-turn set admits (packet §5: 1024 entries).
const TRACKED_ITEMS_MAX: usize = 1024;

/// The ID bytes all the sets together admit (packet §5: 256 KiB per
/// session). The first ID past either bound is an explicit overflow
/// (review r2 #2): nothing continues with inaccurate correlation.
const TRACKED_BYTES_MAX: usize = 256 * 1024;

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

/// A set of item IDs admitted by count ([`TRACKED_ITEMS_MAX`]) and by
/// bytes, against the shared [`TRACKED_BYTES_MAX`].
#[derive(Default)]
struct Ids {
    ids: HashSet<String>,
}

impl Ids {
    /// Admits `id`, charging its bytes to `bytes`; an ID already held is
    /// admitted free.
    fn admit(&mut self, id: &str, bytes: &mut usize) -> Result<(), NormalizeError> {
        if self.ids.contains(id) {
            return Ok(());
        }
        let charged = bytes.saturating_add(id.len());
        if self.ids.len() >= TRACKED_ITEMS_MAX || charged > TRACKED_BYTES_MAX {
            return Err(NormalizeError::Overflow);
        }
        self.ids.insert(id.to_owned());
        *bytes = charged;
        Ok(())
    }

    /// Releases `id` and its bytes.
    fn release(&mut self, id: &str, bytes: &mut usize) {
        if self.ids.remove(id) {
            *bytes = bytes.saturating_sub(id.len());
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }
}

/// One turn's normalizer: it keeps the structured-output text, the
/// thread's usage, the open tools and the denials so far.
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
    /// Tool items started and not completed.
    open_tools: Ids,
    /// Items whose approval request VIA declined.
    declined_by_via: Ids,
    /// Items already reported denied.
    denied: Ids,
    /// The ID bytes the three sets hold.
    id_bytes: usize,
    /// An ID overflowed: the normalizer takes nothing more.
    overflowed: bool,
}

impl TurnNormalizer {
    pub(crate) fn new(schema: bool) -> Self {
        Self {
            schema,
            answer: Answer::Text(String::new()),
            total: None,
            cache_write: CacheWrite::None,
            window: None,
            open_tools: Ids::default(),
            declined_by_via: Ids::default(),
            denied: Ids::default(),
            id_bytes: 0,
            overflowed: false,
        }
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
            Notification::ItemStarted(event) => self
                .item_started(&event.item)?
                .map_or(Step::Activity, progress),
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
    pub(crate) fn retained(&self) -> usize {
        match &self.answer {
            Answer::Text(text) => text.len(),
            Answer::OverLimit => 0,
        }
    }

    /// Records a request [`DECLINES`] answered, so the item's declined
    /// status is not reported again as a vendor denial (C2: VIA's own
    /// decline is `vendor.request_declined` only).
    /// An ID that cannot be admitted is an overflow, as in
    /// [`Self::observe`].
    pub(crate) fn note_decline(&mut self, request: &ServerRequest) -> Result<(), NormalizeError> {
        if self.overflowed {
            return Err(NormalizeError::Overflow);
        }
        if let Some(item) = &request.item_id {
            let admitted = self.declined_by_via.admit(item, &mut self.id_bytes);
            self.overflowed = admitted.is_err();
            admitted?;
        }
        Ok(())
    }

    /// Whether a tool item started and has not completed.
    pub(crate) fn tools_open(&self) -> bool {
        !self.open_tools.ids.is_empty()
    }

    /// The marks of an item's start: model output, and a tool's ID and type.
    fn item_started(&mut self, item: &Item) -> Result<Option<ProgressMarks>, NormalizeError> {
        let tools_started = if item.kind.is_tool() {
            self.open_tools.admit(&item.id, &mut self.id_bytes)?;
            vec![(item.id.clone(), item.kind.as_str().to_owned())]
        } else if matches!(item.kind, ItemKind::AgentMessage | ItemKind::Reasoning) {
            Vec::new()
        } else {
            return Ok(None);
        };
        Ok(Some(ProgressMarks {
            model: true,
            tools_started,
            ..ProgressMarks::default()
        }))
    }

    fn item_completed(&mut self, item: &Item) -> Result<Step, NormalizeError> {
        if item.kind.is_tool() {
            self.open_tools.release(&item.id, &mut self.id_bytes);
            let mut observations = vec![Observation::Progress(ProgressMarks {
                tools_ended: vec![item.id.clone()],
                ..ProgressMarks::default()
            })];
            observations.extend(self.denial(item)?.map(Observation::ActionDenied));
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

    /// A command or file-change item the vendor declined, once per item,
    /// unless VIA's own decline caused it.
    fn denial(&mut self, item: &Item) -> Result<Option<Denial>, NormalizeError> {
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
            | ItemKind::Other(_) => return Ok(None),
        };
        if item.status.as_deref() != Some("declined")
            || self.declined_by_via.contains(&item.id)
            || self.denied.contains(&item.id)
        {
            return Ok(None);
        }
        self.denied.admit(&item.id, &mut self.id_bytes)?;
        Ok(Some(Denial {
            kind,
            target: item.target.clone().unwrap_or_default(),
            reason: DENIAL_REASON.to_owned(),
        }))
    }

    fn terminal(&mut self, turn: &Turn, at: Instant) -> Result<Step, NormalizeError> {
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
        let terminal = VendorTerminal {
            at,
            status,
            stop_reason,
            vendor_stop_reason: vendor_stop_reason.to_owned(),
            vendor_code: info.map(|info| info.kind.clone()),
            class_hint: failed.then(|| class_hint(info)),
            detail: error.map(detail),
            structured_output: None,
            steps: None,
            usage: None,
            cost: None,
            vendor: self.vendor(),
        };
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
