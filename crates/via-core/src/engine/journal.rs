//! Turn-event and terminal commits whose outcome Store may leave uncertain.
//!
//! Store reports an uncertain commit when SQLite failed while committing or its
//! worker vanished with the reply; the event may or may not be durable. Core
//! settles such an event against the durable event head before `turn.ended`
//! takes the next sequence. A receipted turn whose terminal cannot be made
//! durable reads as C1 `store_error`, never as a running turn.

use std::{collections::HashSet, future::Future, sync::Mutex as StdMutex, time::SystemTime};

use serde_json::Value;
use via_store::{EventRecord, StoreClient, StoreError, StoredEvent, TerminalRecord};

use super::{Accepted, TurnRecord, lock};
use crate::api::{Event, EventBody, RawSpan, rfc3339};
use crate::{ApiError, RawRef, SessionId, TurnNumber};

/// Core's narrow Store port for one turn: `StoreClient` in production, a closed
/// fault backend in unit tests.
pub(super) trait TurnJournal: Sync {
    /// Commits one event of a running turn.
    fn commit_event(
        &self,
        record: EventRecord,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Commits the terminal envelope and `turn.ended` together.
    fn commit_terminal(
        &self,
        record: TerminalRecord,
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

    async fn commit_terminal(&self, record: TerminalRecord) -> Result<(), StoreError> {
        Self::commit_terminal(self, record).await
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

/// Receipted turns whose terminal could not be made durable (C1 `store_error`).
#[derive(Default)]
pub(super) struct Unresolved(StdMutex<HashSet<(SessionId, TurnNumber)>>);

impl Unresolved {
    pub(super) fn insert(&self, session: &SessionId, turn: TurnNumber) {
        lock(&self.0).insert((session.clone(), turn));
    }

    fn contains(&self, session: &SessionId, turn: TurnNumber) -> bool {
        lock(&self.0).contains(&(session.clone(), turn))
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

/// Commits the terminal record; an uncertain failure is settled by reading back
/// the durable result.
pub(super) async fn commit_terminal(
    journal: &impl TurnJournal,
    record: TerminalRecord,
) -> Result<(), ApiError> {
    let (session, turn) = (record.session_id.clone(), record.turn);
    match journal.commit_terminal(record).await {
        Ok(()) => Ok(()),
        Err(error) if may_have_committed(&error) => match journal.result(&session, turn).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) | Err(_) => Err(ApiError::STORE),
        },
        Err(_) => Err(ApiError::STORE),
    }
}

/// Reads a durable terminal result as is; an unresolved turn without one is
/// `store_error` rather than a turn that looks still running.
pub(super) async fn read_result(
    journal: &impl TurnJournal,
    unresolved: &Unresolved,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<Option<Value>, ApiError> {
    match journal.result(session, turn).await {
        Ok(Some(result)) => Ok(Some(result)),
        Ok(None) if unresolved.contains(session, turn) => Err(ApiError::STORE),
        Ok(None) => Ok(None),
        Err(_) => Err(ApiError::STORE),
    }
}

#[cfg(test)]
mod tests;
