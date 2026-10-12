//! Server generation budgets and shared route staging (`opencode.md` §9).

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Notify;

use super::router::RouterFailure;

/// VIA-opened driver session states (`opencode.md` §9).
pub(super) const SESSION_STATES: usize = 1024;
/// Settled turn tombstones per generation (`opencode.md` §9).
pub(super) const TOMBSTONES: usize = 4096;
/// Child and descendant mappings, separate from driver sessions (§9).
pub(super) const CHILDREN: usize = 4096;
/// Vendor interactive requests whose settlement is unknown (§9).
pub(super) const INTERACTIVE: usize = 64;
/// HTTP requests without complete responses or proven withdrawal (§9).
pub(super) const REQUESTS: usize = 64;
/// Aggregate live/tombstoned correlation identities (§9).
pub(super) const KEYS: usize = 65_536;
/// Aggregate retained correlation ID bytes (§9).
pub(super) const KEY_BYTES: usize = 8 * 1024 * 1024;
/// Individual retained routing IDs and short fields (§9; runtime §8).
pub(super) const ID_BYTES: usize = 1024;
/// Route items retained across ingress lanes and active delivery (§9).
pub(super) const STAGING_MESSAGES: usize = 1024;
/// Route bytes retained across ingress lanes and active delivery (§9).
pub(super) const STAGING_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Resource {
    Session,
    Tombstone,
    Child,
    Interactive,
    Request,
}

impl Resource {
    const fn index(self) -> usize {
        match self {
            Self::Session => 0,
            Self::Tombstone => 1,
            Self::Child => 2,
            Self::Interactive => 3,
            Self::Request => 4,
        }
    }

    const fn limit(self) -> usize {
        match self {
            Self::Session => SESSION_STATES,
            Self::Tombstone => TOMBSTONES,
            Self::Child => CHILDREN,
            Self::Interactive => INTERACTIVE,
            Self::Request => REQUESTS,
        }
    }
}

#[derive(Default)]
pub(super) struct Retained {
    resources: [usize; 5],
    keys: usize,
    key_bytes: usize,
}

impl Retained {
    pub(super) fn reserve(&mut self, resource: Resource) -> Result<(), RouterFailure> {
        let used = &mut self.resources[resource.index()];
        if *used >= resource.limit() {
            return Err(RouterFailure::Overflow);
        }
        *used += 1;
        Ok(())
    }

    pub(super) fn release(&mut self, resource: Resource) {
        let used = &mut self.resources[resource.index()];
        *used = used.saturating_sub(1);
    }

    pub(super) fn key(&mut self, id: &str) -> Result<(), RouterFailure> {
        if id.len() > ID_BYTES {
            return Err(RouterFailure::Protocol);
        }
        if self.keys >= KEYS || self.key_bytes.saturating_add(id.len()) > KEY_BYTES {
            return Err(RouterFailure::Overflow);
        }
        self.keys += 1;
        self.key_bytes += id.len();
        Ok(())
    }

    pub(super) fn release_keys(&mut self, keys: usize, bytes: usize) {
        self.keys = self.keys.saturating_sub(keys);
        self.key_bytes = self.key_bytes.saturating_sub(bytes);
    }
}

pub(super) struct Failure {
    cause: Mutex<Option<RouterFailure>>,
    changed: Arc<Notify>,
}

impl Failure {
    pub(super) fn new(changed: Arc<Notify>) -> Self {
        Self {
            cause: Mutex::new(None),
            changed,
        }
    }

    pub(super) fn get(&self) -> Option<RouterFailure> {
        *self.cause.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn set(&self, cause: RouterFailure) {
        self.cause
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(cause);
        self.changed.notify_waiters();
    }
}

#[derive(Default)]
struct Staged {
    messages: usize,
    bytes: usize,
}

pub(super) struct Staging {
    used: Mutex<Staged>,
    pub(super) failure: Arc<Failure>,
}

/// Keeps one route-staging reservation through delivery and cloned buffers (§9).
#[derive(Clone)]
pub struct StagingPermit(Arc<StagingLease>);

impl fmt::Debug for StagingPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagingPermit")
            .field("bytes", &self.0.bytes)
            .finish()
    }
}

struct StagingLease {
    staging: Arc<Staging>,
    bytes: usize,
}

impl Drop for StagingLease {
    fn drop(&mut self) {
        self.staging.release(1, self.bytes);
    }
}

impl Staging {
    pub(super) fn new(failure: Arc<Failure>) -> Self {
        Self {
            used: Mutex::new(Staged::default()),
            failure,
        }
    }

    pub(super) fn reserve(self: &Arc<Self>, bytes: usize) -> Option<StagingPermit> {
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        if self.failure.get().is_some() {
            return None;
        }
        if used.messages >= STAGING_MESSAGES || used.bytes.saturating_add(bytes) > STAGING_BYTES {
            self.failure.set(RouterFailure::Overflow);
            return None;
        }
        used.messages += 1;
        used.bytes += bytes;
        Some(StagingPermit(Arc::new(StagingLease {
            staging: self.clone(),
            bytes,
        })))
    }

    pub(super) fn release(&self, messages: usize, bytes: usize) {
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        used.messages = used.messages.saturating_sub(messages);
        used.bytes = used.bytes.saturating_sub(bytes);
    }
}
