//! The Route runtime every route shares: it owns Wire's runtime, the only
//! opener of vendor connections, and forwards Host's passive facts of
//! shutdown, recovery and held capacity. It runs no protocol itself.

use tokio::sync::watch;
use via_wire::{WireCleanup, WireCloseReport, WireRuntime};

use crate::{
    Deadline, ExitReport, ReprobeReport, RuntimeConfig, RuntimeResources, WireError, WireRecovery,
    WireShutdown,
};

/// Wire's runtime and Host's facts, shared by every route of the daemon.
pub struct RouteRuntime {
    wire: WireRuntime,
}

impl RouteRuntime {
    /// Forwards the unopened resources to Wire's sole bootstrap split.
    pub fn new(config: RuntimeConfig, resources: RuntimeResources) -> Result<Self, WireError> {
        let wire = WireRuntime::new(config, resources)?;
        Ok(Self { wire })
    }

    /// Wire's runtime, which opens each route's connections.
    pub(crate) fn wire(&self) -> &WireRuntime {
        &self.wire
    }

    /// Drains Host controls and reapers before Core releases the Store owner.
    pub async fn shutdown(
        &self,
        deadline: Deadline,
        turns: &[(crate::SessionId, crate::TurnNumber)],
    ) -> WireShutdown {
        self.wire.shutdown(deadline, turns).await
    }

    /// Hands Host capacity for a group it did not launch (design §11).
    pub fn hold_capacity(
        &self,
        anchor_id: String,
        owner: crate::ProcessOwner,
        token: via_wire::CapacityToken,
    ) {
        self.wire.hold_capacity(anchor_id, owner, token);
    }

    /// Returns one page of passive Host recovery facts without exposing a
    /// signal handle: up to `limit` anchors after the `after` id.
    pub async fn recover_page(
        &self,
        after: Option<String>,
        limit: u32,
        deadline: Deadline,
    ) -> Result<Vec<WireRecovery>, WireError> {
        self.wire.recover_page(after, limit, deadline).await
    }

    /// [`Self::recover_page`] of the anchors in `cohort` only.
    pub async fn recover_cohort_page(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: via_wire::AnchorCohort,
        deadline: Deadline,
    ) -> Result<Vec<WireRecovery>, WireError> {
        self.wire
            .recover_cohort_page(after, limit, cohort, deadline)
            .await
    }

    /// One non-signalling re-probe pass over held groups, optionally only
    /// one session's (design §8).
    pub async fn reprobe_held(
        &self,
        deadline: Deadline,
        owner: Option<crate::SessionId>,
    ) -> Result<ReprobeReport, WireError> {
        self.wire.reprobe_held(deadline, owner).await
    }

    /// Host's session-scoped pre-launch absence check (runtime §5.2),
    /// through Wire unchanged: true only when every `Turn` anchor record of
    /// `session` has a committed absence proof. It signals nothing.
    pub async fn session_predecessors_resolved(
        &self,
        session: &crate::SessionId,
        deadline: Deadline,
    ) -> Result<bool, WireError> {
        self.wire
            .session_predecessors_resolved(session, deadline)
            .await
    }

    /// Held groups no live control owns (design §6.6).
    pub fn held_unproven(&self) -> usize {
        self.wire.held_unproven()
    }

    /// Advances on every added holding (design §8).
    pub fn holdings_changed(&self) -> watch::Receiver<u64> {
        self.wire.holdings_changed()
    }

    /// Positive evidence that a vendor of one of `anchors` is live (Task 4
    /// design §11.3 `process.alive`).
    pub fn live_armed(&self, anchors: &[String]) -> bool {
        self.wire.live_armed(anchors)
    }

    /// Groups whose cleanup a live control or acquisition still owns
    /// (design §6.4).
    pub fn pending_cleanup(&self) -> usize {
        self.wire.pending_cleanup()
    }

    /// Subscribes Host's early stop to the daemon force signal (design §6.8),
    /// which carries the instant the force was raised.
    pub fn watch_force(&self, forced: watch::Receiver<Option<tokio::time::Instant>>) {
        self.wire.watch_force(forced);
    }

    /// Host's sticky journal-uncertain watch (x.3.2 X0 item 2.6).
    pub fn journal_uncertain(&self) -> watch::Receiver<bool> {
        self.wire.journal_uncertain()
    }

    /// Creates a turn's evidence folder on a shared route (x.3.2 X0 item
    /// 1.4).
    pub async fn turn_folder(
        &self,
        session: &crate::SessionId,
        turn: crate::TurnNumber,
    ) -> Result<via_wire::TurnFolder, WireError> {
        self.wire.turn_folder(session, turn).await
    }
}

/// A turn's process facts once its route retired it: on a persistent
/// connection the helper's housekeeping close, never a fact of the logical
/// turn (decision H1); otherwise the turn's own close.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Retirement {
    /// A process may have launched.
    pub launched: bool,
    /// Host-confirmed exit, when observed.
    pub exit: Option<ExitReport>,
    /// Cleanup certainty, when the route established one.
    pub cleanup: Option<WireCleanup>,
    /// Host stopped the group while its process was live.
    pub forced: bool,
    /// A Host journal write had an uncertain outcome.
    pub journal_uncertain: bool,
}

impl Retirement {
    /// The facts of a launched helper's close `report`, with the `exit`
    /// its turn already saw (K1: a persistent connection's retirement
    /// reports them as soon as Host's close ended).
    pub(crate) fn closed(exit: Option<ExitReport>, report: &WireCloseReport) -> Self {
        let reported = report
            .vendor_exit
            .filter(|exit| exit.code.is_some() || exit.signal.is_some());
        Self {
            launched: true,
            exit: exit.or(reported),
            cleanup: Some(report.cleanup),
            forced: report.forced,
            journal_uncertain: report.journal_uncertain,
        }
    }
}
