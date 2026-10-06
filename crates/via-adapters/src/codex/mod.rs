//! `Adapter::Codex` (`codex-app-server`, adapter design §6;
//! docs/specs/vendors/codex.md). The daemon builds it when the harness's
//! binary resolves, with the daemon's instance cache and the Route
//! runtime, over which it keeps the shared-server registry. It plans
//! purely (capabilities, bound gate, effort and vendor-key refusals,
//! inherited configuration, the server key); its driver runs each turn on
//! a shared `codex app-server` (x.3.2 X3).

mod delivery;
mod driver;
#[cfg(test)]
mod driver_tests;
mod launch;
mod normalize;
mod plan;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use via_routes::RouteRuntime;
use via_routes::codex::{ServerId, Servers};

use crate::config::{BootstrapEnv, CodexSettings};
use crate::harness::Harness;
use crate::instance::InstanceCache;
use crate::passthrough::{self, VendorArgs};
use crate::plan::{
    Bound, CatalogModel, DescribeRequest, Inherit, ModelChoice, Refusal, RefusalKind, RoutePlan,
    ServerKey, TurnCheck, TurnParams, VendorOptions, effective_inherit,
};

pub(crate) use driver::{CodexSession, connection_id, run_turn};
use launch::ServerRecipe;
use normalize::DiscoveredModel;

/// The harness this adapter serves, a [`crate::HARNESSES`] name.
pub(crate) const HARNESS: &str = "codex";

/// This adapter's version (AD12): changed when stored session state or the
/// server recipe changes.
const ADAPTER_VERSION: &str = "1";

/// The adapter version a session's turns record (AD12).
pub(crate) fn adapter_version() -> String {
    ADAPTER_VERSION.to_owned()
}

/// The stored adapter versions this one resumes; none before the first.
const COMPATIBLE: &[&str] = &[];

/// The Codex adapter.
pub(crate) struct CodexAdapter {
    /// The resolved vendor binary (design §5.4).
    binary: PathBuf,
    /// The daemon's instance cache (C2 §5 AD7).
    instances: Arc<InstanceCache>,
    /// The daemon's bootstrap environment, which the recipe filters.
    env: BootstrapEnv,
    /// Codex's `daemon.json` settings (`harnesses.codex`).
    settings: CodexSettings,
    /// The shared-server registry (x.3.2 X0 item 2).
    servers: Arc<Servers>,
    /// Each server key's catalog, whole, as its live instance's
    /// `model/list` discovery last returned it (packet §3: cached per
    /// server instance): it resolves a plan with no model to its default
    /// (ruling Q1), lists the route's models and resolves a model-only
    /// plan (C1 §3.13), and judges a vendor effort. Each is kept with the
    /// instance that discovered it and counts only while that instance is
    /// live, so a later instance of the key discovers its own.
    catalogs: Mutex<BTreeMap<String, Discovered>>,
}

/// A catalog and the server instance that discovered it.
type Discovered = (ServerId, Arc<[DiscoveredModel]>);

/// One turn's values the route judges purely.
struct PerTurn<'a> {
    effort: Option<&'a str>,
    bound: Option<&'a Bound>,
    max_steps: bool,
    vendor: &'a VendorOptions,
    vendor_args: &'a VendorArgs,
}

impl CodexAdapter {
    pub(crate) fn new(
        binary: PathBuf,
        instances: Arc<InstanceCache>,
        (env, settings): (&BootstrapEnv, CodexSettings),
        runtime: Arc<RouteRuntime>,
    ) -> Self {
        Self {
            binary,
            instances,
            env: env.clone(),
            settings,
            servers: Servers::new(runtime, normalize::DECLINES),
            catalogs: Mutex::default(),
        }
    }

    /// The shared-server registry.
    pub(crate) fn servers(&self) -> &Arc<Servers> {
        &self.servers
    }

    /// The live servers as `daemon/status` lists them (C1 §3.14): each
    /// one's handshake version and the sessions leasing it.
    pub(crate) fn server_reports(&self) -> Vec<crate::plan::ServerReport> {
        self.servers
            .reports()
            .into_iter()
            .map(|report| crate::plan::ServerReport {
                harness: HARNESS,
                vendor_version: normalize::instance_version(&report.user_agent).map(str::to_owned),
                key: ServerKey::new(report.key),
                sessions: report.sessions,
            })
            .collect()
    }

    /// The route's vendor home, `<state>/vendor/codex`: the server's SQLite
    /// home and working directory.
    fn vendor_home(&self) -> PathBuf {
        self.servers.vendor_state_dir().join(HARNESS)
    }

    /// The launch recipe of a server for `requested`'s inherited settings
    /// and a session's raw arguments (C2 §6.3).
    fn recipe(&self, requested: Inherit, vendor_args: &VendorArgs) -> ServerRecipe {
        ServerRecipe::new(
            &self.binary,
            (requested, self.settings),
            &self.env,
            &self.vendor_home(),
        )
        .with_vendor_args(vendor_args)
    }

    /// The server key of `requested`'s recipe with `vendor_args`: what
    /// equal sessions share.
    fn server_key(&self, requested: Inherit, vendor_args: &VendorArgs) -> String {
        self.recipe(requested, vendor_args)
            .config_hash(ADAPTER_VERSION)
            .hex()
    }

    /// C2 §6.3: a session with raw arguments whose server recipe does not
    /// fit Host's launch request is refused naming `vendor_args`; with
    /// none, the recipe is VIA's own and is not judged here.
    fn launch_refusal(
        &self,
        route: &'static str,
        (requested, vendor_args): (Inherit, &VendorArgs),
    ) -> Option<Refusal> {
        (!vendor_args.is_empty() && !self.recipe(requested, vendor_args).fits()).then(|| {
            Refusal::new(
                RefusalKind::InvalidParam {
                    field: "vendor_args",
                },
                Some(route),
                format!(
                    "the server's launch arguments, vendor_args included, exceed route \
                     {route}'s 64 KiB launch request limit"
                ),
            )
        })
    }

    /// The catalog server key `key`'s live instance discovered, if any:
    /// none once that instance retired or was lost.
    fn catalog(&self, key: &str) -> Option<Arc<[DiscoveredModel]>> {
        let (server, models) = self
            .catalogs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()?;
        self.servers.is_live(&server).then_some(models)
    }

    /// Keeps the whole catalog instance `server` of server key `key`
    /// discovered.
    fn discovered(&self, key: String, server: ServerId, models: Arc<[DiscoveredModel]>) {
        self.catalogs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, (server, models));
    }

    /// The models `requested`'s server discovered (C1 §3.13 `models`, and
    /// model-only resolution): none before its discovery.
    pub(crate) fn listed(&self, requested: Inherit) -> Vec<CatalogModel> {
        self.catalog(&self.server_key(requested, &VendorArgs::default()))
            .iter()
            .flat_map(|catalog| catalog.iter())
            .map(|model| CatalogModel {
                model: model.model.clone(),
                aliases: Vec::new(),
            })
            .collect()
    }

    /// The plan of a spawn or `describe` (C2 §2): pure but for one `stat`
    /// of the binary, which names the last version seen for it. With no
    /// bundled catalog a named model is taken as given (discovery judges
    /// it at the first turn); with none named, the discovered catalog's
    /// default once a server reported one, else `unknown_model` (ruling
    /// Q1).
    pub(crate) fn plan(
        &self,
        harness: Harness,
        req: &DescribeRequest,
        requested: Inherit,
    ) -> Result<RoutePlan, Refusal> {
        let route = harness.route();
        let key = self.server_key(requested, &req.vendor_args);
        let catalog = self.catalog(&key);
        let model = resolved_model(req.model.as_deref(), catalog.as_deref()).ok_or_else(|| {
            Refusal::new(
                RefusalKind::UnknownModel,
                Some(route),
                format!("route {route} has no default model: name one"),
            )
        })?;
        let capabilities = plan::capabilities();
        let mut refusals = refusals(
            route,
            &PerTurn {
                effort: req.effort.as_deref(),
                bound: req.bound.as_ref(),
                max_steps: false,
                vendor: &req.vendor,
                vendor_args: &req.vendor_args,
            },
        );
        refusals.extend(self.launch_refusal(route, (requested, &req.vendor_args)));
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
        let effective_bound = req
            .bound
            .clone()
            .filter(|bound| plan::sandbox(bound).is_ok());
        let (inherit, switch_warning) = effective_inherit(&plan::categories(), requested);
        let vendor_version = self.instances.last_version(harness.name(), &self.binary);
        let refused = self
            .instances
            .refusal(&self.binary, &key, std::time::Instant::now())
            .is_some();
        let version_status = if refused {
            // C2 §5 AD7: a cached refusal refuses the plan, as Claude's.
            let mut refusal = Refusal::new(
                RefusalKind::VersionRefused,
                Some(route),
                "a recent handshake check of this binary failed on something VIA relies on",
            );
            refusal.reason = Some("handshake_refused");
            refusals.push(refusal);
            crate::plan::VersionStatus::Refused
        } else {
            vendor_version.as_deref().map_or(
                crate::plan::VersionStatus::Untested,
                normalize::version_status,
            )
        };
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
            // VIA's launch settings only (ruling: X0 item 3): sessions with
            // equal keys share one server.
            server_key: Some(ServerKey::new(key)),
        })
    }

    /// A resume turn (C2 §2): AD12's version check, then the per-turn
    /// refusals, then (AD18) the effort against the session's model in the
    /// catalog its server key's live instance discovered, once one did; the
    /// bound applies as requested.
    pub(crate) fn check_turn(
        &self,
        route: &'static str,
        stored_version: &str,
        turn: &TurnParams,
    ) -> Result<TurnCheck, Refusal> {
        let catalog = turn
            .inherit
            .and_then(|inherit| self.catalog(&self.server_key(inherit, &turn.vendor_args)));
        let checked = Self::judge_turn(route, stored_version, turn, catalog.as_deref())?;
        match turn
            .inherit
            .and_then(|inherit| self.launch_refusal(route, (inherit, &turn.vendor_args)))
        {
            Some(refusal) => Err(refusal),
            None => Ok(checked),
        }
    }

    /// [`Self::check_turn`] against `catalog`, the session's server key's
    /// discovered catalog if any: a model it lists that does not advertise
    /// the effort refuses it; no catalog, or a model it does not list,
    /// leaves the effort to `run_turn`.
    fn judge_turn(
        route: &'static str,
        stored_version: &str,
        turn: &TurnParams,
        catalog: Option<&[DiscoveredModel]>,
    ) -> Result<TurnCheck, Refusal> {
        if stored_version != ADAPTER_VERSION && !COMPATIBLE.contains(&stored_version) {
            let mut refusal = Refusal::new(
                RefusalKind::HarnessUnavailable,
                Some(route),
                "the session's adapter version is not compatible with this adapter",
            );
            refusal.reason = Some("adapter_version");
            return Err(refusal);
        }
        let per_turn = PerTurn {
            effort: turn.effort.as_deref(),
            bound: turn.bound.as_ref(),
            max_steps: turn.max_steps.is_some(),
            vendor: &turn.vendor,
            vendor_args: &turn.vendor_args,
        };
        if let Some(refusal) = refusals(route, &per_turn).into_iter().next() {
            return Err(refusal);
        }
        if turn.sizes.prompt_json.saturating_add(turn.sizes.cwd_json) > plan::PROMPT_ECHO_MAX {
            return Err(Refusal::new(
                RefusalKind::InvalidParam { field: "prompt" },
                Some(route),
                format!("the prompt is larger than route {route} echoes in one message"),
            ));
        }
        if let (Some(model), Some(catalog)) = (turn.model.as_deref(), catalog)
            && driver::vendor_effort(turn.effort.as_deref(), catalog, model).is_err()
        {
            return Err(Refusal::new(
                RefusalKind::InvalidParam { field: "effort" },
                Some(route),
                format!("effort is not a value the session's model advertises on route {route}"),
            ));
        }
        Ok(TurnCheck {
            effective_bound: turn.bound.clone(),
        })
    }
}

/// Every per-turn refusal, in C1 member order: `effort`, `max_steps`,
/// `bound`, `vendor`, then the session's `vendor_args` (C2 §6.3).
/// `output_schema` is native and never refused.
fn refusals(route: &'static str, turn: &PerTurn<'_>) -> Vec<Refusal> {
    let invalid = |field, message: String| {
        Refusal::new(RefusalKind::InvalidParam { field }, Some(route), message)
    };
    let mut refusals = Vec::new();
    if turn.effort.is_some_and(plan::effort_refused) {
        refusals.push(invalid(
            "effort",
            format!("effort is not a value route {route} accepts"),
        ));
    }
    if turn.max_steps {
        refusals.push(invalid(
            "max_steps",
            format!("max_steps is unsupported on route {route}"),
        ));
    }
    if let Some(Err(refusal)) = turn.bound.map(plan::sandbox) {
        refusals.push(Refusal::new(
            RefusalKind::BoundUnsupported,
            Some(route),
            refusal.message(route),
        ));
    }
    match plan::vendor_refusal(HARNESS, turn.vendor) {
        Some(plan::VendorRefusal::Reserved) => refusals.push(Refusal::new(
            RefusalKind::VendorOptionConflict { field: "vendor" },
            Some(route),
            format!("a vendor option sets what route {route} reserves"),
        )),
        Some(plan::VendorRefusal::NotAllowed) => refusals.push(invalid(
            "vendor",
            format!("route {route} accepts no other vendor options"),
        )),
        None => {}
    }
    if let Some(index) = passthrough::conflict(turn.vendor_args.as_slice(), &plan::ARG_RULES) {
        refusals.push(Refusal::new(
            RefusalKind::VendorOptionConflict {
                field: "vendor_args",
            },
            Some(route),
            format!("vendor_args[{index}] sets what route {route} owns"),
        ));
    }
    refusals
}

/// The model a plan resolves (ruling Q1): the named one, taken as given
/// (discovery judges it at the first turn); with none named, the
/// discovered catalog's default once a server reported one, else none
/// (`unknown_model`).
fn resolved_model(named: Option<&str>, catalog: Option<&[DiscoveredModel]>) -> Option<String> {
    match named.filter(|model| !model.is_empty()) {
        Some(model) => Some(model.to_owned()),
        None => catalog?
            .iter()
            .find(|model| model.default)
            .map(|model| model.model.clone()),
    }
}
