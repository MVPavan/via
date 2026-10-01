//! Durable multi-turn session orchestration. Store decides persistence;
//! Adapter owns vendor I/O.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, PoisonError, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::api::{Cancel, EventBody, Exit, Failure, FailureClass, Warning};
use crate::{SessionId, TurnNumber};
use via_adapters::{
    AdapterConfig, AdapterSet, CancellationToken, DescribeRequest, RefusalKind, RuntimeConfig,
    TaskTracker,
};
use via_store::{Store, StoreClient, StoreLock};

mod batch;
mod close;
mod control;
mod drive;
mod final_text;
mod journal;
mod lane;
mod latch;
mod progress;
mod queue;
mod read;
mod receipt;
mod recovery;
mod reprobe;
mod resolve;
mod slots;
mod status;
mod stop;
mod terminal;
#[cfg(test)]
mod tests;

pub use batch::FailureBatches;
use journal::{Head, UncertainEvent, Unresolved};
use latch::{FailureSite, WriteOutcome};
use queue::{CONNECTION_SLOTS, DAEMON_QUEUE_LIMIT, Slot};
pub use recovery::Handoff;
pub use status::{Connections, DaemonCounts, Limits};
pub use stop::{EngineShutdown, FinalEntry, StopMode};
#[cfg(feature = "test-failpoints")]
pub use terminal::envelope_at_maximum;

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
    /// This Engine's own `Arc`, which every Engine is made in
    /// ([`Engine::open`]): a turn handed to its lane's actor holds the
    /// Engine until it ends.
    me: Weak<Engine>,
    _store_owner: Store,
    store: StoreClient,
    adapter: AdapterSet,
    /// The daemon's working directory at startup: a session that names no
    /// `cwd` is frozen with it (design §11.1, §5.1 #22).
    cwd: PathBuf,
    /// Sessions' lanes on their drivers (C2 §2): opened at a session's
    /// first dispatch, kept while a live connection can be pinned.
    lanes: lane::Lanes,
    /// Owns every task the drivers start (C2 §2 `SessionCx`).
    tracker: TaskTracker,
    /// The drivers' cancellation; final shutdown cancels it.
    cancel: CancellationToken,
    active: AtomicUsize,
    admission: Arc<tokio::sync::Mutex<()>>,
    /// The stop mode, the force watch and the latch's phase one with the
    /// failure record, shared with Store's read-corruption observer.
    signal: Arc<latch::Signal>,
    /// Force-stopped turns awaiting Host cleanup evidence in final shutdown.
    forced: StdMutex<Vec<ForcedTurn>>,
    /// Running turns whose terminal write failed after the run loop ended,
    /// kept for final shutdown's failure-resolution batch (design §7.4).
    affected: StdMutex<Vec<batch::AffectedTurn>>,
    /// Receipted turns with no terminal known to have committed; a turn whose
    /// terminal could not be made durable reads as `store_error`.
    unresolved: Unresolved,
    /// Set once final shutdown committed its last record; nothing commits after.
    finalized: AtomicBool,
    /// Sessions with dispatch state: queue, dispatcher and event head. A slot
    /// is retired when its dispatcher exits with nothing left (design §2).
    /// Shared with each lane's [`SessionWriter`].
    sessions: Arc<StdMutex<HashMap<SessionId, Arc<Slot>>>>,
    /// Receipted turns without a confirmed submission or durable cancellation.
    queued: AtomicUsize,
    /// Phase two of the latch, set under `admission` after `failure_pending`
    /// (runtime §7): the latch is ordered after every receipt inside it.
    store_failed: Arc<AtomicBool>,
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
    /// Connection slots (design §11): a `Run` turn reserves one before its
    /// grant; at launch Host takes it for the group's life. FIFO waiters.
    slots: Arc<tokio::sync::Semaphore>,
    /// The pool's size: `connections.limit` (design §6.6).
    slot_limit: usize,
    /// Slots held for groups an earlier daemon left unproven (design §11).
    recovered: slots::RecoveredSlots,
    /// Sessions durably `closing`, or treated so after an uncertain
    /// `Closing` (design §4 step 7, §6.6 [r3.5]); a confirmed `Closed`
    /// removes one. Changed only under `admission`.
    closing: StdMutex<HashSet<SessionId>>,
    /// The `final_shutdown` fence (design §6.8 [r3.2]): set once, under
    /// `admission`, when daemon main stops accepting work; read under
    /// `admission` by the close fence. Its watch also stops the re-probe
    /// loop (§8).
    final_shutdown: watch::Sender<bool>,
    /// Sessions whose dispatcher is running: inserted when it starts,
    /// removed when its future ends or is dropped (design §6.8 step 3).
    /// Final shutdown settles nothing of a session still here.
    dispatching: StdMutex<HashSet<SessionId>>,
    /// Daemon config's disk and WAL thresholds, read once at start
    /// (Task 4 design §5.5).
    limits: Limits,
    /// When this Engine opened (design §11.2), as `daemon/status` reports it.
    started_at: String,
    /// `Sessions.open` (design §11.2): sessions not closed, seeded from the
    /// Store at open, +1 at each spawn receipt and −1 at each closed-now
    /// Store answer.
    open_sessions: AtomicUsize,
    /// The data-size walk's cached result (design §5.3); the async mutex
    /// shares one walk among concurrent `daemon/status` calls.
    data_size: tokio::sync::Mutex<Option<status::DataSize>>,
    /// Permits for diagnostic blob steps, the `logs` file checks and the
    /// data-size walk (coding-style §5): each is held until its blocking
    /// work ends, so diagnostics hold at most [`DIAGNOSTIC_STEPS`] of the
    /// Store's blob-step slots and turn work keeps the rest.
    diagnostics: Arc<tokio::sync::Semaphore>,
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
    /// The next submission's queued-turn read fails, writing nothing.
    submission_unread: AtomicBool,
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
    /// The next close caller waits for `release` before it subscribes to
    /// the close watch, holding `admission`.
    hold_before_subscribe: AtomicBool,
    /// The next close pass waits for `release` after its absence check,
    /// before it takes `admission` for `Closed`.
    hold_before_closed: AtomicBool,
    granted: tokio::sync::Notify,
    release: tokio::sync::Notify,
    /// Re-probe passes begun.
    reprobe_passes: AtomicUsize,
    /// Each restart recovery the adapter set was asked for (C2 §2
    /// Recover): the session, how many Host facts it was given, and its
    /// answer (`resumed`, `unknown` or `dead`).
    recoveries: std::sync::Mutex<Vec<(SessionId, usize, &'static str)>>,
    /// The next restart recovery of a session answers `Resumed` with this
    /// driver and session channel: the fake never resumes.
    resume: std::sync::Mutex<
        Option<(
            via_adapters::SessionDriver,
            tokio::sync::mpsc::Receiver<via_adapters::Admitted>,
        )>,
    >,
    /// The next recovered turn waits for `release` after its history read.
    hold_after_history: AtomicBool,
}

/// Committed facts of a turn whose execution a force stop abandoned.
struct ForcedTurn {
    started: Started,
    record: TurnRecord,
    requested_at: String,
    /// A vendor may have launched: Host sent ARM.
    launched: bool,
    /// Route's own Host close: its stop found the vendor live, and whether it
    /// proved group absence; recovery can add to these, never retract them.
    close: RouteClose,
    /// The turn's stop order's cause, if any: an idle order ends the forced
    /// turn `failed(deadline_idle)` (design §2), a `protocol` one
    /// `failed(protocol)`.
    cause: Option<via_adapters::StopCause>,
    /// The final text Core received before the force took the turn.
    text: drive::TurnText,
}

/// Evidence from Route's verified Host close of a forced turn.
#[derive(Clone, Copy, Default)]
struct RouteClose {
    forced: bool,
    quiescent: bool,
}

/// Durable facts of a turn about to take its terminal: its queue entry and,
/// once committed, its submission.
#[derive(Clone)]
struct Started {
    session: SessionId,
    turn: TurnNumber,
    queued_at: String,
    /// Sequence of the turn's `turn.queued`, the envelope's `first_seq`.
    first_seq: u64,
    /// The session's frozen `cwd`, the envelope's `cwd` (design §11.1);
    /// `None` where the terminal's writer does not read it.
    cwd: Option<String>,
    /// Submission time and clock; `None` for a turn cancelled while queued.
    submitted: Option<(String, Instant)>,
    /// The turn's absolute evidence folder, once submitted (Task 4 design
    /// §7.1); the envelope's `evidence.folder`.
    folder: Option<String>,
}

/// A held `admission` guard: receipts, stop acceptance, the Store-failed
/// latch and every `session.closed` decision are ordered by it.
type Admission<'a> = tokio::sync::MutexGuard<'a, ()>;

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The daemon-wide connection-slot pool (design §11) and its size. Test
/// builds only: `VIA_TEST_CONNECTION_SLOTS` lowers it.
/// Most diagnostic blob steps running at once, overrun ones included.
const DIAGNOSTIC_STEPS: usize = 2;

fn connection_slots() -> (Arc<tokio::sync::Semaphore>, usize) {
    let slots = Arc::new(tokio::sync::Semaphore::new(CONNECTION_SLOTS));
    #[cfg(feature = "test-failpoints")]
    if let Some(lowered) = std::env::var("VIA_TEST_CONNECTION_SLOTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    {
        let forgotten = slots.forget_permits(CONNECTION_SLOTS.saturating_sub(lowered));
        return (slots, CONNECTION_SLOTS - forgotten);
    }
    (slots, CONNECTION_SLOTS)
}

impl Engine {
    /// Opens the sole Store owner and passes unopened lower resources to
    /// Adapter/Wire, with the adapters' validated configuration. The
    /// Engine is made in its `Arc`: a session's lane actor runs its turns
    /// on it (Sol r3 N1).
    pub fn open(
        state: &Path,
        runtime: &Path,
        adapters: AdapterConfig,
        binary: PathBuf,
    ) -> Result<Arc<Self>, String> {
        Self::open_with(state, runtime, adapters, binary, DAEMON_QUEUE_LIMIT, None)
    }

    /// [`Engine::open`] under `lock`, the `store.lock` daemon main took
    /// before any State mutation (runtime §6.1); the Store holds it. The
    /// thresholds are daemon config's (Task 4 design §5.5).
    pub fn open_locked(
        state: &Path,
        runtime: &Path,
        adapters: AdapterConfig,
        binary: PathBuf,
        (lock, limits): (StoreLock, Limits),
    ) -> Result<Arc<Self>, String> {
        Self::open_with(
            state,
            runtime,
            adapters,
            binary,
            DAEMON_QUEUE_LIMIT,
            Some((lock, limits)),
        )
    }

    /// `open` with the dispatcher-start channel's capacity, which unit tests
    /// lower, and the `store.lock` already taken with daemon config's
    /// thresholds, if any; without them the defaults apply.
    fn open_with(
        state: &Path,
        runtime: &Path,
        adapters: AdapterConfig,
        binary: PathBuf,
        start_capacity: usize,
        locked: Option<(StoreLock, Limits)>,
    ) -> Result<Arc<Self>, String> {
        // Test builds only: the named failpoints activate before any Store write.
        #[cfg(feature = "test-failpoints")]
        via_store::failpoint::activate_from_environment()?;
        let cwd = std::env::current_dir()
            .map_err(|_| "daemon working directory is unavailable".to_owned())?;
        let limits = locked
            .as_ref()
            .map_or_else(Limits::default, |(_, limits)| *limits);
        let owner = match locked {
            Some((lock, limits)) => Store::open_with_limits(state, lock, limits.wal),
            None => Store::open(state),
        }
        .map_err(|error| error.to_string())?;
        let open_sessions = usize::try_from(owner.open_sessions()).unwrap_or(usize::MAX);
        // Design §7.1 (T3-S5 round 2, decision 11): SQLite corruption on any
        // read reaches the failure hook at Store's read reply, before any
        // read of recovery or of the Engine.
        let signal = Arc::new(latch::Signal::new());
        let observer = Arc::clone(&signal);
        owner.on_read_corruption(move || observer.read_corrupt());
        let store = owner.client();
        let adapter = AdapterSet::new(
            adapters,
            RuntimeConfig {
                anchor_binary: binary,
                anchor_dir: runtime.join("anchors"),
            },
            owner.runtime_resources(),
        )
        .map_err(|error| error.to_string())?;
        let (starts, start_receiver) = mpsc::channel(start_capacity);
        let (slots, slot_limit) = connection_slots();
        Ok(Arc::new_cyclic(|me| Self {
            me: me.clone(),
            _store_owner: owner,
            store,
            adapter,
            cwd,
            lanes: Arc::new(StdMutex::new(HashMap::new())),
            tracker: TaskTracker::new(),
            cancel: CancellationToken::new(),
            active: AtomicUsize::new(0),
            admission: Arc::new(tokio::sync::Mutex::new(())),
            signal,
            forced: StdMutex::new(Vec::new()),
            affected: StdMutex::new(Vec::new()),
            unresolved: Unresolved::default(),
            finalized: AtomicBool::new(false),
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            queued: AtomicUsize::new(0),
            store_failed: Arc::new(AtomicBool::new(false)),
            force_sessions: StdMutex::new(None),
            read_retries_until: watch::Sender::new(None),
            starts,
            start_receiver: StdMutex::new(Some(start_receiver)),
            pending_starts: StdMutex::new(HashSet::new()),
            slots,
            slot_limit,
            recovered: slots::RecoveredSlots::default(),
            closing: StdMutex::new(HashSet::new()),
            final_shutdown: watch::Sender::new(false),
            dispatching: StdMutex::new(HashSet::new()),
            limits,
            started_at: crate::api::rfc3339(std::time::SystemTime::now()),
            open_sessions: AtomicUsize::new(open_sessions),
            data_size: tokio::sync::Mutex::new(None),
            diagnostics: Arc::new(tokio::sync::Semaphore::new(DIAGNOSTIC_STEPS)),
            #[cfg(test)]
            faults: Faults::default(),
        }))
    }

    /// Test hook: once `flag` is armed, signals `granted` and waits for `release`.
    #[cfg(test)]
    async fn hold(&self, flag: &AtomicBool) {
        if flag.swap(false, Ordering::AcqRel) {
            self.faults.granted.notify_one();
            self.faults.release.notified().await;
        }
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

    /// The session's [`SessionWriter`], for its lane.
    fn session_writer(&self, session: &SessionId) -> SessionWriter {
        SessionWriter {
            store: self.store.clone(),
            sessions: Arc::clone(&self.sessions),
            admission: Arc::clone(&self.admission),
            signal: Arc::clone(&self.signal),
            store_failed: Arc::clone(&self.store_failed),
            session: session.clone(),
        }
    }

    /// The Store's owned blob steps (design §6.5, coding-style §5): final
    /// shutdown counts those still running after the Store is dropped as
    /// pending work.
    pub fn blob_tasks(&self) -> via_store::BlobTasks {
        self.store.blob_tasks()
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

    /// Sessions in the durable closing set (design §6.6 `sessions.closing`):
    /// each counts as active work for a plain stop and idle exit.
    pub fn closing_sessions(&self) -> usize {
        lock(&self.closing).len()
    }

    /// Groups whose cleanup a live control or acquisition still owns
    /// (design §6.4): Host's owned pending cleanup, which blocks idle exit.
    pub fn pending_cleanup(&self) -> usize {
        self.adapter.pending_cleanup()
    }

    /// Whether an adapter of this daemon serves `harness`: planning it is
    /// not refused `harness_unavailable` (C2 §2 `plan`; pure).
    fn harness_available(&self, harness: &str) -> bool {
        let request = DescribeRequest {
            harness: Some(harness.to_owned()),
            ..DescribeRequest::default()
        };
        !matches!(
            self.adapter.plan(&request),
            Err(refusal) if refusal.kind == RefusalKind::HarnessUnavailable
        )
    }

    /// The session's dispatch slot, created when it has none. Whether a new
    /// turn may run behind earlier ones is decided from their durable state.
    fn slot_for(&self, session: &SessionId) -> Arc<Slot> {
        Arc::clone(
            lock(&self.sessions)
                .entry(session.clone())
                .or_insert_with(|| Slot::new(Head::new(None), Weak::clone(&self.me))),
        )
    }
}

/// What a lane needs to commit the session-level event of an observation
/// received outside a running turn (C2 §2 session drain; decision H3 as
/// narrowed), as it arrives.
struct SessionWriter {
    store: StoreClient,
    sessions: Arc<StdMutex<HashMap<SessionId, Arc<Slot>>>>,
    admission: Arc<tokio::sync::Mutex<()>>,
    signal: Arc<latch::Signal>,
    store_failed: Arc<AtomicBool>,
    session: SessionId,
}

impl SessionWriter {
    /// Commits `body`, attributed `(turn, late)`, at the session's next
    /// sequence, with `identity` the session's identity columns in the same
    /// transaction ([`journal::commit_session_event`]). Under `admission`,
    /// on the session's slot head: a slot made for the write when the
    /// session has none, and removed after it, so no receipt or dispatcher
    /// meets it. The Store refuses it once the session is closed. A failed
    /// commit is the session's Store failure (design §7: phase one, and
    /// phase two under the `admission` held); nothing is written once Store
    /// failure is pending.
    async fn commit(
        &self,
        (body, at, attributed): (EventBody, &str, (Option<u32>, bool)),
        identity: Option<via_store::SessionIdentity>,
    ) -> journal::SessionWrite {
        let _admission = self.admission.lock().await;
        if self.signal.failure_pending.load(Ordering::Acquire)
            || self.store_failed.load(Ordering::Acquire)
        {
            return journal::SessionWrite::Refused;
        }
        let (slot, made) = {
            let mut sessions = lock(&self.sessions);
            if let Some(slot) = sessions.get(&self.session) {
                (Arc::clone(slot), false)
            } else {
                // Write-only: no turn meets it, so it bounds no lane.
                let slot = Slot::new(Head::new(None), Weak::new());
                sessions.insert(self.session.clone(), Arc::clone(&slot));
                (slot, true)
            }
        };
        let head = Arc::clone(&slot.head);
        let written = journal::commit_session_event(
            &self.store,
            (&head, &self.session),
            (body, at, attributed),
            identity,
        )
        .await;
        drop(head);
        if made {
            let mut sessions = lock(&self.sessions);
            if slot.idle()
                && slot.unleased()
                && sessions
                    .get(&self.session)
                    .is_some_and(|mapped| Arc::ptr_eq(mapped, &slot))
            {
                sessions.remove(&self.session);
            }
        }
        self.report(&written);
        written
    }

    /// Writes `identity`, confirmed again by the connection generation
    /// that committed the session's open event, into the session's
    /// identity columns, with no event ([`journal::commit_identity_columns`]),
    /// as [`Self::commit`] writes.
    async fn commit_columns(&self, identity: via_store::SessionIdentity) -> journal::SessionWrite {
        let _admission = self.admission.lock().await;
        if self.signal.failure_pending.load(Ordering::Acquire)
            || self.store_failed.load(Ordering::Acquire)
        {
            return journal::SessionWrite::Refused;
        }
        let written = journal::commit_identity_columns(&self.store, &self.session, identity).await;
        self.report(&written);
        written
    }

    /// A Host journal write of the session's driver that no turn reports
    /// had an uncertain outcome (critical r1 #4): like every uncertain
    /// write, it latches Store failure (runtime §7), once per lane, as
    /// `reported` records. Phase one (failure pending and the force) is
    /// published before anything is awaited, under `reported`'s lock, so
    /// a second reader returns only once it is published (critical r2
    /// F3); phase two then waits for `admission` (design §7.4).
    async fn journal_uncertain(&self, reported: &StdMutex<bool>) {
        let latches = {
            let mut reported = lock(reported);
            if *reported {
                return;
            }
            *reported = true;
            self.signal.report(
                FailureSite::Journal,
                WriteOutcome::Uncertain,
                latch::FailureScope::Session(&self.session),
            )
        };
        if latches {
            let _admission = self.admission.lock().await;
            self.store_failed.store(true, Ordering::Release);
        }
    }

    /// A failed write is the session's Store failure; the caller holds
    /// `admission`.
    fn report(&self, written: &journal::SessionWrite) {
        if let journal::SessionWrite::Failed(outcome) = written
            && self.signal.report(
                FailureSite::SessionEvent,
                *outcome,
                latch::FailureScope::Session(&self.session),
            )
        {
            // Phase two, under the `admission` held (design §7.4).
            self.store_failed.store(true, Ordering::Release);
        }
    }
}

/// Committed acceptance facts the envelope reports.
#[derive(Clone)]
struct Accepted {
    at: String,
    /// The vendor's turn ID, when the route has one.
    vendor_turn_id: Option<String>,
}

/// Durable progress of a running turn: the session's shared event head and
/// what its committed events established. A clone is kept
/// for the failure-resolution batch of a turn whose terminal failed
/// (design §7.4).
#[derive(Clone)]
struct TurnRecord {
    session: SessionId,
    turn: TurnNumber,
    head: Arc<Head>,
    accepted: Option<Accepted>,
    /// The turn's first failed Store write (design §7.2): after it the turn
    /// writes nothing but its one resolution write.
    first_failure: Option<FailureNote>,
    /// The event commit Store left uncertain, settled before `turn.ended`.
    uncertain: Option<UncertainEvent>,
    /// The step tracker (Task 4 design §2.4) and the rows the terminal
    /// carries (§3.2).
    steps: progress::StepTracker,
    /// What the turn's observations and end established for its envelope.
    vendor: lane::VendorRecord,
}

/// A turn's first failed Store write: where it failed and whether it may
/// have committed (design §7.1, §7.2 [r3.18]).
#[derive(Clone, Copy, Debug)]
struct FailureNote {
    site: FailureSite,
    outcome: WriteOutcome,
}

/// Core's terminal decision from adapter evidence (C1 §5, §8.2).
#[derive(Clone)]
struct Terminal {
    state: &'static str,
    failure: Option<Failure>,
    stop_reason: &'static str,
    vendor_stop_reason: Option<String>,
    /// The final text inline; `None` once it went to `final_text.txt`,
    /// whether or not the file is named (design §6.4).
    final_text: Option<String>,
    /// The durable `final_text.txt` holding a longer final text (design §6.4).
    final_text_file: Option<crate::api::FinalTextFile>,
    exit: Option<Exit>,
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

/// A C1 §8.2 failure; its message is cut to 2 KiB encoded at a character
/// boundary (Task 4 design §6.4).
fn failure(class: FailureClass, message: String, vendor_code: Option<String>) -> Failure {
    Failure {
        class,
        message: crate::api::failure_message(message),
        vendor_code,
        retryable: false,
        data: None,
    }
}
