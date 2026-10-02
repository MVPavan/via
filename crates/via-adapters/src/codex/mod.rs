//! `Adapter::Codex` (`codex-app-server`, adapter design §6): a stub until
//! via-5lr.3.2 fills it. The daemon builds it when the harness's binary
//! resolves, with the daemon's instance cache; until then it plans
//! nothing and runs no turn, so `plan`, `check_turn` and its driver refuse
//! `harness_unavailable` exactly as before it existed.

use std::path::PathBuf;
use std::sync::Arc;

use crate::instance::InstanceCache;

/// The harness this adapter serves, a [`crate::HARNESSES`] name.
pub(crate) const HARNESS: &str = "codex";

/// The Codex adapter.
pub(crate) struct CodexAdapter {
    /// The resolved vendor binary (design §5.4).
    #[expect(dead_code, reason = "the server launch runs it (via-5lr.3.2)")]
    binary: PathBuf,
    /// The daemon's instance cache (C2 §5 AD7).
    #[expect(dead_code, reason = "the handshake check records in it (via-5lr.3.2)")]
    instances: Arc<InstanceCache>,
}

impl CodexAdapter {
    pub(crate) fn new(binary: PathBuf, instances: Arc<InstanceCache>) -> Self {
        Self { binary, instances }
    }
}
