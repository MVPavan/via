//! The re-probe loop over held harness-process slots (design §8): a Core task
//! daemon main spawns at serve start and joins in final shutdown's first
//! pipeline step. Only a proof releases a holding (design §11).

use std::time::Duration;

use via_adapters::{AdapterError, ReprobeReport};
use via_store::{ANCHOR_PAGE_LIMIT, AnchorCohort, AnchorOwner};

use super::Engine;
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use crate::{Cleanup, Deadline};

/// The first pass after a holding appears, and after one is added.
const FIRST_PASS: Duration = Duration::from_secs(1);

/// Passes double up to this interval while holdings remain.
const LAST_PASS: Duration = Duration::from_secs(10);

/// While nothing is held, the in-memory counts are read this often; no
/// Store read is made.
const IDLE_CHECK: Duration = Duration::from_secs(1);

/// Bound of one pass: one probe per held group and one page, each under
/// Host's native 3 s.
const PASS_BOUND: Duration = Duration::from_secs(3);

impl Engine {
    /// Bounds resumed paging to the anchors committed before serving
    /// (design §8): daemon main calls it after [`Engine::recover`] and before
    /// admission, when no anchor of this daemon exists yet. Only when startup
    /// reconciliation left anchors unread is the cohort read: one read-only
    /// Store query, whose failure fails startup like recovery's own reads.
    ///
    /// # Errors
    ///
    /// Returns the Store read failure.
    pub async fn bound_resumed_paging(&self) -> Result<(), String> {
        if self.recovered.unread().is_none() {
            return Ok(());
        }
        let cohort = self
            .store
            .anchor_cohort()
            .await
            .map_err(|error| format!("store_error: {error}"))?;
        self.recovered.save_cohort(cohort);
        Ok(())
    }

    /// Runs re-probe passes while holdings exist (design §8): at 1 s,
    /// doubling to 10 s, and back to 1 s whenever Host signals an added
    /// holding, during a wait or a pass. It continues through a drain, so
    /// capacity can return while drained turns wait, and returns at
    /// final-shutdown entry, on force and on the latch; final shutdown's
    /// reconciliation then takes over. A pass in progress finishes under its
    /// own bound, so no proof commit or reconciliation step is cut in the
    /// middle. It holds no Core lock across an `.await`; `RecoveredSlots`
    /// and the Host ledger are short `std` mutexes taken alone. Wakes: the
    /// force watch, the final-shutdown fence's watch and Host's holdings
    /// generation.
    pub async fn reprobe(&self) {
        let mut force = self.signal.force.subscribe();
        let mut entered = self.final_shutdown.subscribe();
        let mut added = self.adapter.holdings_changed();
        let mut interval = FIRST_PASS;
        let mut next = tokio::time::Instant::now() + FIRST_PASS;
        loop {
            tokio::select! {
                biased;
                _ = force.wait_for(Option::is_some) => return,
                _ = entered.wait_for(|entered| *entered) => return,
                // A closed generation disables this arm; the timer goes on.
                Ok(()) = added.changed() => {
                    interval = FIRST_PASS;
                    next = next.min(tokio::time::Instant::now() + FIRST_PASS);
                    continue;
                }
                () = tokio::time::sleep_until(next) => {}
            }
            if self.holdings() == 0 {
                // Nothing to read: only the in-memory counts, each second.
                interval = FIRST_PASS;
                next = tokio::time::Instant::now() + IDLE_CHECK;
                continue;
            }
            self.reprobe_pass().await;
            // A holding added during the pass has marked `added` changed:
            // the next select resets the interval.
            interval = interval.saturating_mul(2).min(LAST_PASS);
            next = tokio::time::Instant::now() + interval;
        }
    }

    /// One pass (design §8): the non-signalling absence probe over every
    /// held group with no live control, then one page of the interrupted
    /// startup reconciliation, if groups remain unread.
    async fn reprobe_pass(&self) {
        #[cfg(test)]
        self.faults
            .reprobe_passes
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let deadline = Deadline::at(tokio::time::Instant::now() + PASS_BOUND);
        // A failed pass keeps every token, and the next pass retries.
        let pass = self.adapter.reprobe_held(deadline, None).await;
        // An uncertain pass latches (scope `daemon`); a not-committed proof
        // is recorded against its owner session.
        self.proof_failures(&pass, FailureScope::Request).await;
        // A pending latch ends the pass: final shutdown's reconciliation
        // takes over, and no page is read for it.
        if self.store_failed() {
            return;
        }
        if let Some(unread) = self.recovered.unread()
            && let Some(cohort) = unread.cohort
        {
            self.resume_paging(unread.cursor, cohort, deadline).await;
        }
    }

    /// Design §7.2 row 12 [O1.D10]: reports a pass's absence proofs whose
    /// commit failed to the failure hook, and returns the worst outcome
    /// reported. One that did not commit is scoped: its token stays held and
    /// the next pass retries it. One whose commit may have committed
    /// latches. A pass that failed otherwise keeps every token and reports
    /// nothing. A not-committed proof is recorded against its owner session;
    /// `scope` covers an uncertain pass, which has no owner. The caller
    /// holds no lock.
    pub(super) async fn proof_failures(
        &self,
        pass: &Result<ReprobeReport, AdapterError>,
        scope: FailureScope<'_>,
    ) -> Option<WriteOutcome> {
        match pass {
            Ok(report) => {
                for owner in &report.not_committed {
                    // A shared server's proof is no session's (x.3.2 X0
                    // item 6.1): scope `daemon`, no address.
                    let owner = match owner {
                        via_adapters::ProcessOwner::Turn { session_id, .. } => {
                            FailureScope::Session(session_id)
                        }
                        via_adapters::ProcessOwner::Server { .. } => FailureScope::Daemon,
                    };
                    self.store_failure(FailureSite::Absence, WriteOutcome::NotCommitted, owner)
                        .finish()
                        .await;
                }
                (!report.not_committed.is_empty()).then_some(WriteOutcome::NotCommitted)
            }
            Err(error) => self.proof_error(error, scope).await,
        }
    }

    /// Design §7.2 row 12 [O1.D10]: reports the error of a re-probe pass or
    /// of a resumed-paging page to the failure hook. An absence proof whose
    /// commit may have committed but did not answer latches, whichever call
    /// carried it, and is returned. Any other error keeps every token and
    /// reports nothing: the next pass retries. The caller holds no lock.
    async fn proof_error(
        &self,
        error: &AdapterError,
        scope: FailureScope<'_>,
    ) -> Option<WriteOutcome> {
        if !error.journal_uncertain() {
            return None;
        }
        self.store_failure(FailureSite::Absence, WriteOutcome::Uncertain, scope)
            .finish()
            .await;
        Some(WriteOutcome::Uncertain)
    }

    /// Reads one page of the anchors startup reconciliation left unread
    /// (design §8 step 2): each anchor becomes an identified holding or a
    /// proof, and the unread count is recomputed past the new cursor. A
    /// failed read keeps the cursor and the count for the next pass.
    ///
    /// Anchor ids are random, so this daemon's own anchors sort among the
    /// unread ones, and reconciliation would challenge and stop a live one.
    /// Every read is therefore bounded to the startup `cohort`, which holds
    /// no anchor of this daemon: paging progresses while its groups run.
    async fn resume_paging(&self, after: Option<String>, cohort: AnchorCohort, deadline: Deadline) {
        // A failed read or reconciliation leaves the page unread: the next
        // pass retries it from the same cursor. A page whose absence-proof
        // commit was uncertain latches first: it may have committed.
        let Ok(owners) = self
            .store
            .cohort_owners_page(after.clone(), ANCHOR_PAGE_LIMIT, cohort)
            .await
        else {
            return;
        };
        let reports = match self
            .adapter
            .recover_cohort_page(after, ANCHOR_PAGE_LIMIT, cohort, deadline)
            .await
        {
            Ok(reports) => reports,
            Err(error) => {
                self.proof_error(&error, FailureScope::Request).await;
                return;
            }
        };
        self.hold_unread(&owners, &reports);
        let end = owners.len() < ANCHOR_PAGE_LIMIT as usize;
        let cursor = owners.last().map(|owner| owner.anchor_id.clone());
        if end {
            self.recovered.resume(cursor, 0);
            return;
        }
        let pool = u32::try_from(self.slot_limit).unwrap_or(u32::MAX);
        // A failed count keeps the old cursor and count, and the next pass
        // reads the page again: holding an anchor again replaces Host's entry
        // and drops its old token, so the counts stay balanced.
        if let Ok(unread) = self
            .store
            .unproven_cohort_anchors_up_to(cursor.clone(), pool, cohort)
            .await
        {
            self.recovered.resume(cursor, unread);
        }
    }

    /// Holds a slot for each anchor of the page Host did not prove absent,
    /// as startup reconciliation does (design §11): Host keeps its token
    /// until a later proof.
    fn hold_unread(&self, owners: &[AnchorOwner], reports: &[via_adapters::AnchorRecovery]) {
        for owner in owners {
            let proved = reports.iter().any(|report| {
                report.anchor_id == owner.anchor_id && report.cleanup == Cleanup::Quiescent
            });
            if !proved {
                let token = self.recovered.hold(&self.slots);
                self.adapter.hold_capacity(
                    owner.anchor_id.clone(),
                    owner.owner.clone(),
                    Box::new(token),
                );
            }
        }
    }
}
