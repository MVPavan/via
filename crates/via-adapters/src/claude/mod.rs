//! `Adapter::Claude` (`claude-cli`, adapter design §6; vendor packet
//! `docs/specs/vendors/claude-code.md`): pure planning ([`plan`]), the
//! per-turn launch recipe ([`launch`]) and the stream normalizer
//! ([`normalize`]). The daemon builds it when the harness's binary
//! resolves, with the daemon's instance cache. Its driver ([`run_turn`])
//! runs each turn on its own private process.

mod driver;
mod launch;
mod normalize;
mod plan;

pub(crate) use driver::{connection_id, run_turn};
pub(crate) use plan::adapter_version;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{BootstrapEnv, ClaudeMode};
use crate::instance::InstanceCache;
use crate::plan::CatalogModel;

/// The harness this adapter serves, a [`crate::HARNESSES`] name.
pub(crate) const HARNESS: &str = "claude";

/// The Claude Code adapter.
pub(crate) struct ClaudeAdapter {
    /// The resolved vendor binary (design §5.4).
    binary: PathBuf,
    /// The daemon's instance cache (C2 §5 AD7).
    instances: Arc<InstanceCache>,
    /// The bundled catalog (packet §2).
    catalog: Vec<CatalogModel>,
    /// The launch environment: the packet's allow-list, as captured at
    /// daemon start (packet §4, B7).
    env: Vec<(OsString, OsString)>,
    /// The configured launch mode (`harnesses.claude.restricted`).
    mode: ClaudeMode,
}

impl ClaudeAdapter {
    pub(crate) fn new(
        binary: PathBuf,
        instances: Arc<InstanceCache>,
        env: &BootstrapEnv,
        mode: ClaudeMode,
    ) -> Self {
        Self {
            binary,
            instances,
            catalog: plan::catalog(),
            env: launch::allowed_env(env),
            mode,
        }
    }
}
