//! Turn driving: submission, adapter execution, event commits and the terminal commit.

use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant, SystemTime},
};

use tokio::sync::mpsc;
use via_adapters::{
    AdapterError, FakeAcceptanceObservation, FakeObservation, FakeTerminalEvidence, Observation,
    RouteError, ToolStatus, WireCleanup,
};
use via_store::{AcceptanceRecord, SubmissionRecord, TerminalRecord};

use super::journal::{self, TurnJournal, UncertainEvent, Unresolved};
use super::stop::stop_outcome;
use super::terminal::{classify, terminal_envelope};
use super::{Accepted, Engine, ForcedTurn, RouteClose, Started, Terminal, TurnRecord, lock};
use crate::api::{
    Effective, Event, EventBody, FAKE_WALL_MS, FailureClass, RawSpan, Timestamps, Warning, rfc3339,
};
use crate::{ApiError, ConnectionId, Deadline, RawRef, SessionId, TurnNumber, TurnState};

/// Reason recorded on `session.closed` for a `daemon/stop --force` (C1 §7.1).
const FORCE_CLOSE_REASON: &str = "daemon_stop_force";

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
    /// Continues independently of the client connection after the committed receipt.
    pub async fn drive(&self, session_text: &str, prompt: String) -> Result<(), ApiError> {
        struct Active<'a>(&'a AtomicUsize);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _active = Active(&self.active);
        let session = SessionId::try_from(session_text).map_err(|_| ApiError::INVALID_PARAMS)?;
        let turn = TurnNumber::try_from(1).map_err(|_| ApiError::STORE)?;
        let (queued_at, submitted, submitted_clock) =
            Self::submit(&self.store, &self.unresolved, &session, turn).await?;
        let started = Started {
            session: session.clone(),
            turn,
            queued_at,
            submitted_at: rfc3339(submitted),
            submitted_clock,
        };
        let connection = ConnectionId::try_from(
            format!("c_{}", session.as_str().trim_start_matches("s_")).as_str(),
        )
        .map_err(|_| ApiError::STORE)?;
        let mut record = TurnRecord {
            session: session.clone(),
            turn,
            seq: 2,
            accepted: None,
            spans: Vec::new(),
            store_failed: false,
            uncertain: None,
        };
        let wall = Duration::from_millis(FAKE_WALL_MS);
        let deadline = Deadline::at(tokio::time::Instant::now() + wall);
        let deadline_at = rfc3339(SystemTime::now() + wall);
        let outcome = match self
            .execute(&mut record, connection.clone(), prompt, deadline)
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
        let seq = record.seq + 1;
        let ended_at = rfc3339(SystemTime::now());
        // Monotonic, so wall-clock steps cannot distort or drop the duration.
        let elapsed = started.submitted_clock.elapsed();
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
            submitted_at: Some(started.submitted_at.clone()),
            accepted_at: record.accepted.as_ref().map(|accepted| accepted.at.clone()),
            ended_at,
        };
        let duration_ms = u64::try_from(elapsed.as_millis()).ok();
        let envelope = terminal_envelope(
            &started.session,
            started.turn,
            terminal,
            record.accepted,
            record.spans,
            timestamps,
            duration_ms,
            seq,
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
        journal::commit_terminal(
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
        .await
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
                match self
                    .accept(&record.session, record.turn, record.seq + 1, observation)
                    .await
                {
                    Ok(accepted) => {
                        record.seq += 1;
                        RawSpan::include(&mut record.spans, &accepted.raw_ref);
                        record.accepted = Some(accepted);
                    }
                    Err(uncertain) => {
                        record.store_failed = true;
                        record.uncertain = uncertain.map(|accepted| UncertainEvent {
                            seq: record.seq + 1,
                            raw_ref: Some(accepted.raw_ref.clone()),
                            accepted: Some(accepted),
                        });
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

    /// Commits submission intent with `turn.submitted` (seq 2) before any agent I/O.
    /// A turn whose submission cannot be confirmed never reaches `finish`; it is
    /// recorded failed at its last committed state, `queued`.
    ///
    /// Returns the durable `turn.queued` time and the submission time.
    pub(super) async fn submit(
        journal: &impl TurnJournal,
        unresolved: &Unresolved,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<(String, SystemTime, Instant), ApiError> {
        let submitted = Self::commit_submission(journal, session, turn).await;
        if submitted.is_err() {
            unresolved.fail(session, turn, TurnState::Queued);
        }
        // Submission intent is durable and no agent I/O has happened yet.
        #[cfg(feature = "test-failpoints")]
        if submitted.is_ok()
            && via_store::failpoint::hit_async("core.intent.after_commit")
                .await
                .is_err()
        {
            unresolved.fail(session, turn, TurnState::Running);
            return Err(ApiError::STORE);
        }
        submitted
    }

    /// `submit`'s Store work: reads `turn.queued` and commits `turn.submitted`.
    async fn commit_submission(
        journal: &impl TurnJournal,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<(String, SystemTime, Instant), ApiError> {
        // S1 sessions hold one turn, so its events start at seq 1 (turn.queued).
        let queued = journal
            .events(session, 1, 1)
            .await
            .map_err(|_| ApiError::STORE)?;
        let queued_at = queued
            .first()
            .and_then(|event| event.event.get("at")?.as_str().map(str::to_owned))
            .ok_or(ApiError::STORE)?;
        let submitted = SystemTime::now();
        let submitted_clock = Instant::now();
        let event = Event {
            seq: 2,
            session_id: session,
            turn: Some(turn.get()),
            late: false,
            at: &rfc3339(submitted),
            raw_ref: None,
            body: EventBody::TurnSubmitted { attempt: 1 },
        }
        .to_value()?;
        journal
            .commit_submission(SubmissionRecord {
                session_id: session.clone(),
                turn,
                event,
            })
            .await
            .map_err(|_| ApiError::STORE)?;
        Ok((queued_at, submitted, submitted_clock))
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
