//! Turns whose own Store writes or reads failed outside Core's event
//! commits (design §7.2, §7.3). The resolution write of a queued turn that
//! fails without agent I/O (row 2, §7.3): `commit_submit_failed` commits
//! `turn.submitted` and `turn.ended` `failed(store)`, `cancel: null`, in
//! one transaction (`queued → running → failed`, C1 §7.2); the live
//! dispatcher and the restart handoff share it. It resolves a submission
//! that did not commit, a corrupt frozen row and the dispatcher's expired
//! read streak. And a running turn whose Route reports a Store failure: a
//! Host journal write (rows 3 and 4) or the evidence folder.

use std::fmt;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;
use via_adapters::{RouteError, StoreFailure};
use via_store::{QueuedTurn, StoreClient, StoreError, SubmitFailedRecord};

use super::drive::{Step, SubmitFailure};
use super::journal::Head;
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use super::queue::Slot;
use super::terminal::terminal_envelope;
use super::{Engine, FailureNote, Terminal, TurnRecord, failure};
use crate::api::{Event, EventBody, FailureClass, Timestamps, Usage, rfc3339};
use crate::{SessionId, TurnNumber};

/// The failure message of a turn whose frozen row cannot be parsed.
pub(super) const CORRUPT_ROW: &str = "a frozen value of the queued turn could not be read";

/// The failure message of a turn whose submission intent did not commit.
pub(super) const SUBMISSION_FAILED: &str = "the turn's submission could not be recorded";

/// The failure message of a turn whose dispatcher could not read its
/// queued state for the whole read streak (design §7.3).
const READ_FAILED: &str = "the turn's queued state could not be read";

/// How long the head's read sequence may keep failing (design §7.3).
const READ_FAILURE_BOUND: Duration = Duration::from_secs(10);

/// The read streak's bound: 10 s. Test builds only:
/// `VIA_TEST_READ_FAILURE_MS` lowers it (design §11).
fn read_failure_bound() -> Duration {
    #[cfg(feature = "test-failpoints")]
    if let Some(lowered) = std::env::var("VIA_TEST_READ_FAILURE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(lowered);
    }
    READ_FAILURE_BOUND
}

/// The dispatcher's read-failure streak for its queue head (design §7.3
/// [r3.8]). It starts when the head's read sequence (predecessors, then the
/// queued row, then the head lock) first fails, with an absolute deadline,
/// and resets only when that sequence completes or the head changes. It
/// lives in the dispatcher and needs no lock; its wake is the dispatcher's
/// read-retry timer.
pub(super) struct ReadStreak {
    bound: Duration,
    head: Option<(TurnNumber, Instant)>,
}

impl ReadStreak {
    pub(super) fn new() -> Self {
        Self::with_bound(read_failure_bound())
    }

    fn with_bound(bound: Duration) -> Self {
        Self { bound, head: None }
    }

    /// The head's read sequence completed, or the head changed.
    pub(super) fn reset(&mut self) {
        self.head = None;
    }

    /// A read of head `turn`'s sequence failed at `now`. Returns the retry
    /// wake, `min(backoff, deadline − now)`, so the deadline is never
    /// overslept; `None` once the deadline has passed, which ends the
    /// streak.
    pub(super) fn failed(
        &mut self,
        turn: TurnNumber,
        now: Instant,
        backoff: Duration,
    ) -> Option<Duration> {
        let deadline = match self.head {
            Some((head, deadline)) if head == turn => deadline,
            _ => {
                let deadline = now + self.bound;
                self.head = Some((turn, deadline));
                deadline
            }
        };
        let left = deadline.saturating_duration_since(now);
        if left.is_zero() {
            self.head = None;
            return None;
        }
        Some(backoff.min(left))
    }
}

/// The committed queueing of the turn: its `turn.queued` time and sequence.
pub(super) struct Queueing {
    pub(super) queued_at: String,
    pub(super) queued_seq: u64,
}

impl From<&QueuedTurn> for Queueing {
    fn from(queued: &QueuedTurn) -> Self {
        Self {
            queued_at: queued.queued_at.clone(),
            queued_seq: queued.queued_seq,
        }
    }
}

/// Why the resolution write did not commit.
#[derive(Debug)]
pub(super) enum SubmitFailed {
    /// The session head could not be read: nothing was written.
    Head(StoreError),
    /// The session's sequence is exhausted: nothing was written.
    Exhausted,
    /// The events or the envelope could not be encoded: nothing was written.
    Encode,
    /// The commit failed.
    Commit(StoreError),
}

impl SubmitFailed {
    /// The write's outcome (design §7.1): only a commit can be uncertain;
    /// a corrupt head read is corruption (T3-S5 round 1, decision 10).
    pub(super) fn outcome(&self) -> WriteOutcome {
        match self {
            Self::Head(error) => WriteOutcome::of_read(error),
            Self::Exhausted | Self::Encode => WriteOutcome::NotCommitted,
            Self::Commit(error) => WriteOutcome::of(error),
        }
    }
}

impl fmt::Display for SubmitFailed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Head(error) | Self::Commit(error) => write!(formatter, "{error}"),
            Self::Exhausted => formatter.write_str("the session's sequence is exhausted"),
            Self::Encode => formatter.write_str("the resolution write could not be encoded"),
        }
    }
}

impl Engine {
    /// The dispatcher's step after `submit` failed for its claimed head
    /// `turn` (design §7.2 row 2, §7.3). A read that failed rolls the claim
    /// back into the read streak; an uncertain submission or SQLite
    /// corruption latches. A submission that did not commit, or a frozen
    /// row Core or Store cannot parse, fails the turn with row 2's
    /// resolution write, after `connection` was released: no agent I/O.
    /// A row Store cannot parse gives no queueing, which then comes from
    /// the committed `turn.queued` [s4.8].
    pub(super) async fn submit_failure(
        &self,
        slot: &Slot,
        (session, turn): (&SessionId, TurnNumber),
        failure: SubmitFailure,
        connection: OwnedSemaphorePermit,
    ) -> Step {
        let scope = FailureScope::Turn(session, turn);
        let (queueing, site, message) = match failure {
            SubmitFailure::Unread => {
                slot.rollback(turn);
                return Step::Unread(turn);
            }
            SubmitFailure::Failed(outcome) => {
                slot.rollback(turn);
                self.store_failure(FailureSite::Submission, outcome, scope)
                    .finish()
                    .await;
                return Step::Next;
            }
            SubmitFailure::NotCommitted(queueing) => {
                (Some(queueing), FailureSite::Submission, SUBMISSION_FAILED)
            }
            SubmitFailure::Corrupt(queueing) => (queueing, FailureSite::CorruptRow, CORRUPT_ROW),
        };
        // No agent I/O: the connection slot is released first.
        drop(connection);
        let queueing = if let Some(queueing) = queueing {
            queueing
        } else if let Ok(queueing) = self.queueing(session, turn).await {
            queueing
        } else {
            slot.rollback(turn);
            return Step::Unread(turn);
        };
        self.store_failure(site, WriteOutcome::NotCommitted, scope)
            .finish()
            .await;
        self.submit_failed(slot, session, turn, queueing, message)
            .await
    }

    /// The read streak of head `turn` expired (design §7.3): the dispatcher
    /// claims it and fails it with row 2's resolution write, without agent
    /// I/O. The queueing comes from the committed `turn.queued`, since the
    /// row may be what cannot be read; when even that read fails, the
    /// resolution write cannot be issued, which escalates. A head that
    /// changed, a close order, force or the latch leave the turn to their
    /// own path. The caller holds no lock.
    pub(super) async fn read_expired(
        &self,
        slot: &Slot,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Step {
        if !slot.claim(turn) {
            return Step::Next;
        }
        if slot.closing() || !self.grant() {
            slot.rollback(turn);
            return Step::Next;
        }
        let scope = FailureScope::Turn(session, turn);
        self.store_failure(FailureSite::Read, WriteOutcome::NotCommitted, scope)
            .finish()
            .await;
        let queueing = match self.queueing(session, turn).await {
            Ok(queueing) => queueing,
            Err(outcome) => {
                // The resolution write cannot be issued: it escalates. A
                // corrupt read was recorded at Store's read reply
                // (T3-S5 round 3, decision 13).
                slot.rollback(turn);
                self.store_failure(FailureSite::Resolution, outcome, scope)
                    .finish()
                    .await;
                return Step::Next;
            }
        };
        self.submit_failed(slot, session, turn, queueing, READ_FAILED)
            .await
    }

    /// Design §7.2 rows 3, 4 and 6 [O1.D10, D11]: a Store write the turn's
    /// Route depended on failed (`cause`), or a Host journal write of it
    /// had an uncertain outcome (`journal_uncertain`, §7.1). Route's Store
    /// failure is the turn's first failure, reported to the failure hook.
    /// One that did not commit stops the turn with cause `store`: the order
    /// is attached before the turn settles, so its disposition is `failed
    /// (store)` with the `cancel` evidence, and its terminal is the
    /// resolution write. One that may have committed latches. The caller
    /// holds no lock.
    pub(super) async fn route_failed(
        &self,
        slot: &Slot,
        record: &mut TurnRecord,
        cause: Option<&RouteError>,
        journal_uncertain: bool,
    ) {
        if journal_uncertain {
            let scope = FailureScope::Turn(&record.session, record.turn);
            self.store_failure(FailureSite::Journal, WriteOutcome::Uncertain, scope)
                .finish()
                .await;
        }
        let Some(RouteError::Store { kind, .. }) = cause else {
            return;
        };
        if record.first_failure.is_some() {
            // An earlier write already failed and keeps the turn's note for
            // its resolution write; Route's failure follows it. An uncertain
            // one still latches (design §7.1: every uncertain outcome).
            if kind.latches() {
                let scope = FailureScope::Turn(&record.session, record.turn);
                self.store_failure(FailureSite::Evidence, WriteOutcome::Uncertain, scope)
                    .finish()
                    .await;
            }
            return;
        }
        let site = match kind {
            StoreFailure::NotCommitted => FailureSite::Journal,
            StoreFailure::Evidence
            | StoreFailure::NotEnqueued
            | StoreFailure::WriterLost
            | StoreFailure::Uncertain => FailureSite::Evidence,
        };
        let outcome = if kind.latches() {
            WriteOutcome::Uncertain
        } else {
            WriteOutcome::NotCommitted
        };
        record.first_failure = Some(FailureNote { site, outcome });
        self.report_first_failure(record, false).await;
        if outcome == WriteOutcome::NotCommitted {
            slot.store_order(record.turn, tokio::time::Instant::now());
        }
    }

    /// Fails the claimed queue head `turn` with row 2's resolution write
    /// (design §7.2, §7.3), without agent I/O; the caller dropped its
    /// connection slot. On a commit the turn leaves the queue (its stop
    /// channels close, so a cancel waiting on them reads the terminal) and
    /// the session's successors dispatch normally. The write is the turn's
    /// one resolution write: any failure of it rolls the claim back and
    /// latches (escalation), after the head was released.
    pub(super) async fn submit_failed(
        &self,
        slot: &Slot,
        session: &SessionId,
        turn: TurnNumber,
        queueing: Queueing,
        message: &str,
    ) -> Step {
        let committed =
            commit_submit_failed(&self.store, &slot.head, (session, turn), queueing, message).await;
        match committed {
            Ok(()) => {
                slot.pop(turn);
                self.unresolved.resolve(session, turn);
                self.queued.fetch_sub(1, Ordering::AcqRel);
                self.active.fetch_sub(1, Ordering::AcqRel);
            }
            Err(error) => {
                slot.rollback(turn);
                self.store_failure(
                    FailureSite::Resolution,
                    error.outcome(),
                    FailureScope::Turn(session, turn),
                )
                .finish()
                .await;
            }
        }
        Step::Next
    }
}

/// Commits `turn` of `session` `failed(store)` with `message`, holding the
/// session head: the head advances by two on a commit and is re-read after
/// an uncertain one. No vendor I/O happened, so no duration is claimed.
pub(super) async fn commit_submit_failed(
    store: &StoreClient,
    head: &Head,
    (session, turn): (&SessionId, TurnNumber),
    queueing: Queueing,
    message: &str,
) -> Result<(), SubmitFailed> {
    let head = head
        .lock(store, session)
        .await
        .map_err(SubmitFailed::Head)?;
    let submitted_seq = head.next();
    let ended_seq = submitted_seq
        .checked_add(1)
        .ok_or(SubmitFailed::Exhausted)?;
    let at = rfc3339(SystemTime::now());
    let terminal = Terminal {
        state: "failed",
        failure: Some(failure(FailureClass::Store, message.to_owned(), None)),
        stop_reason: "error",
        vendor_stop_reason: None,
        final_text: String::new(),
        exit: None,
        warnings: Vec::new(),
        cancel: None,
    };
    let event = |seq, body| {
        Event {
            seq,
            session_id: session,
            turn: Some(turn.get()),
            late: false,
            at: &at,
            body,
        }
        .to_value()
        .map_err(|_| SubmitFailed::Encode)
    };
    let submitted = event(submitted_seq, EventBody::TurnSubmitted { attempt: 1 })?;
    let ended = event(
        ended_seq,
        EventBody::TurnEnded {
            state: terminal.state,
            failure: terminal.failure.clone(),
            stop_reason: terminal.stop_reason,
            cancel: None,
        },
    )?;
    let timestamps = Timestamps {
        queued_at: queueing.queued_at,
        submitted_at: Some(at.clone()),
        accepted_at: None,
        ended_at: at.clone(),
    };
    let envelope = terminal_envelope(
        session,
        turn,
        terminal,
        None,
        // Never submitted to a vendor: no evidence folder (design §7.1).
        None,
        timestamps,
        None,
        (queueing.queued_seq, ended_seq),
        Usage::UNAVAILABLE,
    );
    let envelope = serde_json::to_value(&envelope).map_err(|_| SubmitFailed::Encode)?;
    let committed = store
        .commit_submit_failed(SubmitFailedRecord {
            session_id: session.clone(),
            turn,
            submitted,
            ended,
            envelope,
        })
        .await;
    match committed {
        Ok(()) => {
            head.committed(2);
            Ok(())
        }
        Err(error) => {
            if WriteOutcome::of(&error).head_unknown() {
                head.lost();
            }
            Err(SubmitFailed::Commit(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::Instant;

    use super::ReadStreak;
    use crate::TurnNumber;

    fn turn(n: u32) -> TurnNumber {
        TurnNumber::try_from(n).unwrap()
    }

    /// Design §7.3 [r3.8]: the streak's deadline is absolute from its first
    /// failure; its wakes never oversleep it; a new head starts a new
    /// streak, and a reset one starts over.
    #[test]
    fn the_read_streak_keeps_an_absolute_deadline() {
        let bound = Duration::from_secs(10);
        let backoff = Duration::from_secs(4);
        let mut streak = ReadStreak::with_bound(bound);
        let start = Instant::now();
        assert_eq!(streak.failed(turn(1), start, backoff), Some(backoff));
        let late = start + Duration::from_secs(8);
        assert_eq!(
            streak.failed(turn(1), late, backoff),
            Some(Duration::from_secs(2)),
            "the wake stops at the deadline"
        );
        assert_eq!(streak.failed(turn(1), start + bound, backoff), None);
        // The streak ended: the same head starts a new one.
        assert_eq!(
            streak.failed(turn(1), start + bound, backoff),
            Some(backoff)
        );
        // Another head starts its own.
        let other = start + bound + Duration::from_secs(9);
        assert_eq!(streak.failed(turn(2), other, backoff), Some(backoff));
        streak.reset();
        let after = other + Duration::from_secs(9);
        assert_eq!(streak.failed(turn(2), after, backoff), Some(backoff));
        assert_eq!(streak.failed(turn(2), after + bound, backoff), None);
    }
}
