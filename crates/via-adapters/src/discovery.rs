//! C1 `models` with on-demand discovery (owner 2026-10-12; C2 §2
//! `discover_models`): every configured harness's catalog, or one's, where
//! a harness whose live catalog is not known has VIA's own server or
//! process fetch it. Harnesses are discovered concurrently, all by Core's
//! one absolute deadline. A harness that is not installed, or whose
//! discovery fails or outlives the deadline, does not fail the call: its
//! `harnesses` entry says so, and only Pi falls back to its bundled
//! catalog.
//!
//! Every process discovery starts holds a harness-process slot, as a
//! turn's does, and stops within the call's bounds: a shared server
//! (Codex's, `OpenCode`'s) through its registry's ordinary last release,
//! Pi's private process before its probe returns.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Semaphore;

use crate::harness::{HARNESSES, Harness};
use crate::plan::{Adapter, AdapterSet, CatalogModel, Inherit, ModelEntry, ModelSource};
use crate::{CapacityToken, Deadline, claude, codex};

/// The message of a discovery that outlived its deadline.
pub(crate) const TIMED_OUT: &str = "discovery timed out";

/// One harness's discovery outcome (C1 §3.13 `harnesses[].status`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStatus {
    /// The catalog is current: bundled, or read from a live instance.
    Ok,
    /// The harness's binary was not found when the daemon started.
    Unavailable,
    /// Discovery failed or timed out; the entries are the fallback.
    Error,
}

/// One C1 §3.13 `harnesses` entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HarnessCatalog {
    /// The harness.
    pub harness: &'static str,
    /// Its outcome.
    pub status: CatalogStatus,
    /// VIA's text: why it failed, or a note on the catalog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// C1 §3.13 `models`, with discovery.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelsReport {
    /// Every harness's entries, in C1 harness order.
    pub models: Vec<ModelEntry>,
    /// One outcome per harness listed, in the same order.
    pub harnesses: Vec<HarnessCatalog>,
}

/// One harness's entries and outcome.
type Listed = (Vec<ModelEntry>, HarnessCatalog);

impl AdapterSet {
    /// C1 §3.13 `models`: each harness's catalog, or `harness`'s only,
    /// discovering a live catalog where none is known, by `deadline`, with
    /// harness-process slots from `slots`. Every vendor harness of this
    /// build is listed, an unconfigured one `unavailable`; the fake only
    /// when configured; an unknown name lists nothing.
    pub async fn discover_models(
        &self,
        harness: Option<&str>,
        (slots, deadline): (&Arc<Semaphore>, Deadline),
    ) -> ModelsReport {
        let wanted = |name: &str| harness.is_none_or(|harness| harness == name);
        let one = |row: &'static crate::HarnessRow| async move {
            if !wanted(row.name) {
                return None;
            }
            Some(match self.adapter(Harness::Vendor(row)) {
                Some(adapter) => self.listed(adapter, (slots, deadline)).await,
                None => (
                    Vec::new(),
                    outcome(
                        row.name,
                        CatalogStatus::Unavailable,
                        Some("the harness binary was not found when the daemon started".into()),
                    ),
                ),
            })
        };
        // Four fixed rows (HARNESSES), joined in place: no task is spawned.
        let (first, second, third, fourth) = match HARNESSES {
            [first, second, third, fourth] => {
                tokio::join!(one(first), one(second), one(third), one(fourth))
            }
            // The table's length is fixed at four (harness.rs).
            _ => (None, None, None, None),
        };
        let fake = match self.adapter(Harness::Fake) {
            Some(adapter) if wanted(crate::FAKE) => {
                Some(self.listed(adapter, (slots, deadline)).await)
            }
            Some(_) | None => None,
        };
        let mut report = ModelsReport::default();
        for (models, catalog) in [first, second, third, fourth, fake].into_iter().flatten() {
            report.models.extend(models);
            report.harnesses.push(catalog);
        }
        report
    }

    /// One configured adapter's entries and outcome.
    async fn listed(
        &self,
        adapter: Adapter<'_>,
        (slots, deadline): (&Arc<Semaphore>, Deadline),
    ) -> Listed {
        let name = adapter.name();
        let (catalog, source, catalog_outcome) = match adapter {
            Adapter::Fake(fake) => (fake.catalog().to_vec(), ModelSource::Bundled, Ok(None)),
            Adapter::Claude(claude) => (
                claude.catalog().to_vec(),
                ModelSource::Bundled,
                Ok(Some(claude::MODELS_NOTE.to_owned())),
            ),
            Adapter::Codex(codex) => {
                // The table always names the harness.
                let requested = Harness::parse(codex::HARNESS)
                    .map_or(Inherit::OD2_DEFAULT, |harness| self.config.inherit(harness));
                // A known catalog, live or saved, is never an error.
                match codex.discover(requested, (slots, deadline)).await {
                    Ok(catalog) => (catalog, ModelSource::Discovered, Ok(None)),
                    Err(message) => (Vec::new(), ModelSource::Discovered, Err(message)),
                }
            }
            Adapter::OpenCode(opencode) => match opencode.discover((slots, deadline)).await {
                Ok(catalog) => (catalog, ModelSource::Discovered, Ok(None)),
                Err(message) => (Vec::new(), ModelSource::Discovered, Err(message)),
            },
            Adapter::Pi(pi) => match pi.discover(&self.runtime, (slots, deadline)).await {
                Ok(catalog) => (catalog, ModelSource::Discovered, Ok(None)),
                Err(message) => (pi.catalog().to_vec(), ModelSource::Bundled, Err(message)),
            },
        };
        let catalog_outcome = match catalog_outcome {
            Ok(note) => outcome(name, CatalogStatus::Ok, note),
            Err(message) => outcome(name, CatalogStatus::Error, Some(message)),
        };
        (entries(name, catalog, source), catalog_outcome)
    }
}

fn outcome(
    harness: &'static str,
    status: CatalogStatus,
    message: Option<String>,
) -> HarnessCatalog {
    HarnessCatalog {
        harness,
        status,
        message,
    }
}

fn entries(
    harness: &'static str,
    catalog: Vec<CatalogModel>,
    source: ModelSource,
) -> Vec<ModelEntry> {
    catalog
        .into_iter()
        .map(|entry| ModelEntry {
            model: entry.model,
            harness,
            aliases: entry.aliases,
            source,
        })
        .collect()
}

/// A harness-process slot for a discovery launch (design §3.1), waited
/// for in the pool's FIFO order by `deadline`.
pub(crate) async fn slot(
    slots: &Arc<Semaphore>,
    deadline: Deadline,
) -> Result<CapacityToken, String> {
    match tokio::time::timeout_at(deadline.instant(), Arc::clone(slots).acquire_owned()).await {
        Ok(Ok(permit)) => Ok(Box::new(permit)),
        // The pool is never closed while the daemon serves.
        Ok(Err(_closed)) => Err("no harness-process slot is available".to_owned()),
        Err(_elapsed) => Err(format!(
            "{TIMED_OUT} waiting for a free harness-process slot"
        )),
    }
}
