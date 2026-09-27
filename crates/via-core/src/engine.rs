//! Durable first-turn orchestration. Store decides persistence; Adapter owns vendor I/O.

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex as StdMutex, OnceLock, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::api::{
    Bound, Cancel, Capabilities, Cost, Effective, Envelope, Event, EventBody, EventRange, Exit,
    FAKE_WALL_MS, Failure, FailureClass, RawSpan, Receipt, Requested, RoutePlan, Timestamps, Usage,
    VendorFields, Warning, rfc3339,
};
use crate::{
    ApiError, ConnectionId, DaemonStopParams, Deadline, FakeConfig, RawRef, SessionId, SpawnParams,
    SteerParams, TurnNumber, hash_handle, parse_address,
};
use via_adapters::{
    AdapterError, AdapterRuntime, AdapterRuntimeConfig, Cleanup, FakeAcceptanceObservation,
    FakeObservation, FakeTerminalEvidence, Observation, RouteError, RuntimeConfig, ToolStatus,
    VendorTerminalStatus, WireCleanup,
};
use via_store::{
    AcceptanceRecord, SpawnRecord, Store, StoreClient, SubmissionRecord, TerminalRecord,
};

mod journal;

use journal::{TurnJournal, UncertainEvent, Unresolved};

/// One daemon's durable state and opaque vendor runtime.
pub struct Engine {
    _store_owner: Store,
    store: StoreClient,
    adapter: AdapterRuntime,
    active: AtomicUsize,
    admission: tokio::sync::Mutex<()>,
    /// Accepted `daemon/stop` mode; set under `admission`, never cleared.
    stop: StdMutex<Option<StopMode>>,
    /// Tells running drives to force-close their execution (C1 §3.14 `force`).
    force: watch::Sender<bool>,
    /// When the force stop was accepted: every forced turn's `requested_at`.
    force_requested_at: OnceLock<String>,
    /// Force-stopped turns awaiting Host cleanup evidence in final shutdown.
    forced: StdMutex<Vec<ForcedTurn>>,
    /// Receipted turns with no terminal known to have committed; a turn whose
    /// terminal could not be made durable reads as `store_error`.
    unresolved: Unresolved,
    /// Set once final shutdown committed its last record; nothing commits after.
    finalized: AtomicBool,
}

/// The C1 §3.14 stop mode Core accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopMode {
    /// No active work: final shutdown at once.
    Idle,
    /// Admission closed; accepted turns finish under their own deadlines first.
    Drain,
    /// Every running turn is closed with mode `force`; final shutdown at once.
    Force,
}

impl StopMode {
    /// The mode word used in the daemon's final shutdown summary.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Drain => "drain",
            Self::Force => "force",
        }
    }
}

/// Passive final-shutdown facts for the daemon's exit decision.
#[derive(Debug)]
pub struct EngineShutdown {
    /// Committed anchors Host reconciled.
    pub anchors: usize,
    /// Process owners whose group absence is unproved.
    pub uncertain_owners: usize,
    /// Host tasks whose result was not collected by the deadline.
    pub pending_tasks: usize,
    /// Host tasks that panicked, were cancelled or failed their child wait.
    pub failed_tasks: usize,
    /// Named Host deadline, Store or recovery failure.
    pub failure: Option<String>,
    /// Force-stopped turns whose cancelled terminal record did not commit.
    pub uncommitted_turns: usize,
    /// Receipted turns with no durable terminal record at final shutdown.
    pub unresolved_turns: usize,
}

impl EngineShutdown {
    /// Clean only with positive cleanup, every join collected and every record committed.
    pub fn is_clean(&self) -> bool {
        self.uncertain_owners == 0
            && self.pending_tasks == 0
            && self.failed_tasks == 0
            && self.failure.is_none()
            && self.uncommitted_turns == 0
            && self.unresolved_turns == 0
    }
}

/// Committed facts of a turn whose execution a force stop abandoned.
struct ForcedTurn {
    started: Started,
    record: TurnRecord,
    requested_at: String,
    /// Route's force cleanup could not record every vendor byte.
    raw_incomplete: bool,
}

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
    },
}

/// Durable facts established once a turn's submission committed.
struct Started {
    session: SessionId,
    turn: TurnNumber,
    queued_at: String,
    submitted_at: String,
    submitted_clock: Instant,
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Engine {
    /// Opens the sole Store owner and passes unopened lower resources to Adapter/Wire.
    pub fn open(
        state: &Path,
        runtime: &Path,
        fake: FakeConfig,
        binary: PathBuf,
    ) -> Result<Self, String> {
        let owner = Store::open(state).map_err(|error| error.to_string())?;
        let store = owner.client();
        let adapter = AdapterRuntime::new(
            AdapterRuntimeConfig {
                runtime: RuntimeConfig {
                    anchor_binary: binary,
                    anchor_dir: runtime.join("anchors"),
                },
                fake,
            },
            owner.runtime_resources(),
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            _store_owner: owner,
            store,
            adapter,
            active: AtomicUsize::new(0),
            admission: tokio::sync::Mutex::new(()),
            stop: StdMutex::new(None),
            force: watch::Sender::new(false),
            force_requested_at: OnceLock::new(),
            forced: StdMutex::new(Vec::new()),
            unresolved: Unresolved::default(),
            finalized: AtomicBool::new(false),
        })
    }

    /// Accepts a C1 §3.14 `daemon/stop` and closes admission to new work.
    ///
    /// A plain stop is refused while turns are active. A repeated request keeps
    /// the accepted mode, except that `force` escalates a drain.
    pub async fn request_stop(&self, params: &DaemonStopParams) -> Result<StopMode, ApiError> {
        if params.drain && params.force {
            return Err(ApiError::INVALID_PARAMS);
        }
        let _admission = self.admission.lock().await;
        let mut stop = lock(&self.stop);
        let requested = if params.force {
            StopMode::Force
        } else if params.drain {
            StopMode::Drain
        } else {
            StopMode::Idle
        };
        let mode = match *stop {
            Some(current) if requested != StopMode::Force => current,
            None if requested == StopMode::Idle && self.active() > 0 => {
                return Err(ApiError::SESSIONS_ACTIVE);
            }
            _ => requested,
        };
        *stop = Some(mode);
        if mode == StopMode::Force {
            self.force_requested_at
                .get_or_init(|| rfc3339(SystemTime::now()));
            self.force.send_replace(true);
        }
        Ok(mode)
    }

    /// The accepted `daemon/stop` mode, if any; daemon main reads it on notice.
    pub fn stop_mode(&self) -> Option<StopMode> {
        *lock(&self.stop)
    }

    /// Returns the number of receipted turns still being driven.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Commits a receipt before authorizing any process launch.
    pub async fn spawn(&self, params: SpawnParams) -> Result<(Value, String, String), ApiError> {
        let _admission = self.admission.lock().await;
        if lock(&self.stop).is_some() {
            return Err(ApiError::DAEMON_STOPPING);
        }
        if params.harness != "fake" || !self.adapter.fake_available() {
            return Err(ApiError::HARNESS_UNAVAILABLE);
        }
        if params.model != "fake" || params.prompt.is_empty() {
            return Err(ApiError::INVALID_PARAMS);
        }
        let hash = hash_handle(&params.handle)?;
        let turn = TurnNumber::try_from(1).map_err(|_| ApiError::STORE)?;
        let session = crate::api::new_session_id()?;
        let plan = RoutePlan::fake();
        let receipt = Receipt {
            session_id: session.clone(),
            turn: format!("{}/{}", session.as_str(), turn.get()),
            state: "queued",
            warnings: plan.warnings(),
            plan,
            capabilities: Capabilities::fake(),
            effective: Effective::fake(&params.model),
        };
        let receipt = serde_json::to_value(&receipt).map_err(|_| ApiError::STORE)?;
        let at = rfc3339(SystemTime::now());
        let initial_event = Event {
            seq: 1,
            session_id: &session,
            turn: Some(turn.get()),
            late: false,
            at: &at,
            raw_ref: None,
            body: EventBody::TurnQueued { queue_position: 0 },
        }
        .to_value()?;
        let stored = self
            .store
            .commit_spawn(SpawnRecord {
                session_id: session.clone(),
                handle_hash: hash,
                receipt,
                params: json!({"harness":"fake","model":"fake"}),
                prompt: params.prompt.clone(),
                initial_event,
            })
            .await
            .map_err(|_| ApiError::STORE)?;
        self.unresolved.receipt(&session, turn);
        self.active.fetch_add(1, Ordering::AcqRel);
        Ok((stored.receipt, session.as_str().to_owned(), params.prompt))
    }

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
        let (queued_at, submitted, submitted_clock) = self.submit(&session, turn).await?;
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

    /// Commits `cancel.settled` for a cancel Core requested at `requested_at`
    /// and returns the envelope's C1 §3.5 `cancel` object.
    async fn settle(
        &self,
        record: &mut TurnRecord,
        requested_at: String,
        outcome: &'static str,
        cleanup: &'static str,
    ) -> Cancel {
        self.commit_event(record, EventBody::CancelSettled { outcome, cleanup }, None)
            .await;
        Cancel {
            outcome,
            cleanup,
            requested_at,
            settled_at: rfc3339(SystemTime::now()),
        }
    }

    /// Commits the turn's terminal; one that cannot be made durable is recorded so
    /// that reads report `store_error` instead of a running turn. With
    /// `close_session`, `session.closed` commits in the same transaction.
    async fn finish(
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
    async fn finish_turn(
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
            Err(_) => unresolved.fail(&started.session, started.turn),
        }
        committed
    }

    /// Commits `turn.ended` at the sequence after every event `record` committed,
    /// with the terminal envelope whose raw spans bound every committed reference.
    /// An uncertain event commit is settled against the durable head first. With
    /// `close_session`, `session.closed` follows in the same transaction.
    async fn commit_turn_ended(
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
    async fn commit_event(
        &self,
        record: &mut TurnRecord,
        body: EventBody,
        raw_ref: Option<RawRef>,
    ) {
        journal::commit_event(&self.store, record, body, raw_ref).await;
    }

    /// Commits submission intent with `turn.submitted` (seq 2) before any agent I/O.
    ///
    /// Returns the durable `turn.queued` time and the submission time.
    async fn submit(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<(String, SystemTime, Instant), ApiError> {
        // S1 sessions hold one turn, so its events start at seq 1 (turn.queued).
        let queued = self
            .store
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
        self.store
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

    /// Final shutdown: Host closes live controls, reconciles every anchor and
    /// joins its tasks; then force-stopped turns commit their terminal with that
    /// evidence, and every receipted turn is checked for a durable terminal.
    /// Every step shares the caller's single absolute deadline.
    pub async fn shutdown(&self, deadline: Deadline) -> EngineShutdown {
        let report = self.adapter.shutdown(deadline).await;
        let forced = std::mem::take(&mut *lock(&self.forced));
        let mut uncommitted_turns = 0;
        for turn in forced {
            let evidence = report.recovery.iter().find(|record| {
                record.session_id == turn.started.session && record.turn == turn.started.turn
            });
            // C1 §7.6: `forced` only with Host force evidence and `quiescent` only
            // after verified group absence. A complete journal without an anchor
            // intent for the turn means nothing was launched: no vendor could
            // acknowledge, so the cancel stays `requested`, with nothing to clean.
            let (outcome, cleanup) = match evidence {
                Some(record) => stop_outcome(record.cleanup == Cleanup::Quiescent, record.forced),
                None if report.failure.is_none() => ("requested", "quiescent"),
                None => ("requested", "uncertain"),
            };
            let commit = async {
                let mut record = turn.record;
                let cancel = self
                    .settle(&mut record, turn.requested_at, outcome, cleanup)
                    .await;
                let mut terminal = Terminal {
                    state: "cancelled",
                    failure: None,
                    stop_reason: "interrupted",
                    vendor_stop_reason: None,
                    final_text: String::new(),
                    exit: None,
                    raw_ref: None,
                    raw_incomplete: false,
                    warnings: Vec::new(),
                    cancel: Some(cancel),
                };
                if turn.raw_incomplete {
                    // `raw_log.incomplete` committed when the drive ended.
                    terminal.warnings.push(Warning::RAW_LOG_INCOMPLETE);
                }
                if record.store_failed {
                    // C1 §8.2: the durable stream already lost an event; a
                    // cancellation must not present it as a complete record.
                    terminal.fail(FailureClass::Store, "a turn event could not be recorded");
                }
                self.finish(&turn.started, record, terminal, true).await
            };
            if !matches!(
                tokio::time::timeout_at(deadline.instant(), commit).await,
                Ok(Ok(()))
            ) {
                uncommitted_turns += 1;
            }
        }
        let unresolved_turns = self.unresolved_turns(deadline).await;
        self.finalized.store(true, Ordering::Release);
        EngineShutdown {
            anchors: report.recovery.len(),
            uncertain_owners: report
                .recovery
                .iter()
                .filter(|record| record.cleanup != Cleanup::Quiescent)
                .count(),
            pending_tasks: report.pending_tasks,
            failed_tasks: report.failed_tasks,
            failure: report.failure,
            uncommitted_turns,
            unresolved_turns,
        }
    }

    /// Counts receipted turns with no durable terminal, re-reading the Store for
    /// each one not known to have committed (a failed commit may still have).
    async fn unresolved_turns(&self, deadline: Deadline) -> usize {
        let turns = self.unresolved.turns();
        let mut unresolved = 0;
        for (session, turn) in turns {
            let read =
                tokio::time::timeout_at(deadline.instant(), self.store.result(&session, turn));
            if let Ok(Ok(Some(_))) = read.await {
                self.unresolved.resolve(&session, turn);
            } else {
                unresolved += 1;
            }
        }
        unresolved
    }

    /// Authenticates before reporting fake's unsupported mutation capability.
    pub async fn steer(&self, params: SteerParams) -> Result<Value, ApiError> {
        let hash = hash_handle(&params.handle)?;
        if !self
            .store
            .authenticate(&params.session, &hash)
            .await
            .map_err(|_| ApiError::STORE)?
        {
            return Err(ApiError::INVALID_HANDLE);
        }
        Err(ApiError::UNSUPPORTED_VERB)
    }

    /// Reads a committed terminal result without waiting.
    pub async fn result(&self, address: &str) -> Result<Value, ApiError> {
        let (session, turn) = parse_address(address)?;
        journal::read_result(&self.store, &self.unresolved, &session, turn)
            .await?
            .ok_or(ApiError::TURN_NOT_FINISHED)
    }

    /// Waits for a durable terminal result independently of client lifetime.
    ///
    /// Once final shutdown committed its last record, a result still missing
    /// can never commit in this daemon: the wait ends `daemon_stopping`.
    pub async fn wait(&self, address: &str) -> Result<Value, ApiError> {
        let (session, turn) = parse_address(address)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            // Read before the Store: a result committed before finalization is seen.
            let finalized = self.finalized.load(Ordering::Acquire);
            if let Some(result) =
                journal::read_result(&self.store, &self.unresolved, &session, turn).await?
            {
                return Ok(result);
            }
            if finalized {
                return Err(ApiError::DAEMON_STOPPING);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ApiError::WAIT_TIMEOUT);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Reads the first bounded page of durable canonical events.
    pub async fn events(&self, session: &str) -> Result<Value, ApiError> {
        let id = SessionId::try_from(session).map_err(|_| ApiError::INVALID_PARAMS)?;
        let events = self
            .store
            .events(&id, 1, 1000)
            .await
            .map_err(|_| ApiError::STORE)?;
        let next_after = events.last().map_or(0, |event| event.seq);
        Ok(
            json!({"events":events.into_iter().map(|event| event.event).collect::<Vec<_>>(),"next_after":next_after,"more":false}),
        )
    }

    /// Reads bounded raw excerpts referenced by committed events.
    pub async fn logs(&self, session: &str) -> Result<Value, ApiError> {
        let id = SessionId::try_from(session).map_err(|_| ApiError::INVALID_PARAMS)?;
        self.store.logs(&id).await.map_err(|_| ApiError::STORE)
    }
}

/// C1 §7.4 outcome and §3.5 cleanup of a stop Core ordered: `forced` needs
/// Host force evidence and proved group absence; otherwise the cancel was only
/// `requested`, and cleanup is `quiescent` only with proved absence.
fn stop_outcome(quiescent: bool, forced: bool) -> (&'static str, &'static str) {
    match (quiescent, forced) {
        (true, true) => ("forced", "quiescent"),
        (true, false) => ("requested", "quiescent"),
        (false, _) => ("requested", "uncertain"),
    }
}

/// Assembles the C1 §5 envelope; `last_seq` is `turn.ended`, and S1 turns start at seq 1.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is a distinct committed fact of the one turn"
)]
fn terminal_envelope(
    session: &SessionId,
    turn: TurnNumber,
    terminal: Terminal,
    accepted: Option<Accepted>,
    raw_spans: Vec<RawSpan>,
    timestamps: Timestamps,
    duration_ms: Option<u64>,
    last_seq: u64,
) -> Envelope {
    let plan = RoutePlan::fake();
    let mut warnings = plan.warnings();
    warnings.extend(terminal.warnings);
    if terminal
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.cleanup == "uncertain")
    {
        warnings.push(Warning::CANCEL_CLEANUP_UNCERTAIN);
    }
    Envelope {
        api_version: 1,
        session_id: session.clone(),
        turn: turn.get(),
        address: format!("{}/{}", session.as_str(), turn.get()),
        revision: 0,
        state: terminal.state,
        failure: terminal.failure,
        stop_reason: terminal.stop_reason,
        vendor_stop_reason: terminal.vendor_stop_reason,
        cancel: terminal.cancel,
        harness: "fake",
        model: Requested {
            requested: "fake".to_owned(),
            resolved: "fake".to_owned(),
        },
        effort: Requested {
            requested: None,
            resolved: None,
        },
        warnings,
        plan,
        vendor_session_id: None,
        cwd: None,
        bound: Bound::NONE,
        final_text: terminal.final_text,
        structured_output: None,
        denied_actions: [],
        auto_declined_requests: [],
        steps: None,
        usage: Usage::UNAVAILABLE,
        cost: Cost::UNAVAILABLE,
        timestamps,
        duration_ms,
        exit: terminal.exit,
        events: EventRange {
            first_seq: 1,
            last_seq,
            count: last_seq,
        },
        raw_spans,
        vendor_options: json!({}),
        vendor: VendorFields {
            turn_id: accepted.map(|accepted| accepted.vendor_turn_id),
        },
    }
}

/// Committed acceptance facts the envelope reports.
struct Accepted {
    at: String,
    raw_ref: RawRef,
    vendor_turn_id: String,
}

/// Durable progress of the one running turn: the last committed sequence and the
/// bounding raw spans of every event committed so far.
struct TurnRecord {
    session: SessionId,
    turn: TurnNumber,
    seq: u64,
    accepted: Option<Accepted>,
    spans: Vec<RawSpan>,
    store_failed: bool,
    /// The event commit Store left uncertain, settled before `turn.ended`.
    uncertain: Option<UncertainEvent>,
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

/// Core's terminal decision from adapter evidence (C1 §5, §8.2).
struct Terminal {
    state: &'static str,
    failure: Option<Failure>,
    stop_reason: &'static str,
    vendor_stop_reason: Option<String>,
    final_text: String,
    exit: Option<Exit>,
    raw_ref: Option<RawRef>,
    raw_incomplete: bool,
    warnings: Vec<Warning>,
    cancel: Option<Cancel>,
}

impl Terminal {
    /// Replaces the disposition with a Core-detected failure.
    fn fail(&mut self, class: FailureClass, message: &str) {
        self.state = "failed";
        self.failure = Some(failure(class, message.to_owned(), None));
        self.stop_reason = "error";
    }
}

/// Maps a typed route cause onto C1 §7.6 state, §8.2 class and stop reason.
fn route_disposition(cause: &RouteError) -> (&'static str, Option<FailureClass>, &'static str) {
    match cause {
        RouteError::Protocol { .. } => ("failed", Some(FailureClass::Protocol), "error"),
        RouteError::ProcessExited { .. } => ("failed", Some(FailureClass::ProcessExited), "error"),
        RouteError::Overflow { .. } => ("failed", Some(FailureClass::Overflow), "error"),
        RouteError::Store { .. } => ("failed", Some(FailureClass::Store), "error"),
        RouteError::Deadline { .. } => ("failed", Some(FailureClass::DeadlineWall), "deadline"),
        // Core settles a force stop itself; this is only the C1 §7.6 force row.
        RouteError::ForceStopped { .. } => ("cancelled", None, "interrupted"),
        // Input may have reached the vendor and no exit is confirmed (§7.6).
        RouteError::TransportLost { .. } => ("unknown", None, "error"),
    }
}

fn classify(accepted: bool, outcome: Result<FakeTerminalEvidence, AdapterError>) -> Terminal {
    let evidence = match outcome {
        Ok(evidence) => evidence,
        Err(error) => return failed_terminal(error),
    };
    let failed = |class, message: &str| Some(failure(class, message.to_owned(), None));
    let failure = if accepted {
        match evidence.status {
            VendorTerminalStatus::Completed if evidence.exit.code != Some(0) => failed(
                FailureClass::ProcessExited,
                "the vendor exited unsuccessfully",
            ),
            VendorTerminalStatus::Completed if evidence.cleanup != Cleanup::Quiescent => failed(
                FailureClass::ProcessExited,
                "vendor process group cleanup is unconfirmed",
            ),
            VendorTerminalStatus::Completed => None,
            VendorTerminalStatus::Interrupted | VendorTerminalStatus::Failed => Some(failure(
                FailureClass::VendorError,
                "the vendor reported a failed turn".to_owned(),
                evidence.vendor_code.clone(),
            )),
        }
    } else {
        failed(
            FailureClass::SubmitFailed,
            "the vendor did not accept the submission",
        )
    };
    let stop_reason = match (&failure, evidence.status) {
        (None, _) => canonical_stop_reason(&evidence.stop_reason),
        (Some(_), VendorTerminalStatus::Interrupted) => "interrupted",
        (Some(_), VendorTerminalStatus::Completed | VendorTerminalStatus::Failed) => "error",
    };
    Terminal {
        state: if failure.is_none() {
            "completed"
        } else {
            "failed"
        },
        failure,
        stop_reason,
        vendor_stop_reason: Some(evidence.stop_reason),
        final_text: evidence.final_text,
        exit: Some(Exit {
            code: evidence.exit.code,
            signal: evidence.exit.signal,
        }),
        raw_ref: Some(evidence.terminal_raw),
        raw_incomplete: false,
        warnings: Vec::new(),
        cancel: None,
    }
}

/// Keeps the typed cause, cited frame, confirmed exit and raw completeness of a
/// failed drive.
fn failed_terminal(error: AdapterError) -> Terminal {
    let message = error.to_string();
    let (state, class, stop_reason, route) = match error {
        AdapterError::Route(route) => {
            let (state, class, stop_reason) = route_disposition(&route.cause);
            (state, class, stop_reason, Some(route))
        }
        // No process was launched for the submission.
        AdapterError::Unavailable => ("failed", Some(FailureClass::SubmitFailed), "error", None),
        AdapterError::Open(_) | AdapterError::Protocol => {
            ("failed", Some(FailureClass::Protocol), "error", None)
        }
    };
    Terminal {
        state,
        failure: class.map(|class| failure(class, message, None)),
        stop_reason,
        vendor_stop_reason: None,
        final_text: String::new(),
        exit: route
            .as_ref()
            .and_then(|route| route.exit)
            .map(|exit| Exit {
                code: exit.code,
                signal: exit.signal,
            }),
        raw_ref: route.as_ref().and_then(|route| route.evidence.clone()),
        raw_incomplete: route.is_some_and(|route| route.raw_incomplete),
        warnings: Vec::new(),
        cancel: None,
    }
}

fn failure(class: FailureClass, message: String, vendor_code: Option<String>) -> Failure {
    Failure {
        class,
        message,
        vendor_code,
        retryable: false,
    }
}

/// Maps a vendor stop word onto C1's closed `stop_reason` set.
fn canonical_stop_reason(vendor: &str) -> &'static str {
    match vendor {
        "end_turn" => "end_turn",
        "max_steps" => "max_steps",
        "budget" => "budget",
        "refusal" => "refusal",
        "interrupted" => "interrupted",
        "deadline" => "deadline",
        "error" => "error",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::{FailureClass, TurnNumber, failed_terminal};
    use via_adapters::{AdapterError, RouteError, RouteFailure};

    fn route(cause: RouteError, raw_incomplete: bool) -> AdapterError {
        AdapterError::Route(RouteFailure {
            cause,
            evidence: None,
            exit: None,
            raw_incomplete,
            cleanup: None,
            forced: false,
        })
    }

    /// Causes the fake vendor cannot trigger end to end keep their C1 §8.2 class.
    #[test]
    fn route_causes_keep_their_c1_disposition() {
        let turn = TurnNumber::try_from(1).unwrap();
        for (cause, class) in [
            (RouteError::Overflow { turn }, FailureClass::Overflow),
            (RouteError::Store { turn }, FailureClass::Store),
            (RouteError::Deadline { turn }, FailureClass::DeadlineWall),
            (
                RouteError::ProcessExited { turn },
                FailureClass::ProcessExited,
            ),
        ] {
            let terminal = failed_terminal(route(cause, false));
            assert_eq!(terminal.state, "failed");
            assert_eq!(terminal.failure.map(|failure| failure.class), Some(class));
            assert!(!terminal.raw_incomplete);
        }
        let lost = failed_terminal(route(
            RouteError::TransportLost {
                turn,
                evidence: None,
            },
            true,
        ));
        assert_eq!(lost.state, "unknown");
        assert!(lost.failure.is_none());
        assert!(lost.raw_incomplete);
    }
}
