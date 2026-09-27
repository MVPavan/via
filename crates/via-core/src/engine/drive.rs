//! Turn driving: submission, adapter execution, event commits and the terminal commit.

use std::{
    sync::{
        Arc,
        atomic::Ordering,
    },
    time::{Duration, Instant, SystemTime},
};

use tokio::sync::{mpsc, watch};
use via_adapters::{
    AdapterError, FakeAcceptanceObservation, FakeObservation, FakeTerminalEvidence, Observation,
    RouteError, ToolStatus, WireCleanup,
};
use via_store::{AcceptanceRecord, QueuedTurn, SubmissionRecord, TerminalRecord};

use super::journal::{self, Head, TurnJournal, UncertainEvent, Unresolved};
use super::queue::{Backoff, Slot};
use super::stop::{StopMode, stop_outcome};
use super::terminal::{classify, terminal_envelope};
use super::{Accepted, Engine, ForcedTurn, RouteClose, Started, Terminal, TurnRecord, lock};
use crate::api::{
    Effective, Event, EventBody, FAKE_WALL_MS, FailureClass, RawSpan, Timestamps, Warning, rfc3339,
};
use crate::{ApiError, ConnectionId, Deadline, RawRef, SessionId, TurnNumber, TurnState};

/// Reason recorded on `session.closed` for a `daemon/stop --force` (C1 §7.1).
const FORCE_CLOSE_REASON: &str = "daemon_stop_force";

/// A committed submission: the queued turn's facts and the submission time.
pub(super) struct Submission {
    session: SessionId,
    turn: TurnNumber,
    queued: QueuedTurn,
    submitted: SystemTime,
    clock: Instant,
}

/// Why a granted turn's submission did not commit.
pub(super) enum SubmitFailure {
    /// A read before the commit failed; nothing was written.
    Unread,
    /// The commit failed or its outcome is unknown: Store failure latches.
    Failed,
}

/// A queued turn's dispatch decision from its predecessors' durable state.
enum Decision {
    Run,
    Cancel,
    /// Not yet: an earlier turn is unresolved or Store could not be read.
    Wait,
}

/// What the dispatcher does after one step.
enum Step {
    /// Decide again at once.
    Next,
    /// Wait for a wake or the read-retry timer.
    Wait,
}

/// How a drive's execution ended.
enum Driven {
    /// The adapter returned its outcome.
    Finished(Result<FakeTerminalEvidence, AdapterError>),
    /// A force stop closed the execution through Route.
    Forced {
        requested_at: String,
        /// Route's cleanup drain could not record every vendor byte.
        raw_incomplete: bool,
        /// A vendor may have launched: Host sent ARM.
        launched: bool,
        /// Route's own Host close evidence.
        close: RouteClose,
    },
}

impl Engine {
    /// The session's dispatcher (design §2), run by daemon main as one task per
    /// session. It owns the session's queued turns and decides each queue head
    /// from durable state; only events and a read-retry timer wake it. A
    /// submitted turn runs inline, so at most one turn of the session runs. It
    /// returns once nothing is queued, or once a force stop or a Store failure
    /// settled the queue.
    pub async fn dispatcher(&self, session: SessionId) -> Result<(), ApiError> {
        let slot = self.slot(&session).ok_or(ApiError::STORE)?;
        slot.live();
        let mut force = self.force.subscribe();
        let mut backoff = Backoff::new();
        loop {
            if *force.borrow() {
                self.force_queue(&slot, &session).await;
                slot.stop();
                return Ok(());
            }
            let Some(turn) = slot.front() else {
                if self.exit(&session, &slot).await {
                    return Ok(());
                }
                continue;
            };
            let step = match self.decide(&session, turn).await {
                Decision::Run => self.dispatch(&slot, &session, turn).await,
                Decision::Cancel => self.cancel_queued(&slot, &session, turn, false).await,
                Decision::Wait => Step::Wait,
            };
            match step {
                Step::Next => backoff.reset(),
                Step::Wait => Self::await_wake(&slot, &mut force, backoff.next()).await,
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
        let _admission = self.admission.lock().await;
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
    fn grant(&self) -> bool {
        *lock(&self.stop) != Some(StopMode::Force) && !self.store_failed()
    }

    /// Grants, submits and runs the queue head. It keeps its queued count
    /// until Store confirms the submission. A failed or uncertain submission
    /// latches Store failure and the turn stays queued with no vendor I/O.
    async fn dispatch(&self, slot: &Slot, session: &SessionId, turn: TurnNumber) -> Step {
        if !self.grant() {
            return Step::Next;
        }
        #[cfg(test)]
        if self.faults.hold_after_grant.load(Ordering::Acquire) {
            self.faults.granted.notify_one();
            self.faults.release.notified().await;
        }
        let submission = match self.submit(slot, session, turn).await {
            Ok(submission) => submission,
            Err(SubmitFailure::Unread) => return Step::Wait,
            Err(SubmitFailure::Failed) => {
                self.latch();
                return Step::Next;
            }
        };
        slot.pop(turn);
        self.queued.fetch_sub(1, Ordering::AcqRel);
        self.run(slot, submission).await;
        self.active.fetch_sub(1, Ordering::AcqRel);
        Step::Next
    }

    /// Under force (design §2.3): commits every queued turn `queued →
    /// cancelled` without submission. `session.closed` rides on the last one
    /// only when every other turn of the session is durably settled. After a
    /// Store failure nothing is written: each queued turn stays `queued` and
    /// unresolved, reading `store_error`.
    async fn force_queue(&self, slot: &Slot, session: &SessionId) {
        let turns = slot.queued();
        let last = turns.last().copied();
        for turn in turns {
            if self.store_failed() {
                self.unresolved.fail(session, turn, TurnState::Queued);
                continue;
            }
            let close = Some(turn) == last && !self.unresolved.others(session, turn);
            if matches!(
                self.cancel_queued(slot, session, turn, close).await,
                Step::Wait
            ) {
                // Only a read failed: nothing was written, but no retry under force.
                self.unresolved.fail(session, turn, TurnState::Queued);
            }
        }
    }

    /// C1 P6/§7.3 from durable state. While any earlier turn is unresolved
    /// (queued, including an orphan awaiting reconciliation, or running, or
    /// with no durable terminal) the turn waits; so does it when Store cannot
    /// answer. Otherwise the latest submitted earlier turn decides: durably
    /// `unknown` or cleanup `pending` cancels, anything else runs. Turns
    /// cancelled while queued never ran and are passed over.
    async fn decide(&self, session: &SessionId, turn: TurnNumber) -> Decision {
        let Ok(predecessors) = self.predecessors(session, turn).await else {
            return Decision::Wait;
        };
        if predecessors.unresolved {
            return Decision::Wait;
        }
        match predecessors.last_submitted {
            Some(envelope)
                if envelope["state"] == "unknown" || envelope["cancel"]["cleanup"] == "pending" =>
            {
                Decision::Cancel
            }
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
                return Err(via_store::StoreError::Unavailable);
            }
        }
        self.store.predecessors(session, turn).await
    }

    /// Executes a submitted turn to its terminal, or hands it to final shutdown
    /// after a force stop.
    async fn run(&self, slot: &Slot, submission: Submission) {
        let Submission {
            session,
            turn,
            queued,
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
        // One private connection per turn; turn 1 keeps the session's own name.
        let suffix = session.as_str().trim_start_matches("s_");
        let connection = match turn.get() {
            1 => format!("c_{suffix}"),
            n => format!("c_{suffix}t{n}"),
        };
        let Ok(connection) = ConnectionId::try_from(connection.as_str()) else {
            self.unresolved.fail(&session, turn, TurnState::Running);
            return;
        };
        let mut record = TurnRecord {
            session: session.clone(),
            turn,
            head: Arc::clone(&slot.head),
            accepted: None,
            spans: Vec::new(),
            store_failed: false,
            uncertain: None,
        };
        let wall = Duration::from_millis(FAKE_WALL_MS);
        let deadline = Deadline::at(tokio::time::Instant::now() + wall);
        let deadline_at = rfc3339(SystemTime::now() + wall);
        let outcome = match self
            .execute(&mut record, connection.clone(), queued.prompt, deadline)
            .await
        {
            Driven::Finished(outcome) => outcome,
            Driven::Forced {
                requested_at,
                raw_incomplete,
                launched,
                close,
            } => {
                if raw_incomplete {
                    let body = EventBody::RawLogIncomplete {
                        connection_id: connection,
                    };
                    self.commit_event(&mut record, body, None).await;
                }
                self.commit_event(&mut record, EventBody::CancelRequested {}, None)
                    .await;
                // Final shutdown commits the cancelled terminal once Host has evidence.
                lock(&self.forced).push(ForcedTurn {
                    started,
                    record,
                    requested_at,
                    raw_incomplete,
                    launched,
                    close,
                });
                return;
            }
        };
        // C1 §7.6: Core's deadline cancels the turn; Route force-closed its group.
        let deadline_stop = match &outcome {
            Err(AdapterError::Route(route))
                if matches!(route.cause, RouteError::Deadline { .. }) =>
            {
                let quiescent = route.cleanup == Some(WireCleanup::Quiescent);
                Some(stop_outcome(quiescent, route.forced))
            }
            _ => None,
        };
        let mut terminal = classify(record.accepted.is_some(), outcome);
        if terminal.raw_incomplete {
            let body = EventBody::RawLogIncomplete {
                connection_id: connection,
            };
            self.commit_event(&mut record, body, None).await;
            terminal.warnings.push(Warning::RAW_LOG_INCOMPLETE);
        }
        if let Some((outcome, cleanup)) = deadline_stop {
            self.commit_event(&mut record, EventBody::CancelRequested {}, None)
                .await;
            terminal.cancel = Some(
                self.settle(&mut record, deadline_at, outcome, cleanup)
                    .await,
            );
        }
        if record.store_failed {
            // Acceptance or an observation could not be recorded after dispatch.
            terminal.fail(FailureClass::Store, "a turn event could not be recorded");
        }
        // A terminal that did not commit reads `store_error` and latches.
        let _ = self.finish(&started, record, terminal, false).await;
    }

    /// Commits a never-submitted turn `queued → cancelled` (C1 §7.2), behind
    /// an `unknown` predecessor or under force; no vendor I/O happened. With
    /// `close_session`, `session.closed` commits in the same transaction.
    /// Once committed the turn leaves the queue. A failed or uncertain commit
    /// latches Store failure; a failed read before it only waits.
    async fn cancel_queued(
        &self,
        slot: &Slot,
        session: &SessionId,
        turn: TurnNumber,
        close_session: bool,
    ) -> Step {
        let Ok(Some(queued)) = self.store.queued_turn(session, turn).await else {
            return Step::Wait;
        };
        // Settle an unknown head now, so the commit below reads nothing.
        if slot.head.lock(&self.store, session).await.is_err() {
            return Step::Wait;
        }
        let started = Started {
            session: session.clone(),
            turn,
            queued_at: queued.queued_at,
            first_seq: queued.queued_seq,
            submitted: None,
        };
        let record = TurnRecord {
            session: session.clone(),
            turn,
            head: Arc::clone(&slot.head),
            accepted: None,
            spans: Vec::new(),
            store_failed: false,
            uncertain: None,
        };
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
            cancel: None,
        };
        #[cfg(test)]
        let injected = self
            .faults
            .cancel_fails
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        #[cfg(not(test))]
        let injected = false;
        let committed = !injected
            && Self::commit_turn_ended(&self.store, &started, record, terminal, close_session)
                .await
                .is_ok();
        if committed {
            slot.pop(turn);
            self.unresolved.resolve(session, turn);
            self.queued.fetch_sub(1, Ordering::AcqRel);
            self.active.fetch_sub(1, Ordering::AcqRel);
        } else {
            self.unresolved.fail(session, turn, TurnState::Queued);
            self.latch();
        }
        Step::Next
    }

    /// Commits the turn's terminal; one that cannot be made durable is recorded so
    /// that reads report `store_error` instead of a running turn. With
    /// `close_session`, `session.closed` commits in the same transaction.
    pub(super) async fn finish(
        &self,
        started: &Started,
        record: TurnRecord,
        terminal: Terminal,
        close_session: bool,
    ) -> Result<(), ApiError> {
        let finished = Self::finish_turn(
            &self.store,
            &self.unresolved,
            started,
            record,
            terminal,
            close_session,
        )
        .await;
        if finished.is_err() {
            self.latch();
        }
        finished
    }

    /// `finish` over any journal, so the Store/Core boundary is testable.
    pub(super) async fn finish_turn(
        journal: &impl TurnJournal,
        unresolved: &Unresolved,
        started: &Started,
        record: TurnRecord,
        terminal: Terminal,
        close_session: bool,
    ) -> Result<(), ApiError> {
        let committed =
            Self::commit_turn_ended(journal, started, record, terminal, close_session).await;
        match committed {
            Ok(()) => unresolved.resolve(&started.session, started.turn),
            Err(_) => unresolved.fail(&started.session, started.turn, TurnState::Running),
        }
        committed
    }

    /// Commits `turn.ended` at the sequence after every event `record` committed,
    /// with the terminal envelope whose raw spans bound every committed reference.
    /// An uncertain event commit is settled against the durable head first. With
    /// `close_session`, `session.closed` follows in the same transaction.
    pub(super) async fn commit_turn_ended(
        journal: &impl TurnJournal,
        started: &Started,
        mut record: TurnRecord,
        terminal: Terminal,
        close_session: bool,
    ) -> Result<(), ApiError> {
        journal::reconcile(journal, &mut record)
            .await
            .map_err(|_| ApiError::STORE)?;
        if let Some(reference) = &terminal.raw_ref {
            RawSpan::include(&mut record.spans, reference);
        }
        let shared = Arc::clone(&record.head);
        let head = shared
            .lock(journal, &started.session)
            .await
            .map_err(|_| ApiError::STORE)?;
        let seq = head.next();
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
        let events = if closed.is_some() { 2 } else { 1 };
        let committed = journal::commit_terminal(
            journal,
            TerminalRecord {
                session_id: started.session.clone(),
                turn: started.turn,
                envelope,
                event,
                raw_ref,
            },
            closed,
        )
        .await;
        match committed {
            Ok(()) => head.committed(events),
            // Durable or not, re-read the head before the session's next event.
            Err(_) => head.lost(),
        }
        committed
    }

    /// Drives the adapter under the turn deadline, committing each observation it
    /// reports in decode order before the adapter outcome is returned. A force stop
    /// reaches Route, which force-closes the group and drains its output first:
    /// frames it read still commit, and the raw log is complete or reported not.
    async fn execute(
        &self,
        record: &mut TurnRecord,
        connection: ConnectionId,
        prompt: String,
        deadline: Deadline,
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
            self.force.subscribe(),
        ));
        // No branch is cancelled mid-commit: an observation arm runs to completion
        // before the next poll, and the adapter's own sends wait for capacity.
        loop {
            tokio::select! {
                Some(observation) = observed_rx.recv() => {
                    self.observe(record, observation).await;
                }
                result = &mut execute => {
                    while let Ok(observation) = observed_rx.try_recv() {
                        self.observe(record, observation).await;
                    }
                    return match result {
                        Err(AdapterError::Route(route))
                            if matches!(route.cause, RouteError::ForceStopped { .. }) =>
                        {
                            Driven::Forced {
                                requested_at: self
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
                            }
                        }
                        result => Driven::Finished(result),
                    };
                }
            }
        }
    }

    /// Commits one adapter observation at the next sequence, in decode order.
    /// After the first Store failure the rest are dropped and the turn fails `store`.
    async fn observe(&self, record: &mut TurnRecord, observation: FakeObservation) {
        match observation {
            FakeObservation::Accepted(observation) => {
                // Route admits one acceptance; a repeat would be deduplicated anyway.
                if record.store_failed || record.accepted.is_some() {
                    return;
                }
                let shared = Arc::clone(&record.head);
                let Ok(head) = shared.lock(&self.store, &record.session).await else {
                    record.store_failed = true;
                    self.latch();
                    return;
                };
                let seq = head.next();
                match self
                    .accept(&record.session, record.turn, seq, observation)
                    .await
                {
                    Ok(accepted) => {
                        head.committed(1);
                        RawSpan::include(&mut record.spans, &accepted.raw_ref);
                        record.accepted = Some(accepted);
                    }
                    Err(uncertain) => {
                        record.store_failed = true;
                        self.latch();
                        if let Some(accepted) = uncertain {
                            head.lost();
                            record.uncertain = Some(UncertainEvent {
                                seq,
                                raw_ref: Some(accepted.raw_ref.clone()),
                                accepted: Some(accepted),
                            });
                        }
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
        journal::commit_event(&self.store, record, body, raw_ref).await;
        if record.store_failed {
            self.latch();
        }
    }

    /// Commits submission intent with `turn.submitted` before any agent I/O;
    /// the test fault backend can lose its reply.
    async fn submit(
        &self,
        slot: &Slot,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Submission, SubmitFailure> {
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
            return Err(SubmitFailure::Failed);
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
            slot.pop(turn);
            self.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(SubmitFailure::Failed);
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
        let Ok(Some(queued)) = journal.queued_turn(session, turn).await else {
            return Err(SubmitFailure::Unread);
        };
        let Ok(head) = head.lock(journal, session).await else {
            return Err(SubmitFailure::Unread);
        };
        let submitted = SystemTime::now();
        let clock = Instant::now();
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
        .map_err(|_| SubmitFailure::Failed)?;
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
                if journal::may_have_committed(&error) {
                    head.lost();
                }
                return Err(SubmitFailure::Failed);
            }
        }
        Ok(Submission {
            session: session.clone(),
            turn,
            queued,
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
                effective: Effective::fake("fake"),
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
