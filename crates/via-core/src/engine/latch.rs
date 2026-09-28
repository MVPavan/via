//! The Store-failed latch (runtime §7), the force signal and the force-path
//! read cutoff.

use std::{
    sync::atomic::Ordering,
    time::{Duration, SystemTime},
};

use tokio::sync::watch;

use super::stop::StopMode;
use super::{Admission, Engine, lock};
use crate::api::rfc3339;

/// Part of final shutdown's deadline that force-path read retries leave for
/// Host cleanup (its 3 s native stop) and forced terminals.
const READ_RETRY_RESERVE: Duration = Duration::from_secs(4);

impl Engine {
    /// Latches Store failure after Core's first failed or uncertain state
    /// write (runtime §7), in two phases (design §3.2). Phase one runs now,
    /// before anything is awaited: `failure_pending` and the force signal are
    /// published under the `stop` mutex, so no grant, no pre-ARM gate and no
    /// new receipt passes from here on. Phase two, the returned future,
    /// finalizes the latch under `admission`, ordered after any receipt
    /// already inside it. The caller holds no slot, session or head lock.
    pub(super) fn latch(&self) -> impl Future<Output = ()> + '_ {
        self.fail_pending();
        async move {
            let admission = self.admission.lock().await;
            self.latch_held(&admission);
        }
    }

    /// [`Engine::latch`] for a caller already holding `admission`: both phases
    /// at once.
    pub(super) fn latch_held(&self, _admission: &Admission<'_>) {
        self.fail_pending();
        self.store_failed.store(true, Ordering::Release);
    }

    /// Phase one of the latch: marks the failure pending and sends the force
    /// signal under the `stop` mutex, which the grant takes, so running turns
    /// take the forced path and daemon main starts final shutdown, which then
    /// reports an unclean exit.
    fn fail_pending(&self) {
        let mut stop = lock(&self.stop);
        if self.failure_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        *stop = Some(StopMode::Force);
        self.force_requested_at
            .get_or_init(|| rfc3339(SystemTime::now()));
        self.force.send_replace(true);
    }

    /// Whether phase two finalized the latch under `admission`.
    #[cfg(test)]
    pub(super) fn latch_finalized(&self) -> bool {
        self.store_failed.load(Ordering::Acquire)
    }

    /// Final shutdown began with this absolute deadline: force-path reads
    /// stop retrying in time for Host cleanup and forced terminals.
    pub fn begin_final_shutdown(&self, deadline: tokio::time::Instant) {
        let by = deadline
            .checked_sub(READ_RETRY_RESERVE)
            .unwrap_or_else(tokio::time::Instant::now);
        self.read_retries_until
            .send_if_modified(|until| until.is_none() && until.replace(by).is_none());
    }

    /// Until when a force-path read may run or retry, once final shutdown began.
    pub(super) fn read_retries_until(&self) -> Option<tokio::time::Instant> {
        *self.read_retries_until.borrow()
    }

    /// Resolves at the force-path read cutoff; never before final shutdown began.
    pub(super) async fn read_cutoff(&self) {
        let mut until = self.read_retries_until.subscribe();
        let by = match until.wait_for(Option::is_some).await {
            Ok(by) => *by,
            Err(_) => None,
        };
        match by {
            Some(by) => tokio::time::sleep_until(by).await,
            None => std::future::pending().await,
        }
    }

    /// Whether a Store failure was observed: pending or finalized.
    pub fn store_failed(&self) -> bool {
        self.failure_pending.load(Ordering::Acquire) || self.store_failed.load(Ordering::Acquire)
    }

    /// Wakes when a force stop is accepted or Store failure latches; daemon
    /// main then starts final shutdown in the mode `stop_mode` reports.
    pub fn force_signal(&self) -> watch::Receiver<bool> {
        self.force.subscribe()
    }
}
