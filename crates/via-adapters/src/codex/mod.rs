//! `Adapter::Codex` (`codex-app-server`, adapter design §6;
//! docs/specs/vendors/codex.md). The daemon builds it when the harness's
//! binary resolves, with the daemon's instance cache and the Route
//! runtime, over which it keeps the shared-server registry. It plans
//! purely (capabilities, bound gate, effort and vendor-key refusals,
//! inherited configuration, the server key); its driver runs each turn on
//! a shared `codex app-server` (x.3.2 X3).

mod driver;
mod launch;
mod normalize;
mod plan;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use via_routes::RouteRuntime;
use via_routes::codex::Servers;

use crate::config::BootstrapEnv;
use crate::harness::Harness;
use crate::instance::InstanceCache;
use crate::plan::{
    Bound, DescribeRequest, Inherit, ModelChoice, Refusal, RefusalKind, RoutePlan, ServerKey,
    TurnCheck, TurnParams, VendorOptions, effective_inherit,
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
    /// The shared-server registry (x.3.2 X0 item 2).
    servers: Arc<Servers>,
    /// The last catalog a server's `model/list` discovery returned, whole
    /// (packet §3): it resolves a plan with no model to its default (ruling
    /// Q1) and judges a vendor effort.
    catalog: Mutex<Option<Arc<[DiscoveredModel]>>>,
}

/// One turn's values the route judges purely.
struct PerTurn<'a> {
    effort: Option<&'a str>,
    bound: Option<&'a Bound>,
    max_steps: bool,
    vendor: &'a VendorOptions,
}

impl CodexAdapter {
    pub(crate) fn new(
        binary: PathBuf,
        instances: Arc<InstanceCache>,
        env: &BootstrapEnv,
        runtime: Arc<RouteRuntime>,
    ) -> Self {
        Self {
            binary,
            instances,
            env: env.clone(),
            servers: Servers::new(runtime, normalize::DECLINES),
            catalog: Mutex::new(None),
        }
    }

    /// The shared-server registry.
    pub(crate) fn servers(&self) -> &Arc<Servers> {
        &self.servers
    }

    /// The route's vendor home, `<state>/vendor/codex`: the server's SQLite
    /// home and working directory.
    fn vendor_home(&self) -> PathBuf {
        self.servers.vendor_state_dir().join(HARNESS)
    }

    /// The launch recipe of a server for `requested`'s inherited settings.
    fn recipe(&self, requested: Inherit) -> ServerRecipe {
        ServerRecipe::new(&self.binary, requested, &self.env, &self.vendor_home())
    }

    /// The last discovered catalog, if any.
    fn catalog(&self) -> Option<Arc<[DiscoveredModel]>> {
        self.catalog
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Keeps a server's whole discovered catalog.
    fn discovered(&self, models: Arc<[DiscoveredModel]>) {
        *self.catalog.lock().unwrap_or_else(PoisonError::into_inner) = Some(models);
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
        let model = match req.model.clone().filter(|model| !model.is_empty()) {
            Some(model) => Some(model),
            None => self.catalog().and_then(|catalog| {
                catalog
                    .iter()
                    .find(|model| model.default)
                    .map(|model| model.model.clone())
            }),
        }
        .ok_or_else(|| {
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
        let effective_bound = req
            .bound
            .clone()
            .filter(|bound| plan::sandbox(bound).is_ok());
        let (inherit, switch_warning) = effective_inherit(&plan::categories(), requested);
        let key = self.recipe(requested).config_hash(ADAPTER_VERSION);
        let vendor_version = self.instances.last_version(harness.name(), &self.binary);
        let refused = self
            .instances
            .refusal(&self.binary, &key.hex(), std::time::Instant::now())
            .is_some();
        let version_status = if refused {
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
            server_key: Some(ServerKey::new(key.hex())),
        })
    }

    /// A resume turn (C2 §2): AD12's version check, then the per-turn
    /// refusals; the bound applies as requested.
    pub(crate) fn check_turn(
        route: &'static str,
        stored_version: &str,
        turn: &TurnParams,
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
        };
        match refusals(route, &per_turn).into_iter().next() {
            Some(refusal) => Err(refusal),
            None => Ok(TurnCheck {
                effective_bound: turn.bound.clone(),
            }),
        }
    }
}

/// Every per-turn refusal, in C1 member order: `effort`, `max_steps`,
/// `bound`, `vendor`. `output_schema` is native and never refused.
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
            RefusalKind::VendorOptionConflict,
            Some(route),
            format!("a vendor option sets what route {route} reserves"),
        )),
        Some(plan::VendorRefusal::NotAllowed) => refusals.push(invalid(
            "vendor",
            format!("route {route} accepts no other vendor options"),
        )),
        None => {}
    }
    refusals
}
