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
mod recovery;
mod stop;
mod terminal;
#[cfg(test)]
mod tests;

use journal::{Head, UncertainEvent, Unresolved};
use queue::{DAEMON_QUEUE_LIMIT, SESSION_QUEUE_LIMIT, Slot};
pub use stop::{EngineShutdown, StopMode};

/// A committed receipt and, when this request created or adopted the turn,
/// that turn, now queued with its session's dispatcher. A replayed retry
/// enqueues nothing.
#[derive(Debug)]
pub struct Receipted {
    /// The exact C1 receipt, original or replayed.
    pub receipt: Value,
    /// The turn this request enqueued.
    pub enqueued: Option<(SessionId, TurnNumber)>,
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
    /// Sessions with dispatch state: queue, dispatcher and event head. A slot
    /// is retired when its dispatcher exits with nothing left (design §2).
    sessions: StdMutex<HashMap<SessionId, Arc<Slot>>>,
    /// Receipted turns without a confirmed submission or durable cancellation.
    queued: AtomicUsize,
    /// Phase two of the latch, set under `admission` after `failure_pending`
    /// (runtime §7): the latch is ordered after every receipt inside it.
    store_failed: AtomicBool,
    /// Phase one of the latch: a failed or uncertain write was observed; set
    /// under the `stop` mutex before the observer awaits anything.
    failure_pending: AtomicBool,
    /// Sessions with dispatch state when force was accepted, for final
    /// shutdown's closure pass.
    force_sessions: StdMutex<Option<Vec<SessionId>>>,
    /// Until when a force-path read may retry: final shutdown's deadline less
    /// the part kept for Host cleanup and forced terminals.
    read_retries_until: watch::Sender<Option<tokio::time::Instant>>,
    /// Sessions whose dispatcher daemon main must start.
    starts: mpsc::Sender<SessionId>,
    /// Daemon main's end of `starts`, taken once.
    start_receiver: StdMutex<Option<mpsc::Receiver<SessionId>>>,
    /// Starts that found `starts` full; daemon main retries them.
    pending_starts: StdMutex<HashSet<SessionId>>,
    /// Test-only in-process Store fault backend; production builds have none.
    #[cfg(test)]
    faults: Faults,
}

/// Store faults and dispatch hooks injected in unit tests.
#[cfg(test)]
#[derive(Default)]
struct Faults {
    /// The next receipt commit succeeds, but its reply reports an unknown outcome.
    receipt_reply_lost: AtomicBool,
    /// This many dispatch reads of a turn's predecessors fail.
    predecessors_unreadable: AtomicUsize,
    /// Dispatch reads of predecessors made.
    reads: AtomicUsize,
    /// The next submission commit succeeds, but its reply reports an unknown outcome.
    submission_reply_lost: AtomicBool,
    /// This many `queued → cancelled` commits fail, writing nothing.
    cancel_fails: AtomicUsize,
    /// The next cancellation waits for `release` before its first read.
    hold_cancel_read: AtomicBool,
    /// This many cancellation reads of the queued turn fail.
    cancel_read_fails: AtomicUsize,
    /// The next closing cancellation waits for `release` before `admission`.
    hold_before_close: AtomicBool,
    /// The next receipt commit waits for `release` first, holding `admission`.
    hold_receipt: AtomicBool,
    /// A turn decided `Run` waits for `grant_release` before its grant.
    hold_before_grant: AtomicBool,
    grant_paused: tokio::sync::Notify,
    grant_release: tokio::sync::Notify,
    /// The next closing cancellation waits for `release` after its failure
    /// and close checks, before its commit.
    hold_after_close_check: AtomicBool,
    /// A granted turn waits for `release` before its submission commit.
    hold_after_grant: AtomicBool,
    granted: tokio::sync::Notify,
    release: tokio::sync::Notify,
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

/// Part of final shutdown's deadline that force-path read retries leave for
/// Host cleanup (its 3 s native stop) and forced terminals.
const READ_RETRY_RESERVE: Duration = Duration::from_secs(4);

/// A held `admission` guard: receipts, stop acceptance, the Store-failed
/// latch and every `session.closed` decision are ordered by it.
type Admission<'a> = tokio::sync::MutexGuard<'a, ()>;

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
        Self::open_with(state, runtime, fake, binary, DAEMON_QUEUE_LIMIT)
    }

    /// `open` with the dispatcher-start channel's capacity; unit tests lower it.
    fn open_with(
        state: &Path,
        runtime: &Path,
        fake: FakeConfig,
        binary: PathBuf,
        start_capacity: usize,
    ) -> Result<Self, String> {
        // Test builds only: the named failpoints activate before any Store write.
        #[cfg(feature = "test-failpoints")]
        via_store::failpoint::activate_from_environment()?;
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
        let (starts, start_receiver) = mpsc::channel(start_capacity);
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
            store_failed: AtomicBool::new(false),
            failure_pending: AtomicBool::new(false),
            force_sessions: StdMutex::new(None),
            read_retries_until: watch::Sender::new(None),
            starts,
            start_receiver: StdMutex::new(Some(start_receiver)),
            pending_starts: StdMutex::new(HashSet::new()),
            #[cfg(test)]
            faults: Faults::default(),
        })
    }

    /// A receipt commit that reported failure (C1 §8.1, runtime §7): latches
    /// Store failure and is `store_error` with `commit_outcome`, `unknown`
    /// with `retry: same_key_only` when it may have committed. Restart
    /// recovery settles an unknown one.
    fn receipt_failed(&self, error: &StoreError, admission: &Admission<'_>) -> ApiError {
        self.latch_held(admission);
        if journal::may_have_committed(error) {
            ApiError::RECEIPT_UNKNOWN
        } else {
            ApiError::RECEIPT_NOT_COMMITTED
        }
    }

    /// Latches Store failure after Core's first failed or uncertain state
    /// write (runtime §7), in two phases (design §3.2). Phase one runs now,
    /// before anything is awaited: `failure_pending` and the force signal are
    /// published under the `stop` mutex, so no grant, no pre-ARM gate and no
    /// new receipt passes from here on. Phase two, the returned future,
    /// finalizes the latch under `admission`, ordered after any receipt
    /// already inside it. The caller holds no slot, session or head lock.
    pub(super) fn latch(&self) -> impl Future<Output = ()> + '_ {
        self.fail_pending();
        async move {
            let admission = self.admission.lock().await;
            self.latch_held(&admission);
        }
    }

    /// [`Engine::latch`] for a caller already holding `admission`: both phases
    /// at once.
    pub(super) fn latch_held(&self, _admission: &Admission<'_>) {
        self.fail_pending();
        self.store_failed.store(true, Ordering::Release);
    }

    /// Phase one of the latch: marks the failure pending and sends the force
    /// signal under the `stop` mutex, which the grant takes, so running turns
    /// take the forced path and daemon main starts final shutdown, which then
    /// reports an unclean exit.
    fn fail_pending(&self) {
        let mut stop = lock(&self.stop);
        if self.failure_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        *stop = Some(StopMode::Force);
        self.force_requested_at
            .get_or_init(|| rfc3339(SystemTime::now()));
        self.force.send_replace(true);
    }

    /// Whether phase two finalized the latch under `admission`.
    #[cfg(test)]
    fn latch_finalized(&self) -> bool {
        self.store_failed.load(Ordering::Acquire)
    }

    /// Final shutdown began with this absolute deadline: force-path reads
    /// stop retrying in time for Host cleanup and forced terminals.
    pub fn begin_final_shutdown(&self, deadline: tokio::time::Instant) {
        let by = deadline
            .checked_sub(READ_RETRY_RESERVE)
            .unwrap_or_else(tokio::time::Instant::now);
        self.read_retries_until
            .send_if_modified(|until| until.is_none() && until.replace(by).is_none());
    }

    /// Until when a force-path read may run or retry, once final shutdown began.
    fn read_retries_until(&self) -> Option<tokio::time::Instant> {
        *self.read_retries_until.borrow()
    }

    /// Resolves at the force-path read cutoff; never before final shutdown began.
    async fn read_cutoff(&self) {
        let mut until = self.read_retries_until.subscribe();
        let by = match until.wait_for(Option::is_some).await {
            Ok(by) => *by,
            Err(_) => None,
        };
        match by {
            Some(by) => tokio::time::sleep_until(by).await,
            None => std::future::pending().await,
        }
    }

    /// Test hook: once `flag` is armed, signals `granted` and waits for `release`.
    #[cfg(test)]
    async fn hold(&self, flag: &AtomicBool) {
        if flag.swap(false, Ordering::AcqRel) {
            self.faults.granted.notify_one();
            self.faults.release.notified().await;
        }
    }

    /// Whether a Store failure was observed: pending or finalized.
    pub fn store_failed(&self) -> bool {
        self.failure_pending.load(Ordering::Acquire) || self.store_failed.load(Ordering::Acquire)
    }

    /// Wakes when a force stop is accepted or Store failure latches; daemon
    /// main then starts final shutdown in the mode `stop_mode` reports.
    pub fn force_signal(&self) -> watch::Receiver<bool> {
        self.force.subscribe()
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

    /// The dispatch slot of a session with dispatch state.
    fn slot(&self, session: &SessionId) -> Option<Arc<Slot>> {
        lock(&self.sessions).get(session).cloned()
    }

    /// Records a receipted turn under admission: tracked until durable,
    /// counted active and queued, and enqueued with its dispatcher.
    fn receipted(&self, session: &SessionId, turn: TurnNumber, slot: &Slot) {
        self.unresolved.receipt(session, turn);
        self.active.fetch_add(1, Ordering::AcqRel);
        self.queued.fetch_add(1, Ordering::AcqRel);
        if slot.enqueue(turn) {
            self.request_start(session.clone());
        }
    }

    /// Asks daemon main to start the session's dispatcher; a full channel
    /// leaves the start pending for daemon main's retry (design §5).
    fn request_start(&self, session: SessionId) {
        let mut pending = lock(&self.pending_starts);
        if let Err(mpsc::error::TrySendError::Full(session)) = self.starts.try_send(session) {
            pending.insert(session);
        }
    }

    /// Moves pending starts into the channel while it has capacity. Daemon
    /// main calls it after each start it takes and in final shutdown.
    pub fn retry_starts(&self) {
        let mut pending = lock(&self.pending_starts);
        let waiting: Vec<SessionId> = pending.iter().cloned().collect();
        for session in waiting {
            if self.starts.try_send(session.clone()).is_err() {
                break;
            }
            pending.remove(&session);
        }
    }

    /// Whether a start still waits for channel capacity.
    pub fn starts_pending(&self) -> bool {
        !lock(&self.pending_starts).is_empty()
    }

    /// Daemon main's receiver of sessions whose dispatcher it must start.
    pub fn take_starts(&self) -> Option<mpsc::Receiver<SessionId>> {
        lock(&self.start_receiver).take()
    }

    /// Removes an idle, unleased slot; the caller holds admission, so no
    /// receipt commit is using it.
    fn retire(&self, session: &SessionId) {
        let mut sessions = lock(&self.sessions);
        if sessions
            .get(session)
            .is_some_and(|slot| slot.idle() && slot.unleased())
        {
            sessions.remove(session);
        }
    }

    /// Returns the number of receipted turns not yet settled.
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
        let admission = self.admission.lock().await;
        // Runtime §7: no new mutation, not even a keyed replay, after a failed write.
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
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
                        Ok(Receipted {
                            receipt: stored.receipt,
                            enqueued: None,
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
        #[cfg(test)]
        self.hold(&self.faults.hold_receipt).await;
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
        if let Err(error) = self.receipt_reply(stored) {
            return Err(self.receipt_failed(&error, &admission));
        }
        let slot = Slot::new(Head::new(Some(2)));
        lock(&self.sessions).insert(session.clone(), Arc::clone(&slot));
        self.receipted(&session, turn, &slot);
        Ok(Receipted {
            receipt,
            enqueued: Some((session, turn)),
        })
    }

    /// C1 §3.3: authenticates, replays a keyed retry, then commits the next
    /// turn `queued` with its `turn.queued` event before the receipt.
    pub async fn resume(
        &self,
        params: ResumeParams,
        raw_params: &str,
    ) -> Result<Receipted, ApiError> {
        let admission = self.admission.lock().await;
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
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
                        Ok(Receipted {
                            enqueued: None,
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
        self.queue_turn(session, &snapshot, params.prompt, operation, &admission)
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
        admission: &Admission<'_>,
    ) -> Result<Receipted, ApiError> {
        let turn = TurnNumber::try_from(snapshot.turns + 1).map_err(|_| ApiError::STORE)?;
        let slot = self.slot_for(&session);
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
                } else {
                    drop(head);
                }
                // A slot this request created holds nothing: retire it.
                drop(slot);
                self.retire(&session);
                return Err(self.receipt_failed(&error, admission));
            }
        }
        self.receipted(&session, turn, &slot);
        Ok(Receipted {
            receipt,
            enqueued: Some((session, turn)),
        })
    }

    /// The session's dispatch slot, created when it has none. Whether a new
    /// turn may run behind earlier ones is decided from their durable state.
    fn slot_for(&self, session: &SessionId) -> Arc<Slot> {
        Arc::clone(
            lock(&self.sessions)
                .entry(session.clone())
                .or_insert_with(|| Slot::new(Head::new(None))),
        )
    }

    /// Authenticates before reporting fake's unsupported mutation capability.
    pub async fn steer(&self, params: SteerParams) -> Result<Value, ApiError> {
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
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
