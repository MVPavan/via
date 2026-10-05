//! Claude's pure planning (packet §§3–4; C2 §5, §6.1; AD12, AD13, AD18):
//! capabilities, the `checked` versions, the bundled catalog, the effort
//! table, bound refusals, the argument budget (x.3.2 G8) and vendor-option
//! refusals. Reads only bundled data, configuration and the instance
//! cache, after one `stat` of the binary; starts and writes nothing.

use std::collections::BTreeMap;

use super::{ClaudeAdapter, launch};
use crate::capabilities::{BoundMode, Capabilities, ParamSupport, Support, UsageSupport, Verbs};
use crate::config::ClaudeMode;
use crate::harness::Harness;
use crate::plan::{
    Bound, CatalogModel, Category, CategoryDecl, DescribeRequest, Inherit, InheritState,
    ModelChoice, ParamSizes, Refusal, RefusalKind, RoutePlan, Switch, TurnParams, VendorOptions,
    VersionStatus, effective_inherit,
};

/// Versions the maintainers' live check passed (C2 §5): the 2026-09-30
/// re-probe's and the 2026-10-05 live round's.
pub(crate) const CHECKED: &[&str] = &["2.1.285", "2.1.289"];

/// The efforts `--effort` accepts (packet §4, help 2.1.285). Claude ignores
/// any other with only a stderr warning, so VIA refuses it (AD18).
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Linux's per-argument limit (`MAX_ARG_STRLEN`, 32 pages) less the NUL:
/// the longest `--append-system-prompt` or `--json-schema` value (G8).
pub(crate) const ARG_MAX: usize = 128 * 1024 - 1;

/// The bundled catalog (packet §2): Claude Code's model aliases, which the
/// vendor resolves at init. The first is the default.
const MODELS: [&str; 3] = ["sonnet", "opus", "haiku"];

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
                reason: "a busy Claude Code process merges input into the running turn".to_owned(),
            },
            cancel: Support::Partial {
                semantics: "aborts_tools_then_result".to_owned(),
            },
            close: Support::Native,
        },
        params: ParamSupport {
            instructions: Support::Native,
            output_schema: Support::Native,
            effort: Support::Native,
            max_steps: Support::Partial {
                semantics: "agentic_turn_limit".to_owned(),
            },
        },
        bounds: vec![BoundMode::Full],
        network_control: false,
        recover: Support::Unsupported {
            reason: "a Claude Code turn's stdio cannot be rejoined after a daemon restart"
                .to_owned(),
        },
        usage: UsageSupport {
            tokens: "turn".to_owned(),
            cost: "reported_cumulative".to_owned(),
        },
    }
}

/// AD13 per category (packet §4, design §5.4.1), as the configured mode
/// delivers them (via-umz; live rounds of 2026-10-05). Only the MCP switch
/// is applied per category (`--strict-mcp-config`, verified). The others
/// have no per-category switch: `--restricted` turns hooks, instruction
/// files and the user's and project's plugins, skills and agents off
/// together (built-ins only, verified); without it every one of them
/// loads (verified). MCP servers without the switch load in the default
/// mode (verified); under `--restricted` what loads varied between live
/// launches (none, or the account's claude.ai connectors), so they are
/// `unknown`. A request the mode cannot deliver warns.
pub(crate) fn categories(mode: ClaudeMode) -> BTreeMap<Category, CategoryDecl> {
    let loaded = match mode {
        ClaudeMode::Unrestricted => InheritState::On,
        ClaudeMode::Restricted => InheritState::Off,
    };
    let unswitched = CategoryDecl {
        on: Switch::None,
        off: Switch::None,
        observed: Some(loaded),
    };
    let mcp_loaded = match mode {
        ClaudeMode::Unrestricted => Some(InheritState::On),
        ClaudeMode::Restricted => None,
    };
    BTreeMap::from([
        (Category::Hooks, unswitched),
        (
            Category::McpServers,
            CategoryDecl {
                on: Switch::None,
                off: Switch::Verified,
                observed: mcp_loaded,
            },
        ),
        (Category::Plugins, unswitched),
        (Category::Skills, unswitched),
        (Category::Agents, unswitched),
        (Category::InstructionFiles, unswitched),
    ])
}

/// The refusal cache's clock: a plan reads entries and the driver writes
/// refusals at its instant. Test builds: while the failpoint
/// `adapter.claude.plan_clock_ms` (a `value`) is armed, the clock is
/// frozen at a fixed origin (the real time of its first armed reading in
/// this process) plus that many milliseconds, so a refusal's expiry runs
/// under controlled time that real elapsed time never moves (x.3.2 C3).
pub(super) fn clock() -> std::time::Instant {
    let now = std::time::Instant::now();
    #[cfg(feature = "test-failpoints")]
    if let Ok(Some(ahead)) = via_routes::failpoint::value("adapter.claude.plan_clock_ms") {
        static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let origin = *ORIGIN.get_or_init(|| now);
        return origin
            .checked_add(std::time::Duration::from_millis(ahead))
            .unwrap_or(origin);
    }
    now
}

/// The values a turn sets that the route must accept.
struct PerTurn<'a> {
    effort: Option<&'a str>,
    bound: Option<&'a Bound>,
    vendor: &'a VendorOptions,
    sizes: ParamSizes,
}

impl ClaudeAdapter {
    /// The bundled catalog.
    pub(crate) fn catalog(&self) -> &[CatalogModel] {
        &self.catalog
    }

    /// The catalogued model `requested` names, else `requested` unchanged
    /// for the vendor to judge (design §5.2); with none, the default.
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

    /// The last version an init reported for `harness` run from this
    /// program path, and a handshake refusal for `requested`'s recipe
    /// cached and live at `now` (C2 §5).
    fn version(
        &self,
        harness: Harness,
        requested: Inherit,
        schema: bool,
        now: std::time::Instant,
    ) -> (Option<String>, VersionStatus) {
        let version = self.instances.last_version(harness.name(), &self.binary);
        let recipe = launch::recipe_key(self.mode, requested, schema);
        let status = if self.instances.refusal(&self.binary, &recipe, now).is_some() {
            VersionStatus::Refused
        } else if version
            .as_deref()
            .is_some_and(|version| CHECKED.contains(&version))
        {
            VersionStatus::Tested
        } else {
            VersionStatus::Untested
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
        self.plan_at(harness, req, model, requested, clock())
    }

    /// [`Self::plan`] with its clock at `now`: the refusal cache's entries
    /// are live or expired as of `now`.
    fn plan_at(
        &self,
        harness: Harness,
        req: &DescribeRequest,
        model: ModelChoice,
        requested: Inherit,
        now: std::time::Instant,
    ) -> RoutePlan {
        let route = harness.route();
        let capabilities = capabilities();
        let mut refusals = refusals(
            route,
            &PerTurn {
                effort: req.effort.as_deref(),
                bound: req.bound.as_ref(),
                vendor: &req.vendor,
                sizes: req.sizes,
            },
        );
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
            self.version(harness, requested, req.sizes.output_schema > 0, now);
        if version_status == VersionStatus::Refused {
            refusals.push(handshake_refused(route));
        }
        let effective_bound = req.bound.clone().filter(|_| {
            !refusals
                .iter()
                .any(|r| r.kind == RefusalKind::BoundUnsupported)
        });
        let (inherit, switch_warning) = effective_inherit(&categories(self.mode), requested);
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
            // Every Claude turn is its own private process.
            server_key: None,
        }
    }

    /// Every refusal of a turn the route would launch, spawn's turn 1 and
    /// a resume's alike: its values in C1 member order
    /// ([`Self::check_values`]), then a launch request past Host's cap
    /// ([`Self::frame_refusal`]), then a handshake refusal cached for the
    /// session's recipe on this binary (C2 §5; critical r1 #1, #2).
    pub(crate) fn check_turn(&self, route: &'static str, turn: &TurnParams) -> Vec<Refusal> {
        self.check_turn_at(route, turn, clock())
    }

    /// [`Self::check_turn`] with the refusal cache read at `now`.
    fn check_turn_at(
        &self,
        route: &'static str,
        turn: &TurnParams,
        now: std::time::Instant,
    ) -> Vec<Refusal> {
        let mut refusals = Self::check_values(route, turn);
        refusals.extend(self.frame_refusal(route, turn));
        // The session's mode, read back from its frozen states; the
        // configured one where they are unknown.
        let mode = turn
            .inherit_effective
            .map_or(self.mode, launch::session_mode);
        if let Some(inherit) = turn.inherit
            && self
                .instances
                .refusal(
                    &self.binary,
                    &launch::recipe_key(mode, inherit, turn.output_schema),
                    now,
                )
                .is_some()
        {
            refusals.push(handshake_refused(route));
        }
        refusals
    }

    /// The refusals of a turn's values alone, in C1 member order.
    pub(crate) fn check_values(route: &'static str, turn: &TurnParams) -> Vec<Refusal> {
        refusals(
            route,
            &PerTurn {
                effort: turn.effort.as_deref(),
                bound: turn.bound.as_ref(),
                vendor: &turn.vendor,
                sizes: turn.sizes,
            },
        )
    }

    /// Critical r1 #1: the turn's launch request (Host's `Configure`
    /// frame: binary, argv, `cwd`, environment and Host's marker) past the
    /// cap the anchor reads under is `invalid_params`, naming the larger of
    /// `instructions` and `output_schema` (`bound`, its extra directories,
    /// when neither is set), so nothing is accepted that Host cannot start.
    ///
    /// Host computes the encoded size ([`crate::PrivateProcessSpec::configure_fits`]).
    /// The values known only by size (C2 §2) stand in at their largest
    /// encoding: every byte of the instructions, schema, `cwd` and model as
    /// one of three digits, the session flag the longer `--session-id` with
    /// such an ID, and the MCP switch present when `inherit` is unknown. So
    /// spawn and resume judge the same session alike, and a launch that
    /// passes always fits; one near the cap may be refused that would fit.
    fn frame_refusal(&self, route: &'static str, turn: &TurnParams) -> Option<Refusal> {
        let widest = |len: usize| "z".repeat(len);
        let model = widest(turn.sizes.model);
        let session = "f".repeat(36);
        let instructions = widest(turn.sizes.instructions);
        // A JSON string of the schema's length, quotes besides, parses.
        let schema = serde_json::value::RawValue::from_string(format!(
            "\"{}\"",
            widest(turn.sizes.output_schema)
        ))
        .ok()?;
        let extra_write_dirs = turn
            .bound
            .as_ref()
            .map_or(&[][..], |bound| bound.extra_write_dirs.as_slice());
        let recipe = launch::Recipe {
            model: &model,
            session: launch::Continue::New(&session),
            // The longer argv, whatever the session's mode.
            mode: ClaudeMode::Restricted,
            // Unknown: OD2's default, MCP servers off, passes the switch
            // (the longer argv).
            inherit: turn.inherit.unwrap_or(Inherit::OD2_DEFAULT),
            extra_write_dirs,
            // Present instructions pass the flag, empty ones too (critical r2 #1).
            instructions: turn.instructions.then_some(instructions.as_str()),
            effort: turn.effort.as_deref(),
            output_schema: turn.output_schema.then_some(&*schema),
            max_steps: turn.max_steps,
        };
        let args = launch::argv(&recipe).ok()?;
        let cwd = std::path::PathBuf::from(widest(turn.sizes.cwd));
        if crate::PrivateProcessSpec::configure_fits(&self.binary, &args, &cwd, &self.env_list()) {
            return None;
        }
        let field = match (turn.sizes.instructions, turn.sizes.output_schema) {
            (0, 0) => "bound",
            (instructions, schema) if schema > instructions => "output_schema",
            _ => "instructions",
        };
        Some(Refusal::new(
            RefusalKind::InvalidParam { field },
            Some(route),
            format!(
                "the launch's arguments together, instructions and output_schema \
                 included, exceed route {route}'s 64 KiB launch request limit"
            ),
        ))
    }
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

/// The refusals of one turn's values, in C1 §4 member order: `effort`,
/// `instructions`, `bound`, `output_schema`, `vendor`.
fn refusals(route: &'static str, turn: &PerTurn<'_>) -> Vec<Refusal> {
    let invalid = |field, message: String| {
        Refusal::new(RefusalKind::InvalidParam { field }, Some(route), message)
    };
    let mut refusals = Vec::new();
    if let Some(effort) = turn.effort
        && !EFFORTS.contains(&effort)
    {
        refusals.push(invalid(
            "effort",
            format!("effort is not a value route {route} accepts: low, medium, high, xhigh or max"),
        ));
    }
    if turn.sizes.instructions > ARG_MAX {
        refusals.push(invalid(
            "instructions",
            format!("instructions exceed route {route}'s 128 KiB argument limit"),
        ));
    }
    if let Some(bound) = turn.bound
        && (bound.mode != BoundMode::Full || !bound.network)
    {
        refusals.push(Refusal::new(
            RefusalKind::BoundUnsupported,
            Some(route),
            format!(
                "route {route} enforces only the full bound with network allowed; \
                 read_only, workspace_write and network:false await CLAUDE-BOUND-1"
            ),
        ));
    }
    if turn.sizes.output_schema > ARG_MAX {
        refusals.push(invalid(
            "output_schema",
            format!("output_schema exceeds route {route}'s 128 KiB argument limit"),
        ));
    }
    refusals.extend(vendor_refusal(route, turn.vendor));
    refusals
}

/// Packet §4, C2 §6.1: the route takes no free-form vendor option. A key
/// naming a flag, setting or override the recipe owns, in any normalized
/// spelling, is `vendor_option_conflict`; any other key `invalid_params`.
/// Only the `claude` object is read; neither message echoes the key.
fn vendor_refusal(route: &'static str, vendor: &VendorOptions) -> Option<Refusal> {
    let options = vendor.get(super::HARNESS)?;
    if options.keys().any(|key| reserved(key)) {
        return Some(Refusal::new(
            RefusalKind::VendorOptionConflict,
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

/// Normalized prefixes of the flag, setting and environment-override
/// families the recipe owns (C2 §6.1, packet §4): each names several
/// spellings or members (`permissionMode`, `permissionPromptTool`, …;
/// `ANTHROPIC_*`, `CLAUDE_CODE_*`).
const RESERVED_PREFIXES: [&str; 19] = [
    "permission",
    "dangerously",
    "allowdangerously",
    "allowedtool",
    "disallowedtool",
    "adddir",
    "systemprompt",
    "appendsystemprompt",
    "maxturn",
    "includepartial",
    "replayuser",
    "setting",
    "agent",
    "mcp",
    "plugin",
    "disableslashcommand",
    "claudeconfig",
    "anthropic",
    "claudecode",
];

/// Normalized names matched exactly (review r2 #7, r3 #3 and #4): the
/// recipe's singleton flags, the launch environment's variables (the
/// known locale variables included), and VIA's canonical parameters.
const RESERVED_NAMES: [&str; 48] = [
    "env",
    "environment",
    "tool",
    "tools",
    "resume",
    "sessionid",
    "continue",
    "forksession",
    "model",
    "fallbackmodel",
    "effort",
    "jsonschema",
    "inputformat",
    "outputformat",
    "print",
    "verbose",
    "bare",
    "restricted",
    "safemode",
    "strictmcpconfig",
    "nosessionpersistence",
    "sessionpersistence",
    "worktree",
    "configdir",
    "lcall",
    "lcctype",
    "lccollate",
    "lcmessages",
    "lcmonetary",
    "lcnumeric",
    "lctime",
    "home",
    "path",
    "lang",
    "cwd",
    "prompt",
    "instructions",
    "outputschema",
    "maxsteps",
    "bound",
    "extrawritedirs",
    "inherit",
    "harness",
    "session",
    "vendor",
    "require",
    "handle",
    "allowuntested",
];

/// Single-letter flags the recipe owns: `-p`, `-r` and `-c`.
const RESERVED_SHORT: [&str; 3] = ["p", "r", "c"];

/// A key without leading dashes, lowercased, with `-`, `_` and `.`
/// removed: `--permission-mode`, `permissionMode` and `PERMISSION_MODE`
/// all normalize to `permissionmode`.
fn normalize(key: &str) -> String {
    key.trim_start_matches('-')
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | '.'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn reserved(key: &str) -> bool {
    // The locale family, `LC_*`, before normalization drops its `_`.
    let locale = key
        .trim_start_matches('-')
        .get(..3)
        .is_some_and(|head| head.eq_ignore_ascii_case("lc_"));
    let key = normalize(key);
    locale
        || RESERVED_SHORT.contains(&key.as_str())
        || RESERVED_NAMES.contains(&key.as_str())
        || RESERVED_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request: each category `on` or `off`, all 64 combinations.
    fn every_request() -> impl Iterator<Item = Inherit> {
        (0..64_u8).map(|bits| {
            let mut inherit = Inherit::OD2_DEFAULT;
            for (index, category) in Category::ALL.into_iter().enumerate() {
                let on = bits & (1 << index) != 0;
                inherit.set(
                    category,
                    if on {
                        InheritState::On
                    } else {
                        InheritState::Off
                    },
                );
            }
            inherit
        })
    }

    fn uniform(state: InheritState) -> Inherit {
        let mut inherit = Inherit::OD2_DEFAULT;
        for category in Category::ALL {
            inherit.set(category, state);
        }
        inherit
    }

    /// The categories a `config_switch_unverified` warning lists.
    fn warned(warning: Option<&crate::plan::Warning>) -> Vec<String> {
        warning
            .and_then(|warning| warning.data.as_ref())
            .and_then(|data| data["categories"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|entry| entry["category"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// via-umz: the effective states are what each mode delivers (the
    /// 2026-10-05 live round): restricted loads no hooks, instruction
    /// files, user plugins, skills or agents; unrestricted loads them all,
    /// and its MCP servers too (verified). Restricted MCP servers without
    /// `--strict-mcp-config` varied live, so are `unknown`; the switch
    /// turns them off in both modes. Claude's default request is what the default mode
    /// delivers, MCP servers on (owner, 2026-10-05), so it never warns; a
    /// request a mode cannot deliver warns, listing each such category.
    #[test]
    fn categories_report_each_mode_truthfully() {
        use InheritState::{Off, On, Unknown};
        let harness = Harness::Vendor(&crate::harness::HARNESSES[0]);
        let default =
            crate::config::AdapterConfig::load(crate::config::BootstrapEnv::default(), None)
                .unwrap()
                .inherit(harness);
        assert_eq!(default, uniform(On), "Claude's default request");
        let plan = |mode, requested| effective_inherit(&categories(mode), requested);

        let (inherit, warning) = plan(ClaudeMode::Unrestricted, default);
        assert_eq!(inherit.effective, default);
        assert!(warning.is_none(), "{warning:?}");

        let (inherit, warning) = plan(ClaudeMode::Restricted, uniform(Off));
        assert_eq!(inherit.effective, uniform(Off));
        assert!(warning.is_none(), "{warning:?}");

        let rest = ["hooks", "plugins", "skills", "agents", "instruction_files"];
        let (inherit, warning) = plan(ClaudeMode::Restricted, default);
        let mut restricted = uniform(Off);
        restricted.set(Category::McpServers, Unknown);
        assert_eq!(inherit.effective, restricted);
        assert_eq!(
            warned(warning.as_ref()),
            [
                "hooks",
                "mcp_servers",
                "plugins",
                "skills",
                "agents",
                "instruction_files"
            ]
        );

        let (inherit, warning) = plan(ClaudeMode::Unrestricted, uniform(Off));
        let mut unrestricted = uniform(On);
        unrestricted.set(Category::McpServers, Off);
        assert_eq!(inherit.effective, unrestricted);
        assert_eq!(warned(warning.as_ref()), rest);
    }

    /// A session's mode is frozen by its effective states: for every
    /// request, the states a mode's plan freezes read back as that mode,
    /// so a launch reproduces them whatever the configuration says now.
    /// A session frozen before the mode existed (hooks `unknown`) was
    /// launched with `--restricted`.
    #[test]
    fn frozen_states_name_the_session_mode() {
        for mode in [ClaudeMode::Unrestricted, ClaudeMode::Restricted] {
            for requested in every_request() {
                let (inherit, _) = effective_inherit(&categories(mode), requested);
                assert_eq!(
                    launch::session_mode(inherit.effective),
                    mode,
                    "{requested:?}"
                );
            }
        }
        let mut legacy = Inherit::OD2_DEFAULT;
        legacy.set(Category::Hooks, InheritState::Unknown);
        assert_eq!(launch::session_mode(legacy), ClaudeMode::Restricted);
    }

    /// Normalized spellings of one owned flag are all reserved; other keys
    /// are not.
    #[test]
    fn reserved_keys_are_normalized() {
        for key in [
            "--permission-mode",
            "permissionMode",
            "PERMISSION_MODE",
            "permission.mode",
            "-p",
            "P",
            "--append-system-prompt-file",
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_MODEL",
        ] {
            assert!(reserved(key), "{key}");
        }
        for key in [
            "--debug",
            "temperature",
            "pp",
            "x",
            "pathology",
            "language",
            "boundary",
            "promptly",
            "homepage",
            "lcd",
            "modeling",
            "effortless",
            "printer",
            "barely",
            "toolbox",
            "envoy",
        ] {
            assert!(!reserved(key), "{key}");
        }
    }

    /// Review r1 #11, specified apart from the prefix table: the canonical
    /// parameters and the launch environment's names are owned, in their
    /// normalized spellings.
    #[test]
    fn owned_names_are_reserved() {
        for key in [
            "PATH",
            "path",
            "LANG",
            "lang",
            "env",
            "ENVIRONMENT",
            "LC_ALL",
            "lcAll",
            "lc.all",
            "--lc-all",
            "LC_CTYPE",
            "HOME",
            "instructions",
            "INSTRUCTIONS",
            "output_schema",
            "outputSchema",
            "--output-schema",
            "max_steps",
            "maxSteps",
            "bound",
            "prompt",
            "cwd",
            "extra_write_dirs",
            "extraWriteDirs",
            "inherit",
            "harness",
            "model",
            "effort",
            "session",
            "session_id",
            "vendor",
        ] {
            assert!(reserved(key), "{key}");
        }
    }

    /// Review r1 #8: a handshake refusal cached for the schema recipe does
    /// not refuse the plain recipe, and the other way round.
    #[test]
    fn refusal_cache_keys_on_the_schema_mode() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("claude");
        std::fs::write(&binary, b"").unwrap();
        let instances = std::sync::Arc::new(crate::instance::InstanceCache::default());
        let adapter = ClaudeAdapter::new(
            binary.clone(),
            instances.clone(),
            &crate::config::BootstrapEnv::from_vars::<_, &str, &str>([]),
            ClaudeMode::Unrestricted,
        );
        let requested = Inherit::OD2_DEFAULT;
        instances.record_refusal(
            &binary,
            launch::recipe_key(ClaudeMode::Unrestricted, requested, true),
            crate::instance::Incompatibility::ReadbackDiffers("tools"),
            std::time::Instant::now(),
        );
        let harness = Harness::Vendor(&crate::harness::HARNESSES[0]);
        let now = std::time::Instant::now();
        assert_eq!(
            adapter.version(harness, requested, true, now).1,
            VersionStatus::Refused
        );
        assert_ne!(
            adapter.version(harness, requested, false, now).1,
            VersionStatus::Refused
        );
    }

    /// An adapter over a fresh instance cache, its binary in `dir`.
    fn adapter_in(
        dir: &std::path::Path,
    ) -> (
        ClaudeAdapter,
        std::sync::Arc<crate::instance::InstanceCache>,
        std::path::PathBuf,
    ) {
        let binary = dir.join("claude");
        std::fs::write(&binary, b"").unwrap();
        let instances = std::sync::Arc::new(crate::instance::InstanceCache::default());
        let adapter = ClaudeAdapter::new(
            binary.clone(),
            instances.clone(),
            &crate::config::BootstrapEnv::from_vars::<_, &str, &str>([]),
            ClaudeMode::Unrestricted,
        );
        (adapter, instances, binary)
    }

    /// Critical r1 #1: the cap Host's anchor reads its `Configure` frame
    /// under, through `check_turn` and Host's own encoding. The largest
    /// instructions `check_turn` admits launch: the real recipe's process
    /// (its UUID, model and `cwd`) fits Host's cap, and one byte more is
    /// refused `invalid_params` naming `instructions`. The bound is tight
    /// to the stand-ins' slack: the real launch 11 bytes past it does not
    /// fit. Instructions and a schema that each fit alone are refused
    /// together, naming the larger; under the per-argument limit all.
    #[test]
    fn launch_request_past_host_cap_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("claude");
        let adapter = ClaudeAdapter::new(
            binary,
            std::sync::Arc::new(crate::instance::InstanceCache::default()),
            &crate::config::BootstrapEnv::from_vars([
                ("HOME", "/home/user"),
                ("PATH", "/usr/bin:/bin"),
                ("LANG", "C.UTF-8"),
            ]),
            ClaudeMode::Unrestricted,
        );
        let cwd = std::path::Path::new("/work/project");
        let model = adapter.resolve(None);
        let turn = |instructions: usize, output_schema: usize| TurnParams {
            output_schema: output_schema > 0,
            instructions: instructions > 0,
            sizes: ParamSizes {
                instructions,
                output_schema,
                cwd: cwd.as_os_str().len(),
                model: model.len(),
                ..ParamSizes::default()
            },
            inherit: Some(Inherit::OD2_DEFAULT),
            ..TurnParams::default()
        };
        let now = std::time::Instant::now();
        let fields = |turn: &TurnParams| {
            adapter
                .check_turn_at("claude-cli", turn, now)
                .iter()
                .map(Refusal::field)
                .collect::<Vec<_>>()
        };
        let (mut fits, mut over) = (0, ARG_MAX);
        while over - fits > 1 {
            let mid = fits.midpoint(over);
            if fields(&turn(mid, 0)).is_empty() {
                fits = mid;
            } else {
                over = mid;
            }
        }
        assert!(fits > 4 * 1024 && fits < 16 * 1024, "{fits}");
        assert_eq!(fields(&turn(fits + 1, 0)), [Some("instructions")]);
        let id = crate::SessionId::try_from("s_7f3k9q2mzr4c").unwrap();
        let session = launch::expected_session_id(&id);
        let real = |n: usize| {
            let text = "z".repeat(n);
            let recipe = launch::Recipe {
                model: &model,
                session: launch::Continue::New(&session),
                // The longer argv, as the frame check counts it.
                mode: ClaudeMode::Restricted,
                inherit: Inherit::OD2_DEFAULT,
                extra_write_dirs: &[],
                instructions: Some(&text),
                effort: None,
                output_schema: None,
                max_steps: None,
            };
            let owner = crate::ProcessOwner::Turn {
                session_id: id.clone(),
                turn: crate::TurnNumber::try_from(1).unwrap(),
            };
            let spec = adapter.process_spec(owner, cwd, &recipe).unwrap();
            crate::PrivateProcessSpec::configure_fits(
                &spec.program,
                &spec.args,
                &spec.cwd,
                &spec.env,
            )
        };
        assert!(real(fits), "the largest admitted launch fits Host's cap");
        assert!(
            !real(fits + 11),
            "the bound is tight to the stand-ins' slack"
        );
        let half = fits / 2 + 64;
        assert!(fields(&turn(half, 0)).is_empty());
        assert!(fields(&turn(0, half)).is_empty());
        assert_eq!(fields(&turn(half, half + 1)), [Some("output_schema")]);
        assert_eq!(fields(&turn(half, half)), [Some("instructions")]);
    }

    /// Critical r2 #1: present but empty instructions still pass
    /// `--append-system-prompt ""`, which `check_turn` counts. With empty
    /// instructions, the largest schema it admits launches: the real
    /// recipe's process (the empty flag and a schema of that compact
    /// length) fits Host's cap; one byte more is refused naming
    /// `output_schema`.
    #[test]
    fn empty_instructions_count_their_flag() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = ClaudeAdapter::new(
            dir.path().join("claude"),
            std::sync::Arc::new(crate::instance::InstanceCache::default()),
            &crate::config::BootstrapEnv::from_vars([("PATH", "/usr/bin:/bin")]),
            ClaudeMode::Unrestricted,
        );
        let cwd = std::path::Path::new("/work/project");
        let model = adapter.resolve(None);
        let turn = |output_schema: usize| TurnParams {
            output_schema: true,
            instructions: true,
            sizes: ParamSizes {
                instructions: 0,
                output_schema,
                cwd: cwd.as_os_str().len(),
                model: model.len(),
                ..ParamSizes::default()
            },
            inherit: Some(Inherit::OD2_DEFAULT),
            ..TurnParams::default()
        };
        let now = std::time::Instant::now();
        let fields = |turn: &TurnParams| {
            adapter
                .check_turn_at("claude-cli", turn, now)
                .iter()
                .map(Refusal::field)
                .collect::<Vec<_>>()
        };
        let (mut fits, mut over) = (18, ARG_MAX);
        while over - fits > 1 {
            let mid = fits.midpoint(over);
            if fields(&turn(mid)).is_empty() {
                fits = mid;
            } else {
                over = mid;
            }
        }
        assert_eq!(fields(&turn(fits + 1)), [Some("output_schema")]);
        // `{"description":"…"}` encodes to 18 bytes besides the text.
        let schema = format!(r#"{{"description":"{}"}}"#, "z".repeat(fits - 18));
        assert_eq!(schema.len(), fits);
        let schema = serde_json::value::RawValue::from_string(schema).unwrap();
        let id = crate::SessionId::try_from("s_7f3k9q2mzr4c").unwrap();
        let session = launch::expected_session_id(&id);
        let recipe = launch::Recipe {
            model: &model,
            session: launch::Continue::New(&session),
            mode: ClaudeMode::Restricted,
            inherit: Inherit::OD2_DEFAULT,
            extra_write_dirs: &[],
            instructions: Some(""),
            effort: None,
            output_schema: Some(&schema),
            max_steps: None,
        };
        let owner = crate::ProcessOwner::Turn {
            session_id: id.clone(),
            turn: crate::TurnNumber::try_from(1).unwrap(),
        };
        let spec = adapter.process_spec(owner, cwd, &recipe).unwrap();
        assert!(spec.args.iter().any(|arg| arg == "--append-system-prompt"));
        assert!(
            crate::PrivateProcessSpec::configure_fits(
                &spec.program,
                &spec.args,
                &spec.cwd,
                &spec.env
            ),
            "the largest admitted launch with empty instructions fits Host's cap"
        );
    }

    /// Critical r1 #2: `check_turn`, a resume's admission, reads the
    /// refusal cache for the session's recipe on this binary: a live
    /// entry refuses `harness_unavailable` with `handshake_refused`; the
    /// other schema mode's recipe, an unknown `inherit` and an expired
    /// entry do not.
    #[test]
    fn check_turn_reads_the_refusal_cache() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, instances, binary) = adapter_in(dir.path());
        let written = std::time::Instant::now();
        instances.record_refusal(
            &binary,
            launch::recipe_key(ClaudeMode::Unrestricted, Inherit::OD2_DEFAULT, false),
            crate::instance::Incompatibility::FeatureAbsent("interrupt_receipt_v1"),
            written,
        );
        let turn = |schema: bool, inherit: Option<Inherit>| TurnParams {
            output_schema: schema,
            inherit,
            ..TurnParams::default()
        };
        let refused = adapter.check_turn_at(
            "claude-cli",
            &turn(false, Some(Inherit::OD2_DEFAULT)),
            written,
        );
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert_eq!(refused[0].kind.code(), "harness_unavailable");
        assert_eq!(refused[0].reason, Some("handshake_refused"));
        // The lookup is the session's mode, read from its frozen states, not
        // the configured one: a restricted session's recipe is not refused.
        // (Before the expiry below, whose lookup drops the entry.)
        let frozen = |mode| TurnParams {
            inherit_effective: Some(
                effective_inherit(&categories(mode), Inherit::OD2_DEFAULT)
                    .0
                    .effective,
            ),
            ..turn(false, Some(Inherit::OD2_DEFAULT))
        };
        let restricted = frozen(ClaudeMode::Restricted);
        assert!(
            adapter
                .check_turn_at("claude-cli", &restricted, written)
                .is_empty()
        );
        let unrestricted = frozen(ClaudeMode::Unrestricted);
        assert_eq!(
            adapter
                .check_turn_at("claude-cli", &unrestricted, written)
                .len(),
            1
        );
        let ttl = crate::instance::REFUSAL_TTL;
        for (turn, at) in [
            (turn(true, Some(Inherit::OD2_DEFAULT)), written),
            (turn(false, None), written),
            (turn(false, Some(Inherit::OD2_DEFAULT)), written + ttl),
        ] {
            assert!(adapter.check_turn_at("claude-cli", &turn, at).is_empty());
        }
    }

    /// A plan of the default model and recipe at `now`.
    fn plan_of(adapter: &ClaudeAdapter, now: std::time::Instant) -> RoutePlan {
        let request = DescribeRequest {
            harness: Some("claude".to_owned()),
            ..DescribeRequest::default()
        };
        let model = crate::plan::ModelChoice {
            requested: None,
            resolved: adapter.resolve(None),
        };
        let harness = Harness::Vendor(&crate::harness::HARNESSES[0]);
        adapter.plan_at(harness, &request, model, Inherit::OD2_DEFAULT, now)
    }

    /// C2 §5 refusal cache, with controlled time through `plan` itself:
    /// while the entry is live (to 599.999 s after it was written) the plan
    /// is refused `harness_unavailable` with `reason:"handshake_refused"`
    /// and its version status is `refused`; at 600 s it has expired and the
    /// plan admits the launch, which re-checks. The last version seen stays
    /// reported throughout.
    #[test]
    fn cached_refusal_refuses_until_ten_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, instances, binary) = adapter_in(dir.path());
        let harness = Harness::Vendor(&crate::harness::HARNESSES[0]);
        instances.record_version(harness.name(), &binary, "2.1.285".to_owned());
        let written = std::time::Instant::now();
        instances.record_refusal(
            &binary,
            launch::recipe_key(ClaudeMode::Unrestricted, Inherit::OD2_DEFAULT, false),
            crate::instance::Incompatibility::FeatureAbsent("interrupt_receipt_v1"),
            written,
        );
        let ttl = crate::instance::REFUSAL_TTL;
        assert_eq!(ttl, std::time::Duration::from_secs(600));
        for at in [written, written + std::time::Duration::from_millis(599_999)] {
            let plan = plan_of(&adapter, at);
            assert_eq!(plan.version_status, VersionStatus::Refused);
            assert_eq!(plan.vendor_version.as_deref(), Some("2.1.285"));
            let refusal = plan
                .refusals
                .iter()
                .find(|refusal| refusal.kind == RefusalKind::VersionRefused)
                .expect("a version refusal");
            assert_eq!(refusal.kind.code(), "harness_unavailable");
            assert_eq!(refusal.reason, Some("handshake_refused"));
        }
        let plan = plan_of(&adapter, written + ttl);
        assert_eq!(
            (plan.vendor_version.as_deref(), plan.version_status),
            (Some("2.1.285"), VersionStatus::Tested)
        );
        assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    }

    /// Invariant 13 through Claude's plan (which `describe` reports): the
    /// last version is looked up for this harness and this program path
    /// only, and a refusal for this program path only. Another harness's
    /// version on this path, this harness's on another path, and another
    /// path's refusal leave the plan `null`/`untested`; this pair's own
    /// entry is then reported.
    #[test]
    fn plan_reads_only_its_own_harness_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, instances, binary) = adapter_in(dir.path());
        let other = dir.path().join("other-claude");
        let harness = Harness::Vendor(&crate::harness::HARNESSES[0]);
        assert_eq!(harness.name(), "claude");
        let now = std::time::Instant::now();
        instances.record_version("codex", &binary, "2.1.285".to_owned());
        instances.record_version(harness.name(), &other, "2.1.290".to_owned());
        instances.record_refusal(
            &other,
            launch::recipe_key(ClaudeMode::Unrestricted, Inherit::OD2_DEFAULT, false),
            crate::instance::Incompatibility::FeatureAbsent("interrupt_receipt_v1"),
            now,
        );
        let plan = plan_of(&adapter, now);
        assert_eq!(
            (plan.vendor_version.as_deref(), plan.version_status),
            (None, VersionStatus::Untested)
        );
        assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
        instances.record_version(harness.name(), &binary, "2.1.285".to_owned());
        let plan = plan_of(&adapter, now);
        assert_eq!(
            (plan.vendor_version.as_deref(), plan.version_status),
            (Some("2.1.285"), VersionStatus::Tested)
        );
    }

    /// Bead via-7c6: the live round of 2026-10-05 passed on 2.1.289, so a
    /// plan whose last version seen is 2.1.289 is `tested`, as 2.1.285 is;
    /// another version stays `untested`.
    #[test]
    fn checked_versions_are_tested() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, instances, binary) = adapter_in(dir.path());
        let harness = Harness::Vendor(&crate::harness::HARNESSES[0]);
        let now = std::time::Instant::now();
        for (version, status) in [
            ("2.1.285", VersionStatus::Tested),
            ("2.1.289", VersionStatus::Tested),
            ("2.1.290", VersionStatus::Untested),
        ] {
            instances.record_version(harness.name(), &binary, version.to_owned());
            assert_eq!(plan_of(&adapter, now).version_status, status, "{version}");
        }
    }

    /// AD18 and G8: efforts outside the table and values past the argument
    /// limit are refused by member, in C1 order; the limit itself passes.
    #[test]
    fn per_turn_values_are_refused_by_member() {
        let vendor = VendorOptions::new();
        let fields = |effort, instructions, output_schema| {
            refusals(
                "claude-cli",
                &PerTurn {
                    effort,
                    bound: None,
                    vendor: &vendor,
                    sizes: ParamSizes {
                        instructions,
                        output_schema,
                        ..ParamSizes::default()
                    },
                },
            )
            .iter()
            .map(Refusal::field)
            .collect::<Vec<_>>()
        };
        assert!(fields(Some("max"), ARG_MAX, ARG_MAX).is_empty());
        assert_eq!(
            fields(Some(""), ARG_MAX + 1, ARG_MAX + 1),
            [Some("effort"), Some("instructions"), Some("output_schema")]
        );
        assert_eq!(fields(Some("bogus"), 0, 0), [Some("effort")]);
    }
}
