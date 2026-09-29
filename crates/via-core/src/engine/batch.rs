//! Final shutdown's failure-resolution batch (design §7.4 [O1.D4]). An
//! affected running turn, one of whose own writes was uncertain or whose
//! resolution write failed, is kept for final shutdown with its `Started`
//! facts and `TurnRecord`. After Host reconciliation it is re-read first;
//! only a turn still without a terminal gets one `commit_failure_resolution`:
//! `turn.ended` `failed(store)` with its stop evidence, and `queued →
//! cancelled` (`cancel: null`) for every queued turn of its session, in one
//! transaction. Nothing retries a batch that failed or was skipped.

use std::sync::atomic::Ordering;
use std::time::Duration;

use via_store::{FAILURE_BATCH_CANCELLATIONS, FailureResolutionRecord, QueuedTurn, StoreClient};

use super::drive::{ended_record, queued_cancellation};
use super::journal;
use super::latch::{FINALIZE_WRITE, FailureScope, FailureSite, WriteOutcome};
use super::queue::Slot;
use super::{Engine, Started, Terminal, TurnRecord, lock};
use crate::api::FailureClass;
use crate::{Deadline, SessionId, TurnNumber};

/// Bound of the batch's re-read of the turn and its session's queued rows.
const BATCH_READ: Duration = Duration::from_secs(2);

/// The failure message of a turn whose terminal the batch writes after the
/// terminal's own write failed.
const TERMINAL_LOST: &str = "the turn's terminal could not be recorded";

/// A running turn kept for the batch: its facts, its record and the
/// terminal Core decided, which the batch commits as `failed(store)`.
pub(super) struct AffectedTurn {
    pub(super) started: Started,
    pub(super) record: TurnRecord,
    pub(super) terminal: Terminal,
}

/// The summary's `failure_batches` (design §7.4): batches committed, and
/// batches skipped because a read or the write failed or timed out.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FailureBatches {
    /// Batches that committed.
    pub committed: usize,
    /// Batches that were not issued or did not commit.
    pub skipped: usize,
}

/// Whether a forced turn is affected (design §7.4): its own write was
/// uncertain or found the database corrupt. A turn whose first failure was
/// not committed keeps final shutdown's single best-effort terminal.
pub(super) fn affected(record: &TurnRecord) -> bool {
    record
        .first_failure
        .is_some_and(|note| note.outcome != WriteOutcome::NotCommitted)
}

impl Engine {
    /// Keeps a running turn whose terminal write failed for final
    /// shutdown's batch. The affected list's mutex is taken alone. No wake:
    /// final shutdown collects the list after the dispatchers joined.
    pub(super) fn keep_affected(&self, turn: AffectedTurn) {
        lock(&self.affected).push(turn);
    }

    /// Takes every running turn kept for the batch.
    pub(super) fn take_affected(&self) -> Vec<AffectedTurn> {
        std::mem::take(&mut *lock(&self.affected))
    }

    /// Step 1 of the batch (design §7.4): `None` when a read failed,
    /// `Some(None)` when the turn already has a terminal, otherwise the rows
    /// of its session's queued turns. The caller bounds the reads, which
    /// `store` issues on the Latch lane.
    async fn batch_reads(
        store: &StoreClient,
        session: &SessionId,
        number: TurnNumber,
        record: &mut TurnRecord,
        queued: &[TurnNumber],
    ) -> Option<Option<Vec<(TurnNumber, QueuedTurn)>>> {
        if store.terminal_facts(session, number).await.ok()?.is_some() {
            return Some(None);
        }
        journal::reconcile(store, record).await.ok()?;
        let mut rows = Vec::with_capacity(queued.len());
        for turn in queued {
            if let Some(row) = store.queued_turn(session, *turn).await.ok()? {
                rows.push((*turn, row));
            }
        }
        Some(Some(rows))
    }

    /// Resolves one affected turn (design §7.4 steps 1 to 3), each step
    /// bounded by 2 s and by `deadline`. The caller holds no lock.
    pub(super) async fn resolve_affected(
        &self,
        turn: AffectedTurn,
        deadline: Deadline,
        batches: &mut FailureBatches,
    ) {
        let AffectedTurn {
            started,
            mut record,
            mut terminal,
        } = turn;
        let session = &started.session;
        let number = started.turn;
        let slot = self.slot(session);
        let queued = slot.as_ref().map(|slot| slot.queued()).unwrap_or_default();
        let read_by = deadline
            .instant()
            .min(tokio::time::Instant::now() + BATCH_READ);
        // Design §6.2: the whole failure-resolution unit is on the Latch
        // lane, one request at a time.
        let latch = self.store.latch();
        // Step 1: the earlier outcome first; a read that fails skips the batch.
        let reads = Self::batch_reads(&latch, session, number, &mut record, &queued);
        let (turns, rows): (Vec<TurnNumber>, Vec<QueuedTurn>) =
            match tokio::time::timeout_at(read_by, reads).await {
                Ok(Some(Some(rows))) if rows.len() <= FAILURE_BATCH_CANCELLATIONS => {
                    rows.into_iter().unzip()
                }
                Ok(Some(None)) => {
                    // A terminal that persisted is kept; no batch is issued.
                    self.unresolved.resolve(session, number);
                    return;
                }
                Ok(Some(Some(_)) | None) | Err(_) => {
                    batches.skipped += 1;
                    return;
                }
            };
        if !terminal
            .failure
            .as_ref()
            .is_some_and(|failure| failure.class == FailureClass::Store)
        {
            terminal.fail(FailureClass::Store, TERMINAL_LOST);
        }
        let write_by = deadline
            .instant()
            .min(tokio::time::Instant::now() + FINALIZE_WRITE);
        let shared = std::sync::Arc::clone(&record.head);
        let affected = AffectedTurn {
            started: started.clone(),
            record,
            terminal,
        };
        let write = async {
            // A failed head read writes nothing; a corrupt one is reported
            // as corruption (design §7.1, T3-S5 round 1, decision 10).
            let head = match shared.lock(&latch, session).await {
                Ok(head) => head,
                Err(error) => return Err(WriteOutcome::of_read(&error)),
            };
            let queued = turns.iter().copied().zip(rows).collect();
            // A record that cannot be encoded writes nothing.
            let (batch, written) = build(affected, slot.as_deref(), queued, head.next())
                .ok_or(WriteOutcome::NotCommitted)?;
            let committed = latch.commit_failure_resolution(batch).await;
            match &committed {
                Ok(()) => head.committed(written),
                Err(error) if !WriteOutcome::of(error).head_unknown() => drop(head),
                Err(_) => head.lost(),
            }
            committed.map_err(|error| WriteOutcome::of(&error))
        };
        let outcome = match tokio::time::timeout_at(write_by, write).await {
            Ok(Ok(())) => {
                batches.committed += 1;
                self.unresolved.resolve(session, number);
                for turn in &turns {
                    if let Some(slot) = &slot {
                        slot.pop(*turn);
                    }
                    self.unresolved.resolve(session, *turn);
                    self.queued.fetch_sub(1, Ordering::AcqRel);
                    self.active.fetch_sub(1, Ordering::AcqRel);
                }
                return;
            }
            Ok(Err(outcome)) => outcome,
            // No reply within the bound: the head is re-read by restart.
            Err(_) => WriteOutcome::Uncertain,
        };
        batches.skipped += 1;
        self.store_failure(
            FailureSite::Batch,
            outcome,
            FailureScope::Turn(session, number),
        )
        .finish()
        .await;
    }
}

/// The batch for `affected` from `seq` on: its `turn.ended`, then one
/// cancellation per `queued` turn, and how many
/// events that is. `None` when a record cannot be encoded.
fn build(
    affected: AffectedTurn,
    slot: Option<&Slot>,
    queued: Vec<(TurnNumber, QueuedTurn)>,
    first: u64,
) -> Option<(FailureResolutionRecord, u64)> {
    let AffectedTurn {
        started,
        record,
        terminal,
    } = affected;
    let session = &started.session;
    let mut seq = first;
    let ended = ended_record(&started, record, terminal, seq).ok()?;
    let mut cancellations = Vec::with_capacity(queued.len());
    if let Some(slot) = slot {
        for (turn, row) in queued {
            seq += 1;
            let (started, record, terminal, _) =
                queued_cancellation(slot, session, turn, (&row).into(), None);
            cancellations.push(ended_record(&started, record, terminal, seq).ok()?);
        }
    }
    let batch = FailureResolutionRecord {
        terminal: ended,
        cancellations,
    };
    Some((batch, seq + 1 - first))
}
