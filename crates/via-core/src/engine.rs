//! Durable first-turn orchestration. Store decides persistence; Adapter owns vendor I/O.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    ApiError, ConnectionId, Deadline, FakeConfig, SessionId, SpawnParams, SteerParams, TurnNumber,
    hash_handle, parse_address,
};
use via_adapters::{
    AdapterRuntime, AdapterRuntimeConfig, Cleanup, FakeAcceptanceObservation, FakeTerminalEvidence,
    RuntimeConfig, VendorTerminalStatus,
};
use via_store::{SpawnRecord, Store, StoreClient, TerminalRecord};

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
        let address = format!("{}/1", session.as_str());
        let receipt = json!({"api_version":1,"session_id":session,"turn":address,"turn_number":turn.get(),"address":address,"state":"queued","revision":0});
        let initial_event = json!({"seq":1,"type":"turn.queued","session_id":session,"turn":turn.get(),"address":address});
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
        self.store
            .commit_submission(&session, turn)
            .await
            .map_err(|_| ApiError::STORE)?;
        let connection = ConnectionId::try_from(
            format!("c_{}", session.as_str().trim_start_matches("s_")).as_str(),
        )
        .map_err(|_| ApiError::STORE)?;
        let (accepted_tx, mut accepted_rx) = mpsc::channel::<FakeAcceptanceObservation>(1);
        let deadline = Deadline::at(tokio::time::Instant::now() + Duration::from_secs(30));
        let execute = self.adapter.execute(
            session.clone(),
            turn,
            connection,
            prompt,
            accepted_tx,
            deadline,
        );
        tokio::pin!(execute);
        let mut accepted = false;
        let mut acceptance_open = true;
        let outcome = loop {
            tokio::select! {
                message = accepted_rx.recv(), if !accepted && acceptance_open => {
                    if let Some(observation) = message {
                        self.store.commit_acceptance(&session, turn, &observation.raw_ref, observation.vendor_turn_id.as_str()).await.map_err(|_| ApiError::STORE)?;
                        accepted = true;
                    } else {
                        acceptance_open = false;
                    }
                }
                result = &mut execute => {
                    if !accepted && let Ok(observation) = accepted_rx.try_recv() {
                        self.store.commit_acceptance(&session, turn, &observation.raw_ref, observation.vendor_turn_id.as_str()).await.map_err(|_| ApiError::STORE)?;
                        accepted = true;
                    }
                    break result;
                }
            }
        };
        let (envelope, raw_ref) = terminal_envelope(&session, turn, accepted, outcome);
        let event = json!({"seq":if accepted {4} else {3},"type":"turn.terminal","session_id":session,"turn":turn.get(),"state":envelope["state"]});
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

fn terminal_envelope(
    session: &SessionId,
    turn: TurnNumber,
    accepted: bool,
    outcome: Result<FakeTerminalEvidence, via_adapters::AdapterError>,
) -> (Value, Option<crate::RawRef>) {
    let address = format!("{}/{}", session.as_str(), turn.get());
    match outcome {
        Ok(evidence) => {
            let success = accepted
                && evidence.status == VendorTerminalStatus::Completed
                && evidence.exit.code == Some(0)
                && evidence.cleanup == Cleanup::Quiescent;
            let state = if success { "completed" } else { "failed" };
            let raw_ref = evidence.terminal_raw;
            let envelope = json!({"api_version":1,"session_id":session,"turn":turn.get(),"address":address,"revision":0,"state":state,"failure":if success {Value::Null} else {json!({"kind":"vendor_failed"})},"stop_reason":evidence.stop_reason,"vendor_stop_reason":evidence.stop_reason,"final_text":evidence.final_text,"harness":"fake","model":"fake","exit":{"code":evidence.exit.code,"signal":evidence.exit.signal},"raw_spans":[raw_ref]});
            (envelope, Some(raw_ref))
        }
        Err(error) => {
            let envelope = json!({"api_version":1,"session_id":session,"turn":turn.get(),"address":address,"revision":0,"state":"failed","failure":{"kind":"transport_lost","message":error.to_string()},"stop_reason":"error","final_text":"","harness":"fake","model":"fake","exit":null,"raw_spans":[]});
            (envelope, None)
        }
    }
}
