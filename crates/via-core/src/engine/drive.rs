//! Turn driving: submission, adapter execution, event commits and the terminal commit.

use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant, SystemTime},
};

use tokio::sync::watch;
use via_adapters::{
    AdapterError, Admitted, Decline, Denial, DenialKind, Observation, ObservationItem, Prepared,
    RouteError, StopOrder, StopWatch, TurnActivity, TurnCx, TurnEnd, TurnEvidence, TurnSpec,
    VendorTerminal, VersionStatus, WireCleanup, observation::Acceptance,
};
use via_store::{
    AcceptanceRecord, CancelCause, Prompt, QueuedTurn, StepRow, StepsRecord, StoreError,
    SubmissionRecord, TerminalExtras, TerminalRecord,
};

use super::batch::AffectedTurn;
use super::final_text::FinalText;
use super::journal::{self, Durable, Head, TurnJournal, UncertainEvent, Unended, Unresolved};
use super::lane::{Attribution, Inbox, Lane, LaneClaim, Mapped, Retained, turn_job};
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use super::progress::Progress;
use super::queue::{Ack, Backoff, Claim, Front, Owner, QueuedOutcome, Slot};
use super::resolve::{Queueing, ReadStreak};
use super::stop::StopMode;
use super::terminal::{dispose, turn_envelope};
use super::{
    Accepted, Engine, FailureNote, ForcedTurn, RouteClose, Started, Terminal, TurnRecord, lock,
};
use crate::api::{
    AutoDeclined, Cancel, DeniedAction, Effective, Event, EventBody, FailureClass, FinalTextFile,
    STRUCTURED_OUTPUT_INLINE, StructuredOutputFile, Timestamps, Warning, rfc3339,
};
use crate::{ApiError, Deadline, SessionId, TurnNumber, TurnState};

/// Reason recorded on `session.closed` for a `daemon/stop --force` (C1 §7.1).
pub(super) const FORCE_CLOSE_REASON: &str = "daemon_stop_force";

/// The Store correlation tag of an acceptance with no vendor turn ID: the
/// acceptance token follows it. Recovery reports no vendor turn ID for
/// such an acceptance.
pub(super) const TOKEN_CORRELATION: &str = "t:";

/// The Store correlation tag of an acceptance's vendor turn ID, which
/// follows it verbatim. Every correlation is tagged, so no vendor ID reads
/// as a token (critical r1 #10: C2 reserves no prefix of its own).
pub(super) const VENDOR_CORRELATION: &str = "v:";

/// C1 P7's tool-grace window.
const TOOL_GRACE: Duration = Duration::from_secs(60);

/// A committed submission: the queued turn's facts, its loaded prompt, its
/// frozen effective values and the submission time.
pub(super) struct Submission {
    session: SessionId,
    turn: TurnNumber,
    queued: QueuedTurn,
    prompt: String,
    effective: Effective,
    submitted: SystemTime,
    clock: Instant,
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
/// (rows 7 and 9), and with the latch whose phase one a failure that may
/// have written raises before its read-back (runtime §7).
#[derive(Clone, Copy, Default)]
pub(super) struct Commit<'a> {
    pub(super) retry: bool,
    pub(super) latch: Option<&'a super::latch::Signal>,
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
    /// Core refused the vendor's evidence, an unrepresentable token count
    /// (review r1) or an acceptance naming a vendor turn the lane keeps
    /// for another (Sol r3 N6): its order was sent or upgraded to cause
    /// `protocol`.
    refused: bool,
    /// When the idle deadline strikes; disarmed once any order exists.
    idle_at: Option<tokio::time::Instant>,
    /// The turn's frozen idle budget.
    idle: Duration,
    /// The final text's pieces so far (design §6.4).
    final_text: FinalText,
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

/// A turn's settled final text, for its terminal (Task 4 design §2.3,
/// §6.4).
pub(super) struct TurnText {
    /// The inline text; `None` once it spilled.
    inline: Option<String>,
    /// The durable file, when the text spilled.
    file: Option<FinalTextFile>,
    /// A file step failed: the turn fails `store`.
    failed: bool,
}

impl TurnText {
    /// Puts the text in `terminal`; whether a file step failed.
    pub(super) fn apply(self, terminal: &mut Terminal) -> bool {
        terminal.final_text = self.inline;
        terminal.final_text_file = self.file;
        self.failed
    }
}

/// How a drive's execution ended.
enum Driven {
    /// The driver returned the turn's one result (C2 §4.1, AD4): its
    /// retained vendor terminal, if any, and its evidence or failure.
    Finished(Box<(Option<VendorTerminal>, Result<TurnEvidence, AdapterError>)>),
    /// A force stop closed the execution through Route.
    Forced(Forced),
}

impl Driven {
    /// Route's Store failure, if its outcome carries one, and whether a
    /// Host journal write of the turn had an uncertain outcome (design
    /// §7.1, §7.2 rows 3, 4 and 6).
    fn store_facts(&self) -> (Option<&RouteError>, bool) {
        match self {
            Self::Finished(finished) => match &finished.1 {
                Ok(evidence) => (None, evidence.journal_uncertain),
                Err(AdapterError::Route(route)) => (Some(&route.cause), route.journal_uncertain),
                Err(error) => (None, error.journal_uncertain()),
            },
            Self::Forced(forced) => (None, forced.journal_uncertain),
        }
    }
}

/// Evidence of an execution a force stop closed through Route.
struct Forced {
    requested_at: String,
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
            if force.borrow().is_some() {
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
    /// pass's. A failed read keeps the claim and feeds the read streak
    /// (§7.3), which retries on the timer and, at its deadline, cancels the
    /// turn from its committed `turn.queued` ([`Engine::read_expired`]).
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
            Cancelled::Unread => Step::Unread(turn),
            cancelled @ (Cancelled::Failed(_) | Cancelled::Latched | Cancelled::Expired) => {
                slot.cancel_failed(turn, cancelled.published());
                Step::Next
            }
        }
    }

    /// Waits for a wake, a force stop or the read-retry timer.
    async fn await_wake(
        slot: &Slot,
        force: &mut watch::Receiver<Option<tokio::time::Instant>>,
        delay: Duration,
    ) {
        tokio::select! {
            () = slot.woken() => {}
            _ = force.wait_for(Option::is_some) => {}
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
    async fn dispatch(&self, slot: &Arc<Slot>, session: &SessionId, turn: TurnNumber) -> Step {
        // AD16: a pinned live connection of the session's driver needs no
        // slot; a session without a usable driver needs one. Otherwise
        // design §11: a connection slot before the grant. It is dropped at
        // once if nothing launches; at launch Host takes it for the group's
        // life. Force, the latch or a change of the head gives up the wait:
        // the queued path, never submitted. The session's lane is claimed
        // before its driver is prepared, and a failed driver is retired
        // first, so its own slot is free for its successor (C2 §2, Sol r2
        // #1). The claim is given back on every path that does not run.
        let claim = self.claim_lane(session).await;
        let prepared = claim
            .as_ref()
            .map_or(Prepared::NeedsConnection, |claim| claim.driver.prepare());
        let connection = match prepared {
            Prepared::Pinned(_) => None,
            Prepared::NeedsConnection => match self.reserve_connection(slot, turn).await {
                Some(permit) => Some(permit),
                None => return Step::Next,
            },
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
        // Task 4 design §5.3, §5.4: below the free-space floor or at
        // `wal.max`, the turn fails `store` before submission; no agent I/O,
        // so the connection slot is released first.
        if let Some(message) = self.dispatch_refusal().await {
            drop(connection);
            return self.fail_at_dispatch(slot, session, turn, message).await;
        }
        #[cfg(test)]
        if self.faults.hold_after_grant.load(Ordering::Acquire) {
            self.faults.granted.notify_one();
            self.faults.release.notified().await;
        }
        // Accepted limit (Sol r4, before actor ownership): dropping this
        // future (the dispatcher aborted) from the claim through submission
        // and the lane's replacement below, before `run_on_lane` hands the
        // turn to the lane's actor, can leave a claimed queued turn, or a
        // durably running one, with no actor job. No vendor I/O has
        // happened yet; bounded shutdown reports the turn unresolved, and
        // restart recovery settles it. It is a limit before the actor owns
        // the turn, not a cancellation guarantee.
        let submission = match self.submit(slot, session, turn).await {
            Ok(submission) => submission,
            Err(failure) => {
                return self
                    .submit_failure(slot, (session, turn), failure, connection)
                    .await;
            }
        };
        // C2 §2: the session's driver, opened at its first dispatch, or
        // replaced when its health failed. A pin that went stale meanwhile
        // is the driver's to refuse (AD16 rule 4).
        let cwd = submission
            .queued
            .cwd
            .as_ref()
            .map_or_else(|| self.cwd.clone(), PathBuf::from);
        let route = submission.queued.route.clone();
        let claim = match claim {
            Some(claim) => claim,
            None => {
                self.open_lane(session, &route, submission.effective.model(), cwd)
                    .await
            }
        };
        self.queued.fetch_sub(1, Ordering::AcqRel);
        self.run_on_lane(slot, submission, (claim, prepared, connection))
            .await;
        Step::Next
    }

    /// Hands the submitted turn, with its lane's claim, to the lane's
    /// actor, which runs it to its end (Sol r3 N1), and waits for that
    /// end. Dropping this future, as aborting the dispatcher does (design
    /// §6.8 step 3), neither cancels the turn nor strands what it holds:
    /// its stop order, the daemon's force and the drivers' cancellation
    /// stop it, inside the actor. A lane that already ended, at final
    /// shutdown's cancellation, gives the turn back, and it runs on the
    /// daemon's tracker with no session channel, never in this future.
    #[expect(
        clippy::expect_used,
        reason = "every Engine is made in its Arc (Engine::open_with), which a borrowed Engine keeps alive"
    )]
    async fn run_on_lane(
        &self,
        slot: &Arc<Slot>,
        submission: Submission,
        (claim, prepared, connection): (
            LaneClaim,
            Prepared,
            Option<tokio::sync::OwnedSemaphorePermit>,
        ),
    ) {
        let engine = self.me.upgrade().expect("the Engine's own Arc");
        let slot = Arc::clone(slot);
        let lane = Arc::clone(claim.lane());
        let (done, ended) = tokio::sync::oneshot::channel::<()>();
        let job = turn_job(move |inbox| {
            Box::pin(async move {
                let held: &Lane = &claim;
                engine
                    .run(&slot, submission, (held, prepared, connection), inbox)
                    .await;
                engine.active.fetch_sub(1, Ordering::AcqRel);
                drop(claim);
                // The dispatcher may be gone: nothing waits for the end.
                let _ = done.send(());
            })
        });
        if let Err(job) = lane.hand_over(job) {
            // Never the caller's to run (Sol r4 R1): the daemon's tracker
            // owns it, with its record, run and claim.
            self.tracker.spawn(async move {
                let mut inbox = Inbox::closed();
                job(&mut inbox).await;
            });
        }
        // The job always runs to its end, which sends; a runtime ending
        // under it ends this future too.
        let _ = ended.await;
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
                _ = force.wait_for(Option::is_some) => return None,
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
        // Design §6.7: the terminal's facts, never its parsed envelope.
        match predecessors.last_submitted {
            // C1 §7.3: dispatch needs settled cleanup; pending cleanup waits.
            Some(facts)
                if facts
                    .cancel
                    .as_ref()
                    .is_some_and(|cancel| cancel.cleanup == "pending") =>
            {
                Decision::Wait
            }
            // P6: behind an `unknown` predecessor the queue is cancelled.
            Some(facts) if facts.state == "unknown" => Decision::Cancel,
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
    /// A submitted turn's start facts: the session's frozen `cwd` (design
    /// §11.1), or the daemon's startup directory for a session frozen
    /// without one (§5.1 #22).
    fn started(
        &self,
        session: &SessionId,
        turn: TurnNumber,
        queued: QueuedTurn,
        (submitted, clock): (SystemTime, Instant),
    ) -> Started {
        let cwd = queued.cwd.map_or_else(|| self.cwd.clone(), PathBuf::from);
        Started {
            session: session.clone(),
            turn,
            queued_at: queued.queued_at,
            first_seq: queued.queued_seq,
            cwd: cwd.to_str().map(str::to_owned),
            submitted: Some((rfc3339(submitted), clock)),
            folder: Some(self.evidence_folder(session, turn)),
        }
    }

    async fn run(
        &self,
        slot: &Slot,
        submission: Submission,
        (lane, prepared, capacity): (&Lane, Prepared, Option<tokio::sync::OwnedSemaphorePermit>),
        inbox: &mut Inbox,
    ) {
        let Submission {
            session,
            turn,
            queued,
            prompt,
            effective,
            submitted,
            clock,
        } = submission;
        let started = self.started(&session, turn, queued, (submitted, clock));
        // Design §2 [r1.11]: both deadlines run from the submission clock.
        let origin = tokio::time::Instant::from_std(clock);
        let (deadline, deadline_at) = wall_deadline(&effective, origin, submitted);
        let (mut record, activity, (route_stop, orders)) =
            start_turn(slot, &session, turn, deadline.instant());
        let mut control = Control {
            slot,
            turn,
            orders,
            observed: false,
            stored: false,
            refused: false,
            idle_at: Some(origin + effective.idle()),
            idle: effective.idle(),
            final_text: FinalText::new(),
        };
        // C2 §2 `TurnSpec`: the intake carries no other per-turn value yet.
        let spec = TurnSpec {
            prompt,
            ..TurnSpec::default()
        };
        let cx = self.turn_cx(turn, (prepared, capacity), activity, (deadline, route_stop));
        let driven = self
            .execute(
                &mut record,
                (lane, &effective),
                (spec, cx),
                (&mut control, inbox),
            )
            .await;
        let (cause, journal_uncertain) = driven.store_facts();
        self.route_failed(slot, &mut record, cause, journal_uncertain)
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
        let (vendor, outcome) = match driven {
            Driven::Finished(finished) => *finished,
            Driven::Forced(forced) => {
                // Task 4 design §2.3: the text Core holds is completed text;
                // the forced terminal keeps it.
                let text = self.settle_text(&mut control, &mut record).await;
                self.hand_off(slot, started, (record, text), forced, order)
                    .await;
                return;
            }
        };
        // C1 §7.6 and design §2's disposition table: Core decides from the
        // evidence and the order.
        let disposed = dispose(
            record.accepted.is_some(),
            (vendor.as_ref(), outcome),
            order.as_ref(),
            deadline.instant(),
        );
        let mut terminal = disposed.terminal;
        if let Some((outcome, cleanup)) = disposed.stop {
            let requested_at = if let Some(order) = &order {
                order.requested_at.clone()
            } else {
                // The wall deadline's own request (C1 §7.6).
                self.commit_event(&mut record, EventBody::CancelRequested {})
                    .await;
                deadline_at
            };
            terminal.cancel = Some(
                self.settle(&mut record, requested_at, outcome, cleanup)
                    .await,
            );
        }
        let text_failed = self
            .settle_final_text(&mut control, &mut record, &mut terminal)
            .await;
        refused_evidence(&record, &mut terminal);
        if record.first_failure.is_some() {
            // Acceptance or an observation could not be recorded after dispatch.
            terminal.fail(FailureClass::Store, "a turn event could not be recorded");
        }
        if text_failed {
            terminal.fail(FailureClass::Store, "the final text could not be written");
        }
        let cause = disposed
            .cancel_cause
            .filter(|_| terminal.state == "cancelled");
        // A terminal that did not commit reads `store_error` and latches.
        let _ = self.finish_with(started, (record, terminal), cause).await;
        // Test builds: the terminal committed, `Running` not yet cleared
        // (Task 4 design §4.2).
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.finish_running.pause").await;
        slot.finish_running(turn);
    }

    /// Hands a forced turn to final shutdown, which commits its terminal once
    /// Host has evidence (design §2 rule 4): the order's cause and
    /// `requested_at` travel with it.
    async fn hand_off(
        &self,
        slot: &Slot,
        started: Started,
        (mut record, text): (TurnRecord, TurnText),
        forced: Forced,
        order: Option<StopOrder>,
    ) {
        let turn = started.turn;
        // One `cancel.requested` per turn [r1.12]: an order's stays.
        let requested_at = if let Some(order) = &order {
            order.requested_at.clone()
        } else {
            self.commit_event(&mut record, EventBody::CancelRequested {})
                .await;
            forced.requested_at
        };
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.run.before_handoff").await;
        lock(&self.forced).push(ForcedTurn {
            started,
            record,
            requested_at,
            launched: forced.launched,
            close: forced.close,
            cause: order.map(|order| order.cause),
            text,
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
    /// session head; `None` when one fails. Store's read reply reports a
    /// corrupt one (design §7.1).
    async fn cancel_reads(
        &self,
        slot: &Slot,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Option<Queueing> {
        #[cfg(test)]
        self.hold(&self.faults.hold_cancel_read).await;
        #[cfg(feature = "test-failpoints")]
        if via_store::failpoint::hit_async("core.force.cancel_read")
            .await
            .is_err()
        {
            return None;
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
            return None;
        }
        let queued = match self.store.queued_turn(session, turn).await {
            Ok(Some(queued)) => Queueing::from(&queued),
            // Design §7.3 [s4.8]: a row Store cannot parse is cancelled
            // from its committed `turn.queued`, never submitted.
            Err(StoreError::CorruptEvidence) => self.queueing(session, turn).await.ok()?,
            Ok(None) | Err(_) => return None,
        };
        // Settle an unknown head now, so the commit below reads nothing.
        slot.head.lock(&self.store, session).await.ok()?;
        Some(queued)
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
        let Some(queued) = queued else {
            if owner == Owner::Dispatcher {
                // Design §7.3 [r1.13]: joined callers get `store_error`.
                slot.read_failed(turn);
            }
            return Cancelled::Unread;
        };
        self.commit_queued_cancel(slot, session, (turn, owner), closing, (cause, queued))
            .await
    }

    /// [`Self::cancel_queued`] after its reads, from the turn's `queued`
    /// facts: the queued row's, or the committed `turn.queued`'s when the
    /// row cannot be read (design §7.3 [s4.8]).
    pub(super) async fn commit_queued_cancel(
        &self,
        slot: &Slot,
        session: &SessionId,
        (turn, owner): (TurnNumber, Owner),
        closing: bool,
        (cause, queued): (Option<(CancelCause, String)>, Queueing),
    ) -> Cancelled {
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
        // Design §6.8 step 3 (Sol r4 R4): never before the session's lane
        // drain completes; the closure pass closes it after.
        let close = closing && !self.unresolved.others(session, turn) && self.lane_drained(session);
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
        if durable.closed {
            // Task 4 design §11.2: a closed-now answer.
            self.session_closed();
        }
        if let Some(outcome) = durable.uncertain {
            // The terminal is durable, but the commit itself was uncertain
            // or corrupt: a Store failure, so the restart handoff fails
            // startup (§10).
            self.store_failure(
                FailureSite::QueuedCancel,
                outcome,
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
            latch: Some(&self.signal),
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
        // Design §6.2: final shutdown's terminal is on the Lifecycle lane.
        let finished = Self::finish_turn_with(
            &self.store.lifecycle(),
            &self.unresolved,
            started,
            (record, terminal),
            close_session,
            TerminalExtras::default(),
            Commit {
                retry: false,
                latch: Some(&self.signal),
            },
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
    /// is kept for final shutdown's failure-resolution batch (design §7.4)
    /// before the latch is finished.
    async fn finish_with(
        &self,
        started: Started,
        (record, terminal): (TurnRecord, Terminal),
        cancel_cause: Option<CancelCause>,
    ) -> Result<(), ApiError> {
        let extras = TerminalExtras { cancel_cause };
        let mut record = record;
        let retry = record.first_failure.is_none();
        // C1 §5, §7.6 (Sol r2 #8): the spill write and the terminal are one
        // logical commit with one retry; a spill that took it leaves the
        // terminal none, and its first failure is reported as a retried
        // commit's is.
        let (finished, kept) = if let Some(spill_retried) = self.spill(&mut record, retry).await {
            let kept = (record.clone(), terminal.clone());
            let mode = Commit {
                retry: retry && !spill_retried,
                latch: Some(&self.signal),
            };
            let finished = Self::finish_turn_with(
                &self.store,
                &self.unresolved,
                &started,
                (record, terminal),
                false,
                extras,
                mode,
            )
            .await
            .map(|durable| Durable {
                retried: durable.retried || spill_retried,
                ..durable
            });
            (finished, kept)
        } else {
            // C1 §5, §7.6: the file write is part of the commit naming it,
            // so that commit failed and nothing was written.
            self.unresolved
                .fail(&started.session, started.turn, TurnState::Running);
            let unended = Unended {
                error: ApiError::STORE,
                outcome: WriteOutcome::NotCommitted,
            };
            (Err(unended), (record, terminal))
        };
        if finished.is_err() {
            let (record, terminal) = kept;
            self.keep_affected(AffectedTurn {
                started: started.clone(),
                record,
                terminal,
            });
        }
        let sites = (FailureSite::Terminal, FailureSite::Resolution);
        self.finished(&started, &finished, None, sites).await;
        finished.map(drop).map_err(|unended| unended.error)
    }

    /// C1 §5: before the commit that names it, writes a structured output
    /// over [`STRUCTURED_OUTPUT_INLINE`] encoded whole to the turn's
    /// `structured_output.json`, synced with its folder, and puts the file
    /// in its place; with `retry` a failed write is tried once more, taking
    /// the commit's one retry. `Some(retried)` once written, or with
    /// nothing to write: whether that took the retry. `None` when the
    /// write failed: the commit that would name the file fails, and both
    /// fields are `null`.
    pub(super) async fn spill(&self, record: &mut TurnRecord, retry: bool) -> Option<bool> {
        let session = record.session.clone();
        let turn = record.turn;
        let Some(retained) = record.vendor.retained.as_mut() else {
            return Some(false);
        };
        let Some(encoded) = retained
            .structured_output
            .as_ref()
            .and_then(|value| serde_json::to_vec(value).ok())
            .filter(|encoded| encoded.len() > STRUCTURED_OUTPUT_INLINE)
        else {
            return Some(false);
        };
        // Taken first: a write cut short by a caller's bound names nothing.
        retained.structured_output = None;
        for attempt in 0..=u8::from(retry) {
            let written = self
                .store
                .write_structured_output(&session, turn, encoded.clone())
                .await;
            if let Ok(file) = written {
                retained.structured_output_file = Some(StructuredOutputFile {
                    path: file.path.display().to_string(),
                    bytes: file.bytes,
                });
                return Some(attempt > 0);
            }
        }
        None
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
        if matches!(finished, Ok(durable) if durable.closed) {
            // Task 4 design §11.2: a closed-now answer.
            self.session_closed();
        }
        let failed = match finished {
            Ok(Durable {
                uncertain: Some(outcome),
                ..
            }) => Some((first, *outcome)),
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
        mode: Commit<'_>,
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
    /// with the terminal envelope.
    /// An uncertain event commit is settled against the durable head first. With
    /// `close_session`, `session.closed` follows in the same transaction;
    /// otherwise `extras` commit with the terminal. With `mode.retry`, a commit known not committed is retried
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
        extras: TerminalExtras,
        mode: Commit<'_>,
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
        let seq = first;
        let ended = ended_record(started, record, terminal, seq)?;
        let closed = if close_session {
            let closed = Event {
                seq: seq + 1,
                session_id: &started.session,
                turn: None,
                late: false,
                at: &rfc3339(SystemTime::now()),
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
            journal::commit_terminal_with(journal, ended, closed, extras, (mode.retry, mode.latch))
                .await;
        match &committed {
            Ok(Durable {
                uncertain: None,
                closed,
                ..
            }) => head.committed(seq + 1 - first + u64::from(*closed)),
            // Nothing was written: the sequence stays the session's next.
            Err(unended) if unended.outcome == WriteOutcome::NotCommitted => drop(head),
            // Uncertain: re-read the head before the session's next event.
            Ok(_) | Err(_) => head.lost(),
        }
        committed
    }

    /// The turn's context (C2 §2 `TurnCx`): the connection slot Core
    /// reserved, if any, the activity clock, the wall deadline, the stop
    /// order and the daemon force.
    fn turn_cx(
        &self,
        turn: TurnNumber,
        (prepared, capacity): (Prepared, Option<tokio::sync::OwnedSemaphorePermit>),
        activity: TurnActivity,
        (wall, stop): (Deadline, StopWatch),
    ) -> TurnCx {
        TurnCx {
            turn,
            prepared,
            capacity: capacity.map(|permit| Box::new(permit) as via_adapters::CapacityToken),
            activity,
            wall,
            tool_grace: TOOL_GRACE,
            stop,
            force: self.signal.force.subscribe(),
        }
    }

    /// The session channel (C2 A1): 1,024 items and a 4 MiB byte budget;
    /// an item's permit is held until it is handled. What arrived before
    /// the turn is the session drain's (C2 §2), which keeps servicing the
    /// turn's order and idle deadline (runtime §8). It is finite (critical
    /// r1 #1): only what the channel held at the handover; what arrives
    /// later is the running turn's to attribute.
    async fn pre_turn_drain(
        &self,
        record: &mut TurnRecord,
        lane: &Lane,
        control: &mut Control<'_>,
        inbox: &mut Inbox,
    ) {
        let mut handled = 0;
        for _ in 0..inbox.len() {
            let Some(admitted) = inbox.try_recv() else {
                break;
            };
            self.between_items(record, control, &mut handled, true)
                .await;
            lane.dispose(admitted).await;
        }
    }

    /// Runs the turn on the session's driver under the turn deadline,
    /// handling each observation on the session channel, the lane actor's
    /// `inbox`, in decode order before its result is acted on. What
    /// arrived before the turn is handled first, as the session drain
    /// handles it (C2 §2): durable items commit with their own
    /// attribution, late ones of earlier turns `late: true` (AD4),
    /// session-level ones with no turn; the turn's record then takes the
    /// identity that drain may have committed (Sol r3 N7). A force stop
    /// reaches Route, which force-closes the group and drains its output
    /// first: messages it read are still handled. What the driver
    /// delivered before it returned is handled one item at a time, each
    /// taken only once the one before it is done (Sol r3 N2).
    ///
    /// The turn's stop order reaches Route through its `TurnCx`; this loop
    /// observes it once (design §2), and orders the idle deadline itself
    /// when no meaningful progress came within the idle budget (design §5).
    async fn execute(
        &self,
        record: &mut TurnRecord,
        (lane, effective): (&Lane, &Effective),
        (spec, cx): (TurnSpec, TurnCx),
        (control, inbox): (&mut Control<'_>, &mut Inbox),
    ) -> Driven {
        self.pre_turn_drain(record, lane, control, inbox).await;
        if matches!(cx.prepared, Prepared::NeedsConnection) {
            // The turn opens a connection generation (C2 §2, critical r1
            // #5): ownership is scoped to it, once the older generation's
            // items above were disposed of.
            lane.new_generation();
        }
        record.vendor.identity = lane.identity();
        let mut run = Box::pin(lane.driver.run_turn(spec, cx));
        // Design §9: every commit here runs inside `while_polling`, so the
        // driver keeps servicing its controls; a result it returns early is
        // kept, with the channel's count at that return, and acted on after
        // the commit.
        let mut early = None;
        // Items the fired idle deadline handled, for the control checks.
        let mut frontier = 0;
        let end = loop {
            if let Some(end) = early.take() {
                break end;
            }
            let idle_at = control.idle_at;
            // Critical r1b #12 (runtime §8): biased, controls first. A ready
            // order or idle deadline is serviced before the next data item,
            // and the driver's return before it too: the final drain then
            // takes what was delivered by the return (critical r1 #1), and
            // a never-empty channel cannot hold the run's end.
            tokio::select! {
                biased;
                changed = control.orders.changed(), if !control.observed => {
                    let order = changed
                        .ok()
                        .and_then(|()| control.orders.borrow_and_update().clone());
                    if let Some(order) = order {
                        while_polling(
                            (&mut run, &mut early),
                            || inbox.len(),
                            self.observe_order(record, control, &order),
                        )
                        .await;
                        stop_for_store(record, control);
                    }
                }
                () = sleep_until_some(idle_at), if idle_at.is_some() => {
                    // Design §5, decided at the item frontier (critical r3
                    // #1).
                    self.idle_fired(
                        record,
                        (lane, effective),
                        control,
                        (&mut run, &mut early),
                        (inbox, idle_at),
                    )
                    .await;
                    super::lane::ready_item(&mut frontier).await;
                }
                end = &mut run => {
                    // What the driver delivered by its return (critical
                    // r2 F2), counted at its first `Ready`.
                    let delivered = inbox.len();
                    // Test builds: the driver's turn returned.
                    #[cfg(feature = "test-failpoints")]
                    let _ = via_store::failpoint::hit_async("core.run.returned").await;
                    break (end, delivered);
                }
                Some(admitted) = inbox.recv() => {
                    self.run_item(
                        record,
                        (lane, effective),
                        control,
                        (&mut run, &mut early),
                        (admitted, &*inbox),
                    )
                    .await;
                }
            }
        };
        // The driver delivered the turn's items before it returned.
        let (end, delivered) = end;
        self.final_drain(record, (Some(lane), effective), control, (inbox, delivered))
            .await;
        let TurnEnd {
            terminal,
            instance,
            leftovers: _,
            outcome,
        } = end;
        // AD7: the version the turn's own instance reported, on every
        // outcome once its handshake was read.
        record.vendor.instance = instance.map(|instance| {
            let tested = instance.version_status == VersionStatus::Tested;
            (instance.vendor_version, tested)
        });
        record.vendor.retained = terminal.as_ref().map(Retained::of);
        match outcome {
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
                    launched: route.launched,
                    close: RouteClose {
                        forced: route.forced,
                        quiescent: route.cleanup == Some(WireCleanup::Quiescent),
                    },
                    journal_uncertain: route.journal_uncertain,
                })
            }
            outcome => Driven::Finished(Box::new((terminal, outcome))),
        }
    }

    /// Handles one item the running turn's loop took, in decode order,
    /// while the driver is polled (design §9); the driver's first `Ready`
    /// keeps the channel's count then ([`while_polling`]). The turn's own
    /// meaningful progress moves its idle deadline ([`note_progress`]).
    async fn run_item<E>(
        &self,
        record: &mut TurnRecord,
        (lane, effective): (&Lane, &Effective),
        control: &mut Control<'_>,
        run: (&mut E, &mut Option<(TurnEnd, usize)>),
        (admitted, inbox): (Admitted, &Inbox),
    ) where
        E: std::future::Future<Output = TurnEnd> + Unpin,
    {
        let Admitted { item, permit } = admitted;
        note_progress(lane, control, &item);
        while_polling(run, || inbox.len(), async {
            // Test builds: Core holds before handling an observation.
            #[cfg(feature = "test-failpoints")]
            let _ = via_store::failpoint::hit_async("core.observations.pause").await;
            self.observe(record, Some(lane), effective, control, item)
                .await;
        })
        .await;
        // Handled: its bytes return to the budget.
        drop(permit);
        stop_for_store(record, control);
    }

    /// The turn's idle deadline fired (Task 4 design §5), decided at the
    /// item frontier (critical r3 #1): the turn expires only when the
    /// channel is empty or its next item was decoded after the deadline
    /// (its own `at`). That item, taken to read it, is then handled after
    /// the idle order, as ordinary late progress. Otherwise the next item
    /// is handled now, in order, as the run loop handles it ([`Self::run_item`]):
    /// the turn's own progress moves the deadline by its `at`, and the loop
    /// fires again while the deadline stays passed, the turn's order
    /// checked first each time (runtime §8). It is finite: only items
    /// decoded before a deadline qualify, and the deadline moves only on
    /// progress.
    async fn idle_fired<E>(
        &self,
        record: &mut TurnRecord,
        (lane, effective): (&Lane, &Effective),
        control: &mut Control<'_>,
        run: (&mut E, &mut Option<(TurnEnd, usize)>),
        (inbox, deadline): (&mut Inbox, Option<tokio::time::Instant>),
    ) where
        E: std::future::Future<Output = TurnEnd> + Unpin,
    {
        let next = inbox.try_recv();
        let expires = next
            .as_ref()
            .is_none_or(|admitted| deadline.is_none_or(|deadline| admitted.item.at > deadline));
        if expires {
            control.idle_at = None;
            // The timer fired; its order is not issued yet (design §10).
            #[cfg(feature = "test-failpoints")]
            let _ = via_store::failpoint::hit_async("core.run.idle_expired").await;
            control
                .slot
                .idle_order(control.turn, tokio::time::Instant::now());
        }
        if let Some(admitted) = next {
            self.run_item(record, (lane, effective), control, run, (admitted, &*inbox))
                .await;
        }
    }

    /// The turn's final drain (Sol r3 N2): what the driver delivered
    /// before it returned, the `delivered` items the channel held then,
    /// handled one item at a time in decode order, each taken only once the
    /// one before it is done, while the turn's order is still serviced
    /// (runtime §8, Sol r4 R5). It is finite (critical r1 #1): the
    /// terminal follows, and what arrives later is the lane actor's
    /// between turns. The count is read at the driver's first `Ready`, even
    /// when it returned during a commit (critical r2 F2).
    async fn final_drain(
        &self,
        record: &mut TurnRecord,
        (lane, effective): (Option<&Lane>, &Effective),
        control: &mut Control<'_>,
        (inbox, delivered): (&mut Inbox, usize),
    ) {
        let mut handled = 0;
        for _ in 0..delivered {
            let Some(admitted) = inbox.try_recv() else {
                break;
            };
            self.between_items(record, control, &mut handled, false)
                .await;
            self.drain_one(record, lane, effective, control, admitted)
                .await;
        }
    }

    /// Services the turn's controls before each ready item of a drain
    /// (runtime §8: at most 128 ready data items between checks; Sol r4
    /// R5): an order not yet observed is observed, and, `before` the
    /// turn's run, an idle deadline that passed issues its order, as the
    /// run loop does. Every [`super::lane::READY_ITEMS`]th item yields. The
    /// driver's health is its own during the run, and the lane's actor's
    /// between turns. After the run returned no deadline is issued: the
    /// turn already ended.
    async fn between_items(
        &self,
        record: &mut TurnRecord,
        control: &mut Control<'_>,
        handled: &mut usize,
        before: bool,
    ) {
        let now = tokio::time::Instant::now();
        if before && control.idle_at.is_some_and(|idle_at| idle_at <= now) {
            control.idle_at = None;
            control.slot.idle_order(control.turn, now);
        }
        if !control.observed && control.orders.has_changed().unwrap_or(false) {
            let order = control.orders.borrow_and_update().clone();
            if let Some(order) = order {
                self.observe_order(record, control, &order).await;
                stop_for_store(record, control);
            }
        }
        super::lane::ready_item(handled).await;
    }

    /// Handles one observation queued on the session channel, in decode
    /// order, and returns its budget. A failed write sends or upgrades the
    /// turn's order to cause `store`, as in the observation branch (design
    /// §7.2 row 5).
    async fn drain_one(
        &self,
        record: &mut TurnRecord,
        lane: Option<&Lane>,
        effective: &Effective,
        control: &mut Control<'_>,
        Admitted { item, permit }: Admitted,
    ) {
        self.observe(record, lane, effective, control, item).await;
        drop(permit);
        stop_for_store(record, control);
    }

    /// Test builds: runs the running turn of `slot` on `lane` as `run`
    /// does, with the run loop's own order receiver and `inbox` for the
    /// session channel.
    #[cfg(test)]
    pub(super) async fn execute_turn(
        &self,
        (slot, lane): (&Slot, &Lane),
        (record, effective): (&mut TurnRecord, &Effective),
        (orders, inbox): (watch::Receiver<Option<StopOrder>>, &mut Inbox),
        (spec, cx): (TurnSpec, TurnCx),
    ) {
        self.execute_turn_idle(
            (slot, lane),
            (record, effective),
            (orders, inbox),
            (spec, cx),
            None,
        )
        .await;
    }

    /// Test builds: [`Self::execute_turn`] with an idle budget, its
    /// deadline running from now.
    #[cfg(test)]
    pub(super) async fn execute_turn_idle(
        &self,
        (slot, lane): (&Slot, &Lane),
        (record, effective): (&mut TurnRecord, &Effective),
        (orders, inbox): (watch::Receiver<Option<StopOrder>>, &mut Inbox),
        (spec, cx): (TurnSpec, TurnCx),
        idle: Option<Duration>,
    ) {
        let mut control = Control {
            slot,
            turn: record.turn,
            orders,
            observed: false,
            stored: false,
            refused: false,
            idle_at: idle.map(|idle| tokio::time::Instant::now() + idle),
            idle: idle.unwrap_or_default(),
            final_text: FinalText::new(),
        };
        let _driven = self
            .execute(record, (lane, effective), (spec, cx), (&mut control, inbox))
            .await;
    }

    /// Test builds: drains `queued` for the running `turn` of `slot` as
    /// `execute`'s completion does, with the run loop's own order receiver
    /// and the session's `lane`; without one every item is the running
    /// turn's.
    #[cfg(test)]
    pub(super) async fn drain_queued(
        &self,
        (slot, lane): (&Slot, Option<&Lane>),
        record: &mut TurnRecord,
        effective: &Effective,
        orders: watch::Receiver<Option<StopOrder>>,
        queued: Vec<ObservationItem>,
    ) {
        let budget = Arc::new(tokio::sync::Semaphore::new(queued.len()));
        let queued: Vec<Admitted> = queued
            .into_iter()
            .map(|item| Admitted {
                item,
                permit: Arc::clone(&budget)
                    .try_acquire_owned()
                    .expect("one permit per item"),
            })
            .collect();
        let mut control = Control {
            slot,
            turn: record.turn,
            orders,
            observed: false,
            stored: false,
            refused: false,
            idle_at: None,
            idle: Duration::ZERO,
            final_text: FinalText::new(),
        };
        let (sender, receiver) = tokio::sync::mpsc::channel(queued.len().max(1));
        for admitted in queued {
            assert!(
                sender.try_send(admitted).is_ok(),
                "the channel holds every queued item"
            );
        }
        drop(sender);
        let mut inbox = Inbox::of(receiver);
        let delivered = inbox.len();
        self.final_drain(
            record,
            (lane, effective),
            &mut control,
            (&mut inbox, delivered),
        )
        .await;
    }

    /// Handles one observation in decode order (C2 §4): commits the
    /// acceptance at the next sequence; folds progress marks into the step
    /// tracker, publishing each change to `slot` (Task 4 design §2.4), and
    /// their usage into the turn's ledger (AD6); commits denials and
    /// declines as events for the envelope's lists; and keeps a confirmed
    /// identity. An item of an earlier, ended turn is late (AD4). After the
    /// first Store failure no event commits and the turn fails `store`;
    /// progress still folds, and its rows ride in the terminal.
    async fn observe(
        &self,
        record: &mut TurnRecord,
        lane: Option<&Lane>,
        effective: &Effective,
        control: &mut Control<'_>,
        item: ObservationItem,
    ) {
        let slot = control.slot;
        let vendor_turn = item
            .vendor_turn
            .as_ref()
            .map(|turn| turn.as_str().to_owned());
        // C2 §2: a confirmed identity is the session's, whichever vendor
        // turn its message names.
        if let Observation::IdentityConfirmed(identity) = item.observation {
            self.confirm_identity(record, lane, identity).await;
            return;
        }
        // The acceptance is the running turn's by its correlation with the
        // turn's start (C2 §4.1): it maps the vendor turn it names, which
        // only then is current (Sol r2 #5).
        let attribution = match (&item.observation, lane) {
            (Observation::Accepted(_), _) | (_, None) => Attribution::Current,
            (_, Some(lane)) => lane.attribute(vendor_turn.as_deref(), Some(record.turn)),
        };
        match attribution {
            Attribution::Current => {}
            Attribution::Late(turn) => {
                self.observe_other(record, (Some(turn.get()), true), item.observation)
                    .await;
                return;
            }
            Attribution::Session => {
                self.observe_other(record, (None, false), item.observation)
                    .await;
                return;
            }
            // C2 §2: an expired vendor turn's traffic is dropped; it is
            // never another turn's or the session's.
            Attribution::Expired => return,
        }
        match item.observation {
            Observation::Accepted(acceptance) => {
                let mapped = acceptance_turn(&acceptance, vendor_turn.as_deref());
                if map_acceptance(record, lane, control, mapped) == Mapped::Collided {
                    // An ID the lane maps to another turn, or tombstoned,
                    // establishes nothing.
                    return;
                }
                let correlation = acceptance_turn(&acceptance, vendor_turn.as_deref()).map_or_else(
                    || format!("{TOKEN_CORRELATION}{}", acceptance.correlation.get()),
                    |vendor_turn| format!("{VENDOR_CORRELATION}{vendor_turn}"),
                );
                let vendor_turn_id = acceptance
                    .vendor_turn_id
                    .as_ref()
                    .map(|id| id.as_str().to_owned());
                let running = lane.and_then(|lane| lane.driver.adapter_version());
                self.observe_acceptance(
                    record,
                    slot,
                    effective,
                    (correlation, vendor_turn_id, running),
                )
                .await;
            }
            Observation::Progress(marks) => {
                // A refused item changes nothing; the run loop stops the
                // turn `protocol` (review r1).
                let Ok(folded) = record.steps.fold(&marks, item.at) else {
                    return;
                };
                if let Some(usage) = &marks.usage {
                    record.vendor.ledger.add(usage);
                }
                if let Some(row) = folded.row {
                    // Design §3.2: a boundary publishes the new step first,
                    // then commits the ended step's row.
                    #[cfg(feature = "test-failpoints")]
                    let _ = via_store::failpoint::hit_async("core.progress.publish").await;
                    slot.publish_progress(record.turn, &folded.delta);
                    self.commit_step(record, row).await;
                } else {
                    slot.publish_progress(record.turn, &folded.delta);
                }
            }
            Observation::FinalText(piece) => {
                // Design §6.4: inline up to 256 KiB encoded, else the file;
                // a failed file step fails the turn `store` as a known
                // `NotCommitted` does (§3.2).
                let address = (&record.session, record.turn);
                if control
                    .final_text
                    .push(&self.store, address, &piece)
                    .await
                    .is_err()
                {
                    self.final_text_failed(record).await;
                }
            }
            Observation::ActionDenied(denial) => {
                let own = (Some(record.turn.get()), false);
                self.commit_denial(record, own, denial).await;
            }
            Observation::RequestDeclined(decline) => {
                let own = (Some(record.turn.get()), false);
                self.commit_decline(record, own, decline).await;
            }
            Observation::Warning(warning) => self.own_warning(record, warning).await,
            // C2 §4: a mismatch commits nothing itself; the turn is
            // disposed from its end (r3). A vendor close has no C1 event:
            // the driver ends the connection, and the turn's end carries
            // what it did to the turn. Core routes no steer, so no steer
            // report comes. An identity was handled before attribution.
            Observation::IdentityConfirmed(_)
            | Observation::ResumeMismatch { .. }
            | Observation::VendorClosed(_)
            | Observation::SteerDelivered(_)
            // Discarded until via-jm4.35: a late terminal's revision
            // write is not in the Store yet.
            | Observation::LateTerminal(_) => {}
        }
    }

    /// Commits the running turn's own adapter warning; one of C1 §5's
    /// closed list, once committed, is also its envelope's, once per code
    /// ([`Warning::adapter`]).
    async fn own_warning(&self, record: &mut TurnRecord, warning: via_adapters::Warning) {
        let own = (Some(record.turn.get()), false);
        // Sol r3 N8: retained within its caps, as the envelope reports it.
        let listed = Warning::adapter(warning.code, warning.data.clone()).map(Warning::capped);
        let committed = self.commit_warning(record, own, warning).await;
        if let (Some(_), Some(listed)) = (committed, listed)
            && !record
                .vendor
                .warnings
                .iter()
                .any(|kept| kept.code() == listed.code())
        {
            record.vendor.warnings.push(listed);
        }
    }

    /// Commits an adapter-reported `warning` event attributed to `(turn,
    /// late)` (C1 §6.1), within C1 §5's caps: `message` cut to 1 KiB
    /// encoded, `data` over 4 KiB encoded left out. Its sequence once it
    /// committed.
    async fn commit_warning(
        &self,
        record: &mut TurnRecord,
        attributed: (Option<u32>, bool),
        warning: via_adapters::Warning,
    ) -> Option<u64> {
        let at = rfc3339(SystemTime::now());
        let body = EventBody::warning(warning.code, &warning.message, warning.data);
        let failed = record.first_failure.is_some();
        let seq = journal::commit_event_as(&self.store, record, body, &at, attributed).await;
        self.report_first_failure(record, failed).await;
        seq
    }

    /// Commits a confirmed identity (C2 §2 delayed identity, C1 §6.1):
    /// the first confirmation of a connection generation commits exactly
    /// one `session.opened` (the session's first) or `session.reopened`,
    /// carrying the ID and transcript hint, before any acceptance of the
    /// same message, which the driver sends after it. Only a committed
    /// identity becomes the session's (decision H3: the journal holds it);
    /// a failed commit is the turn's first failure. Only the driver's
    /// current generation counts; that generation confirming again with a
    /// new transcript hint writes the session's columns with no event (Sol
    /// r2 #4).
    async fn confirm_identity(
        &self,
        record: &mut TurnRecord,
        lane: Option<&Lane>,
        identity: via_adapters::observation::Identity,
    ) {
        let confirmed = Lane::confirmed(lane, &identity);
        let Some(lane) = lane else {
            record.vendor.identity = Some(confirmed);
            return;
        };
        // A stale generation's confirmation counts nothing (C2 §2).
        if !lane.current(&identity.connection_id) {
            return;
        }
        let vendor_version = identity.vendor_version.clone();
        let opened = lane.open_event(&identity.connection_id, &confirmed, vendor_version);
        let opened_event = opened.is_some();
        if opened.is_none() && !lane.changes(&confirmed) {
            record.vendor.identity = Some(confirmed);
            return;
        }
        if record.first_failure.is_some() {
            return;
        }
        let written = match opened {
            Some(body) => {
                let at = rfc3339(SystemTime::now());
                journal::commit_session_event(
                    &self.store,
                    (&record.head, &record.session),
                    (body, &at, (None, false)),
                    Some(confirmed.columns()),
                )
                .await
            }
            // The same generation again, naming more: its columns, with
            // no event.
            None => {
                journal::commit_identity_columns(&self.store, &record.session, confirmed.columns())
                    .await
            }
        };
        match written {
            journal::SessionWrite::Committed => {
                if opened_event {
                    lane.opened(identity.connection_id, confirmed.clone());
                } else {
                    lane.confirm(confirmed.clone());
                }
                record.vendor.identity = Some(confirmed);
            }
            journal::SessionWrite::Refused => {}
            journal::SessionWrite::Failed(outcome) => {
                record.first_failure = Some(FailureNote {
                    site: FailureSite::Event,
                    outcome,
                });
                self.report_first_failure(record, false).await;
            }
        }
    }

    /// Commits the turn's acceptance at the next sequence (C2 §4
    /// `turn.accepted`), with its Store correlation, vendor turn ID and the
    /// running adapter's version.
    async fn observe_acceptance(
        &self,
        record: &mut TurnRecord,
        slot: &Slot,
        effective: &Effective,
        evidence: (String, Option<String>, Option<String>),
    ) {
        if let Some(delta) = record.steps.accept() {
            slot.publish_progress(record.turn, &delta);
        }
        // The driver admits one acceptance; a repeat would be deduplicated anyway.
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
            .accept((&record.session, record.turn, seq), effective, evidence)
            .await
        {
            Ok(accepted) => {
                head.committed(1);
                record.accepted = Some(accepted);
            }
            Err((outcome, sent)) => {
                if outcome.head_unknown() {
                    head.lost();
                    if let Some((accepted, event)) = sent {
                        record.uncertain = Some(UncertainEvent {
                            seq,
                            event,
                            accepted: Some(accepted),
                        });
                    }
                } else {
                    drop(head);
                }
                // The head lock is released before the latch takes
                // admission; corruption latches (design §7.1).
                self.event_failed(record, outcome).await;
            }
        }
    }

    /// An observation that is not the running turn's (AD4, C1 §6.1, C2
    /// §2), attributed to `(turn, late)`: of an earlier, ended turn with
    /// `late: true`, or session-level with `turn: null`. A durable one, a
    /// denial, a decline or a warning, is committed under the running
    /// turn, the one the Store admits events for; it never changes an
    /// envelope.
    /// Non-durable ones are dropped, and so is a late terminal, until
    /// via-jm4.35.
    async fn observe_other(
        &self,
        record: &mut TurnRecord,
        attributed: (Option<u32>, bool),
        observation: Observation,
    ) {
        match observation {
            Observation::ActionDenied(denial) => {
                self.commit_denial(record, attributed, denial).await;
            }
            Observation::RequestDeclined(decline) => {
                self.commit_decline(record, attributed, decline).await;
            }
            Observation::Warning(warning) => {
                self.commit_warning(record, attributed, warning).await;
            }
            Observation::Accepted(_)
            | Observation::IdentityConfirmed(_)
            | Observation::Progress(_)
            | Observation::FinalText(_)
            | Observation::SteerDelivered(_)
            | Observation::VendorClosed(_)
            | Observation::ResumeMismatch { .. }
            // Discarded until via-jm4.35: a late terminal's revision
            // write is not in the Store yet.
            | Observation::LateTerminal(_) => {}
        }
    }

    /// Commits `action.denied` attributed to `(turn, late)`; the running
    /// turn's own is kept for its envelope's `denied_actions`.
    async fn commit_denial(
        &self,
        record: &mut TurnRecord,
        (turn, late): (Option<u32>, bool),
        denial: Denial,
    ) {
        let kind = denial_kind(denial.kind);
        let at = rfc3339(SystemTime::now());
        let body = denial_body(&denial);
        let failed = record.first_failure.is_some();
        let seq = journal::commit_event_as(&self.store, record, body, &at, (turn, late)).await;
        self.report_first_failure(record, failed).await;
        let own = turn == Some(record.turn.get()) && !late;
        if let (Some(seq), true) = (seq, own) {
            let entry = DeniedAction::new((kind, denial.target, denial.reason), at, seq);
            record.vendor.denied.push(entry);
        }
    }

    /// Commits `vendor.request_declined` attributed to `(turn, late)`; the
    /// running turn's own is kept for its envelope's
    /// `auto_declined_requests`.
    async fn commit_decline(
        &self,
        record: &mut TurnRecord,
        (turn, late): (Option<u32>, bool),
        decline: Decline,
    ) {
        let at = rfc3339(SystemTime::now());
        let body = decline_body(&decline);
        let failed = record.first_failure.is_some();
        let seq = journal::commit_event_as(&self.store, record, body, &at, (turn, late)).await;
        self.report_first_failure(record, failed).await;
        let own = turn == Some(record.turn.get()) && !late;
        if let (Some(seq), true) = (seq, own) {
            let entry = AutoDeclined::new(
                (decline.vendor_method, decline.summary, decline.blocking),
                at,
                seq,
            );
            record.vendor.declined.push(entry);
        }
    }

    /// Makes the final text's file durable before the terminal names it
    /// (design §6.4) and puts the text in `terminal`; whether a file step
    /// failed, which the caller turns into `failed(store)`.
    async fn settle_final_text(
        &self,
        control: &mut Control<'_>,
        record: &mut TurnRecord,
        terminal: &mut Terminal,
    ) -> bool {
        self.settle_text(control, record).await.apply(terminal)
    }

    /// Makes the final text Core holds durable (design §6.4): inline, or
    /// its file synced. A failed file step is the turn's first failure.
    async fn settle_text(&self, control: &mut Control<'_>, record: &mut TurnRecord) -> TurnText {
        let text = std::mem::replace(&mut control.final_text, FinalText::new())
            .settle()
            .await;
        if text.failed {
            self.final_text_failed(record).await;
        }
        TurnText {
            inline: text.inline,
            file: text.file.map(|file| FinalTextFile {
                path: file.path.display().to_string(),
                bytes: file.bytes,
                truncated: file.truncated,
            }),
            failed: text.failed,
        }
    }

    /// Records a failed final-text file step as the turn's first failure, a
    /// known `NotCommitted`, and reports it (design §6.4).
    async fn final_text_failed(&self, record: &mut TurnRecord) {
        if record.first_failure.is_none() {
            self.event_failed(record, WriteOutcome::NotCommitted).await;
        }
    }

    /// Commits the row of a step that ended (Task 4 design §3.2) on the
    /// Internal lane. After the turn's first failure the row is carried to
    /// the terminal. A known `NotCommitted` becomes the first failure and
    /// its row is carried; an uncertain outcome latches, and the row, which
    /// may have committed, is not.
    async fn commit_step(&self, record: &mut TurnRecord, row: StepRow) {
        if record.first_failure.is_some() {
            record.steps.carried.push(row);
            return;
        }
        let committed = self
            .store
            .commit_steps(StepsRecord {
                session_id: record.session.clone(),
                turn: record.turn,
                rows: vec![row],
            })
            .await;
        if let Err(error) = committed {
            let outcome = WriteOutcome::of(&error);
            if outcome == WriteOutcome::NotCommitted {
                record.steps.carried.push(row);
            }
            record.first_failure = Some(FailureNote {
                site: FailureSite::Event,
                outcome,
            });
            self.report_first_failure(record, false).await;
        }
    }

    /// Commits one non-lifecycle event of the running turn at the next sequence.
    pub(super) async fn commit_event(&self, record: &mut TurnRecord, body: EventBody) {
        let failed = record.first_failure.is_some();
        journal::commit_event(&self.store, record, body).await;
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
            slot.start_running(
                turn,
                tokio::time::Instant::now(),
                Progress::starting(turn.get()),
            );
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
        let mut queued = match journal.queued_turn(session, turn).await {
            Ok(Some(queued)) => queued,
            // Store could not parse the row's frozen values (design §7.3).
            Err(StoreError::CorruptEvidence) => return Err(SubmitFailure::Corrupt(None)),
            Ok(None) | Err(_) => return Err(SubmitFailure::Unread),
        };
        let queueing = |queued: &QueuedTurn| Queueing::from(queued);
        // A frozen row Core cannot read fails the turn: nothing is sent.
        let Ok(effective) = serde_json::from_value::<Effective>(queued.effective.clone()) else {
            return Err(SubmitFailure::Corrupt(Some(queueing(&queued))));
        };
        // Design §6.5: a blob prompt is loaded into one exact `String` with
        // its SHA-256 and UTF-8 checks; a blob that differs from its record
        // fails the turn as corrupt evidence, before anything is sent.
        let prompt = match std::mem::replace(&mut queued.prompt, Prompt::Inline(String::new())) {
            Prompt::Inline(text) => text,
            Prompt::Blob(blob) => match journal.load_prompt(&blob).await {
                Ok(text) => text,
                Err(StoreError::CorruptEvidence) => {
                    return Err(SubmitFailure::Corrupt(Some(queueing(&queued))));
                }
                Err(_) => return Err(SubmitFailure::Unread),
            },
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
            body: EventBody::TurnSubmitted { attempt: 1 },
        }
        .to_value()
        .map_err(|_| SubmitFailure::NotCommitted(queueing(&queued)))?;
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
                return Err(SubmitFailure::NotCommitted(queueing(&queued)));
            }
        }
        Ok(Submission {
            session: session.clone(),
            turn,
            queued,
            prompt,
            effective,
            submitted,
            clock,
        })
    }

    /// Commits vendor acceptance as C2 evidence and C1 `turn.started` together.
    /// A failure carries its classified outcome ([`WriteOutcome::of`]) and,
    /// when the head is unknown, the acceptance and the event sent.
    /// `correlation` is the acceptance's tagged Store correlation: the
    /// vendor turn ID, else the acceptance token. `adapter_version`, the running
    /// adapter's, becomes the session's recorded one in the same commit
    /// (C1 §3.3, decision H3).
    async fn accept(
        &self,
        (session, turn, seq): (&SessionId, TurnNumber, u64),
        effective: &Effective,
        (correlation, vendor_turn_id, adapter_version): (String, Option<String>, Option<String>),
    ) -> Result<Accepted, AcceptFailure> {
        let at = rfc3339(SystemTime::now());
        let event = Event {
            seq,
            session_id: session,
            turn: Some(turn.get()),
            late: false,
            at: &at,
            body: EventBody::TurnStarted {
                effective: effective.clone(),
            },
        }
        .to_value()
        .map_err(|_| (WriteOutcome::NotCommitted, None))?;
        // The vendor accepted; its acceptance is not yet recorded.
        #[cfg(feature = "test-failpoints")]
        if via_store::failpoint::hit_async("core.accept.before_commit")
            .await
            .is_err()
        {
            return Err((WriteOutcome::NotCommitted, None));
        }
        let committed = self
            .store
            .commit_acceptance(AcceptanceRecord {
                session_id: session.clone(),
                turn,
                correlation,
                event: event.clone(),
                adapter_version,
            })
            .await;
        let accepted = Accepted { at, vendor_turn_id };
        match committed {
            Ok(()) => Ok(accepted),
            Err(error) => {
                let outcome = WriteOutcome::of(&error);
                Err((outcome, outcome.head_unknown().then_some((accepted, event))))
            }
        }
    }
}

/// A failed acceptance commit: its classified outcome and, when the head is
/// unknown, the acceptance and the event sent.
type AcceptFailure = (WriteOutcome, Option<(Accepted, serde_json::Value)>);

/// `turn.ended` at `seq` with the terminal envelope; `record` is already
/// reconciled (design §7.4's batch builds it the same way).
pub(super) fn ended_record(
    started: &Started,
    record: TurnRecord,
    terminal: Terminal,
    seq: u64,
) -> Result<TerminalRecord, ApiError> {
    let ended_at = rfc3339(SystemTime::now());
    // Design §3.2: every terminal built from the record carries the rows it
    // could not commit and the open step's.
    let steps = record.steps.terminal_rows();
    let event = Event {
        seq,
        session_id: &started.session,
        turn: Some(started.turn.get()),
        late: false,
        at: &ended_at,
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
    let envelope = turn_envelope(
        (&started.session, started.turn),
        terminal,
        record.accepted,
        (started.cwd.clone(), started.folder.clone()),
        (timestamps, duration_ms),
        (started.first_seq, seq),
        record.vendor,
    );
    let envelope = serde_json::to_value(&envelope).map_err(|_| ApiError::STORE)?;
    Ok(TerminalRecord {
        session_id: started.session.clone(),
        turn: started.turn,
        envelope,
        event,
        steps,
    })
}

/// Design §7.2 row 5: the turn's first write that did not commit stops it
/// with cause `store`, once; later events are dropped and its terminal is
/// the resolution write. An uncertain one latches instead, and the latch's
/// force stops the turn. A token count the tracker refused stops it with
/// cause `protocol`, once (review r1).
fn stop_for_store(record: &TurnRecord, control: &mut Control<'_>) {
    if !control.refused && record.steps.unrepresentable() {
        // Review r1: the vendor reported a token count Store cannot hold.
        control.refused = true;
        control
            .slot
            .protocol_order(control.turn, tokio::time::Instant::now());
    }
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

/// Awaits `commit` while polling the adapter's `execute` (design §9): a
/// result it returns meanwhile is kept in `early` with the session
/// channel's count at that first `Ready`, read by `delivered` (critical r2
/// F2: what the driver delivered by its return is the final drain's), and
/// a completed `execute` is never polled again.
async fn while_polling<E, F>(
    (execute, early): (&mut E, &mut Option<(E::Output, usize)>),
    delivered: impl Fn() -> usize,
    commit: F,
) -> F::Output
where
    E: std::future::Future + Unpin,
    F: std::future::Future,
{
    let mut commit = std::pin::pin!(commit);
    if early.is_none() {
        tokio::select! {
            biased;
            output = &mut commit => return output,
            result = &mut *execute => {
                *early = Some((result, delivered()));
                // Test builds: the driver's turn returned.
                #[cfg(feature = "test-failpoints")]
                let _ = via_store::failpoint::hit_async("core.run.returned").await;
            }
        }
    }
    commit.await
}

/// Moves the running turn's idle deadline for `item`, the turn's own
/// meaningful progress ([`current_progress`]) decoded before it, to the
/// item's own `at` plus the idle budget (critical r2 F1): progress counts
/// when it was decoded, not when Core reads it.
fn note_progress(lane: &Lane, control: &mut Control<'_>, item: &ObservationItem) {
    if let Some(idle_at) = control.idle_at.as_mut()
        && item.at < *idle_at
        && current_progress(lane, control.turn, item)
    {
        *idle_at = (*idle_at).max(item.at + control.idle);
    }
}

/// Meaningful progress resets the idle deadline (Task 4 design §2.6):
/// acceptance, and a `progress` item with a `model` mark or a tool start or
/// end. Usage-only items never do; unknown messages send no item.
fn progress(observation: &Observation) -> bool {
    match observation {
        Observation::Accepted(_) => true,
        Observation::Progress(marks) => {
            marks.model || !marks.tools_started.is_empty() || !marks.tools_ended.is_empty()
        }
        Observation::IdentityConfirmed(_)
        | Observation::FinalText(_)
        | Observation::ActionDenied(_)
        | Observation::RequestDeclined(_)
        | Observation::SteerDelivered(_)
        | Observation::Warning(_)
        | Observation::VendorClosed(_)
        | Observation::ResumeMismatch { .. }
        | Observation::LateTerminal(_) => false,
    }
}

/// Maps an acceptance's vendor turn on the lane ([`Lane::map_vendor_turn`]).
/// Sol r3 N6, critical r1 #6 (C2 §4.1): a collision or the tombstones'
/// exhaustion failed the lane; the turn stops at once, once, and an
/// exhausted one fails `overflow` ([`refused_evidence`]).
fn map_acceptance(
    record: &mut TurnRecord,
    lane: Option<&Lane>,
    control: &mut Control<'_>,
    vendor_turn: Option<String>,
) -> Mapped {
    let mapped = match (lane, vendor_turn) {
        (Some(lane), Some(vendor_turn)) => lane.map_vendor_turn(&vendor_turn, record.turn),
        _ => Mapped::Taken,
    };
    if mapped != Mapped::Taken && !control.refused {
        control.refused = true;
        control
            .slot
            .protocol_order(control.turn, tokio::time::Instant::now());
    }
    record.vendor.overflowed |= mapped == Mapped::Exhausted;
    mapped
}

/// The failure of vendor evidence Core refused, once the turn's record
/// shows it: a token count (refused after `execute` returned, when no
/// order could reach it), or an acceptance that exhausted the lane's
/// tombstones, whose protocol order stopped the turn (critical r1 #6).
fn refused_evidence(record: &TurnRecord, terminal: &mut Terminal) {
    if record.steps.unrepresentable() {
        terminal.fail(FailureClass::Protocol, super::terminal::TOKENS_STOP);
    } else if record.vendor.overflowed {
        terminal.fail(FailureClass::Overflow, super::terminal::OVERFLOW_STOP);
    }
}

/// Whether `item` is meaningful progress of the running `turn` on `lane`,
/// which resets the turn's idle deadline (critical r1 #7): its acceptance,
/// the turn's by its correlation, or progress `lane` attributes to the
/// turn. Late, expired and session-level progress never does.
pub(super) fn current_progress(lane: &Lane, turn: TurnNumber, item: &ObservationItem) -> bool {
    progress(&item.observation)
        && (matches!(item.observation, Observation::Accepted(_))
            || lane.attribute(
                item.vendor_turn
                    .as_ref()
                    .map(via_adapters::VendorTurnId::as_str),
                Some(turn),
            ) == Attribution::Current)
}

/// The vendor turn an acceptance names: its own vendor turn ID, else the
/// item's.
fn acceptance_turn(acceptance: &Acceptance, item: Option<&str>) -> Option<String> {
    acceptance
        .vendor_turn_id
        .as_ref()
        .map(|id| id.as_str().to_owned())
        .or_else(|| item.map(str::to_owned))
}

/// `action.denied`'s body for `denial` (C1 §6.1).
fn denial_body(denial: &Denial) -> EventBody {
    EventBody::ActionDenied {
        kind: denial_kind(denial.kind),
        target: denial.target.clone(),
        reason: denial.reason.clone(),
    }
}

/// `vendor.request_declined`'s body for `decline` (C1 §6.1).
fn decline_body(decline: &Decline) -> EventBody {
    EventBody::RequestDeclined {
        vendor_method: decline.vendor_method.clone(),
        summary: decline.summary.clone(),
        blocking: decline.blocking,
    }
}

/// The event a durable session observation other than an identity
/// commits (C1 §6.1): a denial, a decline or an adapter warning; else
/// none.
pub(super) fn held_body(observation: &Observation) -> Option<EventBody> {
    match observation {
        Observation::ActionDenied(denial) => Some(denial_body(denial)),
        Observation::RequestDeclined(decline) => Some(decline_body(decline)),
        Observation::Warning(warning) => Some(EventBody::warning(
            warning.code,
            &warning.message,
            warning.data.clone(),
        )),
        Observation::Accepted(_)
        | Observation::IdentityConfirmed(_)
        | Observation::Progress(_)
        | Observation::FinalText(_)
        | Observation::SteerDelivered(_)
        | Observation::VendorClosed(_)
        | Observation::ResumeMismatch { .. }
        | Observation::LateTerminal(_) => None,
    }
}

/// C1 §5's `action.denied` kind word.
const fn denial_kind(kind: DenialKind) -> &'static str {
    match kind {
        DenialKind::FileWrite => "file_write",
        DenialKind::Command => "command",
        DenialKind::Network => "network",
        DenialKind::Other => "other",
    }
}

/// A submitted turn's record, its activity clock and its `Running` entry
/// (Task 4 design §2.4): the published progress and the activity clock
/// share the step tracker's clock.
fn start_turn(
    slot: &Slot,
    session: &SessionId,
    turn: TurnNumber,
    wall: tokio::time::Instant,
) -> (
    TurnRecord,
    TurnActivity,
    (StopWatch, watch::Receiver<Option<StopOrder>>),
) {
    let record = new_record(slot, session, turn);
    let clock = record.steps.clock();
    let activity = TurnActivity::new(clock.base());
    let running = slot.start_running(
        turn,
        wall,
        Progress::new(turn.get(), clock, activity.clone()),
    );
    (record, activity, running)
}

/// A turn's record before its first event: it writes at the slot's head.
fn new_record(slot: &Slot, session: &SessionId, turn: TurnNumber) -> TurnRecord {
    TurnRecord {
        session: session.clone(),
        turn,
        head: Arc::clone(&slot.head),
        accepted: None,
        first_failure: None,
        uncertain: None,
        steps: super::progress::StepTracker::default(),
        vendor: super::lane::VendorRecord::default(),
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
        cwd: queued.cwd,
        submitted: None,
        folder: None,
    };
    let record = new_record(slot, session, turn);
    let terminal = Terminal {
        state: "cancelled",
        failure: None,
        stop_reason: "interrupted",
        vendor_stop_reason: None,
        final_text: Some(String::new()),
        final_text_file: None,
        exit: None,
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
    };
    (started, record, terminal, extras)
}
