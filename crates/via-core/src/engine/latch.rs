//! The Store-failed latch (runtime §7), the force signal and the force-path
//! read cutoff.

use std::{
    sync::atomic::Ordering,
    time::{Duration, SystemTime},
};

use serde_json::{Value, json};
use tokio::sync::watch;

use via_store::StoreError;

use super::journal::may_have_committed;
use super::stop::StopMode;
use super::{Admission, Engine, lock};
use crate::api::rfc3339;
use crate::{ApiError, SessionId, TurnNumber};

/// Final shutdown's budgets (design §6.8, the one table [r4.2, r5.10]),
/// each measured back from the final deadline. The reserve is per pipeline,
/// not per turn: later commits cut at the deadline count as uncommitted.
///
/// | Budget | Ends at |
/// |---|---|
/// | force-path read cutoff (§6.7) | `deadline − (FINALIZE_RESERVE + 3 s)` |
/// | dispatcher joins, then abort (step 3) | `deadline − (FINALIZE_RESERVE + ABORTED_JOIN)` |
/// | aborted dispatchers' joins (step 3) | `deadline − FINALIZE_RESERVE` |
/// | Host reconciliation (pipeline step 4) | `deadline − FINALIZE_RESERVE` |
/// | each step 5 write | `min(now + FINALIZE_WRITE, deadline)` |
/// | client joins, then the Store join | `deadline − 2 s`, then `deadline` |
///
/// `FINALIZE_RESERVE` covers 2 s for §7.4's re-read, 2 s for the batch or a
/// forced terminal and 1 s for the closure pass.
pub(super) const FINALIZE_RESERVE: Duration = Duration::from_secs(5);

/// Pipeline step 3's wait for the dispatchers it aborted, before Host
/// reconciliation begins (design §6.8): an abort takes effect when the task
/// is next polled, so a dispatcher is joined, not only cancelled.
pub(super) const ABORTED_JOIN: Duration = Duration::from_secs(1);

/// Bound of each finalization write in pipeline step 5 (design §6.8).
pub(super) const FINALIZE_WRITE: Duration = Duration::from_secs(2);

/// Host's native stop and absence verification ahead of finalization: the
/// force-path read cutoff leaves it this much before `FINALIZE_RESERVE`.
const HOST_STOP: Duration = Duration::from_secs(3);

/// Part of final shutdown's deadline that force-path reads leave, so the
/// dispatchers join before Host reconciliation needs its time (§6.7).
const READ_RETRY_RESERVE: Duration = FINALIZE_RESERVE.saturating_add(HOST_STOP);

/// Where a Core Store write failed: one site per row of design §7.2, plus
/// the resolution write, the dispatcher's reads (§7.3) and the latch batch
/// (§7.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FailureSite {
    /// A `spawn` or `resume` receipt (row 1).
    Receipt,
    /// Submission intent, `turn.submitted` (row 2).
    Submission,
    /// Acceptance, a turn event or `cancel.requested` (row 5).
    Event,
    /// A running turn's natural terminal, which is retried once (row 7).
    Terminal,
    /// A turn's one resolution write after its first failure, or the one
    /// retry of rows 7 and 9 (design §7.2 escalation).
    Resolution,
    /// A caller's `queued → cancelled` (row 8).
    RequestCancel,
    /// A dispatcher's `queued → cancelled`, which is retried once (row 9).
    QueuedCancel,
    /// The force closure pass's standalone `session.closed` (row 14).
    SessionClosed,
    /// A close's `Closing` commit (row 10).
    Closing,
    /// A close's `Closed` commit (row 11).
    Closed,
    /// A forced turn's terminal in final shutdown (row 15).
    ForcedTerminal,
    /// Final shutdown's failure-resolution batch (§7.4); only reached after
    /// the latch.
    Batch,
    /// A raw append or sync Route depends on (row 6).
    Raw,
    /// A Host journal write of a turn's acquisition (rows 3 and 4).
    Journal,
    /// A group-absence proof's commit (row 12).
    Absence,
}

impl FailureSite {
    /// Whether a not-committed write at this site is scoped to its request,
    /// turn or session (design §7.2): every row but the escalation.
    fn scoped(self) -> bool {
        match self {
            Self::Receipt
            | Self::RequestCancel
            | Self::SessionClosed
            | Self::Closing
            | Self::Closed
            | Self::ForcedTerminal
            | Self::Submission
            | Self::Event
            | Self::Terminal
            | Self::QueuedCancel
            | Self::Raw
            | Self::Journal
            | Self::Absence => true,
            // The escalation: a turn's one resolution write failed.
            Self::Resolution | Self::Batch => false,
        }
    }

    /// `store_failure.scope` of a scoped failure (design §7.5): a receipt, a
    /// caller cancel and `Closing` are the request's; `Closed` and a closure
    /// commit the session's; the rest the turn's.
    fn scope(self) -> &'static str {
        match self {
            Self::Receipt | Self::RequestCancel | Self::Closing => "request",
            Self::Closed | Self::SessionClosed | Self::Absence => "session",
            Self::Submission
            | Self::Event
            | Self::Terminal
            | Self::Resolution
            | Self::QueuedCancel
            | Self::ForcedTerminal
            | Self::Batch
            | Self::Raw
            | Self::Journal => "turn",
        }
    }
}

/// A failed write's durable outcome (design §7.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WriteOutcome {
    /// Rolled back or never enqueued: nothing was written.
    NotCommitted,
    /// The write may have committed.
    Uncertain,
    /// SQLite reported corruption (`SQLITE_CORRUPT`, `SQLITE_NOTADB`).
    Corrupt,
}

impl WriteOutcome {
    /// Classifies a Store error (design §7.1's one mapping): corruption,
    /// a write that may have committed, or one that did not.
    pub(super) fn of(error: &StoreError) -> Self {
        if matches!(error, StoreError::Corrupt(_)) {
            Self::Corrupt
        } else if may_have_committed(error) {
            Self::Uncertain
        } else {
            Self::NotCommitted
        }
    }

    /// Whether the write may be durable, so the session head is re-read
    /// before its next event (design §7.1 sequence numbers).
    pub(super) fn head_unknown(self) -> bool {
        match self {
            Self::NotCommitted => false,
            Self::Uncertain | Self::Corrupt => true,
        }
    }

    /// The `store_error` a request reports for its failed commit: `unknown`
    /// unless nothing was written (C1 §8.1).
    pub(super) fn api_error(self) -> ApiError {
        match self {
            Self::NotCommitted => ApiError::RECEIPT_NOT_COMMITTED,
            Self::Uncertain | Self::Corrupt => ApiError::RECEIPT_UNKNOWN,
        }
    }
}

/// Who a failed write belongs to (design §7.2 scope): the failure record's
/// `affected` addresses (§7.5).
#[derive(Clone, Copy, Debug)]
pub(super) enum FailureScope<'a> {
    /// A request that has not been receipted.
    Request,
    /// A receipted turn.
    Turn(&'a SessionId, TurnNumber),
    /// A session-level write.
    Session(&'a SessionId),
}

impl FailureScope<'_> {
    /// The C1 addresses of the failure's turn or session.
    fn addresses(self) -> Vec<String> {
        match self {
            Self::Request => Vec::new(),
            Self::Turn(session, turn) => vec![format!("{}/{}", session.as_str(), turn.get())],
            Self::Session(session) => vec![session.as_str().to_owned()],
        }
    }
}

/// Most addresses `store_failure.affected` lists (design §7.5).
const AFFECTED_ADDRESSES: usize = 16;

/// The latest Store failure and how many there were since daemon start
/// (design §7.5): an Engine `std` mutex, taken alone, never across an
/// `.await`. No wake.
#[derive(Default)]
pub(super) struct FailureRecord {
    latest: Option<LatestFailure>,
    count: u64,
}

/// The latest failure's `store_failure` fields; no prompt, payload or handle.
struct LatestFailure {
    kind: &'static str,
    scope: &'static str,
    since: String,
    addresses: Vec<String>,
}

/// `store_failure.kind` (design §7.5): corruption first, then an uncertain
/// outcome, then the site's own kind.
fn failure_kind(site: FailureSite, outcome: WriteOutcome) -> &'static str {
    match outcome {
        WriteOutcome::Corrupt => "corrupt_store",
        WriteOutcome::Uncertain => "commit_uncertain",
        WriteOutcome::NotCommitted => match site {
            FailureSite::Receipt
            | FailureSite::Submission
            | FailureSite::Event
            | FailureSite::Terminal
            | FailureSite::Resolution
            | FailureSite::RequestCancel
            | FailureSite::QueuedCancel
            | FailureSite::SessionClosed
            | FailureSite::Closing
            | FailureSite::Closed
            | FailureSite::ForcedTerminal
            | FailureSite::Batch => "commit_failed",
            FailureSite::Raw => "raw_failed",
            FailureSite::Journal | FailureSite::Absence => "journal_failed",
        },
    }
}

/// Phase two of a latch the failure hook began, if it latched; the caller
/// finishes it under `admission`, as its lock position allows. A scoped
/// failure's finish does nothing.
#[must_use = "phase two of the latch runs under admission"]
pub(super) struct Latching<'a> {
    engine: &'a Engine,
    latches: bool,
}

impl Latching<'_> {
    /// Whether the failure latched (design §7.4) rather than being scoped.
    pub(super) fn latches(&self) -> bool {
        self.latches
    }

    /// Takes `admission` and finalizes the latch; the caller holds no slot,
    /// session or head lock. A scoped failure takes nothing.
    pub(super) async fn finish(self) {
        if self.latches {
            let admission = self.engine.admission.lock().await;
            self.engine.latch_held(&admission);
        }
    }

    /// Finalizes the latch under the caller's `admission`.
    pub(super) fn finish_held(self, admission: &Admission<'_>) {
        if self.latches {
            self.engine.latch_held(admission);
        }
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
    /// every failure site reports its site, outcome and scope. Design §7.2's
    /// split (O1): a not-committed write is scoped to its request, turn or
    /// session; an uncertain one, SQLite corruption and a failed resolution
    /// write (the escalation) latch. Either way the failure record is
    /// updated (§7.5). A latch's phase one runs now; the caller finishes
    /// phase two. Lock: the failure-record mutex alone, then, when latching,
    /// the `stop` mutex alone. Wakes: the force watch (phase one).
    pub(super) fn store_failure(
        &self,
        site: FailureSite,
        outcome: WriteOutcome,
        scope: FailureScope<'_>,
    ) -> Latching<'_> {
        let latches = match outcome {
            WriteOutcome::Uncertain | WriteOutcome::Corrupt => true,
            WriteOutcome::NotCommitted => !site.scoped(),
        };
        self.record_failure(site, outcome, scope, latches);
        if latches {
            self.fail_pending();
        }
        Latching {
            engine: self,
            latches,
        }
    }

    /// Records the latest failure (design §7.5); the latch's scope is
    /// `daemon`.
    fn record_failure(
        &self,
        site: FailureSite,
        outcome: WriteOutcome,
        scope: FailureScope<'_>,
        latches: bool,
    ) {
        let latest = LatestFailure {
            kind: failure_kind(site, outcome),
            scope: if latches { "daemon" } else { site.scope() },
            since: rfc3339(SystemTime::now()),
            addresses: scope.addresses(),
        };
        let mut record = lock(&self.failures);
        record.count = record.count.saturating_add(1);
        record.latest = Some(latest);
    }

    /// `daemon/status` `health` (design §7.5): `store_failed` from the
    /// latch's phase one on, sticky.
    pub fn health(&self) -> &'static str {
        if self.store_failed() {
            "store_failed"
        } else {
            "healthy"
        }
    }

    /// `daemon/status` `store_failure` (design §7.5, amendment A9): `None`
    /// until the first failure, then the latest one with the count since
    /// daemon start. It carries no prompt, payload or handle.
    pub fn store_failure_status(&self) -> Option<Value> {
        let record = lock(&self.failures);
        let latest = record.latest.as_ref()?;
        let listed: Vec<&String> = latest.addresses.iter().take(AFFECTED_ADDRESSES).collect();
        Some(json!({
            "kind": latest.kind,
            "scope": latest.scope,
            "since": latest.since,
            "count": record.count,
            "affected": {"addresses": listed, "count": latest.addresses.len()},
        }))
    }

    /// The latching failure's phase-one time (design §7.4 [r3.17]): final
    /// shutdown's deadline and diagnostic window run from it.
    pub fn failed_at(&self) -> Option<tokio::time::Instant> {
        self.failed_at.get().copied()
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
        self.failed_at.get_or_init(tokio::time::Instant::now);
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
