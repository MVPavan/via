//! Pi's typed RPC records (packet §5.1) and the lines VIA writes (packet
//! §§2.1, 6, 7.1). A record missing a member VIA reads, or holding one of
//! the wrong type, is malformed: the turn's protocol failure (C2 rule 6).
//! Everything else in a record is never read.

use serde_json::{Map, Value, json};
use via_wire::OutboundMessage;

/// C2 A1: an ID, command, method, tool name or stop reason past this many
/// bytes is malformed (packet §5.1).
pub const SHORT_MAX: usize = 1024;

/// Why a line is not a record VIA can read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// Not a JSON object with a string `type`.
    NotTyped,
    /// A known record missing a member VIA reads, or holding a wrong one.
    Malformed(&'static str),
    /// Past runtime §8's JSON structure limits.
    Limits,
}

/// One decoded Pi record.
#[derive(Clone, Debug)]
pub enum Record {
    /// A command's reply.
    Response(Response),
    /// `agent_start`, `turn_start`, `turn_end` or `agent_end`: lifecycle,
    /// their bodies never read (packet §5.1).
    Lifecycle,
    /// `agent_settled`: the run's end (E28).
    Settled,
    /// `message_start` of a message with `role`.
    MessageStart(Role),
    /// `message_end` of a message.
    MessageEnd(MessageEnd),
    /// `message_update`: whether its event streams model output (packet
    /// §5.2: text, thinking or a tool call, started or growing).
    MessageUpdate {
        /// One `progress {model:true}`.
        model: bool,
    },
    /// `tool_execution_start`.
    ToolStart {
        /// `toolCallId`.
        call_id: String,
        /// `toolName`.
        name: String,
    },
    /// `tool_execution_end`.
    ToolEnd {
        /// `toolCallId`.
        call_id: String,
    },
    /// `compaction_end`: a model call (E63), with its usage when it
    /// reported one.
    CompactionEnd(Option<Usage>),
    /// `extension_ui_request` (packet §6).
    UiRequest(UiRequest),
    /// Any other record, known or not: activity, no observation.
    Activity,
}

/// A message's role (packet §5.2); an unknown role is activity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// The prompt's echo.
    User,
    /// A model call's message.
    Assistant,
    /// The system patch (packet §4.7).
    System,
    /// Any other role.
    Other,
}

/// A `message_end`, as VIA reads it.
#[derive(Clone, Debug)]
pub enum MessageEnd {
    /// One model call's message.
    Assistant(Box<AssistantEnd>),
    /// The system patch.
    System(Box<SystemPatch>),
    /// Another role's: activity.
    Other,
}

/// An assistant `message_end` (packet §5.1, docs `message-types.md`).
#[derive(Clone, Debug, PartialEq)]
pub struct AssistantEnd {
    /// The text of each `text` block, in order.
    pub text: Vec<String>,
    /// `stopReason`.
    pub stop_reason: String,
    /// `errorMessage`, when a string. Free vendor text: never copied out
    /// of the route's and normalizer's matching (packet §5.4).
    pub error_message: Option<String>,
    /// The call's usage.
    pub usage: Usage,
}

/// One model call's usage (packet §5.5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Usage {
    /// `input`, cache reads excluded (E24).
    pub input: u64,
    /// `output`.
    pub output: u64,
    /// `cacheRead`.
    pub cache_read: u64,
    /// `cacheWrite`.
    pub cache_write: u64,
    /// `reasoning`, optional in Pi's type.
    pub reasoning: Option<u64>,
    /// `totalTokens`.
    pub total: u64,
    /// `cost.total`, US dollars from catalog prices.
    pub cost: f64,
}

impl Usage {
    /// Pi reports missing usage as all zeros (E26): the input, output and
    /// cache counters all 0.
    #[must_use]
    pub fn is_missing(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_write == 0
    }
}

/// A member of the system patch: absent (unchanged), `null` (removed) or
/// the section's new text (packet §4.7).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Section {
    /// Not in the patch: unchanged.
    #[default]
    Absent,
    /// `null`: removed.
    Removed,
    /// The section's full new text.
    Text(String),
}

/// The system `message_end`'s patch: its project context and tool
/// loadout changes (packet §§4.5, 4.7).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SystemPatch {
    /// `sections.project_context`.
    pub project_context: Section,
    /// `toolsAdded` names.
    pub tools_added: Vec<String>,
    /// `toolsRemoved` names.
    pub tools_removed: Vec<String>,
}

/// An `extension_ui_request` (packet §6).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiRequest {
    /// The request's `id`, when it has one.
    pub id: Option<String>,
    /// `method`.
    pub method: String,
    /// `title`, when a string.
    pub title: Option<String>,
}

/// A command's reply (packet §5.1). Its `error` text is never kept.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    /// The `id` VIA sent, when present.
    pub id: Option<String>,
    /// `command`.
    pub command: String,
    /// `success`.
    pub success: bool,
    /// `data`, when an object.
    pub data: Option<Map<String, Value>>,
}

/// `get_state`'s data, as the handshake checks it (packet §2.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateData {
    /// `sessionId`.
    pub session_id: String,
    /// `sessionFile` (≤ 4 KiB).
    pub session_file: String,
    /// `model.provider`.
    pub provider: String,
    /// `model.id`.
    pub model_id: String,
    /// `thinkingLevel`.
    pub thinking_level: String,
}

/// Decodes one RPC line (without its LF).
pub fn decode(line: &[u8]) -> Result<Record, DecodeError> {
    // Runtime §8 structure limits, before any serde pass.
    via_wire::json_limits::scan(line).map_err(|_| DecodeError::Limits)?;
    let value: Value = serde_json::from_slice(line).map_err(|_| DecodeError::NotTyped)?;
    let object = value.as_object().ok_or(DecodeError::NotTyped)?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or(DecodeError::NotTyped)?;
    Ok(match kind {
        "response" => Record::Response(response(object)?),
        "agent_start" | "turn_start" | "turn_end" | "agent_end" => Record::Lifecycle,
        "agent_settled" => Record::Settled,
        "message_start" => Record::MessageStart(role(message(object)?)?),
        "message_end" => Record::MessageEnd(message_end(message(object)?)?),
        "message_update" => {
            let event = object
                .get("assistantMessageEvent")
                .and_then(|event| short(event, "type"))
                .ok_or(DecodeError::Malformed(
                    "a message_update without its event type",
                ))?;
            Record::MessageUpdate {
                model: matches!(
                    event,
                    "text_start"
                        | "text_delta"
                        | "thinking_start"
                        | "thinking_delta"
                        | "toolcall_start"
                        | "toolcall_delta"
                ),
            }
        }
        "tool_execution_start" => {
            let (call_id, name) = tool(object)?;
            Record::ToolStart { call_id, name }
        }
        "tool_execution_end" => Record::ToolEnd {
            call_id: tool(object)?.0,
        },
        "compaction_end" => Record::CompactionEnd(
            object
                .get("result")
                .and_then(|result| result.get("usage"))
                .map(usage)
                .transpose()?,
        ),
        "extension_ui_request" => Record::UiRequest(UiRequest {
            id: optional_short(object, "id", "an extension_ui_request with a malformed id")?,
            method: short_member(object, "method").ok_or(DecodeError::Malformed(
                "an extension_ui_request without its method",
            ))?,
            title: object
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        _ => Record::Activity,
    })
}

/// A string member of at most [`SHORT_MAX`] bytes.
fn short<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| text.len() <= SHORT_MAX)
}

fn short_member(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| text.len() <= SHORT_MAX)
        .map(str::to_owned)
}

/// An optional short string member: absent or `null` is `None`; any other
/// non-string, or one too long, is malformed.
fn optional_short(
    object: &Map<String, Value>,
    key: &str,
    malformed: &'static str,
) -> Result<Option<String>, DecodeError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.len() <= SHORT_MAX => Ok(Some(text.clone())),
        Some(_) => Err(DecodeError::Malformed(malformed)),
    }
}

fn response(object: &Map<String, Value>) -> Result<Response, DecodeError> {
    let malformed = DecodeError::Malformed("a malformed command reply");
    let command = short_member(object, "command").ok_or(malformed)?;
    let success = object
        .get("success")
        .and_then(Value::as_bool)
        .ok_or(malformed)?;
    if !success && !object.get("error").is_some_and(Value::is_string) {
        return Err(malformed);
    }
    Ok(Response {
        id: optional_short(object, "id", "a command reply with a malformed id")?,
        command,
        success,
        data: object.get("data").and_then(Value::as_object).cloned(),
    })
}

fn message(object: &Map<String, Value>) -> Result<&Map<String, Value>, DecodeError> {
    object
        .get("message")
        .and_then(Value::as_object)
        .ok_or(DecodeError::Malformed(
            "a message record without its message",
        ))
}

fn role(message: &Map<String, Value>) -> Result<Role, DecodeError> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .ok_or(DecodeError::Malformed("a message without its role"))?;
    Ok(match role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::Other,
    })
}

fn message_end(message: &Map<String, Value>) -> Result<MessageEnd, DecodeError> {
    Ok(match role(message)? {
        Role::Assistant => MessageEnd::Assistant(Box::new(assistant(message)?)),
        Role::System => MessageEnd::System(Box::new(system(message)?)),
        Role::User | Role::Other => MessageEnd::Other,
    })
}

/// Pi's documented `AssistantMessage`: every member VIA reads present and
/// well-typed, else malformed (packet §5.1).
fn assistant(message: &Map<String, Value>) -> Result<AssistantEnd, DecodeError> {
    let content =
        message
            .get("content")
            .and_then(Value::as_array)
            .ok_or(DecodeError::Malformed(
                "an assistant message without its content",
            ))?;
    let mut text = Vec::new();
    for block in content {
        let kind = short(block, "type").ok_or(DecodeError::Malformed(
            "an assistant content block without its type",
        ))?;
        if kind == "text" {
            text.push(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or(DecodeError::Malformed("a text block without its text"))?
                    .to_owned(),
            );
        }
    }
    let stop_reason = short_member(message, "stopReason").ok_or(DecodeError::Malformed(
        "an assistant message without its stopReason",
    ))?;
    let usage = usage(message.get("usage").ok_or(DecodeError::Malformed(
        "an assistant message without its usage",
    ))?)?;
    Ok(AssistantEnd {
        text,
        stop_reason,
        error_message: message
            .get("errorMessage")
            .and_then(Value::as_str)
            .map(str::to_owned),
        usage,
    })
}

/// A usage object: non-negative integer token counters and a
/// non-negative `cost.total` (packet §5.1).
fn usage(value: &Value) -> Result<Usage, DecodeError> {
    let malformed = DecodeError::Malformed("a malformed usage object");
    let count = |key: &str| value.get(key).and_then(Value::as_u64).ok_or(malformed);
    let reasoning = match value.get("reasoning") {
        None | Some(Value::Null) => None,
        Some(reasoning) => Some(reasoning.as_u64().ok_or(malformed)?),
    };
    let cost = value
        .get("cost")
        .and_then(|cost| cost.get("total"))
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
        .ok_or(malformed)?;
    Ok(Usage {
        input: count("input")?,
        output: count("output")?,
        cache_read: count("cacheRead")?,
        cache_write: count("cacheWrite")?,
        reasoning,
        total: count("totalTokens")?,
        cost,
    })
}

/// The system patch's members VIA reads (packet §§4.5, 4.7).
fn system(message: &Map<String, Value>) -> Result<SystemPatch, DecodeError> {
    let malformed = DecodeError::Malformed("a malformed system patch");
    let project_context = match message.get("sections") {
        None | Some(Value::Null) => Section::Absent,
        Some(Value::Object(sections)) => match sections.get("project_context") {
            None => Section::Absent,
            Some(Value::Null) => Section::Removed,
            Some(Value::String(text)) => Section::Text(text.clone()),
            Some(_) => return Err(malformed),
        },
        Some(_) => return Err(malformed),
    };
    let names = |key: &str| -> Result<Vec<String>, DecodeError> {
        match message.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(entries)) => entries
                .iter()
                .map(|entry| {
                    entry
                        .as_str()
                        .or_else(|| entry.get("name").and_then(Value::as_str))
                        .filter(|name| name.len() <= SHORT_MAX)
                        .map(str::to_owned)
                        .ok_or(malformed)
                })
                .collect(),
            Some(_) => Err(malformed),
        }
    };
    Ok(SystemPatch {
        project_context,
        tools_added: names("toolsAdded")?,
        tools_removed: names("toolsRemoved")?,
    })
}

fn tool(object: &Map<String, Value>) -> Result<(String, String), DecodeError> {
    let malformed = DecodeError::Malformed("a tool record without its call ID or tool name");
    Ok((
        short_member(object, "toolCallId").ok_or(malformed)?,
        short_member(object, "toolName").ok_or(malformed)?,
    ))
}

/// `get_state`'s data (packet §2.1); `None` when a member VIA checks is
/// missing or malformed.
#[must_use]
pub(super) fn state_data(data: &Map<String, Value>) -> Option<StateData> {
    let model = data.get("model")?;
    let session_file = data
        .get("sessionFile")?
        .as_str()
        .filter(|file| file.len() <= 4096)?
        .to_owned();
    Some(StateData {
        session_id: short_member(data, "sessionId")?,
        session_file,
        provider: short(model, "provider")?.to_owned(),
        model_id: short(model, "id")?.to_owned(),
        thinking_level: short_member(data, "thinkingLevel")?,
    })
}

/// `get_available_models`' data: each model's provider and ID; `None` when
/// malformed.
#[must_use]
pub(super) fn models_data(data: &Map<String, Value>) -> Option<Vec<(String, String)>> {
    data.get("models")?
        .as_array()?
        .iter()
        .map(|model| {
            Some((
                short(model, "provider")?.to_owned(),
                short(model, "id")?.to_owned(),
            ))
        })
        .collect()
}

/// `get_commands`' data: each command's name; `None` when malformed.
#[must_use]
pub(super) fn commands_data(data: &Map<String, Value>) -> Option<Vec<String>> {
    data.get("commands")?
        .as_array()?
        .iter()
        .map(|command| short(command, "name").map(str::to_owned))
        .collect()
}

/// One JSON line with its LF.
fn line(value: &Value) -> Vec<u8> {
    let mut bytes = value.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}

/// A command without arguments, `{"id":ID,"type":KIND}` (packet §2.1 step
/// 3, §7.1).
#[must_use]
pub(super) fn command(id: &str, kind: &str) -> Vec<u8> {
    line(&json!({"id": id, "type": kind}))
}

/// The cancellation of the dialog `id` (packet §6).
#[must_use]
pub(super) fn ui_cancel(id: &str) -> Vec<u8> {
    line(&json!({"type": "extension_ui_response", "id": id, "cancelled": true}))
}

/// The one prompt (packet §2.1 step 4), streamed: `{"id":ID,"type":"prompt",
/// "message":PROMPT}`.
#[must_use]
pub(super) fn prompt_start(id: &str, prompt: String) -> OutboundMessage {
    let mut prefix = br#"{"id":"#.to_vec();
    prefix.extend(Value::from(id).to_string().into_bytes());
    prefix.extend(br#","type":"prompt","message":""#);
    OutboundMessage::Start {
        prefix,
        prompt,
        suffix: b"\"}\n".to_vec(),
        escape: escape_json,
    }
}

/// Appends `slice` as the contents of a JSON string, escaped as `serde_json`
/// writes it.
fn escape_json(slice: &str, piece: &mut Vec<u8>) {
    let start = piece.len();
    if serde_json::to_writer(&mut *piece, slice).is_ok() {
        // Drop the quotes around the string.
        piece.remove(start);
        piece.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(value: &Value) -> Result<Record, DecodeError> {
        decode(value.to_string().as_bytes())
    }

    fn usage_json() -> Value {
        json!({"input": 80, "output": 10, "cacheRead": 20, "cacheWrite": 0,
            "totalTokens": 110, "cost": {"total": 0.5}})
    }

    /// The members VIA reads decode; the rest is never read.
    #[test]
    fn known_records_decode() {
        let end = decoded(
            &json!({"type": "message_end", "message": {"role": "assistant",
            "content": [{"type": "thinking", "thinking": "t"}, {"type": "text", "text": "A"},
                {"type": "toolCall", "id": "c"}],
            "stopReason": "stop", "usage": usage_json(), "extra": [1]}}),
        )
        .unwrap();
        let Record::MessageEnd(MessageEnd::Assistant(end)) = end else {
            panic!("{end:?}")
        };
        assert_eq!(end.text, ["A"]);
        assert_eq!(end.usage.cache_read, 20);
        assert_eq!(end.usage.reasoning, None);
        let reply = decoded(
            &json!({"id": "v", "type": "response", "command": "get_state",
            "success": true, "data": {"sessionId": "s"}}),
        )
        .unwrap();
        assert!(matches!(
            reply,
            Record::Response(Response { success: true, .. })
        ));
        assert!(matches!(
            decoded(
                &json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta"}})
            ),
            Ok(Record::MessageUpdate { model: true })
        ));
        assert!(matches!(
            decoded(
                &json!({"type": "message_update", "assistantMessageEvent": {"type": "text_end"}})
            ),
            Ok(Record::MessageUpdate { model: false })
        ));
        assert!(matches!(
            decoded(&json!({"type": "queue_update"})),
            Ok(Record::Activity)
        ));
        let patch = decoded(&json!({"type": "message_end", "message": {"role": "system",
            "sections": {"project_context": null}, "toolsAdded": [{"name": "read"}]}}))
        .unwrap();
        let Record::MessageEnd(MessageEnd::System(patch)) = patch else {
            panic!("{patch:?}")
        };
        assert_eq!(patch.project_context, Section::Removed);
        assert_eq!(patch.tools_added, ["read"]);
    }

    /// Packet §5.1: a missing or malformed member VIA reads is malformed;
    /// all-zero usage is well-formed.
    #[test]
    fn malformed_records_are_refused() {
        let assistant =
            |message: Value| decoded(&json!({"type": "message_end", "message": message}));
        let mut no_content =
            json!({"role": "assistant", "stopReason": "stop", "usage": usage_json()});
        assert!(matches!(
            assistant(no_content.clone()),
            Err(DecodeError::Malformed(_))
        ));
        no_content["content"] = json!([]);
        assert!(assistant(no_content.clone()).is_ok());
        no_content["usage"]["output"] = json!("ten");
        assert!(matches!(
            assistant(no_content.clone()),
            Err(DecodeError::Malformed(_))
        ));
        no_content["usage"]["output"] = json!(-1);
        assert!(matches!(
            assistant(no_content),
            Err(DecodeError::Malformed(_))
        ));
        let zero = json!({"role": "assistant", "content": [], "stopReason": "error",
            "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 0, "cost": {"total": 0}}});
        let Ok(Record::MessageEnd(MessageEnd::Assistant(end))) = assistant(zero) else {
            panic!("all-zero usage refused")
        };
        assert!(end.usage.is_missing());
        assert!(matches!(
            decoded(&json!({"type": "response", "command": "prompt", "success": false})),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            decoded(&json!({"type": "tool_execution_start", "toolCallId": "c"})),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(decoded(&json!(["x"])), Err(DecodeError::NotTyped)));
        assert!(matches!(
            decoded(
                &json!({"type": "message_end", "message": {"role": "assistant",
                "content": [], "stopReason": "s".repeat(SHORT_MAX + 1), "usage": usage_json()}})
            ),
            Err(DecodeError::Malformed(_))
        ));
    }

    /// Packet §5.1: `get_state.data` requires every member VIA checks,
    /// `sessionFile` included (review r1 minor: an absent one passed).
    #[test]
    fn state_data_requires_session_file() {
        let full = json!({"sessionId": "s", "sessionFile": "f.jsonl",
            "model": {"provider": "p", "id": "m"}, "thinkingLevel": "off"});
        let data = |value: &Value| value.as_object().cloned().unwrap();
        let state = state_data(&data(&full)).unwrap();
        assert_eq!(state.session_file, "f.jsonl");
        for member in ["sessionId", "sessionFile", "model", "thinkingLevel"] {
            let mut missing = full.clone();
            missing.as_object_mut().unwrap().remove(member);
            assert!(state_data(&data(&missing)).is_none(), "{member} absent");
        }
        for wrong in [Value::Null, json!(1), json!("f".repeat(4097))] {
            let mut malformed = full.clone();
            malformed["sessionFile"] = wrong;
            assert!(state_data(&data(&malformed)).is_none(), "{malformed}");
        }
    }

    /// The prompt line is one JSON object whatever the prompt holds.
    #[test]
    fn prompt_line_is_one_object() {
        let OutboundMessage::Start {
            prefix,
            prompt,
            suffix,
            escape,
        } = prompt_start("via-1-prompt", "a\"b\n\u{1}".to_owned())
        else {
            panic!("not a start")
        };
        let mut bytes = prefix;
        escape(&prompt, &mut bytes);
        bytes.extend(suffix);
        assert_eq!(bytes.last(), Some(&b'\n'));
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            json!({"id": "via-1-prompt", "type": "prompt", "message": "a\"b\n\u{1}"})
        );
    }
}
