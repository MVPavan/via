//! Turn-event and terminal commits whose outcome Store may leave uncertain.
//!
//! Store reports an uncertain commit when SQLite failed while committing or its
//! worker vanished with the reply; the event may or may not be durable. Core
//! settles such an event against the durable event head before `turn.ended`
//! takes the next sequence. A receipted turn whose terminal cannot be made
//! durable reads as C1 `store_error`, never as a running turn.

use std::{collections::HashMap, future::Future, sync::Mutex as StdMutex, time::SystemTime};

use serde_json::Value;
use via_store::{EventRecord, StoreClient, StoreError, StoredEvent, TerminalRecord};

use super::{Accepted, TurnRecord, lock};
use crate::api::{Event, EventBody, RawSpan, rfc3339};
use crate::{ApiError, RawRef, SessionId, TurnNumber, TurnState};

/// Core's narrow Store port for one turn: `StoreClient` in production, a closed
/// fault backend in unit tests.
pub(super) trait TurnJournal: Sync {
    /// Commits one event of a running turn.
    fn commit_event(
        &self,
        record: EventRecord,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Commits the terminal envelope and `turn.ended` together; with `closed`,
    /// that `session.closed` event follows in the same transaction.
    fn commit_terminal(
        &self,
        record: TerminalRecord,
        closed: Option<Value>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Reads a bounded page of durable events from `from_seq`.
    fn events(
        &self,
        session: &SessionId,
        from_seq: u64,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<StoredEvent>, StoreError>> + Send;
    /// Reads a committed terminal envelope, if any.
    fn result(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> impl Future<Output = Result<Option<Value>, StoreError>> + Send;
}

impl TurnJournal for StoreClient {
    async fn commit_event(&self, record: EventRecord) -> Result<(), StoreError> {
        Self::commit_event(self, record).await
    }

    async fn commit_terminal(
        &self,
        record: TerminalRecord,
        closed: Option<Value>,
    ) -> Result<(), StoreError> {
        match closed {
            Some(closed) => Self::commit_closing_terminal(self, record, closed).await,
            None => Self::commit_terminal(self, record).await,
        }
    }

    async fn events(
        &self,
        session: &SessionId,
        from_seq: u64,
        limit: u32,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        Self::events(self, session, from_seq, limit).await
    }

    async fn result(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<Value>, StoreError> {
        Self::result(self, session, turn).await
    }
}

/// Whether a failed commit may nonetheless be durable.
pub(super) fn may_have_committed(error: &StoreError) -> bool {
    matches!(error, StoreError::Uncertain(_) | StoreError::Unavailable)
}

/// An event commit Store did not confirm but may have made durable. After the
/// first Store failure Core commits nothing more, so a turn holds at most one.
pub(super) struct UncertainEvent {
    pub(super) seq: u64,
    pub(super) raw_ref: Option<RawRef>,
    /// Acceptance facts to restore if the uncertain event was `turn.started`.
    pub(super) accepted: Option<Accepted>,
}

/// Most receipted turns whose terminal could not be made durable that the daemon
/// retains; while this many are retained, spawn refuses new work with
/// `store_error`, so no affected turn loses its C1 `store_error` read.
pub(super) const FAILED_TURNS_LIMIT: usize = 256;

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
/// terminal a later read finds durable. Failed entries are bounded by
/// [`FAILED_TURNS_LIMIT`] plus the turns in flight when it was reached. Final
/// shutdown is clean only when the set is empty.
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

    /// Whether another receipt keeps failed turns within their bound.
    pub(super) fn admits(&self) -> bool {
        let failed = lock(&self.0)
            .values()
            .filter(|entry| matches!(entry, Entry::Failed(_)))
            .count();
        failed < FAILED_TURNS_LIMIT
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

/// Commits one non-lifecycle event of the running turn at the next sequence.
pub(super) async fn commit_event(
    journal: &impl TurnJournal,
    record: &mut TurnRecord,
    body: EventBody,
    raw_ref: Option<RawRef>,
) {
    if record.store_failed {
        return;
    }
    let seq = record.seq + 1;
    let at = rfc3339(SystemTime::now());
    let event = Event {
        seq,
        session_id: &record.session,
        turn: Some(record.turn.get()),
        late: false,
        at: &at,
        raw_ref: raw_ref.as_ref(),
        body,
    }
    .to_value();
    let Ok(event) = event else {
        record.store_failed = true;
        return;
    };
    let committed = journal
        .commit_event(EventRecord {
            session_id: record.session.clone(),
            turn: record.turn,
            event,
            raw_ref: raw_ref.clone(),
        })
        .await;
    if let Err(error) = committed {
        record.store_failed = true;
        if may_have_committed(&error) {
            record.uncertain = Some(UncertainEvent {
                seq,
                raw_ref,
                accepted: None,
            });
        }
        return;
    }
    record.seq = seq;
    if let Some(reference) = &raw_ref {
        RawSpan::include(&mut record.spans, reference);
    }
}

/// Settles an uncertain event against the durable head: a durable event
/// advances `record` exactly as a confirmed commit would; an absent one leaves it.
pub(super) async fn reconcile(
    journal: &impl TurnJournal,
    record: &mut TurnRecord,
) -> Result<(), StoreError> {
    let Some(uncertain) = record.uncertain.take() else {
        return Ok(());
    };
    let head = journal.events(&record.session, uncertain.seq, 1).await?;
    match head.first() {
        None => Ok(()),
        Some(event) if event.seq == uncertain.seq && event.raw_ref == uncertain.raw_ref => {
            record.seq = uncertain.seq;
            if let Some(reference) = &uncertain.raw_ref {
                RawSpan::include(&mut record.spans, reference);
            }
            if uncertain.accepted.is_some() {
                record.accepted = uncertain.accepted;
            }
            Ok(())
        }
        // Core is the running turn's only writer; another head is not its event.
        Some(_) => Err(StoreError::CorruptEvidence),
    }
}

/// Commits the terminal record (and `closed`, if any, atomically with it); an
/// uncertain failure is settled by reading back the durable result.
pub(super) async fn commit_terminal(
    journal: &impl TurnJournal,
    record: TerminalRecord,
    closed: Option<Value>,
) -> Result<(), ApiError> {
    let (session, turn) = (record.session_id.clone(), record.turn);
    match journal.commit_terminal(record, closed).await {
        Ok(()) => Ok(()),
        Err(error) if may_have_committed(&error) => match journal.result(&session, turn).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) | Err(_) => Err(ApiError::STORE),
        },
        Err(_) => Err(ApiError::STORE),
    }
}

/// Reads a durable terminal result as is; a turn whose terminal could not be
/// made durable is C1 `store_error` with its last committed state, never a turn
/// that looks still running.
pub(super) async fn read_result(
    journal: &impl TurnJournal,
    unresolved: &Unresolved,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<Option<Value>, ApiError> {
    match journal.result(session, turn).await {
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
