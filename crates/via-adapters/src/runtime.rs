//! The observation channel's bounds and the stall bound (C2 A1), and the
//! passive Host facts of shutdown and recovery under their C2 names
//! (adapter design §5.1 #40, #41).

use std::time::Duration;

use crate::{Cleanup, SessionId, TurnNumber};
use via_routes::WireRecovery;

/// C2 A1: the observation channel holds at most 1,024 items ...
pub const OBSERVATION_ITEMS: usize = 1024;

/// ... and at most 4 MiB of them, counted by the item's cost.
pub const OBSERVATION_BYTES: usize = 4 * 1024 * 1024;

/// C2 A1: a delivery blocked this long without an item accepted fails the
/// turn `overflow`.
const EVENT_STALL: Duration = Duration::from_secs(10);

/// The stall bound: 10 s. Test builds only: `VIA_TEST_EVENT_STALL_MS`
/// lowers it (Task 4 design §13.1).
pub(crate) fn event_stall() -> Duration {
    #[cfg(feature = "test-failpoints")]
    if let Some(lowered) = std::env::var("VIA_TEST_EVENT_STALL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(lowered);
    }
    EVENT_STALL
}

/// Passive recovery facts for Core's later crash reconciliation.
pub struct AnchorRecovery {
    /// Owning VIA session.
    pub session_id: SessionId,
    /// Opaque committed anchor identifier.
    pub anchor_id: String,
    /// Opaque launch generation.
    pub generation: String,
    /// Owning turn.
    pub turn: TurnNumber,
    /// Cleanup certainty under Host's validated group.
    pub cleanup: Cleanup,
    /// Host stopped the group while its vendor was live (Host force evidence).
    pub forced: bool,
}

/// Route's passive shutdown report as the Adapter's.
pub(crate) fn shutdown_report(report: via_routes::WireShutdown) -> AdapterShutdown {
    AdapterShutdown {
        recovery: report
            .recovery
            .into_iter()
            .map(|turn| AnchorTurnRecovery {
                session_id: turn.owner_session,
                turn: turn.owner_turn,
                cleanup: cleanup(turn.cleanup),
                forced: turn.forced,
            })
            .collect(),
        anchors: report.anchors,
        uncertain_anchors: report.uncertain_anchors,
        pending_tasks: report.pending_tasks,
        failed_tasks: report.failed_tasks,
        failure: report.failure,
    }
}

/// Route's cleanup certainty as the Adapter's.
pub(crate) fn cleanup(cleanup: via_routes::WireCleanup) -> Cleanup {
    match cleanup {
        via_routes::WireCleanup::Quiescent => Cleanup::Quiescent,
        via_routes::WireCleanup::Uncertain => Cleanup::Uncertain,
    }
}

pub(crate) fn normalize_recovery(report: WireRecovery) -> AnchorRecovery {
    AnchorRecovery {
        session_id: report.owner_session,
        anchor_id: report.anchor_id,
        generation: report.generation,
        turn: report.owner_turn,
        cleanup: cleanup(report.cleanup),
        forced: report.forced,
    }
}

/// Passive per-turn shutdown recovery facts.
pub struct AnchorTurnRecovery {
    /// Owning VIA session.
    pub session_id: SessionId,
    /// Owning turn.
    pub turn: TurnNumber,
    /// Quiescent only when every anchor of the turn was proved absent.
    pub cleanup: Cleanup,
    /// Host stopped a group of the turn while its vendor was live.
    pub forced: bool,
}

/// Passive shutdown status; no Host operation or signal handle escapes Adapter.
pub struct AdapterShutdown {
    /// Per-turn recovery facts for the requested turns.
    pub recovery: Vec<AnchorTurnRecovery>,
    /// Committed anchors reconciled.
    pub anchors: usize,
    /// Reconciled anchors without positive absence proof.
    pub uncertain_anchors: usize,
    /// Host tasks still pending at the shutdown deadline.
    pub pending_tasks: usize,
    /// Host tasks that panicked, were cancelled or failed their child wait.
    pub failed_tasks: usize,
    /// Bounded description of the deadline, Store or recovery failure, if any.
    pub failure: Option<String>,
}
