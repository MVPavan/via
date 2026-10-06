//! A session's and a turn's frozen values as their Store rows hold them:
//! the internal `sessions.params` object written at spawn, its strict and
//! lenient decoding, and the projection of a turn's plan onto its C1
//! envelope fields (adapter design §5.1 #25, #33).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use via_adapters::{
    Bound, Capabilities, InheritPlan, SessionRef, Support, VendorArgs, VendorOptions, Verb,
};
use via_store::SessionRoute;

use super::{Effective, Planned, SessionMembers};
use crate::api::{self, ApiError, Requested, SpawnParams, Warning};

/// C1 §3.2's five `effective` members of a stored row: a queued turn's
/// `status` entry.
pub(crate) fn c1_effective(stored: &Value) -> Value {
    let member = |name| stored.get(name).cloned().unwrap_or(Value::Null);
    json!({
        "model": member("model"),
        "effort": member("effort"),
        "bound": member("bound"),
        "deadlines": member("deadlines"),
        "max_steps": member("max_steps"),
    })
}

/// The frozen session parameters as `sessions.params` holds them (design
/// §11.1, adapter design §5.1 #25). The route and adapter version are the
/// receipt's and the session's column (decision B5).
#[derive(Deserialize, Serialize)]
struct Params {
    harness: String,
    model: String,
    cwd: Option<String>,
    #[serde(default)]
    allow_untested: bool,
    /// The inherited-configuration settings as requested, and their
    /// effective states (AD13), which the session's driver opens with
    /// (C2 §6.2, critical r2 #5): required, never defaulted. The
    /// `config_switch_unverified` categories derive from them.
    inherit: InheritPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    vendor: VendorOptions,
    /// The raw vendor arguments every launch appends (C1 §4
    /// `vendor_args`, owner 2026-10-06); omitted when none, so a row
    /// written before them reads as none.
    #[serde(default, skip_serializing_if = "VendorArgs::is_empty")]
    vendor_args: VendorArgs,
}

/// The `vendor_args` member of a stored `sessions.params`, read alone:
/// every other member is skipped unread.
#[derive(Deserialize)]
struct StoredVendorArgs<'a> {
    #[serde(default, borrow, deserialize_with = "present")]
    vendor_args: Option<&'a serde_json::value::RawValue>,
}

/// A present member's raw text, `null` included.
fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<&'de serde_json::value::RawValue>, D::Error> {
    <&serde_json::value::RawValue>::deserialize(deserializer).map(Some)
}

/// Whether a stored `sessions.params` holds `vendor_args` other than
/// none: a list that decodes and is non-empty, or a member present that
/// does not decode (`null` included: corrupt, so warned rather than
/// dropped). Text that is not a JSON object holds no readable member.
fn passes_vendor_args(params: &str) -> bool {
    // serde would also read a struct from an array, by position.
    if !params.trim_start().starts_with('{') {
        return false;
    }
    let Ok(StoredVendorArgs {
        vendor_args: Some(raw),
    }) = serde_json::from_str(params)
    else {
        return false;
    };
    serde_json::from_str::<VendorArgs>(raw.get()).map_or(true, |args| !args.is_empty())
}

/// A spawn's frozen `sessions.params` from its plan: its turn-1 `vendor`
/// options and its `vendor_args` are the session's.
pub(crate) fn frozen_params(
    planned: &Planned,
    (params, members, cwd): (&SpawnParams, &SessionMembers, &str),
) -> Result<Value, ApiError> {
    serde_json::to_value(Params {
        harness: planned.plan.harness.to_owned(),
        model: params.model.clone(),
        cwd: Some(cwd.to_owned()),
        allow_untested: params.allow_untested,
        inherit: planned.plan.inherit,
        instructions: members.instructions.clone(),
        vendor: planned.effective.vendor.clone(),
        vendor_args: members.vendor_args.clone(),
    })
    .map_err(|_| ApiError::STORE)
}

/// A session's frozen facts, read back from its Store row (decision F12):
/// what its plan fixed at spawn.
#[derive(Clone, Debug, Default)]
pub(crate) struct Frozen {
    pub(crate) harness: String,
    pub(crate) route: String,
    /// The session's recorded adapter version (C1 §3.3, decision B5).
    pub(crate) adapter_version: String,
    /// The model as the caller requested it.
    pub(crate) model: String,
    /// The session's frozen working directory.
    pub(crate) cwd: Option<String>,
    pub(crate) instructions: Option<String>,
    pub(crate) vendor: VendorOptions,
    /// The session's raw vendor arguments (C1 §4 `vendor_args`).
    pub(crate) vendor_args: VendorArgs,
    /// Whether the stored row holds `vendor_args` other than none, read
    /// apart from the other members so a corrupt one cannot hide them
    /// (critical review, 2026-10-06): the `vendor_passthrough` evidence.
    passthrough: bool,
    pub(crate) allow_untested: bool,
    /// The frozen `inherit`, as requested and effective; `None` only
    /// where the row's parameters were not read.
    pub(crate) inherit: Option<InheritPlan>,
    capabilities: Option<Capabilities>,
}

impl Frozen {
    /// The facts of `route`, the session's Store identity; a member the row
    /// lacks or Core cannot read stays empty, never invented.
    pub(crate) fn of(route: &SessionRoute) -> Self {
        let params = route
            .params
            .as_deref()
            .and_then(|params| serde_json::from_str::<Params>(params).ok());
        let capabilities = route
            .capabilities
            .as_deref()
            .and_then(|capabilities| serde_json::from_str(capabilities).ok());
        Self::from_parts(route, params, capabilities)
    }

    /// The facts of `route` for a submission (Sol r1 #14, T3 §7.3): a
    /// frozen parameters or capabilities value that is present but cannot
    /// be decoded is corruption, `None`; an absent one stays empty.
    pub(crate) fn decode(route: &SessionRoute) -> Option<Self> {
        Self::decode_cause(route).ok()
    }

    /// [`Frozen::decode`], with the cause of a value that does not decode
    /// (critical r2 #10).
    pub(crate) fn decode_cause(route: &SessionRoute) -> Result<Self, String> {
        let params = match route.params.as_deref() {
            Some(params) => Some(
                serde_json::from_str::<Params>(params)
                    .map_err(|error| format!("frozen session parameters do not decode: {error}"))?,
            ),
            None => None,
        };
        let capabilities = match route.capabilities.as_deref() {
            Some(capabilities) => Some(
                serde_json::from_str(capabilities)
                    .map_err(|error| format!("frozen capabilities do not decode: {error}"))?,
            ),
            None => None,
        };
        Ok(Self::from_parts(route, params, capabilities))
    }

    fn from_parts(
        route: &SessionRoute,
        params: Option<Params>,
        capabilities: Option<Capabilities>,
    ) -> Self {
        let mut frozen = Self {
            harness: route.harness.clone(),
            route: route.route.clone().unwrap_or_default(),
            adapter_version: route.adapter_version.clone().unwrap_or_default(),
            passthrough: route.params.as_deref().is_some_and(passes_vendor_args),
            capabilities,
            ..Self::default()
        };
        if let Some(params) = params {
            frozen.model = params.model;
            frozen.cwd = params.cwd;
            frozen.instructions = params.instructions;
            frozen.vendor = params.vendor;
            frozen.vendor_args = params.vendor_args;
            frozen.allow_untested = params.allow_untested;
            frozen.inherit = Some(params.inherit);
        }
        frozen
    }

    /// The C2 §2 `SessionRef` of the session.
    pub(crate) fn session_ref(&self) -> SessionRef {
        SessionRef {
            harness: self.harness.clone(),
            route: self.route.clone(),
            adapter_version: self.adapter_version.clone(),
        }
    }

    /// The route's declared `capabilities.usage.tokens` (C1 §4.1), which
    /// labels `status` `progress.tokens` and the envelope's `usage`.
    pub(crate) fn token_scope(&self) -> &str {
        self.capabilities
            .as_ref()
            .map_or("turn", |capabilities| capabilities.usage.tokens.as_str())
    }

    /// Whether the route's declared support of `verb` is unsupported; a
    /// route whose capabilities are unread refuses nothing here.
    pub(crate) fn lacks(&self, verb: Verb) -> bool {
        self.capabilities.as_ref().is_some_and(|capabilities| {
            matches!(capabilities.verbs.get(verb), Support::Unsupported { .. })
        })
    }

    /// The route's declared `steer` support; none when unread.
    pub(crate) fn steer(&self) -> Option<&Support> {
        self.capabilities
            .as_ref()
            .map(|capabilities| &capabilities.verbs.steer)
    }

    /// The session's one `config_switch_unverified` warning (C1 §5, AD13),
    /// listing every category whose effective state is not the requested
    /// one; none when each is.
    pub(crate) fn config_warning(&self) -> Option<Warning> {
        let categories = self.inherit.as_ref()?.unverified();
        if categories.is_empty() {
            return None;
        }
        Warning::adapter(
            "config_switch_unverified",
            Some(json!({ "categories": categories })),
        )
    }

    /// The session's `vendor_passthrough` warning (C1 §5, owner
    /// 2026-10-06): one while it passes `vendor_args`, else none, even
    /// where another frozen member is corrupt.
    pub(crate) fn passthrough_warning(&self) -> Option<Warning> {
        (self.passthrough || !self.vendor_args.is_empty()).then_some(Warning::VENDOR_PASSTHROUGH)
    }
}

/// What a turn's envelope takes from its session's plan and its own
/// frozen values (adapter design §5.1 #33).
#[derive(Clone, Default)]
pub(crate) struct TurnPlan {
    pub(crate) frozen: Frozen,
    /// The turn's frozen values; `None` where its writer did not read them.
    pub(crate) effective: Option<Effective>,
}

impl TurnPlan {
    /// The plan of a turn of the session `route`, with its stored values.
    pub(crate) fn of(route: &SessionRoute, effective: Option<&Value>) -> Self {
        Self {
            frozen: Frozen::of(route),
            effective: effective.and_then(|value| serde_json::from_value(value.clone()).ok()),
        }
    }

    /// C1 §5 `model`: as requested at spawn, and as the turn ran it.
    pub(crate) fn model(&self) -> Requested<String> {
        Requested {
            requested: self.frozen.model.clone(),
            resolved: self.effective.as_ref().map_or_else(
                || self.frozen.model.clone(),
                |effective| effective.model.clone(),
            ),
        }
    }

    /// C1 §5 `effort`, as the turn froze it.
    pub(crate) fn effort(&self) -> Requested<Option<String>> {
        let effort = self
            .effective
            .as_ref()
            .and_then(|effective| effective.effort.clone());
        Requested {
            requested: effort.clone(),
            resolved: effort,
        }
    }

    /// C1 §5 `bound`: the turn's requested bound, the effective one its
    /// plan enforces, and whether they were inherited.
    pub(crate) fn bound(&self) -> api::Bound {
        let value =
            |bound: Option<&Bound>| bound.and_then(|bound| serde_json::to_value(bound).ok());
        let effective = self.effective.as_ref();
        api::Bound {
            requested: value(effective.and_then(Effective::requested_bound)),
            effective: value(effective.and_then(|effective| effective.bound.as_ref())),
            inherited: self
                .effective
                .as_ref()
                .is_some_and(|effective| effective.bound_inherited),
        }
    }

    /// C1 §5 `vendor_options`: the turn's frozen options.
    pub(crate) fn vendor_options(&self) -> Value {
        self.effective
            .as_ref()
            .and_then(|effective| serde_json::to_value(&effective.vendor).ok())
            .unwrap_or_else(|| json!({}))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use via_store::SessionRoute;

    use super::{Frozen, passes_vendor_args};

    /// Critical review (2026-10-06): `vendor_args` is read alone, so the
    /// warning's evidence survives any other member's corruption; a
    /// present member that does not decode is warned, none is not.
    #[test]
    fn vendor_args_are_read_apart_from_the_other_members() {
        for (params, warned) in [
            (r#"{"vendor_args":["--name=x"],"inherit":null}"#, true),
            (r#"{"vendor_args":["--name=x"],"model":7}"#, true),
            (r#"{"vendor_args":[1]}"#, true),
            (r#"{"vendor_args":null}"#, true),
            (r#"{"vendor_args":"--x"}"#, true),
            (r#"{"vendor_args":[]}"#, false),
            (r#"{"inherit":null}"#, false),
            ("not json", false),
            ("[1]", false),
        ] {
            assert_eq!(passes_vendor_args(params), warned, "{params}");
            let route = SessionRoute {
                harness: "fake".to_owned(),
                params: Some(params.to_owned()),
                ..SessionRoute::default()
            };
            assert_eq!(
                Frozen::of(&route).passthrough_warning().is_some(),
                warned,
                "{params}"
            );
        }
    }

    fn route(params: &serde_json::Value) -> SessionRoute {
        SessionRoute {
            harness: "fake".to_owned(),
            params: Some(params.to_string()),
            ..SessionRoute::default()
        }
    }

    /// Critical r1 #2, r2 #5: the frozen `inherit`, as requested and
    /// effective, decodes into the typed C2 value; one that is absent,
    /// lacks either half, or has a half that is misnamed, incomplete or
    /// not a state is corrupt, never a default. The
    /// `config_switch_unverified` categories derive from the two halves.
    #[test]
    fn the_frozen_inherit_is_typed_and_required() {
        let effective = json!({"hooks":"unknown","mcp_servers":"off","plugins":"on",
                               "skills":"on","agents":"off","instruction_files":"on"});
        let requested = json!({"hooks":"on","mcp_servers":"off","plugins":"on",
                               "skills":"on","agents":"on","instruction_files":"on"});
        let inherit = json!({"requested": requested, "effective": effective});
        let params = json!({"harness":"fake","model":"fake","cwd":"/w","inherit":inherit});
        let frozen = Frozen::decode(&route(&params)).unwrap();
        assert_eq!(serde_json::to_value(frozen.inherit).unwrap(), inherit);
        assert_eq!(
            serde_json::to_value(frozen.config_warning().unwrap()).unwrap()["data"],
            (json!({"categories": [
                {"category":"hooks","requested":"on","effective":"unknown"},
                {"category":"agents","requested":"on","effective":"off"},
            ]}))
        );
        let mut absent = params.clone();
        absent.as_object_mut().unwrap().remove("inherit");
        let mut effective_only = params.clone();
        effective_only["inherit"] = effective;
        let mut no_request = params.clone();
        no_request["inherit"]
            .as_object_mut()
            .unwrap()
            .remove("requested");
        let mut unknown_state = params.clone();
        unknown_state["inherit"]["effective"]["hooks"] = json!("maybe");
        let mut missing = params.clone();
        missing["inherit"]["requested"]
            .as_object_mut()
            .unwrap()
            .remove("skills");
        let mut extra = params.clone();
        extra["inherit"]["effective"]["themes"] = json!("on");
        let mut extra_half = params.clone();
        extra_half["inherit"]["observed"] = requested;
        let mut shape = params;
        shape["inherit"] = json!(["on"]);
        for case in [
            absent,
            effective_only,
            no_request,
            unknown_state,
            missing,
            extra,
            extra_half,
            shape,
        ] {
            assert!(Frozen::decode(&route(&case)).is_none(), "{case}");
        }
    }
}
