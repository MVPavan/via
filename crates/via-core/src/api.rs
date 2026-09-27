use std::{
    fmt,
    fs::File,
    io::Read,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{ConnectionId, RawRef, SessionId, TurnNumber, TurnState};

/// Strict C1 parameters for creating Task 1's fake session and first turn.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnParams {
    /// Selected harness, currently `fake` in S1.
    pub harness: String,
    /// Explicit model name.
    pub model: String,
    /// User prompt delivered once after submission intent commits.
    pub prompt: String,
    /// Caller-owned 256-bit bearer handle.
    pub handle: String,
    /// C1 P4 retry key: the same key, handle and params replay the receipt.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// Strict C1 §3.3 `resume` parameters; the fake route has no per-turn options.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeParams {
    /// Session to add a turn to.
    pub session: SessionId,
    /// Caller-owned bearer handle.
    pub handle: String,
    /// The new turn's prompt.
    pub prompt: String,
    /// C1 §3 retry key: the same key and params replay the turn receipt.
    #[serde(default)]
    pub op_key: Option<String>,
}

/// Strict C1 §3.8 `wait` parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitParams {
    /// A canonical session or turn address.
    pub address: String,
    /// Bound on the wait; [`DEFAULT_WAIT_MS`] when absent.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// `wait` bound when the caller gives no `timeout_ms`.
pub const DEFAULT_WAIT_MS: u64 = 30_000;

/// Strict C1 steer parameters; fake must refuse after authentication.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteerParams {
    /// Session to mutate.
    pub session: SessionId,
    /// Text that will not be sent on unsupported routes.
    pub text: String,
    /// Caller-owned bearer handle.
    pub handle: String,
}

/// Strict C1 read-address parameter set.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    /// A canonical session or turn address.
    pub address: String,
}

/// Strict C1 session-address parameters for the current `events` and `logs`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionReadParams {
    /// Session whose durable history is read.
    pub session: SessionId,
}

/// Strict C1 `daemon/status` parameters; the method takes none.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStatusParams {}

/// Strict C1 §3.14 `daemon/stop` parameters; `drain` and `force` exclude each other.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStopParams {
    /// Refuse new work, let accepted turns finish, then stop.
    #[serde(default)]
    pub drain: bool,
    /// Close every session with mode `force` and stop at once.
    #[serde(default)]
    pub force: bool,
}

/// Named C1 request error without sensitive input in its message.
#[derive(Clone, Debug)]
pub struct ApiError {
    /// JSON-RPC error code.
    pub code: i32,
    /// Stable C1 kind.
    pub kind: &'static str,
    /// Bounded public explanation.
    pub message: &'static str,
    /// C1 §3.8/§9 facts of a receipted turn whose terminal is not durable.
    pub unpersisted: Option<Box<Unpersisted>>,
    /// C1 §8.1 `invalid_params.data.kind2` refinement.
    pub kind2: Option<&'static str>,
    /// C1 §8.1 `store_error` before a receipt: what happened to its commit.
    pub commit_outcome: Option<ReceiptOutcome>,
}

/// C1 §8.1 `data.commit_outcome` of a `store_error` before a receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptOutcome {
    /// The receipt definitely did not commit.
    NotCommitted,
    /// The receipt may have committed; only the same keyed retry can tell.
    Unknown,
}

/// A receipted turn whose terminal could not be made durable (C1 `store_error`).
#[derive(Clone, Debug)]
pub struct Unpersisted {
    session: SessionId,
    turn: TurnNumber,
    durable_state: TurnState,
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// C1 §3.8 `store_error` for a receipted turn whose terminal is not durable;
    /// `durable_state` is the turn's last committed lifecycle state.
    pub fn unpersisted(session: &SessionId, turn: TurnNumber, durable_state: TurnState) -> Self {
        Self {
            unpersisted: Some(Box::new(Unpersisted {
                session: session.clone(),
                turn,
                durable_state,
            })),
            ..Self::STORE
        }
    }

    /// C1 §8.1 `store_error` for a receipt commit that definitely did not happen.
    pub const RECEIPT_NOT_COMMITTED: Self = Self {
        commit_outcome: Some(ReceiptOutcome::NotCommitted),
        ..Self::STORE
    };

    /// C1 §8.1 `store_error` for a receipt commit whose outcome is unknown.
    pub const RECEIPT_UNKNOWN: Self = Self {
        commit_outcome: Some(ReceiptOutcome::Unknown),
        ..Self::STORE
    };

    /// The C1 §9 JSON-RPC `error.data` object: `kind` plus the kind's own fields.
    pub fn data(&self) -> Value {
        let mut data = json!({"kind":self.kind});
        if let Some(kind2) = self.kind2 {
            data["kind2"] = json!(kind2);
        }
        match self.commit_outcome {
            Some(ReceiptOutcome::NotCommitted) => data["commit_outcome"] = json!("not_committed"),
            Some(ReceiptOutcome::Unknown) => {
                data["commit_outcome"] = json!("unknown");
                // Only the same keyed request can find out what happened.
                data["retry"] = json!("same_key_only");
            }
            None => {}
        }
        if let Some(turn) = &self.unpersisted {
            data["session"] = json!(turn.session.as_str());
            data["turn"] = json!(turn.turn.get());
            data["durable_state"] = json!(turn.durable_state.as_str());
            data["terminal_persisted"] = json!(false);
        }
        data
    }

    /// Invalid request fields or identifier format.
    pub const INVALID_PARAMS: Self = Self {
        code: -32602,
        kind: "invalid_params",
        message: "invalid parameters",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// A caller handle did not authorize a mutation.
    pub const INVALID_HANDLE: Self = Self {
        code: -32002,
        kind: "invalid_handle",
        message: "invalid session handle",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The selected route has no such control capability.
    pub const UNSUPPORTED_VERB: Self = Self {
        code: -32006,
        kind: "unsupported_verb",
        message: "verb is unsupported on this route",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The fake route is not configured or selected.
    pub const HARNESS_UNAVAILABLE: Self = Self {
        code: -32009,
        kind: "harness_unavailable",
        message: "harness is unavailable",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The daemon accepted a stop and admits no new work.
    pub const DAEMON_STOPPING: Self = Self {
        code: -32017,
        kind: "daemon_stopping",
        message: "the daemon is stopping",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// Active sessions refuse a plain stop (C1 §3.14).
    pub const SESSIONS_ACTIVE: Self = Self {
        code: -32012,
        kind: "admission_refused",
        message: "sessions are active",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The daemon already retains its bound of turns without a durable terminal.
    pub const TURNS_AT_CAPACITY: Self = Self {
        code: -32012,
        kind: "admission_refused",
        message: "too many unresolved turns",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The Store cannot establish or read the required durable state.
    pub const STORE: Self = Self {
        code: -32018,
        kind: "store_error",
        message: "durable storage failed",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The turn has not yet ended.
    pub const TURN_NOT_FINISHED: Self = Self {
        code: -32015,
        kind: "turn_not_finished",
        message: "turn has not finished",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// A wait deadline elapsed while the turn remains active.
    pub const WAIT_TIMEOUT: Self = Self {
        code: -32016,
        kind: "wait_timeout",
        message: "wait timed out",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The requested session is absent.
    pub const SESSION_NOT_FOUND: Self = Self {
        code: -32003,
        kind: "session_not_found",
        message: "session does not exist",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The session is closed or closing.
    pub const SESSION_CLOSED: Self = Self {
        code: -32004,
        kind: "session_closed",
        message: "session is closed",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The session exists but has no such turn.
    pub const TURN_NOT_FOUND: Self = Self {
        code: -32005,
        kind: "turn_not_found",
        message: "turn does not exist",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The session already holds its bound of queued turns (C1 P6).
    pub const QUEUE_FULL: Self = Self {
        code: -32011,
        kind: "queue_full",
        message: "the session queue is full",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// The daemon already holds its bound of queued turns (runtime §8).
    pub const QUEUED_AT_CAPACITY: Self = Self {
        code: -32012,
        kind: "admission_refused",
        message: "too many queued turns",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
    };
    /// A retry key was reused with another handle or other params (C1 P4).
    pub const IDEMPOTENCY_CONFLICT: Self = Self {
        code: -32602,
        kind: "invalid_params",
        message: "retry key reused with different parameters",
        unpersisted: None,
        kind2: Some("idempotency_conflict"),
        commit_outcome: None,
    };
}

/// Hashes only a canonical handle; the plaintext must never cross into Store.
pub fn hash_handle(handle: &str) -> Result<[u8; 32], ApiError> {
    let body = handle.strip_prefix("h_").ok_or(ApiError::INVALID_HANDLE)?;
    if body.len() != 43
        || !body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(ApiError::INVALID_HANDLE);
    }
    let last = body.as_bytes()[42];
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let index = alphabet
        .iter()
        .position(|byte| *byte == last)
        .ok_or(ApiError::INVALID_HANDLE)?;
    if index & 0b11 != 0 {
        return Err(ApiError::INVALID_HANDLE);
    }
    Ok(Sha256::digest(handle.as_bytes()).into())
}

/// Parses a session or one-based turn address; a bare session names no turn,
/// which the caller resolves to the latest one (C1 §3).
pub fn parse_address(address: &str) -> Result<(SessionId, Option<TurnNumber>), ApiError> {
    let (session, turn) = match address.split_once('/') {
        Some((session, turn)) => {
            let turn = turn.parse::<u32>().map_err(|_| ApiError::INVALID_PARAMS)?;
            (
                session,
                Some(TurnNumber::try_from(turn).map_err(|_| ApiError::INVALID_PARAMS)?),
            )
        }
        None => (address, None),
    };
    Ok((
        SessionId::try_from(session).map_err(|_| ApiError::INVALID_PARAMS)?,
        turn,
    ))
}

/// Longest `idempotency_key` or `op_key` accepted (C1 §3).
pub(crate) const RETRY_KEY_LIMIT: usize = 64;

/// Checks a retry key's C1 §3 bound: 1–64 printable ASCII characters
/// (0x21–0x7E), so its characters are its bytes.
pub(crate) fn retry_key(key: Option<&str>) -> Result<Option<&str>, ApiError> {
    match key {
        Some(key)
            if key.is_empty()
                || key.len() > RETRY_KEY_LIMIT
                || !key.bytes().all(|byte| byte.is_ascii_graphic()) =>
        {
            Err(ApiError::INVALID_PARAMS)
        }
        key => Ok(key),
    }
}

/// Exact retry identity (C1 P4, runtime §6): the original params object's
/// bytes with the top-level `handle` value replaced by its hash. Every other
/// byte, whitespace and member order included, is kept, so only a
/// byte-identical retry matches. Duplicate top-level members are refused.
pub fn retry_identity(raw_params: &str, handle_hash: &[u8; 32]) -> Result<Vec<u8>, ApiError> {
    use std::collections::HashSet;

    use serde::de::{Deserializer, MapAccess, Visitor};
    use serde_json::value::RawValue;

    /// The byte range of the top-level `handle` value within the params text.
    struct Handle(Option<(usize, usize)>);

    struct Members<'a>(&'a str);

    impl<'de> Visitor<'de> for Members<'de> {
        type Value = Handle;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a params object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Handle, A::Error> {
            let mut seen = HashSet::new();
            let mut handle = None;
            while let Some((key, value)) = map.next_entry::<String, &'de RawValue>()? {
                if key == "handle" {
                    let start = (value.get().as_ptr() as usize)
                        .checked_sub(self.0.as_ptr() as usize)
                        .ok_or_else(|| serde::de::Error::custom("handle outside params"))?;
                    handle = Some((start, start + value.get().len()));
                }
                if !seen.insert(key) {
                    return Err(serde::de::Error::custom("duplicate params member"));
                }
            }
            Ok(Handle(handle))
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(raw_params);
    let Handle(handle) = deserializer
        .deserialize_map(Members(raw_params))
        .map_err(|_| ApiError::INVALID_PARAMS)?;
    deserializer.end().map_err(|_| ApiError::INVALID_PARAMS)?;
    let (start, end) = handle.ok_or(ApiError::INVALID_PARAMS)?;
    let mut identity = Vec::with_capacity(raw_params.len());
    identity.extend_from_slice(&raw_params.as_bytes()[..start]);
    identity.push(b'"');
    for byte in handle_hash {
        identity.extend_from_slice(format!("{byte:02x}").as_bytes());
    }
    identity.push(b'"');
    identity.extend_from_slice(&raw_params.as_bytes()[end..]);
    Ok(identity)
}

pub(crate) fn new_session_id() -> Result<SessionId, ApiError> {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut random = [0; 8];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|_| ApiError::STORE)?;
    let mut bits = u64::from_be_bytes(random) & ((1_u64 << 60) - 1);
    let mut digits = [b'0'; 12];
    for digit in digits.iter_mut().rev() {
        *digit = ALPHABET[(bits & 31) as usize];
        bits >>= 5;
    }
    let suffix = std::str::from_utf8(&digits).map_err(|_| ApiError::STORE)?;
    SessionId::try_from(format!("s_{suffix}").as_str()).map_err(|_| ApiError::STORE)
}

// ---- C1 response DTOs (receipt §3.2, capabilities §4.1, envelope §5, events §6.1) ----

/// The only route this build can plan.
pub(crate) const FAKE_ROUTE: &str = "fake";
/// Absolute wall deadline Core applies to a fake turn; reported as effective.
pub(crate) const FAKE_WALL_MS: u64 = 30_000;

/// One `support` entry of the C1 §4.1 capabilities DTO.
#[derive(Clone, Copy, Serialize)]
#[serde(tag = "support", rename_all = "snake_case")]
pub(crate) enum Support {
    Native,
    Unsupported { reason: &'static str },
}

#[derive(Serialize)]
pub(crate) struct Verbs {
    spawn: Support,
    resume: Support,
    steer: Support,
    cancel: Support,
    close: Support,
}

#[derive(Serialize)]
pub(crate) struct ParamSupport {
    instructions: Support,
    output_schema: Support,
    effort: Support,
    max_steps: Support,
}

#[derive(Serialize)]
pub(crate) struct UsageSupport {
    tokens: &'static str,
    cost: &'static str,
}

/// C1 §4.1 capabilities, stating only what this build actually does.
#[derive(Serialize)]
pub(crate) struct Capabilities {
    verbs: Verbs,
    params: ParamSupport,
    bounds: [&'static str; 0],
    network_control: bool,
    recover: Support,
    usage: UsageSupport,
}

impl Capabilities {
    /// The fake route: one prompt, one turn, no controls, bounds or usage.
    pub(crate) fn fake() -> Self {
        let unsupported = |reason| Support::Unsupported { reason };
        Self {
            verbs: Verbs {
                spawn: Support::Native,
                resume: Support::Native,
                steer: unsupported("the fake route has no steer input"),
                cancel: unsupported("cancel is not implemented in S1"),
                close: unsupported("close is not implemented in S1"),
            },
            params: ParamSupport {
                instructions: unsupported("the fake route has no instructions input"),
                output_schema: unsupported("the fake route has no schema input"),
                effort: unsupported("the fake route has no effort setting"),
                max_steps: unsupported("the fake route has no step limit"),
            },
            bounds: [],
            network_control: false,
            recover: unsupported("fake turns do not survive a daemon restart"),
            usage: UsageSupport {
                tokens: "unavailable",
                cost: "unavailable",
            },
        }
    }
}

#[derive(Clone, Copy, Serialize)]
pub(crate) struct Deadlines {
    wall_ms: u64,
    /// No idle deadline is enforced on the fake route.
    idle_ms: Option<u64>,
}

/// Values frozen at acceptance (§3.2 `effective`, `turn.started` payload).
#[derive(Clone, Serialize)]
pub(crate) struct Effective {
    model: String,
    effort: Option<String>,
    bound: Option<Value>,
    deadlines: Deadlines,
    max_steps: Option<u64>,
}

impl Effective {
    pub(crate) fn fake(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            effort: None,
            bound: None,
            deadlines: Deadlines {
                wall_ms: FAKE_WALL_MS,
                idle_ms: None,
            },
            max_steps: None,
        }
    }
}

/// Route-plan fields shared by the receipt and the envelope.
#[derive(Clone, Serialize)]
pub(crate) struct RoutePlan {
    route: &'static str,
    adapter_version: &'static str,
    vendor_version: Option<String>,
    version_status: &'static str,
}

impl RoutePlan {
    /// The fake agent reports no version, so it is never inside a tested set.
    pub(crate) fn fake() -> Self {
        Self {
            route: FAKE_ROUTE,
            // Workspace crates share one version; via-adapters has no separate constant.
            adapter_version: env!("CARGO_PKG_VERSION"),
            vendor_version: None,
            version_status: "untested",
        }
    }

    pub(crate) fn warnings(&self) -> Vec<Warning> {
        if self.version_status == "untested" {
            vec![Warning {
                code: "vendor_version_untested",
                message: "the fake agent reports no version",
            }]
        } else {
            Vec::new()
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct Warning {
    code: &'static str,
    message: &'static str,
}

impl Warning {
    /// C1 §3.5: a settled cancel whose group absence is unproved.
    pub(crate) const CANCEL_CLEANUP_UNCERTAIN: Self = Self {
        code: "cancel_cleanup_uncertain",
        message: "process group cleanup after cancellation is unconfirmed",
    };

    /// Announces that bytes exchanged with the vendor are missing from the raw log.
    pub(crate) const RAW_LOG_INCOMPLETE: Self = Self {
        code: "raw_log_incomplete",
        message: "the raw log lost bytes for this turn",
    };
}

/// C1 §3.5/§7.4 cancel outcome with separate cleanup certainty.
#[derive(Clone, Serialize)]
pub(crate) struct Cancel {
    pub(crate) outcome: &'static str,
    pub(crate) cleanup: &'static str,
    pub(crate) requested_at: String,
    pub(crate) settled_at: String,
}

/// C1 §3.2 spawn receipt.
#[derive(Serialize)]
pub(crate) struct Receipt {
    pub(crate) session_id: SessionId,
    pub(crate) turn: String,
    pub(crate) state: &'static str,
    #[serde(flatten)]
    pub(crate) plan: RoutePlan,
    pub(crate) capabilities: Capabilities,
    pub(crate) effective: Effective,
    pub(crate) warnings: Vec<Warning>,
}

/// C1 §3.3 turn receipt.
#[derive(Serialize)]
pub(crate) struct TurnReceipt {
    pub(crate) turn: String,
    pub(crate) state: &'static str,
    pub(crate) queue_position: u32,
    pub(crate) effective: Effective,
    pub(crate) warnings: Vec<Warning>,
}

#[derive(Serialize)]
pub(crate) struct Requested<T> {
    pub(crate) requested: T,
    pub(crate) resolved: T,
}

#[derive(Serialize)]
pub(crate) struct Bound {
    requested: Option<Value>,
    effective: Option<Value>,
    inherited: bool,
}

/// C1 §8.2 `failure.class` values Core commits for the fake route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureClass {
    DeadlineWall,
    SubmitFailed,
    VendorError,
    ProcessExited,
    Protocol,
    Overflow,
    Store,
    /// C1 §8.2: the daemon restarted before the turn ended (§7.5).
    DaemonRestart,
}

/// C1 §5 `failure`.
#[derive(Clone, Serialize)]
pub(crate) struct Failure {
    pub(crate) class: FailureClass,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) vendor_code: Option<String>,
    pub(crate) retryable: bool,
}

/// C1 §5 `usage`: every count `null` while provenance is `unavailable`.
#[derive(Serialize)]
pub(crate) struct Usage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    scope: &'static str,
    provenance: &'static str,
}

impl Usage {
    pub(crate) const UNAVAILABLE: Self = Self {
        input_tokens: None,
        cached_input_tokens: None,
        output_tokens: None,
        reasoning_output_tokens: None,
        total_tokens: None,
        scope: "turn",
        provenance: "unavailable",
    };
}

#[derive(Serialize)]
pub(crate) struct Cost {
    usd: Option<f64>,
    scope: &'static str,
    provenance: &'static str,
}

impl Cost {
    pub(crate) const UNAVAILABLE: Self = Self {
        usd: None,
        scope: "turn",
        provenance: "unavailable",
    };
}

#[derive(Serialize)]
#[expect(clippy::struct_field_names, reason = "C1 §5 fixes these wire names")]
pub(crate) struct Timestamps {
    pub(crate) queued_at: String,
    pub(crate) submitted_at: Option<String>,
    pub(crate) accepted_at: Option<String>,
    pub(crate) ended_at: String,
}

#[derive(Serialize)]
pub(crate) struct Exit {
    pub(crate) code: Option<i32>,
    pub(crate) signal: Option<i32>,
}

#[derive(Serialize)]
pub(crate) struct EventRange {
    pub(crate) first_seq: u64,
    pub(crate) last_seq: u64,
    pub(crate) count: u64,
}

/// C1 §5 bounding span of one connection's raw log for a turn; `last_offset` is exclusive.
#[derive(Serialize)]
pub(crate) struct RawSpan {
    connection_id: ConnectionId,
    path: String,
    first_offset: u64,
    last_offset: u64,
}

impl RawSpan {
    /// Widens the per-connection bounding spans, in first-seen order, to cover one
    /// committed event reference.
    pub(crate) fn include(spans: &mut Vec<Self>, reference: &RawRef) {
        let id = reference.connection_id();
        if let Some(span) = spans.iter_mut().find(|span| &span.connection_id == id) {
            span.first_offset = span.first_offset.min(reference.offset());
            span.last_offset = span.last_offset.max(reference.end_offset());
        } else {
            spans.push(Self {
                connection_id: id.clone(),
                path: format!("raw/{}.raw", id.as_str()),
                first_offset: reference.offset(),
                last_offset: reference.end_offset(),
            });
        }
    }
}

#[derive(Serialize)]
pub(crate) struct VendorFields {
    pub(crate) turn_id: Option<String>,
}

/// C1 §5 terminal result envelope.
#[derive(Serialize)]
pub(crate) struct Envelope {
    pub(crate) api_version: u32,
    pub(crate) session_id: SessionId,
    pub(crate) turn: u32,
    pub(crate) address: String,
    pub(crate) revision: u32,
    pub(crate) state: &'static str,
    pub(crate) failure: Option<Failure>,
    pub(crate) stop_reason: &'static str,
    pub(crate) vendor_stop_reason: Option<String>,
    /// Only a forced daemon stop cancels on this route; otherwise `null`.
    pub(crate) cancel: Option<Cancel>,
    pub(crate) harness: &'static str,
    pub(crate) model: Requested<String>,
    pub(crate) effort: Requested<Option<String>>,
    #[serde(flatten)]
    pub(crate) plan: RoutePlan,
    pub(crate) vendor_session_id: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) bound: Bound,
    pub(crate) final_text: String,
    pub(crate) structured_output: Option<Value>,
    pub(crate) denied_actions: [Value; 0],
    pub(crate) auto_declined_requests: [Value; 0],
    pub(crate) steps: Option<u64>,
    pub(crate) usage: Usage,
    pub(crate) cost: Cost,
    pub(crate) timestamps: Timestamps,
    /// Milliseconds from `submitted_at` to `ended_at`; `null` without a submission.
    pub(crate) duration_ms: Option<u64>,
    pub(crate) exit: Option<Exit>,
    pub(crate) events: EventRange,
    pub(crate) raw_spans: Vec<RawSpan>,
    pub(crate) vendor_options: Value,
    pub(crate) warnings: Vec<Warning>,
    pub(crate) vendor: VendorFields,
}

impl Bound {
    /// The fake route enforces no bound; nothing was requested or applied.
    pub(crate) const NONE: Self = Self {
        requested: None,
        effective: None,
        inherited: false,
    };
}

/// C1 §6.1 event payloads Core commits today.
#[derive(Serialize)]
#[serde(tag = "type")]
pub(crate) enum EventBody {
    #[serde(rename = "turn.queued")]
    TurnQueued { queue_position: u32 },
    #[serde(rename = "turn.submitted")]
    TurnSubmitted { attempt: u32 },
    #[serde(rename = "turn.started")]
    TurnStarted { effective: Effective },
    #[serde(rename = "turn.ended")]
    TurnEnded {
        state: &'static str,
        failure: Option<Failure>,
        stop_reason: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        cancel: Option<Cancel>,
    },
    #[serde(rename = "assistant.text")]
    AssistantText {
        text: String,
        #[serde(rename = "final")]
        is_final: bool,
    },
    #[serde(rename = "tool.started")]
    ToolStarted {
        tool_id: String,
        name: String,
        input_summary: String,
    },
    #[serde(rename = "tool.ended")]
    ToolEnded {
        tool_id: String,
        status: &'static str,
        output_summary: String,
        exit_code: Option<i32>,
    },
    /// C2 A1 keeps an explicit `truncated` marker beside the bounded payload.
    #[serde(rename = "vendor.other")]
    VendorOther {
        vendor_type: String,
        payload: String,
        truncated: bool,
    },
    #[serde(rename = "raw_log.incomplete")]
    RawLogIncomplete { connection_id: ConnectionId },
    #[serde(rename = "cancel.requested")]
    CancelRequested {},
    #[serde(rename = "cancel.settled")]
    CancelSettled {
        outcome: &'static str,
        cleanup: &'static str,
    },
    #[serde(rename = "session.closed")]
    SessionClosed { reason: &'static str },
}

/// C1 §6.1 event with every common field.
#[derive(Serialize)]
pub(crate) struct Event<'a> {
    pub(crate) seq: u64,
    pub(crate) session_id: &'a SessionId,
    pub(crate) turn: Option<u32>,
    pub(crate) late: bool,
    pub(crate) at: &'a str,
    pub(crate) raw_ref: Option<&'a RawRef>,
    #[serde(flatten)]
    pub(crate) body: EventBody,
}

impl Event<'_> {
    pub(crate) fn to_value(&self) -> Result<Value, ApiError> {
        serde_json::to_value(self).map_err(|_| ApiError::STORE)
    }
}

/// Formats wall time as RFC 3339 UTC with millisecond precision.
pub(crate) fn rfc3339(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs();
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let clock = seconds % 86_400;
    // Civil-from-days (H. Hinnant), valid for the proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        clock / 3_600,
        clock / 60 % 60,
        clock % 60,
        since.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::{ConnectionId, EventBody, UNIX_EPOCH, retry_identity, retry_key, rfc3339};

    #[test]
    fn observation_events_use_c1_tags_and_fields() {
        let connection = ConnectionId::try_from("c_01").unwrap();
        let bodies = [
            (
                EventBody::AssistantText {
                    text: "t".to_owned(),
                    is_final: false,
                },
                json!({"type":"assistant.text","text":"t","final":false}),
            ),
            (
                EventBody::VendorOther {
                    vendor_type: "note".to_owned(),
                    payload: "{".to_owned(),
                    truncated: true,
                },
                json!({"type":"vendor.other","vendor_type":"note","payload":"{","truncated":true}),
            ),
            (
                EventBody::RawLogIncomplete {
                    connection_id: connection,
                },
                json!({"type":"raw_log.incomplete","connection_id":"c_01"}),
            ),
        ];
        for (body, expected) in bodies {
            assert_eq!(serde_json::to_value(body).unwrap(), expected);
        }
    }

    /// C1 P4 / runtime §6: identity is the params bytes with only the handle
    /// value replaced by its hash; whitespace and member order are kept.
    #[test]
    fn retry_identity_keeps_every_byte_but_the_handle() {
        let hash = [0xab; 32];
        let raw = r#"{"prompt": "p","handle":"h_secret" ,"model":"fake"}"#;
        let identity = String::from_utf8(retry_identity(raw, &hash).unwrap()).unwrap();
        assert_eq!(
            identity,
            format!(
                r#"{{"prompt": "p","handle":"{}" ,"model":"fake"}}"#,
                "ab".repeat(32)
            )
        );
        assert!(!identity.contains("h_secret"));
        let respaced = r#"{"prompt":"p","handle":"h_secret" ,"model":"fake"}"#;
        assert_ne!(
            retry_identity(respaced, &hash).unwrap(),
            identity.as_bytes()
        );
        assert_ne!(retry_identity(raw, &[0; 32]).unwrap(), identity.as_bytes());
    }

    /// C1 §3: `idempotency_key` and `op_key` are 1–64 printable ASCII
    /// characters (0x21–0x7E), so characters equal bytes.
    #[test]
    fn retry_keys_are_one_to_sixty_four_printable_ascii_characters() {
        let longest = "~".repeat(64);
        for key in ["k", "!", longest.as_str(), "k-17_A.b:c"] {
            assert_eq!(retry_key(Some(key)).unwrap(), Some(key), "{key}");
        }
        let too_long = "k".repeat(65);
        // 32 two-byte characters: 64 bytes, but not ASCII.
        let wide = "é".repeat(32);
        for key in [
            "",
            " ",
            "a b",
            "k\u{7f}",
            "tab\t",
            too_long.as_str(),
            wide.as_str(),
        ] {
            assert_eq!(
                retry_key(Some(key)).unwrap_err().kind,
                "invalid_params",
                "{key:?}"
            );
        }
        assert_eq!(retry_key(None).unwrap(), None);
    }

    #[test]
    fn retry_identity_refuses_duplicate_members_and_non_objects() {
        let hash = [0; 32];
        for raw in [
            r#"{"handle":"a","handle":"b"}"#,
            r#"{"prompt":"a","prompt":"b","handle":"h"}"#,
            r#"{"prompt":"p"}"#,
            r#"["handle"]"#,
            r#"{"handle":"h"} trailing"#,
        ] {
            assert_eq!(
                retry_identity(raw, &hash).unwrap_err().kind,
                "invalid_params",
                "{raw}"
            );
        }
    }

    #[test]
    fn rfc3339_formats_utc_milliseconds() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let leap_day = UNIX_EPOCH + Duration::from_millis(951_868_799_042);
        assert_eq!(rfc3339(leap_day), "2000-02-29T23:59:59.042Z");
        let new_year = UNIX_EPOCH + Duration::from_hours(499_656);
        assert_eq!(rfc3339(new_year), "2027-01-01T00:00:00.000Z");
    }
}
