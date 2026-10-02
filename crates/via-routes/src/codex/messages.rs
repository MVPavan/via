//! The typed decoding of what a Codex app-server writes (vendors/codex.md
//! §2–§5): newline-delimited JSON-RPC with no `jsonrpc` member. Decoding
//! reads the envelope first, then the method (coding style §3): an
//! unknown notification keeps only its method and thread; a malformed
//! message of a known method is an error, never the fallback.

use serde::de::{DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use thiserror::Error;

use crate::{SHORT_FIELD_MAX, UNKNOWN_TAG_MAX};

/// A JSON-RPC request ID: VIA's own are integers; a server request's is
/// echoed exactly as it came.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric ID.
    Int(i64),
    /// A string ID.
    Str(String),
}

/// What the server wrote that VIA cannot accept as Codex protocol.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("malformed Codex message: {0}")]
pub struct DecodeError(&'static str);

impl DecodeError {
    /// A bounded diagnostic, with no vendor text in it.
    pub fn detail(self) -> &'static str {
        self.0
    }
}

/// One message the server wrote.
#[derive(Debug)]
pub enum Incoming {
    /// The reply to one of VIA's requests.
    Response(Response),
    /// A request VIA must answer.
    Request(ServerRequest),
    /// A notification.
    Notification(Notification),
}

/// The reply to one of VIA's requests: its result, unparsed until the
/// pairing knows the method, or its error.
#[derive(Debug)]
pub struct Response {
    /// The request it answers.
    pub id: RequestId,
    /// The result, or the error.
    pub outcome: Result<Box<RawValue>, RpcError>,
}

/// A JSON-RPC error reply.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct RpcError {
    /// The code; every refusal the re-probe saw was `-32600`.
    pub code: i64,
    /// The vendor's free text.
    pub message: String,
}

/// A server request: VIA answers it by method under its exact ID. Its
/// thread and turn, when its params name them, associate the decline.
#[derive(Debug)]
pub struct ServerRequest {
    /// The ID to answer under.
    pub id: RequestId,
    /// The method.
    pub method: String,
    /// `params.threadId`, when present.
    pub thread_id: Option<String>,
    /// `params.turnId`, when present.
    pub turn_id: Option<String>,
}

/// A notification, typed by method.
#[derive(Debug, Eq, PartialEq)]
pub enum Notification {
    /// `turn/started`.
    TurnStarted(TurnEvent),
    /// `turn/completed`: the turn's terminal.
    TurnCompleted(TurnEvent),
    /// `item/started`.
    ItemStarted(ItemEvent),
    /// `item/completed`.
    ItemCompleted(ItemEvent),
    /// `item/agentMessage/delta`.
    AgentMessageDelta(DeltaEvent),
    /// `item/reasoning/summaryTextDelta` or `item/reasoning/textDelta`.
    ReasoningDelta(DeltaEvent),
    /// `thread/tokenUsage/updated`.
    TokenUsage(TokenUsageEvent),
    /// `error`: a diagnostic, terminal only through `turn/completed`.
    Error(ErrorEvent),
    /// `thread/status/changed`.
    ThreadStatusChanged {
        /// The thread.
        thread_id: String,
    },
    /// `thread/closed`.
    ThreadClosed {
        /// The thread.
        thread_id: String,
    },
    /// Any other method: activity only.
    Unknown {
        /// The method, cut to [`UNKNOWN_TAG_MAX`] bytes.
        method: String,
        /// `params.threadId`, when present and a string.
        thread_id: Option<String>,
    },
}

/// `turn/started` and `turn/completed` params.
#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnEvent {
    /// The thread.
    pub thread_id: String,
    /// The turn.
    pub turn: Turn,
}

/// A turn as the server reports it; its `items` are not read.
#[derive(Debug, Deserialize, Eq, PartialEq)]
pub struct Turn {
    /// The turn ID.
    pub id: String,
    /// Its status.
    pub status: TurnStatus,
    /// Its error, when it failed.
    #[serde(default)]
    pub error: Option<TurnError>,
}

/// A turn's status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    /// `completed`.
    Completed,
    /// `interrupted`.
    Interrupted,
    /// `failed`.
    Failed,
    /// `inProgress`.
    InProgress,
}

/// A turn's or an `error` notification's error.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    /// The vendor's message.
    pub message: String,
    /// The typed error code, when present.
    #[serde(default, deserialize_with = "error_info")]
    pub codex_error_info: Option<CodexErrorInfo>,
    /// More vendor text, when present.
    #[serde(default)]
    pub additional_details: Option<String>,
}

/// `codexErrorInfo`: a code (`"unauthorized"`), or a one-member object
/// (`{"httpConnectionFailed": {"httpStatusCode": 401}}`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexErrorInfo {
    /// The code, or the object's one member name.
    pub kind: String,
    /// `httpStatusCode`, when the object carries one.
    pub http_status: Option<u16>,
}

/// `item/started` and `item/completed` params.
#[derive(Debug, Eq, PartialEq)]
pub struct ItemEvent {
    /// The thread.
    pub thread_id: String,
    /// The turn.
    pub turn_id: String,
    /// The item.
    pub item: Item,
}

/// A thread item: what VIA reads of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Item {
    /// The item ID.
    pub id: String,
    /// Its type.
    pub kind: ItemKind,
    /// An `agentMessage`'s text.
    pub text: Option<String>,
    /// An `agentMessage`'s phase (`commentary`, `final_answer`).
    pub phase: Option<String>,
    /// A tool item's status.
    pub status: Option<String>,
}

/// A thread item's `type`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ItemKind {
    /// `userMessage`.
    UserMessage,
    /// `agentMessage`.
    AgentMessage,
    /// `reasoning`.
    Reasoning,
    /// `commandExecution`.
    CommandExecution,
    /// `fileChange`.
    FileChange,
    /// `mcpToolCall`.
    McpToolCall,
    /// `dynamicToolCall`.
    DynamicToolCall,
    /// `collabAgentToolCall`.
    CollabAgentToolCall,
    /// `webSearch`.
    WebSearch,
    /// `imageGeneration`.
    ImageGeneration,
    /// Any other type, by name.
    Other(String),
}

/// The tool item types: a started one is `tools_started`, a completed one
/// `tools_ended` (packet §5).
const TOOL_KINDS: [(&str, ItemKind); 7] = [
    ("commandExecution", ItemKind::CommandExecution),
    ("fileChange", ItemKind::FileChange),
    ("mcpToolCall", ItemKind::McpToolCall),
    ("dynamicToolCall", ItemKind::DynamicToolCall),
    ("collabAgentToolCall", ItemKind::CollabAgentToolCall),
    ("webSearch", ItemKind::WebSearch),
    ("imageGeneration", ItemKind::ImageGeneration),
];

impl ItemKind {
    fn parse(name: &str) -> Self {
        match name {
            "userMessage" => Self::UserMessage,
            "agentMessage" => Self::AgentMessage,
            "reasoning" => Self::Reasoning,
            other => TOOL_KINDS
                .iter()
                .find(|(tool, _)| *tool == other)
                .map_or_else(|| Self::Other(other.to_owned()), |(_, kind)| kind.clone()),
        }
    }

    /// The vendor's type name.
    pub fn as_str(&self) -> &str {
        match self {
            Self::UserMessage => "userMessage",
            Self::AgentMessage => "agentMessage",
            Self::Reasoning => "reasoning",
            Self::Other(name) => name,
            Self::CommandExecution
            | Self::FileChange
            | Self::McpToolCall
            | Self::DynamicToolCall
            | Self::CollabAgentToolCall
            | Self::WebSearch
            | Self::ImageGeneration => TOOL_KINDS
                .iter()
                .find(|(_, kind)| kind == self)
                .map_or("", |(name, _)| name),
        }
    }

    /// Whether the item is a tool.
    pub fn is_tool(&self) -> bool {
        match self {
            Self::CommandExecution
            | Self::FileChange
            | Self::McpToolCall
            | Self::DynamicToolCall
            | Self::CollabAgentToolCall
            | Self::WebSearch
            | Self::ImageGeneration => true,
            Self::UserMessage | Self::AgentMessage | Self::Reasoning | Self::Other(_) => false,
        }
    }
}

/// A text delta of one item.
#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeltaEvent {
    /// The thread.
    pub thread_id: String,
    /// The turn.
    pub turn_id: String,
    /// The item.
    pub item_id: String,
    /// The text added.
    pub delta: String,
}

/// `thread/tokenUsage/updated` params.
#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageEvent {
    /// The thread.
    pub thread_id: String,
    /// The turn.
    pub turn_id: String,
    /// The counts.
    #[serde(rename = "tokenUsage")]
    pub usage: TokenUsage,
}

/// A thread's token counts: the last model call's and the thread total.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// The thread total.
    pub total: TokenBreakdown,
    /// The last model call.
    pub last: TokenBreakdown,
    /// The model's context window, when reported.
    #[serde(default)]
    pub model_context_window: Option<u64>,
}

/// One token count set.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TokenBreakdown {
    /// All tokens.
    pub total_tokens: u64,
    /// Input tokens, cached ones included.
    pub input_tokens: u64,
    /// Cached input tokens.
    pub cached_input_tokens: u64,
    /// Cache-write input tokens, when reported (0.159.2).
    #[serde(default)]
    pub cache_write_input_tokens: Option<u64>,
    /// Output tokens.
    pub output_tokens: u64,
    /// Reasoning output tokens.
    pub reasoning_output_tokens: u64,
}

/// `error` notification params.
#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEvent {
    /// The thread.
    pub thread_id: String,
    /// The turn.
    pub turn_id: String,
    /// The error.
    pub error: TurnError,
    /// Whether the vendor retries: a diagnostic, never a terminal.
    pub will_retry: bool,
}

/// The `initialize` result.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// `<clientInfo.name>/<version> …`, the instance version's source.
    pub user_agent: String,
}

/// One `model/list` page.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResult {
    /// The page's models.
    pub data: Vec<Model>,
    /// The next page's cursor, or none at the end.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One catalog model.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    /// The model name `thread/start` takes.
    pub model: String,
    /// The efforts it advertises.
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<EffortOption>,
    /// Its default effort, when stated.
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    /// Whether the catalog hides it.
    #[serde(default)]
    pub hidden: bool,
    /// Whether it is the catalog's default.
    #[serde(default)]
    pub is_default: bool,
}

/// One advertised effort.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffortOption {
    /// The effort value.
    pub reasoning_effort: String,
}

/// The `thread/start` and `thread/resume` result: the thread and the
/// settings it echoes, which the handshake check compares.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResult {
    /// The thread.
    pub thread: ThreadRef,
    /// The effective model.
    pub model: String,
    /// The effective working directory.
    pub cwd: String,
    /// The effective approval policy, as echoed.
    pub approval_policy: Value,
    /// The effective approvals reviewer, when echoed.
    #[serde(default)]
    pub approvals_reviewer: Option<String>,
    /// The effective structured sandbox policy, as echoed.
    pub sandbox: Value,
    /// The instruction files the thread loaded (inventory, packet §4).
    #[serde(default)]
    pub instruction_sources: Vec<String>,
}

/// A thread's identity.
#[derive(Debug, Deserialize)]
pub struct ThreadRef {
    /// The thread ID.
    pub id: String,
}

/// The `turn/start` result: the accepted turn.
#[derive(Debug, Deserialize)]
pub struct TurnStartResult {
    /// The turn.
    pub turn: Turn,
}

/// The `turn/steer` result.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerResult {
    /// The turn the input went to.
    pub turn_id: String,
}

/// The `thread/unsubscribe` result.
#[derive(Debug, Deserialize)]
pub struct UnsubscribeResult {
    /// What the detach found.
    pub status: UnsubscribeStatus,
}

/// A detach's status; each proves detachment only (packet §2).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum UnsubscribeStatus {
    /// The connection was subscribed.
    Unsubscribed,
    /// It was not subscribed.
    NotSubscribed,
    /// The thread was not loaded.
    NotLoaded,
}

/// A paired result, typed once the pairing knows its method.
pub fn result<T: DeserializeOwned>(raw: &RawValue) -> Result<T, DecodeError> {
    serde_json::from_str(raw.get()).map_err(|_| DecodeError("a result does not match its method"))
}

/// The envelope every message shares.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "present")]
    result: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<RpcError>,
}

/// Decodes one line the server wrote.
pub fn decode(line: &[u8]) -> Result<Incoming, DecodeError> {
    let envelope: Envelope =
        serde_json::from_slice(line).map_err(|_| DecodeError("not a JSON-RPC message"))?;
    match envelope {
        Envelope {
            id: Some(id),
            method: None,
            result: Some(result),
            error: None,
            ..
        } => Ok(Incoming::Response(Response {
            id,
            outcome: Ok(result),
        })),
        Envelope {
            id: Some(id),
            method: None,
            result: None,
            error: Some(error),
            ..
        } => Ok(Incoming::Response(Response {
            id,
            outcome: Err(error),
        })),
        Envelope {
            id: Some(id),
            method: Some(method),
            params,
            result: None,
            error: None,
        } => {
            let ids = loose_ids(params.as_deref());
            Ok(Incoming::Request(ServerRequest {
                id,
                method: short(method)?,
                thread_id: ids.thread_id.map(short).transpose()?,
                turn_id: ids.turn_id.map(short).transpose()?,
            }))
        }
        Envelope {
            id: None,
            method: Some(method),
            params,
            result: None,
            error: None,
        } => notification(method, params.as_deref()).map(Incoming::Notification),
        Envelope { .. } => Err(DecodeError(
            "neither a response, a request nor a notification",
        )),
    }
}

/// Types a notification by method.
fn notification(method: String, raw: Option<&RawValue>) -> Result<Notification, DecodeError> {
    let typed = |what| move |_| DecodeError(what);
    let params = raw.map_or("null", RawValue::get);
    let notification = match method.as_str() {
        "turn/started" => {
            Notification::TurnStarted(serde_json::from_str(params).map_err(typed("turn/started"))?)
        }
        "turn/completed" => Notification::TurnCompleted(
            serde_json::from_str(params).map_err(typed("turn/completed"))?,
        ),
        "item/started" => Notification::ItemStarted(item_event(params)?),
        "item/completed" => Notification::ItemCompleted(item_event(params)?),
        "item/agentMessage/delta" => Notification::AgentMessageDelta(
            serde_json::from_str(params).map_err(typed("item/agentMessage/delta"))?,
        ),
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
            Notification::ReasoningDelta(
                serde_json::from_str(params).map_err(typed("a reasoning delta"))?,
            )
        }
        "thread/tokenUsage/updated" => Notification::TokenUsage(
            serde_json::from_str(params).map_err(typed("thread/tokenUsage/updated"))?,
        ),
        "error" => Notification::Error(serde_json::from_str(params).map_err(typed("error"))?),
        "thread/status/changed" => Notification::ThreadStatusChanged {
            thread_id: thread_of(params, "thread/status/changed")?,
        },
        "thread/closed" => Notification::ThreadClosed {
            thread_id: thread_of(params, "thread/closed")?,
        },
        _ => {
            let mut method = method;
            truncate(&mut method, UNKNOWN_TAG_MAX);
            let thread_id = loose_ids(raw)
                .thread_id
                .filter(|id| id.len() <= SHORT_FIELD_MAX);
            return Ok(Notification::Unknown { method, thread_id });
        }
    };
    check_ids(&notification)?;
    Ok(notification)
}

/// `item/started` and `item/completed` params, tolerant of item types
/// VIA does not know; an `agentMessage` needs its text.
fn item_event(params: &str) -> Result<ItemEvent, DecodeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Raw {
        thread_id: String,
        turn_id: String,
        item: RawItem,
    }
    #[derive(Deserialize)]
    struct RawItem {
        #[serde(rename = "type")]
        kind: String,
        id: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        phase: Option<String>,
        #[serde(default)]
        status: Option<String>,
    }
    let raw: Raw = serde_json::from_str(params).map_err(|_| DecodeError("an item event"))?;
    let kind = ItemKind::parse(&short(raw.item.kind)?);
    if kind == ItemKind::AgentMessage && raw.item.text.is_none() {
        return Err(DecodeError("an agentMessage item without text"));
    }
    Ok(ItemEvent {
        thread_id: raw.thread_id,
        turn_id: raw.turn_id,
        item: Item {
            id: raw.item.id,
            kind,
            text: raw.item.text,
            phase: raw.item.phase,
            status: raw.item.status,
        },
    })
}

/// The thread of a notification whose params need only `threadId`.
fn thread_of(params: &str, what: &'static str) -> Result<String, DecodeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Raw {
        thread_id: String,
    }
    serde_json::from_str::<Raw>(params)
        .map(|raw| raw.thread_id)
        .map_err(|_| DecodeError(what))
}

/// Every ID a typed notification carries is within [`SHORT_FIELD_MAX`].
fn check_ids(notification: &Notification) -> Result<(), DecodeError> {
    let ids: Vec<&str> = match notification {
        Notification::TurnStarted(event) | Notification::TurnCompleted(event) => {
            vec![&event.thread_id, &event.turn.id]
        }
        Notification::ItemStarted(event) | Notification::ItemCompleted(event) => {
            vec![&event.thread_id, &event.turn_id, &event.item.id]
        }
        Notification::AgentMessageDelta(event) | Notification::ReasoningDelta(event) => {
            vec![&event.thread_id, &event.turn_id, &event.item_id]
        }
        Notification::TokenUsage(event) => vec![&event.thread_id, &event.turn_id],
        Notification::Error(event) => vec![&event.thread_id, &event.turn_id],
        Notification::ThreadStatusChanged { thread_id }
        | Notification::ThreadClosed { thread_id } => vec![thread_id],
        Notification::Unknown { .. } => Vec::new(),
    };
    if ids.iter().any(|id| id.len() > SHORT_FIELD_MAX) {
        return Err(DecodeError("an ID longer than its bound"));
    }
    Ok(())
}

/// `threadId` and `turnId` of params of any shape, when they are strings.
struct LooseIds {
    thread_id: Option<String>,
    turn_id: Option<String>,
}

fn loose_ids(params: Option<&RawValue>) -> LooseIds {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Raw {
        #[serde(default)]
        thread_id: Option<Value>,
        #[serde(default)]
        turn_id: Option<Value>,
    }
    let raw = params.and_then(|params| serde_json::from_str::<Raw>(params.get()).ok());
    let text = |value: Option<Value>| match value {
        Some(Value::String(text)) => Some(text),
        _ => None,
    };
    match raw {
        Some(raw) => LooseIds {
            thread_id: text(raw.thread_id),
            turn_id: text(raw.turn_id),
        },
        None => LooseIds {
            thread_id: None,
            turn_id: None,
        },
    }
}

/// A short field (method, ID or type tag) within [`SHORT_FIELD_MAX`].
fn short(field: String) -> Result<String, DecodeError> {
    if field.len() > SHORT_FIELD_MAX {
        Err(DecodeError("a field longer than its bound"))
    } else {
        Ok(field)
    }
}

/// Cuts `text` to at most `max` bytes, at a character boundary.
fn truncate(text: &mut String, max: usize) {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}

/// `codexErrorInfo`: a string code or a one-member object; null or any
/// other shape is none.
fn error_info<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<CodexErrorInfo>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(kind) => Some(CodexErrorInfo {
            kind,
            http_status: None,
        }),
        Value::Object(map) if map.len() == 1 => {
            map.into_iter().next().map(|(kind, detail)| CodexErrorInfo {
                kind,
                http_status: detail
                    .get("httpStatusCode")
                    .and_then(Value::as_u64)
                    .and_then(|code| u16::try_from(code).ok()),
            })
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            None
        }
    })
}

/// A member that is present, null included, is `Some`.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(deserializer).map(Some)
}
