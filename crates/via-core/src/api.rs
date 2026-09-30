use std::{
    fmt,
    fs::File,
    io::Read,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use via_store::json_limits::{self, Shape};

use crate::{SessionId, TurnNumber, TurnState};

/// Strict C1 §3.2 parameters for creating a fake session and its first turn.
/// Free-form members are kept as their raw text (`Box<RawValue>`) and
/// inspected only by [`json_limits::shape`] and [`json_limits::string_list`]
/// (Task 4 design §10.2): no value is built from a peer's bytes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnParams {
    /// Selected harness, currently `fake` in S1.
    pub harness: String,
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
    /// Session working directory (design §11.1); the fake's default when omitted.
    #[serde(default, deserialize_with = "given")]
    pub cwd: Option<String>,
    /// Caller's session label, at most [`LABEL_MAX`] bytes.
    #[serde(default, deserialize_with = "given")]
    pub label: Option<String>,
    /// Immutable session policy (C1 P13).
    #[serde(default)]
    pub allow_untested: bool,
    #[serde(default, deserialize_with = "raw")]
    instructions: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "raw")]
    require: Option<Box<RawValue>>,
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
    /// Caller-owned bearer handle.
    pub handle: String,
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
struct DeadlineParams {
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
enum Nullable<T> {
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

/// The C1 §4 per-turn parameters of one `spawn` or `resume`, as sent.
pub(crate) struct PerTurn<'a> {
    effort: Option<&'a RawValue>,
    bound: Option<&'a RawValue>,
    output_schema: Option<&'a RawValue>,
    deadlines: Option<&'a Nullable<DeadlineParams>>,
    max_steps: Option<&'a RawValue>,
    vendor: Option<&'a RawValue>,
}

/// What a turn sets for itself on the fake route; anything else inherits.
pub(crate) struct Overrides {
    wall_ms: Option<u64>,
    idle_ms: Option<u64>,
}

/// Longest `label` (C1 §4), in bytes.
pub(crate) const LABEL_MAX: usize = 120;

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
            &const { Named::field("prompt") },
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

    /// Design §11.1: checks the session members without I/O: `label` at
    /// most [`LABEL_MAX`] bytes, `instructions` refused by the fake, and
    /// each `require`d verb met by [`Capabilities::fake`], the first unmet
    /// one refused by name.
    pub(crate) fn check_session_members(&self) -> Result<(), ApiError> {
        if self
            .label
            .as_ref()
            .is_some_and(|label| label.len() > LABEL_MAX)
        {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::field("label") },
                "label is longer than 120 bytes",
            ));
        }
        if self.instructions.is_some() {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::fake("instructions") },
                "instructions is unsupported on route fake",
            ));
        }
        if let Some(require) = &self.require {
            Capabilities::fake().require(require)?;
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
                &const { Named::field("harness") },
                "harness is session scope; resume cannot set it",
            ),
            (
                &self.model,
                &const { Named::field("model") },
                "model is session scope; resume cannot set it",
            ),
            (
                &self.allow_untested,
                &const { Named::field("allow_untested") },
                "allow_untested is session scope; resume cannot set it",
            ),
            (
                &self.instructions,
                &const { Named::field("instructions") },
                "instructions is session scope; resume cannot set it",
            ),
            (
                &self.cwd,
                &const { Named::field("cwd") },
                "cwd is session scope; resume cannot set it",
            ),
            (
                &self.require,
                &const { Named::field("require") },
                "require is spawn scope; resume cannot set it",
            ),
            (
                &self.label,
                &const { Named::field("label") },
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
    /// Validates the values against the fake route's capabilities
    /// ([`Capabilities::fake`]). Omitted values inherit. C1 §1 lets only
    /// members typed "or null" be null: a null `output_schema` or
    /// `max_steps` is accepted (`output_schema: null` clears, which on this
    /// route is already the state); a null `effort`, `bound` or `deadlines`,
    /// or a nested null in `deadlines` (A9), is `invalid_params`.
    pub(crate) fn fake_overrides(&self) -> Result<Overrides, ApiError> {
        let given = |value: Option<&RawValue>| {
            value.is_some_and(|value| json_limits::shape(value.get()) != Shape::Null)
        };
        let null = |value: Option<&RawValue>| {
            value.is_some_and(|value| json_limits::shape(value.get()) == Shape::Null)
        };
        if null(self.effort) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::fake("effort") },
                "effort cannot be null",
            ));
        }
        if given(self.effort) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::fake("effort") },
                "effort is unsupported on route fake",
            ));
        }
        if given(self.output_schema) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::fake("output_schema") },
                "output_schema is unsupported on route fake",
            ));
        }
        if given(self.max_steps) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::fake("max_steps") },
                "max_steps is unsupported on route fake",
            ));
        }
        if null(self.bound) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::fake("bound") },
                "bound cannot be null",
            ));
        }
        // The fake route declares no bounds, so any bound is unenforceable.
        if self.bound.is_some() {
            return Err(ApiError::naming(
                ApiError::BOUND_UNSUPPORTED,
                &const { Named::fake("bound") },
                "bound is unsupported on route fake, which declares no bounds",
            ));
        }
        // Only `{}` or empty per-harness objects: the fake declares no options.
        if self.vendor.is_some_and(|vendor| !no_vendor_options(vendor)) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::fake("vendor") },
                "vendor options are unsupported on route fake",
            ));
        }
        let deadlines = match self.deadlines {
            None => {
                return Ok(Overrides {
                    wall_ms: None,
                    idle_ms: None,
                });
            }
            Some(Nullable::Null) => {
                return Err(ApiError::naming(
                    ApiError::INVALID_PARAMS,
                    &const { Named::fake("deadlines") },
                    "deadlines cannot be null",
                ));
            }
            Some(Nullable::Given(deadlines)) => deadlines,
        };
        // Design §5: an idle deadline of 0 would stop every turn at once.
        if deadlines.idle_ms == Some(0) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::field("deadlines.idle_ms") },
                "deadlines.idle_ms must be at least 1",
            ));
        }
        if deadlines.wall_ms == Some(0) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::field("deadlines.wall_ms") },
                "deadlines.wall_ms must be at least 1",
            ));
        }
        Ok(Overrides {
            wall_ms: deadlines.wall_ms,
            idle_ms: deadlines.idle_ms,
        })
    }
}

/// Whether `vendor` is an object whose every member is an empty object:
/// options for no harness. Each member is inspected by its shape only.
fn no_vendor_options(vendor: &RawValue) -> bool {
    match json_limits::shape(vendor.get()) {
        Shape::Object { empty: true } => true,
        Shape::Object { empty: false } => {
            serde_json::from_str::<std::collections::BTreeMap<String, &RawValue>>(vendor.get())
                .is_ok_and(|options| {
                    options.values().all(|harness| {
                        json_limits::shape(harness.get()) == (Shape::Object { empty: true })
                    })
                })
        }
        Shape::Null | Shape::Bool | Shape::Number | Shape::String | Shape::Array { .. } => false,
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
    /// Caller-owned bearer handle.
    pub handle: String,
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
    /// Caller-owned bearer handle.
    pub handle: String,
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

/// Strict C1 session-address parameters for the current `events`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionReadParams {
    /// Session whose durable history is read.
    pub session: SessionId,
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
    harness: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    bound: Option<Box<RawValue>>,
    #[serde(default)]
    require: Option<Box<RawValue>>,
    #[serde(default)]
    vendor: Option<Box<RawValue>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    #[expect(
        dead_code,
        reason = "the fake route is untested either way; it changes no refusal"
    )]
    allow_untested: bool,
}

impl DescribeParams {
    /// The fake route's plan (C1 §3.1), from [`Capabilities::fake`] with no
    /// process and no write: a model other than `fake` is `unknown_model`,
    /// a harness other than `fake` (or no fake agent) `harness_unavailable`.
    /// What the route cannot do for the given `bound`, `vendor` or
    /// `require` is listed in `refusals`, each by member and kind.
    pub(crate) fn describe(&self, fake_available: bool) -> Result<Value, ApiError> {
        if self.harness.is_none() && self.model.is_none() {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::field("model") },
                "describe takes a harness or a model",
            ));
        }
        if self
            .harness
            .as_deref()
            .is_some_and(|harness| harness != "fake")
            || !fake_available
        {
            return Err(ApiError::HARNESS_UNAVAILABLE);
        }
        if self.model.as_deref().is_some_and(|model| model != "fake") {
            return Err(ApiError::UNKNOWN_MODEL);
        }
        if self
            .cwd
            .as_deref()
            .is_some_and(|cwd| cwd.len() > PATH_MAX || !std::path::Path::new(cwd).is_absolute())
        {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::field("cwd") },
                "cwd must be an absolute path of at most 4096 bytes",
            ));
        }
        let capabilities = Capabilities::fake();
        let mut refusals = Vec::new();
        let mut refuse = |error: ApiError| match error.named {
            Some(named) if named.route.is_some() => {
                refusals.push(json!({"field":named.field,"kind":error.kind,
                    "message":error.message}));
                Ok(())
            }
            _ => Err(error),
        };
        let per_turn = PerTurn {
            effort: None,
            bound: self.bound.as_deref(),
            output_schema: None,
            deadlines: None,
            max_steps: None,
            vendor: self.vendor.as_deref(),
        };
        if let Err(error) = per_turn.fake_overrides() {
            refuse(error)?;
        }
        if let Some(require) = &self.require
            && let Err(error) = capabilities.require(require)
        {
            refuse(error)?;
        }
        let plan = RoutePlan::fake();
        let mut described = json!({"harness":"fake",
            "model":{"requested":self.model,"resolved":"fake"},
            "capabilities":capabilities,"effective_bound":null,
            "refusals":refusals,"warnings":plan.warnings()});
        if let (Some(described), Value::Object(plan)) = (described.as_object_mut(), json!(plan)) {
            described.extend(plan);
        }
        Ok(described)
    }
}

/// Strict C1 §3.13 `models` parameters (Task 4 design §4.6).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsParams {
    #[serde(default)]
    harness: Option<String>,
}

impl ModelsParams {
    /// The fake's one model; none for another harness.
    pub(crate) fn models(&self) -> Value {
        let models = if self
            .harness
            .as_deref()
            .is_none_or(|harness| harness == "fake")
        {
            json!([{"model":"fake","harness":"fake","aliases":[],"source":"builtin"}])
        } else {
            json!([])
        };
        json!({ "models": models })
    }
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
    pub named: Option<&'static Named>,
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
#[derive(Debug)]
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

    /// A per-turn member the fake route's capabilities refuse.
    const fn fake(field: &'static str) -> Self {
        Self {
            field,
            route: Some(FAKE_ROUTE),
        }
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
        if let Some(named) = self.named {
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
    pub(crate) fn naming(base: Self, named: &'static Named, message: &'static str) -> Self {
        Self {
            message,
            named: Some(named),
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
    /// The fake route is not configured or selected.
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

/// The only route this build can plan.
pub(crate) const FAKE_ROUTE: &str = "fake";
/// C1 §4 `deadlines.wall_ms` default (A3), for a fake turn that neither
/// sets nor inherits one.
pub(crate) const DEFAULT_WALL_MS: u64 = 3_600_000;
/// C1 §4 `deadlines.idle_ms` default (design §5).
pub(crate) const DEFAULT_IDLE_MS: u64 = 600_000;

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

/// The fake route's declared `capabilities.usage.tokens`, which labels
/// `status` `progress.tokens` (Task 4 design §2.4) and the envelope's
/// `usage`: its samples are exact per turn by construction (§2.5).
pub(crate) const FAKE_TOKEN_SCOPE: &str = "turn";

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
    /// C1 §4.1 `require` (design §11.1): `require` is a list of verb names,
    /// each met only by `native` support unless written `verb:partial`,
    /// which `partial` support also meets. The first unmet verb is
    /// `missing_capability` naming it; anything else is `invalid_params`.
    pub(crate) fn require(&self, require: &RawValue) -> Result<(), ApiError> {
        let invalid = || {
            ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::field("require") },
                "require is a list of verb names",
            )
        };
        let listed = json_limits::string_list(require.get()).ok_or_else(invalid)?;
        for name in &listed {
            // A verb met natively also meets `verb:partial`.
            let verb = name.strip_suffix(":partial").unwrap_or(name);
            let (named, support): (&'static Named, Support) = match verb {
                "spawn" => (&const { Named::fake("spawn") }, self.verbs.spawn),
                "resume" => (&const { Named::fake("resume") }, self.verbs.resume),
                "steer" => (&const { Named::fake("steer") }, self.verbs.steer),
                "cancel" => (&const { Named::fake("cancel") }, self.verbs.cancel),
                "close" => (&const { Named::fake("close") }, self.verbs.close),
                _ => return Err(invalid()),
            };
            let met = match support {
                Support::Native => true,
                Support::Unsupported { .. } => false,
            };
            if !met {
                return Err(ApiError::naming(
                    ApiError::MISSING_CAPABILITY,
                    named,
                    "a required verb is not supported natively on route fake",
                ));
            }
        }
        Ok(())
    }

    /// The fake route: one prompt, one turn, no controls, bounds or usage.
    pub(crate) fn fake() -> Self {
        let unsupported = |reason| Support::Unsupported { reason };
        Self {
            verbs: Verbs {
                spawn: Support::Native,
                resume: Support::Native,
                steer: unsupported("the fake route has no steer input"),
                cancel: Support::Native,
                close: Support::Native,
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
                tokens: FAKE_TOKEN_SCOPE,
                cost: "unavailable",
            },
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(crate) struct Deadlines {
    wall_ms: u64,
    /// Longest time without meaningful progress (design §5).
    idle_ms: u64,
}

/// Values frozen at acceptance (§3.2 `effective`, `turn.started` payload),
/// stored in the turn's Store row and driven from.
#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Effective {
    model: String,
    effort: Option<String>,
    bound: Option<Value>,
    deadlines: Deadlines,
    max_steps: Option<u64>,
}

impl Effective {
    /// Turn 1 of a fake session: its own values, else the fake route's defaults.
    pub(crate) fn fake(model: &str, overrides: &Overrides) -> Self {
        Self {
            model: model.to_owned(),
            effort: None,
            bound: None,
            deadlines: Deadlines {
                wall_ms: overrides.wall_ms.unwrap_or(DEFAULT_WALL_MS),
                idle_ms: overrides.idle_ms.unwrap_or(DEFAULT_IDLE_MS),
            },
            max_steps: None,
        }
    }

    /// C1 P5: a later turn's values, each omitted one inherited from the
    /// latest accepted turn's frozen `self`.
    pub(crate) fn inherit(&self, overrides: &Overrides) -> Self {
        let mut effective = self.clone();
        if let Some(wall_ms) = overrides.wall_ms {
            effective.deadlines.wall_ms = wall_ms;
        }
        if let Some(idle_ms) = overrides.idle_ms {
            effective.deadlines.idle_ms = idle_ms;
        }
        effective
    }

    /// The turn's Core-owned wall deadline budget.
    pub(crate) fn wall(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.deadlines.wall_ms)
    }

    /// The turn's Core-owned idle deadline budget (design §5).
    pub(crate) fn idle(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.deadlines.idle_ms)
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
    /// C1 §8.2: no meaningful progress within `idle_ms` (design §5).
    DeadlineIdle,
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

/// C1 §5 `usage`: every count `null` while provenance is `unavailable`;
/// a route that reports only a total fills `total_tokens`.
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

    /// The fake route's figure: the turn's summed samples under its declared
    /// scope, reported; unavailable without a sample.
    pub(crate) fn fake(total: Option<u64>) -> Self {
        match total {
            Some(total) => Self {
                total_tokens: Some(total),
                scope: FAKE_TOKEN_SCOPE,
                provenance: "reported",
                ..Self::UNAVAILABLE
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
    pub(crate) evidence: EvidenceRef,
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

    use super::{EventBody, SpawnParams, UNIX_EPOCH, retry_identity, retry_key, rfc3339};

    /// Fake-route edge rules: an empty options object per harness passes; a
    /// null `bound`, `effort` or `deadlines` is `invalid_params` while a
    /// present bound is `bound_unsupported`; nullable `output_schema` and
    /// `max_steps` accept null; a zero wall budget is refused.
    #[test]
    fn fake_per_turn_edge_values() {
        let check = |extra: serde_json::Value| {
            let mut params = json!({"harness":"fake","model":"fake","prompt":"p","handle":"h"});
            for (member, value) in extra.as_object().unwrap() {
                params[member] = value.clone();
            }
            let params: SpawnParams = serde_json::from_value(params).unwrap();
            params
                .per_turn()
                .fake_overrides()
                .map(|overrides| overrides.wall_ms)
                .map_err(|error| {
                    (
                        error.kind,
                        error.named.map(|named| (named.field, named.route)),
                    )
                })
        };
        assert_eq!(check(json!({"vendor":{"fake":{},"codex":{}}})), Ok(None));
        assert_eq!(check(json!({"deadlines":{"wall_ms":7}})), Ok(Some(7)));
        assert_eq!(
            check(json!({"bound":null})),
            Err(("invalid_params", Some(("bound", Some("fake")))))
        );
        assert_eq!(
            check(json!({"bound":{"mode":"full","extra_write_dirs":[],"network":true}})),
            Err(("bound_unsupported", Some(("bound", Some("fake")))))
        );
        assert_eq!(
            check(json!({"effort":null})),
            Err(("invalid_params", Some(("effort", Some("fake")))))
        );
        assert_eq!(
            check(json!({"deadlines":null})),
            Err(("invalid_params", Some(("deadlines", Some("fake")))))
        );
        assert_eq!(
            check(json!({"output_schema":null,"max_steps":null})),
            Ok(None)
        );
        assert_eq!(
            check(json!({"vendor":{"fake":{"k":"v"}}})),
            Err(("invalid_params", Some(("vendor", Some("fake")))))
        );
        assert_eq!(
            check(json!({"deadlines":{"wall_ms":0}})),
            Err(("invalid_params", Some(("deadlines.wall_ms", None))))
        );
    }

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
}
