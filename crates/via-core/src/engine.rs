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
use tokio::sync::watch;

use crate::api::{
    Cancel, Capabilities, Effective, Event, EventBody, Exit, Failure, FailureClass, RawSpan,
    Receipt, RoutePlan, Warning, rfc3339,
};
use crate::{
    ApiError, FakeConfig, RawRef, SessionId, SpawnParams, SteerParams, TurnNumber, hash_handle,
    parse_address,
};
use via_adapters::{AdapterRuntime, AdapterRuntimeConfig, RuntimeConfig};
use via_store::{SpawnRecord, Store, StoreClient};

mod drive;
mod journal;
mod stop;
mod terminal;

use journal::{UncertainEvent, Unresolved};
pub use stop::{EngineShutdown, StopMode};

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

/// Committed facts of a turn whose execution a force stop abandoned.
struct ForcedTurn {
    started: Started,
    record: TurnRecord,
    requested_at: String,
    /// Route's force cleanup could not record every vendor byte.
    raw_incomplete: bool,
    /// A vendor may have launched: Host sent ARM.
    launched: bool,
    /// Route's own Host close: its stop found the vendor live, and whether it
    /// proved group absence; recovery can add to these, never retract them.
    close: RouteClose,
}

/// Evidence from Route's verified Host close of a forced turn.
#[derive(Clone, Copy, Default)]
struct RouteClose {
    forced: bool,
    quiescent: bool,
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
        // Bounds the turns retained for their `store_error` reads.
        journal::admission(&self.store, &self.unresolved).await?;
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

fn failure(class: FailureClass, message: String, vendor_code: Option<String>) -> Failure {
    Failure {
        class,
        message,
        vendor_code,
        retryable: false,
    }
}
