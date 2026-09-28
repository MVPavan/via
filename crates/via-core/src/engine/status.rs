//! `daemon/status` counts (C1 §3.14; design §6.6): the durable closing set
//! and the connection slots.

use super::{Engine, lock};

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
    /// Receipted turns not yet settled (`sessions.active`).
    pub active: usize,
    /// The durable closing set's size (`sessions.closing` [r3.5]).
    pub closing: usize,
    /// Connection slots.
    pub connections: Connections,
}

impl Engine {
    /// The in-memory counts of `daemon/status` (design §6.6). Each lock is
    /// a short `std` mutex taken alone; no wake.
    pub fn counts(&self) -> DaemonCounts {
        DaemonCounts {
            active: self.active(),
            closing: self.closing_sessions(),
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
}
