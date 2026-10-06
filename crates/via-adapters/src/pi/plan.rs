//! Pi's pure planning (packet §§3–4.8; C2 §5, §6.1–6.3): capabilities, the
//! `checked` versions, the bundled catalog, the effort values, bound and
//! size refusals, the launch request's cap and vendor-option and raw
//! argument refusals. Reads only bundled data, configuration and the
//! instance cache; starts and writes nothing.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{PiAdapter, launch};
use crate::capabilities::{BoundMode, Capabilities, ParamSupport, Support, UsageSupport, Verbs};
use crate::harness::Harness;
use crate::passthrough::{self, Rules, Takes, VendorArgs};
use crate::plan::{
    Bound, CatalogModel, Category, CategoryDecl, DescribeRequest, Inherit, InheritState,
    ModelChoice, ParamSizes, Refusal, RefusalKind, RoutePlan, Switch, TurnParams, VendorOptions,
    VersionStatus, effective_inherit,
};

/// Versions the maintainers' live check passed (C2 §5): none yet; every
/// version is `untested` (packet §3, owner OD1).
pub(crate) const CHECKED: &[&str] = &[];

/// The values `--thinking` takes (packet §4.5, help 1.0.2).
const EFFORTS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Packet §4.5 (PI-15): the largest admitted prompt, in bytes of its JSON
/// string encoding.
pub(crate) const PROMPT_JSON_MAX: usize = 524_288;

/// Packet §4.5 (PI-15): the largest admitted instructions, in bytes of
/// their JSON string encoding.
pub(crate) const INSTRUCTIONS_JSON_MAX: usize = 262_144;

/// The bundled catalog (packet §2): the live-qualified model,
/// provider-qualified, with no alias: a bare ID is another route's (Codex
/// discovers `gpt-6-luna`), so model-only routing stays unique. The first
/// is the default.
const MODELS: [&str; 1] = ["openai/gpt-6-luna"];

/// This adapter's version (AD12): capabilities belong to it.
pub(crate) fn adapter_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

pub(super) fn catalog() -> Vec<CatalogModel> {
    MODELS
        .iter()
        .map(|model| CatalogModel {
            model: (*model).to_owned(),
            aliases: Vec::new(),
        })
        .collect()
}

/// The capabilities snapshot (packet §3).
fn capabilities() -> Capabilities {
    Capabilities {
        verbs: Verbs {
            spawn: Support::Native,
            resume: Support::Native,
            steer: Support::Unsupported {
                reason: "steer is deferred past the first release".to_owned(),
            },
            cancel: Support::Native,
            close: Support::Native,
        },
        params: ParamSupport {
            instructions: Support::Native,
            output_schema: Support::Unsupported {
                reason: "Pi takes no output schema".to_owned(),
            },
            effort: Support::Native,
            max_steps: Support::Unsupported {
                reason: "Pi has no step limit".to_owned(),
            },
        },
        bounds: vec![BoundMode::Full],
        network_control: false,
        recover: Support::Unsupported {
            reason: "a Pi turn's stdio cannot be rejoined after a daemon restart".to_owned(),
        },
        usage: UsageSupport {
            tokens: "turn".to_owned(),
            cost: "turn".to_owned(),
        },
    }
}

/// AD13 per category (packet §4.6). `-ne` is unconditional, so hooks, MCP
/// servers and plugins are off whatever is requested (verified), as are
/// agents (no loader in Pi 1.0, verified by source); a request for them
/// on warns. Skills and instruction files switch both ways: `-ns`, `-nc`
/// (verified).
pub(crate) fn categories() -> BTreeMap<Category, CategoryDecl> {
    let off = CategoryDecl {
        on: Switch::None,
        off: Switch::Verified,
        observed: Some(InheritState::Off),
    };
    BTreeMap::from([
        (Category::Hooks, off),
        (Category::McpServers, off),
        (Category::Plugins, off),
        (Category::Skills, CategoryDecl::VERIFIED),
        (Category::Agents, off),
        (Category::InstructionFiles, CategoryDecl::VERIFIED),
    ])
}

/// The refusal cache's clock.
pub(super) fn clock() -> std::time::Instant {
    std::time::Instant::now()
}

/// The values a turn sets that the route must accept.
struct PerTurn<'a> {
    effort: Option<&'a str>,
    bound: Option<&'a Bound>,
    output_schema: bool,
    max_steps: Option<u64>,
    vendor: &'a VendorOptions,
    vendor_args: &'a VendorArgs,
    sizes: ParamSizes,
}

impl PiAdapter {
    /// The bundled catalog.
    pub(crate) fn catalog(&self) -> &[CatalogModel] {
        &self.catalog
    }

    /// The catalogued model `requested` names, else `requested` unchanged
    /// (checked at the handshake, packet §2.1); with none, the default.
    pub(crate) fn resolve(&self, requested: Option<&str>) -> String {
        match requested {
            None => MODELS[0].to_owned(),
            Some(name) => self
                .catalog
                .iter()
                .find(|entry| entry.matches(name))
                .map_or(name, |entry| &entry.model)
                .to_owned(),
        }
    }

    /// AD12: the stored version is this adapter's.
    pub(crate) fn check_version(route: &'static str, stored: &str) -> Result<(), Refusal> {
        if stored == adapter_version() {
            return Ok(());
        }
        let mut refusal = Refusal::new(
            RefusalKind::HarnessUnavailable,
            Some(route),
            "the session's adapter version is not compatible with this adapter",
        );
        refusal.reason = Some("adapter_version");
        Err(refusal)
    }

    /// The last version read for `harness` from this program path, and a
    /// handshake refusal cached for `requested`'s recipe with the session's
    /// raw arguments, live at `now` (C2 §5).
    fn version(
        &self,
        harness: Harness,
        (requested, vendor_args): (Inherit, &VendorArgs),
        now: std::time::Instant,
    ) -> (Option<String>, VersionStatus) {
        let version = self.instances.last_version(harness.name(), &self.binary);
        let recipe = launch::recipe_key(requested, vendor_args);
        let status = if self.instances.refusal(&self.binary, &recipe, now).is_some() {
            VersionStatus::Refused
        } else {
            version_status(version.as_deref())
        };
        (version, status)
    }

    pub(crate) fn plan(
        &self,
        harness: Harness,
        req: &DescribeRequest,
        model: ModelChoice,
        requested: Inherit,
    ) -> RoutePlan {
        let route = harness.route();
        let capabilities = capabilities();
        let mut refusals = model_refusal(route, &model.resolved)
            .into_iter()
            .collect::<Vec<_>>();
        refusals.extend(self.clamp_refusal(route, &model.resolved, req.effort.as_deref()));
        refusals.extend(refusals_of(
            route,
            &PerTurn {
                effort: req.effort.as_deref(),
                bound: req.bound.as_ref(),
                output_schema: req.sizes.output_schema > 0,
                max_steps: None,
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
        let (vendor_version, version_status) =
            self.version(harness, (requested, &req.vendor_args), clock());
        if version_status == VersionStatus::Refused {
            refusals.push(handshake_refused(route));
        }
        let effective_bound = req.bound.clone().filter(|_| {
            !refusals
                .iter()
                .any(|r| r.kind == RefusalKind::BoundUnsupported)
        });
        let (inherit, switch_warning) = effective_inherit(&categories(), requested);
        RoutePlan {
            harness: harness.name(),
            model,
            route,
            adapter_version: adapter_version(),
            vendor_version,
            version_status,
            capabilities,
            effective_bound,
            refusals,
            // Core derives `vendor_version_untested` from the version fields.
            warnings: switch_warning.into_iter().collect(),
            inherit,
            // Every Pi turn is its own private process.
            server_key: None,
        }
    }

    /// Every refusal of a turn the route would launch: its values in C1
    /// member order ([`Self::check_values`]), the session's model, an
    /// effort Pi clamped for that model, then a launch request past Host's
    /// cap, then a handshake refusal cached for the session's recipe on
    /// this binary (C2 §5).
    pub(crate) fn check_turn(&self, route: &'static str, turn: &TurnParams) -> Vec<Refusal> {
        let mut refusals = Self::check_values(route, turn);
        if let Some(model) = turn.model.as_deref() {
            refusals.extend(model_refusal(route, model));
            refusals.extend(self.clamp_refusal(route, model, turn.effort.as_deref()));
        }
        refusals.extend(self.frame_refusal(route, turn));
        if let Some(inherit) = turn.inherit
            && self
                .instances
                .refusal(
                    &self.binary,
                    &launch::recipe_key(inherit, &turn.vendor_args),
                    clock(),
                )
                .is_some()
        {
            refusals.push(handshake_refused(route));
        }
        refusals
    }

    /// The refusals of a turn's values alone, in C1 member order.
    pub(crate) fn check_values(route: &'static str, turn: &TurnParams) -> Vec<Refusal> {
        refusals_of(
            route,
            &PerTurn {
                effort: turn.effort.as_deref(),
                bound: turn.bound.as_ref(),
                output_schema: turn.output_schema,
                max_steps: turn.max_steps,
                vendor: &turn.vendor,
                vendor_args: &turn.vendor_args,
                sizes: turn.sizes,
            },
        )
    }

    /// Packet §4.5: Pi clamped `effort` for `model` on this binary at a
    /// recent handshake (`get_state.thinkingLevel`), so the same request is
    /// refused before any receipt while the cache holds it.
    fn clamp_refusal(
        &self,
        route: &'static str,
        model: &str,
        effort: Option<&str>,
    ) -> Option<Refusal> {
        let key = launch::clamp_key(model, effort?);
        self.instances.refusal(&self.binary, &key, clock())?;
        Some(Refusal::new(
            RefusalKind::InvalidParam { field: "effort" },
            Some(route),
            format!("route {route} recently applied a different effort for this model"),
        ))
    }

    /// C2 §6.3: the turn's launch request (Host's `Configure` frame:
    /// binary, argv, `cwd`, environment and Host's marker) past the cap
    /// the anchor reads under is `invalid_params`, naming `vendor_args`
    /// when the session passes any, else `cwd`. The instructions go by
    /// file (packet §4.5), so only paths, the model and the session's raw
    /// arguments stand in, at their largest encoding.
    fn frame_refusal(&self, route: &'static str, turn: &TurnParams) -> Option<Refusal> {
        let widest = |len: usize| "z".repeat(len);
        let model = widest(turn.sizes.model.max(1));
        let session = "f".repeat(36);
        let state = self.vendor_state_dir.join("pi");
        let session_dir = state.join("sessions").join(widest(32));
        let instructions = state.join("instructions").join(widest(32));
        let recipe = launch::Recipe {
            model: &model,
            thinking: turn.effort.as_deref(),
            session_dir: &session_dir,
            session: launch::Continue::New(&session),
            inherit: turn.inherit.unwrap_or(Inherit::OD2_DEFAULT),
            instructions: turn.instructions.then_some(instructions.as_path()),
            vendor_args: turn.vendor_args.as_slice(),
        };
        let args = launch::argv(&recipe);
        let cwd = PathBuf::from(widest(turn.sizes.cwd));
        if crate::PrivateProcessSpec::configure_fits(&self.binary, &args, &cwd, &self.env_list()) {
            return None;
        }
        let field = if turn.vendor_args.is_empty() {
            "cwd"
        } else {
            "vendor_args"
        };
        Some(Refusal::new(
            RefusalKind::InvalidParam { field },
            Some(route),
            format!(
                "the launch's arguments together exceed route {route}'s 64 KiB launch request limit"
            ),
        ))
    }
}

/// `untested` unless `version` is a checked one (C2 §5).
pub(super) fn version_status(version: Option<&str>) -> VersionStatus {
    if version.is_some_and(|version| CHECKED.contains(&version)) {
        VersionStatus::Tested
    } else {
        VersionStatus::Untested
    }
}

/// Packet §4.5: Pi takes the model exactly as `provider/id`; a model
/// without a provider could only be fuzzy-matched (E30), so it is refused
/// before any receipt.
fn model_refusal(route: &'static str, model: &str) -> Option<Refusal> {
    let qualified = model
        .split_once('/')
        .is_some_and(|(provider, id)| !provider.is_empty() && !id.is_empty());
    (!qualified).then(|| {
        Refusal::new(
            RefusalKind::InvalidParam { field: "model" },
            Some(route),
            format!("route {route} takes a provider-qualified model, provider/id"),
        )
    })
}

/// C2 §5: a recent handshake check of the binary failed for the recipe.
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
fn refusals_of(route: &'static str, turn: &PerTurn<'_>) -> Vec<Refusal> {
    let invalid = |field, message: String| {
        Refusal::new(RefusalKind::InvalidParam { field }, Some(route), message)
    };
    let mut refusals = Vec::new();
    if turn.sizes.prompt_json > PROMPT_JSON_MAX {
        refusals.push(invalid(
            "prompt",
            format!("the prompt exceeds route {route}'s 512 KiB encoded limit"),
        ));
    }
    if let Some(effort) = turn.effort
        && !EFFORTS.contains(&effort)
    {
        refusals.push(invalid(
            "effort",
            format!(
                "effort is not a value route {route} accepts: off, minimal, low, medium, high, \
                 xhigh or max"
            ),
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
                format!(
                    "route {route} enforces only the full bound with network allowed: \
                     Pi has no sandbox or permission mode"
                ),
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
    if turn.max_steps.is_some() {
        refusals.push(invalid(
            "max_steps",
            format!("route {route} takes no max_steps"),
        ));
    }
    refusals.extend(vendor_refusal(route, turn.vendor));
    refusals.extend(args_refusal(route, turn.vendor_args));
    refusals
}

/// C2 §6.3: the session's raw arguments, judged against [`ARG_RULES`];
/// the message names the argument's index, never its text.
fn args_refusal(route: &'static str, vendor_args: &VendorArgs) -> Option<Refusal> {
    let index = passthrough::conflict(vendor_args.as_slice(), &ARG_RULES)?;
    Some(Refusal::new(
        RefusalKind::VendorOptionConflict {
            field: "vendor_args",
        },
        Some(route),
        format!("vendor_args[{index}] sets what route {route} owns"),
    ))
}

/// The raw-argument rules (C2 §6.3, packet §4.8, the pinned `pi --help`
/// of 1.0.2). Pi's parser matches each option as an exact string, its
/// short forms multi-letter (`-ne`, `-nbt`), so no single-dash element is
/// a cluster of switches: every one is refused. Pi does not split
/// `--name=value` (it keeps it as an unknown flag); the matching judges it
/// by its name all the same, so a reserved name is refused in either
/// spelling. The unreserved options taking a value are `--use-theme` and
/// `--tui-mode`.
const ARG_RULES: Rules = Rules {
    long_reserved: |name| RESERVED_NAMES.contains(&name),
    short_reserved: |_| true,
    long_value: |name| match name {
        "usetheme" | "tuimode" => Some(Takes::One),
        _ => None,
    },
    short_value: |_| None,
    value_reserved: |_, _| false,
};

/// Normalized long names the route reserves (packet §4.8, help 1.0.2):
/// every recipe flag and its negations and opposites, VIA's canonical
/// parameters, and the options that change the mode, the session, the
/// model, the tools, the loaded resources or exit at once
/// (`--list-models`, `--export`, `--help`, `--version`).
const RESERVED_NAMES: [&str; 36] = [
    "provider",
    "model",
    "apikey",
    "systemprompt",
    "appendsystemprompt",
    "mode",
    "print",
    "continue",
    "resume",
    "session",
    "sessionid",
    "fork",
    "sessiondir",
    "nosession",
    "name",
    "models",
    "notools",
    "nobuiltintools",
    "tools",
    "excludetools",
    "thinking",
    "extension",
    "noextensions",
    "skill",
    "noskills",
    "prompttemplate",
    "noprompttemplates",
    "theme",
    "nocontextfiles",
    "export",
    "listmodels",
    "approve",
    "noapprove",
    "offline",
    "help",
    "version",
];

/// Pi's short forms (help 1.0.2): reserved as `vendor` keys too.
const RESERVED_SHORT: [&str; 17] = [
    "p", "c", "r", "n", "nt", "nbt", "t", "xt", "e", "ne", "ns", "np", "nc", "a", "na", "h", "v",
];

/// Packet §4.8, C2 §6.1: the vendor-option allow-list is empty. A key
/// naming a recipe flag, a short form, or a `PI_*` environment name, in
/// any normalized spelling, is `vendor_option_conflict`; any other key
/// `invalid_params`. Only the `pi` object is read; neither message echoes
/// the key.
fn vendor_refusal(route: &'static str, vendor: &VendorOptions) -> Option<Refusal> {
    let options = vendor.get(super::HARNESS)?;
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
    let environment = key
        .trim_start_matches('-')
        .get(..3)
        .is_some_and(|head| head.eq_ignore_ascii_case("pi_"));
    let name = passthrough::normalize(key);
    environment
        || RESERVED_NAMES.contains(&name.as_str())
        || RESERVED_SHORT.contains(&name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> VendorArgs {
        VendorArgs::try_from(list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>()).unwrap()
    }

    /// Packet §4.8: the recipe's flags, their opposites and every
    /// single-dash element are refused; the unreserved options pass, a
    /// value option taking exactly its next element.
    #[test]
    fn vendor_args_follow_the_pinned_help() {
        let refused = |list: &[&str]| args_refusal("pi-rpc", &args(list)).is_some();
        for list in [
            &["--approve"][..],
            &["--thinking", "high"],
            &["--session-dir=/x"],
            &["--list-models"],
            &["--no-context-files"],
            &["-a"],
            &["-ns"],
            &["-x"],
            &["--"],
            &["message"],
            &["--use-theme", "--verbose"],
            &["--Model", "m"],
        ] {
            assert!(refused(list), "{list:?} passed");
        }
        for list in [
            &["--verbose"][..],
            &["--no-themes"],
            &["--use-theme", "dark"],
            &["--tui-mode=regular"],
            &[],
        ] {
            assert!(!refused(list), "{list:?} refused");
        }
    }

    /// Packet §4.8: vendor keys naming a recipe flag, a short form or a
    /// `PI_*` name conflict; others are invalid.
    #[test]
    fn vendor_keys_are_reserved_or_invalid() {
        for key in [
            "--provider",
            "approve",
            "noApprove",
            "ne",
            "PI_OFFLINE",
            "pi_telemetry",
        ] {
            assert!(reserved_key(key), "{key}");
        }
        for key in ["zz-option", "verbose", "pilot"] {
            assert!(!reserved_key(key), "{key}");
        }
    }

    /// Packet §4.5: only `provider/id` launches.
    #[test]
    fn models_are_provider_qualified() {
        assert!(model_refusal("pi-rpc", "openai/gpt-6-luna").is_none());
        for model in ["gpt-6-luna", "/x", "openai/"] {
            assert!(model_refusal("pi-rpc", model).is_some(), "{model}");
        }
    }

    /// Packet §4.6: hooks, MCP servers, plugins and agents are off however
    /// requested; skills and instruction files follow the request.
    #[test]
    fn categories_follow_the_recipe() {
        let mut on = Inherit::OD2_DEFAULT;
        for category in Category::ALL {
            on.set(category, InheritState::On);
        }
        let (plan, warning) = effective_inherit(&categories(), on);
        assert_eq!(plan.effective.get(Category::Hooks), InheritState::Off);
        assert_eq!(plan.effective.get(Category::Agents), InheritState::Off);
        assert_eq!(plan.effective.get(Category::Skills), InheritState::On);
        assert_eq!(
            plan.effective.get(Category::InstructionFiles),
            InheritState::On
        );
        assert!(warning.is_some());
        let mut off = on;
        for category in Category::ALL {
            off.set(category, InheritState::Off);
        }
        let (plan, warning) = effective_inherit(&categories(), off);
        assert_eq!(plan.effective, off);
        assert!(warning.is_none());
    }
}
