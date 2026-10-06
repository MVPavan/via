//! `Adapter::Pi` (`pi-rpc`, vendor packet `docs/specs/vendors/pi.md`):
//! pure planning ([`plan`]), the per-turn launch recipe and VIA's private
//! agent directory ([`launch`]), the profile policy ([`profile`]) and the
//! normalizer ([`normalize`]). The daemon builds it when the harness's
//! binary resolves, with the daemon's instance cache. Its driver
//! ([`run_turn`]) runs each turn on its own private `pi --mode rpc`
//! process.

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

use crate::config::BootstrapEnv;
use crate::instance::InstanceCache;
use crate::plan::CatalogModel;

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
