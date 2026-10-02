//! Claude Code stream-json, typed (vendor packet §5): the messages the
//! normalizer reads and the lines VIA writes. Strict about what VIA relies
//! on, tolerant of the rest: unknown fields are ignored, an unknown type or
//! `system` subtype decodes to [`Message::Unknown`] (activity only), and a
//! malformed message of a known type is a [`DecodeError`], which the route
//! turns into `protocol`.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{OutboundMessage, SHORT_FIELD_MAX, UNKNOWN_TAG_MAX};

/// Why a line did not decode.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum DecodeError {
    /// The line is not one JSON object with a string `type`.
    #[error("a stream-json line is not a typed JSON object")]
    NotTyped,
    /// A known message type is missing a field VIA reads, has one of the
    /// wrong shape, or one past its bound.
    #[error("a malformed {0} message")]
    Malformed(&'static str),
}

/// One decoded stream-json message.
#[derive(Debug)]
pub enum Message {
    /// `system/init`: the connection's identity and handshake.
    Init(Box<Init>),
    /// `system/permission_denied`: a live denial.
    PermissionDenied(PermissionDenied),
    /// `assistant`: model output, or a vendor-synthetic error message.
    Assistant(Box<AssistantMessage>),
    /// `user`: tool results, or vendor-inserted user text.
    User(UserMessage),
    /// `result`: the terminal (or a pre-init rejection).
    Result(Box<ResultMessage>),
    /// `control_request`: a request VIA must answer on the control lane.
    ControlRequest(ControlRequest),
    /// `control_response`: an answer to a request VIA sent.
    ControlResponse(ControlResponse),
    /// Any other type or `system` subtype, its tag cut to
    /// [`UNKNOWN_TAG_MAX`] bytes: activity only.
    Unknown {
        /// `type`, or `system/<subtype>`.
        tag: String,
    },
}

/// `system/init`. Only `session_id` is required to decode: the handshake
/// check, not the decoder, judges the version, permission mode, tools and
/// capabilities.
#[derive(Debug, Deserialize)]
pub struct Init {
    /// The vendor session this process runs.
    pub session_id: String,
    /// The complete version string.
    #[serde(default)]
    pub claude_code_version: Option<String>,
    /// The permission-mode echo.
    #[serde(default, rename = "permissionMode")]
    pub permission_mode: Option<String>,
    /// The tool list.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Protocol capabilities, such as `interrupt_receipt_v1`.
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    /// The model as the vendor resolved it.
    #[serde(default)]
    pub model: Option<String>,
    /// MCP servers loaded (inventory).
    #[serde(default)]
    pub mcp_servers: Option<Vec<Value>>,
    /// Plugins loaded (inventory).
    #[serde(default)]
    pub plugins: Option<Vec<Value>>,
    /// Skills loaded (inventory).
    #[serde(default)]
    pub skills: Option<Vec<Value>>,
    /// Agents loaded (inventory).
    #[serde(default)]
    pub agents: Option<Vec<Value>>,
    /// Slash commands loaded (inventory).
    #[serde(default)]
    pub slash_commands: Option<Vec<Value>>,
}

/// `system/permission_denied`.
#[derive(Debug, Deserialize)]
pub struct PermissionDenied {
    /// The denied tool.
    pub tool_name: String,
    /// The denied call.
    pub tool_use_id: String,
    /// Why, as the vendor classifies it (`mode`, …).
    #[serde(default)]
    pub decision_reason_type: Option<String>,
}

/// One content block of an `assistant` or `user` message.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// Reasoning.
    Thinking {},
    /// Redacted reasoning.
    RedactedThinking {},
    /// A tool call.
    ToolUse {
        /// The call's ID.
        id: String,
        /// The tool.
        name: String,
        /// Its input.
        #[serde(default)]
        input: Value,
    },
    /// A tool's result.
    ToolResult {
        /// The call it answers.
        tool_use_id: String,
        /// Whether the call failed or was refused.
        #[serde(default)]
        is_error: Option<bool>,
    },
    /// Any other block type.
    #[serde(other)]
    Other,
}

/// `assistant`.
#[derive(Debug)]
pub struct AssistantMessage {
    /// The vendor message ID; repeated by every block of one message.
    pub id: Option<String>,
    /// The model, `<synthetic>` for a vendor-synthetic message.
    pub model: Option<String>,
    /// The blocks.
    pub content: Vec<Block>,
    /// The vendor session.
    pub session_id: Option<String>,
    /// A synthetic error message's code (`authentication_failed`, …).
    pub error: Option<String>,
    /// A vendor-synthetic API-error message (`is_api_error_message`, or
    /// model `<synthetic>`): never acceptance, progress or final text.
    pub synthetic: bool,
}

#[derive(Deserialize)]
struct RawAssistant {
    message: RawAssistantBody,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    is_api_error_message: Option<bool>,
}

#[derive(Deserialize)]
struct RawAssistantBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    content: Vec<Block>,
}

/// A `user` message's content: plain text or blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    /// Plain text.
    Text(String),
    /// Blocks.
    Blocks(Vec<Block>),
}

/// `user`.
#[derive(Debug)]
pub struct UserMessage {
    /// The content.
    pub content: UserContent,
    /// The vendor session.
    pub session_id: Option<String>,
}

#[derive(Deserialize)]
struct RawUser {
    message: RawUserBody,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct RawUserBody {
    content: UserContent,
}

/// One `result.permission_denials` entry.
#[derive(Debug, Deserialize)]
pub struct PermissionDenial {
    /// The denied tool.
    pub tool_name: String,
    /// The denied call.
    pub tool_use_id: String,
    /// Its input.
    #[serde(default)]
    pub tool_input: Value,
}

/// `result.usage`: the turn aggregate.
#[derive(Debug, Default, Deserialize)]
pub struct ResultUsage {
    /// Uncached input.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Input written to the cache.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    /// Input read from the cache.
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    /// Output, reasoning included.
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Output details.
    #[serde(default)]
    pub output_tokens_details: Option<OutputDetails>,
    /// The fallback credit, kept for vendor data.
    #[serde(default)]
    pub fallback_credit: Option<Value>,
}

/// `result.usage.output_tokens_details`.
#[derive(Debug, Default, Deserialize)]
pub struct OutputDetails {
    /// Reasoning output.
    #[serde(default)]
    pub thinking_tokens: Option<u64>,
}

/// `result`.
#[derive(Debug, Deserialize)]
pub struct ResultMessage {
    /// `success`, `error_during_execution`, `error_max_turns`, …; never
    /// a classification on its own (a `success` can carry `is_error`).
    pub subtype: String,
    /// Whether the turn failed.
    pub is_error: bool,
    /// The vendor session.
    pub session_id: String,
    /// The final text, or the error detail when `is_error`.
    #[serde(default)]
    pub result: Option<String>,
    /// The vendor stop reason.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// `completed`, `aborted_tools`, `api_error`, `max_turns`, …
    #[serde(default)]
    pub terminal_reason: Option<String>,
    /// The HTTP status of an API error.
    #[serde(default)]
    pub api_error_status: Option<u64>,
    /// Agentic iterations the vendor counted.
    #[serde(default)]
    pub num_turns: Option<u64>,
    /// The session's cumulative cost.
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    /// The turn aggregate.
    #[serde(default)]
    pub usage: Option<ResultUsage>,
    /// Per-model usage, read for `costBasis`.
    #[serde(default, rename = "modelUsage")]
    pub model_usage: Option<Map<String, Value>>,
    /// Actions the vendor denied during the turn.
    #[serde(default)]
    pub permission_denials: Vec<PermissionDenial>,
    /// The structured output, verbatim.
    #[serde(default)]
    pub structured_output: Option<Box<RawValue>>,
    /// Error lines (a pre-init rejection's reason).
    #[serde(default)]
    pub errors: Vec<String>,
}

/// `control_request`, flattened.
#[derive(Debug)]
pub struct ControlRequest {
    /// The ID a reply must echo.
    pub request_id: String,
    /// `can_use_tool`, or another subtype.
    pub subtype: String,
    /// The tool, for `can_use_tool`.
    pub tool_name: Option<String>,
    /// The call, for `can_use_tool`.
    pub tool_use_id: Option<String>,
    /// The tool's input, for `can_use_tool`.
    pub input: Value,
}

#[derive(Deserialize)]
struct RawControlRequest {
    request_id: String,
    request: RawControlBody,
}

#[derive(Deserialize)]
struct RawControlBody {
    subtype: String,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    input: Value,
}

/// `control_response`, flattened.
#[derive(Debug)]
pub struct ControlResponse {
    /// `success` or `error`.
    pub subtype: String,
    /// The request it answers.
    pub request_id: Option<String>,
    /// An interrupt receipt's `still_queued`.
    pub still_queued: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct RawControlResponse {
    response: RawControlResponseBody,
}

#[derive(Deserialize)]
struct RawControlResponseBody {
    subtype: String,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    response: Option<RawReceipt>,
}

#[derive(Deserialize)]
struct RawReceipt {
    #[serde(default)]
    still_queued: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    subtype: Option<String>,
}

/// Decodes one stream-json line (without its LF).
pub fn decode(line: &[u8]) -> Result<Message, DecodeError> {
    let envelope: Envelope = serde_json::from_slice(line).map_err(|_| DecodeError::NotTyped)?;
    let message = match (envelope.kind.as_str(), envelope.subtype.as_deref()) {
        ("system", Some("init")) => Message::Init(Box::new(typed(line, "init")?)),
        ("system", Some("permission_denied")) => {
            Message::PermissionDenied(typed(line, "permission_denied")?)
        }
        ("assistant", _) => Message::Assistant(Box::new(assistant(typed(line, "assistant")?))),
        ("user", _) => {
            let raw: RawUser = typed(line, "user")?;
            Message::User(UserMessage {
                content: raw.message.content,
                session_id: raw.session_id,
            })
        }
        ("result", _) => Message::Result(Box::new(typed(line, "result")?)),
        ("control_request", _) => {
            let raw: RawControlRequest = typed(line, "control_request")?;
            Message::ControlRequest(ControlRequest {
                request_id: raw.request_id,
                subtype: raw.request.subtype,
                tool_name: raw.request.tool_name,
                tool_use_id: raw.request.tool_use_id,
                input: raw.request.input,
            })
        }
        ("control_response", _) => {
            let raw: RawControlResponse = typed(line, "control_response")?;
            Message::ControlResponse(ControlResponse {
                subtype: raw.response.subtype,
                request_id: raw.response.request_id,
                still_queued: raw.response.response.and_then(|r| r.still_queued),
            })
        }
        (kind, subtype) => {
            let tag = match subtype {
                Some(subtype) if kind == "system" => format!("system/{subtype}"),
                _ => kind.to_owned(),
            };
            Message::Unknown {
                tag: cut(tag, UNKNOWN_TAG_MAX),
            }
        }
    };
    bounded(&message)?;
    Ok(message)
}

fn typed<T: DeserializeOwned>(line: &[u8], what: &'static str) -> Result<T, DecodeError> {
    serde_json::from_slice(line).map_err(|_| DecodeError::Malformed(what))
}

fn assistant(raw: RawAssistant) -> AssistantMessage {
    let synthetic = raw.is_api_error_message == Some(true)
        || raw.message.model.as_deref() == Some("<synthetic>");
    AssistantMessage {
        id: raw.message.id,
        model: raw.message.model,
        content: raw.message.content,
        session_id: raw.session_id,
        error: raw.error,
        synthetic,
    }
}

/// Task 4 design §2.2 rule 1: an ID, tool name, type tag or stop reason
/// past [`SHORT_FIELD_MAX`] bytes is malformed.
fn bounded(message: &Message) -> Result<(), DecodeError> {
    let short = |field: Option<&str>| field.is_none_or(|field| field.len() <= SHORT_FIELD_MAX);
    let blocks_short = |blocks: &[Block]| {
        blocks.iter().all(|block| match block {
            Block::ToolUse { id, name, .. } => short(Some(id)) && short(Some(name)),
            Block::ToolResult { tool_use_id, .. } => short(Some(tool_use_id)),
            Block::Text { .. } | Block::Thinking {} | Block::RedactedThinking {} | Block::Other => {
                true
            }
        })
    };
    let (ok, what) = match message {
        Message::Init(init) => (
            short(Some(&init.session_id)) && short(init.claude_code_version.as_deref()),
            "init",
        ),
        Message::PermissionDenied(denied) => (
            short(Some(&denied.tool_name)) && short(Some(&denied.tool_use_id)),
            "permission_denied",
        ),
        Message::Assistant(message) => (
            short(message.id.as_deref())
                && short(message.session_id.as_deref())
                && short(message.error.as_deref())
                && blocks_short(&message.content),
            "assistant",
        ),
        Message::User(message) => (
            short(message.session_id.as_deref())
                && match &message.content {
                    UserContent::Text(_) => true,
                    UserContent::Blocks(blocks) => blocks_short(blocks),
                },
            "user",
        ),
        Message::Result(result) => (
            short(Some(&result.subtype))
                && short(Some(&result.session_id))
                && short(result.stop_reason.as_deref())
                && short(result.terminal_reason.as_deref())
                && result
                    .permission_denials
                    .iter()
                    .all(|d| short(Some(&d.tool_name)) && short(Some(&d.tool_use_id))),
            "result",
        ),
        Message::ControlRequest(request) => (
            short(Some(&request.request_id))
                && short(Some(&request.subtype))
                && short(request.tool_name.as_deref())
                && short(request.tool_use_id.as_deref()),
            "control_request",
        ),
        Message::ControlResponse(response) => (
            short(Some(&response.subtype)) && short(response.request_id.as_deref()),
            "control_response",
        ),
        Message::Unknown { .. } => (true, "unknown"),
    };
    if ok {
        Ok(())
    } else {
        Err(DecodeError::Malformed(what))
    }
}

/// `text` cut at a character boundary to at most `max` bytes.
fn cut(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The one user line a launch writes (packet §5), the prompt escaped slice
/// by slice: `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"…"}]}}`.
pub fn user_start(prompt: String) -> OutboundMessage {
    OutboundMessage::Start {
        prefix: br#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":""#
            .to_vec(),
        prompt,
        suffix: b"\"}]}}\n".to_vec(),
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

/// One JSON line with its LF.
fn line(value: &Value) -> Vec<u8> {
    let mut bytes = value.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}

/// The pinned decline of a control request (packet §6), echoing its ID.
pub fn control_decline(request_id: &str) -> Vec<u8> {
    line(&json!({"type": "control_response", "response": {
        "subtype": "error",
        "request_id": request_id,
        "error": "VIA declines unsupported control request",
    }}))
}

/// The interrupt request (packet §7) under `request_id`, which VIA
/// allocates and pairs with the receipt's `response.request_id`.
pub fn interrupt_request(request_id: &str) -> Vec<u8> {
    line(&json!({"type": "control_request", "request_id": request_id,
        "request": {"subtype": "interrupt"}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(line: &Value) -> Message {
        decode(line.to_string().as_bytes()).unwrap()
    }

    /// Each known type decodes to its variant, with the fields VIA reads.
    #[test]
    fn known_messages_decode() {
        let init = decoded(&json!({"type":"system","subtype":"init","session_id":"u",
            "claude_code_version":"2.1.285","permissionMode":"dontAsk","tools":["Read"],
            "capabilities":["interrupt_receipt_v1"],"skills":["s"],"extra":{"x":1}}));
        let Message::Init(init) = init else {
            panic!("init: {init:?}")
        };
        assert_eq!(init.claude_code_version.as_deref(), Some("2.1.285"));
        assert_eq!(init.permission_mode.as_deref(), Some("dontAsk"));

        let assistant = decoded(&json!({"type":"assistant","session_id":"u","message":{
            "id":"m","model":"x","content":[{"type":"thinking","thinking":"","signature":"s"},
            {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}},
            {"type":"image","source":{}}]}}));
        let Message::Assistant(assistant) = assistant else {
            panic!("assistant: {assistant:?}")
        };
        assert!(!assistant.synthetic);
        assert!(matches!(assistant.content[0], Block::Thinking {}));
        assert!(matches!(&assistant.content[1], Block::ToolUse { id, .. } if id == "t1"));
        assert!(matches!(assistant.content[2], Block::Other));

        let synthetic = decoded(&json!({"type":"assistant","message":{"model":"<synthetic>",
            "content":[{"type":"text","text":"Not logged in"}]},"error":"authentication_failed",
            "is_api_error_message":true}));
        assert!(matches!(synthetic, Message::Assistant(a) if a.synthetic));

        let user = decoded(&json!({"type":"user","message":{"role":"user",
            "content":[{"type":"tool_result","tool_use_id":"t1","content":"x","is_error":true}]}}));
        assert!(matches!(user, Message::User(UserMessage {
            content: UserContent::Blocks(ref b), ..}) if matches!(&b[0],
                Block::ToolResult { tool_use_id, is_error: Some(true) } if tool_use_id == "t1")));
        let text = decoded(&json!({"type":"user","message":{"role":"user","content":"hi"}}));
        assert!(matches!(
            text,
            Message::User(UserMessage {
                content: UserContent::Text(_),
                ..
            })
        ));

        let result = decode(
            br#"{"type":"result","subtype":"success","is_error":false,"session_id":"u",
            "result":"ok","structured_output":{"b":1,"a":2},
            "permission_denials":[{"tool_name":"Edit","tool_use_id":"t2","tool_input":{}}]}"#,
        )
        .unwrap();
        let Message::Result(result) = result else {
            panic!("result: {result:?}")
        };
        assert_eq!(
            result.structured_output.as_ref().map(|raw| raw.get()),
            Some(r#"{"b":1,"a":2}"#)
        );
        assert_eq!(result.permission_denials[0].tool_use_id, "t2");
        let null_output = decoded(
            &json!({"type":"result","subtype":"success","is_error":false,
            "session_id":"u","structured_output":null}),
        );
        assert!(matches!(null_output, Message::Result(r) if r.structured_output.is_none()));

        let request = decoded(&json!({"type":"control_request","request_id":"r1",
            "request":{"subtype":"can_use_tool","tool_name":"Edit","tool_use_id":"t1",
            "input":{"file_path":"/w/p"}}}));
        assert!(matches!(request, Message::ControlRequest(r)
            if r.subtype == "can_use_tool" && r.tool_use_id.as_deref() == Some("t1")));
        let receipt = decoded(
            &json!({"type":"control_response","response":{"subtype":"success",
            "request_id":"r1","response":{"still_queued":[]}}}),
        );
        assert!(matches!(receipt, Message::ControlResponse(r)
            if r.request_id.as_deref() == Some("r1") && r.still_queued == Some(vec![])));
        let denied = decoded(&json!({"type":"system","subtype":"permission_denied",
            "tool_name":"Edit","tool_use_id":"t2","decision_reason_type":"mode"}));
        assert!(matches!(denied, Message::PermissionDenied(d) if d.tool_use_id == "t2"));
    }

    /// Unknown types and `system` subtypes are activity only, with their
    /// tag cut to its bound.
    #[test]
    fn unknown_messages_keep_a_bounded_tag() {
        let rate = decoded(&json!({"type":"rate_limit_event","rate_limit_info":{}}));
        assert!(matches!(rate, Message::Unknown { tag } if tag == "rate_limit_event"));
        let thinking = decoded(&json!({"type":"system","subtype":"thinking_tokens"}));
        assert!(matches!(thinking, Message::Unknown { tag } if tag == "system/thinking_tokens"));
        let long = "é".repeat(UNKNOWN_TAG_MAX);
        let Message::Unknown { tag } = decoded(&json!({"type": long})) else {
            panic!("unknown")
        };
        assert!(tag.len() <= UNKNOWN_TAG_MAX && tag.len() >= UNKNOWN_TAG_MAX - 1);
    }

    /// A malformed known message is an error, never an unknown fallback;
    /// a line that is not a typed object is too.
    #[test]
    fn malformed_known_messages_are_errors() {
        let cases = [
            (json!({"type":"system","subtype":"init"}), "init"),
            (
                json!({"type":"result","subtype":"success","is_error":false}),
                "result",
            ),
            (
                json!({"type":"result","subtype":"success","session_id":"u"}),
                "result",
            ),
            (
                json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"x"}]}}),
                "assistant",
            ),
            (
                json!({"type":"control_request","request":{"subtype":"can_use_tool"}}),
                "control_request",
            ),
            (
                json!({"type":"control_request","request_id":7,"request":{"subtype":"x"}}),
                "control_request",
            ),
            (
                json!({"type":"result","subtype":"success","is_error":false,
                "session_id": "u".repeat(SHORT_FIELD_MAX + 1)}),
                "result",
            ),
        ];
        for (line, what) in cases {
            assert_eq!(
                decode(line.to_string().as_bytes()).unwrap_err(),
                DecodeError::Malformed(what),
                "{line}"
            );
        }
        for line in ["[]", "not json", r#"{"type":1}"#, r#"{"x":1}"#] {
            assert_eq!(
                decode(line.as_bytes()).unwrap_err(),
                DecodeError::NotTyped,
                "{line}"
            );
        }
    }

    /// The encoders write the packet's pinned lines.
    #[test]
    fn encoders_write_the_pinned_lines() {
        let OutboundMessage::Start {
            prefix,
            prompt,
            suffix,
            escape,
        } = user_start("say \"hi\"\n".to_owned())
        else {
            panic!("not a start")
        };
        let mut bytes = prefix;
        escape(&prompt, &mut bytes);
        bytes.extend(suffix);
        assert_eq!(bytes.last(), Some(&b'\n'));
        let written: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            written,
            json!({"type":"user","message":{"role":"user",
                "content":[{"type":"text","text":"say \"hi\"\n"}]}})
        );
        let decline: Value = serde_json::from_slice(&control_decline("r-1")).unwrap();
        assert_eq!(
            decline,
            json!({"type":"control_response","response":{"subtype":"error","request_id":"r-1",
                "error":"VIA declines unsupported control request"}})
        );
        let interrupt: Value = serde_json::from_slice(&interrupt_request("i-1")).unwrap();
        assert_eq!(
            interrupt,
            json!({"type":"control_request","request_id":"i-1","request":{"subtype":"interrupt"}})
        );
    }
}
