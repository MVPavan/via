//! Connection slots held for groups an earlier daemon left unproven, and
//! where the interrupted startup reconciliation stopped (design §11).

use std::sync::{Arc, Mutex, PoisonError};

/// Connection slots held for unproven recovered groups (design §11): at most
/// one permit per group, never more than the pool. A group's token, dropped
/// when Host proves it absent, releases a permit only once fewer groups than
/// permits remain.
#[derive(Clone, Default)]
pub(super) struct RecoveredSlots(Arc<Mutex<Recovered>>);

#[derive(Default)]
struct Recovered {
    permits: Vec<tokio::sync::OwnedSemaphorePermit>,
    groups: usize,
    /// Where the startup reconciliation's paging stopped at its deadline.
    cursor: Option<String>,
}

impl RecoveredSlots {
    pub(super) fn hold(&self, slots: &Arc<tokio::sync::Semaphore>) -> RecoveredGroup {
        let mut recovered = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        recovered.groups += 1;
        if let Ok(permit) = Arc::clone(slots).try_acquire_owned() {
            recovered.permits.push(permit);
        }
        RecoveredGroup(self.clone())
    }

    /// Counts `groups` a recovery deadline left unread. They have no Host
    /// ledger entry, so nothing releases them before the next full
    /// reconciliation (final shutdown or restart).
    pub(super) fn hold_unidentified(&self, slots: &Arc<tokio::sync::Semaphore>, groups: u64) {
        let mut recovered = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        recovered.groups += usize::try_from(groups).unwrap_or(usize::MAX);
        while recovered.permits.len() < recovered.groups {
            let Ok(permit) = Arc::clone(slots).try_acquire_owned() else {
                return;
            };
            recovered.permits.push(permit);
        }
    }

    /// Saves where the startup reconciliation stopped paging at its deadline.
    pub(super) fn save_cursor(&self, cursor: Option<String>) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).cursor = cursor;
    }
}

/// One unproven recovered group's share of [`RecoveredSlots`].
pub(super) struct RecoveredGroup(RecoveredSlots);

impl Drop for RecoveredGroup {
    fn drop(&mut self) {
        let mut recovered = (self.0).0.lock().unwrap_or_else(PoisonError::into_inner);
        recovered.groups -= 1;
        if recovered.permits.len() > recovered.groups {
            recovered.permits.pop();
        }
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
}
