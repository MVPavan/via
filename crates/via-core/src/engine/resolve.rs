//! The resolution write of a queued turn that fails without agent I/O
//! (design §7.2 row 2, §7.3): `commit_submit_failed` commits `turn.submitted`
//! and `turn.ended` `failed(store)`, `cancel: null`, in one transaction
//! (`queued → running → failed`, C1 §7.2). The live dispatcher and the
//! restart handoff share it.

use std::fmt;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use via_store::{StoreClient, StoreError, SubmitFailedRecord};

use super::drive::Step;
use super::journal::Head;
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use super::queue::Slot;
use super::terminal::terminal_envelope;
use super::{Engine, Terminal, failure};
use crate::api::{Event, EventBody, FailureClass, Timestamps, rfc3339};
use crate::{SessionId, TurnNumber};

/// The failure message of a turn whose frozen row cannot be parsed.
pub(super) const CORRUPT_ROW: &str = "a frozen value of the queued turn could not be read";

/// The failure message of a turn whose submission intent did not commit.
pub(super) const SUBMISSION_FAILED: &str = "the turn's submission could not be recorded";

/// The committed queueing of the turn: its `turn.queued` time and sequence.
pub(super) struct Queueing {
    pub(super) queued_at: String,
    pub(super) queued_seq: u64,
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
    /// The write's outcome (design §7.1): only a commit can be uncertain.
    pub(super) fn outcome(&self) -> WriteOutcome {
        match self {
            Self::Head(_) | Self::Exhausted | Self::Encode => WriteOutcome::NotCommitted,
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
        raw_ref: None,
        raw_incomplete: false,
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
            raw_ref: None,
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
        Vec::new(),
        timestamps,
        None,
        (queueing.queued_seq, ended_seq),
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
