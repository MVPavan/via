//! Durable multi-turn session orchestration. Store decides persistence;
//! Adapter owns vendor I/O.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, OnceLock, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::api::{
    Cancel, Capabilities, DEFAULT_WAIT_MS, Effective, Event, EventBody, Exit, Failure,
    FailureClass, RawSpan, Receipt, RoutePlan, TurnReceipt, Warning, retry_key, rfc3339,
};
use crate::{
    ApiError, FakeConfig, RawRef, ResumeParams, SessionId, SpawnParams, SteerParams, TurnNumber,
    WaitParams, hash_handle, parse_address, retry_identity,
};
use via_adapters::{AdapterRuntime, AdapterRuntimeConfig, RuntimeConfig};
use via_store::{
    OperationRecord, ResumeRecord, SessionSnapshot, SpawnKey, SpawnRecord, Store, StoreClient,
    StoreError,
};

mod drive;
mod journal;
mod queue;
mod stop;
mod terminal;
#[cfg(test)]
mod tests;

use journal::{Head, UncertainEvent, Unresolved};
use queue::{DAEMON_QUEUE_LIMIT, SESSION_QUEUE_LIMIT, Slot};
pub use stop::{EngineShutdown, StopMode};

/// A committed receipt and, when this request created the turn, the turn
/// daemon main must drive. A replayed retry creates nothing to drive.
#[derive(Debug)]
pub struct Receipted {
    /// The exact C1 receipt, original or replayed.
    pub receipt: Value,
    /// The new turn to hand to its drive.
    pub drive: Option<(SessionId, TurnNumber)>,
}

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
    /// Every session this daemon admitted work for: dispatch gate and event head.
    sessions: StdMutex<HashMap<SessionId, Arc<Slot>>>,
    /// Receipted turns of this daemon not yet out of the queue.
    queued: AtomicUsize,
    /// Turns whose receipt commit outcome stayed unknown: possibly committed,
    /// never registered or driven. A keyed retry that finds one adopts it.
    orphans: StdMutex<HashSet<(SessionId, TurnNumber)>>,
    /// Orphans the daemon itself found committed, for daemon main to drive.
    adopted: mpsc::Sender<(SessionId, TurnNumber)>,
    /// Daemon main's end of `adopted`, taken once.
    adoptions: StdMutex<Option<mpsc::Receiver<(SessionId, TurnNumber)>>>,
    /// Test-only in-process Store fault backend; production builds have none.
    #[cfg(test)]
    faults: Faults,
}

/// Receipt-commit faults injected in unit tests.
#[cfg(test)]
#[derive(Default)]
struct Faults {
    /// The next receipt commit succeeds, but its reply reports an unknown outcome.
    receipt_reply_lost: AtomicBool,
    /// Reads that reconcile an uncertain receipt commit fail.
    reconcile_unreadable: AtomicBool,
    /// This many dispatch reads of a turn's predecessors fail.
    predecessors_unreadable: AtomicUsize,
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

/// Durable facts of a turn about to take its terminal: its queue entry and,
/// once committed, its submission.
struct Started {
    session: SessionId,
    turn: TurnNumber,
    queued_at: String,
    /// Sequence of the turn's `turn.queued`, the envelope's `first_seq`.
    first_seq: u64,
    /// Submission time and clock; `None` for a turn cancelled while queued.
    submitted: Option<(String, Instant)>,
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
        let (adopted, adoptions) = mpsc::channel(DAEMON_QUEUE_LIMIT);
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
            sessions: StdMutex::new(HashMap::new()),
            queued: AtomicUsize::new(0),
            orphans: StdMutex::new(HashSet::new()),
            adopted,
            adoptions: StdMutex::new(Some(adoptions)),
            #[cfg(test)]
            faults: Faults::default(),
        })
    }

    /// Settles a receipt commit that reported failure (C1 §8.1): Ok when Store
    /// shows the turn committed, so the caller registers and hands it off;
    /// otherwise `store_error` with `commit_outcome`. An unknown outcome leaves
    /// the turn an orphan, which its keyed retry or the daemon's own
    /// reconciliation adopts once Store reads succeed.
    async fn receipt_outcome(
        &self,
        error: &StoreError,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<(), ApiError> {
        if !journal::may_have_committed(error) {
            return Err(ApiError::RECEIPT_NOT_COMMITTED);
        }
        // Admission is held: only this request could have created `turn`.
        match self.reconcile_read(session).await {
            Ok(Some(snapshot)) if snapshot.turns >= turn.get() => Ok(()),
            Ok(_) => Err(ApiError::RECEIPT_NOT_COMMITTED),
            Err(_) => {
                lock(&self.orphans).insert((session.clone(), turn));
                Err(ApiError::RECEIPT_UNKNOWN)
            }
        }
    }

    /// The Store read that reconciles an uncertain receipt commit.
    async fn reconcile_read(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionSnapshot>, StoreError> {
        #[cfg(test)]
        if self.faults.reconcile_unreadable.load(Ordering::Acquire) {
            return Err(StoreError::Unavailable);
        }
        self.store.session_snapshot(session).await
    }

    /// Reconciles orphans once Store reads succeed, taking admission so no
    /// receipt of the same number is in flight.
    pub(super) async fn reconcile_orphans(&self) {
        if lock(&self.orphans).is_empty() {
            return;
        }
        let _admission = self.admission.lock().await;
        self.reconcile_orphans_held().await;
    }

    /// Reconciles every orphan, keyed or not, against Store while admission is
    /// held: a committed one is registered and sent to daemon main exactly
    /// once, one Store shows never committed is forgotten, and one it cannot
    /// read yet stays. After a stop is accepted nothing more is adopted.
    async fn reconcile_orphans_held(&self) {
        if lock(&self.stop).is_some() {
            return;
        }
        let orphans: Vec<_> = lock(&self.orphans).iter().cloned().collect();
        for (session, turn) in orphans {
            match self.reconcile_read(&session).await {
                Ok(Some(snapshot)) if snapshot.turns >= turn.get() => {
                    // Capacity first: a full channel leaves the orphan for a later pass.
                    let Ok(permit) = self.adopted.try_reserve() else {
                        continue;
                    };
                    if let Some(drive) = self.adopt(&session, turn) {
                        permit.send(drive);
                    }
                }
                Ok(_) => {
                    lock(&self.orphans).remove(&(session, turn));
                }
                Err(_) => {}
            }
        }
    }

    /// Registers an orphan found committed, by its keyed retry or the
    /// daemon's reconciliation, so that it is handed off exactly once.
    fn adopt(&self, session: &SessionId, turn: TurnNumber) -> Option<(SessionId, TurnNumber)> {
        if !lock(&self.orphans).remove(&(session.clone(), turn)) {
            return None;
        }
        lock(&self.sessions)
            .entry(session.clone())
            .or_insert_with(|| Slot::new(Head::new(None), turn.get() - 1));
        self.receipted(session, turn);
        Some((session.clone(), turn))
    }

    /// A receipt commit's reply, lost by the test fault backend when armed.
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "the fault backend exists only in tests")
    )]
    fn receipt_reply<T>(&self, reply: Result<T, StoreError>) -> Result<T, StoreError> {
        #[cfg(test)]
        if reply.is_ok() && self.faults.receipt_reply_lost.swap(false, Ordering::AcqRel) {
            return Err(StoreError::Uncertain("injected reply loss".to_owned()));
        }
        reply
    }

    /// The dispatch slot of a session this daemon admitted work for.
    fn slot(&self, session: &SessionId) -> Option<Arc<Slot>> {
        lock(&self.sessions).get(session).cloned()
    }

    /// Records a receipted turn: tracked until durable, driven, and queued.
    fn receipted(&self, session: &SessionId, turn: TurnNumber) {
        // A number reused after an uncommitted unknown receipt is this turn's.
        lock(&self.orphans).remove(&(session.clone(), turn));
        self.unresolved.receipt(session, turn);
        self.active.fetch_add(1, Ordering::AcqRel);
        self.queued.fetch_add(1, Ordering::AcqRel);
    }

    /// Daemon main's receiver of turns the daemon adopted itself after an
    /// unknown receipt outcome; each must be driven like a handed-off turn.
    pub fn take_adoptions(&self) -> Option<mpsc::Receiver<(SessionId, TurnNumber)>> {
        lock(&self.adoptions).take()
    }

    /// Returns the number of receipted turns still being driven.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Commits a receipt before authorizing any process launch.
    ///
    /// A keyed retry is looked up before any admission check (runtime §6): the
    /// same key, handle and byte-identical `raw_params` replay the stored
    /// receipt, anything else under the key is `idempotency_conflict`.
    pub async fn spawn(
        &self,
        params: SpawnParams,
        raw_params: &str,
    ) -> Result<Receipted, ApiError> {
        let _admission = self.admission.lock().await;
        let hash = hash_handle(&params.handle)?;
        let key = match retry_key(params.idempotency_key.as_deref())? {
            Some(key) => {
                let identity = retry_identity(raw_params, &hash)?;
                if let Some(stored) = self
                    .store
                    .spawn_key(key)
                    .await
                    .map_err(|_| ApiError::STORE)?
                {
                    return if stored.identity == identity {
                        let turn = TurnNumber::try_from(1).map_err(|_| ApiError::STORE)?;
                        Ok(Receipted {
                            receipt: stored.receipt,
                            drive: self.adopt(&stored.session_id, turn),
                        })
                    } else {
                        Err(ApiError::IDEMPOTENCY_CONFLICT)
                    };
                }
                Some(SpawnKey {
                    key: key.to_owned(),
                    identity,
                })
            }
            None => None,
        };
        self.reconcile_orphans_held().await;
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
        if self.queued.load(Ordering::Acquire) >= DAEMON_QUEUE_LIMIT {
            return Err(ApiError::QUEUED_AT_CAPACITY);
        }
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
            .commit_keyed_spawn(
                SpawnRecord {
                    session_id: session.clone(),
                    handle_hash: hash,
                    receipt: receipt.clone(),
                    params: json!({"harness":"fake","model":"fake"}),
                    prompt: params.prompt,
                    initial_event,
                },
                key,
            )
            .await;
        let head = match self.receipt_reply(stored) {
            Ok(_) => Head::new(Some(2)),
            Err(error) => {
                self.receipt_outcome(&error, &session, turn).await?;
                Head::new(None)
            }
        };
        lock(&self.sessions).insert(session.clone(), Slot::new(head, 0));
        self.receipted(&session, turn);
        Ok(Receipted {
            receipt,
            drive: Some((session, turn)),
        })
    }

    /// C1 §3.3: authenticates, replays a keyed retry, then commits the next
    /// turn `queued` with its `turn.queued` event before the receipt.
    pub async fn resume(
        &self,
        params: ResumeParams,
        raw_params: &str,
    ) -> Result<Receipted, ApiError> {
        let _admission = self.admission.lock().await;
        let hash = hash_handle(&params.handle)?;
        let key = retry_key(params.op_key.as_deref())?;
        if params.prompt.is_empty() {
            return Err(ApiError::INVALID_PARAMS);
        }
        let session = params.session;
        let snapshot = self
            .store
            .session_snapshot(&session)
            .await
            .map_err(|_| ApiError::STORE)?
            .ok_or(ApiError::SESSION_NOT_FOUND)?;
        if !self
            .store
            .authenticate(&session, &hash)
            .await
            .map_err(|_| ApiError::STORE)?
        {
            return Err(ApiError::INVALID_HANDLE);
        }
        let operation = match key {
            Some(key) => {
                let identity = retry_identity(raw_params, &hash)?;
                let stored = self
                    .store
                    .operation(&session, key)
                    .await
                    .map_err(|_| ApiError::STORE)?;
                if let Some(stored) = stored {
                    return if stored.identity == identity {
                        let turn = stored.result["turn"]
                            .as_str()
                            .and_then(|address| parse_address(address).ok())
                            .and_then(|(_, turn)| turn)
                            .ok_or(ApiError::STORE)?;
                        Ok(Receipted {
                            drive: self.adopt(&session, turn),
                            receipt: stored.result,
                        })
                    } else {
                        Err(ApiError::IDEMPOTENCY_CONFLICT)
                    };
                }
                Some((key.to_owned(), identity))
            }
            None => None,
        };
        if snapshot.closed {
            return Err(ApiError::SESSION_CLOSED);
        }
        self.reconcile_orphans_held().await;
        if lock(&self.stop).is_some() {
            return Err(ApiError::DAEMON_STOPPING);
        }
        journal::admission(&self.store, &self.unresolved).await?;
        if snapshot.queued >= SESSION_QUEUE_LIMIT {
            return Err(ApiError::QUEUE_FULL);
        }
        if self.queued.load(Ordering::Acquire) >= DAEMON_QUEUE_LIMIT {
            return Err(ApiError::QUEUED_AT_CAPACITY);
        }
        self.queue_turn(session, &snapshot, params.prompt, operation)
            .await
    }

    /// Commits the session's next turn `queued`, its `turn.queued` event at the
    /// shared head and any `op_key` result, then returns the turn receipt.
    async fn queue_turn(
        &self,
        session: SessionId,
        snapshot: &SessionSnapshot,
        prompt: String,
        operation: Option<(String, Vec<u8>)>,
    ) -> Result<Receipted, ApiError> {
        let turn = TurnNumber::try_from(snapshot.turns + 1).map_err(|_| ApiError::STORE)?;
        let slot = self.slot_for(&session, snapshot);
        let receipt = TurnReceipt {
            turn: format!("{}/{}", session.as_str(), turn.get()),
            state: "queued",
            queue_position: snapshot.queued,
            effective: Effective::fake("fake"),
            warnings: RoutePlan::fake().warnings(),
        };
        let receipt = serde_json::to_value(&receipt).map_err(|_| ApiError::STORE)?;
        let head = slot
            .head
            .lock(&self.store, &session)
            .await
            .map_err(|_| ApiError::STORE)?;
        let at = rfc3339(SystemTime::now());
        let event = Event {
            seq: head.next(),
            session_id: &session,
            turn: Some(turn.get()),
            late: false,
            at: &at,
            raw_ref: None,
            body: EventBody::TurnQueued {
                queue_position: snapshot.queued,
            },
        }
        .to_value()?;
        let committed = self
            .store
            .commit_resume(ResumeRecord {
                session_id: session.clone(),
                turn,
                prompt,
                event,
                operation: operation.map(|(op_key, identity)| OperationRecord {
                    op_key,
                    identity,
                    result: receipt.clone(),
                }),
            })
            .await;
        match self.receipt_reply(committed) {
            Ok(()) => head.committed(1),
            Err(error) => {
                if journal::may_have_committed(&error) {
                    head.lost();
                }
                if let Err(refused) = self.receipt_outcome(&error, &session, turn).await {
                    // Possibly committed but not driven: later turns must not wait for it.
                    if refused.commit_outcome == Some(crate::api::ReceiptOutcome::Unknown) {
                        slot.finish(turn);
                    }
                    return Err(refused);
                }
            }
        }
        self.receipted(&session, turn);
        Ok(Receipted {
            receipt,
            drive: Some((session, turn)),
        })
    }

    /// The session's dispatch slot, created for a session no drive of this
    /// daemon has touched. Its earlier turns have no drive here; whether a new
    /// turn may run behind them is decided from their durable state.
    fn slot_for(&self, session: &SessionId, snapshot: &SessionSnapshot) -> Arc<Slot> {
        Arc::clone(
            lock(&self.sessions)
                .entry(session.clone())
                .or_insert_with(|| Slot::new(Head::new(None), snapshot.turns)),
        )
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

    /// Resolves a C1 address; a bare session names its latest turn.
    async fn address(&self, address: &str) -> Result<(SessionId, TurnNumber), ApiError> {
        match parse_address(address)? {
            (session, Some(turn)) => Ok((session, turn)),
            (session, None) => {
                let turns = self.turns(&session).await?;
                Ok((
                    session,
                    TurnNumber::try_from(turns).map_err(|_| ApiError::TURN_NOT_FOUND)?,
                ))
            }
        }
    }

    /// The session's highest turn number; `session_not_found` without one.
    async fn turns(&self, session: &SessionId) -> Result<u32, ApiError> {
        Ok(self
            .store
            .session_snapshot(session)
            .await
            .map_err(|_| ApiError::STORE)?
            .ok_or(ApiError::SESSION_NOT_FOUND)?
            .turns)
    }

    /// Refuses a turn the session never had.
    async fn exists(&self, session: &SessionId, turn: TurnNumber) -> Result<(), ApiError> {
        if turn.get() > self.turns(session).await? {
            return Err(ApiError::TURN_NOT_FOUND);
        }
        Ok(())
    }

    /// Reads a committed terminal result without waiting.
    pub async fn result(&self, address: &str) -> Result<Value, ApiError> {
        let (session, turn) = self.address(address).await?;
        if let Some(result) =
            journal::read_result(&self.store, &self.unresolved, &session, turn).await?
        {
            return Ok(result);
        }
        self.exists(&session, turn).await?;
        Err(ApiError::TURN_NOT_FINISHED)
    }

    /// Waits for a durable terminal result independently of client lifetime,
    /// at most `timeout_ms` (C1 §3.8), then `wait_timeout`.
    ///
    /// Once final shutdown committed its last record, a result still missing
    /// can never commit in this daemon: the wait ends `daemon_stopping`.
    pub async fn wait(&self, params: WaitParams) -> Result<Value, ApiError> {
        let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(DEFAULT_WAIT_MS));
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or(ApiError::INVALID_PARAMS)?;
        let (session, turn) = self.address(&params.address).await?;
        let mut checked = false;
        loop {
            // Read before the Store: a result committed before finalization is seen.
            let finalized = self.finalized.load(Ordering::Acquire);
            if let Some(result) =
                journal::read_result(&self.store, &self.unresolved, &session, turn).await?
            {
                return Ok(result);
            }
            if !checked {
                self.exists(&session, turn).await?;
                checked = true;
            }
            if finalized {
                return Err(ApiError::DAEMON_STOPPING);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(ApiError::WAIT_TIMEOUT);
            }
            tokio::time::sleep_until(deadline.min(now + Duration::from_millis(20))).await;
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

/// Durable progress of a running turn: the session's shared event head and the
/// bounding raw spans of every event committed so far.
struct TurnRecord {
    session: SessionId,
    turn: TurnNumber,
    head: Arc<Head>,
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
