//! The re-probe loop over held connection slots (design §8): a Core task
//! daemon main spawns at serve start and joins in final shutdown's first
//! pipeline step. Only a proof releases a holding (design §11).

use std::{sync::Arc, time::Duration};

use tokio::sync::OwnedSemaphorePermit;
use via_store::{ANCHOR_PAGE_LIMIT, AnchorOwner};

use super::Engine;
use super::queue::CONNECTION_SLOTS;
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

/// The connection permits that were free when a page read began, held
/// until it ends (`None` when none were free).
struct Fence(#[expect(dead_code, reason = "held only for its drop")] Option<OwnedSemaphorePermit>);

impl Engine {
    /// Runs re-probe passes while holdings exist (design §8): at 1 s,
    /// doubling to 10 s, and back to 1 s when a holding is added. It
    /// continues through a drain, so capacity can return while drained
    /// turns wait, and returns at final-shutdown entry, on force and on the
    /// latch; final shutdown's reconciliation then takes over. A pass in
    /// progress finishes under its own bound, so no proof commit or
    /// reconciliation step is cut in the middle. It holds no Core lock
    /// across an `.await`; `RecoveredSlots` and the Host ledger are short
    /// `std` mutexes taken alone. Wakes: the force watch and the
    /// final-shutdown fence's watch.
    pub async fn reprobe(&self) {
        let mut force = self.force.subscribe();
        let mut entered = self.final_shutdown.subscribe();
        let mut interval = FIRST_PASS;
        let mut known = 0;
        loop {
            let holdings = self.holdings();
            if holdings > known {
                interval = FIRST_PASS;
            }
            known = holdings;
            let wait = if holdings == 0 { IDLE_CHECK } else { interval };
            tokio::select! {
                biased;
                _ = force.wait_for(|forced| *forced) => return,
                _ = entered.wait_for(|entered| *entered) => return,
                () = tokio::time::sleep(wait) => {}
            }
            if holdings == 0 || self.holdings() == 0 {
                continue;
            }
            self.reprobe_pass().await;
            interval = interval.saturating_mul(2).min(LAST_PASS);
            known = self.holdings();
        }
    }

    /// One pass (design §8): the non-signalling absence probe over every
    /// held group with no live control, then one page of the interrupted
    /// startup reconciliation, if groups remain unread.
    async fn reprobe_pass(&self) {
        let deadline = Deadline::at(tokio::time::Instant::now() + PASS_BOUND);
        // Safe to ignore: a failed pass keeps every token, and the next pass
        // retries; a not-committed proof keeps its token too (§7.2 row 12).
        // S5 owns the latch on an uncertain proof commit (row 12, O1.D10).
        let _ = self.adapter.reprobe_held(deadline, None).await;
        if let Some(unread) = self.recovered.unread() {
            self.resume_paging(unread.cursor, deadline).await;
        }
    }

    /// Reads one page of the anchors startup reconciliation left unread
    /// (design §8 step 2): each anchor becomes an identified holding or a
    /// proof, and the unread count is recomputed past the new cursor. A
    /// failed read keeps the cursor and the count for the next pass.
    ///
    /// Anchor ids are random, so this daemon's own anchors sort among the
    /// unread ones, and reconciliation would challenge and stop a live one.
    /// The page is therefore read only behind [`Engine::own_groups_fence`].
    async fn resume_paging(&self, after: Option<String>, deadline: Deadline) {
        let Some(_fence) = self.own_groups_fence() else {
            return;
        };
        // A failed read or reconciliation leaves the page unread: the next
        // pass retries it from the same cursor.
        let Ok(owners) = self
            .store
            .anchor_owners_page(after.clone(), ANCHOR_PAGE_LIMIT)
            .await
        else {
            return;
        };
        let Ok(reports) = self
            .adapter
            .recover_page(after, ANCHOR_PAGE_LIMIT, deadline)
            .await
        else {
            return;
        };
        self.hold_unread(&owners, &reports);
        let end = owners.len() < ANCHOR_PAGE_LIMIT as usize;
        let cursor = owners.last().map(|owner| owner.anchor_id.clone());
        if end {
            self.recovered.resume(cursor, 0);
            return;
        }
        let pool = u32::try_from(CONNECTION_SLOTS).unwrap_or(u32::MAX);
        // A failed count keeps the old cursor and count, and the next pass
        // reads the page again: holding an anchor again replaces Host's entry
        // and drops its old token, so the counts stay balanced.
        if let Ok(unread) = self
            .store
            .unproven_anchors_up_to(cursor.clone(), pool)
            .await
        {
            self.recovered.resume(cursor, unread);
        }
    }

    /// Takes every free connection permit and keeps them while no permit is
    /// out for a group of this daemon: every outstanding permit is one
    /// `RecoveredSlots` holds, no control or acquisition is live, and every
    /// Host holding is a recovered group. No launch can then start before
    /// the fence drops, since each one first reserves a permit, so the page
    /// holds only anchors of earlier daemons, or proved ones. `None` while
    /// this daemon owns a group; the next pass tries again.
    fn own_groups_fence(&self) -> Option<Fence> {
        let available = self.slots.available_permits();
        let fence = match u32::try_from(available) {
            Ok(0) | Err(_) => None,
            Ok(count) => Arc::clone(&self.slots).try_acquire_many_owned(count).ok(),
        };
        let taken = fence.as_ref().map_or(0, OwnedSemaphorePermit::num_permits);
        let outstanding = self
            .slot_limit
            .saturating_sub(self.slots.available_permits())
            .saturating_sub(taken);
        let held = self.recovered.held();
        let own = outstanding != held.permits
            || self.adapter.pending_cleanup() > 0
            || self.adapter.held_unproven() > held.identified;
        (!own).then_some(Fence(fence))
    }

    /// Holds a slot for each anchor of the page Host did not prove absent,
    /// as startup reconciliation does (design §11): Host keeps its token
    /// until a later proof.
    fn hold_unread(&self, owners: &[AnchorOwner], reports: &[via_adapters::FakeRecovery]) {
        for owner in owners {
            let proved = reports.iter().any(|report| {
                report.anchor_id == owner.anchor_id && report.cleanup == Cleanup::Quiescent
            });
            if !proved {
                let token = self.recovered.hold(&self.slots);
                self.adapter.hold_capacity(
                    owner.anchor_id.clone(),
                    owner.session_id.clone(),
                    Box::new(token),
                );
            }
        }
    }
}
