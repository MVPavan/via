//! Connection slots held for groups an earlier daemon left unproven, and
//! where the interrupted startup reconciliation stopped (design §11).

use std::sync::{Arc, Mutex, PoisonError};

/// Connection slots held for unproven recovered groups (design §11): at most
/// one permit per group, never more than the pool. A group's token, dropped
/// when Host proves it absent, releases a permit only once fewer groups than
/// permits remain. Groups a recovery deadline left unread are counted apart,
/// with the cursor where paging stopped, until the re-probe loop's resumed
/// paging reads them (design §8).
#[derive(Clone, Default)]
pub(super) struct RecoveredSlots(Arc<Mutex<Recovered>>);

#[derive(Default)]
struct Recovered {
    permits: Vec<tokio::sync::OwnedSemaphorePermit>,
    /// Unproven groups Host holds a ledger entry for.
    identified: usize,
    /// Unproven groups past `cursor` that no reconciliation read yet.
    unidentified: usize,
    /// Where the startup reconciliation's paging stopped at its deadline.
    cursor: Option<String>,
}

impl Recovered {
    fn groups(&self) -> usize {
        self.identified.saturating_add(self.unidentified)
    }

    /// Takes free permits up to one per group, then returns any beyond it.
    fn balance(&mut self, slots: Option<&Arc<tokio::sync::Semaphore>>) {
        while let Some(slots) = slots
            && self.permits.len() < self.groups()
        {
            let Ok(permit) = Arc::clone(slots).try_acquire_owned() else {
                break;
            };
            self.permits.push(permit);
        }
        while self.permits.len() > self.groups() {
            self.permits.pop();
        }
    }
}

/// What the re-probe loop resumes paging from (design §8): the cursor where
/// startup paging stopped, while unread groups remain.
pub(super) struct Unread {
    pub(super) cursor: Option<String>,
}

/// Counts of [`RecoveredSlots`] for `daemon/status` (design §6.6).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Held {
    /// Permits held for recovered groups.
    pub(super) permits: usize,
    /// Recovered groups with a Host ledger entry.
    pub(super) identified: usize,
    /// Recovered groups no reconciliation has read yet.
    pub(super) unidentified: usize,
}

impl RecoveredSlots {
    fn lock(&self) -> std::sync::MutexGuard<'_, Recovered> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn hold(&self, slots: &Arc<tokio::sync::Semaphore>) -> RecoveredGroup {
        let mut recovered = self.lock();
        recovered.identified += 1;
        recovered.balance(Some(slots));
        RecoveredGroup(self.clone())
    }

    /// Counts `groups` a recovery deadline left unread. They have no Host
    /// ledger entry: only the re-probe loop's resumed paging (design §8), or
    /// the next full reconciliation, releases them.
    pub(super) fn hold_unidentified(&self, slots: &Arc<tokio::sync::Semaphore>, groups: u64) {
        let mut recovered = self.lock();
        recovered.unidentified = recovered
            .unidentified
            .saturating_add(usize::try_from(groups).unwrap_or(usize::MAX));
        recovered.balance(Some(slots));
    }

    /// Saves where the startup reconciliation stopped paging at its deadline.
    pub(super) fn save_cursor(&self, cursor: Option<String>) {
        self.lock().cursor = cursor;
    }

    /// Where resumed paging starts, while unread groups remain.
    pub(super) fn unread(&self) -> Option<Unread> {
        let recovered = self.lock();
        (recovered.unidentified > 0).then(|| Unread {
            cursor: recovered.cursor.clone(),
        })
    }

    /// Records one page of resumed paging (design §8): paging now stops at
    /// `cursor`, and `unidentified` unproven groups remain past it; permits
    /// beyond the recount are released. The page's unproven anchors were
    /// held as identified groups first, so no permit is released for them.
    pub(super) fn resume(&self, cursor: Option<String>, unidentified: u64) {
        let mut recovered = self.lock();
        recovered.cursor = cursor;
        recovered.unidentified = usize::try_from(unidentified).unwrap_or(usize::MAX);
        recovered.balance(None);
    }

    /// The status counts (design §6.6).
    pub(super) fn held(&self) -> Held {
        let recovered = self.lock();
        Held {
            permits: recovered.permits.len(),
            identified: recovered.identified,
            unidentified: recovered.unidentified,
        }
    }
}

/// One unproven recovered group's share of [`RecoveredSlots`].
pub(super) struct RecoveredGroup(RecoveredSlots);

impl Drop for RecoveredGroup {
    fn drop(&mut self) {
        let mut recovered = self.0.lock();
        recovered.identified = recovered.identified.saturating_sub(1);
        recovered.balance(None);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::RecoveredSlots;

    /// Design §11: more unproven recovered groups than slots hold every
    /// slot, and a slot frees only once fewer groups than slots remain.
    #[test]
    fn recovered_groups_past_the_pool_free_a_slot_only_when_fewer_remain() {
        let slots = Arc::new(tokio::sync::Semaphore::new(2));
        let recovered = RecoveredSlots::default();
        let mut groups: Vec<_> = (0..3).map(|_| recovered.hold(&slots)).collect();
        assert_eq!(slots.available_permits(), 0);
        groups.pop();
        assert_eq!(slots.available_permits(), 0, "two groups still fill both");
        groups.pop();
        assert_eq!(slots.available_permits(), 1);
        groups.pop();
        assert_eq!(slots.available_permits(), 2);
    }

    /// Design §11: unidentified groups a recovery deadline left unread are
    /// never released, and count with the identified ones past the pool.
    #[test]
    fn unidentified_groups_stay_counted_when_identified_ones_are_proved_absent() {
        let slots = Arc::new(tokio::sync::Semaphore::new(4));
        let recovered = RecoveredSlots::default();
        let mut groups: Vec<_> = (0..3).map(|_| recovered.hold(&slots)).collect();
        recovered.hold_unidentified(&slots, 5);
        assert_eq!(slots.available_permits(), 0);
        groups.clear();
        assert_eq!(
            slots.available_permits(),
            0,
            "five unread groups still fill four"
        );
    }

    /// Design §8: resumed paging holds a page's unproven anchors as
    /// identified groups, then recounts the unread ones past the new
    /// cursor; permits beyond the recount are released, and paging ends
    /// once nothing is unread.
    #[test]
    fn resumed_paging_recounts_unread_groups_and_frees_the_rest() {
        let slots = Arc::new(tokio::sync::Semaphore::new(4));
        let recovered = RecoveredSlots::default();
        recovered.hold_unidentified(&slots, 3);
        recovered.save_cursor(Some("a1".to_owned()));
        assert_eq!(slots.available_permits(), 1);
        let unread = recovered.unread().expect("three groups are unread");
        assert_eq!(unread.cursor.as_deref(), Some("a1"));
        // One page: one anchor still unproven, one proved; one remains unread.
        let group = recovered.hold(&slots);
        recovered.resume(Some("a3".to_owned()), 1);
        let held = recovered.held();
        assert_eq!(
            (held.permits, held.identified, held.unidentified),
            (2, 1, 1)
        );
        assert_eq!(slots.available_permits(), 2);
        assert_eq!(
            recovered
                .unread()
                .and_then(|unread| unread.cursor)
                .as_deref(),
            Some("a3")
        );
        // The last page: nothing unread; the held group's proof frees its slot.
        recovered.resume(Some("a4".to_owned()), 0);
        assert!(recovered.unread().is_none());
        assert_eq!(slots.available_permits(), 3);
        drop(group);
        assert_eq!(slots.available_permits(), 4);
        assert_eq!(recovered.held(), super::Held::default());
    }
}
