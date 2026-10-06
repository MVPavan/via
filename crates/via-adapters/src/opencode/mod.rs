//! The `OpenCode` adapter (`vendors/opencode.md`): the launch recipe and
//! the shared server's acquisition. The session driver, `plan` and
//! `describe` arrive with the adapter surface (chunk B); until then the
//! module is reached only by its tests.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use via_routes::opencode::{LaunchFailure, ServerPin, Servers};

use crate::{CapacityToken, ProcessOwner};

mod launch;

#[cfg(test)]
#[path = "serve_tests.rs"]
mod serve_tests;

/// The adapter version, part of the launch key (§3.1).
pub(crate) const ADAPTER_VERSION: &str = "1";

/// The versions that run (§12).
pub(crate) const CHECKED: &[&str] = &["2.0.22"];

/// The route's shared server, as the adapter acquires it: its recipe and
/// the registry.
pub(crate) struct OpenCodeServers {
    recipe: launch::ServerRecipe,
    servers: Arc<Servers>,
}

impl OpenCodeServers {
    /// The server of `binary` under the registry's vendor state directory,
    /// with `path` as its `PATH`.
    pub(crate) fn new(binary: &Path, path: Option<&OsStr>, servers: Arc<Servers>) -> Self {
        Self {
            recipe: launch::ServerRecipe::new(binary, path, servers.vendor_state_dir()),
            servers,
        }
    }

    /// A pin on the live or launching server, or a new launch holding
    /// `capacity`: the namespace and probe directories are created first
    /// (the anchor's cwd and lock live there), where missing.
    pub(crate) fn acquire(
        &self,
        owner: ProcessOwner,
        capacity: CapacityToken,
    ) -> Result<ServerPin, LaunchFailure> {
        let key = self.recipe.server_key(ADAPTER_VERSION);
        if let Some(pin) = self.servers.pin(&key) {
            return Ok(pin);
        }
        self.recipe
            .namespace()
            .create()
            .and_then(|()| self.recipe.probe().create())
            .map_err(|_| LaunchFailure::Transient {
                step: "create the namespace directory",
            })?;
        self.servers
            .launch_or_join(key, self.recipe.launch(owner, CHECKED), capacity)
    }
}
