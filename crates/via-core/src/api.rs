use std::{
    borrow::Cow,
    fmt,
    fs::File,
    io::Read,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use via_adapters::{Capabilities, VersionStatus};

use crate::{SessionId, TurnNumber, TurnState};

/// Strict C1 §3.2 parameters for creating a session and its first turn.
/// Free-form members are kept as their raw text (`Box<RawValue>`) and
/// inspected only by [`json_limits::shape`] and [`json_limits::string_list`]
/// (Task 4 design §10.2): no value is built from a peer's bytes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnParams {
    /// Selected harness; omitted, the one whose catalog lists `model`
    /// (adapter design §5.2).
    #[serde(default, deserialize_with = "given")]
    pub harness: Option<String>,
    /// Explicit model name.
    pub model: String,
    /// Inline prompt; exactly one of `prompt` and `prompt_file`.
    #[serde(default, deserialize_with = "given")]
    pub prompt: Option<String>,
    /// Absolute path of a prompt file, copied at receipt (design §10.4).
    #[serde(default, deserialize_with = "given")]
    pub prompt_file: Option<String>,
    /// Caller-owned 256-bit bearer handle.
    pub handle: String,
    /// C1 P4 retry key: the same key, handle and params replay the receipt.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Session working directory (design §11.1); the daemon's startup
    /// directory when omitted.
    #[serde(default, deserialize_with = "given")]
    pub cwd: Option<String>,
    /// Caller's session label, at most [`LABEL_MAX`] bytes.
    #[serde(default, deserialize_with = "given")]
    pub label: Option<String>,
    /// Immutable session policy (C1 P13).
    #[serde(default)]
    pub allow_untested: bool,
    #[serde(default, deserialize_with = "raw")]
    pub(crate) instructions: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    pub(crate) require: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    effort: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    bound: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    output_schema: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "nullable")]
    deadlines: Option<Nullable<DeadlineParams>>,
    #[serde(default, deserialize_with = "raw")]
    max_steps: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    vendor: Option<Box<RawValue>>,
}

/// Strict C1 §3.3 `resume` parameters. Session-scope members are accepted
/// only to be refused by name (C1 §4 `session_scope_on_resume`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeParams {
    /// Session to add a turn to.
    pub session: SessionId,
    /// Caller-owned bearer handle; absent, the mutation is `invalid_handle`
    /// like a wrong one (F15).
    #[serde(default)]
    pub handle: Option<String>,
    /// The new turn's inline prompt; exactly one of `prompt` and `prompt_file`.
    #[serde(default, deserialize_with = "given")]
    pub prompt: Option<String>,
    /// Absolute path of the new turn's prompt file (design §10.4).
    #[serde(default, deserialize_with = "given")]
    pub prompt_file: Option<String>,
    /// C1 §3 retry key: the same key and params replay the turn receipt.
    #[serde(default)]
    pub op_key: Option<String>,
    #[serde(default, deserialize_with = "raw")]
    effort: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    bound: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    output_schema: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "nullable")]
    deadlines: Option<Nullable<DeadlineParams>>,
    #[serde(default, deserialize_with = "raw")]
    max_steps: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    vendor: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    harness: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    model: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    allow_untested: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    instructions: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    cwd: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    require: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    label: Option<Box<RawValue>>,
}

/// C1 §4 `deadlines` as sent: `{wall_ms?, idle_ms?}`. A present member is
/// a number; a nested `null` is `invalid_params` (A9).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeadlineParams {
    #[serde(default, deserialize_with = "given")]
    wall_ms: Option<u64>,
    #[serde(default, deserialize_with = "given")]
    idle_ms: Option<u64>,
}

/// A present member's raw text, `null` included: `Some("null")` is kept
/// distinct from an omitted member.
fn raw<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(deserializer).map(Some)
}

/// A present member that is not typed "or null": `null` is refused.
fn given<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

/// A present typed member: an explicit `null`, or its value.
pub(crate) enum Nullable<T> {
    Null,
    Given(T),
}

/// Like [`raw`] for a typed member, keeping an explicit `null`.
fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Nullable<T>>, D::Error> {
    Ok(Some(match Option::<T>::deserialize(deserializer)? {
        None => Nullable::Null,
        Some(value) => Nullable::Given(value),
    }))
}

/// The C1 §4 per-turn parameters of one `spawn` or `resume`, as sent;
/// [`crate::intake`] decodes them.
pub(crate) struct PerTurn<'a> {
    pub(crate) effort: Option<&'a RawValue>,
    pub(crate) bound: Option<&'a RawValue>,
    pub(crate) output_schema: Option<&'a RawValue>,
    pub(crate) deadlines: Option<&'a Nullable<DeadlineParams>>,
    pub(crate) max_steps: Option<&'a RawValue>,
    pub(crate) vendor: Option<&'a RawValue>,
}

impl DeadlineParams {
    /// The budgets given; design §5: a budget of 0 would stop every turn
    /// at once, so it is refused by name.
    pub(crate) fn budgets(&self) -> Result<(Option<u64>, Option<u64>), ApiError> {
        if self.idle_ms == Some(0) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                Named::field("deadlines.idle_ms"),
                "deadlines.idle_ms must be at least 1",
            ));
        }
        if self.wall_ms == Some(0) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                Named::field("deadlines.wall_ms"),
                "deadlines.wall_ms must be at least 1",
            ));
        }
        Ok((self.wall_ms, self.idle_ms))
    }
}

/// Longest `label` (C1 §4), in bytes.
pub(crate) const LABEL_MAX: usize = 120;

/// Longest `bound`, encoded (Task 4 design §6.4).
const BOUND_MAX: usize = 32 * 1024;

/// Longest `vendor`, encoded (design §6.4).
const VENDOR_MAX: usize = 16 * 1024;

/// Longest `model` or `effort`, encoded (design §6.4).
const SHORT_MEMBER_MAX: usize = 1024;

/// Longest `cwd` or `prompt_file` path (design §10.4, §11.1), in bytes.
pub(crate) const PATH_MAX: usize = 4096;

/// Longest C1 request line, its line feed included (design §5.2, A39).
pub const REQUEST_LINE_MAX: usize = 1024 * 1024;

/// Where a turn's prompt comes from (C1 §4): exactly one of `prompt` and
/// `prompt_file`.
pub(crate) enum PromptSource {
    Inline(String),
    File(String),
}

/// Takes the one prompt source a request gave; both or neither is
/// `invalid_params`.
fn prompt_source(
    prompt: Option<String>,
    prompt_file: Option<String>,
) -> Result<PromptSource, ApiError> {
    match (prompt, prompt_file) {
        (Some(text), None) => Ok(PromptSource::Inline(text)),
        (None, Some(path)) => Ok(PromptSource::File(path)),
        _ => Err(ApiError::naming(
            ApiError::INVALID_PARAMS,
            Named::field("prompt"),
            "exactly one of prompt and prompt_file is required",
        )),
    }
}

impl SpawnParams {
    pub(crate) fn per_turn(&self) -> PerTurn<'_> {
        PerTurn {
            effort: self.effort.as_deref(),
            bound: self.bound.as_deref(),
            output_schema: self.output_schema.as_deref(),
            deadlines: self.deadlines.as_ref(),
            max_steps: self.max_steps.as_deref(),
            vendor: self.vendor.as_deref(),
        }
    }

    /// Takes the turn's prompt source out of the parameters.
    pub(crate) fn take_prompt(&mut self) -> Result<PromptSource, ApiError> {
        prompt_source(self.prompt.take(), self.prompt_file.take())
    }

    /// Design §11.1: checks the session members' sizes without I/O: the
    /// `model`, the per-turn members and `label` at most [`LABEL_MAX`]
    /// bytes. The route's rules are the plan's ([`crate::intake`]).
    pub(crate) fn check_session_members(&self) -> Result<(), ApiError> {
        // Design §6.4: the envelope's members a caller sizes, refused at
        // receipt over their maxima.
        if via_adapters::encoded_text_len(&self.model) + 2 > SHORT_MEMBER_MAX {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                Named::field("model"),
                "model is longer than 1 KiB encoded",
            ));
        }
        self.per_turn().check_sizes()?;
        if self
            .label
            .as_ref()
            .is_some_and(|label| label.len() > LABEL_MAX)
        {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                Named::field("label"),
                "label is longer than 120 bytes",
            ));
        }
        Ok(())
    }
}

impl ResumeParams {
    pub(crate) fn per_turn(&self) -> PerTurn<'_> {
        PerTurn {
            effort: self.effort.as_deref(),
            bound: self.bound.as_deref(),
            output_schema: self.output_schema.as_deref(),
            deadlines: self.deadlines.as_ref(),
            max_steps: self.max_steps.as_deref(),
            vendor: self.vendor.as_deref(),
        }
    }

    /// Takes the turn's prompt source out of the parameters.
    pub(crate) fn take_prompt(&mut self) -> Result<PromptSource, ApiError> {
        prompt_source(self.prompt.take(), self.prompt_file.take())
    }

    /// C1 §3.3/§4: session-scope parameters, `allow_untested` included, are
    /// fixed at spawn; any of them on `resume` is refused by name.
    pub(crate) fn refuse_session_scope(&self) -> Result<(), ApiError> {
        let members = [
            (
                &self.harness,
                Named::field("harness"),
                "harness is session scope; resume cannot set it",
            ),
            (
                &self.model,
                Named::field("model"),
                "model is session scope; resume cannot set it",
            ),
            (
                &self.allow_untested,
                Named::field("allow_untested"),
                "allow_untested is session scope; resume cannot set it",
            ),
            (
                &self.instructions,
                Named::field("instructions"),
                "instructions is session scope; resume cannot set it",
            ),
            (
                &self.cwd,
                Named::field("cwd"),
                "cwd is session scope; resume cannot set it",
            ),
            (
                &self.require,
                Named::field("require"),
                "require is spawn scope; resume cannot set it",
            ),
            (
                &self.label,
                Named::field("label"),
                "label is session scope; resume cannot set it",
            ),
        ];
        match members.into_iter().find(|(value, ..)| value.is_some()) {
            Some((_, named, message)) => Err(ApiError {
                kind2: Some("session_scope_on_resume"),
                ..ApiError::naming(ApiError::INVALID_PARAMS, named, message)
            }),
            None => Ok(()),
        }
    }
}

impl PerTurn<'_> {
    /// Design §6.4: a `bound` over 32 KiB, a `vendor` over 16 KiB or an
    /// `effort` over 1 KiB encoded is `invalid_params` naming the member,
    /// before any route rule.
    pub(crate) fn check_sizes(&self) -> Result<(), ApiError> {
        let members = [
            (
                self.bound,
                BOUND_MAX,
                Named::field("bound"),
                "bound is longer than 32 KiB encoded",
            ),
            (
                self.vendor,
                VENDOR_MAX,
                Named::field("vendor"),
                "vendor is longer than 16 KiB encoded",
            ),
            (
                self.effort,
                SHORT_MEMBER_MAX,
                Named::field("effort"),
                "effort is longer than 1 KiB encoded",
            ),
        ];
        match members
            .into_iter()
            .find(|(value, max, ..)| value.is_some_and(|value| value.get().len() > *max))
        {
            Some((_, _, named, message)) => {
                Err(ApiError::naming(ApiError::INVALID_PARAMS, named, message))
            }
            None => Ok(()),
        }
    }
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

/// Strict C1 §3.5 `cancel` parameters (design §3).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelParams {
    /// Session whose turn is cancelled.
    pub session: SessionId,
    /// Caller-owned bearer handle; absent, the mutation is `invalid_handle`
    /// like a wrong one (F15).
    #[serde(default)]
    pub handle: Option<String>,
    /// One-based turn; omitted, the running turn, else the latest one.
    #[serde(default)]
    pub turn: Option<u32>,
    /// Grace before a running turn is force-closed;
    /// [`DEFAULT_FORCE_AFTER_MS`] when absent.
    #[serde(default)]
    pub force_after_ms: Option<u64>,
    /// Reply only once the turn is terminal.
    #[serde(default)]
    pub wait: bool,
}

/// `cancel` grace when the caller gives no `force_after_ms` (C1 §3.5).
pub const DEFAULT_FORCE_AFTER_MS: u64 = 10_000;

/// C1 §3.6 `close` mode.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CloseMode {
    /// Running work gets until shortly before the deadline to stop.
    #[default]
    Graceful,
    /// Running work is force-closed at once.
    Force,
}

/// Strict C1 §3.6 `close` parameters (design §4).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseParams {
    /// Session to close.
    pub session: SessionId,
    /// Caller-owned bearer handle; absent, the mutation is `invalid_handle`
    /// like a wrong one (F15).
    #[serde(default)]
    pub handle: Option<String>,
    /// Graceful by default.
    #[serde(default)]
    pub mode: CloseMode,
    /// Close deadline from acceptance; [`DEFAULT_CLOSE_DEADLINE_MS`] when absent.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    /// C1 §3 retry key: the same key and params replay the close result.
    #[serde(default)]
    pub op_key: Option<String>,
}

/// `close` deadline when the caller gives no `deadline_ms` (design §4).
pub const DEFAULT_CLOSE_DEADLINE_MS: u64 = 10_000;

/// Strict C1 §3.4 `steer` parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteerParams {
    /// Session whose active turn is steered.
    pub session: SessionId,
    /// The input; never sent on a route that does not support steer.
    pub text: String,
    /// The turn the caller means; another active turn is `turn_mismatch`.
    #[serde(default)]
    pub expect_turn: Option<u32>,
    /// Caller-owned bearer handle; absent, the mutation is `invalid_handle`
    /// like a wrong one (F15).
    #[serde(default)]
    pub handle: Option<String>,
}

/// Strict C1 read-address parameter set.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    /// A canonical session or turn address.
    pub address: String,
}

/// Strict C1 §3.7 `status` parameters (Task 4 A26): the turn defaults to
/// the running turn, else the latest; `limit` is 1 to 1000, default 100.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusParams {
    /// The session described.
    pub session: SessionId,
    /// The selected turn.
    #[serde(default)]
    pub turn: Option<TurnNumber>,
    /// Step rows after this step.
    #[serde(default)]
    pub after_step: Option<u32>,
    /// At most this many step rows.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Strict C1 §3.12 `logs` parameters: exactly one of a session address or
/// a turn address (Task 4 design §4.4).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogsParams {
    /// A session: its running turn, else its latest submitted one.
    #[serde(default)]
    pub session: Option<SessionId>,
    /// A turn address `<session_id>/<turn>`.
    #[serde(default)]
    pub turn: Option<String>,
}

impl LogsParams {
    /// The session and, for a turn address, the turn.
    pub(crate) fn address(self) -> Result<(SessionId, Option<TurnNumber>), ApiError> {
        match (self.session, self.turn) {
            (Some(session), None) => Ok((session, None)),
            (None, Some(turn)) => match parse_address(&turn)? {
                (session, Some(number)) => Ok((session, Some(number))),
                (_, None) => Err(ApiError::INVALID_PARAMS),
            },
            (Some(_), Some(_)) | (None, None) => Err(ApiError::INVALID_PARAMS),
        }
    }
}

/// The durable event types (Task 4 design §2.1): the names `events`'
/// `types` filter accepts.
const EVENT_TYPES: [&str; 16] = [
    "session.opened",
    "session.reopened",
    "session.closed",
    "turn.queued",
    "turn.submitted",
    "turn.started",
    "turn.ended",
    "turn.revised",
    "cancel.requested",
    "cancel.settled",
    "steer.delivered",
    "action.denied",
    "vendor.request_declined",
    "process.exited",
    "server.lost",
    "warning",
];

/// An `events` `types` filter: the distinct durable types named, however
/// often each is repeated. Each name is matched as it is decoded, so a
/// long list builds no string; a name that is no durable type is
/// `invalid_params`.
pub(crate) struct EventTypes([bool; EVENT_TYPES.len()]);

impl EventTypes {
    /// The named types, in [`EVENT_TYPES`] order.
    pub(crate) fn names(&self) -> Vec<String> {
        EVENT_TYPES
            .iter()
            .zip(self.0)
            .filter(|(_, named)| *named)
            .map(|(name, _)| (*name).to_owned())
            .collect()
    }
}

impl<'de> Deserialize<'de> for EventTypes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// One name, matched without being kept.
        struct Known(usize);

        impl<'de> Deserialize<'de> for Known {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Name;
                impl serde::de::Visitor<'_> for Name {
                    type Value = Known;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a durable event type")
                    }

                    fn visit_str<E: serde::de::Error>(self, name: &str) -> Result<Known, E> {
                        EVENT_TYPES
                            .iter()
                            .position(|known| *known == name)
                            .map(Known)
                            .ok_or_else(|| E::custom("not a durable event type"))
                    }
                }
                deserializer.deserialize_str(Name)
            }
        }

        struct Names;
        impl<'de> serde::de::Visitor<'de> for Names {
            type Value = EventTypes;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a list of durable event types")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<EventTypes, A::Error> {
                let mut named = [false; EVENT_TYPES.len()];
                let mut any = false;
                while let Some(Known(index)) = seq.next_element()? {
                    named[index] = true;
                    any = true;
                }
                if !any {
                    return Err(serde::de::Error::custom("types names no event type"));
                }
                Ok(EventTypes(named))
            }
        }
        deserializer.deserialize_seq(Names)
    }
}

/// Strict C1 §3.11 `events` parameters (Task 4 design §4.3): exactly one of
/// a session or a turn address; `follow` is an unknown field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsParams {
    /// The session whose events are read.
    #[serde(default)]
    pub session: Option<SessionId>,
    /// A turn address `<session_id>/<turn>`: only that turn's events.
    #[serde(default)]
    pub turn: Option<String>,
    /// The page starts after this sequence; 0 by default.
    #[serde(default)]
    pub after: Option<u64>,
    /// Most events returned: 200 by default, 1 to 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    types: Option<EventTypes>,
}

impl EventsParams {
    /// The Store query the parameters name.
    pub(crate) fn query(self) -> Result<via_store::EventsQuery, ApiError> {
        let (session, turn) = LogsParams {
            session: self.session,
            turn: self.turn,
        }
        .address()?;
        let limit = self.limit.unwrap_or(200);
        if limit == 0 || limit > 1000 {
            return Err(ApiError::INVALID_PARAMS);
        }
        Ok(via_store::EventsQuery {
            session,
            turn,
            after: self.after.unwrap_or(0),
            limit,
            types: self
                .types
                .as_ref()
                .map(EventTypes::names)
                .unwrap_or_default(),
        })
    }
}

/// Strict C1 §3.10 `list` parameters (Task 4 design §4.5, §6.8).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    /// Only sessions in this state: `active`, `idle` or `closed`.
    #[serde(default)]
    pub state: Option<String>,
    /// Only sessions of this harness.
    #[serde(default)]
    pub harness: Option<String>,
    /// Only sessions with this label.
    #[serde(default)]
    pub label: Option<String>,
    /// Only sessions whose latest durable event is at or after this time.
    #[serde(default)]
    pub since: Option<String>,
    /// Most sessions returned: 50 by default, 1 to 200.
    #[serde(default)]
    pub limit: Option<u32>,
    /// The previous page's `next_cursor`, `l3.<ord>`.
    #[serde(default)]
    pub cursor: Option<String>,
}

impl ListParams {
    /// The Store query the parameters name; a cursor other than `l3.`
    /// and decimal digits at most `i64::MAX`, or a bad filter, is
    /// `invalid_params`.
    pub(crate) fn query(self) -> Result<via_store::ListQuery, ApiError> {
        let before = match self.cursor.as_deref() {
            None => None,
            Some(cursor) => {
                let digits = cursor.strip_prefix("l3.").ok_or(ApiError::INVALID_PARAMS)?;
                if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(ApiError::INVALID_PARAMS);
                }
                // Design §6.8: `ord` is an SQLite integer, a non-negative
                // i64; a larger value is malformed.
                let ord = digits
                    .parse::<i64>()
                    .map_err(|_| ApiError::INVALID_PARAMS)?;
                Some(ord.unsigned_abs())
            }
        };
        if self
            .state
            .as_deref()
            .is_some_and(|state| !matches!(state, "active" | "idle" | "closed"))
            || self
                .harness
                .as_ref()
                .is_some_and(|harness| harness.len() > SHORT_MEMBER_MAX)
            || self
                .label
                .as_ref()
                .is_some_and(|label| label.len() > LABEL_MAX)
        {
            return Err(ApiError::INVALID_PARAMS);
        }
        let since_ms = self
            .since
            .as_deref()
            .map(via_store::at_ms)
            .transpose()
            .map_err(|_| ApiError::INVALID_PARAMS)?;
        let limit = self.limit.unwrap_or(50);
        if limit == 0 || limit > 200 {
            return Err(ApiError::INVALID_PARAMS);
        }
        Ok(via_store::ListQuery {
            before,
            state: self.state,
            harness: self.harness,
            label: self.label,
            since_ms,
            limit,
        })
    }
}

/// Strict C1 `daemon/status` parameters; the method takes none.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStatusParams {}

/// Strict C1 §3.1 `describe` parameters (Task 4 design §4.6): at least one
/// of `harness` and `model`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescribeParams {
    #[serde(default)]
    pub(crate) harness: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) bound: Option<Box<RawValue>>,
    #[serde(default)]
    pub(crate) require: Option<Box<RawValue>>,
    #[serde(default)]
    pub(crate) vendor: Option<Box<RawValue>>,
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) allow_untested: bool,
}

/// Strict C1 §3.13 `models` parameters (Task 4 design §4.6).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsParams {
    #[serde(default)]
    pub(crate) harness: Option<String>,
}

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
    /// The refused request member and route, named in `data`.
    pub named: Option<Box<Named>>,
    /// Why a named member was refused (`data.reason`), such as a prompt
    /// file's (design §10.4).
    pub reason: Option<&'static str>,
    /// The free space and the floor of a `disk_free_floor` refusal
    /// (`data.free_bytes`, `data.floor_bytes`; Task 4 design §5.3).
    pub floor: Option<Box<FreeFloor>>,
}

/// A `disk_free_floor` refusal's numbers (Task 4 design §5.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreeFloor {
    /// Free space of the State directory's filesystem, in bytes.
    pub free_bytes: u64,
    /// The configured `disk.free_floor`, in bytes.
    pub floor_bytes: u64,
}

/// A refused request member (`data.field`) and, when a route's capabilities
/// refused it, that route (`data.route`).
#[derive(Clone, Copy, Debug)]
pub struct Named {
    /// Request member, dotted for a nested one.
    pub field: &'static str,
    /// Route whose capabilities refuse the member.
    pub route: Option<&'static str>,
}

impl Named {
    /// A member refused regardless of route.
    pub(crate) const fn field(field: &'static str) -> Self {
        Self { field, route: None }
    }

    /// A member `route`'s capabilities refuse, when a route was chosen.
    pub(crate) const fn route(field: &'static str, route: Option<&'static str>) -> Self {
        Self { field, route }
    }
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

    /// A failed Store read's C1 error: a full Public lane is
    /// [`Self::STORE_QUEUE_FULL`], which never latches; any other failure
    /// is `store_error` (design §6.1).
    pub(crate) fn read(error: &via_store::StoreError) -> Self {
        if matches!(error, via_store::StoreError::NotEnqueued) {
            Self::STORE_QUEUE_FULL
        } else {
            Self::STORE
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
        if let Some(named) = &self.named {
            data["field"] = json!(named.field);
            if let Some(route) = named.route {
                data["route"] = json!(route);
            }
        }
        if let Some(reason) = self.reason {
            data["reason"] = json!(reason);
        }
        if let Some(floor) = &self.floor {
            data["free_bytes"] = json!(floor.free_bytes);
            data["floor_bytes"] = json!(floor.floor_bytes);
        }
        if self.code == Self::REQUEST_TOO_LARGE.code {
            // Design §10.1: the named kind tells a program to use `prompt_file`.
            data["max_bytes"] = json!(REQUEST_LINE_MAX);
            data["use"] = json!("prompt_file");
        }
        data
    }

    /// Design §10.4 step 4: a prompt file refused by `reason`.
    pub(crate) fn prompt_file(reason: &'static str) -> Self {
        Self {
            kind2: Some("prompt_file"),
            reason: Some(reason),
            message: "prompt_file refused",
            ..Self::INVALID_PARAMS
        }
    }

    /// Design §10.1 (A39): a request line over [`REQUEST_LINE_MAX`] bytes;
    /// the connection closes after this reply.
    pub const REQUEST_TOO_LARGE: Self = Self {
        code: -32020,
        kind: "request_too_large",
        message: "request line over 1 MiB; pass a large prompt as prompt_file",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };

    /// C1 §8.1: a model the harness does not offer (Task 4 design §4.6).
    pub const UNKNOWN_MODEL: Self = Self {
        code: -32010,
        kind: "unknown_model",
        message: "unknown model",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };

    /// Design §5.3 (A42): free space on the State directory's filesystem is
    /// below `disk.free_floor`; new work is refused before any write.
    pub(crate) fn disk_free_floor(free_bytes: u64, floor_bytes: u64) -> Self {
        Self {
            message: "free disk space is below the floor",
            kind2: Some("disk_free_floor"),
            floor: Some(Box::new(FreeFloor {
                free_bytes,
                floor_bytes,
            })),
            ..Self::STORE_QUEUE_FULL
        }
    }

    /// Design §5.4 (A42): the WAL is at `wal.max`; the receipt was refused
    /// before `BEGIN`, known not committed.
    pub const WAL_FULL: Self = Self {
        message: "the Store WAL is at its limit; new work is refused",
        kind2: Some("wal_full"),
        ..Self::RECEIPT_NOT_COMMITTED
    };

    /// C1 §8.1: a `require`d capability the route does not meet.
    pub const MISSING_CAPABILITY: Self = Self {
        code: -32007,
        kind: "missing_capability",
        message: "a required capability is not met",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };

    /// A refusal naming the member (and route) in `named`.
    pub(crate) fn naming(base: Self, named: Named, message: &'static str) -> Self {
        Self {
            message,
            named: Some(Box::new(named)),
            ..base
        }
    }

    /// Invalid request fields or identifier format.
    pub const INVALID_PARAMS: Self = Self {
        code: -32602,
        kind: "invalid_params",
        message: "invalid parameters",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// A caller handle did not authorize a mutation.
    pub const INVALID_HANDLE: Self = Self {
        code: -32002,
        kind: "invalid_handle",
        message: "invalid session handle",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The selected route has no such control capability.
    pub const UNSUPPORTED_VERB: Self = Self {
        code: -32006,
        kind: "unsupported_verb",
        message: "verb is unsupported on this route",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The harness is unknown, not configured, or cannot run the session.
    pub const HARNESS_UNAVAILABLE: Self = Self {
        code: -32009,
        kind: "harness_unavailable",
        message: "harness is unavailable",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The daemon accepted a stop and admits no new work.
    pub const DAEMON_STOPPING: Self = Self {
        code: -32017,
        kind: "daemon_stopping",
        message: "the daemon is stopping",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// Active sessions refuse a plain stop (C1 §3.14).
    pub const SESSIONS_ACTIVE: Self = Self {
        code: -32012,
        kind: "admission_refused",
        message: "sessions are active",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The daemon already retains its bound of turns without a durable terminal.
    pub const TURNS_AT_CAPACITY: Self = Self {
        code: -32012,
        kind: "admission_refused",
        message: "too many unresolved turns",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// A C1 read found the Store's Public lane full (Task 4 design §6.1):
    /// nothing was read, and the Store did not fail.
    pub const STORE_QUEUE_FULL: Self = Self {
        code: -32012,
        kind: "admission_refused",
        message: "the Store read lane is full",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The Store cannot establish or read the required durable state.
    pub const STORE: Self = Self {
        code: -32018,
        kind: "store_error",
        message: "durable storage failed",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// A bound the route cannot enforce (C1 §4.2).
    pub const BOUND_UNSUPPORTED: Self = Self {
        code: -32008,
        kind: "bound_unsupported",
        message: "bound is unsupported on this route",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// `steer` found no active turn (C1 §3.4).
    pub const NO_ACTIVE_TURN: Self = Self {
        code: -32013,
        kind: "no_active_turn",
        message: "the session has no active turn",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// `steer`'s `expect_turn` names another turn than the active one.
    pub const TURN_MISMATCH: Self = Self {
        code: -32014,
        kind: "turn_mismatch",
        message: "the active turn is not the expected one",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// `steer` input the driver could not deliver: its control lane was
    /// full, or the input was not written whole (C2 §2).
    pub const STEER_NOT_DELIVERED: Self = Self {
        code: -32012,
        kind: "admission_refused",
        message: "the steer input was not delivered",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The turn has not yet ended.
    pub const TURN_NOT_FINISHED: Self = Self {
        code: -32015,
        kind: "turn_not_finished",
        message: "turn has not finished",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// A wait deadline elapsed while the turn remains active.
    pub const WAIT_TIMEOUT: Self = Self {
        code: -32016,
        kind: "wait_timeout",
        message: "wait timed out",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The requested session is absent.
    pub const SESSION_NOT_FOUND: Self = Self {
        code: -32003,
        kind: "session_not_found",
        message: "session does not exist",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The session is closed or closing.
    pub const SESSION_CLOSED: Self = Self {
        code: -32004,
        kind: "session_closed",
        message: "session is closed",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The session exists but has no such turn.
    pub const TURN_NOT_FOUND: Self = Self {
        code: -32005,
        kind: "turn_not_found",
        message: "turn does not exist",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The session already holds its bound of queued turns (C1 P6).
    pub const QUEUE_FULL: Self = Self {
        code: -32011,
        kind: "queue_full",
        message: "the session queue is full",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// The daemon already holds its bound of queued turns (runtime §8).
    pub const QUEUED_AT_CAPACITY: Self = Self {
        code: -32012,
        kind: "admission_refused",
        message: "too many queued turns",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// Store refused `Closed` twice while a turn of the closing session was
    /// still queued or running (design §4 dispatcher step 6).
    pub const CLOSE_REFUSED: Self = Self {
        code: -32012,
        kind: "admission_refused",
        message: "the session still has unfinished turns",
        unpersisted: None,
        kind2: None,
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
    };
    /// A retry key was reused with another handle or other params (C1 P4).
    pub const IDEMPOTENCY_CONFLICT: Self = Self {
        code: -32602,
        kind: "invalid_params",
        message: "retry key reused with different parameters",
        unpersisted: None,
        kind2: Some("idempotency_conflict"),
        commit_outcome: None,
        named: None,
        reason: None,
        floor: None,
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

/// Exact retry identity (C1 P4, runtime §6; Task 4 design §10.3): the
/// original params object's bytes with the top-level `handle` value
/// replaced by its hash and, for a prompt file, the top-level `prompt_file`
/// value replaced by `prompt_file`'s content token
/// `"sha256:<64 hex>:<len>"`. Every other byte, whitespace and member order
/// included, is kept, so only a byte-identical retry matches. The identity
/// is streamed: a running SHA-256 and length over the borrowed pieces
/// around those spans, never a copy. Duplicate top-level members are
/// refused.
pub fn retry_identity(
    raw_params: &str,
    handle_hash: &[u8; 32],
    prompt_file: Option<&str>,
) -> Result<via_store::Identity, ApiError> {
    use std::collections::HashSet;

    use serde::de::{Deserializer, MapAccess, Visitor};

    /// The byte ranges of the top-level `handle` and `prompt_file` values
    /// within the params text.
    #[derive(Default)]
    struct Spans {
        handle: Option<(usize, usize)>,
        prompt_file: Option<(usize, usize)>,
    }

    struct Members<'a>(&'a str);

    impl<'de> Visitor<'de> for Members<'de> {
        type Value = Spans;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a params object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Spans, A::Error> {
            let mut seen = HashSet::new();
            let mut spans = Spans::default();
            while let Some((key, value)) = map.next_entry::<String, &'de RawValue>()? {
                let start = (value.get().as_ptr() as usize)
                    .checked_sub(self.0.as_ptr() as usize)
                    .ok_or_else(|| serde::de::Error::custom("member outside params"))?;
                let span = Some((start, start + value.get().len()));
                match key.as_str() {
                    "handle" => spans.handle = span,
                    "prompt_file" => spans.prompt_file = span,
                    _ => {}
                }
                if !seen.insert(key) {
                    return Err(serde::de::Error::custom("duplicate params member"));
                }
            }
            Ok(spans)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(raw_params);
    let spans = deserializer
        .deserialize_map(Members(raw_params))
        .map_err(|_| ApiError::INVALID_PARAMS)?;
    deserializer.end().map_err(|_| ApiError::INVALID_PARAMS)?;
    let handle = spans.handle.ok_or(ApiError::INVALID_PARAMS)?;
    let mut hex = [0_u8; 66];
    hex[0] = b'"';
    hex[65] = b'"';
    for (index, byte) in handle_hash.iter().enumerate() {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        hex[1 + index * 2] = DIGITS[usize::from(byte >> 4)];
        hex[2 + index * 2] = DIGITS[usize::from(byte & 15)];
    }
    let content = prompt_file.map(|content| format!("\"{content}\""));
    let mut replaced = vec![(handle, &hex[..])];
    match (spans.prompt_file, &content) {
        (Some(span), Some(content)) => replaced.push((span, content.as_bytes())),
        (None, None) => {}
        _ => return Err(ApiError::INVALID_PARAMS),
    }
    replaced.sort_by_key(|((start, _), _)| *start);
    let bytes = raw_params.as_bytes();
    let mut hasher = Sha256::new();
    let mut len = 0_u64;
    let mut at = 0;
    for ((start, end), with) in replaced {
        for piece in [&bytes[at..start], with] {
            hasher.update(piece);
            len += piece.len() as u64;
        }
        at = end;
    }
    hasher.update(&bytes[at..]);
    len += (bytes.len() - at) as u64;
    Ok(via_store::Identity {
        len,
        sha256: hasher.finalize().into(),
    })
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

/// C1 §4 `deadlines.wall_ms` default (A3), for a turn that neither sets
/// nor inherits one.
pub(crate) const DEFAULT_WALL_MS: u64 = 3_600_000;
/// C1 §4 `deadlines.idle_ms` default (design §5).
pub(crate) const DEFAULT_IDLE_MS: u64 = 600_000;

/// The route and version fields shared by the receipt and the envelope
/// (C1 §3.2, §5; AD7, AD12).
#[derive(Clone, Serialize)]
pub(crate) struct PlanFields {
    pub(crate) route: String,
    pub(crate) adapter_version: String,
    pub(crate) vendor_version: Option<String>,
    pub(crate) version_status: VersionStatus,
}

impl PlanFields {
    /// C1 §5 `vendor_version_untested`, unless the version is `tested`.
    pub(crate) fn warning(&self) -> Option<Warning> {
        match self.version_status {
            VersionStatus::Tested => None,
            VersionStatus::Untested | VersionStatus::Refused if self.vendor_version.is_none() => {
                Some(Warning::VENDOR_VERSION_UNREPORTED)
            }
            VersionStatus::Untested | VersionStatus::Refused => {
                Warning::adapter("vendor_version_untested", None)
            }
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct Warning {
    code: &'static str,
    message: &'static str,
    /// C1 §5: a code's structured detail, where it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl Warning {
    /// A warning without `data`.
    pub(crate) const fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            data: None,
        }
    }

    /// This warning with `data`.
    #[cfg(any(test, feature = "test-failpoints"))]
    pub(crate) fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// This warning within its C1 §5 caps: `message` cut to 1 KiB encoded
    /// at a character boundary, `data` over 4 KiB encoded left out.
    pub(crate) fn capped(mut self) -> Self {
        self.message = cut_encoded(self.message, WARNING_MESSAGE_MAX - 2);
        if self
            .data
            .as_ref()
            .is_some_and(|data| !encodes_within(data, WARNING_DATA_MAX))
        {
            self.data = None;
        }
        self
    }

    /// The warning's stable code.
    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    /// C1 §3.5: a settled cancel whose group absence is unproved.
    pub(crate) const CANCEL_CLEANUP_UNCERTAIN: Self = Self::new(
        "cancel_cleanup_uncertain",
        "process group cleanup after cancellation is unconfirmed",
    );

    /// C1 §5: a turn with an `output_schema` ended with no structured output.
    pub(crate) const STRUCTURED_OUTPUT_MISSING: Self = Self::new(
        "structured_output_missing",
        "the vendor returned no structured output",
    );

    /// C1 §5, AD7: no instance reported the vendor's version.
    pub(crate) const VENDOR_VERSION_UNREPORTED: Self =
        Self::new("vendor_version_untested", "the vendor reported no version");

    /// C1 §5, AD6: the turn's usage ledger overflowed its keys, so the
    /// reported numbers cover an interval VIA did not verify.
    pub(crate) const USAGE_INTERVAL_UNVERIFIED: Self = Self::new(
        "usage_interval_unverified",
        "the reported usage covers an interval VIA could not verify",
    );

    /// An adapter-reported warning as the envelope's (C1 §5: adapter
    /// warnings "reach the envelope only as these codes"): a code of the
    /// closed list with VIA's own message and the adapter's `data`, which
    /// [`Self::capped`] bounds; `None` for any other code, which stays a
    /// `warning` event only.
    pub(crate) fn adapter(code: &str, data: Option<Value>) -> Option<Self> {
        let (code, message) = match code {
            "instructions_partial" => (
                "instructions_partial",
                "the vendor applied the turn's instructions only in part",
            ),
            "vendor_version_untested" => (
                "vendor_version_untested",
                "the vendor version is not one the adapter checked",
            ),
            "usage_interval_unverified" => (
                "usage_interval_unverified",
                Self::USAGE_INTERVAL_UNVERIFIED.message,
            ),
            "structured_output_missing" => (
                "structured_output_missing",
                "the vendor returned no structured output",
            ),
            "cancel_cleanup_uncertain" => (
                "cancel_cleanup_uncertain",
                Self::CANCEL_CLEANUP_UNCERTAIN.message,
            ),
            "predecessor_cleanup_uncertain" => (
                "predecessor_cleanup_uncertain",
                "cleanup of the session's previous process group is unconfirmed",
            ),
            "config_switch_unverified" => (
                "config_switch_unverified",
                "VIA could not apply or verify a requested inheritance setting",
            ),
            "deprecated" => (
                "deprecated",
                "the vendor reported a deprecated feature or setting",
            ),
            _ => return None,
        };
        Some(Self {
            code,
            message,
            data,
        })
    }
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
    pub(crate) plan: PlanFields,
    pub(crate) capabilities: Capabilities,
    /// C1 §3.2's five members ([`crate::intake::Effective::c1`]).
    pub(crate) effective: Value,
    pub(crate) warnings: Vec<Warning>,
}

/// C1 §3.3 turn receipt.
#[derive(Serialize)]
pub(crate) struct TurnReceipt {
    pub(crate) turn: String,
    pub(crate) state: &'static str,
    pub(crate) queue_position: u32,
    pub(crate) effective: Value,
    pub(crate) warnings: Vec<Warning>,
}

#[derive(Serialize)]
pub(crate) struct Requested<T> {
    pub(crate) requested: T,
    pub(crate) resolved: T,
}

/// C1 §5 `bound`: as requested, as the route enforces it, and whether
/// the turn inherited it.
#[derive(Serialize)]
pub(crate) struct Bound {
    pub(crate) requested: Option<Value>,
    pub(crate) effective: Option<Value>,
    pub(crate) inherited: bool,
}

/// C1 §8.2 `failure.class` values Core commits.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureClass {
    DeadlineWall,
    /// C1 §8.2: no meaningful progress within `idle_ms` (design §5).
    DeadlineIdle,
    SubmitFailed,
    /// C1 §8.2 (Q2): the structured output failed VIA's validation.
    StructuredOutputInvalid,
    /// C1 §8.2: the vendor returned a different or fresh session.
    ResumeMismatch,
    VendorError,
    /// C1 §8.2's specific vendor classes, from the adapter's class hint.
    RateLimit,
    Auth,
    ContextExceeded,
    BudgetExceeded,
    /// C1 §8.2: Host-confirmed death of a persistent server.
    ServerLost,
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
    /// C1 §5: an adapter-side `submit_failed`'s reason, and with
    /// `invalid_param` its field; never vendor text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<Value>,
}

/// C1 §5 `usage`: every count `null` while provenance is `unavailable`;
/// a route that reports only a total fills `total_tokens`.
#[derive(Serialize)]
pub(crate) struct Usage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    scope: Cow<'static, str>,
    provenance: &'static str,
}

/// One usage figure's components (AD6), each `None` when a contributing
/// sample lacked it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Tokens {
    pub(crate) input: Option<u64>,
    pub(crate) cached_input: Option<u64>,
    pub(crate) output: Option<u64>,
    pub(crate) reasoning_output: Option<u64>,
    pub(crate) total: Option<u64>,
}

impl Usage {
    pub(crate) const UNAVAILABLE: Self = Self {
        input_tokens: None,
        cached_input_tokens: None,
        output_tokens: None,
        reasoning_output_tokens: None,
        total_tokens: None,
        scope: Cow::Borrowed("turn"),
        provenance: "unavailable",
    };

    /// The turn's reported figure (AD6) under the route's declared `scope`
    /// (C1 §4.1 `usage.tokens`), or `vendor_interval` once its ledger
    /// overflowed; unavailable without a sample.
    pub(crate) fn reported(tokens: Option<Tokens>, interval: bool, scope: &str) -> Self {
        match tokens {
            Some(tokens) => Self {
                input_tokens: tokens.input,
                cached_input_tokens: tokens.cached_input,
                output_tokens: tokens.output,
                reasoning_output_tokens: tokens.reasoning_output,
                total_tokens: tokens.total,
                scope: if interval {
                    Cow::Borrowed("vendor_interval")
                } else {
                    Cow::Owned(scope.to_owned())
                },
                provenance: "reported",
            },
            None => Self::UNAVAILABLE,
        }
    }
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

    /// A vendor-reported cost (AD6) under one of C1 §5's scopes; a scope
    /// C1 does not define, or an amount that is not a finite number, is
    /// unavailable.
    pub(crate) fn reported(usd: f64, scope: &str) -> Self {
        let scope = match scope {
            "turn" => "turn",
            "session_cumulative" => "session_cumulative",
            "vendor_interval" => "vendor_interval",
            _ => return Self::UNAVAILABLE,
        };
        if !usd.is_finite() {
            return Self::UNAVAILABLE;
        }
        Self {
            usd: Some(usd),
            scope,
            provenance: "reported",
        }
    }
}

#[derive(Serialize)]
#[expect(clippy::struct_field_names, reason = "C1 §5 fixes these wire names")]
pub(crate) struct Timestamps {
    pub(crate) queued_at: String,
    pub(crate) submitted_at: Option<String>,
    pub(crate) accepted_at: Option<String>,
    pub(crate) ended_at: String,
}

#[derive(Clone, Serialize)]
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

/// C1 §5 `evidence`: the turn's evidence folder and the vendor's transcript
/// hint, as `logs` returns them (§3.12).
#[derive(Clone, Serialize)]
pub(crate) struct EvidenceRef {
    pub(crate) folder: Option<String>,
    pub(crate) transcript: Option<String>,
}

/// C1 §5 `vendor`: the vendor turn ID and the members of the terminal's
/// bounded vendor data object (AD6).
#[derive(Serialize)]
pub(crate) struct VendorFields {
    pub(crate) turn_id: Option<String>,
    #[serde(flatten)]
    pub(crate) data: serde_json::Map<String, Value>,
}

/// Longest inline `final_text`, encoded with its quotes (Task 4 design
/// §6.4): a longer text goes to `final_text.txt`.
pub(crate) const FINAL_TEXT_INLINE: usize = 256 * 1024;

/// Longest `failure.message`, encoded (design §6.4).
const FAILURE_MESSAGE_MAX: usize = 2 * 1024;

/// Entries an envelope list keeps (design §6.4).
const LIST_KEPT: usize = 1000;

/// Longest envelope list entry, encoded (design §6.4).
const ENTRY_MAX: usize = 256;

/// The longest prefix of `text` whose escaped JSON encoding, quotes
/// excluded, is at most `max` bytes: cut at a character boundary.
pub(crate) fn cut_encoded(text: &str, max: usize) -> &str {
    let mut used = 0;
    for (at, character) in text.char_indices() {
        let mut buffer = [0_u8; 4];
        used += via_adapters::encoded_text_len(character.encode_utf8(&mut buffer));
        if used > max {
            return &text[..at];
        }
    }
    text
}

/// A `failure.message` cut to 2 KiB encoded, its two quotes included, at
/// a character boundary.
pub(crate) fn failure_message(mut message: String) -> String {
    let kept = cut_encoded(&message, FAILURE_MESSAGE_MAX - 2).len();
    message.truncate(kept);
    message
}

/// C1 §5 `final_text_file`: the durable `final_text.txt` holding a final
/// text longer than [`FINAL_TEXT_INLINE`] (design §6.4).
#[derive(Clone, Serialize)]
pub(crate) struct FinalTextFile {
    pub(crate) path: String,
    pub(crate) bytes: u64,
    pub(crate) truncated: bool,
}

/// Longest inline `structured_output`, encoded (C1 §5): a larger value
/// goes to `structured_output.json`.
pub(crate) const STRUCTURED_OUTPUT_INLINE: usize = 32 * 1024;

/// C1 §5 `structured_output_file`: the durable `structured_output.json`
/// holding a structured output larger than [`STRUCTURED_OUTPUT_INLINE`].
#[derive(Clone, Serialize)]
pub(crate) struct StructuredOutputFile {
    pub(crate) path: String,
    pub(crate) bytes: u64,
}

/// Longest warning `message`, encoded with its quotes (C1 §5).
const WARNING_MESSAGE_MAX: usize = 1024;

/// Longest warning `data`, encoded (C1 §5).
const WARNING_DATA_MAX: usize = 4 * 1024;

/// Longest evidence `transcript` hint, encoded with its quotes (C1 §5);
/// a longer one is `null`.
pub(crate) const TRANSCRIPT_MAX: usize = 4 * 1024;

/// Whether `value`'s encoding is at most `max` bytes.
pub(crate) fn encodes_within(value: &impl Serialize, max: usize) -> bool {
    serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= max)
}

/// C1 §5 `denied_actions` entry: an action the vendor's own bound denied.
#[derive(Clone, Serialize)]
pub(crate) struct DeniedAction {
    kind: &'static str,
    target: String,
    reason: String,
    at: String,
    event_seq: u64,
}

impl DeniedAction {
    /// The entry citing the committed `action.denied` at `event_seq`.
    pub(crate) fn new(
        (kind, target, reason): (&'static str, String, String),
        at: String,
        event_seq: u64,
    ) -> Self {
        Self {
            kind,
            target,
            reason,
            at,
            event_seq,
        }
    }
}

/// C1 §5 `auto_declined_requests` entry: a vendor request VIA declined.
#[derive(Clone, Serialize)]
pub(crate) struct AutoDeclined {
    vendor_method: String,
    summary: String,
    blocking: bool,
    at: String,
    event_seq: u64,
}

impl AutoDeclined {
    /// The entry citing the committed `vendor.request_declined` at
    /// `event_seq`.
    pub(crate) fn new(
        (vendor_method, summary, blocking): (String, String, bool),
        at: String,
        event_seq: u64,
    ) -> Self {
        Self {
            vendor_method,
            summary,
            blocking,
            at,
            event_seq,
        }
    }
}

/// An envelope list entry whose two free strings are cut to fit
/// [`ENTRY_MAX`]; the event it cites keeps the full payload.
pub(crate) trait ListEntry: Serialize {
    fn free(&mut self) -> (&mut String, &mut String);
}

impl ListEntry for DeniedAction {
    fn free(&mut self) -> (&mut String, &mut String) {
        (&mut self.target, &mut self.reason)
    }
}

impl ListEntry for AutoDeclined {
    fn free(&mut self) -> (&mut String, &mut String) {
        (&mut self.vendor_method, &mut self.summary)
    }
}

/// One envelope list (design §6.4): the first 1,000 entries, each at most
/// 256 bytes encoded, and the count of all.
#[derive(Clone)]
pub(crate) struct Kept<T> {
    entries: Vec<T>,
    total: u64,
}

impl<T> Default for Kept<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            total: 0,
        }
    }
}

impl<T: ListEntry> Kept<T> {
    /// Counts `entry` and keeps it, cut to fit, while fewer than 1,000 are
    /// kept.
    pub(crate) fn push(&mut self, mut entry: T) {
        self.total += 1;
        if self.entries.len() >= LIST_KEPT {
            return;
        }
        let (first, second) = entry.free();
        let (first_text, second_text) = (std::mem::take(first), std::mem::take(second));
        let fixed = serde_json::to_vec(&entry).map_or(ENTRY_MAX, |bytes| bytes.len());
        let room = ENTRY_MAX.saturating_sub(fixed);
        let first_kept = cut_encoded(&first_text, room / 2);
        let first_used = via_adapters::encoded_text_len(first_kept);
        let second_kept = cut_encoded(&second_text, room - first_used);
        let (first, second) = entry.free();
        first_kept.clone_into(first);
        second_kept.clone_into(second);
        self.entries.push(entry);
    }

    /// The kept entries and the total.
    pub(crate) fn into_parts(self) -> (Vec<T>, u64) {
        (self.entries, self.total)
    }
}

/// Test builds only: members at their design §6.4 maxima, for
/// [`crate::envelope_at_maximum`].
#[cfg(feature = "test-failpoints")]
pub(crate) mod maxima {
    use serde_json::{Value, json};

    use super::{AutoDeclined, Bound, DeniedAction, Warning};

    /// An object whose encoding is `bytes` long.
    pub(crate) fn object_of(bytes: usize) -> Value {
        json!({"pad":"p".repeat(bytes - r#"{"pad":""}"#.len())})
    }

    /// `bound` with both sides at 32 KiB encoded.
    pub(crate) fn bound() -> Bound {
        Bound {
            requested: Some(object_of(32 * 1024)),
            effective: Some(object_of(32 * 1024)),
            inherited: false,
        }
    }

    /// C1 §5's closed list of warning codes.
    pub(crate) const WARNING_CODES: [&str; 8] = [
        "instructions_partial",
        "vendor_version_untested",
        "usage_interval_unverified",
        "structured_output_missing",
        "cancel_cleanup_uncertain",
        "predecessor_cleanup_uncertain",
        "config_switch_unverified",
        "deprecated",
    ];

    /// A string whose encoding, quotes included, is `bytes` long, made of
    /// escaped control characters as far as they fit.
    pub(crate) fn escaped(bytes: usize) -> String {
        let room = bytes - 2;
        let mut text = "\u{1}".repeat(room / 6);
        text.push_str(&"a".repeat(room % 6));
        text
    }

    /// A warning whose `message` is 1 KiB and `data` 4 KiB encoded, both
    /// escaped; its message lives for the process.
    pub(crate) fn warning(code: &'static str) -> Warning {
        let data = json!({"pad": escaped(4 * 1024 - r#"{"pad":}"#.len())});
        Warning::new(code, Box::leak(escaped(1024).into_boxed_str())).with_data(data)
    }

    /// `leftovers` with 16 processes, each `comm` 15 escaped bytes.
    pub(crate) fn leftovers(at: &str) -> Value {
        let process = json!({"pid": i32::MAX, "comm": "\u{1}".repeat(15), "started_at": at});
        json!({"scope": "server", "processes": vec![process; 16], "total": u64::MAX,
               "incomplete": true, "best_effort": true})
    }

    /// A denial whose free strings are `bytes` long each.
    pub(crate) fn denied(bytes: usize, at: &str, event_seq: u64) -> DeniedAction {
        DeniedAction {
            kind: "file_write",
            target: "\u{1}".repeat(bytes),
            reason: "r".repeat(bytes),
            at: at.to_owned(),
            event_seq,
        }
    }

    /// A decline whose free strings are `bytes` long each.
    pub(crate) fn declined(bytes: usize, at: &str, event_seq: u64) -> AutoDeclined {
        AutoDeclined {
            vendor_method: "m".repeat(bytes),
            summary: "\u{e9}".repeat(bytes / 2),
            blocking: true,
            at: at.to_owned(),
            event_seq,
        }
    }
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
    pub(crate) harness: String,
    pub(crate) model: Requested<String>,
    pub(crate) effort: Requested<Option<String>>,
    #[serde(flatten)]
    pub(crate) plan: PlanFields,
    pub(crate) vendor_session_id: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) bound: Bound,
    /// Inline up to [`FINAL_TEXT_INLINE`] encoded; `null` when the text is
    /// in `final_text_file`.
    pub(crate) final_text: Option<String>,
    pub(crate) final_text_file: Option<FinalTextFile>,
    /// Inline up to [`STRUCTURED_OUTPUT_INLINE`] encoded; `null` when the
    /// value is in `structured_output_file`.
    pub(crate) structured_output: Option<Value>,
    pub(crate) structured_output_file: Option<StructuredOutputFile>,
    /// C1 §5 (H5): always present; S-LEFTOVER owns the report, so `null`.
    pub(crate) leftovers: Option<Value>,
    pub(crate) denied_actions: Vec<DeniedAction>,
    pub(crate) auto_declined_requests: Vec<AutoDeclined>,
    pub(crate) denied_actions_total: u64,
    pub(crate) auto_declined_requests_total: u64,
    pub(crate) steps: Option<u64>,
    pub(crate) usage: Usage,
    pub(crate) cost: Cost,
    pub(crate) timestamps: Timestamps,
    /// Milliseconds from `submitted_at` to `ended_at`; `null` without a submission.
    pub(crate) duration_ms: Option<u64>,
    pub(crate) exit: Option<Exit>,
    pub(crate) events: EventRange,
    pub(crate) evidence: EvidenceRef,
    pub(crate) vendor_options: Value,
    pub(crate) warnings: Vec<Warning>,
    pub(crate) vendor: VendorFields,
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
    TurnStarted { effective: Value },
    #[serde(rename = "turn.ended")]
    TurnEnded {
        state: &'static str,
        failure: Option<Failure>,
        stop_reason: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        cancel: Option<Cancel>,
    },
    #[serde(rename = "action.denied")]
    ActionDenied {
        kind: &'static str,
        target: String,
        reason: String,
    },
    #[serde(rename = "vendor.request_declined")]
    RequestDeclined {
        vendor_method: String,
        summary: String,
        blocking: bool,
    },
    #[serde(rename = "cancel.requested")]
    CancelRequested {},
    #[serde(rename = "cancel.settled")]
    CancelSettled {
        outcome: &'static str,
        cleanup: &'static str,
    },
    #[serde(rename = "session.closed")]
    SessionClosed { reason: &'static str },
    /// C1 §3.4, §6.1: steer input the vendor took into the active turn.
    #[serde(rename = "steer.delivered")]
    SteerDelivered { delivery: String },
    /// C1 §6.1, C2 §2: the session's first confirmed connection
    /// generation. Its transcript hint goes to the session's columns.
    #[serde(rename = "session.opened")]
    SessionOpened {
        route: String,
        vendor_session_id: String,
        vendor_version: Option<String>,
    },
    /// C1 §6.1: an adapter-reported warning, within C1 §5's caps.
    #[serde(rename = "warning")]
    Warning {
        code: &'static str,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// C1 §6.1, C2 §2: a later confirmed connection generation.
    #[serde(rename = "session.reopened")]
    SessionReopened {
        route: String,
        vendor_session_id: String,
        vendor_version: Option<String>,
        reason: &'static str,
    },
}

impl EventBody {
    /// A `warning` event within C1 §5's caps: `message` cut to 1 KiB
    /// encoded at a character boundary, `data` over 4 KiB encoded left out.
    pub(crate) fn warning(code: &'static str, message: &str, data: Option<Value>) -> Self {
        Self::Warning {
            code,
            message: cut_encoded(message, WARNING_MESSAGE_MAX - 2).to_owned(),
            data: data.filter(|data| encodes_within(data, WARNING_DATA_MAX)),
        }
    }
}

/// C1 §6.1 event with every common field.
#[derive(Serialize)]
pub(crate) struct Event<'a> {
    pub(crate) seq: u64,
    pub(crate) session_id: &'a SessionId,
    pub(crate) turn: Option<u32>,
    pub(crate) late: bool,
    pub(crate) at: &'a str,
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

    use super::{EventBody, UNIX_EPOCH, retry_identity, retry_key, rfc3339};

    #[test]
    fn durable_events_use_c1_tags_and_fields() {
        let bodies = [
            (
                EventBody::CancelSettled {
                    outcome: "acknowledged",
                    cleanup: "quiescent",
                },
                json!({"type":"cancel.settled","outcome":"acknowledged","cleanup":"quiescent"}),
            ),
            (
                EventBody::SessionClosed { reason: "closed" },
                json!({"type":"session.closed","reason":"closed"}),
            ),
        ];
        for (body, expected) in bodies {
            assert_eq!(serde_json::to_value(body).unwrap(), expected);
        }
    }

    /// C1 P4 / runtime §6: identity is the params bytes with only the handle
    /// value replaced by its hash; whitespace and member order are kept.
    /// Design §10.3: it is streamed over the borrowed pieces, and equals
    /// the length and SHA-256 of those bytes.
    #[test]
    fn retry_identity_keeps_every_byte_but_the_handle() {
        let hash = [0xab; 32];
        let raw = r#"{"prompt": "p","handle":"h_secret" ,"model":"fake"}"#;
        let expected = format!(
            r#"{{"prompt": "p","handle":"{}" ,"model":"fake"}}"#,
            "ab".repeat(32)
        );
        let identity = retry_identity(raw, &hash, None).unwrap();
        assert_eq!(identity, via_store::Identity::of(expected.as_bytes()));
        let respaced = r#"{"prompt":"p","handle":"h_secret" ,"model":"fake"}"#;
        assert_ne!(retry_identity(respaced, &hash, None).unwrap(), identity);
        assert_ne!(retry_identity(raw, &[0; 32], None).unwrap(), identity);
    }

    /// Design §10.3 [t4r18.1]: a prompt file contributes its content's
    /// `"sha256:<hex>:<len>"` in place of its path, wherever the members
    /// stand; every other byte is kept.
    #[test]
    fn retry_identity_puts_the_prompt_file_content_in_place_of_its_path() {
        let hash = [0x01; 32];
        let content = format!("sha256:{}:3", "cd".repeat(32));
        for raw in [
            r#"{"prompt_file":"/tmp/p.txt","handle":"h_x","model":"fake"}"#,
            r#"{"model":"fake", "handle" : "h_x","prompt_file":  "/tmp/p.txt" }"#,
        ] {
            let expected = raw
                .replace("h_x", &"01".repeat(32))
                .replace("/tmp/p.txt", &content);
            assert_eq!(
                retry_identity(raw, &hash, Some(&content)).unwrap(),
                via_store::Identity::of(expected.as_bytes()),
                "{raw}"
            );
        }
        let one = r#"{"prompt_file":"/a","handle":"h"}"#;
        let other = r#"{"prompt_file":"/b","handle":"h"}"#;
        assert_eq!(
            retry_identity(one, &hash, Some(&content)).unwrap(),
            retry_identity(other, &hash, Some(&content)).unwrap(),
            "the path is not part of the identity"
        );
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
                retry_identity(raw, &hash, None).unwrap_err().kind,
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

    /// Design §6.4: `failure.message` is at most 2 KiB encoded, its quotes
    /// included; an ASCII message or one with escapes stops at the bound.
    #[test]
    fn failure_message_encodes_within_two_kib_with_its_quotes() {
        for message in ["a".repeat(4096), "\"".repeat(2048), "é".repeat(2048)] {
            let kept = super::failure_message(message);
            let encoded = serde_json::to_string(&kept).map_or(usize::MAX, |text| text.len());
            assert!(encoded <= 2048, "{encoded} bytes encoded");
            assert!(encoded + 6 > 2048, "{encoded} bytes encoded: cut too far");
        }
    }
}
