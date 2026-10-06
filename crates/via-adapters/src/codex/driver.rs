//! Codex's driver turn (C2 §2, §4, §4.1; vendors/codex.md §2–§5; x.3.2
//! X0 items 1, 2, 5, 8, 10–13): one VIA turn on a shared `codex
//! app-server`.
//!
//! The session leases a server from its first join until its close (AD16):
//! `prepare` pins the session's live server, or a live one of an equal key;
//! otherwise the turn launches one, or joins one launching. On each
//! connection generation the session subscribes to the connection task's
//! abnormal end and opens its thread once, by `thread/start` or, when an
//! identity was confirmed, by `thread/resume` of that exact thread; the
//! reply's echoes are checked, and the registration's normalizer is
//! spawned: it delivers the thread's lane in decode order across the
//! registration's turns (`delivery`). Every turn is a `turn/start` with
//! the full frozen policy, written under the turn's owning write guard and
//! accepted on its paired reply, then handed to the normalizer while the
//! turn waits for its terminal beside its own orders, sealing its delivery
//! at whichever comes first. A close passes the registration's delivery
//! barrier, seals it, detaches with `thread/unsubscribe` and releases the
//! lease; the last lease's release retires the server.
//!
//! A stop order posts the turn's one interrupt intent, owned by the
//! connection (before acceptance it waits on the start's reply). With no
//! terminal by the order's `close_by` the turn ends there (`uncertain`);
//! one decoded after it is late only, and reaches Core as the turn's late
//! terminal.
//! An interrupted terminal acknowledges the stop (the turn's `StopAck`);
//! with a tool still open the turn drains (C1 §3.5 P7): it stays pending
//! until its tools end, `tool_grace` after the terminal's original decode,
//! the wall, or a close, whichever is first (x.3.2 X4 D4). The turn's
//! result follows the earliest positively attested stop it recorded (an
//! order at its `attached`, or the wall once passed), so a wall that came
//! first gives `Deadline` with the terminal kept, unless decoded after
//! the wall's cleanup bound (late only). Every cutoff (the earliest
//! `close_by` of the orders, the wall's cleanup bound and the P7 window)
//! is judged on attached and decode instants by the consumer, as it
//! decodes each piece of evidence, however late the turn's own wait runs
//! ([`Cutoffs`]). Steer is not supported.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tokio::sync::{oneshot, watch};
use tokio::time::Instant;
use via_routes::codex::{
    AbnormalEnd, ClientId, CommitOutcome, Connection, ConnectionEnd, ConnectionFailure,
    ConnectionLoss, FINISH_BY, Fenced, HandshakeBound, LOSS_EVIDENCE, Lane, LaneEnd, LaneLease,
    LaunchError, LeaseSignal, LossCause, Purpose, RequestError, Reservation, Response, RpcError,
    SandboxMode, ServerKey, ServerLease, ServerPin, Subscription, ThreadResult, ThreadSettings,
    TurnFolder, TurnStart, TurnStartResult, TurnWrites, WriteBounds, crash_on_panic, data, result,
    thread_resume, thread_start, turn_start,
};
use via_routes::{Retirement, SendOutcome, StoreFailure, WireCleanup};

use super::delivery::{
    Admission, CONTRADICTED, Delivery, Evidence, Folders, LossRecord, Losses, Normalizing,
    Registration, Retained, Sealed, ServerEvidence, StartCx, Stop, UNDECODED, UNKNOWN,
    losses as lock_losses,
};
use super::normalize::{self, DiscoveredModel, StructuredOutput};
use super::plan::{self as codex_plan, Echoed, Sandbox};
use super::{ADAPTER_VERSION, CodexAdapter, HARNESS, PerTurn, refusals};
use crate::driver::turn::{CLEANUP_ALLOWANCE, end_active, unaccounted};
use crate::driver::{
    Active, ConnectionPin, DriverState, ForceWatch, Prepared, Retiring, SessionDriver, TurnCx,
    TurnSpec, latch, lock, quiescent, rejected,
};
use crate::harness::Harness;
use crate::instance::Incompatibility;
use crate::observation::{
    AdapterError, Charge, Identity, InstanceReport, Observation, ObservationItem, SessionCap,
    TurnEnd, TurnEvidence, UnparsedOutput,
};
use crate::passthrough::VendorArgs;
use crate::plan::{Bound, Inherit, RefusalKind};
use crate::runtime::event_stall;
use crate::{
    AcceptanceToken, Cleanup, Deadline, DriverFailure, DriverHealth, ProcessOwner, RouteError,
    RouteFailure, StartRejected, StopCause, StopOrder, StopWatch, TurnNumber, VendorCode,
    VendorTerminalStatus,
};

/// How long the link of a turn to its server may take (X0 item 1.5).
const LINK_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// The vendor's `thread/resume` error for a thread it no longer has
/// (packet §2: `-32600`, "no rollout found").
const NO_ROLLOUT: &str = "no rollout found";

/// The ID identity confirmations name for connection `generation`.
pub(crate) fn connection_id(generation: u64) -> String {
    format!("codex-{generation}")
}

/// One session's Codex state: its lease on a server and its thread there.
pub(crate) struct CodexSession {
    adapter: Arc<CodexAdapter>,
    attached: Mutex<Option<Attached>>,
    /// The bound the session's last turn applied (C1 P5): a later turn
    /// that names none inherits it.
    bound: Mutex<Option<Bound>>,
    /// The driver's loss record (X0 item 10), which each generation's
    /// abnormal-end handler writes too.
    losses: Arc<Mutex<Losses>>,
    /// The session's metadata cap on its observation budget (x.3.2 X3
    /// §6.2), shared by its registrations and its turns' credits.
    cap: OnceLock<SessionCap>,
}

/// The session's connection generation. The fields drop in order: the
/// thread's registration retires, then closes, then the abnormal-end
/// subscription, and the server lease is released last.
struct Attached {
    /// Retires the thread's registration, once it has one (x.3.2 X3 §6.5):
    /// declared first, so it drops first.
    retire: Option<RetireGuard>,
    /// The thread this generation opened, once it did.
    thread: Option<Arc<Thread>>,
    /// The connection task's abnormal end reaches the driver through it.
    _subscription: Subscription,
    signal: Arc<LeaseSignal>,
    /// The generation holds a registration (the handler then records the
    /// loss).
    registered: Arc<AtomicBool>,
    connection: Arc<Connection>,
    generation: u64,
    /// The evidence folder of each turn this generation ran: a malformed
    /// message naming an earlier one is kept there (X0 item 5).
    folders: Folders,
    /// The session's lease, held from its first join until close (AD16):
    /// `daemon/status` counts it (x.3.2 X4 D2).
    lease: ServerLease,
}

/// The close's delivery barrier ends this long before the close's
/// deadline (X0 item 8.2).
const DELIVERY_MARGIN: std::time::Duration = std::time::Duration::from_millis(500);

/// An open thread: its ID, its registration on the connection, and the
/// registration's delivery, sealed when the thread is released.
pub(crate) struct Thread {
    id: String,
    lease: LaneLease,
    registration: Arc<Registration>,
}

impl Drop for Thread {
    fn drop(&mut self) {
        let _sealed = self.registration.seal();
    }
}

/// The facts a turn takes from the generation it runs on.
struct Generation {
    number: u64,
    thread: Option<Arc<Thread>>,
    signal: Arc<LeaseSignal>,
    folders: Folders,
}

impl CodexSession {
    pub(crate) fn new(adapter: Arc<CodexAdapter>) -> Self {
        Self {
            adapter,
            attached: Mutex::new(None),
            bound: Mutex::new(None),
            losses: Arc::new(Mutex::new(Losses::default())),
            cap: OnceLock::new(),
        }
    }

    /// The session's metadata cap, on `driver`'s observation budget.
    fn cap(&self, driver: &SessionDriver) -> SessionCap {
        self.cap
            .get_or_init(|| SessionCap::new(&driver.observations))
            .clone()
    }

    fn attached(&self) -> std::sync::MutexGuard<'_, Option<Attached>> {
        // Each edit is one assignment: the state stays consistent.
        self.attached.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The registry's key for the session's server, its raw arguments
    /// included (C2 §6.3).
    fn key(&self, requested: Inherit, vendor_args: &VendorArgs) -> ServerKey {
        ServerKey(
            self.adapter
                .recipe(requested, vendor_args)
                .config_hash(ADAPTER_VERSION)
                .bytes(),
        )
    }

    /// AD16 `prepare`: a pin on the session's live server, else on a live
    /// or launching server of an equal key; `None` when the turn needs a
    /// harness-process slot.
    pub(crate) fn prepare(
        &self,
        requested: Inherit,
        vendor_args: &VendorArgs,
    ) -> Option<ServerPin> {
        let own = self
            .attached()
            .as_ref()
            .filter(|attached| usable(&attached.connection))
            .and_then(|attached| attached.lease.pin());
        own.or_else(|| {
            self.adapter
                .servers()
                .pin(&self.key(requested, vendor_args))
        })
    }

    /// Changes whenever `prepare`'s answer may change (C2 §3).
    pub(crate) fn readiness(&self) -> watch::Receiver<u64> {
        self.adapter.servers().epoch()
    }

    /// The close's detach (packet §2, X0 item 8.2; x.3.2 X3 §5): the close
    /// is posted to the connection task, whose cutoff ends the
    /// registration's lane after the messages it took (its admitted
    /// prefix; later ones go to diagnostics); the delivery barrier waits
    /// for the consumer's outcome, bounded by `deadline` less
    /// [`DELIVERY_MARGIN`], then the seal; the outcome decides what joins
    /// the loss record (§5.4). Then the thread's unsubscribe intent (X0
    /// item 8.3), its reply awaited by `deadline`; then the registration
    /// retires and the lease is released. Never a stdin close: the server
    /// is shared.
    pub(crate) async fn detach(&self, deadline: Deadline) {
        let Some(attached) = self.attached().take() else {
            return;
        };
        if let Some(thread) = &attached.thread {
            attached.connection.post_close(&thread.lease);
            let by = deadline
                .instant()
                .checked_sub(DELIVERY_MARGIN)
                .unwrap_or_else(Instant::now);
            let registration = &thread.registration;
            let drained = registration.drain(by).await;
            let sealed = registration.seal();
            let position = registration.floor(sealed.position);
            let unproven = lock_losses(&self.losses).note_close(
                (thread.lease.lane(), attached.generation),
                drained,
                (&sealed, position),
            );
            if unproven {
                registration.mark_incomplete();
            }
            if usable(&attached.connection)
                && let Some(reply) = attached.connection.unsubscribe(&thread.lease, deadline)
            {
                let _answered = tokio::time::timeout_at(deadline.instant(), reply).await;
            }
        }
        // Its guard retires the registration first (x.3.2 X3 §6.5).
        drop(attached);
    }

    /// The generation of `connection`, attaching the session to it as a
    /// new generation, subscribed to its abnormal end, when it is not the
    /// one attached (the earlier one is released).
    fn attach(
        &self,
        pin: &ServerPin,
        connection: &Arc<Connection>,
        driver: &SessionDriver,
    ) -> Option<Generation> {
        let mut attached = self.attached();
        if let Some(current) = attached.as_ref()
            && Arc::ptr_eq(&current.connection, connection)
        {
            return Some(Generation {
                number: current.generation,
                thread: current.thread.clone(),
                signal: Arc::clone(&current.signal),
                folders: Arc::clone(&current.folders),
            });
        }
        // A server that left `Live` meanwhile has no lease: the session is
        // gone for this turn, as a pin no longer held would be.
        let lease = pin.lease()?;
        let generation = {
            let mut state = driver.state();
            state.generation += 1;
            state.generation
        };
        let registered = Arc::new(AtomicBool::new(false));
        let signal = Arc::new(
            LeaseSignal::new(abnormal_handler(
                Arc::clone(&self.losses),
                Arc::clone(&driver.health),
                Arc::clone(&registered),
                generation,
            ))
            .on_overflow(overflow_handler(
                Arc::clone(&self.losses),
                Arc::clone(&driver.health),
                generation,
            )),
        );
        let subscription = connection.subscribe(Arc::clone(&signal));
        let folders = Folders::default();
        let replaced = attached.replace(Attached {
            retire: None,
            thread: None,
            _subscription: subscription,
            signal: Arc::clone(&signal),
            registered: Arc::clone(&registered),
            connection: Arc::clone(connection),
            generation,
            folders: Arc::clone(&folders),
            lease,
        });
        drop(attached);
        drop(replaced);
        Some(Generation {
            number: generation,
            thread: None,
            signal,
            folders,
        })
    }

    /// Keeps the thread this generation opened, and its registration's
    /// retirement into `driver`'s cleanup facts.
    fn opened(&self, connection: &Arc<Connection>, thread: &Arc<Thread>, driver: &SessionDriver) {
        if let Some(attached) = self.attached().as_mut()
            && Arc::ptr_eq(&attached.connection, connection)
        {
            attached.retire = Some(RetireGuard {
                registration: Arc::clone(&thread.registration),
                lane: Arc::clone(thread.lease.lane()),
                loss: LossRecord {
                    losses: Arc::clone(&self.losses),
                    generation: attached.generation,
                },
                state: Arc::clone(&driver.state),
            });
            attached.thread = Some(Arc::clone(thread));
            attached.registered.store(true, Ordering::Release);
        }
    }

    /// A turn abandoned on the session's generation (its `run_turn` was
    /// dropped; x.3.2 X3 fix r2 #7): the generation detaches at once, its
    /// unsubscribe intent posted and its registration and lease released,
    /// so the thread can register again; the next turn opens it as a new
    /// generation.
    fn abandon(&self) {
        let Some(attached) = self.attached().take() else {
            return;
        };
        if let Some(thread) = &attached.thread
            && usable(&attached.connection)
        {
            let by = Deadline::at(Instant::now() + CLEANUP_ALLOWANCE);
            let _unanswered = attached.connection.unsubscribe(&thread.lease, by);
        }
        drop(attached);
    }

    /// The generation is unfit for the next turn: its registration fails
    /// with `failure`'s cause, when one is latched in its health (x.3.2 X3
    /// §4.1), then retires and closes (its later traffic is late), and the
    /// next turn opens the thread again as a new generation.
    fn quarantine(
        &self,
        connection: &Arc<Connection>,
        failure: Option<(DriverFailure, &watch::Sender<DriverHealth>)>,
    ) {
        let mut attached = self.attached();
        if attached
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.connection, connection))
        {
            let gone = attached.take();
            drop(attached);
            if let (Some((cause, health)), Some(gone)) = (failure, gone.as_ref())
                && let Some(thread) = &gone.thread
            {
                let loss = LossRecord {
                    losses: Arc::clone(&self.losses),
                    generation: gone.generation,
                };
                thread
                    .registration
                    .fail(&cause, (health, thread.lease.lane(), &loss));
            }
            drop(gone);
        }
    }
}

/// x.3.2 X3 §6.5: retires a generation's registration synchronously and
/// once: as the close's detach ends, or as the generation drops without
/// one (the driver dropped, a quarantine, an abandoned turn, a detach
/// dropped mid-await). An open tool or unproven continuity folds
/// `Uncertain` into the session's sticky cleanup facts.
pub(super) struct RetireGuard {
    pub(super) registration: Arc<Registration>,
    /// The registration's lane, whose overflow owner a new loss names.
    pub(super) lane: Arc<Lane>,
    pub(super) loss: LossRecord,
    pub(super) state: Arc<Mutex<DriverState>>,
}

impl Drop for RetireGuard {
    fn drop(&mut self) {
        self.registration.retire((&self.lane, &self.loss), || {
            let uncertain = Retirement {
                launched: true,
                exit: None,
                cleanup: Some(WireCleanup::Uncertain),
                forced: false,
                journal_uncertain: false,
            };
            let mut state = lock(&self.state);
            state.retirement = Some(
                state
                    .retirement
                    .map_or(uncertain, |earlier| sticky(earlier, uncertain)),
            );
        });
    }
}

/// The abnormal-end handler of one generation (X0 item 13.2):
/// synchronous, idempotent and never blocking. With a registration it
/// installs or merges the loss (`omitted` unknown); either way it latches
/// the driver's failure, so Core retires the driver.
pub(super) fn abnormal_handler(
    losses: Arc<Mutex<Losses>>,
    health: Arc<watch::Sender<DriverHealth>>,
    registered: Arc<AtomicBool>,
    generation: u64,
) -> impl Fn(AbnormalEnd) + Send + Sync + 'static {
    move |end: AbnormalEnd| {
        if registered.load(Ordering::Acquire) {
            // Seen on no lane: an overflow before the abnormal end was
            // signalled by the connection task itself, its push and the
            // signal with no await between, so a record already names the
            // lane's overflow owner.
            lock_losses(&losses).note(None, generation, end.first_unqueued, UNKNOWN);
        }
        latch(&health, DriverFailure::OwnedTask);
    }
}

/// The lane-overflow handler of one generation (x.3.2 X3 fix r2 #1):
/// the connection task calls it as the generation's overflowed lane drops
/// a message, whether or not a turn runs. It records the loss and latches
/// the driver's failure at once, so Core retires the driver without
/// waiting on any normalizer. Synchronous, idempotent, never blocking.
pub(super) fn overflow_handler(
    losses: Arc<Mutex<Losses>>,
    health: Arc<watch::Sender<DriverHealth>>,
    generation: u64,
) -> impl Fn(AbnormalEnd) + Send + Sync + 'static {
    move |end: AbnormalEnd| {
        let latest = {
            let mut losses = lock_losses(&losses);
            // A new record names the lane's overflow owner, a
            // predecessor's late message included (critical review x5
            // r3), as every other observer of the overflow does; else the
            // session's latest turn.
            match end.owner {
                Some(owner) => {
                    losses.note_turn(None, owner, (generation, end.first_unqueued, UNKNOWN));
                }
                None => losses.note(None, generation, end.first_unqueued, UNKNOWN),
            }
            losses.latest
        };
        latch(
            &health,
            latest.map_or(DriverFailure::ObservationOverflow, |turn| {
                DriverFailure::Route(RouteError::Overflow { turn })
            }),
        );
    }
}

/// Whether a connection still takes requests.
fn usable(connection: &Connection) -> bool {
    connection.failure().is_none() && connection.ended().is_none()
}

/// The turn's settlement, when its `run_turn` returns, is dropped or
/// unwinds (B1): the turn's delivery is sealed, its cleanup facts are
/// recorded for the session's close, sticky across turns (an uncertain
/// cleanup stays uncertain), and only then is the retirement published
/// `CleanedUp` and `Delivered`. Without the turn's own end (a dropped
/// future) its cleanup is uncertain once anything was written, and the
/// abandonment is latched.
struct Settle<'a> {
    session: &'a CodexSession,
    state: &'a Mutex<DriverState>,
    health: &'a watch::Sender<DriverHealth>,
    turn: TurnNumber,
    done: watch::Sender<Retiring>,
    /// A byte of the turn may have reached the vendor.
    launched: AtomicBool,
    /// The turn's delivery, once accepted.
    delivery: Mutex<Option<Arc<Delivery>>>,
    /// The turn's own cleanup facts, from its end.
    ended: Mutex<Option<Retirement>>,
}

impl<'a> Settle<'a> {
    fn new(
        (driver, session): (&'a SessionDriver, &'a CodexSession),
        turn: TurnNumber,
        done: watch::Sender<Retiring>,
    ) -> Self {
        Self {
            session,
            state: &driver.state,
            health: &driver.health,
            turn,
            done,
            launched: AtomicBool::new(false),
            delivery: Mutex::new(None),
            ended: Mutex::new(None),
        }
    }

    fn launched(&self) {
        self.launched.store(true, Ordering::Release);
    }

    fn deliver(&self, delivery: &Arc<Delivery>) {
        *self.delivery.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(delivery));
    }

    /// Records the cleanup facts of the turn's `end`.
    fn record(&self, end: &TurnEnd) {
        let evidence = match &end.outcome {
            Ok(evidence) => evidence.clone(),
            Err(error) => error.evidence(),
        };
        let launched = match &end.outcome {
            Err(AdapterError::Route(failure)) => failure.launched,
            Ok(_) => true,
            Err(_) => self.launched.load(Ordering::Acquire),
        };
        *self.ended.lock().unwrap_or_else(PoisonError::into_inner) = Some(Retirement {
            launched,
            exit: None,
            cleanup: Some(match evidence.cleanup {
                Cleanup::Quiescent => WireCleanup::Quiescent,
                Cleanup::Uncertain | Cleanup::Pending => WireCleanup::Uncertain,
            }),
            forced: false,
            journal_uncertain: evidence.journal_uncertain,
        });
    }
}

impl Drop for Settle<'_> {
    fn drop(&mut self) {
        if let Some(delivery) = self
            .delivery
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            delivery.seal();
        }
        let ended = self
            .ended
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let facts = ended.unwrap_or_else(|| {
            latch(self.health, DriverFailure::TurnAbandoned);
            self.session.abandon();
            let launched = self.launched.load(Ordering::Acquire);
            Retirement {
                launched,
                exit: None,
                cleanup: Some(if launched {
                    WireCleanup::Uncertain
                } else {
                    WireCleanup::Quiescent
                }),
                forced: false,
                journal_uncertain: false,
            }
        });
        {
            let mut state = lock(self.state);
            state.retirement = Some(match state.retirement {
                Some(earlier) => sticky(earlier, facts),
                None => facts,
            });
        }
        end_active(self.state, self.turn);
        self.done.send_replace(Retiring::CleanedUp);
        self.done.send_replace(Retiring::Delivered);
    }
}

/// The session's cleanup facts after a later turn: uncertainty a turn
/// left on the shared server is never erased by a later turn (ruling 6),
/// nor is Host's `Uncertain` for an acquisition that launched nothing.
fn sticky(earlier: Retirement, later: Retirement) -> Retirement {
    let uncertain = !quiescent(&earlier) || !quiescent(&later);
    Retirement {
        launched: earlier.launched || later.launched,
        exit: None,
        cleanup: Some(if uncertain {
            WireCleanup::Uncertain
        } else {
            WireCleanup::Quiescent
        }),
        forced: false,
        journal_uncertain: earlier.journal_uncertain || later.journal_uncertain,
    }
}

/// The orders that end a turn: Core's stop, the driver's close, the wall,
/// the daemon force and the session's cancellation; and the turn's P7
/// grace, and what stopped it first.
pub(super) struct Orders {
    pub(super) stop: StopWatch,
    pub(super) close: watch::Receiver<Option<StopOrder>>,
    pub(super) wall: Deadline,
    pub(super) cancel: tokio_util::sync::CancellationToken,
    /// C1 P7: how long an interrupted turn's tools may run on.
    pub(super) tool_grace: std::time::Duration,
    /// x.3.2 X4 D4.2: the earliest positively attested stop, which alone
    /// decides settlement's provenance; it only ever moves earlier.
    pub(super) first: Option<Provenance>,
}

/// Why a turn is ending, and by when it must have ended.
#[derive(Clone, Copy, Debug)]
pub(super) struct Ending {
    cause: EndCause,
    by: Deadline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EndCause {
    Stopped,
    Wall,
}

/// x.3.2 X4 D4.2: a positively attested stop and its instant: an order's
/// publication (`attached`), the wall once passed, or the session's
/// cancellation when observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Provenance {
    pub(super) cause: EndCause,
    pub(super) at: Instant,
}

impl Provenance {
    /// Its order: earlier first, and at one instant the wall before an
    /// order (`Stopped` needs `attached < wall`).
    fn key(self) -> (Instant, bool) {
        (self.at, self.cause == EndCause::Stopped)
    }
}

impl Orders {
    /// The orders of a turn under `wall`, with its P7 `tool_grace`.
    pub(super) fn new(
        (stop, close): (StopWatch, watch::Receiver<Option<StopOrder>>),
        (wall, tool_grace): (Deadline, std::time::Duration),
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            stop,
            close,
            wall,
            cancel,
            tool_grace,
            first: None,
        }
    }

    /// x.3.2 X4 D4.2 (I4, I11): records the earliest positively attested
    /// stop instant among the visible orders (each at its `attached`), the
    /// wall once passed by `now` (or proved passed by an order attached at
    /// or after it), and the session's cancellation (at `now`). The record
    /// is written once one exists and only ever moves earlier, so whoever
    /// notes, in whatever order, the final record is the same.
    pub(super) fn note(&mut self, now: Instant) {
        let wall = self.wall.instant();
        let attached = [
            self.stop.borrow().as_ref().map(|order| order.attached),
            self.close.borrow().as_ref().map(|order| order.attached),
        ];
        let mut candidates = Vec::with_capacity(4);
        let mut wall_passed = wall <= now;
        for at in attached.into_iter().flatten() {
            // A value read at or after `attached` (I11): an order attached
            // at or after the wall proves the wall passed.
            wall_passed |= wall <= at;
            candidates.push(Provenance {
                cause: EndCause::Stopped,
                at,
            });
        }
        if wall_passed {
            candidates.push(Provenance {
                cause: EndCause::Wall,
                at: wall,
            });
        }
        if self.cancel.is_cancelled() {
            candidates.push(Provenance {
                cause: EndCause::Stopped,
                at: now,
            });
        }
        let earliest = candidates
            .into_iter()
            .min_by_key(|candidate| candidate.key());
        if let Some(candidate) = earliest
            && self.first.is_none_or(|first| candidate.key() < first.key())
        {
            self.first = Some(candidate);
        }
    }

    /// The cut the wait ends a turn at, read at `now` ([`cut_of`]).
    pub(super) fn cut(&self, now: Instant) -> Option<Instant> {
        cut_of(
            (&self.stop.borrow(), &self.close.borrow()),
            self.wall.instant(),
            now,
        )
    }

    /// The turn's cutoffs, for its consumer.
    pub(super) fn cutoffs(&self) -> Cutoffs {
        Cutoffs {
            stop: self.stop.clone(),
            close: self.close.clone(),
            wall: self.wall.instant(),
            tool_grace: self.tool_grace,
            cancel: self.cancel.clone(),
        }
    }

    /// Resolves once either order changes (published, merged or
    /// replaced), from clones of the orders' receivers: the wait's cut is
    /// then read again.
    fn changed(&self) -> impl Future<Output = ()> + use<> {
        let (mut stop, mut close) = (self.stop.clone(), self.close.clone());
        let key = |order: &Option<StopOrder>| {
            order
                .as_ref()
                .map(|order| (order.attached, order.close_by.instant()))
        };
        let seen = (key(&stop.borrow()), key(&close.borrow()));
        async move {
            let stopped = async {
                if stop.wait_for(|order| key(order) != seen.0).await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            let closed = async {
                if close.wait_for(|order| key(order) != seen.1).await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                () = stopped => {}
                () = closed => {}
            }
        }
    }

    /// The recorded provenance's cause; `Stopped` while none is recorded.
    fn provenance(&self) -> EndCause {
        self.first.map_or(EndCause::Stopped, |first| first.cause)
    }

    /// x.3.2 X4 D4.3 (Q8): a close (Core's close order or the driver's own)
    /// or the session's cancellation detaches a draining turn at once.
    fn detached(&self) -> bool {
        self.cancel.is_cancelled()
            || self.close.borrow().is_some()
            || self
                .stop
                .borrow()
                .as_ref()
                .is_some_and(|order| order.cause == StopCause::Close)
    }

    /// Resolves once [`Self::detached`] holds, from clones of the orders'
    /// receivers.
    fn detaching(&self) -> impl Future<Output = ()> + use<> {
        let (mut stop, mut close, cancel) =
            (self.stop.clone(), self.close.clone(), self.cancel.clone());
        async move {
            let closed = async {
                if close.wait_for(Option::is_some).await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            let stopped = async {
                let close = |order: &Option<StopOrder>| {
                    order
                        .as_ref()
                        .is_some_and(|order| order.cause == StopCause::Close)
                };
                if stop.wait_for(close).await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                () = closed => {}
                () = stopped => {}
                () = cancel.cancelled() => {}
            }
        }
    }

    /// Resolves at the first order (the force excluded), with its own end,
    /// having noted the provenance (x.3.2 X4 D4.2): its cause is the
    /// record's.
    async fn ordered(&mut self) -> Ending {
        let ending = self.first_order().await;
        self.note(Instant::now());
        Ending {
            cause: self.provenance(),
            by: ending.by,
        }
    }

    /// The first order (the force excluded), with its cause and its own
    /// end.
    async fn first_order(&mut self) -> Ending {
        let Self {
            stop,
            close,
            wall,
            cancel,
            ..
        } = self;
        let order = |order: &Option<StopOrder>| {
            order.as_ref().map(|order| Ending {
                cause: EndCause::Stopped,
                by: order.close_by,
            })
        };
        let set = order(&stop.borrow()).or_else(|| order(&close.borrow()));
        if let Some(ending) = set {
            return ending;
        }
        let stopped = async {
            let found = stop
                .wait_for(Option::is_some)
                .await
                .map(|order| order.as_ref().map(|order| order.close_by));
            match found {
                Ok(by) => by,
                Err(_) => std::future::pending().await,
            }
        };
        let closed = async {
            let found = close
                .wait_for(Option::is_some)
                .await
                .map(|order| order.as_ref().map(|order| order.close_by));
            match found {
                Ok(by) => by,
                Err(_) => std::future::pending().await,
            }
        };
        let soon = || Deadline::at(Instant::now() + CLEANUP_ALLOWANCE);
        tokio::select! {
            by = stopped => Ending { cause: EndCause::Stopped, by: by.unwrap_or_else(soon) },
            by = closed => Ending { cause: EndCause::Stopped, by: by.unwrap_or_else(soon) },
            () = tokio::time::sleep_until(wall.instant()) => Ending {
                cause: EndCause::Wall,
                by: Deadline::at(wall.instant() + CLEANUP_ALLOWANCE),
            },
            () = cancel.cancelled() => Ending { cause: EndCause::Stopped, by: soon() },
        }
    }
}

/// X4 critical review (C2 §4.1 "Two deadlines", "One wall cutoff"): the
/// cut of evidence decoded at `decoded`, from attached, decode and wall
/// instants only: the earliest `close_by` of the visible orders (Core's
/// stop, the driver's close relay) attached at or before it, and the
/// wall's cleanup bound once the wall passed at or before it with no
/// order attached earlier. Evidence decoded after its cut is late only.
fn cut_of(
    (stop, close): (&Option<StopOrder>, &Option<StopOrder>),
    wall: Instant,
    decoded: Instant,
) -> Option<Instant> {
    let orders = || [stop, close].into_iter().flatten();
    let close_by = orders()
        .filter(|order| order.attached <= decoded)
        .map(|order| order.close_by.instant())
        .min();
    let walled = (wall <= decoded && orders().all(|order| order.attached >= wall))
        .then_some(wall + CLEANUP_ALLOWANCE);
    close_by.into_iter().chain(walled).min()
}

/// One turn's cutoffs, which its consumer judges each piece of its
/// cleanup evidence by, at the evidence's decode (X4 critical review):
/// the orders' watches (their `attached` instants are the stops' and the
/// detach's), the wall, the P7 grace and the session's cancellation.
pub(crate) struct Cutoffs {
    stop: StopWatch,
    close: watch::Receiver<Option<StopOrder>>,
    wall: Instant,
    tool_grace: std::time::Duration,
    cancel: tokio_util::sync::CancellationToken,
}

impl Cutoffs {
    /// Cutoffs with no order ever, the wall `wall` away and a P7 grace of
    /// `tool_grace`.
    #[cfg(test)]
    pub(crate) fn unbounded(wall: std::time::Duration, tool_grace: std::time::Duration) -> Self {
        Self {
            stop: watch::channel(None).1,
            close: watch::channel(None).1,
            wall: Instant::now() + wall,
            tool_grace,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Whether evidence decoded at `decoded` is past its cut ([`cut_of`]):
    /// late only; a terminal acknowledges nothing and is the turn's late
    /// terminal.
    pub(crate) fn late(&self, decoded: Instant) -> bool {
        cut_of(
            (&self.stop.borrow(), &self.close.borrow()),
            self.wall,
            decoded,
        )
        .is_some_and(|cut| decoded > cut)
    }

    /// Whether the tools of a terminal decoded at `terminal` ending by a
    /// message decoded at `decoded` close its P7 window (C1 §3.5; x.3.2 X4
    /// D4.3, Q8; revision 9 I3): only before the window's end, `tool_grace`
    /// after the terminal, the wall, or a detach (a close's publication:
    /// Core's close order or the driver's close relay; or the session's
    /// cancellation, once seen), whichever is first. A later one proves
    /// nothing: the cleanup stays uncertain.
    pub(crate) fn drains(&self, terminal: Instant, decoded: Instant) -> bool {
        let stop = self.stop.borrow();
        let close = self.close.borrow();
        let detached = [
            stop.as_ref()
                .filter(|order| order.cause == StopCause::Close),
            close.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(|order| order.attached);
        let end = detached
            .chain([terminal + self.tool_grace, self.wall])
            .min()
            .unwrap_or(self.wall);
        decoded < end && !self.cancel.is_cancelled()
    }
}

/// Resolves once the daemon force is raised.
async fn forced(force: &mut ForceWatch) {
    if force.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// One turn's facts as its failures report them.
struct Turn<'a> {
    driver: &'a SessionDriver,
    session: &'a CodexSession,
    number: TurnNumber,
    instance: Option<InstanceReport>,
    /// The turn's first byte was handed to Wire (X0 item 1.6).
    launched: bool,
    /// An earlier request of the turn (its `thread/start` or
    /// `thread/resume`) was handed to Wire before the current one, and
    /// answered: delivery evidence a later request proven unwritten never
    /// erases (bead via-20s review #1).
    delivered: bool,
    /// The settlement, which learns the same.
    settle: &'a Settle<'a>,
}

impl Turn<'_> {
    /// A byte of the turn was handed to Wire: its current request's.
    fn launch(&mut self) {
        self.delivered = self.launched;
        self.launched = true;
        self.settle.launched();
    }

    /// The turn's facts when its current request was proven unwritten:
    /// launched only if an earlier request of it was delivered.
    fn unsent(&self) -> Self {
        Turn {
            launched: self.delivered,
            instance: self.instance.clone(),
            ..*self
        }
    }

    /// The turn's end with `cause`, latched in the health lane as C2 §2
    /// latches a route failure.
    /// While the server lives, the cleanup is the turn's reported tool
    /// items (C2 §2 server-route evidence): none were reported here.
    fn failed(&self, cause: RouteError, loss: Option<ConnectionLoss>) -> TurnEnd {
        self.failure(cause, loss, Some(WireCleanup::Quiescent))
    }

    /// [`Self::failed`] with the cleanup a live server's turn reports:
    /// `None` leaves it unproven (a stop, whose cleanup is P7's).
    fn failure(
        &self,
        cause: RouteError,
        loss: Option<ConnectionLoss>,
        live: Option<WireCleanup>,
    ) -> TurnEnd {
        if let Some(failure) = health_cause(&cause) {
            self.driver.fail(failure);
        }
        let journal_uncertain = loss.is_some_and(|loss| loss.journal_uncertain);
        TurnEnd {
            loss: None,
            aggregate: None,
            terminal: None,
            instance: self.instance.clone(),
            leftovers: None,
            outcome: Err(AdapterError::Route(RouteFailure {
                cause,
                undecoded: None,
                // A server route's turn has no exit of its own (C2 §2).
                exit: None,
                launched: self.launched,
                // A turn that sent nothing has the no-launch evidence (C2
                // §2): its server's loss cleanup is not its own, and an
                // unlaunched failure's cleanup is only acquisition evidence
                // (bead via-20s review #3).
                cleanup: if self.launched {
                    loss.map_or(live, |loss| Some(loss.cleanup))
                } else {
                    None
                },
                forced: false,
                journal_uncertain,
                acknowledged: false,
                shared: true,
                launch: None,
            })),
        }
    }

    /// A definite rejection before acceptance.
    fn rejected(&self, reason: StartRejected) -> TurnEnd {
        TurnEnd {
            loss: None,
            aggregate: None,
            terminal: None,
            instance: self.instance.clone(),
            leftovers: None,
            outcome: Err(AdapterError::Rejected {
                reason,
                evidence: TurnEvidence::no_launch(false),
            }),
        }
    }

    /// Sends one observation of the turn before its acceptance, beside
    /// the daemon force and the turn's own orders (X0: controls stay
    /// serviceable while delivery is blocked; x.3.2 X3 fix r2 #4): while
    /// the sink has no room, a cancel, a close or the wall ends the turn at
    /// once, as nothing of it was started. An undelivered one latches the observation overflow.
    /// A send cut by either never reaches the sink.
    async fn emit(
        &self,
        observation: Observation,
        (orders, force): (&mut Orders, &mut ForceWatch),
    ) -> Result<(), Emit> {
        let item = ObservationItem {
            at: Instant::now(),
            vendor_turn: None,
            observation,
        };
        tokio::select! {
            biased;
            () = forced(force) => Err(Emit::Forced),
            // A sink with room takes it at once, an order or not.
            sent = self.driver.observations.send(item, event_stall()) => sent.map_err(|_| {
                self.driver.fail(DriverFailure::ObservationOverflow);
                Emit::Undelivered
            }),
            ending = orders.ordered() => Err(Emit::Ended(ending.cause)),
        }
    }
}

/// Why a pre-acceptance observation was not sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Emit {
    Undelivered,
    Forced,
    /// The turn's own order: a stop, a close or the wall.
    Ended(EndCause),
}

/// The failure a route cause latches in the health lane (C2 §2).
fn health_cause(cause: &RouteError) -> Option<DriverFailure> {
    match cause {
        RouteError::Protocol { .. }
        | RouteError::TransportLost { .. }
        | RouteError::Overflow { .. }
        | RouteError::Store { .. }
        | RouteError::HandshakeRefused { .. } => Some(DriverFailure::Route(cause.clone())),
        RouteError::ServerLost { .. } => Some(DriverFailure::ServerLost),
        RouteError::ResumeMismatch { .. } => Some(DriverFailure::ResumeMismatch),
        RouteError::ProcessExited { .. }
        | RouteError::Stopped { .. }
        | RouteError::Deadline { .. }
        | RouteError::ForceStopped { .. }
        | RouteError::InvalidParam { .. } => None,
    }
}

/// A lost connection's cause, as its turns report it (X0 item 13.1).
fn loss_cause(loss: &ConnectionLoss, turn: TurnNumber) -> RouteError {
    match loss.cause {
        LossCause::Protocol => RouteError::Protocol {
            turn,
            detail: "the shared connection failed to decode a message",
        },
        LossCause::Overflow => RouteError::Overflow { turn },
        LossCause::ServerLost => RouteError::ServerLost { turn },
        LossCause::TransportLost => RouteError::TransportLost { turn },
    }
}

/// The loss of an ended connection, if it failed.
fn connection_loss(connection: &Connection) -> Option<ConnectionLoss> {
    match connection.ended() {
        Some(ConnectionEnd::Failed(loss)) => Some(loss),
        Some(_) | None => None,
    }
}

/// How an ended lane fails the turn. The connection task's own failure
/// (X0 item 13.2) is a transport loss with its cleanup unproven.
fn lane_end(
    end: LaneEnd,
    connection: &Connection,
    turn: TurnNumber,
) -> (RouteError, Option<ConnectionLoss>) {
    match end {
        // A failed generation's turn reports its cause (`settle_turn`).
        LaneEnd::Overflow | LaneEnd::Quarantined => (RouteError::Overflow { turn }, None),
        LaneEnd::Lost(loss) => (loss_cause(&loss, turn), Some(loss)),
        LaneEnd::Retired => (RouteError::TransportLost { turn }, None),
        // Only a close cuts a lane off, after it stopped the turn.
        LaneEnd::Closed => (RouteError::Stopped { turn }, None),
        LaneEnd::Abnormal => {
            let loss = connection_loss(connection).unwrap_or(ConnectionLoss {
                cause: LossCause::TransportLost,
                cleanup: WireCleanup::Uncertain,
                exit: None,
                journal_uncertain: false,
            });
            (RouteError::TransportLost { turn }, Some(loss))
        }
    }
}

/// The handshake's instance report (C2 §5 OD1): the `userAgent` version
/// and its status.
fn instance_report(user_agent: &str) -> InstanceReport {
    let version = normalize::instance_version(user_agent).map(str::to_owned);
    InstanceReport {
        version_status: version.as_deref().map_or(
            crate::plan::VersionStatus::Untested,
            normalize::version_status,
        ),
        vendor_version: version,
    }
}

/// VIA's own marker in the vendor home (via-25f): written once a server
/// on that home answered its first `initialize`, so Codex's backfill of
/// its thread index is done. Codex's own files are no signal: its SQLite
/// index exists before the backfill completes.
const INITIALIZED: &str = ".via-initialized";

/// The handshake bound of a launch on `home` (via-25f): the first launch's
/// while `home` holds no [`INITIALIZED`] marker, else the warm one. Only
/// the name is checked.
pub(super) fn handshake_bound(home: &Path) -> HandshakeBound {
    match std::fs::symlink_metadata(home.join(INITIALIZED)) {
        Ok(_) => HandshakeBound::Warm,
        Err(_) => HandshakeBound::First,
    }
}

/// Writes the [`INITIALIZED`] marker (empty, 0600, `create_new`, so no
/// link is followed) after a server's handshake succeeded on `home`. Best
/// effort: an existing marker or an error leaves the next launch on the
/// long bound at worst.
pub(super) fn mark_initialized(home: &Path) {
    let marker = home.join(INITIALIZED);
    if std::fs::symlink_metadata(&marker).is_ok() {
        return;
    }
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(marker);
}

/// Creates `<state>/vendor/codex` (0700) when missing; a non-directory or
/// a symlink there is refused.
fn ensure_home(home: &Path) -> std::io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(home) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(home)?.is_dir() {
                Ok(())
            } else {
                Err(std::io::Error::other("the vendor home is not a directory"))
            }
        }
        Err(error) => Err(error),
    }
}

/// Runs one submitted turn (C2 §4.1), inline but for its registration's
/// normalizer, which the session's tracker owns. Its settlement runs however it ends.
pub(crate) async fn run_turn(
    driver: &SessionDriver,
    session: &CodexSession,
    spec: TurnSpec,
    cx: TurnCx,
) -> TurnEnd {
    // A failed driver writes no further turn (x.3.2 X3 fix r4 #5): Core
    // retires it, and its generation is unfit.
    if matches!(*driver.health.borrow(), DriverHealth::Failed { .. }) {
        return rejected(AdapterError::Rejected {
            reason: StartRejected::SessionGone,
            evidence: TurnEvidence::no_launch(false),
        });
    }
    let sandbox = match admit(driver, session, &spec) {
        Ok(sandbox) => sandbox,
        Err(reason) => {
            return rejected(AdapterError::Rejected {
                reason,
                evidence: TurnEvidence::no_launch(false),
            });
        }
    };
    let (close, close_rx) = watch::channel(None);
    let Some(done) = activate(driver, cx.turn, close) else {
        return rejected(AdapterError::Rejected {
            reason: StartRejected::SessionGone,
            evidence: TurnEvidence::no_launch(false),
        });
    };
    let noted = {
        let mut losses = lock_losses(&session.losses);
        losses.latest = Some(cx.turn);
        losses.noted
    };
    let settle = Settle::new((driver, session), cx.turn, done);
    let mut end = turn(driver, session, (spec, sandbox), cx, close_rx, &settle).await;
    // x.3.2 X5 (X0 item 10, simplified by the owner 2026-10-05): a loss
    // noted while this turn ran is this turn's; one noted after it ended
    // reaches only the driver's record.
    end.loss = lock_losses(&session.losses).since(noted);
    settle.record(&end);
    end
}

/// The turn under its settlement.
async fn turn(
    driver: &SessionDriver,
    session: &CodexSession,
    (spec, sandbox): (TurnSpec, Sandbox),
    cx: TurnCx,
    close: watch::Receiver<Option<StopOrder>>,
    settle: &Settle<'_>,
) -> TurnEnd {
    let TurnCx {
        turn,
        prepared,
        capacity,
        activity,
        wall,
        tool_grace,
        stop,
        mut force,
        stop_ack,
    } = cx;
    let mut orders = Orders::new((stop, close), (wall, tool_grace), driver.cancel.clone());
    let mut facts = Turn {
        driver,
        session,
        number: turn,
        instance: None,
        launched: false,
        delivered: false,
        settle,
    };
    let folder = match folder(&facts).await {
        Ok(folder) => Arc::new(folder),
        Err(end) => return *end,
    };
    let pin = match join(&mut facts, (prepared, capacity), (&mut orders, &mut force)).await {
        Ok(pin) => pin,
        Err(end) => return *end,
    };
    let Some((connection, server)) = pin.live() else {
        return facts.failed(RouteError::TransportLost { turn }, None);
    };
    let catalog = adopt(&mut facts, pin.server(), &server);
    let Some(generation) = session.attach(&pin, &connection, driver) else {
        return facts.failed(RouteError::TransportLost { turn }, None);
    };
    let Ok(effort) = vendor_effort(spec.effort.as_deref(), &catalog, &driver.spec.model) else {
        return facts.rejected(StartRejected::InvalidParam { field: "effort" });
    };
    let admitted = admit_turn(
        &facts,
        (&generation, &connection),
        (&mut orders, &mut force),
    )
    .await;
    let (reservation, credit) = match admitted {
        Ok(admitted) => admitted,
        Err(end) => return *end,
    };
    // The turn's input writes: withdrawn before their first byte however
    // the turn ends (X0 item 12.2).
    let mut writes = TurnWrites::new(&connection);
    let thread = if let Some(thread) = generation.thread.clone() {
        thread
    } else {
        let ids = Ids {
            connection: &connection,
            generation: &generation,
        };
        let opened = open_thread(
            &mut facts,
            (&ids, reservation),
            &sandbox,
            (&mut orders, &mut force, &mut writes),
        )
        .await;
        match opened {
            Ok(thread) => thread,
            Err(end) => return *end,
        }
    };
    generation
        .folders
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(turn, Arc::clone(&folder));
    let start = Started {
        thread: &thread,
        connection: &connection,
        generation: generation.number,
        signal: &generation.signal,
        sandbox: &sandbox,
        effort: effort.as_deref(),
        schema: spec.output_schema.is_some(),
    };
    let end = run_started(
        &mut facts,
        &start,
        (spec, credit, stop_ack),
        (&activity, &mut orders, &mut force, &mut writes),
    )
    .await;
    drop(writes);
    quarantine_unfit((driver, session), &connection, (&thread, turn), &end);
    drop(pin);
    end
}

/// Everything before the turn's first write, holding nothing of it
/// meanwhile: a generation resuming the session's thread reserves it
/// first, waiting while another generation's resume, unsubscribe or
/// registration of it is outstanding (x.3.2 X4 D3); the turn is linked to
/// its server; then the thread's start gate and the turn's credit (x.3.2
/// X3 §4.2 steps 0 and 1).
async fn admit_turn(
    facts: &Turn<'_>,
    (generation, connection): (&Generation, &Arc<Connection>),
    (orders, force): (&mut Orders, &mut ForceWatch),
) -> Result<(Option<Reservation>, Charge), Box<TurnEnd>> {
    let driver = facts.driver;
    let identity = driver.state().identity.clone();
    let reservation = match (&generation.thread, identity) {
        (None, Some(thread)) => {
            let waits = (&mut *orders, &mut *force, &*driver.health);
            match reserved(connection, &thread, waits).await {
                Ok(reservation) => Some(reservation),
                Err(why) => {
                    return Err(Box::new(uncredited(
                        facts,
                        connection,
                        why,
                        (orders, force),
                    )));
                }
            }
        }
        (Some(_) | None, _) => None,
    };
    link(facts, connection, orders.wall).await?;
    let cap = facts.session.cap(driver);
    let thread = generation.thread.as_deref();
    let gate = thread.map(|thread| (thread.lease.lane().as_ref(), &*thread.registration));
    match credited(&cap, (&mut *orders, &mut *force, &*driver.health), gate).await {
        Ok(credit) => Ok((reservation, credit)),
        Err(why) => Err(Box::new(uncredited(
            facts,
            connection,
            why,
            (orders, force),
        ))),
    }
}

/// X0 item 5 (x.3.2 X3 fix r1 #4): a turn ending with a malformed
/// message's protocol failure reports where that message was kept, once
/// the consumer kept it after failing.
pub(super) async fn with_undecoded(mut end: TurnEnd, registration: &Registration) -> TurnEnd {
    if let Err(AdapterError::Route(failure)) = &mut end.outcome
        && matches!(
            failure.cause,
            RouteError::Protocol {
                detail: UNDECODED,
                ..
            }
        )
        && failure.undecoded.is_none()
    {
        failure.undecoded = registration.undecoded().await;
    }
    end
}

/// Quarantines the generation a turn's `end` leaves unfit for the next
/// turn: a failure latched in the health fails its registration first
/// (x.3.2 X3 §4.1).
fn quarantine_unfit(
    (driver, session): (&SessionDriver, &CodexSession),
    connection: &Arc<Connection>,
    (thread, turn): (&Thread, TurnNumber),
    end: &TurnEnd,
) {
    let overflowed = thread.lease.lane().overflowed_now();
    if !(quarantines(end) || !usable(connection) || overflowed) {
        return;
    }
    let cause = match &end.outcome {
        Err(AdapterError::Route(failure)) => health_cause(&failure.cause),
        Ok(_) | Err(_) => None,
    }
    .or_else(|| overflowed.then_some(DriverFailure::Route(RouteError::Overflow { turn })));
    session.quarantine(connection, cause.map(|cause| (cause, &*driver.health)));
}

/// The bytes of a maximal vendor turn ID (C2 A1): a turn's credit is
/// sized for it (x.3.2 X3 §3.4).
const VENDOR_ID_MAX: usize = 1024;

/// Why a turn's credit was not reserved (x.3.2 X3 §4.2 step 1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Uncredited {
    /// The session's cap is full: the connection's overflow (§6.4).
    Exhausted,
    /// The turn's own order: a stop, the driver's close, the wall or the
    /// session's cancellation.
    Ordered,
    /// The daemon force.
    Forced,
    /// The driver's health failed.
    Failed,
    /// The budget stayed full past the stall bound.
    Stalled,
    /// The registration's generation failed (with its cause), or the
    /// registration retired (F3).
    Gone(Option<DriverFailure>),
    /// The connection ended while the turn waited on its thread's fence
    /// (x.3.2 X4 D3), with how: its failure keeps its own class.
    Lost(ConnectionEnd),
}

/// x.3.2 X4 D3: reserves `thread` on `connection` for the generation's
/// resume, waiting on the connection's epoch while it is fenced, beside
/// the daemon force, the connection's end, the driver's failure and the
/// turn's orders, any of which ends the wait with nothing reserved.
async fn reserved(
    connection: &Arc<Connection>,
    thread: &str,
    (orders, force, health): (&mut Orders, &mut ForceWatch, &watch::Sender<DriverHealth>),
) -> Result<Reservation, Uncredited> {
    let mut health = health.subscribe();
    loop {
        let mut epoch = match connection.reserve(thread) {
            Ok(reservation) => return Ok(reservation),
            Err(Fenced::Busy(epoch)) => Some(epoch),
            // The end's cause comes from the arm below.
            Err(Fenced::Ended) => None,
        };
        let cleared = async {
            let changed = match epoch.as_mut() {
                Some(epoch) => epoch.changed().await.is_ok(),
                None => false,
            };
            if !changed {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            () = forced(force) => return Err(Uncredited::Forced),
            end = connection.end() => return Err(Uncredited::Lost(end)),
            _ = health.wait_for(|health| matches!(health, DriverHealth::Failed { .. })) => {
                return Err(Uncredited::Failed);
            }
            _ = orders.ordered() => return Err(Uncredited::Ordered),
            () = cleared => {}
        }
    }
}

/// x.3.2 X3 §4.2 steps 0 and 1: on a registered thread (its lane and
/// registration) the start gate is waited for first, then the turn's
/// credit is reserved.
pub(super) async fn credited(
    cap: &SessionCap,
    (orders, force, health): (&mut Orders, &mut ForceWatch, &watch::Sender<DriverHealth>),
    thread: Option<(&Lane, &Registration)>,
) -> Result<Charge, Uncredited> {
    if let Some(gate) = thread {
        start_gate((orders, force), gate).await?;
    }
    let registration = thread.map(|(_, registration)| registration);
    credit(cap, (orders, force, health), registration).await
}

/// x.3.2 X3 §4.2 step 0: waits for the thread's start gate (§2.2) before
/// any credit is reserved, beside the turn's orders (a stop, the driver's
/// close, the wall or the session's cancellation), the daemon force, and
/// the failure or retirement of the registration. No stall arm: a vendor
/// slow to answer the predecessor's start is no consumer stall. Any arm
/// but the gate ends the wait with nothing launched.
pub(super) async fn start_gate(
    (orders, force): (&mut Orders, &mut ForceWatch),
    (lane, registration): (&Lane, &Registration),
) -> Result<(), Uncredited> {
    tokio::select! {
        biased;
        () = forced(force) => Err(Uncredited::Forced),
        () = registration.gone() => Err(Uncredited::Gone(registration.failure())),
        _ = orders.ordered() => Err(Uncredited::Ordered),
        () = lane.start_gate() => Ok(()),
    }
}

/// x.3.2 X3 §4.2 step 1: reserves the turn's credit before any of its jobs
/// exists. The cap slot is taken at once; the budget bytes are awaited
/// beside the turn's orders, the daemon force, the failure of its
/// generation's `registration` (once it has one) or its retirement, the
/// driver's failure and the stall bound, and any of them ends the wait with
/// nothing reserved.
pub(super) async fn credit(
    cap: &SessionCap,
    (orders, force, health): (&mut Orders, &mut ForceWatch, &watch::Sender<DriverHealth>),
    registration: Option<&Registration>,
) -> Result<Charge, Uncredited> {
    let slot = cap
        .credit_slot(VENDOR_ID_MAX)
        .ok_or(Uncredited::Exhausted)?;
    let slot = match cap.try_charge(slot) {
        Ok(credit) => return Ok(credit),
        Err(slot) => slot,
    };
    let mut health = health.subscribe();
    let gone = async {
        match registration {
            Some(registration) => registration.gone().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = forced(force) => Err(Uncredited::Forced),
        () = gone => Err(Uncredited::Gone(registration.and_then(Registration::failure))),
        _ = health.wait_for(|health| matches!(health, DriverHealth::Failed { .. })) => {
            Err(Uncredited::Failed)
        }
        _ = orders.ordered() => Err(Uncredited::Ordered),
        charged = tokio::time::timeout(event_stall(), cap.charge(slot)) => {
            charged.ok().flatten().ok_or(Uncredited::Stalled)
        }
    }
}

/// A turn whose credit was not reserved ends before any of its jobs
/// exists, with nothing launched (x.3.2 X3 §4.2 step 1).
fn uncredited(
    facts: &Turn<'_>,
    connection: &Connection,
    why: Uncredited,
    (orders, force): (&Orders, &ForceWatch),
) -> TurnEnd {
    let turn = facts.number;
    match why {
        Uncredited::Exhausted => {
            connection.fail(ConnectionFailure::Overflow);
            facts.failed(RouteError::Overflow { turn }, None)
        }
        Uncredited::Stalled => {
            facts.driver.fail(DriverFailure::ObservationOverflow);
            facts.failed(RouteError::Overflow { turn }, None)
        }
        Uncredited::Gone(Some(cause)) => facts.failed(generation_cause(&cause, turn), None),
        Uncredited::Gone(None) | Uncredited::Failed => facts.rejected(StartRejected::SessionGone),
        Uncredited::Lost(ConnectionEnd::Failed(loss)) => {
            facts.failed(loss_cause(&loss, turn), Some(loss))
        }
        Uncredited::Lost(ConnectionEnd::Retired) => {
            facts.failed(RouteError::TransportLost { turn }, None)
        }
        Uncredited::Ordered | Uncredited::Forced => {
            facts.failed(unsent_cause(orders, force, turn), None)
        }
    }
}

/// The turn's evidence folder, where its undecoded messages go.
async fn folder(facts: &Turn<'_>) -> Result<TurnFolder, Box<TurnEnd>> {
    let turn = facts.number;
    let driver = facts.driver;
    match driver
        .runtime
        .turn_folder(&driver.spec.session_id, turn)
        .await
    {
        Ok(folder) => Ok(folder),
        Err(_) => Err(Box::new(facts.failed(
            RouteError::Store {
                turn,
                kind: StoreFailure::Evidence,
            },
            None,
        ))),
    }
}

/// Makes the turn the driver's active one; `None` once the session closed.
/// The sender is the turn's retirement, which [`Settle`] completes.
fn activate(
    driver: &SessionDriver,
    turn: TurnNumber,
    close: watch::Sender<Option<StopOrder>>,
) -> Option<watch::Sender<Retiring>> {
    let (done, retiring) = watch::channel(Retiring::Running);
    let mut state = driver.state();
    if state.closed {
        return None;
    }
    let (steer, _unused) = via_routes::steer::steer_lane(str::len);
    state.active = Some(Active::new(turn, steer, close));
    state.retiring = Some(retiring);
    Some(done)
}

/// The turn's effort as the vendor names it, refused when the session's
/// model is in the catalog and does not advertise it (packet §4).
pub(super) fn vendor_effort(
    requested: Option<&str>,
    catalog: &[DiscoveredModel],
    model: &str,
) -> Result<Option<String>, ()> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    let effort = codex_plan::canonical_effort(requested).unwrap_or(requested);
    match catalog.iter().find(|entry| entry.model == model) {
        Some(entry) if !entry.supports(effort) => Err(()),
        Some(_) | None => Ok(Some(effort.to_owned())),
    }
}

/// Whether a turn's end leaves its generation unfit for the next turn: any
/// route failure but the turn's own stop or deadline.
fn quarantines(end: &TurnEnd) -> bool {
    matches!(&end.outcome, Err(AdapterError::Route(failure))
        if !matches!(failure.cause, RouteError::Stopped { .. } | RouteError::Deadline { .. }))
}

/// The turn's pure refusals and its bound's sandbox (C2 §4.1): a turn
/// naming no bound applies the session's last, else its initial one; one
/// with none at all is refused, as Codex applies an explicit bound on every
/// turn.
fn admit(
    driver: &SessionDriver,
    session: &CodexSession,
    spec: &TurnSpec,
) -> Result<Sandbox, StartRejected> {
    let route = Harness::parse(HARNESS).map_or(HARNESS, Harness::route);
    let per_turn = PerTurn {
        effort: spec.effort.as_deref(),
        bound: spec.bound.as_ref(),
        max_steps: spec.max_steps.is_some(),
        vendor: &spec.vendor,
        // C2 §6.3: re-judged before every launch.
        vendor_args: &driver.spec.vendor_args,
    };
    if let Some(refusal) = refusals(route, &per_turn).into_iter().next() {
        return Err(start_rejected(&refusal.kind, refusal.message));
    }
    let bound = {
        let mut current = session.bound.lock().unwrap_or_else(PoisonError::into_inner);
        let bound = spec
            .bound
            .clone()
            .or_else(|| current.clone())
            .or_else(|| driver.spec.initial_bound.clone());
        current.clone_from(&bound);
        bound
    };
    match bound.as_ref().map(codex_plan::sandbox) {
        Some(Ok(sandbox)) => Ok(sandbox),
        Some(Err(_)) | None => Err(StartRejected::BoundUnsupported(format!(
            "route {route} applies an explicit bound on every turn: name one"
        ))),
    }
}

/// Records the live server's facts the turn reports (its instance and
/// version) and caches its model catalog for instance `id`, which it
/// returns.
fn adopt(
    facts: &mut Turn<'_>,
    id: &via_routes::codex::ServerId,
    server: &via_routes::codex::ServerFacts,
) -> Arc<[DiscoveredModel]> {
    let adapter = &facts.session.adapter;
    facts.instance = Some(instance_report(&server.user_agent));
    if let Some(version) = normalize::instance_version(&server.user_agent) {
        adapter
            .instances
            .record_version(HARNESS, &adapter.binary, version.to_owned());
    }
    let catalog: Arc<[DiscoveredModel]> = server.models.iter().map(normalize::discovered).collect();
    let spec = &facts.driver.spec;
    let key = adapter.server_key(spec.inherit.requested, &spec.vendor_args);
    adapter.discovered(key, id.clone(), Arc::clone(&catalog));
    catalog
}

/// Links the turn to its server in the Store before anything of it is
/// written (runtime §6 `server_turns`), within [`LINK_BOUND`].
async fn link(
    facts: &Turn<'_>,
    connection: &Connection,
    wall: Deadline,
) -> Result<(), Box<TurnEnd>> {
    let driver = facts.driver;
    let turn = facts.number;
    let link_by = Deadline::at((Instant::now() + LINK_BOUND).min(wall.instant()));
    let kind = match connection
        .link_turn(&driver.spec.session_id, turn, link_by)
        .await
    {
        CommitOutcome::Committed(()) => return Ok(()),
        CommitOutcome::NotCommitted(_) => StoreFailure::NotCommitted,
        CommitOutcome::Uncertain(_) => {
            driver.journal.send_replace(true);
            StoreFailure::Uncertain
        }
    };
    Err(Box::new(
        facts.failed(RouteError::Store { turn, kind }, None),
    ))
}

/// The pin of the turn's server, live: the session's own or an equal
/// key's, else a launch or join with the turn's harness-process slot.
async fn join(
    facts: &mut Turn<'_>,
    (prepared, capacity): (Prepared, Option<crate::CapacityToken>),
    (orders, force): (&mut Orders, &mut ForceWatch),
) -> Result<ServerPin, Box<TurnEnd>> {
    let turn = facts.number;
    let driver = facts.driver;
    let adapter = &facts.session.adapter;
    let pin = match prepared {
        Prepared::Pinned(ConnectionPin {
            server: Some(pin), ..
        }) => pin,
        // A pin naming no server (a stand-in's): nothing was sent.
        Prepared::Pinned(_) => {
            return Err(Box::new(facts.rejected(StartRejected::SessionGone)));
        }
        Prepared::NeedsConnection => {
            let Some(capacity) = capacity else {
                return Err(Box::new(facts.rejected(StartRejected::Protocol(
                    "a new server needs a harness-process slot".to_owned(),
                ))));
            };
            let home = adapter.vendor_home();
            if ensure_home(&home).is_err() {
                return Err(Box::new(facts.failed(
                    RouteError::Store {
                        turn,
                        kind: StoreFailure::Evidence,
                    },
                    None,
                )));
            }
            let recipe = adapter.recipe(driver.spec.inherit.requested, &driver.spec.vendor_args);
            let owner = ProcessOwner::Turn {
                session_id: driver.spec.session_id.clone(),
                turn,
            };
            let key = ServerKey(recipe.config_hash(ADAPTER_VERSION).bytes());
            match adapter.servers().launch_or_join(
                key,
                (recipe.process_spec(owner), handshake_bound(&home)),
                capacity,
            ) {
                Ok(pin) => pin,
                Err(failure) => {
                    return Err(Box::new(launch_failed(facts, &failure.into())));
                }
            }
        }
    };
    let ordered = async {
        tokio::select! {
            _ending = orders.ordered() => {}
            () = forced(force) => {}
        }
    };
    match pin.ready(ordered).await {
        Ok(()) => {
            // Whichever waiter sees the server ready, launcher, joiner or
            // pinned, marks its home (review cfix-crit minor).
            mark_initialized(&adapter.vendor_home());
            Ok(pin)
        }
        Err(Some(failure)) => Err(Box::new(launch_failed(facts, &failure))),
        // The turn's own order ended its wait: nothing of it was sent.
        Err(None) => Err(Box::new(
            facts.failed(unsent_cause(orders, force, turn), None),
        )),
    }
}

/// A launch's failure as the waiting turn reports it (C2 §2 health), with
/// the instance its handshake read, when it read one (C2 AD7: every later
/// outcome carries it).
fn launch_failed(facts: &mut Turn<'_>, failure: &LaunchError) -> TurnEnd {
    if let Some(user_agent) = &failure.user_agent {
        facts.instance = Some(instance_report(user_agent));
    }
    let failure = failure.failure.route_failure(facts.number);
    if let Some(cause) = health_cause(&failure.cause) {
        facts.driver.fail(cause);
    }
    TurnEnd {
        loss: None,
        aggregate: None,
        terminal: None,
        instance: facts.instance.clone(),
        leftovers: None,
        outcome: Err(AdapterError::Route(failure)),
    }
}

/// The cause of a turn its order ended before anything was sent.
pub(super) fn unsent_cause(orders: &Orders, force: &ForceWatch, turn: TurnNumber) -> RouteError {
    if force.borrow().is_some() {
        RouteError::ForceStopped { turn }
    } else if Instant::now() >= orders.wall.instant() {
        RouteError::Deadline { turn }
    } else {
        RouteError::Stopped { turn }
    }
}

/// The connection facts a thread open needs.
struct Ids<'a> {
    connection: &'a Arc<Connection>,
    generation: &'a Generation,
}

/// Opens the session's thread on this generation (packet §3): a resume of
/// the confirmed thread its `reservation` holds (x.3.2 X4 D3), else a
/// start, written under the turn's guard on a lane subscribed to the
/// generation's abnormal end; the reply's echoes checked, its identity
/// confirmed. Any failure drops the lane, which closes a registration the
/// reply made (X0 item 8.1).
async fn open_thread(
    facts: &mut Turn<'_>,
    (ids, reservation): (&Ids<'_>, Option<Reservation>),
    sandbox: &Sandbox,
    (orders, force, writes): (&mut Orders, &mut ForceWatch, &mut TurnWrites),
) -> Result<Arc<Thread>, Box<TurnEnd>> {
    let driver = facts.driver;
    let turn = facts.number;
    let lease = ids.connection.open_lane(Some(&ids.generation.signal));
    let resume = reservation
        .as_ref()
        .map(|reservation| reservation.thread().to_owned());
    let settings = ThreadSettings {
        model: &driver.spec.model,
        cwd: &driver.spec.cwd,
        developer_instructions: driver.spec.instructions.as_deref(),
        sandbox: sandbox.mode,
    };
    let bounds = WriteBounds::StartBy {
        start_by: orders.wall,
        finish_by: Deadline::at(Instant::now() + FINISH_BY),
    };
    let requested = ids.connection.request(
        |id| match &resume {
            Some(thread) => thread_resume(id, thread, &settings).map(data),
            None => thread_start(id, &settings).map(data),
        },
        bounds,
        Purpose::Opens {
            lane: &lease,
            reservation,
        },
        Some(writes),
    );
    let requested = requested.map_err(|error| Box::new(request_failed(facts, error)))?;
    facts.launch();
    let reply = await_reply(
        (requested.written, requested.reply),
        (orders, force, None),
        &mut |_ending| {},
    )
    .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(cause) => return Err(Box::new(lost(facts, ids.connection, cause))),
    };
    let opened = match reply.outcome {
        Ok(raw) => result::<ThreadResult>(&raw),
        Err(error) => {
            return Err(Box::new(
                facts.rejected(thread_refused(&error, resume.is_some())),
            ));
        }
    };
    let Ok(opened) = opened else {
        return Err(Box::new(facts.failed(
            RouteError::Protocol {
                turn,
                detail: "the thread reply is malformed",
            },
            None,
        )));
    };
    confirm(facts, (&opened, sandbox), resume, &lease, (orders, force)).await?;
    let thread = Arc::new(Thread {
        id: opened.thread.id.clone(),
        registration: normalize_on_tracker(driver, facts.session, ids, &lease),
        lease,
    });
    facts.session.opened(ids.connection, &thread, driver);
    driver.state().identity = Some(thread.id.clone());
    let identity = Identity {
        vendor_session_id: thread.id.clone(),
        connection_id: connection_id(ids.generation.number),
        transcript: None,
        vendor_version: facts
            .instance
            .as_ref()
            .and_then(|instance| instance.vendor_version.clone()),
    };
    match facts
        .emit(Observation::IdentityConfirmed(identity), (orders, force))
        .await
    {
        Ok(()) => Ok(thread),
        Err(Emit::Undelivered) => Err(Box::new(facts.failed(RouteError::Overflow { turn }, None))),
        Err(Emit::Forced) => Err(Box::new(facts.failure(
            RouteError::ForceStopped { turn },
            None,
            None,
        ))),
        // Nothing of the turn was started: only its thread was opened.
        Err(Emit::Ended(EndCause::Stopped)) => {
            Err(Box::new(facts.failed(RouteError::Stopped { turn }, None)))
        }
        Err(Emit::Ended(EndCause::Wall)) => {
            Err(Box::new(facts.failed(RouteError::Deadline { turn }, None)))
        }
    }
}

/// Checks an opened thread before its identity is confirmed: the echoed
/// policy, a resume's thread ID, and its registration on the connection.
async fn confirm(
    facts: &Turn<'_>,
    (opened, sandbox): (&ThreadResult, &Sandbox),
    resume: Option<String>,
    lease: &LaneLease,
    controls: (&mut Orders, &mut ForceWatch),
) -> Result<(), Box<TurnEnd>> {
    let driver = facts.driver;
    let turn = facts.number;
    let echoed = Echoed {
        model: &driver.spec.model,
        cwd: &driver.spec.cwd,
        sandbox,
    };
    if let Some(field) = echo_differs(opened, &echoed) {
        let adapter = &facts.session.adapter;
        // The refused handshake is cached under exactly what was compared,
        // on the server its vendor args select.
        let hash = adapter.refusal_key(
            driver.spec.inherit.requested,
            &driver.spec.vendor_args,
            &echoed,
        );
        adapter.instances.record_refusal(
            &adapter.binary,
            hash,
            Incompatibility::ReadbackDiffers(field),
            std::time::Instant::now(),
        );
        return Err(Box::new(
            facts.failed(RouteError::HandshakeRefused { turn, detail: None }, None),
        ));
    }
    let returned = &opened.thread.id;
    if let Some(requested) = resume
        && requested != *returned
    {
        driver.fail(DriverFailure::ResumeMismatch);
        // C2 §4 identity: the mismatch is reported; an undelivered report
        // latches the observation overflow after the mismatch.
        let mismatch = Observation::ResumeMismatch {
            requested,
            returned: returned.clone(),
        };
        let _undelivered = facts.emit(mismatch, controls).await;
        return Err(Box::new(TurnEnd {
            loss: None,
            aggregate: None,
            terminal: None,
            instance: facts.instance.clone(),
            leftovers: None,
            outcome: Err(AdapterError::ResumeMismatch {
                evidence: TurnEvidence {
                    exit: None,
                    cleanup: Cleanup::Quiescent,
                    journal_uncertain: false,
                },
            }),
        }));
    }
    if lease.thread().as_deref() != Some(returned.as_str()) {
        return Err(Box::new(facts.failed(
            RouteError::Protocol {
                turn,
                detail: "the thread is already open on the connection",
            },
            None,
        )));
    }
    Ok(())
}

/// The first echoed field that is not the one requested (packet §3:
/// never, the user reviewer, the bound's sandbox, the session's model and
/// directory). Only the sandbox's type is compared: the 0.160.0 echo of a
/// workspace-write thread has `excludeSlashTmp` and `excludeTmpdirEnvVar`
/// false while every `turn/start` applies them true (via-5lr.3.4).
fn echo_differs(opened: &ThreadResult, echoed: &Echoed<'_>) -> Option<&'static str> {
    let sandbox = match echoed.sandbox.mode {
        SandboxMode::ReadOnly => "readOnly",
        SandboxMode::WorkspaceWrite => "workspaceWrite",
        SandboxMode::DangerFullAccess => "dangerFullAccess",
    };
    if opened.model != echoed.model {
        Some("model")
    } else if Path::new(&opened.cwd) != echoed.cwd {
        Some("cwd")
    } else if opened.approval_policy.as_str() != Some("never") {
        Some("approvalPolicy")
    } else if opened.approvals_reviewer.as_deref() != Some("user") {
        Some("approvalsReviewer")
    } else if opened.sandbox["type"].as_str() != Some(sandbox) {
        Some("sandbox")
    } else {
        None
    }
}

/// A thread request's error reply as a definite rejection: a resumed
/// thread the vendor no longer has is gone (packet §2).
fn thread_refused(error: &RpcError, resume: bool) -> StartRejected {
    if resume && error.message.contains(NO_ROLLOUT) {
        StartRejected::SessionGone
    } else {
        StartRejected::VendorError(
            Some(VendorCode::from(error.code.to_string())),
            error.message.clone(),
        )
    }
}

/// A request the connection did not take: nothing of it was written.
fn request_failed(facts: &Turn<'_>, error: RequestError) -> TurnEnd {
    let turn = facts.number;
    match error {
        RequestError::Closed => facts.failed(RouteError::TransportLost { turn }, None),
        RequestError::Exhausted => facts.failed(RouteError::Overflow { turn }, None),
        RequestError::Encode(error) => facts.rejected(StartRejected::Protocol(error.to_string())),
    }
}

/// Why a reply never came.
pub(super) enum Unanswered {
    /// The connection ended or its record went.
    Lost,
    /// The request was not written whole.
    NotWritten(SendOutcome),
    /// The daemon force.
    Forced,
    /// The turn's own order reached its end.
    Ended(EndCause),
    /// The registration's lane overflowed (x.3.2 X3 fix r3 #1).
    Overflow,
    /// The registration's generation failed with `cause` (x.3.2 X3 §4.3):
    /// the request was written, or may have been.
    Generation {
        cause: DriverFailure,
        launched: bool,
    },
}

/// A turn whose request went unanswered.
fn lost(facts: &Turn<'_>, connection: &Connection, cause: Unanswered) -> TurnEnd {
    let turn = facts.number;
    let ended = |facts: &Turn<'_>| match connection_loss(connection) {
        Some(loss) => facts.failed(loss_cause(&loss, turn), Some(loss)),
        None => facts.failed(RouteError::TransportLost { turn }, None),
    };
    match cause {
        Unanswered::NotWritten(SendOutcome::NotWritten) => ended(&facts.unsent()),
        Unanswered::Lost | Unanswered::NotWritten(_) => ended(facts),
        // A stop's cleanup is uncertain: the written request may have
        // started work the vendor never reported, and early traffic of it
        // is lost (x.3.2 X3 §3.2, S8's proviso).
        Unanswered::Forced => facts.failure(
            RouteError::ForceStopped { turn },
            None,
            Some(WireCleanup::Uncertain),
        ),
        Unanswered::Ended(EndCause::Stopped) => facts.failure(
            RouteError::Stopped { turn },
            None,
            Some(WireCleanup::Uncertain),
        ),
        Unanswered::Ended(EndCause::Wall) => facts.failure(
            RouteError::Deadline { turn },
            None,
            Some(WireCleanup::Uncertain),
        ),
        // What was dropped proves nothing: the cleanup is unproven.
        Unanswered::Overflow => facts.failure(
            RouteError::Overflow { turn },
            None,
            Some(WireCleanup::Uncertain),
        ),
        // x.3.2 X3 §4.3: a withdrawn write is no launch; a written one
        // leaves the cleanup unproven.
        Unanswered::Generation { cause, launched } => {
            let cause = generation_cause(&cause, turn);
            if launched {
                facts.failure(cause, None, Some(WireCleanup::Uncertain))
            } else {
                facts.unsent().failed(cause, None)
            }
        }
    }
}

/// The generation's failure `cause` as an admitted turn `turn` ends with
/// it (x.3.2 X3 §4.3): a protocol failure is the turn's protocol error;
/// any other is its overflow, as an idle failure reports it.
fn generation_cause(cause: &DriverFailure, turn: TurnNumber) -> RouteError {
    match cause {
        DriverFailure::Route(RouteError::Protocol { detail, .. }) => {
            RouteError::Protocol { turn, detail }
        }
        DriverFailure::Route(_)
        | DriverFailure::ObservationOverflow
        | DriverFailure::OwnedTask
        | DriverFailure::TurnAbandoned
        | DriverFailure::ServerLost
        | DriverFailure::ResumeMismatch
        | DriverFailure::RetirementUncertain => RouteError::Overflow { turn },
    }
}

/// Awaits a queued request's paired reply, bounded by the force, the
/// failure of the turn's registration (once it has one) and the overflow
/// of its `lane`, and the end of the turn's own order; `on_order` runs
/// once, at the order.
pub(super) async fn await_reply(
    (written, reply): (oneshot::Receiver<SendOutcome>, oneshot::Receiver<Response>),
    (orders, force, thread): (&mut Orders, &mut ForceWatch, Option<(&Lane, &Registration)>),
    on_order: &mut (dyn FnMut(&Ending) + Send),
) -> Result<Response, Unanswered> {
    tokio::pin!(written);
    tokio::pin!(reply);
    let overflowed = async {
        match thread {
            Some((lane, _)) => lane.overflowed().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(overflowed);
    let failed = async {
        match thread {
            Some((_, registration)) => registration.failed().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(failed);
    let mut answered = false;
    let mut ending: Option<Ending> = None;
    loop {
        let end_at = ending.map(|ending| ending.by.instant());
        tokio::select! {
            biased;
            () = forced(force) => return Err(Unanswered::Forced),
            // x.3.2 X3 §4.2 step 4: the generation failed and cancelled the
            // turn's writes; whether the request was written decides. An
            // acceptance already paired goes on, so the turn's cleanup
            // interrupt names its vendor ID; any other reply ends through
            // the failure (§4.3: launched, cleanup uncertain).
            () = &mut failed => {
                let replied = match reply.as_mut().get_mut().try_recv() {
                    Ok(paired) if paired.outcome.is_ok() => return Ok(paired),
                    paired => paired.is_ok(),
                };
                let written = if answered || replied {
                    Ok(SendOutcome::Written)
                } else {
                    (&mut written).await
                };
                let cause = thread
                    .and_then(|(_, registration)| registration.failure())
                    .unwrap_or(DriverFailure::ObservationOverflow);
                return Err(Unanswered::Generation {
                    cause,
                    launched: !matches!(written, Ok(SendOutcome::NotWritten)),
                });
            }
            // Vendor §5: a quarantined generation's turns end at once.
            () = &mut overflowed => return Err(Unanswered::Overflow),
            outcome = &mut written, if !answered => match outcome {
                Ok(SendOutcome::Written) => answered = true,
                Ok(other) => return Err(Unanswered::NotWritten(other)),
                // The connection ended before Wire answered.
                Err(_) => return Err(Unanswered::Lost),
            },
            paired = &mut reply => return paired.map_err(|_| Unanswered::Lost),
            found = orders.ordered(), if ending.is_none() => {
                on_order(&found);
                ending = Some(found);
            }
            () = sleep_until(end_at), if end_at.is_some() => {
                return Err(Unanswered::Ended(ending.map_or(EndCause::Stopped, |ending| ending.cause)));
            }
        }
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl Started<'_> {
    /// The thread's lane and registration.
    fn registered(&self) -> (&Lane, &Registration) {
        (self.thread.lease.lane(), &self.thread.registration)
    }
}

/// A started turn's connection facts.
struct Started<'a> {
    thread: &'a Arc<Thread>,
    connection: &'a Arc<Connection>,
    generation: u64,
    /// The generation's abnormal-end signal: its decode positions.
    signal: &'a LeaseSignal,
    sandbox: &'a Sandbox,
    effort: Option<&'a str>,
    schema: bool,
}

/// Writes `turn/start` with the full frozen policy under the turn's guard;
/// a stop before its reply posts the delayed interrupt intent. Its `Start`
/// marker carries the turn's delivery and credit to the registration's
/// consumer (x.3.2 X3 §2.1), which accepts the turn at its `Reply` in
/// decode order; the turn waits for its decision beside its orders.
async fn run_started(
    facts: &mut Turn<'_>,
    start: &Started<'_>,
    (spec, credit, stop_ack): (TurnSpec, Charge, crate::StopAck),
    (activity, orders, force, writes): (
        &crate::TurnActivity,
        &mut Orders,
        &mut ForceWatch,
        &mut TurnWrites,
    ),
) -> TurnEnd {
    let driver = facts.driver;
    let turn = facts.number;
    let values = TurnStart {
        thread_id: &start.thread.id,
        cwd: &driver.spec.cwd,
        model: &driver.spec.model,
        effort: start.effort,
        output_schema: spec.output_schema.as_deref(),
        sandbox_policy: &start.sandbox.policy,
    };
    let prompt = spec.prompt;
    let bounds = WriteBounds::StartBy {
        start_by: orders.wall,
        finish_by: Deadline::at(Instant::now() + FINISH_BY),
    };
    let Some((_admission, delivery)) = admitted(facts, start, writes).await else {
        return facts.rejected(StartRejected::SessionGone);
    };
    let correlation = Arc::new(OnceLock::new());
    let cx = StartCx {
        delivery: Arc::clone(&delivery),
        activity: activity.clone(),
        schema: start.schema,
        instance: facts.instance.clone(),
        correlation: Arc::clone(&correlation),
        credit,
        stop_ack,
        cutoffs: orders.cutoffs(),
    };
    let requested = start.connection.request(
        |id| {
            let _set = correlation.set(acceptance_token(id));
            turn_start(id, &values, prompt)
        },
        bounds,
        Purpose::Starts {
            lane: &start.thread.lease,
            turn,
            decoded: activity.decode_watermark(),
            cx: Box::new(cx),
        },
        Some(writes),
    );
    let requested = match requested {
        Ok(requested) => requested,
        Err(error) => return request_failed(facts, error),
    };
    facts.launch();
    let start_id = requested.id;
    let reply = await_reply(
        (requested.written, requested.reply),
        (orders, force, Some(start.registered())),
        &mut |ending: &Ending| {
            // Before acceptance the intent waits on the start's reply.
            start
                .connection
                .interrupt(&start.thread.lease, start_id, None, ending.by);
        },
    )
    .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(cause) => {
            let end = unanswered(facts, start, start_id, cause);
            return with_undecoded(end, &start.thread.registration).await;
        }
    };
    let accepted = match reply.outcome {
        Ok(raw) => match result::<TurnStartResult>(&raw) {
            Ok(accepted) => accepted.turn.id,
            // Packet §3: the start was written, and the vendor may run a
            // turn VIA cannot name: its cleanup is unproven.
            Err(_) => {
                return facts.failure(
                    RouteError::Protocol {
                        turn,
                        detail: "the turn/start reply is malformed",
                    },
                    None,
                    Some(WireCleanup::Uncertain),
                );
            }
        },
        // Packet lines 144–145: a refusal the lane contradicts fails the
        // generation; the turn is not rejected (x.3.2 X3 §3.2).
        Err(_) if reply.contradicted => return contradicted(facts, start, start_id),
        Err(error) => {
            return facts.rejected(StartRejected::VendorError(
                Some(VendorCode::from(error.code.to_string())),
                error.message,
            ));
        }
    };
    let accepted_turn = Accepted {
        id: accepted,
        start: start_id,
        delivery: &delivery,
    };
    let cut = wait(start, &accepted_turn, (orders, force)).await;
    cut_seam().await;
    let end = settle_turn(facts, (start, &accepted_turn), orders, cut);
    with_undecoded(end, &start.thread.registration).await
}

/// The acceptance token of the start request `id`.
fn acceptance_token(id: ClientId) -> AcceptanceToken {
    u64::try_from(id.get())
        .ok()
        .and_then(|id| AcceptanceToken::try_from(id).ok())
        .unwrap_or(AcceptanceToken::FIRST)
}

/// A refused start the lane contradicts (x.3.2 X3 §3.2, §4.1): the
/// generation fails `protocol` (first wins with the consumer), and the
/// turn ends with its cause, launched, its cleanup uncertain and the
/// generation's cleanup interrupt posted.
fn contradicted(facts: &Turn<'_>, start: &Started<'_>, start_id: ClientId) -> TurnEnd {
    let turn = facts.number;
    let cause = DriverFailure::Route(RouteError::Protocol {
        turn,
        detail: CONTRADICTED,
    });
    let loss = LossRecord {
        losses: Arc::clone(&facts.session.losses),
        generation: start.generation,
    };
    let (lane, registration) = start.registered();
    registration.fail(&cause, (&facts.driver.health, lane, &loss));
    contradicted_seam();
    let cause = registration.failure().unwrap_or(cause);
    unanswered(
        facts,
        start,
        start_id,
        Unanswered::Generation {
            cause,
            launched: true,
        },
    )
}

/// A started turn whose reply never came; at an overflow the
/// generation's cleanup interrupt is posted, waiting on that reply (x.3.2
/// X3 fix r3 #1). (A start positively not written opens the start gate
/// as the connection forgets its record, §2.2.)
fn unanswered(
    facts: &Turn<'_>,
    start: &Started<'_>,
    start_id: ClientId,
    cause: Unanswered,
) -> TurnEnd {
    if matches!(
        cause,
        Unanswered::Overflow | Unanswered::Generation { launched: true, .. }
    ) {
        let by = Deadline::at(Instant::now() + CLEANUP_ALLOWANCE);
        start
            .connection
            .cleanup_interrupt(&start.thread.lease, start_id, None, by);
    }
    lost(facts, start.connection, cause)
}

/// Spawns the registration's consumer on the session's tracker under
/// `crash_on_panic` (X0 item 13.2): it takes the lane of `lease` across
/// the registration's turns (x.3.2 X3 §3).
fn normalize_on_tracker(
    driver: &SessionDriver,
    session: &CodexSession,
    ids: &Ids<'_>,
    lease: &LaneLease,
) -> Arc<Registration> {
    let registration = Registration::new(ids.generation.signal.enqueued(), session.cap(driver));
    driver.tracker.spawn(crash_on_panic(
        Normalizing::new(
            (Arc::clone(&registration), Arc::clone(lease.lane())),
            (
                driver.observations.clone(),
                Evidence {
                    server: Arc::clone(ids.connection) as Arc<dyn ServerEvidence>,
                    earlier: Arc::clone(&ids.generation.folders),
                },
            ),
            (driver.cancel.clone(), Arc::clone(&driver.health)),
            LossRecord {
                losses: Arc::clone(&session.losses),
                generation: ids.generation.number,
            },
        )
        .run(),
    ));
    registration
}

/// x.3.2 X3 §4.2 step 2: the turn's delivery, whose last delivered
/// message is the one before the lane's first, is admitted on its
/// registration with it, or refused once the generation failed or the
/// registration retired; the generation's failure cancels its writes and
/// stops its delivery. The settlement seals it however the turn ends.
async fn admitted(
    facts: &Turn<'_>,
    start: &Started<'_>,
    writes: &TurnWrites,
) -> Option<(Admission, Arc<Delivery>)> {
    let cancel = writes.canceller();
    let lane = start.thread.lease.lane();
    let before = lane
        .front_seq()
        .map_or_else(|| start.signal.enqueued(), |seq| seq.saturating_sub(1));
    let delivery = Delivery::new(before);
    let registration = &start.thread.registration;
    let admission =
        registration.admit(facts.number, Arc::new(move || cancel.cancel()), &delivery)?;
    facts.settle.deliver(&delivery);
    admitted_seam().await;
    Some((admission, delivery))
}

/// Test builds: a seam between a turn's admission and its `turn/start`
/// (x.3.2 X3 r6 #1, r5 #5), where a test holds the admitted turn.
#[cfg_attr(
    not(feature = "test-failpoints"),
    expect(clippy::unused_async, reason = "only test builds wait at the seam")
)]
async fn admitted_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit_async("adapter.codex.admitted").await;
    }
}

/// Test builds: a marker once a contradicted refusal failed the
/// generation (Sol code r1 #8), which a test awaits before releasing the
/// consumer it holds.
fn contradicted_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit("adapter.codex.contradicted");
    }
}

/// Test builds: a seam between an accepted turn's cutoff and its
/// settlement (x.3.2 X3 fix r3 #2), where a test holds the turn while its
/// normalizer runs on.
#[cfg_attr(
    not(feature = "test-failpoints"),
    expect(clippy::unused_async, reason = "only test builds wait at the seam")
)]
async fn cut_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit_async("adapter.codex.cut").await;
    }
}

/// Test builds: a seam at an accepted turn's wait, before it first polls
/// its orders (x.3.2 X4 D4.2), where a test publishes an order the wait
/// notices only later.
#[cfg_attr(
    not(feature = "test-failpoints"),
    expect(clippy::unused_async, reason = "only test builds wait at the seam")
)]
async fn ordered_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit_async("adapter.codex.ordered").await;
    }
}

/// Test builds: a seam right after a turn's wait read its cut (X4 code
/// review r3 #1), where a test publishes an order the read missed. Hit
/// only once an order is found.
#[cfg_attr(
    not(feature = "test-failpoints"),
    expect(clippy::unused_async, reason = "only test builds wait at the seam")
)]
async fn cut_read_seam() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit_async("adapter.codex.cut_read").await;
    }
}

/// An accepted turn's facts.
struct Accepted<'a> {
    id: String,
    start: ClientId,
    delivery: &'a Arc<Delivery>,
}

/// What ended an accepted turn's wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Cut {
    /// The daemon force.
    Forced,
    /// The delivery decided: the terminal, or why it stopped.
    Decided,
    /// The turn's own order reached its end.
    Order(EndCause),
    /// The connection failed and its evidence did not reach the turn in
    /// time (X0 items 13.1, 13.2).
    LossDeadline,
    /// The thread's lane overflowed (x.3.2 X3 fix r2 #1): the turn ends
    /// at once, whatever the normalizer is doing.
    Overflow,
    /// x.3.2 X4 D4.3: the P7 window ended, at `min(decoded_at +
    /// tool_grace, wall)`, with a tool still open.
    Grace,
    /// x.3.2 X4 D4.3 (Q8): a close or the session's cancellation while
    /// the P7 window was open.
    Detach,
}

/// C2 §5, the one accounting rule (picrit round 4): Codex reports no turn
/// aggregate (`total` is the thread's), so the turn's summed per-call
/// `last` samples are its usage only when the sum is provably whole:
/// - the turn retained its terminal, and no daemon force dropped it;
/// - the connection did not fail before the cutoff (`LossDeadline`) and
///   the lane never overflowed;
/// - every observation of the turn was delivered: no partial seal and no
///   stop of its delivery (a lane end, a failed generation or task);
/// - unless delivery decided at the terminal, nothing more of the turn
///   can have been lost either: no lane end, no registration failure,
///   nothing dropped and nothing left in the lane or outstanding at the
///   cutoff. A decided delivery took the whole turn in order through its
///   terminal, so what the lane holds or loses after it is later traffic;
///   a failure before it would have stopped the turn's delivery instead;
/// - nothing the turn's accounting covered was lost in the connection's
///   read order (picrit rounds 5 to 7): if the connection's routing
///   rejected a message (`rejected`, the first one's decode sequence),
///   delivery decided and that message comes after the last one the
///   turn's delivery took before its seal (`Sealed::last_seq`): the
///   terminal, or what closed an interrupted terminal's P7 window. A
///   rejection fails the connection, but the drain still routes what Wire
///   admitted behind it, so a decided terminal, or a P7-closing tool end,
///   can follow a lost sample; the turn keeps its status and answer, not
///   its sum. Only `Decided` means delivery took the turn through its
///   natural end: any other cut (the P7 bound, a detach, an order, the
///   force) may have left a rejected message of the turn behind
///   `last_seq`, so with a rejection it is unaccounted.
///
/// Anything else (an uncorrelated or malformed message failing the
/// connection, a connection loss, a stall while the terminal drains, an
/// overflow) leaves the tokens unavailable: the all-null aggregate, with
/// a terminal or without one.
pub(super) fn accounted(
    cut: Cut,
    sealed: &Sealed,
    (lane, registration): (&Lane, &Registration),
    rejected: Option<u64>,
) -> bool {
    let rest_whole = || {
        lane.ended().is_none()
            && lane.dropped() == 0
            && lane.front_seq().is_none()
            && registration.outstanding().is_none()
            && registration.failure().is_none()
    };
    !matches!(cut, Cut::Forced | Cut::Overflow | Cut::LossDeadline)
        && !lane.overflowed_now()
        && sealed.terminal.is_some()
        && !sealed.partial
        && sealed.stop.is_none()
        && (cut == Cut::Decided || rest_whole())
        && rejected.is_none_or(|rejected| cut == Cut::Decided && sealed.last_seq < rejected)
}

/// The cutoff a turn's wait finds already reached as it resumes: the
/// daemon force before the delivery's decision (X0 §13.2: the force's
/// disposition stands even with a retained terminal; x.3.2 X3 fix r2 #5),
/// the decision before the lane's overflow.
pub(super) fn ready_cut(forced: bool, decided: bool, overflowed: bool) -> Option<Cut> {
    if forced {
        Some(Cut::Forced)
    } else if decided {
        Some(Cut::Decided)
    } else if overflowed {
        Some(Cut::Overflow)
    } else {
        None
    }
}

/// Waits for the turn's delivery to decide, beside its orders: never
/// behind the normalizer, so a blocked sink delays no control. A stop
/// order posts the turn's interrupt intent at once. While its P7 window
/// is open (x.3.2 X4 D4.3) the order's end no longer applies: the window
/// ends at `min(decoded_at + tool_grace, wall)` (`Grace`), at once on a
/// close or the session's cancellation (`Detach`), or when the tools end
/// (the delivery decides).
async fn wait(
    start: &Started<'_>,
    accepted: &Accepted<'_>,
    (orders, force): (&mut Orders, &mut ForceWatch),
) -> Cut {
    ordered_seam().await;
    let failing = start.connection.failing();
    tokio::pin!(failing);
    let lane = start.thread.lease.lane();
    let mut ending: Option<Ending> = None;
    let mut loss_at: Option<Instant> = None;
    loop {
        let ready = ready_cut(
            force.borrow().is_some(),
            accepted.delivery.decided(),
            lane.overflowed_now(),
        );
        if let Some(cut) = ready {
            return cut;
        }
        let draining = accepted.delivery.draining();
        if draining.is_some() && orders.detached() {
            return Cut::Detach;
        }
        let grace_at =
            draining.map(|decoded_at| (decoded_at + orders.tool_grace).min(orders.wall.instant()));
        // X4 code review r2 #1: the order's end is the cut, as settlement
        // reads it, read again at every order change; the watcher is taken
        // before the read (r3 #1), so an order published after the read
        // still wakes the wait.
        let changed = orders.changed();
        let end_at = ending.filter(|_| draining.is_none()).map(|ending| {
            let by = ending.by.instant();
            orders.cut(Instant::now()).map_or(by, |cut| cut.min(by))
        });
        if ending.is_some() {
            cut_read_seam().await;
        }
        let detaching = orders.detaching();
        tokio::select! {
            biased;
            () = forced(force) => return Cut::Forced,
            () = accepted.delivery.changed() => {}
            () = lane.overflowed() => {}
            () = detaching, if draining.is_some() => {}
            () = changed, if ending.is_some() => {}
            found = orders.ordered(), if ending.is_none() => {
                start.connection.interrupt(
                    &start.thread.lease,
                    accepted.start,
                    Some(&accepted.id),
                    found.by,
                );
                ending = Some(found);
            }
            () = sleep_until(end_at), if end_at.is_some() => {
                return Cut::Order(ending.map_or(EndCause::Stopped, |ending| ending.cause));
            }
            () = sleep_until(grace_at), if grace_at.is_some() => return Cut::Grace,
            () = &mut failing, if loss_at.is_none() => {
                loss_at = Some(Instant::now() + LOSS_EVIDENCE);
            }
            () = sleep_until(loss_at), if loss_at.is_some() => return Cut::LossDeadline,
        }
    }
}

/// The accepted turn's end at its cutoff: delivery is sealed first, so
/// nothing of the turn reaches Core after it; what the seal left
/// undelivered at any cutoff but the terminal joins the driver's loss
/// record. The force decides at once; otherwise a retained terminal wins
/// (C1 §7.6 terminal first), then an overflow, then why delivery
/// stopped, then the order. An overflow (the lane's, or the normalizer's
/// bounds or a stalled sink) proves nothing of what was dropped: the loss
/// is recorded, the generation's cleanup interrupt is posted and the
/// cleanup is uncertain (x.3.2 X3 fix r2 #2).
///
/// x.3.2 X4 D4.2: the provenance is noted first, and settlement reads
/// only that record. A terminal decoded before the wall wins as `Ok`; one
/// decoded at or after it wins too under an order attached before the
/// wall (the order's row); under the wall it is kept beside the wall's
/// `Deadline`, acknowledged when interrupted within the wall's cleanup
/// bound. The consumer never retained a terminal decoded after its cut
/// (late only), nor ended a P7 window by a tool ending decoded after it:
/// the window's end (`Grace`) or a detach keeps the terminal with its
/// cleanup uncertain ([`Cutoffs`]).
fn settle_turn(
    facts: &Turn<'_>,
    (start, accepted): (&Started<'_>, &Accepted<'_>),
    orders: &mut Orders,
    cut: Cut,
) -> TurnEnd {
    orders.note(Instant::now());
    let turn = facts.number;
    let wall = orders.wall.instant();
    // The consumer judged each piece of evidence at its decode (X4
    // critical review): a late terminal was never retained, a late tool
    // ending never ended the drain; settlement reads what it kept.
    let sealed = accepted.delivery.seal();
    let lane = start.thread.lease.lane();
    let terminal_decided = cut == Cut::Decided && sealed.terminal.is_some();
    let registration = &start.thread.registration;
    let undelivered =
        sealed.partial || lane.front_seq().is_some() || registration.outstanding().is_some();
    let abnormal = matches!(sealed.stop, Some(Stop::Lane(LaneEnd::Abnormal)));
    let overflowed = cut == Cut::Overflow
        || lane.overflowed_now()
        || matches!(sealed.stop, Some(Stop::Lane(LaneEnd::Overflow)));
    let cleanup_interrupt = || {
        let by = Deadline::at(Instant::now() + CLEANUP_ALLOWANCE);
        start.connection.cleanup_interrupt(
            &start.thread.lease,
            accepted.start,
            Some(&accepted.id),
            by,
        );
    };
    if !terminal_decided && (undelivered || abnormal || overflowed || cut == Cut::LossDeadline) {
        let position = registration.floor(sealed.position);
        lock_losses(&facts.session.losses).note(Some(lane), start.generation, position, UNKNOWN);
    }
    let reported = Some(if sealed.tools_open {
        WireCleanup::Uncertain
    } else {
        WireCleanup::Quiescent
    });
    let whole = accounted(
        cut,
        &sealed,
        (lane, registration),
        start.connection.rejected(),
    );
    let mut end = 'end: {
        let uncertain = |cause| facts.failure(cause, None, None);
        if cut == Cut::Forced {
            break 'end uncertain(RouteError::ForceStopped { turn });
        }
        if let Some(retained) = sealed.terminal {
            // The terminal stands, but an overflow beside it, however the two
            // were ready, still leaves the cleanup unproven (x.3.2 X3 fix r3
            // #2).
            if overflowed {
                cleanup_interrupt();
            }
            let decoded_at = sealed.decoded_at.unwrap_or(retained.terminal.at);
            let tools_open = sealed.tools_open || overflowed;
            if decoded_at >= wall && orders.provenance() == EndCause::Wall {
                break 'end wall_end(facts, retained, tools_open);
            }
            break 'end terminal_end(facts, retained, tools_open);
        }
        match (cut, sealed.stop) {
            (Cut::Overflow, _) | (_, Some(Stop::Lane(LaneEnd::Overflow))) => {
                cleanup_interrupt();
                facts.failure(
                    RouteError::Overflow { turn },
                    None,
                    Some(WireCleanup::Uncertain),
                )
            }
            // x.3.2 X3 §4.3: never the cleanup of what survived.
            (_, Some(Stop::Generation | Stop::Lane(LaneEnd::Quarantined))) => {
                let cause = registration
                    .failure()
                    .map_or(RouteError::Overflow { turn }, |cause| {
                        generation_cause(&cause, turn)
                    });
                cleanup_interrupt();
                facts.failure(cause, None, Some(WireCleanup::Uncertain))
            }
            (_, Some(Stop::Lane(end))) => {
                let (cause, loss) = lane_end(end, start.connection, turn);
                facts.failure(cause, loss, reported)
            }
            (Cut::Order(_), None) if orders.provenance() == EndCause::Wall => {
                uncertain(RouteError::Deadline { turn })
            }
            (Cut::LossDeadline, None) => match connection_loss(start.connection) {
                Some(loss) => facts.failure(
                    loss_cause(&loss, turn),
                    Some(ConnectionLoss {
                        cleanup: WireCleanup::Uncertain,
                        ..loss
                    }),
                    None,
                ),
                None => uncertain(RouteError::TransportLost { turn }),
            },
            // A P7 cut has a terminal, which the seal took; never reached.
            (Cut::Order(_) | Cut::Decided | Cut::Forced | Cut::Grace | Cut::Detach, None) => {
                uncertain(RouteError::Stopped { turn })
            }
        }
    };
    if !whole {
        unaccounted(&mut end);
    }
    end
}

/// x.3.2 X4 D4.2 rule 3 (C2 §4.1): the wall stopped the turn, whose
/// terminal, decoded at `decoded_at` at or after it, is kept beside the
/// wall's `Deadline`; the stop was acknowledged by an interrupted terminal.
/// P7 is capped at the wall: it settles at once. A terminal decoded
/// after the wall's cleanup bound never gets here: the consumer judged it
/// late ([`Cutoffs::late`]).
fn wall_end(facts: &Turn<'_>, retained: Retained, tools_open: bool) -> TurnEnd {
    let acknowledged = retained.terminal.status == VendorTerminalStatus::Interrupted;
    let mut end = facts.failure(
        RouteError::Deadline { turn: facts.number },
        None,
        Some(if tools_open {
            WireCleanup::Uncertain
        } else {
            WireCleanup::Quiescent
        }),
    );
    if let Err(AdapterError::Route(failure)) = &mut end.outcome {
        failure.acknowledged = acknowledged;
    }
    let kept = terminal_end(facts, retained, tools_open);
    end.terminal = kept.terminal;
    end
}

/// The turn's end with its retained terminal.
fn terminal_end(facts: &Turn<'_>, retained: Retained, tools_open: bool) -> TurnEnd {
    let Retained {
        mut terminal,
        structured,
    } = retained;
    (
        terminal.structured_output,
        terminal.structured_output_unparsed,
    ) = carried(structured);
    TurnEnd {
        loss: None,
        aggregate: None,
        terminal: Some(terminal),
        instance: facts.instance.clone(),
        leftovers: None,
        outcome: Ok(TurnEvidence {
            exit: None,
            cleanup: if tools_open {
                Cleanup::Uncertain
            } else {
                Cleanup::Quiescent
            },
            journal_uncertain: false,
        }),
    }
}

/// The terminal's structured output as C2 carries it (Sol r1 #15): a JSON
/// value, or the reason text is none, which Core treats as present and
/// invalid; absent when none was requested or the text was empty.
pub(super) fn carried(
    structured: StructuredOutput,
) -> (
    Option<Box<serde_json::value::RawValue>>,
    Option<UnparsedOutput>,
) {
    match structured {
        StructuredOutput::Json(json) => (Some(json), None),
        StructuredOutput::NotJson => (None, Some(UnparsedOutput::NotJson)),
        StructuredOutput::OverLimit => (None, Some(UnparsedOutput::OverLimit)),
        StructuredOutput::NotRequested | StructuredOutput::Missing => (None, None),
    }
}

/// A per-turn refusal as the definite rejection it is before submission.
fn start_rejected(kind: &RefusalKind, message: String) -> StartRejected {
    match kind {
        RefusalKind::BoundUnsupported => StartRejected::BoundUnsupported(message),
        RefusalKind::InvalidParam { field } | RefusalKind::VendorOptionConflict { field } => {
            StartRejected::InvalidParam { field }
        }
        RefusalKind::UnsupportedVerb
        | RefusalKind::HarnessUnavailable
        | RefusalKind::UnknownModel
        | RefusalKind::VersionRefused
        | RefusalKind::MissingCapability { .. } => StartRejected::Protocol(message),
    }
}
