//! `AdapterSet`'s session and Host-fact operations (C2 §2, adapter design
//! §3.2): logical session open, recovery after a daemon restart, and S1's
//! Host-fact operations under their C2 names.

use std::sync::Arc;

use tokio::sync::watch;

use crate::driver::{Recovery, SessionCx, SessionDriver, SessionSpec};
use crate::harness::Harness;
use crate::plan::{Adapter, AdapterSet, SessionRef};
use crate::runtime::{normalize_recovery, shutdown_report};
use crate::{
    AdapterError, AdapterShutdown, AnchorRecovery, CapacityToken, Cleanup, Deadline, ProcessOwner,
    ReprobeReport, SessionId, TurnNumber,
};

impl AdapterSet {
    /// Logical (C2 §2, AD3): no vendor I/O. Attaches the session's
    /// observation channel. A session whose harness no adapter serves gets
    /// a driver whose turns fail `Unavailable`; Core refuses such a session
    /// at `check_turn` first.
    pub fn open_session(
        &self,
        session: &SessionRef,
        spec: SessionSpec,
        cx: SessionCx,
    ) -> SessionDriver {
        let kind = Harness::parse(&session.harness)
            .filter(|harness| harness.route() == session.route)
            .and_then(|harness| self.adapter(harness))
            .map(Adapter::driver_kind);
        let driver = SessionDriver::new(Arc::clone(&self.runtime), kind, spec, cx);
        #[cfg(feature = "test-failpoints")]
        let driver = SessionDriver {
            stand_in: self.stand_in.get().cloned(),
            ..driver
        };
        driver
    }

    /// Test builds only: the stand-in admission (x.3.2 X0 item 0) every
    /// driver opened from now on takes, installed on first call.
    #[cfg(feature = "test-failpoints")]
    pub fn stand_in(&self) -> Arc<crate::StandIn> {
        Arc::clone(self.stand_in.get_or_init(Arc::default))
    }

    /// Sticky (C2 §2): some Host journal write's outcome was uncertain,
    /// including writes no driver owns, as a shared server's retirement
    /// after its last lease (x.3.2 X0 item 2.6). Core latches Store
    /// failure on it.
    pub fn journal_uncertain(&self) -> watch::Receiver<bool> {
        self.runtime.journal_uncertain()
    }

    /// After a daemon restart (C2 §2 Recover): never submits input. No
    /// adapter yet declares `recover`, so none resumes: `Dead` only when
    /// Host proved every anchor of the session absent, else `Unknown`.
    /// Cleanup follows AD9's recovery row (`GroupAbsent`).
    pub fn recover(
        &self,
        session: &SessionRef,
        facts: &[AnchorRecovery],
        cx: SessionCx,
    ) -> impl Future<Output = Recovery> + Send + use<> {
        let adapter = Harness::parse(&session.harness).and_then(|harness| self.adapter(harness));
        let recovery = match adapter {
            // Neither the fake nor the vendor stubs resume: the context is
            // not used, and nothing is started.
            Some(Adapter::Fake(_) | Adapter::Claude(_) | Adapter::Codex(_)) | None => {
                drop(cx);
                recovery_by_facts(facts)
            }
        };
        std::future::ready(recovery)
    }

    /// Drains lower process owners before Store shutdown and returns
    /// passive facts.
    pub async fn shutdown(
        &self,
        deadline: Deadline,
        turns: &[(SessionId, TurnNumber)],
    ) -> AdapterShutdown {
        shutdown_report(self.runtime.shutdown(deadline, turns).await)
    }

    /// Hands Host capacity for a group it did not launch (design §11).
    pub fn hold_capacity(&self, anchor_id: String, owner: ProcessOwner, token: CapacityToken) {
        self.runtime.hold_capacity(anchor_id, owner, token);
    }

    /// One non-signalling re-probe pass over held groups, optionally only
    /// one session's (design §8).
    pub async fn reprobe_held(
        &self,
        deadline: Deadline,
        owner: Option<SessionId>,
    ) -> Result<ReprobeReport, AdapterError> {
        self.runtime
            .reprobe_held(deadline, owner)
            .await
            .map_err(AdapterError::Open)
    }

    /// Held groups no live control owns (design §6.6).
    pub fn held_unproven(&self) -> usize {
        self.runtime.held_unproven()
    }

    /// Advances on every added holding (design §8).
    pub fn holdings_changed(&self) -> watch::Receiver<u64> {
        self.runtime.holdings_changed()
    }

    /// Positive evidence that a vendor of one of `anchors` is live.
    pub fn live_armed(&self, anchors: &[String]) -> bool {
        self.runtime.live_armed(anchors)
    }

    /// Groups whose cleanup a live control or acquisition still owns.
    pub fn pending_cleanup(&self) -> usize {
        self.runtime.pending_cleanup()
    }

    /// Subscribes Host's early stop to the daemon force signal (design
    /// §6.8); call once, from within the daemon's runtime.
    pub fn watch_force(&self, forced: watch::Receiver<Option<tokio::time::Instant>>) {
        self.runtime.watch_force(forced);
    }

    /// One page of committed anchors, up to `limit` after `after`, without
    /// signalling authority.
    pub async fn recover_page(
        &self,
        after: Option<String>,
        limit: u32,
        deadline: Deadline,
    ) -> Result<Vec<AnchorRecovery>, AdapterError> {
        self.runtime
            .recover_page(after, limit, deadline)
            .await
            .map(|reports| reports.into_iter().map(normalize_recovery).collect())
            .map_err(AdapterError::Open)
    }

    /// [`Self::recover_page`] of the anchors in `cohort` only.
    pub async fn recover_cohort_page(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: via_routes::AnchorCohort,
        deadline: Deadline,
    ) -> Result<Vec<AnchorRecovery>, AdapterError> {
        self.runtime
            .recover_cohort_page(after, limit, cohort, deadline)
            .await
            .map(|reports| reports.into_iter().map(normalize_recovery).collect())
            .map_err(AdapterError::Open)
    }
}

/// What Host's facts alone establish of a session no adapter resumes:
/// `Dead` only when Host proved every anchor of it absent.
fn recovery_by_facts(facts: &[AnchorRecovery]) -> Recovery {
    if facts.is_empty() {
        Recovery::Unknown {
            reason: "no Host evidence for the session".to_owned(),
        }
    } else if facts.iter().all(|fact| fact.cleanup == Cleanup::Quiescent) {
        Recovery::Dead {
            evidence: format!(
                "Host proved {} anchor group(s) of the session absent",
                facts.len()
            ),
        }
    } else {
        Recovery::Unknown {
            reason: "a process of the session may survive".to_owned(),
        }
    }
}
