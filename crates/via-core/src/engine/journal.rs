//! Turn-event and terminal commits whose outcome Store may leave uncertain.
//!
//! Store reports an uncertain commit when SQLite failed while committing or its
//! worker vanished with the reply; the event may or may not be durable. Core
//! settles such an event against the durable event head before `turn.ended`
//! takes the next sequence. A receipted turn whose terminal cannot be made
//! durable reads as C1 `store_error`, never as a running turn.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime},
};

use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};
use via_store::{
    EventRecord, QueuedTurn, StoreClient, StoreError, StoredEvent, SubmissionRecord,
    TerminalExtras, TerminalRecord,
};

use super::latch::{FailureSite, WriteOutcome};
use super::{Accepted, FailureNote, TurnRecord, lock};
use crate::api::{Event, EventBody, ReceiptOutcome, rfc3339};
use crate::{ApiError, SessionId, TurnNumber, TurnState};

/// Core's narrow Store port for one turn: `StoreClient` in production, a closed
/// fault backend in unit tests.
pub(super) trait TurnJournal: Sync {
    /// Commits submission intent with `turn.submitted`.
    fn commit_submission(
        &self,
        record: SubmissionRecord,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Commits one event of a running turn.
    fn commit_event(
        &self,
        record: EventRecord,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Commits the terminal envelope and `turn.ended` together; with `closed`,
    /// that `session.closed` event follows in the same transaction unless
    /// another turn of the session is queued or running. True when the close
    /// was written.
    fn commit_terminal(
        &self,
        record: TerminalRecord,
        closed: Option<Value>,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;
    /// Commits the terminal with the facts of its own transaction, such as
    /// a cancellation's `cancel_cause` (design §4, §10).
    fn commit_terminal_with(
        &self,
        record: TerminalRecord,
        extras: TerminalExtras,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Reads a bounded page of durable events from `from_seq`.
    fn events(
        &self,
        session: &SessionId,
        from_seq: u64,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<StoredEvent>, StoreError>> + Send;
    /// Returns those of `turns` whose terminal has committed, in one bounded read.
    fn terminated(
        &self,
        turns: Vec<(SessionId, TurnNumber)>,
    ) -> impl Future<Output = Result<Vec<(SessionId, TurnNumber)>, StoreError>> + Send;
    /// Reads a committed terminal envelope, if any.
    fn result(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> impl Future<Output = Result<Option<Value>, StoreError>> + Send;
    /// Reads a queued turn's prompt and `turn.queued` facts.
    fn queued_turn(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> impl Future<Output = Result<Option<QueuedTurn>, StoreError>> + Send;
    /// Reads the session's durable next event sequence.
    fn next_seq(
        &self,
        session: &SessionId,
    ) -> impl Future<Output = Result<Option<u64>, StoreError>> + Send;
}

impl TurnJournal for StoreClient {
    async fn commit_submission(&self, record: SubmissionRecord) -> Result<(), StoreError> {
        Self::commit_submission(self, record).await
    }

    async fn commit_event(&self, record: EventRecord) -> Result<(), StoreError> {
        Self::commit_event(self, record).await
    }

    async fn commit_terminal(
        &self,
        record: TerminalRecord,
        closed: Option<Value>,
    ) -> Result<bool, StoreError> {
        match closed {
            Some(closed) => Self::commit_closing_terminal(self, record, closed).await,
            None => Self::commit_terminal(self, record).await.map(|()| false),
        }
    }

    async fn commit_terminal_with(
        &self,
        record: TerminalRecord,
        extras: TerminalExtras,
    ) -> Result<(), StoreError> {
        Self::commit_terminal_with(self, record, extras).await
    }

    async fn events(
        &self,
        session: &SessionId,
        from_seq: u64,
        limit: u32,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        Self::events(self, session, from_seq, limit).await
    }

    async fn terminated(
        &self,
        turns: Vec<(SessionId, TurnNumber)>,
    ) -> Result<Vec<(SessionId, TurnNumber)>, StoreError> {
        Self::terminated(self, turns).await
    }

    async fn result(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<Value>, StoreError> {
        Self::result(self, session, turn).await
    }

    async fn queued_turn(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<QueuedTurn>, StoreError> {
        Self::queued_turn(self, session, turn).await
    }

    async fn next_seq(&self, session: &SessionId) -> Result<Option<u64>, StoreError> {
        Self::next_seq(self, session).await
    }
}

/// A session's next event sequence, shared by every writer of the session, so
/// its events stay dense while one turn runs and later turns queue (C1 §6.1).
/// Each writer allocates and commits under the lock. After a commit whose
/// outcome is unknown the head is unknown until re-read from the Store.
pub(super) struct Head(Mutex<Option<u64>>);

impl Head {
    pub(super) fn new(next: Option<u64>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(next)))
    }

    /// Locks the head, first re-reading the durable next sequence when unknown.
    /// A writer that finds the head held, as by a same-sequence retry
    /// (design §7.2 [r3.7]), waits for it (test builds acknowledge the wait
    /// at `core.head.contended`).
    pub(super) async fn lock(
        &self,
        journal: &impl TurnJournal,
        session: &SessionId,
    ) -> Result<HeadGuard<'_>, StoreError> {
        let mut guard = if let Ok(guard) = self.0.try_lock() {
            guard
        } else {
            // The head is held: this writer waits (acknowledgement only).
            #[cfg(feature = "test-failpoints")]
            let _ = via_store::failpoint::hit_async("core.head.contended").await;
            self.0.lock().await
        };
        let next = match *guard {
            Some(next) => next,
            None => journal
                .next_seq(session)
                .await?
                .ok_or(StoreError::Constraint("session does not exist"))?,
        };
        *guard = Some(next);
        Ok(HeadGuard { guard, next })
    }
}

/// The locked head; a guard dropped without an outcome leaves it unchanged,
/// as for a commit that definitely did not happen.
pub(super) struct HeadGuard<'a> {
    guard: MutexGuard<'a, Option<u64>>,
    next: u64,
}

impl HeadGuard<'_> {
    /// The sequence the next event takes.
    pub(super) fn next(&self) -> u64 {
        self.next
    }

    /// `count` events committed from `next`.
    pub(super) fn committed(mut self, count: u64) {
        *self.guard = Some(self.next + count);
    }

    /// A commit's outcome is unknown: re-read the head before the next event.
    pub(super) fn lost(mut self) {
        *self.guard = None;
    }
}

/// Whether a failed commit may nonetheless be durable.
pub(super) fn may_have_committed(error: &StoreError) -> bool {
    matches!(error, StoreError::Uncertain(_) | StoreError::WriterLost)
}

/// An event commit Store did not confirm but may have made durable. After the
/// first Store failure Core commits nothing more, so a turn holds at most one.
#[derive(Clone)]
pub(super) struct UncertainEvent {
    pub(super) seq: u64,
    /// The event as sent: only this exact event at `seq` is its own.
    pub(super) event: Value,
    /// Acceptance facts to restore if the uncertain event was `turn.started`.
    pub(super) accepted: Option<Accepted>,
}

/// Most receipted turns without a terminal known durable, in flight or failed,
/// that the daemon retains. While this many are retained, spawn refuses new work:
/// `admission_refused` when all are in flight, `store_error` while a failed turn
/// is retained, so no accepted turn loses its C1 `store_error` read.
pub(super) const UNRESOLVED_LIMIT: usize = 256;

/// Bound on the Store reads that settle failed turns before admission is refused.
const SETTLE_BOUND: Duration = Duration::from_secs(2);

/// What Core knows of a receipted turn with no terminal known to be durable.
#[derive(Clone, Copy)]
enum Entry {
    /// The turn has not tried its terminal.
    Pending,
    /// Its terminal could not be made durable; the last committed turn state.
    Failed(TurnState),
}

/// Receipted turns with no terminal known to have committed. An entry is
/// removed once a terminal is known committed, including a failed turn whose
/// terminal a later read finds durable. Receipts stop at [`UNRESOLVED_LIMIT`]
/// entries. Final shutdown is clean only when the set is empty.
#[derive(Default)]
pub(super) struct Unresolved(StdMutex<HashMap<(SessionId, TurnNumber), Entry>>);

impl Unresolved {
    /// Tracks a receipted turn until its terminal is known committed.
    pub(super) fn receipt(&self, session: &SessionId, turn: TurnNumber) {
        lock(&self.0).insert((session.clone(), turn), Entry::Pending);
    }

    /// Records that the turn's terminal could not be made durable after its last
    /// committed lifecycle state `durable`.
    pub(super) fn fail(&self, session: &SessionId, turn: TurnNumber, durable: TurnState) {
        lock(&self.0).insert((session.clone(), turn), Entry::Failed(durable));
    }

    /// Forgets a turn whose terminal is known committed.
    pub(super) fn resolve(&self, session: &SessionId, turn: TurnNumber) {
        lock(&self.0).remove(&(session.clone(), turn));
    }

    /// Whether another turn of `session` than `turn` is still unresolved.
    pub(super) fn others(&self, session: &SessionId, turn: TurnNumber) -> bool {
        lock(&self.0)
            .keys()
            .any(|(unresolved, number)| unresolved == session && *number != turn)
    }

    /// Whether another receipt keeps the set within its bound.
    fn admits(&self) -> bool {
        lock(&self.0).len() < UNRESOLVED_LIMIT
    }

    /// Failed turns, whose terminals a later read may find durable.
    fn failed_turns(&self) -> Vec<(SessionId, TurnNumber)> {
        lock(&self.0)
            .iter()
            .filter(|(_, entry)| matches!(entry, Entry::Failed(_)))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Every turn not yet known to have a durable terminal.
    pub(super) fn turns(&self) -> Vec<(SessionId, TurnNumber)> {
        lock(&self.0).keys().cloned().collect()
    }

    /// The last committed state of a turn whose terminal could not be made durable.
    fn failed(&self, session: &SessionId, turn: TurnNumber) -> Option<TurnState> {
        match lock(&self.0).get(&(session.clone(), turn)) {
            Some(Entry::Failed(durable)) => Some(*durable),
            Some(Entry::Pending) | None => None,
        }
    }

    /// Forgets a failed turn whose terminal a read found durable after all.
    fn settle(&self, session: &SessionId, turn: TurnNumber) {
        let mut entries = lock(&self.0);
        let key = (session.clone(), turn);
        if matches!(entries.get(&key), Some(Entry::Failed(_))) {
            entries.remove(&key);
        }
    }
}

/// Admits another receipt within the bound. When the set is full, failed turns
/// whose terminals have since become durable are forgotten first. A set still
/// full of in-flight turns is C1 `admission_refused`; one that still retains a
/// failed turn is `store_error`.
pub(super) async fn admission(
    journal: &impl TurnJournal,
    unresolved: &Unresolved,
) -> Result<(), ApiError> {
    if unresolved.admits() {
        return Ok(());
    }
    settle_failed(journal, unresolved).await;
    if unresolved.admits() {
        Ok(())
    } else if unresolved.failed_turns().is_empty() {
        Err(ApiError::TURNS_AT_CAPACITY)
    } else {
        Err(ApiError::STORE)
    }
}

/// Asks Store in one operation which failed turns now have a durable terminal,
/// so every candidate is inspected however many there are, and forgets those.
/// The query is bounded by [`SETTLE_BOUND`]; a failed or expired query keeps
/// every turn.
async fn settle_failed(journal: &impl TurnJournal, unresolved: &Unresolved) {
    let query = journal.terminated(unresolved.failed_turns());
    if let Ok(Ok(terminated)) = tokio::time::timeout(SETTLE_BOUND, query).await {
        for (session, turn) in terminated {
            unresolved.settle(&session, turn);
        }
    }
}

/// Commits one non-lifecycle event of the running turn at the next sequence.
/// After the turn's first failed write nothing more is written; a failure
/// is recorded as the turn's `first_failure` (design §7.2).
pub(super) async fn commit_event(
    journal: &impl TurnJournal,
    record: &mut TurnRecord,
    body: EventBody,
) {
    let at = rfc3339(SystemTime::now());
    commit_event_at(journal, record, body, &at).await;
}

/// [`commit_event`] at a given wall time: a stop order's `cancel.requested`
/// keeps the order's `requested_at` (design §2).
pub(super) async fn commit_event_at(
    journal: &impl TurnJournal,
    record: &mut TurnRecord,
    body: EventBody,
    at: &str,
) {
    if record.first_failure.is_some() {
        return;
    }
    let failed = |outcome| {
        Some(FailureNote {
            site: FailureSite::Event,
            outcome,
        })
    };
    let shared = Arc::clone(&record.head);
    let head = match shared.lock(journal, &record.session).await {
        Ok(head) => head,
        Err(error) => {
            // The head's read failed: nothing was written; corruption latches.
            record.first_failure = failed(WriteOutcome::of_read(&error));
            return;
        }
    };
    let seq = head.next();
    let event = Event {
        seq,
        session_id: &record.session,
        turn: Some(record.turn.get()),
        late: false,
        at,
        body,
    }
    .to_value();
    let Ok(event) = event else {
        record.first_failure = failed(WriteOutcome::NotCommitted);
        return;
    };
    // Test builds: the event is about to be sent to the Store writer, which
    // stays free while this pauses (design §10 [r5.7]).
    #[cfg(feature = "test-failpoints")]
    let _ = via_store::failpoint::hit_async("core.commit.before_send").await;
    let committed = journal
        .commit_event(EventRecord {
            session_id: record.session.clone(),
            turn: record.turn,
            event: event.clone(),
        })
        .await;
    if let Err(error) = committed {
        let outcome = WriteOutcome::of(&error);
        record.first_failure = failed(outcome);
        if outcome.head_unknown() {
            head.lost();
            record.uncertain = Some(UncertainEvent {
                seq,
                event,
                accepted: None,
            });
        }
        return;
    }
    head.committed(1);
}

/// Settles an uncertain event against the durable stream: a durable event
/// advances `record` exactly as a confirmed commit would; an absent one leaves
/// it. Another writer of the session may have taken the sequence since, so
/// only an event of this turn at that sequence is its own, and only when it
/// is the event sent.
pub(super) async fn reconcile(
    journal: &impl TurnJournal,
    record: &mut TurnRecord,
) -> Result<(), StoreError> {
    let Some(uncertain) = record.uncertain.take() else {
        return Ok(());
    };
    let head = journal.events(&record.session, uncertain.seq, 1).await?;
    let own_turn = json!(record.turn.get());
    match head.first() {
        None => Ok(()),
        Some(event) if event.seq != uncertain.seq || event.event.get("turn") != Some(&own_turn) => {
            Ok(())
        }
        Some(event) if event.event == uncertain.event => {
            if uncertain.accepted.is_some() {
                record.accepted = uncertain.accepted;
            }
            Ok(())
        }
        // Core is the running turn's only writer; another event of it there is not its own.
        Some(_) => Err(StoreError::CorruptEvidence),
    }
}

/// A terminal record that is durable. `uncertain` when Store reported an
/// unknown outcome and only the read-back found it: the commit itself was
/// uncertain, which latches Store failure (runtime §7), while the committed
/// result stays readable. `closed` when a requested `session.closed` is known
/// written; Store refuses it while another turn of the session is unfinished.
/// `retried` when the first attempt was not committed and the one
/// same-sequence retry committed (design §7.2 rows 7 and 9).
#[derive(Clone, Copy, Debug)]
pub(super) struct Durable {
    pub(super) uncertain: bool,
    pub(super) closed: bool,
    pub(super) retried: bool,
}

/// Commits the terminal record (and `closed`, if any, atomically with it); an
/// uncertain failure is settled by reading back the durable result. A
/// failure carries its outcome in `commit_outcome` ([`outcome_of`]).
pub(super) async fn commit_terminal(
    journal: &impl TurnJournal,
    record: TerminalRecord,
    closed: Option<Value>,
) -> Result<Durable, ApiError> {
    commit_terminal_with(journal, record, closed, TerminalExtras::default(), false).await
}

/// [`commit_terminal`] with the terminal's `extras` (design §4, §10). A
/// cancellation's cause never rides with a force closure: the two are
/// refused together, writing nothing.
///
/// With `retry` (design §7.2 rows 7 and 9 [r3.7, r3.13]), a first attempt
/// that is known not committed is retried once with the same content and
/// sequence numbers. The caller holds the session head across both
/// attempts, so no other writer takes the sequence in between; test builds
/// can pause before the retry at `core.retry.before`. The retry is the
/// resolution write: its failure is returned as the terminal's.
pub(super) async fn commit_terminal_with(
    journal: &impl TurnJournal,
    record: TerminalRecord,
    closed: Option<Value>,
    extras: TerminalExtras,
    retry: bool,
) -> Result<Durable, ApiError> {
    let (session, turn) = (record.session_id.clone(), record.turn);
    let plain = extras.cancel_cause.is_none();
    if closed.is_some() && !plain {
        return Err(ApiError::RECEIPT_NOT_COMMITTED);
    }
    let again = retry.then(|| {
        (
            duplicate(&record),
            closed.clone(),
            TerminalExtras {
                cancel_cause: extras.cancel_cause,
            },
        )
    });
    let mut committed = attempt(journal, record, closed, extras).await;
    let mut retried = false;
    if let (Err(error), Some((record, closed, extras))) = (&committed, again)
        && !may_have_committed(error)
        && !matches!(error, StoreError::Corrupt(_))
    {
        // The first attempt rolled back; the retry holds the same head.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.retry.before").await;
        retried = true;
        committed = attempt(journal, record, closed, extras).await;
    }
    match committed {
        Ok(closed) => Ok(Durable {
            uncertain: false,
            closed,
            retried,
        }),
        Err(error) if may_have_committed(&error) || matches!(error, StoreError::Corrupt(_)) => {
            match journal.result(&session, turn).await {
                Ok(Some(_)) => Ok(Durable {
                    uncertain: true,
                    closed: false,
                    retried,
                }),
                Ok(None) | Err(_) => Err(ApiError::RECEIPT_UNKNOWN),
            }
        }
        Err(_) => Err(ApiError::RECEIPT_NOT_COMMITTED),
    }
}

/// One attempt of [`commit_terminal_with`]; true when the close was written.
async fn attempt(
    journal: &impl TurnJournal,
    record: TerminalRecord,
    closed: Option<Value>,
    extras: TerminalExtras,
) -> Result<bool, StoreError> {
    if extras.cancel_cause.is_none() {
        journal.commit_terminal(record, closed).await
    } else {
        journal
            .commit_terminal_with(record, extras)
            .await
            .map(|()| false)
    }
}

/// A copy of `record` for the same-sequence retry: the same envelope and
/// events.
fn duplicate(record: &TerminalRecord) -> TerminalRecord {
    TerminalRecord {
        session_id: record.session_id.clone(),
        turn: record.turn,
        envelope: record.envelope.clone(),
        event: record.event.clone(),
    }
}

/// A terminal commit that did not become durable: the `store_error` its
/// caller reports, and the outcome the failure hook classifies (design
/// §7.1). A corrupt session-head read before the commit is `Corrupt`
/// although the reply is a plain `store_error` (T3-S5 round 1, decision 10).
#[derive(Debug)]
pub(super) struct Unended {
    pub(super) error: ApiError,
    pub(super) outcome: WriteOutcome,
}

impl From<ApiError> for Unended {
    fn from(error: ApiError) -> Self {
        Self {
            outcome: outcome_of(&error),
            error,
        }
    }
}

/// The outcome of a failed terminal commit (design §7.1): unknown only when
/// the commit may have written; an error before it wrote nothing.
pub(super) fn outcome_of(error: &ApiError) -> WriteOutcome {
    match error.commit_outcome {
        Some(ReceiptOutcome::Unknown) => WriteOutcome::Uncertain,
        Some(ReceiptOutcome::NotCommitted) | None => WriteOutcome::NotCommitted,
    }
}

/// Reads a durable terminal result as is; a turn whose terminal could not be
/// made durable is C1 `store_error` with its last committed state, never a turn
/// that looks still running.
#[cfg(test)]
pub(super) async fn read_result(
    journal: &impl TurnJournal,
    unresolved: &Unresolved,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<Option<Value>, ApiError> {
    let read = journal.result(session, turn).await;
    settled_result(unresolved, session, turn, read)
}

/// The result `read`, already made (the caller classifies its Store error
/// first, design §7.3): a durable result settles the turn; a turn whose
/// terminal could not be made durable is `store_error` with its last
/// committed state.
pub(super) fn settled_result(
    unresolved: &Unresolved,
    session: &SessionId,
    turn: TurnNumber,
    read: Result<Option<Value>, StoreError>,
) -> Result<Option<Value>, ApiError> {
    match read {
        Ok(Some(result)) => {
            unresolved.settle(session, turn);
            Ok(Some(result))
        }
        read => match unresolved.failed(session, turn) {
            Some(durable) => Err(ApiError::unpersisted(session, turn, durable)),
            None => read.map_err(|_| ApiError::STORE),
        },
    }
}

#[cfg(test)]
mod tests;
