use std::{
    fmt,
    fs::File,
    io::Read,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{ConnectionId, RawRef, SessionId, TurnNumber};

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
}

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

/// Strict C1 `daemon/stop` parameters currently accepted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStopParams {
    /// Stop even while sessions are active.
    #[serde(default)]
    pub force: bool,
}

/// Named C1 request error without sensitive input in its message.
#[derive(Clone, Copy, Debug)]
pub struct ApiError {
    /// JSON-RPC error code.
    pub code: i32,
    /// Stable C1 kind.
    pub kind: &'static str,
    /// Bounded public explanation.
    pub message: &'static str,
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// Invalid request fields or identifier format.
    pub const INVALID_PARAMS: Self = Self {
        code: -32602,
        kind: "invalid_params",
        message: "invalid parameters",
    };
    /// A caller handle did not authorize a mutation.
    pub const INVALID_HANDLE: Self = Self {
        code: -32002,
        kind: "invalid_handle",
        message: "invalid session handle",
    };
    /// The selected route has no such control capability.
    pub const UNSUPPORTED_VERB: Self = Self {
        code: -32006,
        kind: "unsupported_verb",
        message: "verb is unsupported on this route",
    };
    /// The fake route is not configured or selected.
    pub const HARNESS_UNAVAILABLE: Self = Self {
        code: -32009,
        kind: "harness_unavailable",
        message: "harness is unavailable",
    };
    /// The Store cannot establish or read the required durable state.
    pub const STORE: Self = Self {
        code: -32018,
        kind: "store_error",
        message: "durable storage failed",
    };
    /// The turn has not yet ended.
    pub const TURN_NOT_FINISHED: Self = Self {
        code: -32015,
        kind: "turn_not_finished",
        message: "turn has not finished",
    };
    /// A wait deadline elapsed while the turn remains active.
    pub const WAIT_TIMEOUT: Self = Self {
        code: -32016,
        kind: "wait_timeout",
        message: "wait timed out",
    };
    /// The requested session is absent.
    pub const SESSION_NOT_FOUND: Self = Self {
        code: -32003,
        kind: "session_not_found",
        message: "session does not exist",
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

/// Parses a session or one-based turn address without inventing a latest turn.
pub fn parse_address(address: &str) -> Result<(SessionId, TurnNumber), ApiError> {
    let (session, turn) = match address.split_once('/') {
        Some((session, turn)) => {
            let turn = turn.parse::<u32>().map_err(|_| ApiError::INVALID_PARAMS)?;
            (
                session,
                TurnNumber::try_from(turn).map_err(|_| ApiError::INVALID_PARAMS)?,
            )
        }
        None => (
            address,
            TurnNumber::try_from(1).map_err(|_| ApiError::INVALID_PARAMS)?,
        ),
    };
    Ok((
        SessionId::try_from(session).map_err(|_| ApiError::INVALID_PARAMS)?,
        turn,
    ))
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
                resume: unsupported("the S1 fake route runs one turn per session"),
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

/// C1 §5 `failure`.
#[derive(Clone, Serialize)]
pub(crate) struct Failure {
    pub(crate) class: &'static str,
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
    /// Bounds the given event references per connection, in first-seen order.
    pub(crate) fn bounding<'a>(references: impl IntoIterator<Item = &'a RawRef>) -> Vec<Self> {
        let mut spans: Vec<Self> = Vec::new();
        for reference in references {
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
        spans
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
    /// No cancel exists on this route, so it is always `null`.
    pub(crate) cancel: Option<Value>,
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
#[expect(
    clippy::enum_variant_names,
    reason = "only turn events are committed until session and adapter events land"
)]
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
    },
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

    use super::{UNIX_EPOCH, rfc3339};

    #[test]
    fn rfc3339_formats_utc_milliseconds() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let leap_day = UNIX_EPOCH + Duration::from_millis(951_868_799_042);
        assert_eq!(rfc3339(leap_day), "2000-02-29T23:59:59.042Z");
        let new_year = UNIX_EPOCH + Duration::from_hours(499_656);
        assert_eq!(rfc3339(new_year), "2027-01-01T00:00:00.000Z");
    }
}
