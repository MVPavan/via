//! The generic C1 intake (adapter design §5.1 #5–#17, #25): per-turn values
//! with C1 P5 omission and clearing, a route's refusals as C1 errors,
//! `describe` and `models` through the adapter set. A session's frozen
//! facts as its Store row holds them are [`frozen`]'s.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use via_adapters::{
    AdapterSet, Bound, DescribeRequest, Harness, Refusal, RefusalKind, RoutePlan, SessionRef,
    Support, TurnParams, TurnSpec, VendorOptions, Verb, VerbReq, harness_names,
};
use via_store::json_limits::{self, Shape};

use crate::api::{
    ApiError, DEFAULT_IDLE_MS, DEFAULT_WALL_MS, DescribeParams, ModelsParams, Named, Nullable,
    PerTurn, SpawnParams,
};

mod frozen;

pub(crate) use frozen::{Frozen, TurnPlan, c1_effective, frozen_params};

/// Longest `output_schema`, encoded (C1 §4).
const OUTPUT_SCHEMA_MAX: usize = 256 * 1024;

/// Longest effective `bound`, encoded (C1 §5).
const EFFECTIVE_BOUND_MAX: usize = 32 * 1024;

/// Longest `cwd`, encoded with its quotes (C1 §5).
const CWD_MAX: usize = 4 * 1024;

/// A per-turn member as sent (C1 P5): omitted (inherit), an explicit
/// `null` (clear), or a value.
#[derive(Clone)]
pub(crate) enum Member<T> {
    Omitted,
    Null,
    Given(T),
}

impl<T> Member<T> {
    /// The value a turn takes: its own, none after `null`, else `inherited`.
    fn or(self, inherited: Option<T>) -> Option<T> {
        match self {
            Self::Omitted => inherited,
            Self::Null => None,
            Self::Given(value) => Some(value),
        }
    }

    /// The value given, if any.
    fn given(&self) -> Option<&T> {
        match self {
            Self::Given(value) => Some(value),
            Self::Omitted | Self::Null => None,
        }
    }
}

/// What one turn sets for itself (C1 §4); anything omitted inherits.
pub(crate) struct Overrides {
    effort: Member<String>,
    bound: Member<Bound>,
    output_schema: Member<Value>,
    max_steps: Member<u64>,
    vendor: Member<VendorOptions>,
    wall_ms: Option<u64>,
    idle_ms: Option<u64>,
}

/// One member's C1 type rule: its name, whether `null` is a value, and
/// the messages of a refused `null` and of a value of the wrong shape.
struct Rule {
    field: &'static str,
    nullable: bool,
    null: &'static str,
    invalid: &'static str,
}

/// A member decoded by `rule`: an omitted one is `Omitted`, a `null` one
/// `Null` where the member is nullable, else `invalid_params` naming it.
fn member<T: DeserializeOwned>(raw: Option<&RawValue>, rule: &Rule) -> Result<Member<T>, ApiError> {
    let Some(raw) = raw else {
        return Ok(Member::Omitted);
    };
    let refuse =
        |message| ApiError::naming(ApiError::INVALID_PARAMS, Named::field(rule.field), message);
    if json_limits::shape(raw.get()) == Shape::Null {
        return if rule.nullable {
            Ok(Member::Null)
        } else {
            Err(refuse(rule.null))
        };
    }
    serde_json::from_str(raw.get())
        .map(Member::Given)
        .map_err(|_| refuse(rule.invalid))
}

impl PerTurn<'_> {
    /// C1 §4's type rules for the per-turn members, before any route rule:
    /// the encoded sizes (design §6.4); `effort` a string, `bound` a C1
    /// bound and `vendor` an object of option objects, none of them `null`;
    /// `output_schema` `null` (clear) or a JSON Schema object of at most
    /// 256 KiB encoded that compiles as draft 2020-12 (Q2); `max_steps`
    /// `null` (clear) or a positive integer; `deadlines` as before, its
    /// members never `null` (A9) and never 0.
    pub(crate) fn overrides(&self) -> Result<Overrides, ApiError> {
        self.check_sizes()?;
        let effort = member(
            self.effort,
            &Rule {
                field: "effort",
                nullable: false,
                null: "effort cannot be null",
                invalid: "effort is a string",
            },
        )?;
        let bound = member(
            self.bound,
            &Rule {
                field: "bound",
                nullable: false,
                null: "bound cannot be null",
                invalid: "bound is {mode, extra_write_dirs, network}",
            },
        )?;
        let output_schema = schema_member(self.output_schema)?;
        let max_steps = member(
            self.max_steps,
            &Rule {
                field: "max_steps",
                nullable: true,
                null: "",
                invalid: "max_steps is a positive integer or null",
            },
        )?;
        if matches!(max_steps, Member::Given(0)) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                Named::field("max_steps"),
                "max_steps must be at least 1",
            ));
        }
        let vendor = member(
            self.vendor,
            &Rule {
                field: "vendor",
                nullable: false,
                null: "vendor cannot be null",
                invalid: "vendor is an object of per-harness option objects",
            },
        )?;
        let (wall_ms, idle_ms) = match self.deadlines {
            None => (None, None),
            Some(Nullable::Null) => {
                return Err(ApiError::naming(
                    ApiError::INVALID_PARAMS,
                    Named::field("deadlines"),
                    "deadlines cannot be null",
                ));
            }
            Some(Nullable::Given(deadlines)) => deadlines.budgets()?,
        };
        Ok(Overrides {
            effort,
            bound,
            output_schema,
            max_steps,
            vendor,
            wall_ms,
            idle_ms,
        })
    }
}

/// `output_schema` (C1 §4, Q2): `null` clears; a value is an object of at
/// most 256 KiB encoded (whether it compiles is the caller's check).
fn schema_member(raw: Option<&RawValue>) -> Result<Member<Value>, ApiError> {
    let refuse = |message| {
        ApiError::naming(
            ApiError::INVALID_PARAMS,
            Named::field("output_schema"),
            message,
        )
    };
    let Some(raw) = raw else {
        return Ok(Member::Omitted);
    };
    match json_limits::shape(raw.get()) {
        Shape::Null => return Ok(Member::Null),
        Shape::Object { .. } => {}
        Shape::Bool | Shape::Number | Shape::String | Shape::Array { .. } => {
            return Err(refuse("output_schema is a JSON Schema object or null"));
        }
    }
    if raw.get().len() > OUTPUT_SCHEMA_MAX {
        return Err(refuse("output_schema is longer than 256 KiB encoded"));
    }
    let schema: Value = serde_json::from_str(raw.get())
        .map_err(|_| refuse("output_schema is a JSON Schema object or null"))?;
    // Whether it compiles is checked off the executor: [`schema_refused`].
    Ok(Member::Given(schema))
}

/// The C1 error of an `output_schema` that does not compile as a
/// self-contained draft 2020-12 schema within the compile limits.
pub(crate) fn schema_refused() -> ApiError {
    ApiError::naming(
        ApiError::INVALID_PARAMS,
        Named::field("output_schema"),
        "output_schema is not a self-contained draft 2020-12 JSON Schema within VIA's limits",
    )
}

impl Overrides {
    /// The `output_schema` this turn gives, if any.
    pub(crate) fn schema(&self) -> Option<&Value> {
        self.output_schema.given()
    }
}

/// C1 §4.1 `require` (design §11.1): a list of verb names, each optionally
/// `:partial`; anything else is `invalid_params` naming it.
pub(crate) fn require(raw: Option<&RawValue>) -> Result<Vec<VerbReq>, ApiError> {
    let invalid = || {
        ApiError::naming(
            ApiError::INVALID_PARAMS,
            Named::field("require"),
            "require is a list of verb names",
        )
    };
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    json_limits::string_list(raw.get())
        .ok_or_else(invalid)?
        .iter()
        .map(|entry| VerbReq::parse(entry).ok_or_else(invalid))
        .collect()
}

/// C1 §4 `instructions`: `{text}` or `{path}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Instructions {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// A spawn's `instructions` as given (C1 §4): its text, or the path of
/// the file Core reads it from at the receipt.
pub(crate) enum InstructionsSource {
    Text(String),
    Path(String),
}

/// The session's `instructions`, if given: `{text}` or `{path}`; any
/// other shape is refused by name.
pub(crate) fn instructions(raw: Option<&RawValue>) -> Result<Option<InstructionsSource>, ApiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match serde_json::from_str::<Instructions>(raw.get()) {
        Ok(Instructions {
            text: Some(text),
            path: None,
        }) => Ok(Some(InstructionsSource::Text(text))),
        Ok(Instructions {
            text: None,
            path: Some(path),
        }) => Ok(Some(InstructionsSource::Path(path))),
        Ok(_) | Err(_) => Err(instructions_refused(
            "instructions is {text} or {path}",
            None,
        )),
    }
}

/// `invalid_params` naming `instructions`, with a file's `reason`.
pub(crate) fn instructions_refused(
    message: &'static str,
    reason: Option<&'static str>,
) -> ApiError {
    ApiError {
        reason,
        ..ApiError::naming(
            ApiError::INVALID_PARAMS,
            Named::field("instructions"),
            message,
        )
    }
}

/// Whether `cwd` is an absolute path at most 4 KiB encoded (C1 §5).
pub(crate) fn cwd_fits(cwd: &str) -> bool {
    via_adapters::encoded_text_len(cwd) + 2 <= CWD_MAX && std::path::Path::new(cwd).is_absolute()
}

/// A C1 error for a route's refusal (C2 §2), naming the member and route.
pub(crate) fn refused(refusal: &Refusal) -> ApiError {
    let base = match &refusal.kind {
        RefusalKind::UnsupportedVerb => ApiError::UNSUPPORTED_VERB,
        RefusalKind::BoundUnsupported => ApiError::BOUND_UNSUPPORTED,
        RefusalKind::HarnessUnavailable | RefusalKind::VersionRefused => {
            ApiError::HARNESS_UNAVAILABLE
        }
        RefusalKind::UnknownModel => ApiError::UNKNOWN_MODEL,
        RefusalKind::VendorOptionConflict => ApiError {
            kind2: Some("vendor_option_conflict"),
            ..ApiError::INVALID_PARAMS
        },
        RefusalKind::InvalidParam { .. } => ApiError::INVALID_PARAMS,
        RefusalKind::MissingCapability { .. } => ApiError::MISSING_CAPABILITY,
    };
    ApiError {
        message: refusal_message(refusal),
        named: Named {
            field: refusal.field().map(Cow::Borrowed),
            harness: refusal.route.and_then(harness_of).map(Cow::Borrowed),
            route: refusal.route.map(Cow::Borrowed),
            verb: refusal.verb.map(|verb| Cow::Borrowed(verb.as_str())),
            delivery: None,
        }
        .boxed(),
        reason: refusal.reason,
        ..base
    }
}

/// The static name of a known `route`, as frozen in a session.
pub(crate) fn route_name(route: &str) -> Option<&'static str> {
    harness_names()
        .filter_map(Harness::parse)
        .map(Harness::route)
        .find(|known| *known == route)
}

/// The harness `route` serves, when this build knows it.
fn harness_of(route: &str) -> Option<&'static str> {
    harness_names()
        .filter_map(Harness::parse)
        .find(|harness| harness.route() == route)
        .map(Harness::name)
}

/// `unsupported_verb` of `verb` on a session's frozen route (C1 §4.1,
/// §8.1), naming the verb, harness and route.
pub(crate) fn unsupported_on(verb: Verb, frozen: &Frozen) -> ApiError {
    ApiError {
        message: "verb is unsupported on this route",
        named: Named {
            verb: Some(Cow::Borrowed(verb.as_str())),
            harness: Some(Cow::Owned(frozen.harness.clone())),
            route: Some(Cow::Owned(frozen.route.clone())),
            ..Named::default()
        }
        .boxed(),
        ..ApiError::UNSUPPORTED_VERB
    }
}

/// `unsupported_verb` of `verb` on `route` (C1 §4.1, §8.1).
pub(crate) fn unsupported_verb(verb: Verb, route: &'static str) -> ApiError {
    refused(&Refusal {
        kind: RefusalKind::UnsupportedVerb,
        message: String::new(),
        verb: Some(verb),
        route: Some(route),
        reason: None,
    })
}

/// VIA's own bounded message for a refusal, naming its member.
fn refusal_message(refusal: &Refusal) -> &'static str {
    match &refusal.kind {
        RefusalKind::UnsupportedVerb => "verb is unsupported on this route",
        RefusalKind::BoundUnsupported => "the route cannot enforce the requested bound",
        RefusalKind::HarnessUnavailable if refusal.reason == Some("adapter_version") => {
            "the session's adapter version is not compatible with this adapter"
        }
        RefusalKind::HarnessUnavailable => "harness is unavailable",
        RefusalKind::VersionRefused => "the harness binary's version was refused",
        RefusalKind::UnknownModel => "unknown model",
        RefusalKind::VendorOptionConflict => "vendor options use a reserved key",
        RefusalKind::MissingCapability { verb } => match verb {
            Verb::Spawn => "a required verb is not met on this route: spawn",
            Verb::Resume => "a required verb is not met on this route: resume",
            Verb::Steer => "a required verb is not met on this route: steer",
            Verb::Cancel => "a required verb is not met on this route: cancel",
            Verb::Close => "a required verb is not met on this route: close",
        },
        RefusalKind::InvalidParam { field } => match *field {
            "effort" => "effort is not a value this route accepts",
            "output_schema" => "output_schema is unsupported on this route",
            "max_steps" => "max_steps is unsupported on this route",
            "vendor" => "vendor options are unsupported on this route",
            "instructions" => "instructions is unsupported on this route",
            "harness" => "harness is required: several harnesses catalog the model",
            "model" => "model is required",
            _ => "a parameter is not accepted on this route",
        },
    }
}

/// The C1 §3.2 spawn's session members, checked without I/O. An
/// `instructions {path}` is in `instructions_path` until the receipt reads
/// it into `instructions`.
pub(crate) struct SessionMembers {
    pub(crate) require: Vec<VerbReq>,
    pub(crate) instructions: Option<String>,
    pub(crate) instructions_path: Option<String>,
}

impl SpawnParams {
    /// Design §11.1: the session members' shapes, before any route rule.
    pub(crate) fn session_members(&self) -> Result<SessionMembers, ApiError> {
        self.check_session_members()?;
        let (instructions, instructions_path) = match instructions(self.instructions.as_deref())? {
            Some(InstructionsSource::Text(text)) => (Some(text), None),
            Some(InstructionsSource::Path(path)) => (None, Some(path)),
            None => (None, None),
        };
        Ok(SessionMembers {
            require: require(self.require.as_deref())?,
            instructions,
            instructions_path,
        })
    }
}

/// A planned spawn (C2 §2 `plan`): the route, its turn-1 values and what
/// its session freezes.
pub(crate) struct Planned {
    pub(crate) plan: RoutePlan,
    pub(crate) effective: Effective,
}

/// Plans a spawn's session and turn 1 (adapter design §5.1 #23–#25): the
/// adapter set resolves the harness, route and model; the plan's first
/// refusal, an unsupported `instructions`, then `check_turn`'s first
/// refusal of the turn's values (`output_schema`, `max_steps`) are the
/// spawn's error, each naming the member and route.
pub(crate) fn plan_spawn(
    adapter: &AdapterSet,
    params: &SpawnParams,
    members: &SessionMembers,
    cwd: &str,
) -> Result<Planned, ApiError> {
    let overrides = params.per_turn().overrides()?;
    let request = DescribeRequest {
        harness: params.harness.clone(),
        model: Some(params.model.clone()),
        effort: overrides.effort.given().cloned(),
        bound: overrides.bound.given().cloned(),
        require: members.require.clone(),
        vendor: overrides.vendor.given().cloned().unwrap_or_default(),
        cwd: Some(cwd.into()),
        allow_untested: params.allow_untested,
    };
    let plan = adapter
        .plan(&request)
        .map_err(|refusal| refused(&refusal))?;
    // Sol r1 #5 (C1 §4.1): a route that cannot spawn refuses before any receipt.
    if matches!(plan.capabilities.verbs.spawn, Support::Unsupported { .. }) {
        return Err(unsupported_verb(Verb::Spawn, plan.route));
    }
    if let Some(refusal) = plan.refusals.first() {
        return Err(refused(refusal));
    }
    if members.instructions.is_some()
        && matches!(
            plan.capabilities.params.instructions,
            Support::Unsupported { .. }
        )
    {
        return Err(refused(&Refusal {
            kind: RefusalKind::InvalidParam {
                field: "instructions",
            },
            message: String::new(),
            verb: None,
            route: Some(plan.route),
            reason: None,
        }));
    }
    let bound = EffectiveBound::of(
        overrides.bound.given(),
        plan.effective_bound.clone(),
        plan.route,
    )?;
    let effective = Effective::first(plan.model.resolved.clone(), overrides, bound);
    let session = SessionRef {
        harness: plan.harness.to_owned(),
        route: plan.route.to_owned(),
        adapter_version: plan.adapter_version.clone(),
    };
    adapter
        .check_turn(&session, &effective.turn_params())
        .map_err(|refusal| refused(&refusal))?;
    Ok(Planned { plan, effective })
}

impl DescribeParams {
    /// `describe` (C1 §3.1, design §4.6): the plan the parameters would
    /// take, with no process and no write. Shapes are checked first; the
    /// adapter set's refusal of the harness or model is the error, and
    /// what the route refuses of the rest is listed in `refusals`.
    pub(crate) fn describe(&self, adapter: &AdapterSet) -> Result<Value, ApiError> {
        if self.harness.is_none() && self.model.is_none() {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                Named::field("model"),
                "describe takes a harness or a model",
            ));
        }
        if self.cwd.as_deref().is_some_and(|cwd| !cwd_fits(cwd)) {
            return Err(ApiError::naming(
                ApiError::INVALID_PARAMS,
                Named::field("cwd"),
                "cwd must be an absolute path of at most 4 KiB encoded",
            ));
        }
        let shapes = PerTurn {
            effort: None,
            bound: self.bound.as_deref(),
            output_schema: None,
            deadlines: None,
            max_steps: None,
            vendor: self.vendor.as_deref(),
        }
        .overrides()?;
        let request = DescribeRequest {
            harness: self.harness.clone(),
            model: self.model.clone(),
            effort: None,
            bound: shapes.bound.given().cloned(),
            require: require(self.require.as_deref())?,
            vendor: shapes.vendor.given().cloned().unwrap_or_default(),
            cwd: self.cwd.as_ref().map(Into::into),
            allow_untested: self.allow_untested,
        };
        let plan = adapter
            .plan(&request)
            .map_err(|refusal| refused(&refusal))?;
        serde_json::to_value(&plan).map_err(|_| ApiError::STORE)
    }
}

impl ModelsParams {
    /// `models` (C1 §3.13): each configured harness's catalog, or one's.
    pub(crate) fn models(&self, adapter: &AdapterSet) -> Value {
        json!({ "models": adapter.models(self.harness.as_deref()) })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(crate) struct Deadlines {
    wall_ms: u64,
    /// Longest time without meaningful progress (design §5).
    idle_ms: u64,
}

/// A turn's bound as requested and as its route's plan enforces it (C1 §5
/// `bound`, C2 `RoutePlan.effective_bound`); `requested` is kept only
/// while it differs from `effective`.
pub(crate) struct EffectiveBound {
    effective: Option<Bound>,
    requested: Option<Bound>,
}

impl EffectiveBound {
    /// The plan's `effective` bound for `requested` on `route`: at most
    /// 32 KiB encoded (C1 §5), and present whenever one was requested.
    pub(crate) fn of(
        requested: Option<&Bound>,
        effective: Option<Bound>,
        route: &'static str,
    ) -> Result<Self, ApiError> {
        let refuse = |kind| {
            refused(&Refusal {
                kind,
                message: String::new(),
                verb: None,
                route: Some(route),
                reason: None,
            })
        };
        if requested.is_some() && effective.is_none() {
            return Err(refuse(RefusalKind::BoundUnsupported));
        }
        if effective.as_ref().is_some_and(|bound| {
            serde_json::to_string(bound).map_or(true, |text| text.len() > EFFECTIVE_BOUND_MAX)
        }) {
            return Err(ApiError {
                message: "the route's effective bound is longer than 32 KiB encoded",
                ..refuse(RefusalKind::InvalidParam { field: "bound" })
            });
        }
        Ok(Self {
            requested: requested
                .filter(|requested| Some(*requested) != effective.as_ref())
                .cloned(),
            effective,
        })
    }
}

/// A turn's values frozen at acceptance (C1 §3.2 `effective`, P5), as its
/// Store row holds them and as it is driven. Beyond C1's five members it
/// keeps the frozen `output_schema` and `vendor` options and whether its
/// bound was inherited; each is left out of the row while empty, so a
/// turn that sets none of them stores exactly C1's shape.
#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Effective {
    model: String,
    effort: Option<String>,
    /// The effective bound: what the route's plan enforces (C2
    /// `RoutePlan.effective_bound`), C1's `effective.bound`.
    bound: Option<Bound>,
    deadlines: Deadlines,
    max_steps: Option<u64>,
    /// The requested bound, kept only while it differs from the effective
    /// one (Sol r1 #3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bound_requested: Option<Bound>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    vendor: VendorOptions,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    bound_inherited: bool,
}

impl Effective {
    /// Turn 1: its own values on the resolved `model`, else C1's defaults;
    /// `bound` is the plan's effective bound for the requested one.
    pub(crate) fn first(model: String, overrides: Overrides, bound: EffectiveBound) -> Self {
        Self {
            model,
            effort: overrides.effort.or(None),
            bound: bound.effective,
            bound_requested: bound.requested,
            deadlines: Deadlines {
                wall_ms: overrides.wall_ms.unwrap_or(DEFAULT_WALL_MS),
                idle_ms: overrides.idle_ms.unwrap_or(DEFAULT_IDLE_MS),
            },
            max_steps: overrides.max_steps.or(None),
            output_schema: overrides.output_schema.or(None),
            vendor: overrides.vendor.or(None).unwrap_or_default(),
            bound_inherited: false,
        }
    }

    /// C1 P5: a later turn's values, each omitted one inherited from the
    /// latest accepted turn's frozen `self`; `null` clears `output_schema`
    /// and `max_steps`. An omitted bound inherits both the requested and
    /// the effective one and is marked so (C1 §5 `bound`); a given one
    /// stands for both. [`Self::with_bound`] then sets the effective one
    /// the route reports for the requested one.
    pub(crate) fn inherit(&self, overrides: Overrides) -> Self {
        let bound_inherited = matches!(overrides.bound, Member::Omitted) && self.bound.is_some();
        let (bound, bound_requested) = match overrides.bound {
            Member::Given(bound) => (Some(bound), None),
            Member::Omitted | Member::Null => (self.bound.clone(), self.bound_requested.clone()),
        };
        Self {
            model: self.model.clone(),
            effort: overrides.effort.or(self.effort.clone()),
            bound,
            bound_requested,
            deadlines: Deadlines {
                wall_ms: overrides.wall_ms.unwrap_or(self.deadlines.wall_ms),
                idle_ms: overrides.idle_ms.unwrap_or(self.deadlines.idle_ms),
            },
            max_steps: overrides.max_steps.or(self.max_steps),
            output_schema: overrides.output_schema.or(self.output_schema.clone()),
            vendor: overrides
                .vendor
                .or(Some(self.vendor.clone()))
                .unwrap_or_default(),
            bound_inherited,
        }
    }

    /// The turn's frozen model.
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    /// `self` with `bound`, its requested bound as the route enforces it;
    /// whether it was inherited stays.
    pub(crate) fn with_bound(self, bound: EffectiveBound) -> Self {
        Self {
            bound: bound.effective,
            bound_requested: bound.requested,
            ..self
        }
    }

    /// The turn's frozen effective bound, if any.
    pub(crate) fn bound(&self) -> Option<&Bound> {
        self.bound.as_ref()
    }

    /// The turn's requested bound, if any.
    fn requested_bound(&self) -> Option<&Bound> {
        self.bound_requested.as_ref().or(self.bound.as_ref())
    }

    /// The turn's frozen `output_schema`, if any.
    pub(crate) fn output_schema(&self) -> Option<&Value> {
        self.output_schema.as_ref()
    }

    /// The turn's Core-owned wall deadline budget.
    pub(crate) fn wall(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.deadlines.wall_ms)
    }

    /// The turn's Core-owned idle deadline budget (design §5).
    pub(crate) fn idle(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.deadlines.idle_ms)
    }

    /// C1 §3.2's `effective`: the receipt's and `turn.started`'s.
    pub(crate) fn c1(&self) -> Value {
        json!({
            "model": self.model,
            "effort": self.effort,
            "bound": self.bound,
            "deadlines": self.deadlines,
            "max_steps": self.max_steps,
        })
    }

    /// The Store row's form.
    pub(crate) fn stored(&self) -> Result<Value, ApiError> {
        serde_json::to_value(self).map_err(|_| ApiError::STORE)
    }

    /// The values `check_turn` validates against the frozen route: the
    /// bound as requested.
    pub(crate) fn turn_params(&self) -> TurnParams {
        TurnParams {
            effort: self.effort.clone(),
            bound: self.requested_bound().cloned(),
            output_schema: self.output_schema.is_some(),
            max_steps: self.max_steps,
            vendor: self.vendor.clone(),
        }
    }

    /// C2 §2 `TurnSpec` of the turn's `prompt` with its frozen values.
    pub(crate) fn turn_spec(&self, prompt: String) -> TurnSpec {
        TurnSpec {
            prompt,
            effort: self.effort.clone(),
            bound: self.bound.clone(),
            output_schema: self
                .output_schema
                .as_ref()
                .and_then(|schema| serde_json::value::to_raw_value(schema).ok()),
            max_steps: self.max_steps,
            vendor: self.vendor.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use std::borrow::Cow;

    use super::{Effective, EffectiveBound, Member};
    use crate::SpawnParams;

    /// The per-turn type rules (C1 §4): an empty options object per
    /// harness passes; a null `bound`, `effort`, `vendor` or `deadlines`
    /// is `invalid_params` naming it; nullable `output_schema` and
    /// `max_steps` accept null; a schema that is not an object, a zero
    /// step limit and a zero wall budget are refused.
    #[test]
    fn per_turn_type_rules() {
        let check = |extra: serde_json::Value| {
            let mut params = json!({"model":"m","prompt":"p","handle":"h"});
            for (member, value) in extra.as_object().unwrap() {
                params[member] = value.clone();
            }
            let params: SpawnParams = serde_json::from_value(params).unwrap();
            params
                .per_turn()
                .overrides()
                .map(|overrides| overrides.wall_ms)
                .map_err(|error| {
                    let field = error.named.and_then(|named| named.field);
                    (error.kind, field.map(Cow::into_owned))
                })
        };
        assert_eq!(check(json!({"vendor":{"a":{},"b":{}}})), Ok(None));
        assert_eq!(check(json!({"deadlines":{"wall_ms":7}})), Ok(Some(7)));
        for field in ["bound", "effort", "vendor", "deadlines"] {
            assert_eq!(
                check(json!({ field: null })),
                Err(("invalid_params", Some(field.to_owned()))),
                "{field}"
            );
        }
        assert_eq!(
            check(json!({"output_schema":null,"max_steps":null})),
            Ok(None)
        );
        for (field, value) in [
            ("output_schema", json!([])),
            ("max_steps", json!(0)),
            ("vendor", json!({"a":1})),
            ("bound", json!({"mode":"full"})),
            ("effort", json!(3)),
        ] {
            assert_eq!(
                check(json!({ field: value })),
                Err(("invalid_params", Some(field.to_owned()))),
                "{field}"
            );
        }
        assert_eq!(
            check(json!({"deadlines":{"wall_ms":0}})),
            Err(("invalid_params", Some("deadlines.wall_ms".to_owned())))
        );
    }

    /// C1 P5: an omitted member inherits, `null` clears the nullable ones,
    /// and an inherited bound is marked so.
    #[test]
    fn inheritance_and_clearing() {
        let overrides = |raw: serde_json::Value| {
            let mut params = json!({"model":"m","prompt":"p","handle":"h"});
            for (member, value) in raw.as_object().unwrap() {
                params[member] = value.clone();
            }
            let params: SpawnParams = serde_json::from_value(params).unwrap();
            params.per_turn().overrides().unwrap()
        };
        let bound = json!({"mode":"full","extra_write_dirs":[],"network":true});
        let first =
            overrides(json!({"effort":"low","bound":bound,"output_schema":{},"max_steps":3}));
        let planned =
            EffectiveBound::of(first.bound.given(), first.bound.given().cloned(), "r").unwrap();
        let first = Effective::first("m".to_owned(), first, planned);
        assert!(!first.bound_inherited);
        let second = first.inherit(overrides(json!({"effort":"high"})));
        assert_eq!(second.effort.as_deref(), Some("high"));
        assert!(second.bound_inherited && second.output_schema.is_some());
        assert_eq!(second.max_steps, Some(3));
        let third = second.inherit(overrides(json!({"output_schema":null,"max_steps":null})));
        assert!(third.output_schema.is_none() && third.max_steps.is_none());
        assert_eq!(
            third.c1(),
            json!({"model":"m","effort":"high","bound":bound,
                   "deadlines":{"wall_ms":3_600_000,"idle_ms":600_000},"max_steps":null})
        );
        assert!(Member::<u8>::Null.or(Some(1)).is_none());
    }
}
