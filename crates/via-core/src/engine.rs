//! Durable multi-turn session orchestration. Store decides persistence;
//! Adapter owns vendor I/O.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, OnceLock, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::api::{Cancel, Exit, Failure, FailureClass, RawSpan, Warning};
use crate::{FakeConfig, RawRef, SessionId, TurnNumber};
use via_adapters::{AdapterRuntime, AdapterRuntimeConfig, RuntimeConfig};
use via_store::{Store, StoreClient};

mod drive;
mod journal;
mod latch;
mod queue;
mod read;
mod receipt;
mod recovery;
mod slots;
mod stop;
mod terminal;
#[cfg(test)]
mod tests;

use journal::{Head, UncertainEvent, Unresolved};
use queue::{CONNECTION_SLOTS, DAEMON_QUEUE_LIMIT, Slot};
pub use recovery::Handoff;
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
    /// Connection slots (design §11): a `Run` turn reserves one before its
    /// grant; at launch Host takes it for the group's life. FIFO waiters.
    slots: Arc<tokio::sync::Semaphore>,
    /// Slots held for groups an earlier daemon left unproven (design §11).
    recovered: slots::RecoveredSlots,
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

/// A held `admission` guard: receipts, stop acceptance, the Store-failed
/// latch and every `session.closed` decision are ordered by it.
type Admission<'a> = tokio::sync::MutexGuard<'a, ()>;

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The daemon-wide connection-slot pool (design §11). Test builds only:
/// `VIA_TEST_CONNECTION_SLOTS` lowers it.
fn connection_slots() -> Arc<tokio::sync::Semaphore> {
    let slots = Arc::new(tokio::sync::Semaphore::new(CONNECTION_SLOTS));
    #[cfg(feature = "test-failpoints")]
    if let Some(lowered) = std::env::var("VIA_TEST_CONNECTION_SLOTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    {
        slots.forget_permits(CONNECTION_SLOTS.saturating_sub(lowered));
    }
    slots
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
            slots: connection_slots(),
            recovered: slots::RecoveredSlots::default(),
            #[cfg(test)]
            faults: Faults::default(),
        })
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

    /// The session's dispatch slot, created when it has none. Whether a new
    /// turn may run behind earlier ones is decided from their durable state.
    fn slot_for(&self, session: &SessionId) -> Arc<Slot> {
        Arc::clone(
            lock(&self.sessions)
                .entry(session.clone())
                .or_insert_with(|| Slot::new(Head::new(None))),
        )
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
