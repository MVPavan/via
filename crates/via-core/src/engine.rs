//! Durable first-turn orchestration. Store decides persistence; Adapter owns vendor I/O.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::api::{
    Bound, Capabilities, Cost, Effective, Envelope, Event, EventBody, EventRange, Exit,
    FAKE_WALL_MS, Failure, RawSpan, Receipt, Requested, RoutePlan, Timestamps, Usage, VendorFields,
    rfc3339,
};
use crate::{
    ApiError, ConnectionId, Deadline, FakeConfig, RawRef, SessionId, SpawnParams, SteerParams,
    TurnNumber, hash_handle, parse_address,
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
        })
    }

    /// Returns the number of receipted turns still being driven.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Commits a receipt before authorizing any process launch.
    pub async fn spawn(&self, params: SpawnParams) -> Result<(Value, String, String), ApiError> {
        let _admission = self.admission.lock().await;
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
        let submitted_at = rfc3339(submitted);
        let mut seq = 2;
        let connection = ConnectionId::try_from(
            format!("c_{}", session.as_str().trim_start_matches("s_")).as_str(),
        )
        .map_err(|_| ApiError::STORE)?;
        let (accepted_tx, mut accepted_rx) = mpsc::channel::<FakeAcceptanceObservation>(1);
        let deadline =
            Deadline::at(tokio::time::Instant::now() + Duration::from_millis(FAKE_WALL_MS));
        let execute = self.adapter.execute(
            session.clone(),
            turn,
            connection,
            prompt,
            accepted_tx,
            deadline,
        );
        tokio::pin!(execute);
        let mut accepted: Option<Accepted> = None;
        let mut acceptance_open = true;
        let outcome = loop {
            tokio::select! {
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
        seq += 1;
        let ended_at = rfc3339(SystemTime::now());
        // Monotonic, so wall-clock steps cannot distort or drop the duration.
        let elapsed = submitted_clock.elapsed();
        let terminal = classify(accepted.is_some(), outcome);
        let raw_ref = terminal.raw_ref.clone();
        let event = Event {
            seq,
            session_id: &session,
            turn: Some(turn.get()),
            late: false,
            at: &ended_at,
            raw_ref: raw_ref.as_ref(),
            body: EventBody::TurnEnded {
                state: terminal.state,
                failure: terminal.failure.clone(),
                stop_reason: terminal.stop_reason,
            },
        }
        .to_value()?;
        let timestamps = Timestamps {
            queued_at,
            submitted_at: Some(submitted_at),
            accepted_at: accepted.as_ref().map(|accepted| accepted.at.clone()),
            ended_at,
        };
        let duration_ms = u64::try_from(elapsed.as_millis()).ok();
        let envelope = terminal_envelope(
            &session,
            turn,
            terminal,
            accepted,
            timestamps,
            duration_ms,
            seq,
        );
        let envelope = serde_json::to_value(&envelope).map_err(|_| ApiError::STORE)?;
        self.store
            .commit_terminal(TerminalRecord {
                session_id: session,
                turn,
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

    /// Returns Host-verified cleanup facts for every committed anchor.
    pub async fn verify_cleanup(&self) -> Result<Value, ApiError> {
        let _admission = self.admission.lock().await;
        if self.active() != 0 {
            return Err(ApiError {
                code: -32012,
                kind: "admission_refused",
                message: "turns are active",
            });
        }
        let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(5));
        let reports = self
            .adapter
            .recover(deadline)
            .await
            .map_err(|_| ApiError::STORE)?;
        let count = reports.len();
        let absent = reports
            .iter()
            .all(|report| report.cleanup == Cleanup::Quiescent);
        let anchors: Vec<Value> = reports.into_iter().map(|report| json!({
            "anchor_id":report.anchor_id,"generation":report.generation,
            "session_id":report.session_id,"turn":report.turn.get(),
            "cleanup":if report.cleanup == Cleanup::Quiescent { "group_absent" } else { "uncertain" },
        })).collect();
        Ok(
            json!({"inventory_committed":true,"count":count,"absence_proven":absent,
            "status":if count == 0 {"no_anchors"} else if absent {"quiescent"} else {"unverified"},
            "records":anchors}),
        )
    }

    /// Drains Host-owned controls and reapers before the Store owner is released.
    pub async fn shutdown(&self) -> Result<Value, ApiError> {
        let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(10));
        let report = self
            .adapter
            .shutdown(deadline)
            .await
            .map_err(|_| ApiError::STORE)?;
        let absent = report
            .recovery
            .iter()
            .all(|record| record.cleanup == Cleanup::Quiescent);
        let count = report.recovery.len();
        Ok(json!({"anchors":count,"absence_proven":absent,"pending_tasks":report.pending_tasks}))
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
        cancel: None,
        harness: "fake",
        model: Requested {
            requested: "fake".to_owned(),
            resolved: "fake".to_owned(),
        },
        effort: Requested {
            requested: None,
            resolved: None,
        },
        warnings: plan.warnings(),
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
