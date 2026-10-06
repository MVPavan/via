//! The `OpenCode` adapter (`vendors/opencode.md`): the launch recipe and
//! the shared server's acquisition. The session driver, `plan` and
//! `describe` arrive with the adapter surface (chunk B); until then the
//! module is reached only by its tests.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use via_routes::opencode::{LaunchFailure, Prepare, ServerPin, Servers};

use crate::private_dir::Unsafe;
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
        let key = self.recipe.server_key(ADAPTER_VERSION);
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
