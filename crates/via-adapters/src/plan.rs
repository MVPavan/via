//! The pure planning surface (C2 §2, adapter design §3.2, §5.2, AD12, AD13,
//! AD18): `AdapterSet::plan`, `check_turn` and `models` read only bundled
//! data and configuration; they start nothing and write nothing.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value, json};

use std::sync::Arc;

use via_routes::FakeRoute;

use crate::capabilities::{BoundMode, Capabilities, Verb, VerbReq};
use crate::config::AdapterConfig;
use crate::fake::FakeAdapter;
use crate::harness::Harness;
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

/// The effective states for `requested`, and the one
/// `config_switch_unverified` warning listing every category whose
/// effective state is not the requested one (AD13, AC7).
pub(crate) fn effective_inherit(
    decls: &BTreeMap<Category, CategoryDecl>,
    requested: Inherit,
) -> (Inherit, Option<Warning>) {
    let mut effective = requested;
    let mut unmet = Vec::new();
    for category in Category::ALL {
        let asked = requested.get(category);
        let state = decls
            .get(&category)
            .unwrap_or(&CategoryDecl::VERIFIED)
            .effective(asked);
        effective.set(category, state);
        if state != asked {
            unmet.push(json!({"category": category, "requested": asked, "effective": state}));
        }
    }
    let warning = (!unmet.is_empty()).then(|| Warning {
        code: "config_switch_unverified",
        message: "an inherited-configuration setting could not be applied or verified".to_owned(),
        data: Some(json!({ "categories": unmet })),
    });
    (effective, warning)
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
    /// The last vendor version seen for the binary identity, or `None`.
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
    /// Effective inherited-configuration states, frozen at spawn (AD13);
    /// reported in status, not in `describe`.
    #[serde(skip)]
    pub inherit: Inherit,
    /// The persistent server this plan's connections share, if any; not
    /// part of `describe`.
    #[serde(skip)]
    pub server_key: Option<ServerKey>,
}

/// An opaque key naming one persistent server a route may share across
/// sessions (C2 §2); Core only compares it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ServerKey(String);

impl ServerKey {
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

/// The adapters this daemon can plan for and run: in S-CORE only the fake,
/// when its fixture is configured, over the Route runtime.
pub struct AdapterSet {
    pub(crate) fake: Option<Arc<FakeAdapter>>,
    /// The rest of the start-time configuration (design §5.4).
    config: AdapterConfig,
    /// The Route runtime: Wire and Host, which own every connection.
    pub(crate) route: Arc<FakeRoute>,
}

impl AdapterSet {
    /// One adapter per configured harness over the Route runtime; Core
    /// hands the unopened Store resources down unsplit (C2 §2).
    pub fn new(
        mut config: AdapterConfig,
        runtime: RuntimeConfig,
        resources: RuntimeResources,
    ) -> Result<Self, AdapterError> {
        let route = FakeRoute::new(runtime, resources)?;
        Ok(Self {
            fake: config
                .take_fake()
                .map(|fixture| Arc::new(FakeAdapter::new(fixture))),
            config,
            route: Arc::new(route),
        })
    }

    /// The configured adapter for `harness`, if any.
    pub(crate) fn adapter(&self, harness: Harness) -> Option<&Arc<FakeAdapter>> {
        match harness {
            Harness::Fake => self.fake.as_ref(),
            Harness::Vendor(_) => None,
        }
    }

    /// Pure; no I/O. Resolves the harness string, the route and the model,
    /// and lists what the route refuses of the request.
    pub fn plan(&self, req: &DescribeRequest) -> Result<RoutePlan, Refusal> {
        let harness = match (&req.harness, &req.model) {
            (Some(name), _) => Harness::parse(name).ok_or_else(|| unavailable(None))?,
            (None, Some(model)) => {
                let catalogs = self
                    .fake
                    .iter()
                    .map(|fake| (Harness::Fake.name(), fake.catalog()));
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
        let adapter = self
            .adapter(harness)
            .ok_or_else(|| unavailable(Some(route)))?;
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
    /// including AD12's adapter-version compatibility.
    pub fn check_turn(&self, session: &SessionRef, turn: &TurnParams) -> Result<(), Refusal> {
        let harness = Harness::parse(&session.harness).ok_or_else(|| unavailable(None))?;
        let route = harness.route();
        let adapter = self
            .adapter(harness)
            .filter(|_| session.route == route)
            .ok_or_else(|| unavailable(Some(route)))?;
        adapter.check_version(route, &session.adapter_version)?;
        match adapter.check_turn(route, turn).into_iter().next() {
            Some(refusal) => Err(refusal),
            None => Ok(()),
        }
    }

    /// The bundled catalog of each configured harness, or of `harness` only.
    pub fn models(&self, harness: Option<&str>) -> Vec<ModelEntry> {
        let name = Harness::Fake.name();
        if harness.is_some_and(|harness| harness != name) {
            return Vec::new();
        }
        self.fake
            .iter()
            .flat_map(|fake| fake.catalog())
            .map(|entry| ModelEntry {
                model: entry.model.clone(),
                harness: name,
                aliases: entry.aliases.clone(),
                source: ModelSource::Bundled,
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
