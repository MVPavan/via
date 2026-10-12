//! C1 `models` discovery for Codex (owner 2026-10-12): the catalog of the
//! server key a default session of the harness's configured `inherit`
//! takes. A known catalog (a live instance's, else the saved snapshot of
//! the key's last complete discovery) is read as it stands; with none, VIA's
//! own `codex app-server` of that key is launched (or joined) through the
//! shared-server registry, exactly as a turn's, and pinned only until its
//! handshake's `model/list` was read. Dropping the pin is the registry's
//! ordinary last release: the server retires at once unless a session took
//! it meanwhile, and a launch still running at the deadline is dropped by
//! the registry once it publishes with no holder.

use std::sync::Arc;

use via_routes::codex::{ServerFacts, ServerId, ServerKey};

use super::driver::{ensure_home, handshake_bound, mark_initialized};
use super::normalize::{self, DiscoveredModel};
use super::{ADAPTER_VERSION, CodexAdapter, HARNESS};
use crate::passthrough::VendorArgs;
use crate::plan::{CatalogModel, Inherit};
use crate::{Deadline, ProcessOwner};

impl CodexAdapter {
    /// Records what live instance `id` of server key `key` reported in its
    /// handshake: its version in the instance cache and its whole catalog
    /// as the key's discovered one. Returns the catalog, and whether it
    /// replaced the saved snapshot, which the caller then persists.
    pub(super) fn learned(
        &self,
        key: String,
        id: &ServerId,
        server: &ServerFacts,
    ) -> (Arc<[DiscoveredModel]>, bool) {
        let version = normalize::instance_version(&server.user_agent).map(str::to_owned);
        if let Some(version) = &version {
            self.instances
                .record_version(HARNESS, &self.binary, version.clone());
        }
        let catalog: Arc<[DiscoveredModel]> =
            server.models.iter().map(normalize::discovered).collect();
        let replaced = self.discovered(key, id.clone(), Arc::clone(&catalog), version);
        (catalog, replaced)
    }

    /// The catalog of `requested`'s default server key (C1 §3.13): its live
    /// instance's or saved one, else one VIA's own server of that key reports by
    /// `deadline`, launched with a harness-process slot from `slots`. The
    /// error is VIA's text for the caller's `harnesses[].message`.
    pub(crate) async fn discover(
        &self,
        requested: Inherit,
        (slots, deadline): (&Arc<tokio::sync::Semaphore>, Deadline),
    ) -> Result<Vec<CatalogModel>, String> {
        let vendor_args = VendorArgs::default();
        let key = self.server_key(requested, &vendor_args);
        // Known already: a live instance's, else the saved snapshot of
        // the key's last complete discovery (C2 §2, packet §3).
        if let Some(catalog) = self.model_catalog(&key) {
            return Ok(listing(&catalog));
        }
        let home = self.vendor_home();
        ensure_home(&home).map_err(|_| "VIA's Codex state directory is not usable".to_owned())?;
        let capacity = crate::discovery::slot(slots, deadline).await?;
        let recipe = self.recipe(requested, &vendor_args);
        // The registry replaces the owner with the server it mints.
        let owner = ProcessOwner::Server {
            server_id: ServerId::mint()
                .map_err(|_| "a server ID could not be minted".to_owned())?,
        };
        let pin = self
            .servers
            .launch_or_join(
                ServerKey(recipe.config_hash(ADAPTER_VERSION).bytes()),
                (recipe.process_spec(owner), handshake_bound(&home)),
                capacity,
            )
            .map_err(|failure| launch_message(&failure))?;
        match pin
            .ready(tokio::time::sleep_until(deadline.instant()))
            .await
        {
            Ok(()) => mark_initialized(&home),
            Err(Some(error)) => return Err(launch_message(&error.failure)),
            Err(None) => return Err(crate::discovery::TIMED_OUT.to_owned()),
        }
        let (_, facts) = pin
            .live()
            .ok_or_else(|| "the server ended before its catalog was read".to_owned())?;
        let (catalog, replaced) = self.learned(key, pin.server(), &facts);
        // The last release: the server retires unless a session holds it.
        drop(pin);
        if replaced {
            let saved = Arc::clone(&self.saved_catalog);
            let vendor = self.servers.vendor_state_dir().to_path_buf();
            // Packet §3: best effort, awaited here so the request owns it;
            // a failed write keeps the earlier snapshot.
            let _persisted = tokio::task::spawn_blocking(move || saved.persist(&vendor)).await;
        }
        Ok(listing(&catalog))
    }
}

/// The catalog as C1 lists it: every model, no aliases.
fn listing(catalog: &[DiscoveredModel]) -> Vec<CatalogModel> {
    catalog
        .iter()
        .map(|model| CatalogModel {
            model: model.model.clone(),
            aliases: Vec::new(),
        })
        .collect()
}

/// A failed launch as the caller reads it: VIA's text, naming Host's
/// failed step when it named one, never vendor output.
fn launch_message(failure: &via_routes::codex::LaunchFailure) -> String {
    use via_routes::codex::LaunchFailure;
    match failure {
        LaunchFailure::Acquire {
            launch: Some(cause),
            ..
        } => format!("the Codex server could not be started ({})", cause.step),
        LaunchFailure::Acquire { launch: None, .. } | LaunchFailure::Internal => {
            "the Codex server could not be started".to_owned()
        }
        LaunchFailure::Protocol(detail) => {
            format!("the Codex server's handshake failed: {detail}")
        }
        LaunchFailure::Lost(_) => {
            "the Codex server exited or its connection was lost during its handshake".to_owned()
        }
        LaunchFailure::Deadline => "the Codex server's handshake timed out".to_owned(),
        LaunchFailure::Shutdown => "the daemon is shutting down".to_owned(),
    }
}
