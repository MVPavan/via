//! Durable first-turn orchestration. Store decides persistence; Adapter owns vendor I/O.

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex as StdMutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::api::{
    Bound, Cancel, Capabilities, Cost, Effective, Envelope, Event, EventBody, EventRange, Exit,
    FAKE_WALL_MS, Failure, RawSpan, Receipt, Requested, RoutePlan, Timestamps, Usage, VendorFields,
    Warning, rfc3339,
};
use crate::{
    ApiError, ConnectionId, DaemonStopParams, Deadline, FakeConfig, RawRef, SessionId, SpawnParams,
    SteerParams, TurnNumber, hash_handle, parse_address,
};
use via_adapters::{
    AdapterRuntime, AdapterRuntimeConfig, Cleanup, FakeAcceptanceObservation, FakeTerminalEvidence,
    RuntimeConfig, VendorTerminalStatus,
};
use via_store::{
    AcceptanceRecord, SpawnRecord, Store, StoreClient, SubmissionRecord, TerminalRecord,
};

/// One daemon's durable state and opaque vendor runtime.
pub struct Engine {
    _store_owner: Store,
    store: StoreClient,
    adapter: AdapterRuntime,
    active: AtomicUsize,
    admission: tokio::sync::Mutex<()>,
    /// Accepted `daemon/stop` mode; set under `admission`, never cleared.
    stop: StdMutex<Option<StopMode>>,
    /// Tells running drives to abandon their execution (C1 §3.14 `force`).
    force: watch::Sender<bool>,
    /// Force-stopped turns awaiting Host cleanup evidence in final shutdown.
    forced: StdMutex<Vec<ForcedTurn>>,
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
}

impl EngineShutdown {
    /// Clean only with positive cleanup, every join collected and every record committed.
    pub fn is_clean(&self) -> bool {
        self.uncertain_owners == 0
            && self.pending_tasks == 0
            && self.failed_tasks == 0
            && self.failure.is_none()
            && self.uncommitted_turns == 0
    }
}

/// Committed facts of a turn whose execution a force stop abandoned.
struct ForcedTurn {
    started: Started,
    seq: u64,
    accepted: Option<Accepted>,
    requested_at: String,
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
            forced: StdMutex::new(Vec::new()),
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
            self.force.send_replace(true);
        }
        Ok(mode)
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
        let mut seq = 2;
        let connection = ConnectionId::try_from(
            format!("c_{}", session.as_str().trim_start_matches("s_")).as_str(),
        )
        .map_err(|_| ApiError::STORE)?;
        let (accepted_tx, mut accepted_rx) = mpsc::channel::<FakeAcceptanceObservation>(1);
        let deadline =
            Deadline::at(tokio::time::Instant::now() + Duration::from_millis(FAKE_WALL_MS));
        let mut forced = self.force.subscribe();
        let mut execute = Box::pin(self.adapter.execute(
            session.clone(),
            turn,
            connection,
            prompt,
            accepted_tx,
            deadline,
        ));
        let mut accepted: Option<Accepted> = None;
        let mut acceptance_open = true;
        let outcome = loop {
            tokio::select! {
                biased;
                // The watch guard is released before the handler's awaits.
                true = async { forced.wait_for(|forced| *forced).await.is_ok() } => {
                    // Force stop: dropping the execution releases its Host control, so
                    // the anchor's reviewed EOF cleanup stops the whole group. Final
                    // shutdown commits the cancelled terminal once Host has evidence.
                    drop(execute);
                    if accepted.is_none() && let Ok(observation) = accepted_rx.try_recv() {
                        seq += 1;
                        accepted = Some(self.accept(&session, turn, seq, observation).await?);
                    }
                    lock(&self.forced).push(ForcedTurn {
                        started,
                        seq,
                        accepted,
                        requested_at: rfc3339(SystemTime::now()),
                    });
                    return Ok(());
                }
                message = accepted_rx.recv(), if accepted.is_none() && acceptance_open => {
                    if let Some(observation) = message {
                        seq += 1;
                        accepted = Some(self.accept(&session, turn, seq, observation).await?);
                    } else {
                        acceptance_open = false;
                    }
                }
                result = &mut execute => {
                    if accepted.is_none() && let Ok(observation) = accepted_rx.try_recv() {
                        seq += 1;
                        accepted = Some(self.accept(&session, turn, seq, observation).await?);
                    }
                    break result;
                }
            }
        };
        let terminal = classify(accepted.is_some(), outcome);
        self.finish(&started, seq + 1, terminal, accepted).await
    }

    /// Commits `turn.ended` (at `seq`) with the terminal envelope.
    async fn finish(
        &self,
        started: &Started,
        seq: u64,
        terminal: Terminal,
        accepted: Option<Accepted>,
    ) -> Result<(), ApiError> {
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
            accepted_at: accepted.as_ref().map(|accepted| accepted.at.clone()),
            ended_at,
        };
        let duration_ms = u64::try_from(elapsed.as_millis()).ok();
        let envelope = terminal_envelope(
            &started.session,
            started.turn,
            terminal,
            accepted,
            timestamps,
            duration_ms,
            seq,
        );
        let envelope = serde_json::to_value(&envelope).map_err(|_| ApiError::STORE)?;
        self.store
            .commit_terminal(TerminalRecord {
                session_id: started.session.clone(),
                turn: started.turn,
                envelope,
                event,
                raw_ref,
            })
            .await
            .map_err(|_| ApiError::STORE)
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
    async fn accept(
        &self,
        session: &SessionId,
        turn: TurnNumber,
        seq: u64,
        observation: FakeAcceptanceObservation,
    ) -> Result<Accepted, ApiError> {
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
        .to_value()?;
        let vendor_turn_id = observation.vendor_turn_id.as_str().to_owned();
        self.store
            .commit_acceptance(AcceptanceRecord {
                session_id: session.clone(),
                turn,
                raw_ref: observation.raw_ref.clone(),
                correlation: vendor_turn_id.clone(),
                event,
            })
            .await
            .map_err(|_| ApiError::STORE)?;
        Ok(Accepted {
            at,
            raw_ref: observation.raw_ref,
            vendor_turn_id,
        })
    }

    /// Final shutdown: Host closes live controls, reconciles every anchor and
    /// joins its tasks; then force-stopped turns commit `cancelled` with that
    /// evidence. Every step shares the caller's single absolute deadline.
    pub async fn shutdown(&self, deadline: Deadline) -> EngineShutdown {
        let report = self.adapter.shutdown(deadline).await;
        let forced = std::mem::take(&mut *lock(&self.forced));
        let mut uncommitted_turns = 0;
        for turn in forced {
            let evidence = report.recovery.iter().find(|record| {
                record.session_id == turn.started.session && record.turn == turn.started.turn
            });
            // C1 §7.6: `forced` only with Host evidence, `quiescent` only after
            // verified group absence. A complete journal without an anchor intent
            // for the turn means no process was ever launched for it.
            let (outcome, cleanup) = match evidence.map(|record| record.cleanup) {
                Some(Cleanup::Quiescent) => ("forced", "quiescent"),
                None if report.failure.is_none() => ("acknowledged", "quiescent"),
                Some(Cleanup::Uncertain | Cleanup::Pending) | None => ("requested", "uncertain"),
            };
            let terminal = Terminal {
                state: "cancelled",
                failure: None,
                stop_reason: "interrupted",
                vendor_stop_reason: None,
                final_text: String::new(),
                exit: None,
                raw_ref: None,
                cancel: Some(Cancel {
                    outcome,
                    cleanup,
                    requested_at: turn.requested_at,
                    settled_at: rfc3339(SystemTime::now()),
                }),
            };
            let commit = self.finish(&turn.started, turn.seq + 1, terminal, turn.accepted);
            if !matches!(
                tokio::time::timeout_at(deadline.instant(), commit).await,
                Ok(Ok(()))
            ) {
                uncommitted_turns += 1;
            }
        }
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
        }
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
        self.store
            .result(&session, turn)
            .await
            .map_err(|_| ApiError::STORE)?
            .ok_or(ApiError::TURN_NOT_FINISHED)
    }

    /// Waits for a durable terminal result independently of client lifetime.
    pub async fn wait(&self, address: &str) -> Result<Value, ApiError> {
        let (session, turn) = parse_address(address)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(result) = self
                .store
                .result(&session, turn)
                .await
                .map_err(|_| ApiError::STORE)?
            {
                return Ok(result);
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

/// Assembles the C1 §5 envelope; `last_seq` is `turn.ended`, and S1 turns start at seq 1.
fn terminal_envelope(
    session: &SessionId,
    turn: TurnNumber,
    terminal: Terminal,
    accepted: Option<Accepted>,
    timestamps: Timestamps,
    duration_ms: Option<u64>,
    last_seq: u64,
) -> Envelope {
    let raw_spans = RawSpan::bounding(
        accepted
            .as_ref()
            .map(|accepted| &accepted.raw_ref)
            .into_iter()
            .chain(terminal.raw_ref.as_ref()),
    );
    let plan = RoutePlan::fake();
    let mut warnings = plan.warnings();
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

/// Core's terminal decision from adapter evidence (C1 §5, §8.2).
struct Terminal {
    state: &'static str,
    failure: Option<Failure>,
    stop_reason: &'static str,
    vendor_stop_reason: Option<String>,
    final_text: String,
    exit: Option<Exit>,
    raw_ref: Option<RawRef>,
    cancel: Option<Cancel>,
}

fn classify(
    accepted: bool,
    outcome: Result<FakeTerminalEvidence, via_adapters::AdapterError>,
) -> Terminal {
    let evidence = match outcome {
        Ok(evidence) => evidence,
        Err(error) => {
            return Terminal {
                state: "failed",
                failure: Some(failure("protocol", error.to_string(), None)),
                stop_reason: "error",
                vendor_stop_reason: None,
                final_text: String::new(),
                exit: None,
                raw_ref: None,
                cancel: None,
            };
        }
    };
    let failed = |class, message: &str| Some(failure(class, message.to_owned(), None));
    let failure = if accepted {
        match evidence.status {
            VendorTerminalStatus::Completed if evidence.exit.code != Some(0) => {
                failed("process_exited", "the vendor exited unsuccessfully")
            }
            VendorTerminalStatus::Completed if evidence.cleanup != Cleanup::Quiescent => failed(
                "process_exited",
                "vendor process group cleanup is unconfirmed",
            ),
            VendorTerminalStatus::Completed => None,
            VendorTerminalStatus::Interrupted | VendorTerminalStatus::Failed => Some(failure(
                "vendor_error",
                "the vendor reported a failed turn".to_owned(),
                evidence.vendor_code.clone(),
            )),
        }
    } else {
        failed("submit_failed", "the vendor did not accept the submission")
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
        cancel: None,
    }
}

fn failure(class: &'static str, message: String, vendor_code: Option<String>) -> Failure {
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
