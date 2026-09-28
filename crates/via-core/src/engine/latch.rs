//! The Store-failed latch (runtime §7), the force signal and the force-path
//! read cutoff.

use std::{
    sync::atomic::Ordering,
    time::{Duration, SystemTime},
};

use tokio::sync::watch;

use via_store::StoreError;

use super::journal::may_have_committed;
use super::stop::StopMode;
use super::{Admission, Engine, lock};
use crate::api::rfc3339;
use crate::{SessionId, TurnNumber};

/// Final shutdown's budgets (design §6.8, the one table [r4.2, r5.10]),
/// each measured back from the final deadline. The reserve is per pipeline,
/// not per turn: later commits cut at the deadline count as uncommitted.
///
/// | Budget | Ends at |
/// |---|---|
/// | force-path read cutoff (§6.7) | `deadline − (FINALIZE_RESERVE + 3 s)` |
/// | Host reconciliation (pipeline step 4) | `deadline − FINALIZE_RESERVE` |
/// | each step 5 write | `min(now + FINALIZE_WRITE, deadline)` |
/// | client joins, then the Store join | `deadline − 2 s`, then `deadline` |
///
/// `FINALIZE_RESERVE` covers 2 s for §7.4's re-read, 2 s for the batch or a
/// forced terminal and 1 s for the closure pass.
pub(super) const FINALIZE_RESERVE: Duration = Duration::from_secs(5);

/// Bound of each finalization write in pipeline step 5 (design §6.8).
pub(super) const FINALIZE_WRITE: Duration = Duration::from_secs(2);

/// Host's native stop and absence verification ahead of finalization: the
/// force-path read cutoff leaves it this much before `FINALIZE_RESERVE`.
const HOST_STOP: Duration = Duration::from_secs(3);

/// Part of final shutdown's deadline that force-path reads leave, so the
/// dispatchers join before Host reconciliation needs its time (§6.7).
const READ_RETRY_RESERVE: Duration = FINALIZE_RESERVE.saturating_add(HOST_STOP);

/// Where a Core Store write failed: one site per row of design §7.2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FailureSite {
    /// A `spawn` or `resume` receipt (row 1).
    Receipt,
    /// Submission intent, `turn.submitted` (row 2).
    Submission,
    /// Acceptance, a turn event or `cancel.requested` (row 5).
    Event,
    /// A running turn's terminal (row 7) or its resolution write.
    Terminal,
    /// A `queued → cancelled` commit (rows 8 and 9).
    QueuedCancel,
    /// The force closure pass's standalone `session.closed` (row 14).
    SessionClosed,
    /// A close's `Closing` commit (row 10).
    Closing,
    /// A close's `Closed` commit (row 11).
    Closed,
}

/// A failed write's durable outcome (design §7.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WriteOutcome {
    /// Rolled back or never enqueued: nothing was written.
    NotCommitted,
    /// The write may have committed.
    Uncertain,
}

impl WriteOutcome {
    /// Classifies a Store error by whether its write may have committed.
    pub(super) fn of(error: &StoreError) -> Self {
        if may_have_committed(error) {
            Self::Uncertain
        } else {
            Self::NotCommitted
        }
    }
}

/// Who a failed write belongs to (design §7.2 scope).
#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "the addresses are for S5's failure record (design §7.5); the hook stub latches"
)]
pub(super) enum FailureScope<'a> {
    /// A request that has not been receipted.
    Request,
    /// A receipted turn.
    Turn(&'a SessionId, TurnNumber),
    /// A session-level write.
    Session(&'a SessionId),
}

/// Phase two of a latch the failure hook began; the caller finishes it
/// under `admission`, as its lock position allows.
#[must_use = "phase two of the latch runs under admission"]
pub(super) struct Latching<'a>(&'a Engine);

impl Latching<'_> {
    /// Takes `admission` and finalizes the latch; the caller holds no slot,
    /// session or head lock.
    pub(super) async fn finish(self) {
        let admission = self.0.admission.lock().await;
        self.0.latch_held(&admission);
    }

    /// Finalizes the latch under the caller's `admission`.
    pub(super) fn finish_held(self, admission: &Admission<'_>) {
        self.0.latch_held(admission);
    }

    /// Finalizes the latch, under `admission` if the caller holds it.
    pub(super) async fn finish_with(self, admission: Option<&Admission<'_>>) {
        match admission {
            Some(admission) => self.finish_held(admission),
            None => self.finish().await,
        }
    }
}

impl Engine {
    /// Core's one Store write-failure entry point (design §7 [r3.18]):
    /// every failure site reports its site, outcome and scope. S5 applies
    /// design §7.2's table here; until then every failure latches, as
    /// before. Phase one runs now; the caller finishes phase two.
    pub(super) fn store_failure(
        &self,
        site: FailureSite,
        outcome: WriteOutcome,
        scope: FailureScope<'_>,
    ) -> Latching<'_> {
        // Safe to ignore until S5: the latch is the same for every failure.
        let _ = (site, outcome, scope);
        self.fail_pending();
        Latching(self)
    }

    /// Latches Store failure after Core's first failed or uncertain state
    /// write (runtime §7), in two phases (design §3.2). Phase one runs now,
    /// before anything is awaited: `failure_pending` and the force signal are
    /// published under the `stop` mutex, so no grant, no pre-ARM gate and no
    /// new receipt passes from here on. Phase two, the returned future,
    /// finalizes the latch under `admission`, ordered after any receipt
    /// already inside it. The caller holds no slot, session or head lock.
    #[cfg(test)]
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
