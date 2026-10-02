//! The pure planning surface (C2 §2, adapter design §3.2, §5.2, AD12, AD13,
//! AD18): `AdapterSet::plan`, `check_turn` and `models` read only bundled
//! data and configuration; they start nothing and write nothing.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value, json};

use std::sync::Arc;

use via_routes::RouteRuntime;

use crate::capabilities::{BoundMode, Capabilities, Verb, VerbReq};
use crate::claude::{self, ClaudeAdapter};
use crate::codex::{self, CodexAdapter};
use crate::config::AdapterConfig;
use crate::driver::DriverKind;
use crate::fake::FakeAdapter;
use crate::harness::{FAKE, HARNESSES, Harness};
use crate::instance::{InstanceCache, resolve_binary};
use crate::{AdapterError, RuntimeConfig, RuntimeResources};

/// A C1 §4 `bound`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bound {
    /// The mode.
    pub mode: BoundMode,
    /// Extra writable directories.
    pub extra_write_dirs: Vec<PathBuf>,
    /// Whether the network is allowed.
    pub network: bool,
}

/// C1 §4 `vendor`: per-harness option objects, passed through.
pub type VendorOptions = BTreeMap<String, Map<String, Value>>;

/// `describe`, or spawn's turn 1, as the adapter plans it (C2 §2).
#[derive(Clone, Debug, Default)]
pub struct DescribeRequest {
    /// The caller's harness string, unchanged.
    pub harness: Option<String>,
    /// The caller's model string.
    pub model: Option<String>,
    /// Spawn's turn-1 effort; `describe` passes none.
    pub effort: Option<String>,
    /// The requested bound, if any.
    pub bound: Option<Bound>,
    /// `require` entries.
    pub require: Vec<VerbReq>,
    /// Vendor options.
    pub vendor: VendorOptions,
    /// The session's working directory.
    pub cwd: Option<PathBuf>,
    /// Stored for C1 compatibility; no effect (C2 §5).
    pub allow_untested: bool,
    /// The encoded sizes of the session's instructions and the turn's
    /// schema, which Core fills; `describe` has neither.
    pub sizes: ParamSizes,
}

/// The encoded byte sizes of the values a route may carry where a lower
/// limit applies (C2 §2), such as a per-argument limit; 0 when absent. Core
/// fills them from the values it holds, so a route refuses purely, before
/// any receipt; the values themselves never reach `plan` or `check_turn`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ParamSizes {
    /// The session's `instructions` text, in UTF-8 bytes.
    pub instructions: usize,
    /// The turn's `output_schema`, in bytes of its compact JSON encoding,
    /// as the turn's `TurnSpec` carries it.
    pub output_schema: usize,
}

/// A resume turn's per-turn values, the input to `check_turn`.
#[derive(Clone, Debug, Default)]
pub struct TurnParams {
    /// The turn's effort, when set.
    pub effort: Option<String>,
    /// The turn's bound, when set.
    pub bound: Option<Bound>,
    /// Whether a non-null `output_schema` is set.
    pub output_schema: bool,
    /// The turn's step limit, when set.
    pub max_steps: Option<u64>,
    /// Vendor options.
    pub vendor: VendorOptions,
    /// The encoded sizes of the session's frozen instructions and the
    /// turn's effective schema, inherited or set.
    pub sizes: ParamSizes,
}

/// What `check_turn` reports of a resume turn it accepts (C2 §2).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TurnCheck {
    /// The turn's bound as the route will apply it, like
    /// [`RoutePlan::effective_bound`]; `None` when the turn sets none.
    pub effective_bound: Option<Bound>,
}

/// The route identity a session stores and hands back on resume, reopen
/// and recovery (AD12).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRef {
    /// The canonical harness frozen at spawn.
    pub harness: String,
    /// The route frozen at spawn.
    pub route: String,
    /// The adapter version of the session's latest started turn (H3).
    pub adapter_version: String,
}

/// Why a plan or a turn is refused (C2 §2 `Refusal.kind`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefusalKind {
    /// The route does not support the verb.
    UnsupportedVerb,
    /// The route cannot enforce the bound.
    BoundUnsupported,
    /// The harness is unknown, not configured, or cannot run the session.
    HarnessUnavailable,
    /// No catalog resolves the model.
    UnknownModel,
    /// A cached handshake check refused this binary.
    VersionRefused,
    /// A vendor option uses a reserved key.
    VendorOptionConflict,
    /// A parameter the route refuses.
    InvalidParam {
        /// The C1 parameter.
        field: &'static str,
    },
    /// A `require` entry the route does not meet.
    MissingCapability {
        /// The verb.
        verb: Verb,
    },
}

impl RefusalKind {
    /// The C1 §8.1 `kind` code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedVerb => "unsupported_verb",
            Self::BoundUnsupported => "bound_unsupported",
            Self::HarnessUnavailable | Self::VersionRefused => "harness_unavailable",
            Self::UnknownModel => "unknown_model",
            Self::VendorOptionConflict | Self::InvalidParam { .. } => "invalid_params",
            Self::MissingCapability { .. } => "missing_capability",
        }
    }
}

/// A refusal, naming the route whenever one was chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refusal {
    /// What was refused.
    pub kind: RefusalKind,
    /// A bounded message, free of prompts and handles.
    pub message: String,
    /// The verb, for `UnsupportedVerb`.
    pub verb: Option<Verb>,
    /// The route, when one was chosen.
    pub route: Option<&'static str>,
    /// C1 `data.reason`, such as `adapter_version`.
    pub reason: Option<&'static str>,
}

impl Refusal {
    /// A refusal of `kind` on `route`.
    pub(crate) fn new(
        kind: RefusalKind,
        route: Option<&'static str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            verb: None,
            route,
            reason: None,
        }
    }

    /// The C1 parameter this refusal names, if any.
    pub fn field(&self) -> Option<&'static str> {
        match &self.kind {
            RefusalKind::InvalidParam { field } => Some(field),
            RefusalKind::BoundUnsupported => Some("bound"),
            RefusalKind::VendorOptionConflict => Some("vendor"),
            RefusalKind::MissingCapability { verb } => Some(verb.as_str()),
            RefusalKind::UnsupportedVerb
            | RefusalKind::HarnessUnavailable
            | RefusalKind::UnknownModel
            | RefusalKind::VersionRefused => None,
        }
    }
}

/// A `refusals` entry of C1 §3.1: `{field?, kind, message, verb?, route?, reason?}`.
impl Serialize for Refusal {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        if let Some(field) = self.field() {
            map.serialize_entry("field", field)?;
        }
        map.serialize_entry("kind", self.kind.code())?;
        map.serialize_entry("message", &self.message)?;
        if let Some(verb) = self.verb {
            map.serialize_entry("verb", &verb)?;
        }
        if let Some(route) = self.route {
            map.serialize_entry("route", route)?;
        }
        if let Some(reason) = self.reason {
            map.serialize_entry("reason", reason)?;
        }
        map.end()
    }
}

/// A C1 §5 warning.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Warning {
    /// The stable code.
    pub code: &'static str,
    /// A bounded message.
    pub message: String,
    /// Structured data, for `config_switch_unverified`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// `version_status` (C2 §5).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionStatus {
    /// In the adapter's `checked` set.
    Tested,
    /// Not yet checked by the maintainers.
    Untested,
    /// A cached handshake check failed.
    Refused,
}

/// The plan's `model`: as requested, and as resolved.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelChoice {
    /// The caller's model, if given.
    pub requested: Option<String>,
    /// The model the route runs.
    pub resolved: String,
}

/// An inherited-configuration category (AD13, OD2).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Vendor hooks.
    Hooks,
    /// MCP servers.
    McpServers,
    /// Plugins.
    Plugins,
    /// Skills.
    Skills,
    /// Agents.
    Agents,
    /// Instruction files.
    InstructionFiles,
}

impl Category {
    /// Every category, in C1 order.
    pub const ALL: [Self; 6] = [
        Self::Hooks,
        Self::McpServers,
        Self::Plugins,
        Self::Skills,
        Self::Agents,
        Self::InstructionFiles,
    ];

    fn index(self) -> usize {
        match self {
            Self::Hooks => 0,
            Self::McpServers => 1,
            Self::Plugins => 2,
            Self::Skills => 3,
            Self::Agents => 4,
            Self::InstructionFiles => 5,
        }
    }
}

/// A category's state: requested `on`/`off`, or effective `unknown` too.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InheritState {
    /// Loaded.
    On,
    /// Not loaded.
    Off,
    /// Not verified either way.
    Unknown,
}

/// One state per category; serializes to C1 status `inherit`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Inherit([InheritState; 6]);

impl Inherit {
    /// The approved OD2 default request: hooks and MCP servers off, the rest on.
    pub const OD2_DEFAULT: Self = Self([
        InheritState::Off,
        InheritState::Off,
        InheritState::On,
        InheritState::On,
        InheritState::On,
        InheritState::On,
    ]);

    /// The state of `category`.
    pub fn get(&self, category: Category) -> InheritState {
        self.0[category.index()]
    }

    pub(crate) fn set(&mut self, category: Category, state: InheritState) {
        self.0[category.index()] = state;
    }
}

impl Serialize for Inherit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(Category::ALL.len()))?;
        for category in Category::ALL {
            map.serialize_entry(&category, &self.get(category))?;
        }
        map.end()
    }
}

/// The `Serialize` form back: every category exactly once, each a state;
/// anything else, a repeated category included (critical r2 #8), is
/// refused, never completed with a default.
impl<'de> Deserialize<'de> for Inherit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(InheritVisitor)
    }
}

struct InheritVisitor;

impl<'de> serde::de::Visitor<'de> for InheritVisitor {
    type Value = Inherit;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a state for every category, each named once")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Inherit, A::Error> {
        let mut states: [Option<InheritState>; 6] = [None; 6];
        while let Some((category, state)) = map.next_entry::<Category, InheritState>()? {
            let slot = &mut states[category.index()];
            if slot.replace(state).is_some() {
                return Err(serde::de::Error::custom("inherit names a category twice"));
            }
        }
        let mut inherit = Inherit::OD2_DEFAULT;
        for category in Category::ALL {
            let state = states[category.index()]
                .ok_or_else(|| serde::de::Error::custom("inherit names every category"))?;
            inherit.set(category, state);
        }
        Ok(inherit)
    }
}

/// Whether VIA can apply one direction of a category (AD13).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Switch {
    /// No switch is applied: the vendor's own behaviour stands.
    #[default]
    None,
    /// A switch whose effect was seen live.
    Verified,
    /// A switch whose effect is not verified.
    Unverified,
}

/// A route's declaration for one category (AD13).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CategoryDecl {
    /// The switch that turns the category on.
    #[serde(default)]
    pub on: Switch,
    /// The switch that turns it off.
    #[serde(default)]
    pub off: Switch,
    /// The state verified with no switch applied (private profile or
    /// inventory); `None` when the vendor default is unverified.
    #[serde(default)]
    pub observed: Option<InheritState>,
}

impl CategoryDecl {
    /// Both directions switchable and verified.
    pub(crate) const VERIFIED: Self = Self {
        on: Switch::Verified,
        off: Switch::Verified,
        observed: None,
    };

    /// AD13's effective state for `requested`: the request itself only
    /// through a verified switch, otherwise the verified observation when no
    /// switch is applied, else `unknown`.
    pub fn effective(&self, requested: InheritState) -> InheritState {
        let switch = match requested {
            InheritState::On => self.on,
            InheritState::Off => self.off,
            InheritState::Unknown => return InheritState::Unknown,
        };
        match switch {
            Switch::Verified => requested,
            Switch::Unverified => InheritState::Unknown,
            Switch::None => self.observed.unwrap_or(InheritState::Unknown),
        }
    }
}

/// A session's inherited-configuration settings (C2 §2, §6.2): as
/// requested at spawn, and their effective states. Both are frozen
/// session parameters: status shows the effective states, and a reopened
/// session's launch recipe applies the requested settings, whatever the
/// configuration says now (critical r2 #5).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InheritPlan {
    /// The settings requested at spawn.
    pub requested: Inherit,
    /// Their effective states (AD13).
    pub effective: Inherit,
}

impl InheritPlan {
    /// The `config_switch_unverified` warning's `data.categories`: every
    /// category whose effective state is not the requested one, as
    /// `{category, requested, effective}`, in C1 order; empty when each is.
    pub fn unverified(&self) -> Vec<Value> {
        Category::ALL
            .into_iter()
            .filter_map(|category| {
                let (asked, state) = (self.requested.get(category), self.effective.get(category));
                (state != asked)
                    .then(|| json!({"category": category, "requested": asked, "effective": state}))
            })
            .collect()
    }
}

/// The settings `requested` with their effective states, and the one
/// `config_switch_unverified` warning listing every category whose
/// effective state is not the requested one (AD13, AC7).
pub(crate) fn effective_inherit(
    decls: &BTreeMap<Category, CategoryDecl>,
    requested: Inherit,
) -> (InheritPlan, Option<Warning>) {
    let mut effective = requested;
    for category in Category::ALL {
        let state = decls
            .get(&category)
            .unwrap_or(&CategoryDecl::VERIFIED)
            .effective(requested.get(category));
        effective.set(category, state);
    }
    let inherit = InheritPlan {
        requested,
        effective,
    };
    let unmet = inherit.unverified();
    let warning = (!unmet.is_empty()).then(|| Warning {
        code: "config_switch_unverified",
        message: "an inherited-configuration setting could not be applied or verified".to_owned(),
        data: Some(json!({ "categories": unmet })),
    });
    (inherit, warning)
}

/// A route plan (C1 §3.1, C2 §2); serializes to the C1 `describe` result.
#[derive(Clone, Debug, Serialize)]
pub struct RoutePlan {
    /// The canonical harness.
    pub harness: &'static str,
    /// The model, as requested and resolved.
    pub model: ModelChoice,
    /// The route.
    pub route: &'static str,
    /// The adapter version that plans and runs the turn (AD12).
    pub adapter_version: String,
    /// The last vendor version seen for the harness and resolved program
    /// path, or `None`.
    pub vendor_version: Option<String>,
    /// The version status.
    pub version_status: VersionStatus,
    /// The C1 §4.1 capabilities.
    pub capabilities: Capabilities,
    /// The bound the route enforces, when one was requested and is supported.
    pub effective_bound: Option<Bound>,
    /// What the route refuses of the request, each by member.
    pub refusals: Vec<Refusal>,
    /// Warnings.
    pub warnings: Vec<Warning>,
    /// The inherited-configuration settings as requested, and their
    /// effective states, frozen at spawn (AD13, C2 §6.2); the effective
    /// states are reported in status, neither in `describe`.
    #[serde(skip)]
    pub inherit: InheritPlan,
    /// The persistent server this plan's connections share, if any; not
    /// part of `describe`.
    #[serde(skip)]
    pub server_key: Option<ServerKey>,
}

/// An opaque key naming one persistent server a route may share across
/// sessions (C2 §2); Core only compares it.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ServerKey(String);

/// One live shared server, as C1 `daemon/status.servers` lists it (C2 §2
/// `servers`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ServerReport {
    /// The harness it serves.
    pub harness: &'static str,
    /// The version its handshake reported, if any.
    pub vendor_version: Option<String>,
    /// Its key.
    pub key: ServerKey,
    /// The sessions leasing it.
    pub sessions: u32,
}

impl ServerKey {
    /// A key of the opaque text `text`.
    pub(crate) fn new(text: String) -> Self {
        Self(text)
    }

    /// The key's opaque text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A model's catalog source (C1 §3.13).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    /// Compiled into the adapter.
    Bundled,
    /// Read from a live instance.
    Discovered,
}

/// One catalogued model and its aliases.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CatalogModel {
    /// The model name.
    pub model: String,
    /// Other names that resolve to it.
    #[serde(default)]
    pub aliases: Vec<String>,
}

impl CatalogModel {
    /// Whether `name` is the model or one of its aliases.
    pub(crate) fn matches(&self, name: &str) -> bool {
        self.model == name || self.aliases.iter().any(|alias| alias == name)
    }
}

/// One C1 §3.13 `models` entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelEntry {
    /// The model name.
    pub model: String,
    /// Its harness.
    pub harness: &'static str,
    /// Other names that resolve to it.
    pub aliases: Vec<String>,
    /// Where the entry comes from.
    pub source: ModelSource,
}

/// Design §5.2: resolves a model-only request against each configured
/// harness's current catalog. A unique match gives the harness and the
/// catalogued name; several are `InvalidParam { field: "harness" }`; none
/// is `UnknownModel`.
pub fn resolve_model<'a>(
    model: &str,
    catalogs: impl IntoIterator<Item = (&'static str, &'a [CatalogModel])>,
) -> Result<(&'static str, String), RefusalKind> {
    let mut found = None;
    for (harness, catalog) in catalogs {
        if let Some(entry) = catalog.iter().find(|entry| entry.matches(model)) {
            if found.is_some() {
                return Err(RefusalKind::InvalidParam { field: "harness" });
            }
            found = Some((harness, entry.model.clone()));
        }
    }
    found.ok_or(RefusalKind::UnknownModel)
}

/// One configured adapter (adapter design §6 step 2): the closed set this
/// build compiles in, dispatched by match.
#[derive(Clone, Copy)]
pub(crate) enum Adapter<'a> {
    /// The fake test double.
    Fake(&'a Arc<FakeAdapter>),
    /// Claude Code (plans; runs turns from via-p98.3.2's C2).
    Claude(&'a Arc<ClaudeAdapter>),
    /// Codex (`codex-app-server`).
    Codex(&'a Arc<CodexAdapter>),
}

impl<'a> Adapter<'a> {
    /// The harness it serves.
    fn name(self) -> &'static str {
        match self {
            Self::Fake(_) => FAKE,
            Self::Claude(_) => claude::HARNESS,
            Self::Codex(_) => codex::HARNESS,
        }
    }

    /// Its catalog: bundled, or for Codex what the live server of the
    /// harness's configured `inherit` discovered (none before discovery).
    fn catalog(self, config: &AdapterConfig) -> Cow<'a, [CatalogModel]> {
        match self {
            Self::Fake(fake) => Cow::Borrowed(fake.catalog()),
            Self::Claude(claude) => Cow::Borrowed(claude.catalog()),
            Self::Codex(codex) => Cow::Owned(
                Harness::parse(codex::HARNESS)
                    .map(|harness| codex.listed(config.inherit(harness)))
                    .unwrap_or_default(),
            ),
        }
    }

    /// Where its catalog comes from (C1 §3.13 `source`).
    fn source(self) -> ModelSource {
        match self {
            Self::Fake(_) | Self::Claude(_) => ModelSource::Bundled,
            Self::Codex(_) => ModelSource::Discovered,
        }
    }

    /// The driver arm of its sessions.
    pub(crate) fn driver_kind(self) -> DriverKind {
        match self {
            Self::Fake(fake) => DriverKind::Fake(Arc::clone(fake)),
            Self::Claude(claude) => DriverKind::Claude(Arc::clone(claude)),
            Self::Codex(codex) => {
                DriverKind::Codex(Arc::new(codex::CodexSession::new(Arc::clone(codex))))
            }
        }
    }
}

/// The adapters this daemon can plan for and run, over the Route runtime:
/// the fake when its fixture is configured, and each vendor harness whose
/// binary resolves at start (design §5.4).
pub struct AdapterSet {
    pub(crate) fake: Option<Arc<FakeAdapter>>,
    pub(crate) claude: Option<Arc<ClaudeAdapter>>,
    pub(crate) codex: Option<Arc<CodexAdapter>>,
    /// The rest of the start-time configuration (design §5.4).
    config: AdapterConfig,
    /// The Route runtime: Wire and Host, which own every connection.
    pub(crate) runtime: Arc<RouteRuntime>,
    /// Test builds: the sizes each `plan` and `check_turn` received, the
    /// latest last ([`Self::param_sizes_seen`]).
    #[cfg(feature = "test-failpoints")]
    sizes_seen: std::sync::Mutex<Vec<ParamSizes>>,
    /// Test builds: the stand-in admission drivers opened later take
    /// ([`Self::stand_in`]).
    #[cfg(feature = "test-failpoints")]
    pub(crate) stand_in: std::sync::OnceLock<Arc<crate::StandIn>>,
}

impl AdapterSet {
    /// One adapter per configured harness over the Route runtime; Core
    /// hands the unopened Store resources down unsplit (C2 §2). A vendor
    /// adapter is built when its binary resolves, `stat` and access checks
    /// only; every vendor adapter shares one instance cache (C2 §5 AD7).
    pub fn new(
        mut config: AdapterConfig,
        runtime: RuntimeConfig,
        resources: RuntimeResources,
    ) -> Result<Self, AdapterError> {
        let runtime = Arc::new(RouteRuntime::new(runtime, resources)?);
        let instances = Arc::new(InstanceCache::default());
        let binary = |name: &str| {
            let row = HARNESSES.iter().find(|row| row.name == name)?;
            resolve_binary(
                config.harness(row).binary(),
                row.default_binary,
                config.env().var("PATH"),
            )
        };
        let claude = binary(claude::HARNESS).map(|binary| {
            Arc::new(ClaudeAdapter::new(
                binary,
                Arc::clone(&instances),
                config.env(),
            ))
        });
        let codex = binary(codex::HARNESS).map(|binary| {
            Arc::new(CodexAdapter::new(
                binary,
                Arc::clone(&instances),
                config.env(),
                Arc::clone(&runtime),
            ))
        });
        Ok(Self {
            fake: config
                .take_fake()
                .map(|fixture| Arc::new(FakeAdapter::new(fixture))),
            claude,
            codex,
            config,
            runtime,
            #[cfg(feature = "test-failpoints")]
            sizes_seen: std::sync::Mutex::default(),
            #[cfg(feature = "test-failpoints")]
            stand_in: std::sync::OnceLock::new(),
        })
    }

    /// The configured adapter for `harness`, if any.
    pub(crate) fn adapter(&self, harness: Harness) -> Option<Adapter<'_>> {
        match harness {
            Harness::Fake => self.fake.as_ref().map(Adapter::Fake),
            Harness::Vendor(row) => match row.name {
                claude::HARNESS => self.claude.as_ref().map(Adapter::Claude),
                codex::HARNESS => self.codex.as_ref().map(Adapter::Codex),
                // No adapter serves this harness in this build.
                _ => None,
            },
        }
    }

    /// Every configured adapter, in C1 harness order, the fake last.
    fn adapters(&self) -> impl Iterator<Item = Adapter<'_>> {
        [
            self.claude.as_ref().map(Adapter::Claude),
            self.codex.as_ref().map(Adapter::Codex),
            self.fake.as_ref().map(Adapter::Fake),
        ]
        .into_iter()
        .flatten()
    }

    /// The live shared servers (C2 §2 `servers`): a pure in-memory
    /// snapshot. Every route today runs per-turn processes, so none is
    /// listed.
    pub fn servers(&self) -> Vec<ServerReport> {
        Vec::new()
    }

    /// Each shared server that ended, oldest first (at most 16), as its
    /// ID, its launch ordinal (1 for the registry's first launch) and
    /// Host's confirmed exit code: the replay harness judges a server
    /// launch by them (x.3.2 X3). A pure in-memory snapshot.
    pub fn ended_servers(&self) -> Vec<(String, u64, Option<i32>)> {
        self.codex.as_ref().map_or_else(Vec::new, |codex| {
            codex
                .servers()
                .ended()
                .into_iter()
                .map(|end| {
                    (
                        end.server.as_str().to_owned(),
                        end.launch,
                        end.exit.and_then(|exit| exit.code),
                    )
                })
                .collect()
        })
    }

    /// Test builds: records the sizes a `plan` or `check_turn` received.
    #[cfg(feature = "test-failpoints")]
    fn saw(&self, sizes: ParamSizes) {
        let mut seen = self
            .sizes_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if seen.len() == 64 {
            seen.remove(0);
        }
        seen.push(sizes);
    }

    /// Test builds only: the sizes each `plan` and `check_turn` received,
    /// the latest last, up to 64 (x.3.2 G8).
    #[cfg(feature = "test-failpoints")]
    pub fn param_sizes_seen(&self) -> Vec<ParamSizes> {
        self.sizes_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Pure; no I/O. Resolves the harness string, the route and the model,
    /// and lists what the route refuses of the request.
    pub fn plan(&self, req: &DescribeRequest) -> Result<RoutePlan, Refusal> {
        #[cfg(feature = "test-failpoints")]
        self.saw(req.sizes);
        let harness = match (&req.harness, &req.model) {
            (Some(name), _) => Harness::parse(name).ok_or_else(|| unavailable(None))?,
            (None, Some(model)) => {
                let catalogs: Vec<_> = self
                    .adapters()
                    .map(|adapter| (adapter.name(), adapter.catalog(&self.config)))
                    .collect();
                let catalogs = catalogs.iter().map(|(name, catalog)| (*name, &**catalog));
                let (name, _) = resolve_model(model, catalogs).map_err(|kind| {
                    Refusal::new(kind, None, "no unique harness catalogs the model")
                })?;
                Harness::parse(name).ok_or_else(|| unavailable(None))?
            }
            (None, None) => {
                return Err(Refusal::new(
                    RefusalKind::InvalidParam { field: "model" },
                    None,
                    "a plan takes a harness or a model",
                ));
            }
        };
        let route = harness.route();
        let adapter = match self.adapter(harness) {
            Some(Adapter::Fake(fake)) => fake,
            Some(Adapter::Claude(claude)) => {
                let model = ModelChoice {
                    requested: req.model.clone(),
                    resolved: claude.resolve(req.model.as_deref()),
                };
                return Ok(claude.plan(harness, req, model, self.config.inherit(harness)));
            }
            Some(Adapter::Codex(codex)) => {
                return codex.plan(harness, req, self.config.inherit(harness));
            }
            None => {
                return Err(unavailable(Some(route)));
            }
        };
        let resolved = adapter.resolve(req.model.as_deref()).ok_or_else(|| {
            Refusal::new(
                RefusalKind::UnknownModel,
                Some(route),
                format!("no model of route {route} matches"),
            )
        })?;
        Ok(adapter.plan(
            harness,
            req,
            ModelChoice {
                requested: req.model.clone(),
                resolved,
            },
            // Design §5.4: the harness's configured `inherit`; the fake's
            // is the OD2 default.
            self.config.inherit(harness),
        ))
    }

    /// Pure: validates a resume turn's values against the frozen route,
    /// including AD12's adapter-version compatibility, and reports the
    /// turn's bound as the route will apply it.
    pub fn check_turn(
        &self,
        session: &SessionRef,
        turn: &TurnParams,
    ) -> Result<TurnCheck, Refusal> {
        #[cfg(feature = "test-failpoints")]
        self.saw(turn.sizes);
        let harness = Harness::parse(&session.harness).ok_or_else(|| unavailable(None))?;
        let route = harness.route();
        let adapter = match self.adapter(harness).filter(|_| session.route == route) {
            Some(Adapter::Fake(fake)) => fake,
            Some(Adapter::Claude(_)) => {
                ClaudeAdapter::check_version(route, &session.adapter_version)?;
                return match ClaudeAdapter::check_turn(route, turn).into_iter().next() {
                    Some(refusal) => Err(refusal),
                    // The route applies a supported bound as requested.
                    None => Ok(TurnCheck {
                        effective_bound: turn.bound.clone(),
                    }),
                };
            }
            Some(Adapter::Codex(_)) => {
                return CodexAdapter::check_turn(route, &session.adapter_version, turn);
            }
            None => {
                return Err(unavailable(Some(route)));
            }
        };
        adapter.check_version(route, &session.adapter_version)?;
        match adapter.check_turn(route, turn).into_iter().next() {
            Some(refusal) => Err(refusal),
            None => Ok(TurnCheck {
                effective_bound: turn
                    .bound
                    .clone()
                    .map(|bound| adapter.effective_bound(bound)),
            }),
        }
    }

    /// The catalog of each configured harness, or of `harness` only:
    /// bundled, or discovered by Codex's live server.
    pub fn models(&self, harness: Option<&str>) -> Vec<ModelEntry> {
        self.adapters()
            .filter(|adapter| harness.is_none_or(|harness| harness == adapter.name()))
            .flat_map(|adapter| {
                let catalog = adapter.catalog(&self.config).into_owned();
                catalog.into_iter().map(move |entry| ModelEntry {
                    model: entry.model,
                    harness: adapter.name(),
                    aliases: entry.aliases,
                    source: adapter.source(),
                })
            })
            .collect()
    }
}

/// Never echoes the caller's harness string: it is unvalidated input.
fn unavailable(route: Option<&'static str>) -> Refusal {
    Refusal::new(
        RefusalKind::HarnessUnavailable,
        route,
        "the harness is not available in this daemon",
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Inherit;
    use crate::harness::Harness;

    /// Each vendor stub names a row of the harness table, with its route.
    #[test]
    fn each_vendor_stub_names_a_harness_row() {
        for (name, route) in [
            (crate::claude::HARNESS, "claude-cli"),
            (crate::codex::HARNESS, "codex-app-server"),
        ] {
            assert_eq!(Harness::parse(name).map(Harness::route), Some(route));
        }
    }

    /// Critical r2 #8: `inherit` names every category exactly once; seven
    /// members with `hooks` twice are refused, as are five.
    #[test]
    fn inherit_refuses_a_repeated_category() {
        let all = r#""hooks":"off","mcp_servers":"off","plugins":"on","skills":"on","agents":"on","instruction_files":"on""#;
        let parse = |text: String| serde_json::from_str::<Inherit>(&text);
        assert_eq!(parse(format!("{{{all}}}")).unwrap(), Inherit::OD2_DEFAULT);
        assert!(
            parse(format!(r#"{{{all},"hooks":"on"}}"#)).is_err(),
            "hooks twice"
        );
        assert!(
            parse(format!(r#"{{"hooks":"on",{all}}}"#)).is_err(),
            "hooks twice, first"
        );
        assert!(
            serde_json::from_value::<Inherit>(json!({"hooks":"off","mcp_servers":"off",
                "plugins":"on","skills":"on","agents":"on"}))
            .is_err(),
            "five categories"
        );
    }
}
