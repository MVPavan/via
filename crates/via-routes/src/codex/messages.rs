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
    /// x.3.2 X3 §3.2, the refusal check: an error reply to a `turn/start`
    /// whose lane took an item naming an unmapped turn while the start was
    /// open (`Refused { contradicted }`); set by the pairing.
    pub contradicted: bool,
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
    /// `params.itemId`, when present: the item an approval request is for.
    pub item_id: Option<String>,
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

impl Notification {
    /// The thread the notification names, if any: the demux routes by it
    /// (packet §5). Untagged connection traffic names none.
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Self::TurnStarted(event) | Self::TurnCompleted(event) => Some(&event.thread_id),
            Self::ItemStarted(event) | Self::ItemCompleted(event) => Some(&event.thread_id),
            Self::AgentMessageDelta(event) | Self::ReasoningDelta(event) => Some(&event.thread_id),
            Self::TokenUsage(event) => Some(&event.thread_id),
            Self::Error(event) => Some(&event.thread_id),
            Self::ThreadStatusChanged { thread_id } | Self::ThreadClosed { thread_id } => {
                Some(thread_id)
            }
            Self::Unknown { thread_id, .. } => thread_id.as_deref(),
        }
    }

    /// The turn the notification names, if any.
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::TurnStarted(event) | Self::TurnCompleted(event) => Some(&event.turn.id),
            Self::ItemStarted(event) | Self::ItemCompleted(event) => Some(&event.turn_id),
            Self::AgentMessageDelta(event) | Self::ReasoningDelta(event) => Some(&event.turn_id),
            Self::TokenUsage(event) => Some(&event.turn_id),
            Self::Error(event) => Some(&event.turn_id),
            Self::ThreadStatusChanged { .. } | Self::ThreadClosed { .. } | Self::Unknown { .. } => {
                None
            }
        }
    }
}

/// Where one message goes, read from its correlation fields only (x.3.2
/// X0 item 5 step 1), before any full decode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Routing {
    /// A reply to one of VIA's requests, by its integer ID.
    Response(i64),
    /// A request VIA must answer (x.3.2 X3 §5.3: classified before its full
    /// decode): its ID and method, and the thread and turn it names.
    Request {
        /// The ID to answer under.
        id: RequestId,
        /// The method.
        method: String,
        /// `params.threadId`, when a string within its bound.
        thread: Option<String>,
        /// `params.turnId`, likewise.
        turn: Option<String>,
    },
    /// A notification, with the thread and turn it names.
    Notification {
        /// `params.threadId`.
        thread: Option<String>,
        /// `params.turnId`, or `params.turn.id` for `turn/*`.
        turn: Option<String>,
    },
}

/// The methods whose correlation names a thread and a turn: the turn by
/// `params.turnId`, or by `params.turn.id` for the `turn/*` pair.
const TURN_METHODS: [&str; 9] = [
    "turn/started",
    "turn/completed",
    "item/started",
    "item/completed",
    "item/agentMessage/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/textDelta",
    "thread/tokenUsage/updated",
    "error",
];

/// The methods whose correlation names a thread only.
const THREAD_METHODS: [&str; 2] = ["thread/status/changed", "thread/closed"];

/// The routing peek (x.3.2 X0 item 5 step 1): only `id`, `method`,
/// `params.threadId` and `params.turnId` (`params.turn.id` for `turn/*`),
/// against their typed schema. An error is an unattributable message (step
/// 2): not JSON, a reply ID that is not an integer, or a known method whose
/// thread or turn is missing, not a string, or past [`SHORT_FIELD_MAX`].
/// Every other field is the full decode's, at consumption. `params` is
/// borrowed from the line, never copied (via-5lr.3.5).
pub fn peek(line: &[u8]) -> Result<Routing, DecodeError> {
    #[derive(Deserialize)]
    struct Head<'a> {
        #[serde(default, deserialize_with = "present")]
        id: Option<Box<RawValue>>,
        #[serde(default)]
        method: Option<String>,
        #[serde(default, borrow)]
        params: Option<&'a RawValue>,
    }
    let head: Head<'_> =
        serde_json::from_slice(line).map_err(|_| DecodeError("not a JSON-RPC message"))?;
    match (head.id, head.method) {
        (Some(id), None) => match serde_json::from_str::<Value>(id.get()) {
            Ok(Value::Number(number)) => number
                .as_i64()
                .map(Routing::Response)
                .ok_or(DecodeError("a reply ID that is not one of VIA's")),
            _ => Err(DecodeError("a reply ID that is not one of VIA's")),
        },
        (Some(id), Some(method)) => {
            let id = request_id(&id)?;
            fits(&[&method])?;
            let ids = loose_ids(head.params);
            let bounded = |text: Option<String>| text.filter(|text| text.len() <= SHORT_FIELD_MAX);
            Ok(Routing::Request {
                id,
                method,
                thread: bounded(ids.thread),
                turn: bounded(ids.turn),
            })
        }
        (None, Some(method)) => notification_routing(&method, head.params),
        (None, None) => Err(DecodeError(
            "neither a response, a request nor a notification",
        )),
    }
}

/// A notification's correlation, by method.
fn notification_routing(method: &str, params: Option<&RawValue>) -> Result<Routing, DecodeError> {
    #[derive(Deserialize)]
    struct Ids<'a> {
        #[serde(default, rename = "threadId")]
        thread: Option<Value>,
        #[serde(default, rename = "turnId")]
        turn_id: Option<Value>,
        #[serde(default, borrow)]
        turn: Option<&'a RawValue>,
    }
    #[derive(Deserialize)]
    struct TurnHead {
        #[serde(default)]
        id: Option<Value>,
    }
    let ids = match params {
        Some(params) => serde_json::from_str::<Ids<'_>>(params.get()).ok(),
        None => None,
    };
    let text = |value: Option<Value>| match value {
        Some(Value::String(text)) if text.len() <= SHORT_FIELD_MAX => Some(text),
        _ => None,
    };
    let (thread, turn_id, turn) = match ids {
        Some(ids) => (text(ids.thread), text(ids.turn_id), ids.turn),
        None => (None, None, None),
    };
    let known = TURN_METHODS.contains(&method);
    if !known && !THREAD_METHODS.contains(&method) {
        return Ok(Routing::Notification {
            thread,
            turn: turn_id,
        });
    }
    let thread = thread.ok_or(DecodeError("a notification without its thread"))?;
    if !known {
        return Ok(Routing::Notification {
            thread: Some(thread),
            turn: None,
        });
    }
    let turn = if method.starts_with("turn/") {
        turn.and_then(|turn| serde_json::from_str::<TurnHead>(turn.get()).ok())
            .and_then(|head| text(head.id))
    } else {
        turn_id
    };
    let turn = turn.ok_or(DecodeError("a notification without its turn"))?;
    Ok(Routing::Notification {
        thread: Some(thread),
        turn: Some(turn),
    })
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
    /// What a command or file-change item acts on: the command, or the
    /// first changed path; cut to [`SHORT_FIELD_MAX`] bytes.
    pub target: Option<String>,
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
    /// `sleep`: the interruptible `clock.sleep` tool.
    Sleep,
    /// Any other type, by name.
    Other(String),
}

/// The tool item types: a started one is `tools_started`, a completed one
/// `tools_ended` (packet §5).
const TOOL_KINDS: [(&str, ItemKind); 8] = [
    ("commandExecution", ItemKind::CommandExecution),
    ("fileChange", ItemKind::FileChange),
    ("mcpToolCall", ItemKind::McpToolCall),
    ("dynamicToolCall", ItemKind::DynamicToolCall),
    ("collabAgentToolCall", ItemKind::CollabAgentToolCall),
    ("webSearch", ItemKind::WebSearch),
    ("imageGeneration", ItemKind::ImageGeneration),
    ("sleep", ItemKind::Sleep),
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
            | Self::ImageGeneration
            | Self::Sleep => TOOL_KINDS
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
            | Self::ImageGeneration
            | Self::Sleep => true,
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
    pub supported_reasoning_efforts: Vec<EffortOption>,
    /// Its default effort.
    pub default_reasoning_effort: String,
    /// Whether the catalog hides it.
    pub hidden: bool,
    /// Whether it is the catalog's default.
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

/// A result's retained fields within their bounds (review r1 #4).
pub trait Checked {
    /// An error when a retained short field is past [`SHORT_FIELD_MAX`].
    fn check(&self) -> Result<(), DecodeError>;
}

impl Checked for InitializeResult {
    fn check(&self) -> Result<(), DecodeError> {
        fits(&[&self.user_agent])
    }
}

impl Checked for ModelListResult {
    fn check(&self) -> Result<(), DecodeError> {
        if let Some(cursor) = &self.next_cursor {
            fits(&[cursor])?;
        }
        self.data.iter().try_for_each(|model| {
            fits(&[&model.model, &model.default_reasoning_effort])?;
            model
                .supported_reasoning_efforts
                .iter()
                .try_for_each(|effort| fits(&[&effort.reasoning_effort]))
        })
    }
}

impl Checked for ThreadResult {
    fn check(&self) -> Result<(), DecodeError> {
        fits(&[&self.thread.id, &self.model])
    }
}

impl Checked for TurnStartResult {
    fn check(&self) -> Result<(), DecodeError> {
        fits(&[&self.turn.id])
    }
}

impl Checked for TurnSteerResult {
    fn check(&self) -> Result<(), DecodeError> {
        fits(&[&self.turn_id])
    }
}

impl Checked for UnsubscribeResult {
    fn check(&self) -> Result<(), DecodeError> {
        Ok(())
    }
}

/// A paired result, typed once the pairing knows its method: within the
/// structure limits, matching its method, its short fields bounded.
pub fn result<T: DeserializeOwned + Checked>(raw: &RawValue) -> Result<T, DecodeError> {
    limits(raw.get().as_bytes())?;
    let typed: T = serde_json::from_str(raw.get())
        .map_err(|_| DecodeError("a result does not match its method"))?;
    typed.check()?;
    Ok(typed)
}

/// Review r1 #2: the structure limits, before any serde pass.
fn limits(bytes: &[u8]) -> Result<(), DecodeError> {
    via_wire::json_limits::scan(bytes)
        .map(drop)
        .map_err(|_| DecodeError("a message past the JSON structure limits"))
}

/// Every field within [`SHORT_FIELD_MAX`].
fn fits(fields: &[&str]) -> Result<(), DecodeError> {
    if fields.iter().any(|field| field.len() > SHORT_FIELD_MAX) {
        Err(DecodeError("a field longer than its bound"))
    } else {
        Ok(())
    }
}

/// The envelope every message shares. `id` is kept raw so that an
/// explicit `null` is told from an absent member. `params` is borrowed
/// from the line, never copied: a decode's peak stays near one copy of
/// its retained text (via-5lr.3.5, codex-server.md item 9.2).
#[derive(Deserialize)]
struct RawEnvelope<'a> {
    #[serde(default, deserialize_with = "present")]
    id: Option<Box<RawValue>>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default, borrow)]
    params: Option<&'a RawValue>,
    #[serde(default, deserialize_with = "present")]
    result: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<RpcError>,
}

/// Decodes one line the server wrote.
pub fn decode(line: &[u8]) -> Result<Incoming, DecodeError> {
    limits(line)?;
    let raw: RawEnvelope<'_> =
        serde_json::from_slice(line).map_err(|_| DecodeError("not a JSON-RPC message"))?;
    let envelope = Envelope {
        id: raw.id.as_deref().map(request_id).transpose()?,
        method: raw.method,
        params: raw.params,
        result: raw.result,
        error: raw.error,
    };
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
            contradicted: false,
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
            contradicted: false,
        })),
        Envelope {
            id: Some(id),
            method: Some(method),
            params,
            result: None,
            error: None,
        } => {
            let ids = loose_ids(params);
            Ok(Incoming::Request(ServerRequest {
                id,
                method: short(method)?,
                thread_id: ids.thread.map(short).transpose()?,
                turn_id: ids.turn.map(short).transpose()?,
                item_id: ids.item.map(short).transpose()?,
            }))
        }
        Envelope {
            id: None,
            method: Some(method),
            params,
            result: None,
            error: None,
        } => notification(method, params).map(Incoming::Notification),
        Envelope { .. } => Err(DecodeError(
            "neither a response, a request nor a notification",
        )),
    }
}

/// The envelope, its ID typed.
struct Envelope<'a> {
    id: Option<RequestId>,
    method: Option<String>,
    params: Option<&'a RawValue>,
    result: Option<Box<RawValue>>,
    error: Option<RpcError>,
}

/// A present `id`: an integer or a bounded string; `null` or any other
/// shape is no envelope.
fn request_id(raw: &RawValue) -> Result<RequestId, DecodeError> {
    let id: RequestId =
        serde_json::from_str(raw.get()).map_err(|_| DecodeError("an ID that is not one"))?;
    if let RequestId::Str(text) = &id {
        fits(&[text])?;
    }
    Ok(id)
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
                .thread
                .filter(|id| id.len() <= SHORT_FIELD_MAX);
            return Ok(Notification::Unknown { method, thread_id });
        }
    };
    check_ids(&notification)?;
    Ok(notification)
}

/// `item/started` and `item/completed` params. The item's `type` is read
/// first, then only that type's retained fields (review r2 #1): unrelated
/// fields of any shape, and the fields of an unknown type, are ignored.
fn item_event(params: &str) -> Result<ItemEvent, DecodeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Raw<'a> {
        thread_id: String,
        turn_id: String,
        #[serde(borrow)]
        item: &'a RawValue,
    }
    #[derive(Deserialize)]
    struct Head {
        #[serde(rename = "type")]
        kind: String,
        id: String,
    }
    let raw: Raw<'_> = serde_json::from_str(params).map_err(|_| DecodeError("an item event"))?;
    let head: Head = fields(raw.item, "an item without its type or ID")?;
    let kind = ItemKind::parse(&short(head.kind)?);
    let mut item = item_fields(&kind, raw.item)?;
    if let Some(status) = item.status.take() {
        item.status = Some(short(status)?);
    }
    check_status(&kind, item.status.as_deref())?;
    if let Some(target) = &mut item.target {
        truncate(target, SHORT_FIELD_MAX);
    }
    Ok(ItemEvent {
        thread_id: raw.thread_id,
        turn_id: raw.turn_id,
        item: Item {
            id: head.id,
            kind,
            text: item.text,
            phase: item.phase,
            status: item.status,
            target: item.target,
        },
    })
}

/// The retained fields of one item type.
#[derive(Default)]
struct ItemFields {
    text: Option<String>,
    phase: Option<String>,
    status: Option<String>,
    target: Option<String>,
}

/// Decodes `kind`'s retained fields from `raw`, and nothing else.
fn item_fields(kind: &ItemKind, raw: &RawValue) -> Result<ItemFields, DecodeError> {
    #[derive(Deserialize)]
    struct Message {
        text: String,
        #[serde(default)]
        phase: Option<String>,
    }
    #[derive(Deserialize)]
    struct Status {
        status: String,
    }
    #[derive(Deserialize)]
    struct Command {
        status: String,
        command: String,
    }
    #[derive(Deserialize)]
    struct Patch {
        status: String,
        changes: Vec<Change>,
    }
    #[derive(Deserialize)]
    struct Change {
        path: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Sleep {
        #[expect(dead_code, reason = "validated, not retained")]
        duration_ms: u64,
    }
    Ok(match kind {
        ItemKind::AgentMessage => {
            let message: Message = fields(raw, "an agentMessage item")?;
            if message
                .phase
                .as_deref()
                .is_some_and(|phase| !["commentary", "final_answer"].contains(&phase))
            {
                return Err(DecodeError("an agentMessage item with an unknown phase"));
            }
            ItemFields {
                text: Some(message.text),
                phase: message.phase,
                ..ItemFields::default()
            }
        }
        ItemKind::CommandExecution => {
            let command: Command = fields(raw, "a commandExecution item")?;
            ItemFields {
                status: Some(command.status),
                target: Some(command.command),
                ..ItemFields::default()
            }
        }
        ItemKind::FileChange => {
            let patch: Patch = fields(raw, "a fileChange item")?;
            ItemFields {
                status: Some(patch.status),
                target: patch.changes.into_iter().next().map(|change| change.path),
                ..ItemFields::default()
            }
        }
        ItemKind::McpToolCall
        | ItemKind::DynamicToolCall
        | ItemKind::CollabAgentToolCall
        | ItemKind::ImageGeneration => {
            let status: Status = fields(raw, "a tool item without its status")?;
            ItemFields {
                status: Some(status.status),
                ..ItemFields::default()
            }
        }
        ItemKind::Sleep => {
            let _: Sleep = fields(raw, "a sleep item without its duration")?;
            ItemFields::default()
        }
        ItemKind::UserMessage | ItemKind::Reasoning | ItemKind::WebSearch | ItemKind::Other(_) => {
            ItemFields::default()
        }
    })
}

/// Typed fields of an already scanned value.
fn fields<T: DeserializeOwned>(raw: &RawValue, what: &'static str) -> Result<T, DecodeError> {
    serde_json::from_str(raw.get()).map_err(|_| DecodeError(what))
}

/// A tool item's status against its schema: required, and one of its
/// type's values where the schema lists them.
fn check_status(kind: &ItemKind, status: Option<&str>) -> Result<(), DecodeError> {
    let allowed: &[&str] = match kind {
        ItemKind::CommandExecution | ItemKind::FileChange => {
            &["inProgress", "completed", "failed", "declined"]
        }
        ItemKind::McpToolCall | ItemKind::DynamicToolCall => &["inProgress", "completed", "failed"],
        ItemKind::CollabAgentToolCall => &["inProgress", "completed", "failed", "interrupted"],
        // Any status, but one is required.
        ItemKind::ImageGeneration => &[],
        ItemKind::UserMessage
        | ItemKind::AgentMessage
        | ItemKind::Reasoning
        | ItemKind::WebSearch
        | ItemKind::Sleep
        | ItemKind::Other(_) => return Ok(()),
    };
    match status {
        None => Err(DecodeError("a tool item without its status")),
        Some(status) if !allowed.is_empty() && !allowed.contains(&status) => {
            Err(DecodeError("a tool item with an unknown status"))
        }
        Some(_) => Ok(()),
    }
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
    thread: Option<String>,
    turn: Option<String>,
    item: Option<String>,
}

fn loose_ids(params: Option<&RawValue>) -> LooseIds {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default, rename = "threadId")]
        thread: Option<Value>,
        #[serde(default, rename = "turnId")]
        turn: Option<Value>,
        #[serde(default, rename = "itemId")]
        item: Option<Value>,
    }
    let raw = params.and_then(|params| serde_json::from_str::<Raw>(params.get()).ok());
    let text = |value: Option<Value>| match value {
        Some(Value::String(text)) => Some(text),
        _ => None,
    };
    match raw {
        Some(raw) => LooseIds {
            thread: text(raw.thread),
            turn: text(raw.turn),
            item: text(raw.item),
        },
        None => LooseIds {
            thread: None,
            turn: None,
            item: None,
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

/// `codexErrorInfo`: null, a bounded string code, or a one-member object
/// whose value is an object with an optional `httpStatusCode` (uint16 or
/// null). Any other shape is malformed (review r1 #3).
fn error_info<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<CodexErrorInfo>, D::Error> {
    use serde::de::Error as _;
    let malformed = || D::Error::custom("malformed codexErrorInfo");
    let (kind, http_status) = match Value::deserialize(deserializer)? {
        Value::Null => return Ok(None),
        Value::String(kind) => (kind, None),
        Value::Object(map) if map.len() == 1 => {
            let (kind, detail) = map.into_iter().next().ok_or_else(malformed)?;
            let Value::Object(detail) = detail else {
                return Err(malformed());
            };
            let http_status = match detail.get("httpStatusCode") {
                None | Some(Value::Null) => None,
                Some(code) => Some(
                    code.as_u64()
                        .and_then(|code| u16::try_from(code).ok())
                        .ok_or_else(malformed)?,
                ),
            };
            (kind, http_status)
        }
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            return Err(malformed());
        }
    };
    if kind.len() > SHORT_FIELD_MAX {
        return Err(malformed());
    }
    Ok(Some(CodexErrorInfo { kind, http_status }))
}

/// A member that is present, null included, is `Some`.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(deserializer).map(Some)
}

/// Why a route's JSON text is not structured output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonTextError {
    /// Past the structure limits (depth 64, 65,536 nodes).
    Limits,
    /// Not one JSON value.
    NotJson,
}

/// `text` as one JSON value, checked against the structure limits before
/// it is parsed.
pub fn json_text(text: &str) -> Result<Box<RawValue>, JsonTextError> {
    via_wire::json_limits::scan(text.as_bytes()).map_err(|_| JsonTextError::Limits)?;
    serde_json::from_str(text).map_err(|_| JsonTextError::NotJson)
}
