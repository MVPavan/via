//! Codex events as C2 observations (vendors/codex.md §3, §5; C2 §4, §6.2):
//! the handshake version, a `model/list` page, the no-grant declines and
//! one turn's notifications.

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the server driver uses it (x.3.2 X2, X3)")
)]

use serde_json::json;
use serde_json::value::RawValue;
use tokio::time::Instant;
use via_routes::codex::{
    CodexErrorInfo, DeclineTable, Item, ItemKind, ModelListResult, Notification, ServerRequest,
    TokenBreakdown, Turn, TurnError, TurnStatus,
};

use super::plan::CHECKED;
use crate::observation::{
    ClassHint, Decline, Observation, ProgressMarks, StopReason, UsageSample, VendorTerminal,
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
/// final text is the output). `VendorTerminal.structured_output` holds
/// JSON only, so text that is not JSON cannot travel there; Core needs
/// the difference from no text at all.
#[derive(Debug)]
pub(crate) enum StructuredOutput {
    /// No schema was requested.
    NotRequested,
    /// A schema was requested and the turn produced no final text.
    Missing,
    /// The final text, parsed.
    Json(Box<RawValue>),
    /// A schema was requested and the final text is not JSON.
    NotJson,
}

impl PartialEq for StructuredOutput {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Json(left), Self::Json(right)) => left.get() == right.get(),
            (Self::NotRequested, Self::NotRequested)
            | (Self::Missing, Self::Missing)
            | (Self::NotJson, Self::NotJson) => true,
            (Self::NotRequested | Self::Missing | Self::Json(_) | Self::NotJson, _) => false,
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

/// One turn's normalizer: it keeps the final text and the thread's usage
/// so the terminal can carry them.
pub(crate) struct TurnNormalizer {
    /// Whether the turn requested an output schema.
    schema: bool,
    /// The final-answer text so far.
    final_text: Option<String>,
    /// The thread's latest cumulative usage.
    total: Option<TokenBreakdown>,
    /// The turn's cache-write input tokens, summed over its samples.
    cache_write: u64,
    /// The model's context window, as last reported.
    window: Option<u64>,
}

impl TurnNormalizer {
    pub(crate) fn new(schema: bool) -> Self {
        Self {
            schema,
            final_text: None,
            total: None,
            cache_write: 0,
            window: None,
        }
    }

    /// The step `notification` is; an error for a known notification that
    /// contradicts the protocol.
    pub(crate) fn observe(
        &mut self,
        notification: &Notification,
        at: Instant,
    ) -> Result<Step, &'static str> {
        let progress =
            |marks: ProgressMarks| Step::Observations(vec![Observation::Progress(marks)]);
        Ok(match notification {
            Notification::ItemStarted(event) => {
                item_started(&event.item).map_or(Step::Activity, progress)
            }
            Notification::ItemCompleted(event) => self.item_completed(&event.item),
            Notification::AgentMessageDelta(_) | Notification::ReasoningDelta(_) => {
                progress(ProgressMarks {
                    model: true,
                    ..ProgressMarks::default()
                })
            }
            Notification::TokenUsage(event) => {
                let last = event.usage.last;
                self.total = Some(event.usage.total);
                self.cache_write += last.cache_write_input_tokens.unwrap_or_default();
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

    fn item_completed(&mut self, item: &Item) -> Step {
        if item.kind.is_tool() {
            return Step::Observations(vec![Observation::Progress(ProgressMarks {
                tools_ended: vec![item.id.clone()],
                ..ProgressMarks::default()
            })]);
        }
        let (ItemKind::AgentMessage, Some(FINAL_ANSWER), Some(text)) =
            (&item.kind, item.phase.as_deref(), item.text.as_deref())
        else {
            return Step::Activity;
        };
        self.final_text.get_or_insert_default().push_str(text);
        Step::Observations(
            final_text_pieces(text)
                .map(|piece| Observation::FinalText(piece.to_owned()))
                .collect(),
        )
    }

    fn terminal(&mut self, turn: &Turn, at: Instant) -> Result<Step, &'static str> {
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
            TurnStatus::InProgress => return Err("turn/completed with an inProgress turn"),
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
        match self.final_text.take() {
            None => StructuredOutput::Missing,
            Some(text) => serde_json::from_str::<Box<RawValue>>(&text)
                .map_or(StructuredOutput::NotJson, StructuredOutput::Json),
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
            "cacheWriteInputTokens": self.cache_write,
        });
        if let Some(cache_write) = total.cache_write_input_tokens {
            vendor["total"]["cacheWriteInputTokens"] = cache_write.into();
        }
        if let Some(window) = self.window {
            vendor["modelContextWindow"] = window.into();
        }
        serde_json::value::to_raw_value(&vendor).ok()
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
