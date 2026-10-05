//! `daemon/status` (C1 §3.14; design §6.6, Task 4 design §11.2): the
//! session counts, the connection slots, the daemon config's `limits` and
//! `storage`; and the disk floor and WAL limit applied to new work (Task 4
//! design §5.3, §5.4).

use std::{
    collections::HashSet,
    num::NonZeroU32,
    sync::{Arc, atomic::Ordering},
    time::{Duration, SystemTime},
};

use serde_json::{Value, json};
use via_store::{StoreError, WalLimits};

use super::drive::Step;
use super::queue::{DEFAULT_CONNECTION_SLOTS, Slot};
use super::{Engine, lock};
use crate::api::rfc3339;
use crate::{ApiError, DescribeParams, ModelsParams, SessionId, TurnNumber};

/// The `connections` object of `daemon/status` (design §6.6, amendment A9).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Connections {
    /// The connection-slot pool's size.
    pub limit: usize,
    /// Permits out: live groups, reservations and every held permit.
    pub in_use: usize,
    /// Permits held for groups whose absence is unproven: those
    /// `RecoveredSlots` holds for an earlier daemon's groups, plus Host
    /// ledger entries whose close was uncertain.
    pub held_unproven: usize,
}

/// The counts `daemon/status` reports from memory; none reads the Store.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DaemonCounts {
    /// Sessions not closed and neither active nor closing (Task 4 design
    /// §11.2): `open − closing − active`, saturating.
    pub idle: usize,
    /// Distinct sessions with an unresolved turn that are not closing.
    pub active: usize,
    /// The durable closing set's size (`sessions.closing` [r3.5]).
    pub closing: usize,
    /// Connection slots.
    pub connections: Connections,
}

/// Daemon config's thresholds (Task 4 design §5.5), read once at start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Below this much free space new work is refused; 0 turns it off.
    pub free_floor: u64,
    /// `storage.over_warn_size` once VIA's data is larger.
    pub warn_size: u64,
    /// The WAL thresholds (§5.4).
    pub wal: WalLimits,
    /// The connection-slot pool's size (runtime §8; bead via-oq3):
    /// `daemon/status` reports it as `connections.limit`.
    pub connection_slots: NonZeroU32,
}

impl Default for Limits {
    /// Design §5.5's defaults: a 5 GiB floor, a 2 GiB warning, the
    /// default WAL thresholds and 8 connection slots.
    fn default() -> Self {
        Self {
            free_floor: 5 * 1024 * 1024 * 1024,
            warn_size: 2 * 1024 * 1024 * 1024,
            wal: WalLimits::default(),
            connection_slots: DEFAULT_CONNECTION_SLOTS,
        }
    }
}

impl Limits {
    /// `daemon/status` `limits` (A37): the five effective values.
    pub fn to_value(&self) -> Value {
        json!({"disk":{"free_floor":self.free_floor,"warn_size":self.warn_size},
            "wal":{"max":self.wal.max,"checkpoint_bytes":self.wal.checkpoint_bytes,
                "checkpoint_commits":self.wal.checkpoint_commits}})
    }
}

/// One data-size walk's result (design §5.3); `None` for a walk that
/// failed or overran its 2 s step.
pub(super) struct DataSize {
    bytes: Option<u64>,
    measured_at: String,
    at: tokio::time::Instant,
}

/// A data-size walk older than this is recomputed (design §5.3).
const DATA_SIZE_TTL: Duration = Duration::from_secs(60);

/// A queued turn's `failure.message` below the floor (design §5.3).
const BELOW_FLOOR: &str = "free space on the State directory's filesystem is below disk.free_floor";

/// A queued turn's `failure.message` when the free space is unknown.
const FREE_UNREAD: &str = "free space on the State directory's filesystem could not be read";

/// A queued turn's `failure.message` at `wal.max` (design §5.4).
const WAL_AT_MAX: &str = "the Store WAL is at wal.max; new work is refused";

impl Engine {
    /// The in-memory counts of `daemon/status` (design §6.6, Task 4 design
    /// §11.2). Each lock is a short `std` mutex taken alone; no wake.
    pub fn counts(&self) -> DaemonCounts {
        let unresolved: HashSet<SessionId> = self
            .unresolved
            .turns()
            .into_iter()
            .map(|(session, _)| session)
            .collect();
        let (closing, active) = {
            let closing = lock(&self.closing);
            let active = unresolved
                .iter()
                .filter(|session| !closing.contains(*session))
                .count();
            (closing.len(), active)
        };
        let open = self.open_sessions.load(Ordering::Acquire);
        DaemonCounts {
            idle: open.saturating_sub(closing).saturating_sub(active),
            active,
            closing,
            connections: self.connections(),
        }
    }

    /// `connections` (design §6.6). Host's held entries include the
    /// recovered groups it holds for an earlier daemon; those count once,
    /// through the permits `RecoveredSlots` holds for them.
    pub fn connections(&self) -> Connections {
        let recovered = self.recovered.held();
        let host = self.adapter.held_unproven();
        let limit = self.slot_limit;
        Connections {
            limit,
            in_use: limit.saturating_sub(self.slots.available_permits()),
            held_unproven: recovered
                .permits
                .saturating_add(host.saturating_sub(recovered.identified)),
        }
    }

    /// Whether any held group waits for a proof (design §8): a recovered
    /// group, read or unread, or a Host ledger entry with no live control.
    pub(super) fn holdings(&self) -> usize {
        let recovered = self.recovered.held();
        recovered
            .identified
            .saturating_add(recovered.unidentified)
            .saturating_add(self.adapter.held_unproven())
    }

    /// Whether the durable closing set is empty; taken alone.
    pub(super) fn closing_empty(&self) -> bool {
        lock(&self.closing).is_empty()
    }

    /// `describe` (Task 4 design §4.6): the route one set of parameters
    /// would take, with nothing written.
    pub fn describe(&self, params: &DescribeParams) -> Result<Value, ApiError> {
        params.describe(&self.adapter)
    }

    /// `models` (Task 4 design §4.6): each configured harness's catalog.
    pub fn models(&self, params: &ModelsParams) -> Value {
        params.models(&self.adapter)
    }

    /// `daemon/status.servers` (C1 §3.14, C2 §2 `servers`): the adapters'
    /// live shared servers, from memory.
    pub fn servers(&self) -> Vec<via_adapters::ServerReport> {
        self.adapter.servers()
    }

    /// When this Engine opened (Task 4 design §11.2).
    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    /// The daemon config's thresholds (Task 4 design §5.5).
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// A spawn receipt committed a new session (design §11.2).
    pub(super) fn session_opened(&self) {
        self.open_sessions.fetch_add(1, Ordering::AcqRel);
    }

    /// The Store answered that a session is closed now (design §11.2).
    pub(super) fn session_closed(&self) {
        let _ = self
            .open_sessions
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |open| {
                Some(open.saturating_sub(1))
            });
    }

    /// `daemon/status` `storage` (Task 4 design §5.3): the free space read
    /// now, and VIA's data size from a walk at most 60 s old. A call that
    /// finds the cached walk absent or older recomputes it on the blocking
    /// pool; concurrent calls wait on the one walk and share its result. A
    /// value that cannot be read is `null`; a walk that fails or overruns
    /// is cached as `null` too, so at most one walk starts per minute
    /// whatever its outcome (§15). Both reads are diagnostic steps: each
    /// needs one of the Engine's two diagnostic permits, and without one
    /// its value is `null` (a walk's for its minute).
    pub async fn storage(&self) -> Value {
        // A diagnostic step (round 2): with no permit the free space is
        // not read and reports `null`.
        let free = match Arc::clone(&self.diagnostics).try_acquire_owned() {
            Ok(permit) => self.store.free_bytes(permit).await.ok(),
            Err(_) => None,
        };
        let data = {
            let mut cached = self.data_size.lock().await;
            let fresh = cached
                .as_ref()
                .is_some_and(|size| size.at.elapsed() < DATA_SIZE_TTL);
            if !fresh {
                // Test builds: each walk is counted (design §13.1).
                #[cfg(feature = "test-failpoints")]
                let _ = via_store::failpoint::hit_async("core.data_size.walks").await;
                let measured_at = rfc3339(SystemTime::now());
                // A walk with no diagnostic permit counts as failed.
                let bytes = match Arc::clone(&self.diagnostics).try_acquire_owned() {
                    Ok(permit) => self.store.data_bytes(permit).await.ok(),
                    Err(_) => None,
                };
                *cached = Some(DataSize {
                    bytes,
                    measured_at,
                    at: tokio::time::Instant::now(),
                });
            }
            cached
                .as_ref()
                .and_then(|size| Some((size.bytes?, size.measured_at.clone())))
        };
        let floor = self.limits.free_floor;
        json!({
            "free_bytes":free,
            "data_bytes":data.as_ref().map(|(bytes, _)| *bytes),
            "data_measured_at":data.as_ref().map(|(_, at)| at),
            "below_free_floor":free.map(|free| free < floor),
            "over_warn_size":data.map(|(bytes, _)| bytes > self.limits.warn_size),
        })
    }

    /// A receipt's free-space read, taken before `admission` (design
    /// §5.3); `None` when `disk.free_floor` is 0.
    pub(super) async fn free_space(&self) -> Option<Result<u64, StoreError>> {
        if self.limits.free_floor == 0 {
            return None;
        }
        // Turn-critical: outside the diagnostics cap.
        Some(self.store.free_bytes(()).await)
    }

    /// Applies a receipt's free-space read to new work, after the key
    /// lookup found no key (design §5.3 [t4r18.1]): below the floor is
    /// `admission_refused` `disk_free_floor`; an unreadable free space is
    /// `store_error` `not_committed`. Nothing has been written.
    pub(super) fn floor_admits(
        &self,
        free: Option<&Result<u64, StoreError>>,
    ) -> Result<(), ApiError> {
        let floor = self.limits.free_floor;
        match free {
            None => Ok(()),
            Some(Ok(free)) if *free >= floor => Ok(()),
            Some(Ok(free)) => Err(ApiError::disk_free_floor(*free, floor)),
            Some(Err(_)) => Err(ApiError::RECEIPT_NOT_COMMITTED),
        }
    }

    /// Why a queued turn's dispatch fails `store` before submission (design
    /// §5.3, §5.4): the WAL at `wal.max`, or free space below the floor.
    /// It fails rather than waits: nothing would wake a waiting dispatcher.
    pub(super) async fn dispatch_refusal(&self) -> Option<&'static str> {
        if self.store.wal_full() {
            return Some(WAL_AT_MAX);
        }
        match self.free_space().await {
            None => None,
            Some(Ok(free)) if free >= self.limits.free_floor => None,
            Some(Ok(_)) => Some(BELOW_FLOOR),
            Some(Err(_)) => Some(FREE_UNREAD),
        }
    }

    /// Fails the claimed queue head `turn` `failed(store)` with `message`
    /// and no agent I/O (design §5.3): `turn.submitted` and `turn.ended` in
    /// one lifecycle write, which the floor and the WAL limit never refuse.
    /// A queueing that cannot be read rolls the claim back for the read
    /// retry.
    pub(super) async fn fail_at_dispatch(
        &self,
        slot: &Slot,
        session: &SessionId,
        turn: TurnNumber,
        message: &str,
    ) -> Step {
        let Ok(queueing) = self.queueing(session, turn).await else {
            slot.rollback(turn);
            return Step::Unread(turn);
        };
        self.submit_failed(slot, session, turn, queueing, message)
            .await
    }
}
