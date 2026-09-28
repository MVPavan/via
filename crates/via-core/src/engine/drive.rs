//! Turn driving: submission, adapter execution, event commits and the terminal commit.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant, SystemTime},
};

use tokio::sync::{mpsc, watch};
use via_adapters::{
    AdapterError, FakeAcceptanceObservation, FakeObservation, FakeTerminalEvidence, Observation,
    RouteError, StopOrder, StopWatch, ToolStatus, WireCleanup,
};
use via_store::{
    AcceptanceRecord, CancelCause, QueuedTurn, StoreError, SubmissionRecord, TerminalExtras,
    TerminalRecord,
};

use super::batch::AffectedTurn;
use super::journal::{self, Durable, Head, TurnJournal, UncertainEvent, Unended, Unresolved};
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use super::queue::{Ack, Backoff, Claim, Front, Owner, QueuedOutcome, Slot};
use super::resolve::{Queueing, ReadStreak};
use super::stop::StopMode;
use super::terminal::{dispose, terminal_envelope};
use super::{
    Accepted, Engine, FailureNote, ForcedTurn, RouteClose, Started, Terminal, TurnRecord, lock,
};
use crate::api::{
    Cancel, Effective, Event, EventBody, FailureClass, RawSpan, Timestamps, Warning, rfc3339,
};
use crate::{ApiError, ConnectionId, Deadline, RawRef, SessionId, TurnNumber, TurnState};

/// Reason recorded on `session.closed` for a `daemon/stop --force` (C1 §7.1).
pub(super) const FORCE_CLOSE_REASON: &str = "daemon_stop_force";

/// A committed submission: the queued turn's facts, its frozen effective
/// values and the submission time.
pub(super) struct Submission {
    session: SessionId,
    turn: TurnNumber,
    queued: QueuedTurn,
    effective: Effective,
    submitted: SystemTime,
    clock: Instant,
}

/// One private connection per turn; turn 1 keeps the session's own name.
pub(super) fn connection_id(
    session: &SessionId,
    turn: TurnNumber,
) -> Result<ConnectionId, <ConnectionId as TryFrom<&str>>::Error> {
    let suffix = session.as_str().trim_start_matches("s_");
    let connection = match turn.get() {
        1 => format!("c_{suffix}"),
        n => format!("c_{suffix}t{n}"),
    };
    ConnectionId::try_from(connection.as_str())
}

/// Why a granted turn's submission did not commit.
pub(super) enum SubmitFailure {
    /// A read before the commit failed; nothing was written.
    Unread,
    /// The commit's outcome is unknown or the database is corrupt: Store
    /// failure latches.
    Failed(WriteOutcome),
    /// Nothing was written: the turn fails with row 2's resolution write
    /// (design §7.2).
    NotCommitted(Queueing),
    /// A frozen value of the queued row is unparseable: the turn fails with
    /// the same write, and no Store write failed (design §7.3). No
    /// queueing when Store itself could not parse the row.
    Corrupt(Option<Queueing>),
}

/// A queued turn's dispatch decision from its predecessors' durable state.
enum Decision {
    Run,
    Cancel,
    /// Not yet: an earlier turn is unresolved or its cleanup pending.
    Wait,
    /// Store could not be read: the read streak counts it (design §7.3).
    Unread,
}

/// How a terminal commits (design §7.2): retried once at the same sequence
/// (rows 7 and 9), and with the `raw_log.incomplete` the turn's first
/// failure dropped, sequenced just before `turn.ended` in the same
/// transaction (row 6).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Commit {
    pub(super) retry: bool,
    pub(super) raw_owed: bool,
}

/// How a queued turn's cancellation ended.
pub(super) enum Cancelled {
    /// Durably cancelled; the turn left the queue. Carries the envelope's
    /// `cancel` object, if the cancellation had a cause.
    Committed(Option<Cancel>),
    /// A read failed before the commit: nothing was written.
    Unread,
    /// The commit failed with this outcome; the failure hook ran.
    Failed(WriteOutcome),
    /// Store failure is latched: before the commit could run, or by a
    /// commit that is durable but was uncertain.
    Latched,
    /// A read outlived final shutdown's read cutoff: nothing was written.
    Expired,
}

impl Cancelled {
    /// What joined callers are told (design §3.1).
    pub(super) fn published(&self) -> QueuedOutcome {
        match self {
            Self::Committed(_) => QueuedOutcome::Committed,
            Self::Unread | Self::Expired => QueuedOutcome::ReadFailed,
            Self::Failed(WriteOutcome::NotCommitted) => QueuedOutcome::NotCommitted,
            Self::Failed(
                WriteOutcome::Uncertain | WriteOutcome::Corrupt | WriteOutcome::ReadCorrupt,
            )
            | Self::Latched => QueuedOutcome::Uncertain,
        }
    }
}

/// The run loop's stop-order state for its turn (design §2, §5).
struct Control<'a> {
    slot: &'a Slot,
    turn: TurnNumber,
    /// The run loop's own receiver of the turn's order.
    orders: watch::Receiver<Option<StopOrder>>,
    /// The order was observed: `cancel.requested` was attempted once.
    observed: bool,
    /// The turn's first failed write sent or upgraded its order to cause
    /// `store` (design §7.2 row 5).
    stored: bool,
    /// When the idle deadline strikes; disarmed once any order exists.
    idle_at: Option<tokio::time::Instant>,
    /// The turn's frozen idle budget.
    idle: Duration,
}

/// What the dispatcher does after one step.
pub(super) enum Step {
    /// Decide again at once.
    Next,
    /// Wait for a wake or the read-retry timer.
    Wait,
    /// The head's read sequence failed: the read streak decides the wake
    /// (design §7.3).
    Unread(TurnNumber),
}

/// How a drive's execution ended.
enum Driven {
    /// The adapter returned its outcome.
    Finished(Result<FakeTerminalEvidence, AdapterError>),
    /// A force stop closed the execution through Route.
    Forced(Forced),
}

impl Driven {
    /// Route's Store failure, if its outcome carries one, and whether a
    /// Host journal write of the turn had an uncertain outcome (design
    /// §7.1, §7.2 rows 3, 4 and 6).
    fn store_facts(&self) -> (Option<&RouteError>, bool) {
        match self {
            Self::Finished(Ok(evidence)) => (None, evidence.journal_uncertain),
            Self::Finished(Err(AdapterError::Route(route))) => {
                (Some(&route.cause), route.journal_uncertain)
            }
            Self::Finished(Err(_)) => (None, false),
            Self::Forced(forced) => (None, forced.journal_uncertain),
        }
    }
}

/// Evidence of an execution a force stop closed through Route.
struct Forced {
    requested_at: String,
    /// Route's cleanup drain could not record every vendor byte.
    raw_incomplete: bool,
    /// A vendor may have launched: Host sent ARM.
    launched: bool,
    /// Route's own Host close evidence.
    close: RouteClose,
    /// A Host journal write of the turn had an uncertain outcome (§7.1).
    journal_uncertain: bool,
}

impl Engine {
    /// The session's dispatcher (design §2), run by daemon main as one task per
    /// session. It owns the session's queued turns and decides each queue head
    /// from durable state; only events and a read-retry timer wake it. A
    /// submitted turn runs inline, so at most one turn of the session runs. A
    /// close order is handled before any other decision (design §4). It
    /// returns once nothing is queued, running or closing, or once a force
    /// stop or a Store failure settled the queue.
    pub async fn dispatcher(&self, session: SessionId) -> Result<(), ApiError> {
        let slot = self.slot(&session).ok_or(ApiError::STORE)?;
        slot.live();
        // Held until this future ends or is dropped (design §6.8 step 3).
        let _dispatching = self.dispatching(&session);
        let mut force = self.signal.force.subscribe();
        let mut backoff = Backoff::new();
        let mut streak = ReadStreak::new();
        let mut refused = false;
        loop {
            if *force.borrow() {
                self.force_exit(&slot, &session).await;
                return Ok(());
            }
            let step = match slot.front() {
                Front::Closing => match self.close_pass(&slot, &session, &mut refused).await {
                    Some(step) => step,
                    None => return Ok(()),
                },
                Front::Empty => {
                    if self.exit(&session, &slot).await {
                        return Ok(());
                    }
                    continue;
                }
                Front::Turn(turn, Claim::Waiting) => match self.decide(&session, turn).await {
                    Decision::Run => self.dispatch(&slot, &session, turn).await,
                    Decision::Cancel if slot.own(turn) => {
                        self.dispatcher_cancel(&slot, &session, turn).await
                    }
                    Decision::Cancel => Step::Next,
                    Decision::Wait => Step::Wait,
                    Decision::Unread => Step::Unread(turn),
                },
                Front::Turn(turn, Claim::Cancelling(Owner::Dispatcher)) => {
                    self.dispatcher_cancel(&slot, &session, turn).await
                }
                // Another owner holds the head: its pop or rollback wakes us.
                Front::Turn(_, Claim::Cancelling(Owner::Request) | Claim::Claimed) => Step::Wait,
            };
            match step {
                Step::Next => {
                    backoff.reset();
                    streak.reset();
                }
                Step::Wait => Self::await_wake(&slot, &mut force, backoff.next()).await,
                Step::Unread(turn) => {
                    let now = tokio::time::Instant::now();
                    if let Some(delay) = streak.failed(turn, now, backoff.next()) {
                        Self::await_wake(&slot, &mut force, delay).await;
                    } else {
                        backoff.reset();
                        self.read_expired(&slot, &session, turn).await;
                    }
                }
            }
        }
    }

    /// The dispatcher's exit on force or the latch (design §1 close watch):
    /// a close attempt in progress gets `daemon_stopping` (force) or
    /// `store_error` (latch) and its order is cleared before `slot.stop()`,
    /// so no waiter is left and a later caller finds no attempt [r4.6, r5.8].
    pub(super) async fn force_exit(&self, slot: &Slot, session: &SessionId) {
        let reply = if self.store_failed() {
            ApiError::STORE
        } else {
            ApiError::DAEMON_STOPPING
        };
        slot.finish_close(Err(reply));
        self.force_queue(slot, session).await;
        slot.stop();
    }

    /// Performs the dispatcher-owned cancellation of `turn` with its cause
    /// (design §3.1): a P6 cancellation, a rollback's cancel or the close
    /// pass's. A failed read keeps the claim and retries on the timer.
    pub(super) async fn dispatcher_cancel(
        &self,
        slot: &Slot,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Step {
        let cause = slot.cause(turn);
        let cancelled = self
            .cancel_queued(slot, session, (turn, Owner::Dispatcher), false, cause)
            .await;
        match cancelled {
            Cancelled::Committed(_) => Step::Next,
            Cancelled::Unread => Step::Wait,
            cancelled @ (Cancelled::Failed(_) | Cancelled::Latched | Cancelled::Expired) => {
                slot.cancel_failed(turn, cancelled.published());
                Step::Next
            }
        }
    }

    /// Waits for a wake, a force stop or the read-retry timer.
    async fn await_wake(slot: &Slot, force: &mut watch::Receiver<bool>, delay: Duration) {
        tokio::select! {
            () = slot.woken() => {}
            _ = force.wait_for(|forced| *forced) => {}
            () = tokio::time::sleep(delay) => {}
        }
    }

    /// With an empty queue: exits under admission, then `sessions` and the
    /// slot, and retires the slot when no writer lease is out (design §2).
    /// False when a turn was enqueued meanwhile.
    async fn exit(&self, session: &SessionId, slot: &Arc<Slot>) -> bool {
        let admission = self.admission.lock().await;
        self.exit_held(session, slot, &admission)
    }

    /// [`Self::exit`] under the caller's `admission`.
    pub(super) fn exit_held(
        &self,
        session: &SessionId,
        slot: &Arc<Slot>,
        _admission: &super::Admission<'_>,
    ) -> bool {
        let mut sessions = lock(&self.sessions);
        if !slot.exit() {
            return false;
        }
        if slot.unleased()
            && sessions
                .get(session)
                .is_some_and(|mapped| Arc::ptr_eq(mapped, slot))
        {
            sessions.remove(session);
        }
        true
    }

    /// The dispatch grant (design §4): refused once a force stop is accepted
    /// or Store failure latched. `request_stop` accepts force under the same
    /// mutex, so a turn still queued when force is accepted is never submitted.
    pub(super) fn grant(&self) -> bool {
        *lock(&self.signal.stop) != Some(StopMode::Force) && !self.store_failed()
    }

    /// Claims, grants, submits and runs the queue head (design §3.1). It
    /// keeps its queued count until Store confirms the submission. A failed
    /// submission is [`Engine::submit_failure`]'s (design §7.2, §7.3).
    /// Every other path that does not submit rolls the claim back.
    async fn dispatch(&self, slot: &Slot, session: &SessionId, turn: TurnNumber) -> Step {
        // Design §11: a connection slot before the grant. It is dropped at
        // once if nothing launches; at launch Host takes it for the group's
        // life. Force, the latch or a change of the head gives up the wait:
        // the queued path, never submitted.
        let Some(connection) = self.reserve_connection(slot, turn).await else {
            return Step::Next;
        };
        if !slot.claim(turn) {
            return Step::Next;
        }
        #[cfg(test)]
        if self.faults.hold_before_grant.swap(false, Ordering::AcqRel) {
            self.faults.grant_paused.notify_one();
            self.faults.grant_release.notified().await;
        }
        // The queue head is claimed and not yet granted: a crash here leaves
        // it durably `queued` for the restart handoff (design §10).
        #[cfg(feature = "test-failpoints")]
        if via_store::failpoint::hit_async("core.dispatch.before_grant")
            .await
            .is_err()
        {
            slot.rollback(turn);
            return Step::Wait;
        }
        // Design §3.1 [r1.1]: a close order found by the claim step releases
        // the permit and leaves the turn to the close pass.
        if slot.closing() || !self.grant() {
            slot.rollback(turn);
            return Step::Next;
        }
        #[cfg(test)]
        if self.faults.hold_after_grant.load(Ordering::Acquire) {
            self.faults.granted.notify_one();
            self.faults.release.notified().await;
        }
        let submission = match self.submit(slot, session, turn).await {
            Ok(submission) => submission,
            Err(failure) => {
                return self
                    .submit_failure(slot, (session, turn), failure, connection)
                    .await;
            }
        };
        self.queued.fetch_sub(1, Ordering::AcqRel);
        self.run(slot, submission, connection).await;
        self.active.fetch_sub(1, Ordering::AcqRel);
        Step::Next
    }

    /// Waits for a connection slot, FIFO daemon-wide (design §3.1 capacity
    /// wait). The acquire future stays pinned across slot wakes, so the turn
    /// keeps its place; each wake re-checks that `turn` is still the
    /// `Waiting` head with no close order. `None` once force is accepted or
    /// Store failure is pending (the force signal carries both), or once the
    /// head changed: the permit, if any, is dropped and the dispatcher
    /// decides again.
    async fn reserve_connection(
        &self,
        slot: &Slot,
        turn: TurnNumber,
    ) -> Option<tokio::sync::OwnedSemaphorePermit> {
        if let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() {
            return Some(permit);
        }
        let mut force = self.signal.force.subscribe();
        let acquire = Arc::clone(&self.slots).acquire_owned();
        tokio::pin!(acquire);
        // One poll registers the waiter in the semaphore's FIFO queue.
        tokio::select! {
            biased;
            permit = &mut acquire => return permit.ok().filter(|_| slot.waiting_head(turn)),
            () = std::future::ready(()) => {}
        }
        // The reservation is pending and registered (design §10).
        #[cfg(feature = "test-failpoints")]
        if via_store::failpoint::hit_async("core.dispatch.awaiting_slot")
            .await
            .is_err()
        {
            return None;
        }
        loop {
            tokio::select! {
                biased;
                _ = force.wait_for(|forced| *forced) => return None,
                permit = &mut acquire => {
                    return permit.ok().filter(|_| slot.waiting_head(turn));
                }
                () = slot.woken() => {
                    if !slot.waiting_head(turn) {
                        return None;
                    }
                }
            }
        }
    }

    /// Under force (design §2.3): commits every queued turn `queued →
    /// cancelled` without submission. `session.closed` rides on the last one
    /// only when every other turn of the session is durably settled, decided
    /// under `admission`. A failed read retries with backoff until final
    /// shutdown's read budget ends. After a Store failure nothing is written:
    /// each queued turn stays `queued` and unresolved, reading `store_error`.
    /// A cancellation another request owns is waited for, on the slot wake of
    /// its pop or rollback, or until the read cutoff (design §3.1).
    async fn force_queue(&self, slot: &Slot, session: &SessionId) {
        let turns = slot.queued();
        let last = turns.last().copied();
        for turn in turns {
            if !self.own_for_force(slot, session, turn).await {
                continue;
            }
            let mut backoff = Backoff::new();
            loop {
                if self.store_failed() {
                    self.unresolved.fail(session, turn, TurnState::Queued);
                    break;
                }
                // Force cancellations keep `cancel: null` (design §3.2).
                let cancelled = self
                    .cancel_queued(
                        slot,
                        session,
                        (turn, Owner::Dispatcher),
                        Some(turn) == last,
                        None,
                    )
                    .await;
                match cancelled {
                    Cancelled::Committed(_) => break,
                    Cancelled::Failed(_) | Cancelled::Latched => {
                        slot.cancel_failed(turn, cancelled.published());
                        break;
                    }
                    Cancelled::Expired => {
                        // No further reads for this turn: it stays unresolved.
                        slot.cancel_failed(turn, cancelled.published());
                        self.unresolved.fail(session, turn, TurnState::Queued);
                        break;
                    }
                    Cancelled::Unread => {
                        if !self.retry_read(&mut backoff).await {
                            slot.cancel_failed(turn, QueuedOutcome::ReadFailed);
                            self.unresolved.fail(session, turn, TurnState::Queued);
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Takes `turn` for force's cancellation; false when it left the queue,
    /// or when a request-owned cancellation of it outlived the read cutoff.
    async fn own_for_force(&self, slot: &Slot, session: &SessionId, turn: TurnNumber) -> bool {
        loop {
            if slot.own(turn) {
                return true;
            }
            if !slot.queued().contains(&turn) {
                return false;
            }
            tokio::select! {
                () = slot.woken() => {}
                () = self.read_cutoff() => {
                    self.unresolved.fail(session, turn, TurnState::Queued);
                    return false;
                }
            }
        }
    }

    /// Waits out one read-retry delay under force; false once it would run
    /// into the part of final shutdown's deadline kept for Host cleanup and
    /// forced terminals.
    async fn retry_read(&self, backoff: &mut Backoff) -> bool {
        let delay = backoff.next();
        if self
            .read_retries_until()
            .is_some_and(|by| tokio::time::Instant::now() + delay >= by)
        {
            return false;
        }
        tokio::time::sleep(delay).await;
        true
    }

    /// C1 P6/§7.3 from durable state. While any earlier turn is unresolved
    /// (queued, including an orphan awaiting reconciliation, or running, or
    /// with no durable terminal) the turn waits. A read Store cannot answer
    /// is `Unread`, for the read streak (design §7.3); Store's read reply
    /// already latched on SQLite corruption (§7.1). Otherwise the latest submitted earlier turn decides: cleanup
    /// `pending` waits (C1 §7.3 dispatches only after cleanup settles), durably
    /// `unknown` cancels (P6), anything else runs. Turns cancelled while
    /// queued never ran and are passed over.
    async fn decide(&self, session: &SessionId, turn: TurnNumber) -> Decision {
        let Ok(predecessors) = self.predecessors(session, turn).await else {
            return Decision::Unread;
        };
        if predecessors.unresolved {
            return Decision::Wait;
        }
        match predecessors.last_submitted {
            // C1 §7.3: dispatch needs settled cleanup; pending cleanup waits.
            Some(envelope) if envelope["cancel"]["cleanup"] == "pending" => Decision::Wait,
            // P6: behind an `unknown` predecessor the queue is cancelled.
            Some(envelope) if envelope["state"] == "unknown" => Decision::Cancel,
            _ => Decision::Run,
        }
    }

    /// The Store read behind a dispatch decision; the test fault backend can fail it.
    async fn predecessors(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<via_store::Predecessors, via_store::StoreError> {
        #[cfg(test)]
        {
            self.faults.reads.fetch_add(1, Ordering::AcqRel);
            if self
                .faults
                .predecessors_unreadable
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(via_store::StoreError::WriterLost);
            }
        }
        self.store.predecessors(session, turn).await
    }

    /// Executes a submitted turn to its terminal, or hands it to final shutdown
    /// after a force stop. The turn's stop channels move from its claim into
    /// this loop, which observes its order, keeps its idle deadline and marks
    /// it `settling` once `execute` returned (design §2, §5).
    async fn run(
        &self,
        slot: &Slot,
        submission: Submission,
        capacity: tokio::sync::OwnedSemaphorePermit,
    ) {
        let Submission {
            session,
            turn,
            queued,
            effective,
            submitted,
            clock,
        } = submission;
        let started = Started {
            session: session.clone(),
            turn,
            queued_at: queued.queued_at,
            first_seq: queued.queued_seq,
            submitted: Some((rfc3339(submitted), clock)),
        };
        // Design §2 [r1.11]: both deadlines run from the submission clock.
        let origin = tokio::time::Instant::from_std(clock);
        let (deadline, deadline_at) = wall_deadline(&effective, origin, submitted);
        let (route_stop, orders) = slot.start_running(turn, deadline.instant());
        let Ok(connection) = connection_id(&session, turn) else {
            self.unresolved.fail(&session, turn, TurnState::Running);
            slot.finish_running(turn);
            return;
        };
        let mut record = new_record(slot, &session, turn);
        let mut control = Control {
            slot,
            turn,
            orders,
            observed: false,
            stored: false,
            idle_at: Some(origin + effective.idle()),
            idle: effective.idle(),
        };
        let driven = self
            .execute(
                &mut record,
                connection.clone(),
                (queued.prompt, &effective),
                (deadline, route_stop),
                Box::new(capacity),
                &mut control,
            )
            .await;
        let (cause, journal_uncertain) = driven.store_facts();
        let raw_lost = self
            .route_failed(slot, &mut record, cause, journal_uncertain)
            .await;
        // Design §2 [r1.4]: from here cancel and close send no order.
        let order = slot.settle(turn);
        if let Some(order) = &order
            && !control.observed
        {
            self.observe_order(&mut record, &mut control, order).await;
        }
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.run.settling").await;
        let outcome = match driven {
            Driven::Finished(outcome) => outcome,
            Driven::Forced(forced) => {
                self.hand_off(slot, started, record, connection, forced, order)
                    .await;
                return;
            }
        };
        // C1 §7.6 and design §2's disposition table: Core decides from the
        // evidence and the order.
        let disposed = dispose(
            record.accepted.is_some(),
            outcome,
            order.as_ref(),
            deadline.instant(),
        );
        let mut terminal = disposed.terminal;
        // Row 6: a failed raw append lost the connection's bytes.
        terminal.raw_incomplete |= raw_lost;
        let raw_owed = self
            .raw_incomplete(&mut record, &mut terminal, connection)
            .await;
        if let Some((outcome, cleanup)) = disposed.stop {
            let requested_at = if let Some(order) = &order {
                order.requested_at.clone()
            } else {
                // The wall deadline's own request (C1 §7.6).
                self.commit_event(&mut record, EventBody::CancelRequested {}, None)
                    .await;
                deadline_at
            };
            terminal.cancel = Some(
                self.settle(&mut record, requested_at, outcome, cleanup)
                    .await,
            );
        }
        if record.first_failure.is_some() {
            // Acceptance or an observation could not be recorded after dispatch.
            terminal.fail(FailureClass::Store, "a turn event could not be recorded");
        }
        let cause = disposed
            .cancel_cause
            .filter(|_| terminal.state == "cancelled");
        // A terminal that did not commit reads `store_error` and latches.
        let _ = self
            .finish_with(started, (record, terminal), cause, raw_owed)
            .await;
        slot.finish_running(turn);
    }

    /// Commits the terminal's `raw_log.incomplete` when the raw log lost
    /// bytes, with the warning. After the turn's first failure the event is
    /// dropped and owed to its resolution write (design §7.2 row 6):
    /// returns whether it is owed.
    async fn raw_incomplete(
        &self,
        record: &mut TurnRecord,
        terminal: &mut Terminal,
        connection: ConnectionId,
    ) -> bool {
        if !terminal.raw_incomplete {
            return false;
        }
        let body = EventBody::RawLogIncomplete {
            connection_id: connection,
        };
        self.commit_event(record, body, None).await;
        terminal.warnings.push(Warning::RAW_LOG_INCOMPLETE);
        record.first_failure.is_some()
    }

    /// Hands a forced turn to final shutdown, which commits its terminal once
    /// Host has evidence (design §2 rule 4): the order's cause and
    /// `requested_at` travel with it.
    async fn hand_off(
        &self,
        slot: &Slot,
        started: Started,
        mut record: TurnRecord,
        connection: ConnectionId,
        forced: Forced,
        order: Option<StopOrder>,
    ) {
        let turn = started.turn;
        let mut raw_owed = false;
        if forced.raw_incomplete {
            let body = EventBody::RawLogIncomplete {
                connection_id: connection,
            };
            self.commit_event(&mut record, body, None).await;
            raw_owed = record.first_failure.is_some();
        }
        // One `cancel.requested` per turn [r1.12]: an order's stays.
        let requested_at = if let Some(order) = &order {
            order.requested_at.clone()
        } else {
            self.commit_event(&mut record, EventBody::CancelRequested {}, None)
                .await;
            forced.requested_at
        };
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.run.before_handoff").await;
        lock(&self.forced).push(ForcedTurn {
            started,
            record,
            requested_at,
            raw_incomplete: forced.raw_incomplete,
            raw_owed,
            launched: forced.launched,
            close: forced.close,
            cause: order.map(|order| order.cause),
        });
        slot.finish_running(turn);
    }

    /// Commits `cancel.requested` for the turn's order once, at the order's
    /// `requested_at`, and publishes the acknowledgement (design §2
    /// durability). The idle timer is disarmed.
    async fn observe_order(
        &self,
        record: &mut TurnRecord,
        control: &mut Control<'_>,
        order: &StopOrder,
    ) {
        control.observed = true;
        control.idle_at = None;
        let failed = record.first_failure.is_some();
        journal::commit_event_at(
            &self.store,
            record,
            EventBody::CancelRequested {},
            None,
            &order.requested_at,
        )
        .await;
        self.report_first_failure(record, failed).await;
        let ack = if record.first_failure.is_some() {
            Ack::Failed
        } else {
            Ack::Requested(order.requested_at.clone())
        };
        control.slot.acknowledge(control.turn, ack);
    }

    /// `cancel_queued`'s reads: the turn's queueing, then the settled
    /// session head. A failed read carries its Store error, if it has one.
    async fn cancel_reads(
        &self,
        slot: &Slot,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Queueing, Option<StoreError>> {
        #[cfg(test)]
        self.hold(&self.faults.hold_cancel_read).await;
        #[cfg(feature = "test-failpoints")]
        if via_store::failpoint::hit_async("core.force.cancel_read")
            .await
            .is_err()
        {
            return Err(None);
        }
        #[cfg(test)]
        if self
            .faults
            .cancel_read_fails
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(None);
        }
        let queued = match self.store.queued_turn(session, turn).await {
            Ok(Some(queued)) => Queueing::from(&queued),
            // Design §7.3 [s4.8]: a row Store cannot parse is cancelled
            // from its committed `turn.queued`, never submitted.
            Err(StoreError::CorruptEvidence) => {
                self.queueing(session, turn).await.map_err(|_| None)?
            }
            Ok(None) => return Err(None),
            Err(error) => return Err(Some(error)),
        };
        // Settle an unknown head now, so the commit below reads nothing.
        slot.head.lock(&self.store, session).await.map_err(Some)?;
        Ok(queued)
    }

    /// Commits a never-submitted turn `queued → cancelled` (C1 §7.2), behind
    /// an `unknown` predecessor, under force, or for a caller `cancel` or a
    /// `close` (`cause`, with its `requested_at`); no vendor I/O happened.
    /// `owner` holds the turn's `Cancelling` claim. Once committed the turn
    /// leaves the queue. A failed or uncertain commit reaches the failure
    /// hook; a failed read before it writes nothing. A request's commit that
    /// is not committed is scoped to the request (design §7.2 row 8): the
    /// turn stays unresolved only as a receipted turn. With `closing` (the
    /// last queued turn under force), `admission` is held from the latch and
    /// close check through the commit, and `session.closed` rides on it when
    /// no other turn of the session is unresolved.
    pub(super) async fn cancel_queued(
        &self,
        slot: &Slot,
        session: &SessionId,
        (turn, owner): (TurnNumber, Owner),
        closing: bool,
        cause: Option<(CancelCause, String)>,
    ) -> Cancelled {
        // Both reads run under final shutdown's read cutoff (design §2.3).
        let reads = self.cancel_reads(slot, session, turn);
        let queued = tokio::select! {
            biased;
            queued = reads => queued,
            () = self.read_cutoff() => return Cancelled::Expired,
        };
        let Ok(queued) = queued else {
            if owner == Owner::Dispatcher {
                // Design §7.3 [r1.13]: joined callers get `store_error`.
                slot.read_failed(turn);
            }
            return Cancelled::Unread;
        };
        let (started, record, terminal, extras) =
            queued_cancellation(slot, session, turn, queued, cause);
        let cancel = terminal.cancel.clone();
        #[cfg(test)]
        if closing {
            self.hold(&self.faults.hold_before_close).await;
        }
        let admission = if closing {
            Some(self.admission.lock().await)
        } else {
            None
        };
        // Design §3.2: once `failure_pending` is observed no new close-bearing
        // commit starts. One that passed this check may complete; Store's
        // same-transaction refusal is then the closure proof.
        if admission.is_some() && self.store_failed() {
            // Failed while this cancellation read: nothing more is written.
            self.unresolved.fail(session, turn, TurnState::Queued);
            return Cancelled::Latched;
        }
        let close = closing && !self.unresolved.others(session, turn);
        #[cfg(test)]
        if closing {
            self.hold(&self.faults.hold_after_close_check).await;
        }
        // Design §7.2 row 9: the dispatcher keeps its claim and retries once
        // at the same sequence; a request rolls back instead (row 8).
        let retry = owner == Owner::Dispatcher;
        let committed = self
            .commit_cancellation(&started, (record, terminal), (close, extras), retry)
            .await;
        let durable = match committed {
            Ok(durable) => durable,
            Err(unended) => {
                let outcome = unended.outcome;
                let site = match owner {
                    Owner::Request => FailureSite::RequestCancel,
                    // The retry failed, or the first write was uncertain.
                    Owner::Dispatcher => FailureSite::Resolution,
                };
                let latching = self.store_failure(site, outcome, FailureScope::Turn(session, turn));
                if latching.latches() {
                    self.unresolved.fail(session, turn, TurnState::Queued);
                }
                latching.finish_with(admission.as_ref()).await;
                return Cancelled::Failed(outcome);
            }
        };
        slot.pop(turn);
        self.unresolved.resolve(session, turn);
        self.queued.fetch_sub(1, Ordering::AcqRel);
        self.active.fetch_sub(1, Ordering::AcqRel);
        if durable.uncertain {
            // The terminal is durable, but the commit itself was uncertain:
            // a Store failure, so the restart handoff fails startup (§10).
            self.store_failure(
                FailureSite::QueuedCancel,
                WriteOutcome::Uncertain,
                FailureScope::Turn(session, turn),
            )
            .finish_with(admission.as_ref())
            .await;
            return Cancelled::Latched;
        }
        if durable.retried {
            // The retry committed: the first attempt's failure is scoped to
            // the turn, which stays `cancelled` [r3.13].
            self.store_failure(
                FailureSite::QueuedCancel,
                WriteOutcome::NotCommitted,
                FailureScope::Turn(session, turn),
            )
            .finish_with(admission.as_ref())
            .await;
        }
        Cancelled::Committed(cancel)
    }

    /// Commits a queued turn's cancellation, retried once with `retry`
    /// ([`Self::commit_turn_ended_with`]). The test fault backend fails an
    /// attempt before it writes.
    async fn commit_cancellation(
        &self,
        started: &Started,
        (record, terminal): (TurnRecord, Terminal),
        (close, extras): (bool, TerminalExtras),
        retry: bool,
    ) -> Result<Durable, Unended> {
        let faulted = self.cancel_fault();
        if faulted && (!retry || self.cancel_fault()) {
            return Err(ApiError::STORE.into());
        }
        let mode = Commit {
            retry: retry && !faulted,
            raw_owed: false,
        };
        Self::commit_turn_ended_with(&self.store, started, record, terminal, close, extras, mode)
            .await
            .map(|durable| Durable {
                retried: durable.retried || faulted,
                ..durable
            })
    }

    /// Whether the test fault backend fails this `queued → cancelled` commit.
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "the fault backend exists only in tests")
    )]
    fn cancel_fault(&self) -> bool {
        #[cfg(test)]
        return self
            .faults
            .cancel_fails
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        #[cfg(not(test))]
        false
    }

    /// Commits the turn's terminal; one that cannot be made durable is recorded so
    /// that reads report `store_error` instead of a running turn. With
    /// `close_session`, `session.closed` commits in the same transaction, and
    /// the caller holds `admission` (`held`). A failed commit, or an uncertain
    /// one whose read-back found the terminal, latches Store failure (runtime
    /// §7); the committed result stays readable.
    pub(super) async fn finish(
        &self,
        started: &Started,
        record: TurnRecord,
        terminal: Terminal,
        close_session: bool,
        held: Option<&super::Admission<'_>>,
    ) -> Result<(), ApiError> {
        let finished = Self::finish_turn_with(
            &self.store,
            &self.unresolved,
            started,
            (record, terminal),
            close_session,
            TerminalExtras::default(),
            Commit::default(),
        )
        .await;
        // Design §7.2 row 15: a forced terminal is final shutdown's single
        // best-effort write; it is never retried.
        let sites = (FailureSite::ForcedTerminal, FailureSite::ForcedTerminal);
        self.finished(started, &finished, held, sites).await;
        finished.map(drop).map_err(|unended| unended.error)
    }

    /// `finish` for a running turn's own terminal, with the `cancel_cause`
    /// of a cancellation a `cancel` or `close` made (design §4). A natural
    /// terminal is retried once at the same sequence (design §7.2 row 7);
    /// after the turn's first failure the terminal is its one resolution
    /// write and is not retried. Either way the write that ends the attempt
    /// latches when it fails (escalation).
    ///
    /// A terminal that did not become durable leaves the turn affected: it
    /// is kept, with the `raw_log.incomplete` it owes, for final shutdown's
    /// failure-resolution batch (design §7.4) before the latch is finished.
    async fn finish_with(
        &self,
        started: Started,
        (record, terminal): (TurnRecord, Terminal),
        cancel_cause: Option<CancelCause>,
        raw_owed: bool,
    ) -> Result<(), ApiError> {
        let extras = TerminalExtras {
            cancel_cause,
            raw_incomplete: None,
        };
        let mode = Commit {
            retry: record.first_failure.is_none(),
            raw_owed,
        };
        let kept = (record.clone(), terminal.clone());
        let finished = Self::finish_turn_with(
            &self.store,
            &self.unresolved,
            &started,
            (record, terminal),
            false,
            extras,
            mode,
        )
        .await;
        if finished.is_err() {
            let (record, terminal) = kept;
            self.keep_affected(AffectedTurn {
                started: started.clone(),
                record,
                terminal,
                raw_incomplete: mode.raw_owed,
            });
        }
        let sites = (FailureSite::Terminal, FailureSite::Resolution);
        self.finished(&started, &finished, None, sites).await;
        finished.map(drop).map_err(|unended| unended.error)
    }

    /// Reports a terminal commit to the failure hook: a first attempt that
    /// did not commit before its retry committed at `first`, and a failed
    /// or uncertain final write at `last` (an uncertain one, or a corrupt
    /// head read before it, latches at any site).
    async fn finished(
        &self,
        started: &Started,
        finished: &Result<Durable, Unended>,
        held: Option<&super::Admission<'_>>,
        (first, last): (FailureSite, FailureSite),
    ) {
        let failed = match finished {
            Ok(durable) if durable.uncertain => Some((first, WriteOutcome::Uncertain)),
            Ok(durable) if durable.retried => Some((first, WriteOutcome::NotCommitted)),
            Ok(_) => None,
            Err(unended) => Some((last, unended.outcome)),
        };
        if let Some((site, outcome)) = failed {
            let scope = FailureScope::Turn(&started.session, started.turn);
            self.store_failure(site, outcome, scope)
                .finish_with(held)
                .await;
        }
    }

    /// `finish` over any journal, so the Store/Core boundary is testable.
    #[cfg(test)]
    pub(super) async fn finish_turn(
        journal: &impl TurnJournal,
        unresolved: &Unresolved,
        started: &Started,
        record: TurnRecord,
        terminal: Terminal,
        close_session: bool,
    ) -> Result<Durable, Unended> {
        Self::finish_turn_with(
            journal,
            unresolved,
            started,
            (record, terminal),
            close_session,
            TerminalExtras::default(),
            Commit::default(),
        )
        .await
    }

    /// [`Self::finish_turn`] with the terminal's `extras`, committed as
    /// `mode` says ([`Self::commit_turn_ended_with`]).
    async fn finish_turn_with(
        journal: &impl TurnJournal,
        unresolved: &Unresolved,
        started: &Started,
        (record, terminal): (TurnRecord, Terminal),
        close_session: bool,
        extras: TerminalExtras,
        mode: Commit,
    ) -> Result<Durable, Unended> {
        let committed = Self::commit_turn_ended_with(
            journal,
            started,
            record,
            terminal,
            close_session,
            extras,
            mode,
        )
        .await;
        match committed {
            Ok(_) => unresolved.resolve(&started.session, started.turn),
            Err(_) => unresolved.fail(&started.session, started.turn, TurnState::Running),
        }
        committed
    }

    /// [`Self::commit_turn_ended_with`] with no extras.
    #[cfg(test)]
    pub(super) async fn commit_turn_ended(
        journal: &impl TurnJournal,
        started: &Started,
        record: TurnRecord,
        terminal: Terminal,
        close_session: bool,
    ) -> Result<Durable, Unended> {
        Self::commit_turn_ended_with(
            journal,
            started,
            record,
            terminal,
            close_session,
            TerminalExtras::default(),
            Commit::default(),
        )
        .await
    }

    /// Commits `turn.ended` at the sequence after every event `record` committed,
    /// with the terminal envelope whose raw spans bound every committed reference.
    /// An uncertain event commit is settled against the durable head first. With
    /// `close_session`, `session.closed` follows in the same transaction;
    /// otherwise `extras` commit with the terminal, and an owed
    /// `raw_log.incomplete` just before it (`mode.raw_owed`, design §7.2
    /// row 6). With `mode.retry`, a commit known not committed is retried
    /// once at the same sequence, holding the session head across both
    /// attempts (design §7.2 rows 7 and 9 [r3.7]). The head advances only on
    /// a confirmed commit: a write that did not commit leaves it as it was,
    /// and an uncertain one leaves it unknown.
    pub(super) async fn commit_turn_ended_with(
        journal: &impl TurnJournal,
        started: &Started,
        mut record: TurnRecord,
        terminal: Terminal,
        close_session: bool,
        mut extras: TerminalExtras,
        mode: Commit,
    ) -> Result<Durable, Unended> {
        // A failed read writes nothing; a corrupt one latches (design §7.1),
        // and the hook sees it as this write's outcome.
        journal::reconcile(journal, &mut record)
            .await
            .map_err(|error| Unended {
                error: ApiError::STORE,
                outcome: WriteOutcome::of_read(&error),
            })?;
        let shared = Arc::clone(&record.head);
        let head = shared
            .lock(journal, &started.session)
            .await
            .map_err(|error| Unended {
                error: ApiError::STORE,
                outcome: WriteOutcome::of_read(&error),
            })?;
        let first = head.next();
        let mut seq = first;
        if mode.raw_owed {
            let connection_id =
                connection_id(&started.session, started.turn).map_err(|_| ApiError::STORE)?;
            let incomplete = Event {
                seq,
                session_id: &started.session,
                turn: Some(started.turn.get()),
                late: false,
                at: &rfc3339(SystemTime::now()),
                raw_ref: None,
                body: EventBody::RawLogIncomplete { connection_id },
            }
            .to_value()?;
            extras.raw_incomplete = Some(incomplete);
            seq += 1;
        }
        let ended = ended_record(started, record, terminal, seq)?;
        let closed = if close_session {
            let closed = Event {
                seq: seq + 1,
                session_id: &started.session,
                turn: None,
                late: false,
                at: &rfc3339(SystemTime::now()),
                raw_ref: None,
                body: EventBody::SessionClosed {
                    reason: FORCE_CLOSE_REASON,
                },
            }
            .to_value()?;
            Some(closed)
        } else {
            None
        };
        let committed =
            journal::commit_terminal_with(journal, ended, closed, extras, mode.retry).await;
        match &committed {
            Ok(Durable {
                uncertain: false,
                closed,
                ..
            }) => head.committed(seq + 1 - first + u64::from(*closed)),
            // Nothing was written: the sequence stays the session's next.
            Err(error) if journal::outcome_of(error) == WriteOutcome::NotCommitted => drop(head),
            // Uncertain: re-read the head before the session's next event.
            Ok(_) | Err(_) => head.lost(),
        }
        committed.map_err(Unended::from)
    }

    /// Drives the adapter under the turn deadline, committing each observation it
    /// reports in decode order before the adapter outcome is returned. A force stop
    /// reaches Route, which force-closes the group and drains its output first:
    /// frames it read still commit, and the raw log is complete or reported not.
    ///
    /// The turn's stop order reaches Route through `stop`; this loop observes
    /// it once (design §2), and orders the idle deadline itself when no
    /// meaningful progress came within the idle budget (design §5).
    async fn execute(
        &self,
        record: &mut TurnRecord,
        connection: ConnectionId,
        (prompt, effective): (String, &Effective),
        (deadline, stop): (Deadline, StopWatch),
        capacity: via_adapters::CapacityToken,
        control: &mut Control<'_>,
    ) -> Driven {
        // Full: Adapter waits under the turn deadline; this loop keeps draining until
        // the adapter finishes.
        let (observed_tx, mut observed_rx) = mpsc::channel::<FakeObservation>(64);
        let mut execute = Box::pin(self.adapter.execute(
            record.session.clone(),
            record.turn,
            connection,
            prompt,
            observed_tx,
            deadline,
            self.signal.force.subscribe(),
            stop,
            capacity,
        ));
        // No branch is cancelled mid-commit: an observation arm runs to completion
        // before the next poll, and the adapter's own sends wait for capacity.
        loop {
            let idle_at = control.idle_at;
            tokio::select! {
                Some(observation) = observed_rx.recv() => {
                    if let Some(idle_at) = control.idle_at.as_mut()
                        && progress(&observation)
                    {
                        *idle_at = tokio::time::Instant::now() + control.idle;
                    }
                    self.observe(record, effective, observation).await;
                    stop_for_store(record, control);
                }
                changed = control.orders.changed(), if !control.observed => {
                    let order = changed
                        .ok()
                        .and_then(|()| control.orders.borrow_and_update().clone());
                    if let Some(order) = order {
                        self.observe_order(record, control, &order).await;
                        stop_for_store(record, control);
                    }
                }
                () = sleep_until_some(idle_at), if idle_at.is_some() => {
                    // Design §5: no meaningful progress within the budget.
                    control.idle_at = None;
                    // The timer fired; its order is not issued yet (design §10).
                    #[cfg(feature = "test-failpoints")]
                    let _ = via_store::failpoint::hit_async("core.run.idle_expired").await;
                    control.slot.idle_order(control.turn, tokio::time::Instant::now());
                }
                result = &mut execute => {
                    self.drain(record, effective, control, &mut observed_rx).await;
                    return match result {
                        Err(AdapterError::Route(route))
                            if matches!(route.cause, RouteError::ForceStopped { .. }) =>
                        {
                            Driven::Forced(Forced {
                                requested_at: self
                                    .signal
                                    .force_requested_at
                                    .get()
                                    .cloned()
                                    .unwrap_or_else(|| rfc3339(SystemTime::now())),
                                raw_incomplete: route.raw_incomplete,
                                launched: route.launched,
                                close: RouteClose {
                                    forced: route.forced,
                                    quiescent: route.cleanup == Some(WireCleanup::Quiescent),
                                },
                                journal_uncertain: route.journal_uncertain,
                            })
                        }
                        result => Driven::Finished(result),
                    };
                }
            }
        }
    }

    /// Commits the observations still queued when `execute` completed, in
    /// decode order. A failed write sends or upgrades the turn's order to
    /// cause `store`, as in the observation branch (design §7.2 row 5).
    async fn drain(
        &self,
        record: &mut TurnRecord,
        effective: &Effective,
        control: &mut Control<'_>,
        observed: &mut mpsc::Receiver<FakeObservation>,
    ) {
        while let Ok(observation) = observed.try_recv() {
            self.observe(record, effective, observation).await;
            stop_for_store(record, control);
        }
    }

    /// Test builds: drains `queued` for the running `turn` of `slot` as
    /// `execute`'s completion does, with the run loop's own order receiver.
    #[cfg(test)]
    pub(super) async fn drain_queued(
        &self,
        slot: &Slot,
        record: &mut TurnRecord,
        effective: &Effective,
        orders: watch::Receiver<Option<StopOrder>>,
        queued: Vec<FakeObservation>,
    ) {
        let (sender, mut observed) = mpsc::channel(queued.len().max(1));
        for observation in queued {
            let _ = sender.try_send(observation);
        }
        drop(sender);
        let mut control = Control {
            slot,
            turn: record.turn,
            orders,
            observed: false,
            stored: false,
            idle_at: None,
            idle: Duration::ZERO,
        };
        self.drain(record, effective, &mut control, &mut observed)
            .await;
    }

    /// Commits one adapter observation at the next sequence, in decode order.
    /// After the first Store failure the rest are dropped and the turn fails `store`.
    async fn observe(
        &self,
        record: &mut TurnRecord,
        effective: &Effective,
        observation: FakeObservation,
    ) {
        match observation {
            FakeObservation::Accepted(observation) => {
                // Route admits one acceptance; a repeat would be deduplicated anyway.
                if record.first_failure.is_some() || record.accepted.is_some() {
                    return;
                }
                let shared = Arc::clone(&record.head);
                let head = match shared.lock(&self.store, &record.session).await {
                    Ok(head) => head,
                    Err(error) => {
                        // The head's read failed: nothing was written;
                        // corruption latches (design §7.1).
                        self.event_failed(record, WriteOutcome::of_read(&error))
                            .await;
                        return;
                    }
                };
                let seq = head.next();
                match self
                    .accept(&record.session, record.turn, seq, effective, observation)
                    .await
                {
                    Ok(accepted) => {
                        head.committed(1);
                        RawSpan::include(&mut record.spans, &accepted.raw_ref);
                        record.accepted = Some(accepted);
                    }
                    Err(uncertain) => {
                        let outcome = if let Some(accepted) = uncertain {
                            head.lost();
                            record.uncertain = Some(UncertainEvent {
                                seq,
                                raw_ref: Some(accepted.raw_ref.clone()),
                                accepted: Some(accepted),
                            });
                            WriteOutcome::Uncertain
                        } else {
                            drop(head);
                            WriteOutcome::NotCommitted
                        };
                        // The head lock is released before the latch takes admission.
                        self.event_failed(record, outcome).await;
                    }
                }
            }
            FakeObservation::Data {
                observation,
                raw_ref,
            } => {
                self.commit_event(record, event_body(observation), Some(raw_ref))
                    .await;
            }
        }
    }

    /// Commits one non-lifecycle event of the running turn at the next sequence.
    pub(super) async fn commit_event(
        &self,
        record: &mut TurnRecord,
        body: EventBody,
        raw_ref: Option<RawRef>,
    ) {
        let failed = record.first_failure.is_some();
        journal::commit_event(&self.store, record, body, raw_ref).await;
        self.report_first_failure(record, failed).await;
    }

    /// Reports a turn's first failed write, if `record` gained one since
    /// `failed` was read, through the failure hook.
    pub(super) async fn report_first_failure(&self, record: &TurnRecord, failed: bool) {
        if let (false, Some(note)) = (failed, record.first_failure) {
            let scope = FailureScope::Turn(&record.session, record.turn);
            self.store_failure(note.site, note.outcome, scope)
                .finish()
                .await;
        }
    }

    /// Records a failed acceptance commit as the turn's first failure and
    /// reports it.
    async fn event_failed(&self, record: &mut TurnRecord, outcome: WriteOutcome) {
        record.first_failure = Some(FailureNote {
            site: FailureSite::Event,
            outcome,
        });
        self.report_first_failure(record, false).await;
    }

    /// Commits submission intent with `turn.submitted` before any agent I/O;
    /// the test fault backend can fail its read or lose its reply.
    async fn submit(
        &self,
        slot: &Slot,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Submission, SubmitFailure> {
        #[cfg(test)]
        if self.faults.submission_unread.swap(false, Ordering::AcqRel) {
            // The queued-turn read failed: nothing was written.
            return Err(SubmitFailure::Unread);
        }
        let submitted = Self::commit_submission(&self.store, session, turn, &slot.head).await;
        #[cfg(test)]
        if submitted.is_ok()
            && self
                .faults
                .submission_reply_lost
                .swap(false, Ordering::AcqRel)
        {
            if let Ok(head) = slot.head.lock(&self.store, session).await {
                head.lost();
            }
            return Err(SubmitFailure::Failed(WriteOutcome::Uncertain));
        }
        // Submission intent is durable and no agent I/O has happened yet. An
        // injected failure here is a failed write after the commit: the turn
        // leaves the queue at `running`, unresolved, and no vendor I/O follows.
        #[cfg(feature = "test-failpoints")]
        if submitted.is_ok()
            && via_store::failpoint::hit_async("core.intent.after_commit")
                .await
                .is_err()
        {
            self.unresolved.fail(session, turn, TurnState::Running);
            slot.start_running(turn, tokio::time::Instant::now());
            slot.finish_running(turn);
            self.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(SubmitFailure::Failed(WriteOutcome::Uncertain));
        }
        submitted
    }

    /// `submit`'s Store work: reads the queued turn and commits `turn.submitted`
    /// at the session's next sequence. A failed read wrote nothing; a failed
    /// commit is `Failed`, and one that may have committed leaves the head
    /// unknown.
    pub(super) async fn commit_submission(
        journal: &impl TurnJournal,
        session: &SessionId,
        turn: TurnNumber,
        head: &Head,
    ) -> Result<Submission, SubmitFailure> {
        let queued = match journal.queued_turn(session, turn).await {
            Ok(Some(queued)) => queued,
            // Store could not parse the row's frozen values (design §7.3).
            Err(StoreError::CorruptEvidence) => return Err(SubmitFailure::Corrupt(None)),
            Ok(None) | Err(_) => return Err(SubmitFailure::Unread),
        };
        let queueing = || Queueing::from(&queued);
        // A frozen row Core cannot read fails the turn: nothing is sent.
        let Ok(effective) = serde_json::from_value::<Effective>(queued.effective.clone()) else {
            return Err(SubmitFailure::Corrupt(Some(queueing())));
        };
        // Design §2 [r1.11]: the submission clock is taken immediately
        // before the commit; both deadlines run from it.
        let submitted = SystemTime::now();
        let clock = Instant::now();
        #[cfg(feature = "test-failpoints")]
        if via_store::failpoint::hit_async("core.submit.before_commit")
            .await
            .is_err()
        {
            return Err(SubmitFailure::Unread);
        }
        let Ok(head) = head.lock(journal, session).await else {
            return Err(SubmitFailure::Unread);
        };
        let event = Event {
            seq: head.next(),
            session_id: session,
            turn: Some(turn.get()),
            late: false,
            at: &rfc3339(submitted),
            raw_ref: None,
            body: EventBody::TurnSubmitted { attempt: 1 },
        }
        .to_value()
        .map_err(|_| SubmitFailure::NotCommitted(queueing()))?;
        let committed = journal
            .commit_submission(SubmissionRecord {
                session_id: session.clone(),
                turn,
                event,
            })
            .await;
        match committed {
            Ok(()) => head.committed(1),
            Err(error) => {
                let outcome = WriteOutcome::of(&error);
                if outcome.head_unknown() {
                    head.lost();
                    return Err(SubmitFailure::Failed(outcome));
                }
                // Nothing was written: the sequence stays the session's next.
                drop(head);
                return Err(SubmitFailure::NotCommitted(queueing()));
            }
        }
        Ok(Submission {
            session: session.clone(),
            turn,
            queued,
            effective,
            submitted,
            clock,
        })
    }

    /// Commits vendor acceptance as C2 evidence and C1 `turn.started` together.
    /// A failure carries the acceptance when Store may have committed it.
    async fn accept(
        &self,
        session: &SessionId,
        turn: TurnNumber,
        seq: u64,
        effective: &Effective,
        observation: FakeAcceptanceObservation,
    ) -> Result<Accepted, Option<Accepted>> {
        let at = rfc3339(SystemTime::now());
        let event = Event {
            seq,
            session_id: session,
            turn: Some(turn.get()),
            late: false,
            at: &at,
            raw_ref: Some(&observation.raw_ref),
            body: EventBody::TurnStarted {
                effective: effective.clone(),
            },
        }
        .to_value()
        .map_err(|_| None)?;
        let vendor_turn_id = observation.vendor_turn_id.as_str().to_owned();
        // The vendor accepted; its acceptance is not yet recorded.
        #[cfg(feature = "test-failpoints")]
        if via_store::failpoint::hit_async("core.accept.before_commit")
            .await
            .is_err()
        {
            return Err(None);
        }
        let committed = self
            .store
            .commit_acceptance(AcceptanceRecord {
                session_id: session.clone(),
                turn,
                raw_ref: observation.raw_ref.clone(),
                correlation: vendor_turn_id.clone(),
                event,
            })
            .await;
        let accepted = Accepted {
            at,
            raw_ref: observation.raw_ref,
            vendor_turn_id,
        };
        match committed {
            Ok(()) => Ok(accepted),
            Err(error) => Err(journal::may_have_committed(&error).then_some(accepted)),
        }
    }
}

/// `turn.ended` at `seq` with the terminal envelope, whose raw spans bound
/// every reference `record` committed and the terminal's own; `record` is
/// already reconciled (design §7.4's batch builds it the same way).
pub(super) fn ended_record(
    started: &Started,
    mut record: TurnRecord,
    terminal: Terminal,
    seq: u64,
) -> Result<TerminalRecord, ApiError> {
    if let Some(reference) = &terminal.raw_ref {
        RawSpan::include(&mut record.spans, reference);
    }
    let ended_at = rfc3339(SystemTime::now());
    let raw_ref = terminal.raw_ref.clone();
    let event = Event {
        seq,
        session_id: &started.session,
        turn: Some(started.turn.get()),
        late: false,
        at: &ended_at,
        raw_ref: raw_ref.as_ref(),
        body: EventBody::TurnEnded {
            state: terminal.state,
            failure: terminal.failure.clone(),
            stop_reason: terminal.stop_reason,
            cancel: terminal.cancel.clone(),
        },
    }
    .to_value()?;
    let timestamps = Timestamps {
        queued_at: started.queued_at.clone(),
        submitted_at: started.submitted.as_ref().map(|(at, _)| at.clone()),
        accepted_at: record.accepted.as_ref().map(|accepted| accepted.at.clone()),
        ended_at,
    };
    // Monotonic, so wall-clock steps cannot distort or drop the duration.
    let duration_ms = started
        .submitted
        .as_ref()
        .and_then(|(_, clock)| u64::try_from(clock.elapsed().as_millis()).ok());
    let envelope = terminal_envelope(
        &started.session,
        started.turn,
        terminal,
        record.accepted,
        record.spans,
        timestamps,
        duration_ms,
        (started.first_seq, seq),
    );
    let envelope = serde_json::to_value(&envelope).map_err(|_| ApiError::STORE)?;
    Ok(TerminalRecord {
        session_id: started.session.clone(),
        turn: started.turn,
        envelope,
        event,
        raw_ref,
    })
}

/// Design §7.2 row 5: the turn's first write that did not commit stops it
/// with cause `store`, once; later events are dropped and its terminal is
/// the resolution write. An uncertain one latches instead, and the latch's
/// force stops the turn.
fn stop_for_store(record: &TurnRecord, control: &mut Control<'_>) {
    if !control.stored
        && record
            .first_failure
            .is_some_and(|note| note.outcome == WriteOutcome::NotCommitted)
    {
        control.stored = true;
        control
            .slot
            .store_order(control.turn, tokio::time::Instant::now());
    }
}

/// The turn's absolute Core deadline from its own frozen wall budget (C1 §4)
/// and that deadline's wall time, both from the submission clock (design §2
/// [r1.11]). A budget too far off to represent never expires in practice.
fn wall_deadline(
    effective: &Effective,
    origin: tokio::time::Instant,
    submitted: SystemTime,
) -> (Deadline, String) {
    let wall = effective.wall();
    let far = || origin + Duration::from_hours(24 * 365 * 30);
    let at = submitted
        .checked_add(wall)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    (
        Deadline::at(origin.checked_add(wall).unwrap_or_else(far)),
        rfc3339(at),
    )
}

/// Sleeps until `at`; never resolves for `None`.
async fn sleep_until_some(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Meaningful progress resets the idle deadline (design §5 [r1.10]):
/// acceptance, assistant text, and tool start and end. Unknown
/// observations never do; the fake route declares no other progress.
fn progress(observation: &FakeObservation) -> bool {
    match observation {
        FakeObservation::Accepted(_) => true,
        FakeObservation::Data { observation, .. } => match observation {
            Observation::AssistantText { .. }
            | Observation::ToolStarted { .. }
            | Observation::ToolEnded { .. } => true,
            Observation::VendorOther { .. } => false,
        },
    }
}

/// A turn's record before its first event: it writes at the slot's head.
fn new_record(slot: &Slot, session: &SessionId, turn: TurnNumber) -> TurnRecord {
    TurnRecord {
        session: session.clone(),
        turn,
        head: Arc::clone(&slot.head),
        accepted: None,
        spans: Vec::new(),
        first_failure: None,
        uncertain: None,
    }
}

/// The facts, record, terminal and extras of a never-submitted turn's
/// cancellation. Design §3.2: a queued turn has no anchor intent, so a
/// caused cancellation's cleanup is `quiescent`; the envelope and
/// `turn.ended` carry the same `cancel`, and the cause is recorded.
pub(super) fn queued_cancellation(
    slot: &Slot,
    session: &SessionId,
    turn: TurnNumber,
    queued: Queueing,
    cause: Option<(CancelCause, String)>,
) -> (Started, TurnRecord, Terminal, TerminalExtras) {
    let started = Started {
        session: session.clone(),
        turn,
        queued_at: queued.queued_at,
        first_seq: queued.queued_seq,
        submitted: None,
    };
    let record = new_record(slot, session, turn);
    let terminal = Terminal {
        state: "cancelled",
        failure: None,
        stop_reason: "interrupted",
        vendor_stop_reason: None,
        final_text: String::new(),
        exit: None,
        raw_ref: None,
        raw_incomplete: false,
        warnings: Vec::new(),
        cancel: cause.as_ref().map(|(_, requested_at)| Cancel {
            outcome: "acknowledged",
            cleanup: "quiescent",
            requested_at: requested_at.clone(),
            settled_at: rfc3339(SystemTime::now()),
        }),
    };
    let extras = TerminalExtras {
        cancel_cause: cause.map(|(cause, _)| cause),
        raw_incomplete: None,
    };
    (started, record, terminal, extras)
}

/// Maps a normalized observation onto its C1 §6.1 event payload.
fn event_body(observation: Observation) -> EventBody {
    match observation {
        Observation::AssistantText { text } => EventBody::AssistantText {
            text,
            is_final: false,
        },
        Observation::ToolStarted {
            tool_id,
            name,
            input_summary,
        } => EventBody::ToolStarted {
            tool_id,
            name,
            input_summary,
        },
        Observation::ToolEnded {
            tool_id,
            status,
            output_summary,
            exit_code,
        } => EventBody::ToolEnded {
            tool_id,
            status: match status {
                ToolStatus::Completed => "completed",
                ToolStatus::Failed => "failed",
                ToolStatus::Cancelled => "cancelled",
            },
            output_summary,
            exit_code,
        },
        Observation::VendorOther {
            vendor_type,
            payload,
            truncated,
        } => EventBody::VendorOther {
            vendor_type,
            payload,
            truncated,
        },
    }
}
