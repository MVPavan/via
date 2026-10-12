//! `Adapter::Pi` (`pi-rpc`, vendor packet `docs/specs/vendors/pi.md`):
//! pure planning ([`plan`]), the per-turn launch recipe and VIA's private
//! agent directory ([`launch`]), the profile policy ([`profile`]) and the
//! normalizer ([`normalize`]). The daemon builds it when the harness's
//! binary resolves, with the daemon's instance cache. Its driver
//! ([`run_turn`]) runs each turn on its own private `pi --mode rpc`
//! process; C1 `models` reads the live catalog from one more such process,
//! never prompted ([`PiAdapter::discover`]).

mod driver;
mod launch;
mod normalize;
mod plan;
mod profile;

pub(crate) use driver::{connection_id, run_turn};
pub(crate) use plan::adapter_version;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use via_routes::RouteRuntime;
use via_routes::pi::{CatalogError, PiRoute};

use crate::config::BootstrapEnv;
use crate::instance::InstanceCache;
use crate::plan::CatalogModel;
use crate::{Deadline, ProcessOwner};

/// The harness this adapter serves, a [`crate::HARNESSES`] name.
pub(crate) const HARNESS: &str = "pi";

/// The Pi adapter.
pub(crate) struct PiAdapter {
    /// The resolved vendor binary.
    binary: PathBuf,
    /// The daemon's instance cache (C2 §5 AD7).
    instances: Arc<InstanceCache>,
    /// The bundled catalog (packet §2).
    catalog: Vec<CatalogModel>,
    /// The launch environment's allow-list, as captured at daemon start
    /// (packet §4.2).
    env: Vec<(OsString, OsString)>,
    /// The daemon's vendor state directory: VIA's private Pi profile and
    /// session files live under it (packet §4.3).
    vendor_state_dir: PathBuf,
    /// Serializes writes of VIA's launch state (packet §4.4) over each
    /// blocking task's whole lifetime: a task its turn stopped waiting for
    /// still runs, and the next attempt's write must not interleave with
    /// it. One per adapter, so it spans every driver of the set, a
    /// session reopened while an earlier driver's task runs included.
    staging: Arc<std::sync::Mutex<()>>,
}

impl PiAdapter {
    pub(crate) fn new(
        binary: PathBuf,
        instances: Arc<InstanceCache>,
        env: &BootstrapEnv,
        vendor_state_dir: PathBuf,
    ) -> Self {
        Self {
            binary,
            instances,
            catalog: plan::catalog(),
            env: launch::allowed_env(env),
            vendor_state_dir,
            staging: Arc::default(),
        }
    }
}

impl PiAdapter {
    /// C1 `models` (owner 2026-10-12): the catalog Pi's own
    /// `get_available_models` lists by `deadline`, as `provider/id`, from
    /// one private process under VIA's agent directory, launched with a
    /// harness-process slot from `slots` and closed before this returns.
    /// The agent directory passes the turn's pre-launch checks first
    /// (packet §4.2, §4.3), so Pi never creates or reads anything there
    /// that a turn would refuse. The error is VIA's text for the caller's
    /// `harnesses[].message`.
    pub(crate) async fn discover(
        &self,
        runtime: &Arc<RouteRuntime>,
        (slots, deadline): (&Arc<tokio::sync::Semaphore>, Deadline),
    ) -> Result<Vec<CatalogModel>, String> {
        let vendor_state_dir = self.vendor_state_dir.clone();
        // Blocking filesystem checks, off the async workers; awaited, so
        // the request owns the task until it ends.
        let checked = tokio::task::spawn_blocking(move || profile_ready(&vendor_state_dir)).await;
        match checked {
            Ok(Ok(())) => {}
            Ok(Err(detail)) => return Err(detail),
            Err(_) => return Err("the Pi profile check failed".to_owned()),
        }
        let capacity = crate::discovery::slot(slots, deadline).await?;
        let owner = ProcessOwner::Server {
            server_id: via_routes::codex::ServerId::mint()
                .map_err(|_| "a process ID could not be minted".to_owned())?,
        };
        let listed = PiRoute::new(Arc::clone(runtime))
            .available_models(self.probe_spec(owner, capacity), deadline)
            .await
            .map_err(|error| match error {
                CatalogError::Deadline => crate::discovery::TIMED_OUT.to_owned(),
                CatalogError::Launch
                | CatalogError::Write
                | CatalogError::Ended
                | CatalogError::Protocol => error.to_string(),
            })?;
        Ok(listed
            .into_iter()
            .map(|(provider, id)| CatalogModel {
                model: format!("{provider}/{id}"),
                aliases: Vec::new(),
            })
            .collect())
    }
}

/// The turn's pre-launch directory checks (packet §4.2, §4.3): the managed
/// directories down to the agent directory, then the profile policy.
fn profile_ready(vendor_state_dir: &std::path::Path) -> Result<(), String> {
    let agent = match launch::managed(vendor_state_dir, &["pi", "agent"]) {
        Ok(agent) => agent,
        Err(launch::Unsafe::Refused(detail)) => return Err(detail),
        Err(launch::Unsafe::Io) => return Err("the agent directory cannot be read".to_owned()),
    };
    profile::check(&agent, profile::daemon_uid())
        .map(drop)
        .map_err(|detail| format!("the Pi profile policy refused: {detail}"))
}
