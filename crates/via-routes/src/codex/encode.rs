//! The requests VIA writes to a Codex app-server and its server-request
//! replies (vendors/codex.md §1–§4): newline-delimited JSON-RPC with no
//! `jsonrpc` member, the never-ask settings on every thread and turn.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};
use thiserror::Error;

use super::RequestId;
use crate::OutboundMessage;
use crate::fake::escape_json;

/// The `clientInfo.name` VIA sends; `initialize.userAgent` starts with it.
pub const CLIENT_NAME: &str = "via";

/// The approval policy on every thread and turn: never ask (D3).
const APPROVAL_POLICY: &str = "never";

/// The approvals reviewer on every thread and turn: requests reach VIA's
/// decline path, never an inherited automatic reviewer (packet §4).
const APPROVALS_REVIEWER: &str = "user";

/// The error message of a `-32601` reply (packet §4).
pub const UNSUPPORTED_MESSAGE: &str = "Method not supported by VIA";

/// JSON-RPC "method not found", VIA's answer to every request it does not
/// decline with a body.
const METHOD_NOT_FOUND: i64 = -32601;

/// The ID of a request VIA writes: in the protocol's signed 64-bit range
/// and never negative (review r1 #11).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientId(i64);

impl ClientId {
    /// The ID's value.
    pub fn get(self) -> i64 {
        self.0
    }

    /// The ID as the server echoes it in its reply.
    pub fn request_id(self) -> RequestId {
        RequestId::Int(self.0)
    }
}

impl TryFrom<i64> for ClientId {
    type Error = EncodeError;

    fn try_from(id: i64) -> Result<Self, Self::Error> {
        if id < 0 {
            Err(EncodeError("a negative request ID"))
        } else {
            Ok(Self(id))
        }
    }
}

/// One connection's request IDs, from 1 up. At the end of the range it
/// retires for good: the connection can send no further request and fails,
/// rather than wrap onto an ID a reply may still answer.
#[derive(Debug)]
pub struct ClientIds {
    next: Option<i64>,
}

impl Default for ClientIds {
    fn default() -> Self {
        Self { next: Some(1) }
    }
}

impl ClientIds {
    /// An allocator whose first ID is `first`.
    pub fn starting_at(first: ClientId) -> Self {
        Self {
            next: Some(first.0),
        }
    }

    /// The next ID; `None` once the range is spent.
    #[expect(
        clippy::should_implement_trait,
        reason = "an allocator, not an iterator: retirement is permanent"
    )]
    pub fn next(&mut self) -> Option<ClientId> {
        let id = self.next?;
        self.next = id.checked_add(1);
        Some(ClientId(id))
    }
}

/// A thread's `sandbox` mode at `thread/start` and `thread/resume`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// `read-only`.
    ReadOnly,
    /// `workspace-write`.
    WorkspaceWrite,
    /// `danger-full-access`.
    DangerFullAccess,
}

/// A turn's structured `sandboxPolicy` (packet §3 table).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SandboxPolicy {
    /// `{type: "dangerFullAccess"}`.
    DangerFullAccess,
    /// `{type: "readOnly", networkAccess}`.
    ReadOnly {
        /// Whether the network is allowed.
        network_access: bool,
    },
    /// `{type: "workspaceWrite", writableRoots, networkAccess,
    /// excludeSlashTmp, excludeTmpdirEnvVar}`.
    WorkspaceWrite {
        /// The extra writable directories; cwd is the implicit root.
        writable_roots: Vec<PathBuf>,
        /// Whether the network is allowed.
        network_access: bool,
        /// Whether `/tmp` is excluded from the writable roots.
        exclude_slash_tmp: bool,
        /// Whether `$TMPDIR` is excluded from the writable roots.
        exclude_tmpdir_env_var: bool,
    },
}

/// A request VIA could not encode: a value JSON text cannot carry.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("cannot encode the Codex request: {0}")]
pub struct EncodeError(&'static str);

/// The canonical thread settings of `thread/start` and `thread/resume`.
#[derive(Clone, Copy, Debug)]
pub struct ThreadSettings<'a> {
    /// The frozen model.
    pub model: &'a str,
    /// The session's frozen working directory.
    pub cwd: &'a Path,
    /// The frozen instructions, sent byte for byte; none when null.
    pub developer_instructions: Option<&'a str>,
    /// The current bound's sandbox mode.
    pub sandbox: SandboxMode,
}

/// A `turn/start`'s values besides the prompt: the frozen settings, sent
/// on every turn, since turn overrides persist as thread defaults.
#[derive(Clone, Copy, Debug)]
pub struct TurnStart<'a> {
    /// The thread.
    pub thread_id: &'a str,
    /// The session's frozen working directory.
    pub cwd: &'a Path,
    /// The frozen model.
    pub model: &'a str,
    /// The turn's effort; null when it sets none.
    pub effort: Option<&'a str>,
    /// The turn's output schema; null clears an inherited one.
    pub output_schema: Option<&'a RawValue>,
    /// The current bound's structured policy.
    pub sandbox_policy: &'a SandboxPolicy,
}

/// `initialize`: `clientInfo` only, with no experimental capability and
/// no notification opt-out (packet §1, K17).
pub fn initialize(id: ClientId, version: &str) -> Result<Vec<u8>, EncodeError> {
    request(
        id,
        "initialize",
        &json!({"clientInfo": {"name": CLIENT_NAME, "version": version}}),
    )
}

/// `initialized`, the notification after the `initialize` reply.
pub fn initialized() -> Result<Vec<u8>, EncodeError> {
    line(&json!({"method": "initialized"}))
}

/// `model/list`, one page: the first, or the one `cursor` names.
pub fn model_list(id: ClientId, cursor: Option<&str>) -> Result<Vec<u8>, EncodeError> {
    let mut params = Map::new();
    if let Some(cursor) = cursor {
        params.insert("cursor".to_owned(), json!(cursor));
    }
    request(id, "model/list", &Value::Object(params))
}

/// `thread/start` with the canonical settings, never ask, and a
/// persistent thread (packet §3).
pub fn thread_start(id: ClientId, settings: &ThreadSettings<'_>) -> Result<Vec<u8>, EncodeError> {
    let mut params = thread_params(settings)?;
    params.insert("ephemeral".to_owned(), json!(false));
    request(id, "thread/start", &Value::Object(params))
}

/// `thread/resume` of the exact stored thread, with the canonical settings
/// and the current bound's sandbox, never hydrating history (packet §3).
pub fn thread_resume(
    id: ClientId,
    thread_id: &str,
    settings: &ThreadSettings<'_>,
) -> Result<Vec<u8>, EncodeError> {
    let mut params = thread_params(settings)?;
    params.insert("threadId".to_owned(), json!(thread_id));
    params.insert("excludeTurns".to_owned(), json!(true));
    request(id, "thread/resume", &Value::Object(params))
}

/// `turn/start`, streamed by Wire with the prompt escaped slice by slice
/// between the encoded members, so no second whole copy of it is made.
pub fn turn_start(
    id: ClientId,
    start: &TurnStart<'_>,
    prompt: String,
) -> Result<OutboundMessage, EncodeError> {
    let head = json!({"id": id.get(), "method": "turn/start"});
    let thread_id = serde_json::to_string(start.thread_id).map_err(|_| EncodeError("thread"))?;
    let policy =
        serde_json::to_value(start.sandbox_policy).map_err(|_| EncodeError("a writable root"))?;
    let schema = match start.output_schema {
        Some(schema) => {
            serde_json::from_str(schema.get()).map_err(|_| EncodeError("an output schema"))?
        }
        None => Value::Null,
    };
    let rest = json!({
        "cwd": utf8(start.cwd)?,
        "model": start.model,
        "effort": start.effort,
        "outputSchema": schema,
        "approvalPolicy": APPROVAL_POLICY,
        "approvalsReviewer": APPROVALS_REVIEWER,
        "sandboxPolicy": policy,
    });
    let head = members(&head)?;
    let rest = members(&rest)?;
    Ok(OutboundMessage::Start {
        prefix: format!(
            r#"{{{head},"params":{{"threadId":{thread_id},"input":[{{"type":"text","text":""#
        )
        .into_bytes(),
        prompt,
        suffix: format!("\"}}],{rest}}}}}\n").into_bytes(),
        escape: escape_json,
    })
}

/// `turn/steer` into the expected active turn (packet §3).
pub fn turn_steer(
    id: ClientId,
    thread_id: &str,
    expected_turn_id: &str,
    text: &str,
) -> Result<Vec<u8>, EncodeError> {
    request(
        id,
        "turn/steer",
        &json!({"threadId": thread_id, "expectedTurnId": expected_turn_id,
            "input": [{"type": "text", "text": text}]}),
    )
}

/// `turn/interrupt` of one turn.
pub fn turn_interrupt(
    id: ClientId,
    thread_id: &str,
    turn_id: &str,
) -> Result<Vec<u8>, EncodeError> {
    request(
        id,
        "turn/interrupt",
        &json!({"threadId": thread_id, "turnId": turn_id}),
    )
}

/// `thread/unsubscribe`: one session's detach; never a stdin close.
pub fn thread_unsubscribe(id: ClientId, thread_id: &str) -> Result<Vec<u8>, EncodeError> {
    request(id, "thread/unsubscribe", &json!({"threadId": thread_id}))
}

/// The server requests VIA declines with a no-grant body, by method; every
/// other request gets `-32601` (packet §4). The Adapter owns the content.
#[derive(Clone, Copy, Debug)]
pub struct DeclineTable {
    bodies: &'static [(&'static str, &'static str)],
}

impl DeclineTable {
    /// A table of `(method, result JSON)` rows.
    pub const fn new(bodies: &'static [(&'static str, &'static str)]) -> Self {
        Self { bodies }
    }

    /// Whether `method` gets a no-grant body rather than `-32601`.
    pub fn declines(&self, method: &str) -> bool {
        self.body(method).is_some()
    }

    /// The reply to a server request, under its exact ID.
    pub fn reply(&self, id: &RequestId, method: &str) -> Vec<u8> {
        let id = id_json(id);
        let line = match self.body(method) {
            Some(body) => format!(r#"{{"id":{id},"result":{body}}}"#),
            None => format!(
                r#"{{"id":{id},"error":{{"code":{METHOD_NOT_FOUND},"message":"{UNSUPPORTED_MESSAGE}"}}}}"#
            ),
        };
        let mut line = line.into_bytes();
        line.push(b'\n');
        line
    }

    fn body(&self, method: &str) -> Option<&'static str> {
        self.bodies
            .iter()
            .find(|(listed, _)| *listed == method)
            .map(|(_, body)| *body)
    }
}

/// The members of a canonical thread's settings.
fn thread_params(settings: &ThreadSettings<'_>) -> Result<Map<String, Value>, EncodeError> {
    let mut params = Map::new();
    params.insert("model".to_owned(), json!(settings.model));
    params.insert("cwd".to_owned(), json!(utf8(settings.cwd)?));
    if let Some(instructions) = settings.developer_instructions {
        params.insert("developerInstructions".to_owned(), json!(instructions));
    }
    params.insert("sandbox".to_owned(), json!(settings.sandbox));
    params.insert("approvalPolicy".to_owned(), json!(APPROVAL_POLICY));
    params.insert("approvalsReviewer".to_owned(), json!(APPROVALS_REVIEWER));
    Ok(params)
}

/// A request line: `{"id", "method", "params"}`.
fn request(id: ClientId, method: &str, params: &Value) -> Result<Vec<u8>, EncodeError> {
    line(&json!({"id": id.get(), "method": method, "params": params}))
}

/// One JSON line, LF-terminated.
fn line(value: &Value) -> Result<Vec<u8>, EncodeError> {
    let mut line = serde_json::to_vec(value).map_err(|_| EncodeError("a value"))?;
    line.push(b'\n');
    Ok(line)
}

/// An object's members, without its braces.
fn members(value: &Value) -> Result<String, EncodeError> {
    let text = serde_json::to_string(value).map_err(|_| EncodeError("a value"))?;
    text.strip_prefix('{')
        .and_then(|text| text.strip_suffix('}'))
        .map(str::to_owned)
        .ok_or(EncodeError("not an object"))
}

/// A path as JSON text; one that is not UTF-8 cannot be sent.
fn utf8(path: &Path) -> Result<&str, EncodeError> {
    path.to_str().ok_or(EncodeError("a path that is not UTF-8"))
}

/// A request ID as JSON.
fn id_json(id: &RequestId) -> String {
    match id {
        RequestId::Int(number) => number.to_string(),
        RequestId::Str(text) => Value::String(text.clone()).to_string(),
    }
}
