//! `Adapter::OpenCode` (`opencode-serve`, `docs/specs/vendors/opencode.md`):
//! one VIA-owned `opencode serve --stdio` for all of VIA (§3), its launch
//! recipe ([`launch`]), pure planning ([`plan`]) and the session driver
//! ([`driver`]). The daemon builds it when the harness's binary resolves,
//! with the daemon's instance cache and the Route runtime, over which it
//! keeps the shared-server registry.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use via_routes::RouteRuntime;
use via_routes::opencode::{LaunchFailure, Prepare, ServerKey, ServerPin, Servers};

use crate::instance::InstanceCache;
use crate::plan::CatalogModel;
use crate::private_dir::Unsafe;
use crate::{CapacityToken, ProcessOwner};

mod control;
mod delivery;
mod driver;
mod execution;
mod launch;
mod normalize;
mod plan;
mod request;

#[cfg(test)]
mod normalize_tests;

#[cfg(test)]
mod driver_tests;

#[cfg(test)]
mod driver_control_tests;

#[cfg(test)]
mod driver_request_tests;

#[cfg(test)]
mod driver_http_limit_tests;

#[cfg(test)]
mod plan_tests;
#[cfg(test)]
#[path = "serve_tests.rs"]
mod serve_tests;

pub(crate) use driver::{OpenCodeSession, connection_id, run_turn};

/// The harness this adapter serves, a [`crate::HARNESSES`] name.
pub(crate) const HARNESS: &str = "opencode";

/// The adapter version, part of the launch key (§3.1).
pub(crate) const ADAPTER_VERSION: &str = "1";

/// The versions that run (§12).
pub(crate) const CHECKED: &[&str] = &["2.0.22"];

/// The adapter version a session's turns record (AD12).
pub(crate) fn adapter_version() -> String {
    ADAPTER_VERSION.to_owned()
}

/// The `OpenCode` adapter.
pub(crate) struct OpenCodeAdapter {
    /// The resolved vendor binary.
    binary: PathBuf,
    /// The daemon's instance cache (C2 §5 AD7).
    instances: Arc<InstanceCache>,
    /// The one server's recipe and the registry.
    servers: OpenCodeServers,
}

impl OpenCodeAdapter {
    /// The adapter of `binary`, whose server gets `path` as its `PATH`
    /// (the daemon's, as Codex's does), over the Route runtime.
    pub(crate) fn new(
        binary: PathBuf,
        instances: Arc<InstanceCache>,
        path: Option<&OsStr>,
        runtime: Arc<RouteRuntime>,
    ) -> Self {
        let servers = OpenCodeServers::new(&binary, path, Servers::new(runtime));
        Self {
            binary,
            instances,
            servers,
        }
    }

    /// The shared-server registry.
    pub(crate) fn registry(&self) -> &Arc<Servers> {
        &self.servers.servers
    }

    /// The live server as `daemon/status` lists it (C1 §3.14).
    pub(crate) fn server_reports(&self) -> Vec<crate::plan::ServerReport> {
        self.servers
            .servers
            .reports()
            .into_iter()
            .map(|report| crate::plan::ServerReport {
                harness: HARNESS,
                vendor_version: Some(report.version),
                key: crate::plan::ServerKey::new(report.key),
                sessions: report.sessions,
            })
            .collect()
    }

    /// §6 `models`: the live server's catalog, its public fields only;
    /// none while no server is live (no bundled entries).
    pub(crate) fn listed(&self) -> Vec<CatalogModel> {
        self.servers
            .servers
            .live_facts(&self.servers.key())
            .iter()
            .flat_map(|facts| facts.models.iter())
            .map(|model| CatalogModel {
                model: format!("{}/{}", model.provider_id, model.id),
                aliases: Vec::new(),
            })
            .collect()
    }
}

/// The route's shared server, as the adapter acquires it: its recipe and
/// the registry.
pub(crate) struct OpenCodeServers {
    recipe: Arc<launch::ServerRecipe>,
    servers: Arc<Servers>,
}

impl OpenCodeServers {
    /// The server of `binary` under the registry's vendor state directory,
    /// with `path` as its `PATH`.
    pub(crate) fn new(binary: &Path, path: Option<&OsStr>, servers: Arc<Servers>) -> Self {
        Self {
            recipe: Arc::new(launch::ServerRecipe::new(
                binary,
                path,
                servers.vendor_state_dir(),
            )),
            servers,
        }
    }

    /// The registry's key of the one server (§3.1).
    pub(crate) fn key(&self) -> ServerKey {
        self.recipe.server_key(ADAPTER_VERSION)
    }

    /// The key as a plan names it: its full digest in hex.
    pub(crate) fn key_hex(&self) -> String {
        launch::hex(&self.key().0)
    }

    /// The recipe hash in hex (§3.1), which every refusal key starts from
    /// (C2 §5).
    pub(crate) fn recipe_hex(&self) -> String {
        launch::hex(&self.recipe.recipe_hash(ADAPTER_VERSION))
    }

    /// The cache key of a server-level handshake refusal (§2.2): the
    /// recipe hash, with C2 §5's program identity beside it.
    pub(crate) fn refusal_key(&self) -> String {
        format!("server:{}", self.recipe_hex())
    }

    /// C2 §3 `prepare`: a pin on the live or launching server.
    pub(crate) fn pin(&self) -> Option<ServerPin> {
        self.servers.pin(&self.key())
    }

    /// A pin on the live or launching server, or a new launch holding
    /// `capacity`. A new launch first checks and creates the managed
    /// directories (the namespace and the probe root, from VIA's `vendor/`
    /// down, runtime §6.1) on the registry's launch task, the job's one
    /// owner: a refusal is that launch's [`LaunchFailure::Unsafe`], and a
    /// caller that stops waiting leaves the job to the registry, which
    /// collects it through its fence and join.
    pub(crate) fn acquire(
        &self,
        owner: ProcessOwner,
        capacity: CapacityToken,
    ) -> Result<ServerPin, LaunchFailure> {
        let key = self.key();
        if let Some(pin) = self.servers.pin(&key) {
            return Ok(pin);
        }
        let recipe = Arc::clone(&self.recipe);
        let prepare: Prepare = Box::new(move || {
            recipe.prepare().map_err(|unsafe_dir| match unsafe_dir {
                Unsafe::Refused(detail) => LaunchFailure::Unsafe { detail },
                Unsafe::Io => LaunchFailure::Transient {
                    step: "create the namespace directory",
                },
            })
        });
        self.servers
            .launch_or_join(key, self.recipe.launch(owner, CHECKED, prepare), capacity)
    }
}
