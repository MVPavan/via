//! `opencode-serve`'s pure planning (`vendors/opencode.md` §2.2, §4.5, §5,
//! §6, §9, §12): capabilities, the checked versions, inherited
//! configuration, the per-turn refusals and the cached server refusal.
//! Reads only configuration, the instance cache and the registry's
//! in-memory facts; starts and writes nothing.

use std::collections::BTreeMap;

use super::{ADAPTER_VERSION, CHECKED, HARNESS, OpenCodeAdapter};
use crate::capabilities::{BoundMode, Capabilities, ParamSupport, Support, UsageSupport, Verbs};
use crate::harness::Harness;
use crate::passthrough::{self, VendorArgs};
use crate::plan::{
    Bound, Category, CategoryDecl, DescribeRequest, Inherit, ModelChoice, ParamSizes, Refusal,
    RefusalKind, RoutePlan, ServerKey, Switch, TurnCheck, TurnParams, VendorOptions, VersionStatus,
    effective_inherit,
};

/// §9: the largest admitted `json_len(prompt) + json_len(cwd)`.
pub(crate) const PROMPT_ADMISSION_MAX: usize = 1_048_576 - 8_192;

/// §5, §9: the largest admitted instruction entry, in bytes of its JSON
/// string encoding.
pub(crate) const INSTRUCTIONS_JSON_MAX: usize = 262_144;

/// C2 §6.1's `OpenCode` reservations, as normalized fragments: a `vendor`
/// key containing one, in any spelling, sets what the route owns.
const RESERVED: [&str; 34] = [
    "listen",
    "hostname",
    "port",
    "password",
    "home",
    "xdg",
    "config",
    "database",
    "opencode",
    "credential",
    "integration",
    "auth",
    "plugin",
    "mcp",
    "permission",
    "tool",
    "agent",
    "cwd",
    "directory",
    "location",
    "model",
    "provider",
    "variant",
    "effort",
    "instruction",
    "env",
    "session",
    "message",
    "delivery",
    "inbox",
    "form",
    "stdio",
    "service",
    "standalone",
];

/// The capabilities snapshot (§12).
pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        verbs: Verbs {
            spawn: Support::Native,
            resume: Support::Native,
            steer: Support::Unsupported {
                reason: "deferred past the first release".to_owned(),
            },
            cancel: Support::Native,
            close: Support::Native,
        },
        params: ParamSupport {
            instructions: Support::Native,
            output_schema: Support::Unsupported {
                reason: "No structured-output field on opencode-serve 2.0.22".to_owned(),
            },
            effort: Support::Native,
            max_steps: Support::Unsupported {
                reason: "No per-turn step limit on opencode-serve 2.0.22".to_owned(),
            },
        },
        bounds: vec![BoundMode::Full],
        network_control: false,
        recover: Support::Unsupported {
            reason: "an owned server cannot rejoin an in-flight turn after a daemon restart"
                .to_owned(),
        },
        usage: UsageSupport {
            tokens: "turn".to_owned(),
            cost: "turn".to_owned(),
        },
    }
}

/// §4.5 per category: project configuration is always on, so no request
/// is applied (`unknown`), but skills off, which a session rule applies.
pub(crate) fn categories() -> BTreeMap<Category, CategoryDecl> {
    let unapplied = CategoryDecl {
        on: Switch::Unverified,
        off: Switch::Unverified,
        observed: None,
    };
    BTreeMap::from([
        (Category::Hooks, unapplied),
        (Category::McpServers, unapplied),
        (Category::Plugins, unapplied),
        (
            Category::Skills,
            CategoryDecl {
                off: Switch::Verified,
                ..unapplied
            },
        ),
        (Category::Agents, unapplied),
        (Category::InstructionFiles, unapplied),
    ])
}

/// `tested` for a checked version, `refused` for any other seen, else
/// `untested` (§12).
pub(crate) fn version_status(version: Option<&str>) -> VersionStatus {
    match version {
        Some(version) if CHECKED.contains(&version) => VersionStatus::Tested,
        Some(_) => VersionStatus::Refused,
        None => VersionStatus::Untested,
    }
}

/// The route this adapter serves, as refusals name it.
pub(crate) fn route() -> &'static str {
    Harness::parse(HARNESS).map_or(HARNESS, Harness::route)
}

/// The values one turn sets that the route must accept.
pub(crate) struct PerTurn<'a> {
    pub(crate) effort: Option<&'a str>,
    pub(crate) bound: Option<&'a Bound>,
    pub(crate) output_schema: bool,
    pub(crate) max_steps: bool,
    pub(crate) vendor: &'a VendorOptions,
    pub(crate) vendor_args: &'a VendorArgs,
    pub(crate) sizes: ParamSizes,
}

impl OpenCodeAdapter {
    /// The plan of a spawn or `describe` (C2 §2, §6): a model is taken as
    /// `providerID/id` (the location's catalog judges it at the turn);
    /// there is no default, so a plan naming none is `unknown_model`.
    pub(crate) fn plan(
        &self,
        harness: Harness,
        req: &DescribeRequest,
        requested: Inherit,
    ) -> Result<RoutePlan, Refusal> {
        let route = harness.route();
        let Some(model) = req.model.clone().filter(|model| !model.is_empty()) else {
            return Err(Refusal::new(
                RefusalKind::UnknownModel,
                Some(route),
                format!("route {route} has no default model: name one as provider/id"),
            ));
        };
        let capabilities = capabilities();
        let mut refusals: Vec<Refusal> = model_refusal(route, &model).into_iter().collect();
        refusals.extend(refusals_of(
            route,
            &PerTurn {
                effort: req.effort.as_deref(),
                bound: req.bound.as_ref(),
                output_schema: req.sizes.output_schema > 0,
                max_steps: false,
                vendor: &req.vendor,
                vendor_args: &req.vendor_args,
                sizes: req.sizes,
            },
        ));
        if let Err(verb) = capabilities.require(&req.require) {
            refusals.push(Refusal::new(
                RefusalKind::MissingCapability { verb },
                Some(route),
                format!(
                    "a required verb is not supported natively on route {route}: {}",
                    verb.as_str()
                ),
            ));
        }
        let vendor_version = self.instances.last_version(HARNESS, &self.binary);
        let version_status = if self.refused() {
            refusals.push(handshake_refused(route));
            VersionStatus::Refused
        } else {
            version_status(vendor_version.as_deref())
        };
        let effective_bound = req.bound.clone().filter(|_| {
            !refusals
                .iter()
                .any(|refusal| refusal.kind == RefusalKind::BoundUnsupported)
        });
        let (inherit, switch_warning) = effective_inherit(&categories(), requested);
        Ok(RoutePlan {
            harness: harness.name(),
            model: ModelChoice {
                requested: req.model.clone(),
                resolved: model,
            },
            route,
            adapter_version: ADAPTER_VERSION.to_owned(),
            vendor_version,
            version_status,
            capabilities,
            effective_bound,
            refusals,
            // Core derives `vendor_version_untested` from the status.
            warnings: switch_warning.into_iter().collect(),
            inherit,
            // §3.1: one server for all of VIA; nothing per session.
            server_key: Some(ServerKey::new(self.servers.key_hex())),
        })
    }

    /// A resume turn (§6 `check_turn`): AD12's version, the turn's values,
    /// the session's model, then a cached server refusal. No
    /// location-dependent effort check: `run_turn` judges the variant.
    pub(crate) fn check_turn(
        &self,
        route: &'static str,
        stored_version: &str,
        turn: &TurnParams,
    ) -> Result<TurnCheck, Refusal> {
        if stored_version != ADAPTER_VERSION {
            let mut refusal = Refusal::new(
                RefusalKind::HarnessUnavailable,
                Some(route),
                "the session's adapter version is not compatible with this adapter",
            );
            refusal.reason = Some("adapter_version");
            return Err(refusal);
        }
        let mut refusals = check_values(route, turn);
        if let Some(model) = turn.model.as_deref() {
            refusals.extend(model_refusal(route, model));
        }
        if self.refused() {
            refusals.push(handshake_refused(route));
        }
        match refusals.into_iter().next() {
            Some(refusal) => Err(refusal),
            None => Ok(TurnCheck {
                effective_bound: turn.bound.clone(),
            }),
        }
    }

    /// A server-level handshake refusal of this binary is cached (§2.2).
    fn refused(&self) -> bool {
        self.instances
            .refusal(
                &self.binary,
                &self.servers.refusal_key(),
                std::time::Instant::now(),
            )
            .is_some()
    }
}

/// The refusals of a resume turn's values alone, in C1 member order.
pub(crate) fn check_values(route: &'static str, turn: &TurnParams) -> Vec<Refusal> {
    refusals_of(
        route,
        &PerTurn {
            effort: turn.effort.as_deref(),
            bound: turn.bound.as_ref(),
            output_schema: turn.output_schema,
            max_steps: turn.max_steps.is_some(),
            vendor: &turn.vendor,
            vendor_args: &turn.vendor_args,
            sizes: turn.sizes,
        },
    )
}

/// §6: a model is `providerID/id`, split at the first `/`.
pub(crate) fn model_parts(model: &str) -> Option<(&str, &str)> {
    model
        .split_once('/')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
}

/// A model that is not `providerID/id` names no catalog entry: refused
/// before any receipt.
fn model_refusal(route: &'static str, model: &str) -> Option<Refusal> {
    model_parts(model).is_none().then(|| {
        Refusal::new(
            RefusalKind::InvalidParam { field: "model" },
            Some(route),
            format!("route {route} takes a provider-qualified model, provider/id"),
        )
    })
}

/// C2 §5: a recent handshake of this binary was refused.
fn handshake_refused(route: &'static str) -> Refusal {
    let mut refusal = Refusal::new(
        RefusalKind::VersionRefused,
        Some(route),
        "a recent handshake check of this binary failed on something VIA relies on",
    );
    refusal.reason = Some("handshake_refused");
    refusal
}

/// The refusals of one turn's values, in C1 §4 member order: `prompt`,
/// `effort`, `instructions`, `bound`, `output_schema`, `max_steps`,
/// `vendor`, `vendor_args`.
pub(crate) fn refusals_of(route: &'static str, turn: &PerTurn<'_>) -> Vec<Refusal> {
    let invalid = |field, message: String| {
        Refusal::new(RefusalKind::InvalidParam { field }, Some(route), message)
    };
    let mut refusals = Vec::new();
    if turn.sizes.prompt_json.saturating_add(turn.sizes.cwd_json) > PROMPT_ADMISSION_MAX {
        refusals.push(invalid(
            "prompt",
            format!(
                "the encoded prompt and cwd exceed route {route}'s limit of {PROMPT_ADMISSION_MAX} bytes"
            ),
        ));
    }
    // Any other value is judged against the location's catalog in
    // `run_turn` (§5).
    if turn.effort.is_some_and(str::is_empty) {
        refusals.push(invalid(
            "effort",
            format!("effort is not a value route {route} accepts"),
        ));
    }
    if turn.sizes.instructions_json > INSTRUCTIONS_JSON_MAX {
        refusals.push(invalid(
            "instructions",
            format!("instructions exceed route {route}'s 256 KiB encoded limit"),
        ));
    }
    if let Some(bound) = turn.bound {
        if bound.mode != BoundMode::Full || !bound.network {
            refusals.push(Refusal::new(
                RefusalKind::BoundUnsupported,
                Some(route),
                format!("route {route} enforces only the full bound with network allowed"),
            ));
        } else if !bound.extra_write_dirs.is_empty() {
            refusals.push(invalid(
                "bound",
                format!("route {route} takes no extra_write_dirs: the full bound confines nothing"),
            ));
        }
    }
    if turn.output_schema {
        refusals.push(invalid(
            "output_schema",
            format!("route {route} takes no output_schema"),
        ));
    }
    if turn.max_steps {
        refusals.push(invalid(
            "max_steps",
            format!("route {route} takes no max_steps"),
        ));
    }
    refusals.extend(vendor_refusal(route, turn.vendor));
    // §2.2: every non-empty list, reserved or not.
    if !turn.vendor_args.is_empty() {
        refusals.push(invalid(
            "vendor_args",
            format!("route {route} takes no vendor_args in the first release"),
        ));
    }
    refusals
}

/// C2 §6.1: the vendor-option allow-list is empty. A key naming what the
/// route reserves, in any normalized spelling, is `vendor_option_conflict`;
/// any other key `invalid_params`. Only the `opencode` object is read;
/// neither message echoes the key.
fn vendor_refusal(route: &'static str, vendor: &VendorOptions) -> Option<Refusal> {
    let options = vendor.get(HARNESS)?;
    if options.keys().any(|key| reserved_key(key)) {
        return Some(Refusal::new(
            RefusalKind::VendorOptionConflict { field: "vendor" },
            Some(route),
            format!("a vendor option sets a value route {route} owns"),
        ));
    }
    (!options.is_empty()).then(|| {
        Refusal::new(
            RefusalKind::InvalidParam { field: "vendor" },
            Some(route),
            format!("route {route} accepts no vendor options"),
        )
    })
}

fn reserved_key(key: &str) -> bool {
    let name = passthrough::normalize(key);
    RESERVED.iter().any(|fragment| name.contains(fragment))
}

#[cfg(test)]
mod tests {
    use super::reserved_key;

    /// C2 §6.1: reserved names in any spelling; other keys are not.
    #[test]
    fn reserved_vendor_keys_match_in_any_spelling() {
        for key in [
            "model",
            "Provider_ID",
            "--port",
            "OPENCODE_DB",
            "agent",
            "permissions",
            "XDG_CONFIG_HOME",
        ] {
            assert!(reserved_key(key), "{key}");
        }
        for key in ["anything", "temperature", "x"] {
            assert!(!reserved_key(key), "{key}");
        }
    }
}
