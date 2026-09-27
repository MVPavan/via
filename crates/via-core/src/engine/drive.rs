//! Turn driving: submission, adapter execution, event commits and the terminal commit.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use tokio::sync::mpsc;
use via_adapters::{
    AdapterError, FakeAcceptanceObservation, FakeObservation, FakeTerminalEvidence, Observation,
    RouteError, ToolStatus, WireCleanup,
};
use via_store::{AcceptanceRecord, QueuedTurn, SubmissionRecord, TerminalRecord};

use super::journal::{self, Head, TurnJournal, UncertainEvent, Unresolved};
use super::queue::{Finish, Slot};
use super::stop::stop_outcome;
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

/// A queued turn's dispatch decision from its predecessors' durable state.
enum Dispatch {
    Run,
    Wait,
    Cancel,
}

/// Longest a waiting turn goes before re-reading its predecessors: bounds the
/// retry after a failed read and the wait for an orphan's reconciliation.
const DISPATCH_RECHECK: Duration = Duration::from_millis(250);

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

/// Counts a turn out of the daemon's queue once, when it leaves it or its drive ends.
struct Queued<'a>(Option<&'a AtomicUsize>);

impl Queued<'_> {
    fn leave(&mut self) {
        if let Some(queued) = self.0.take() {
            queued.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        self.leave();
    }
}

impl Engine {
    /// Continues independently of the client connection after the committed
    /// receipt. The turn waits for every earlier turn of its session (C1 §7.3),
    /// then submits; behind a predecessor that did not end cleanly it is
    /// cancelled without submission instead.
    pub async fn drive(&self, session: SessionId, turn: TurnNumber) -> Result<(), ApiError> {
        struct Active<'a>(&'a AtomicUsize);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _active = Active(&self.active);
        let mut queued = Queued(Some(&self.queued));
        let slot = self.slot(&session).ok_or(ApiError::STORE)?;
        slot.turn(turn).await;
        let _finish = Finish::new(&slot, turn);
        let mut changes = slot.subscribe();
        let mut force = self.force.subscribe();
        loop {
            let cancel = match self.dispatch(&session, turn).await {
                Dispatch::Run => break,
                Dispatch::Cancel => true,
                // `daemon/stop --force` closes the session: a waiting turn is cancelled.
                Dispatch::Wait => *force.borrow(),
            };
            if cancel {
                let cancelled = self.cancel_queued(&slot, &session, turn).await;
                queued.leave();
                return cancelled;
            }
            // An orphan predecessor may be committed; reconciling it lets it run.
            self.reconcile_orphans().await;
            tokio::select! {
                _ = changes.changed() => {}
                _ = force.wait_for(|forced| *forced) => {}
                () = tokio::time::sleep(DISPATCH_RECHECK) => {}
            }
        }
        let submission =
            Self::submit(&self.store, &self.unresolved, &session, turn, &slot.head).await;
        queued.leave();
        self.run(&slot, submission?).await
    }

    /// C1 P6/§7.3 from durable state. While any earlier turn is unresolved
    /// (queued, including an orphan awaiting reconciliation, or running, or
    /// with no durable terminal) the turn waits; so does it when Store cannot
    /// answer. Otherwise the latest submitted earlier turn decides: durably
    /// `unknown` or cleanup `pending` cancels, anything else runs. Turns
    /// cancelled while queued never ran and are passed over.
    async fn dispatch(&self, session: &SessionId, turn: TurnNumber) -> Dispatch {
        let Ok(predecessors) = self.predecessors(session, turn).await else {
            return Dispatch::Wait;
        };
        if predecessors.unresolved {
            return Dispatch::Wait;
        }
        match predecessors.last_submitted {
            Some(envelope)
                if envelope["state"] == "unknown" || envelope["cancel"]["cleanup"] == "pending" =>
            {
                Dispatch::Cancel
            }
            _ => Dispatch::Run,
        }
    }

    /// The Store read behind a dispatch decision; the test fault backend can fail it.
    async fn predecessors(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<via_store::Predecessors, via_store::StoreError> {
        #[cfg(test)]
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
        self.store.predecessors(session, turn).await
    }

    /// Executes a submitted turn to its terminal.
    async fn run(&self, slot: &Slot, submission: Submission) -> Result<(), ApiError> {
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
        let connection =
            ConnectionId::try_from(connection.as_str()).map_err(|_| ApiError::STORE)?;
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
                return Ok(());
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
        self.finish(&started, record, terminal, false).await
    }

    /// Cancels a turn that was never submitted because a predecessor is
    /// unknown or unresolved (C1 §7.2 `queued` → `cancelled`): no vendor I/O
    /// happened.
    async fn cancel_queued(
        &self,
        slot: &Slot,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<(), ApiError> {
        let queued = self.store.queued_turn(session, turn).await.ok().flatten();
        let Some(queued) = queued else {
            self.unresolved.fail(session, turn, TurnState::Queued);
            return Err(ApiError::STORE);
        };
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
        let committed =
            Self::commit_turn_ended(&self.store, &started, record, terminal, false).await;
        match committed {
            Ok(()) => self.unresolved.resolve(session, turn),
            Err(_) => self.unresolved.fail(session, turn, TurnState::Queued),
        }
        committed
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
        Self::finish_turn(
            &self.store,
            &self.unresolved,
            started,
            record,
            terminal,
            close_session,
        )
        .await
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
    }

    /// Commits submission intent with `turn.submitted` before any agent I/O.
    /// A turn whose submission cannot be confirmed never reaches `finish`; it is
    /// recorded failed at its last committed state, `queued`.
    pub(super) async fn submit(
        journal: &impl TurnJournal,
        unresolved: &Unresolved,
        session: &SessionId,
        turn: TurnNumber,
        head: &Head,
    ) -> Result<Submission, ApiError> {
        let submitted = Self::commit_submission(journal, session, turn, head).await;
        if submitted.is_err() {
            unresolved.fail(session, turn, TurnState::Queued);
        }
        submitted
    }

    /// `submit`'s Store work: reads the queued turn and commits `turn.submitted`
    /// at the session's next sequence.
    async fn commit_submission(
        journal: &impl TurnJournal,
        session: &SessionId,
        turn: TurnNumber,
        head: &Head,
    ) -> Result<Submission, ApiError> {
        let queued = journal
            .queued_turn(session, turn)
            .await
            .map_err(|_| ApiError::STORE)?
            .ok_or(ApiError::STORE)?;
        let head = head
            .lock(journal, session)
            .await
            .map_err(|_| ApiError::STORE)?;
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
        .to_value()?;
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
                return Err(ApiError::STORE);
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
