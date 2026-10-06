//! The `OpenCode` adapter (`vendors/opencode.md`): the launch recipe and
//! the shared server's acquisition. The session driver, `plan` and
//! `describe` arrive with the adapter surface (chunk B); until then the
//! module is reached only by its tests.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use via_routes::opencode::{LaunchFailure, ServerPin, Servers};

use crate::private_dir::Unsafe;
use crate::{CapacityToken, Deadline, ProcessOwner, TaskTracker};

mod launch;

#[cfg(test)]
#[path = "serve_tests.rs"]
mod serve_tests;

/// The adapter version, part of the launch key (§3.1).
pub(crate) const ADAPTER_VERSION: &str = "1";

/// The versions that run (§12).
pub(crate) const CHECKED: &[&str] = &["2.0.22"];

/// Why an acquisition failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AcquireError {
    /// The registry's launch failed, or was refused.
    Launch(LaunchFailure),
    /// A managed directory is not private (runtime §6.1): VIA's text
    /// naming it and the rule, never a value. Nothing was launched.
    Unsafe(String),
    /// The directory step failed, or did not finish by the caller's
    /// deadline: the step. Nothing was launched.
    Prepare(&'static str),
}

/// The route's shared server, as the adapter acquires it: its recipe, the
/// registry, and the tracker owning the blocking directory steps.
pub(crate) struct OpenCodeServers {
    recipe: Arc<launch::ServerRecipe>,
    servers: Arc<Servers>,
    blocking: TaskTracker,
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
            blocking: TaskTracker::new(),
        }
    }

    /// A pin on the live or launching server, or a new launch holding
    /// `capacity`. Before a new launch, the managed directories (the
    /// namespace and the probe root, from VIA's `vendor/` down) are
    /// checked and created where missing, on a blocking task bounded by
    /// `deadline`; at the deadline the task is left to finish and its
    /// result is never used.
    pub(crate) async fn acquire(
        &self,
        owner: ProcessOwner,
        capacity: CapacityToken,
        deadline: Deadline,
    ) -> Result<ServerPin, AcquireError> {
        let key = self.recipe.server_key(ADAPTER_VERSION);
        if let Some(pin) = self.servers.pin(&key) {
            return Ok(pin);
        }
        let recipe = Arc::clone(&self.recipe);
        let prepared = self.blocking.spawn_blocking(move || recipe.prepare());
        match tokio::time::timeout_at(deadline.instant(), prepared).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(Unsafe::Refused(detail)))) => {
                return Err(AcquireError::Unsafe(detail));
            }
            Ok(Ok(Err(Unsafe::Io)) | Err(_)) => {
                return Err(AcquireError::Prepare("create the namespace directory"));
            }
            Err(_) => {
                return Err(AcquireError::Prepare(
                    "check the namespace directory by the deadline",
                ));
            }
        }
        self.servers
            .launch_or_join(key, self.recipe.launch(owner, CHECKED), capacity)
            .map_err(AcquireError::Launch)
    }
}
