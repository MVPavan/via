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
//! reply's echoes are checked. Every turn is a `turn/start` with the full
//! frozen policy, written under the turn's owning write guard and accepted
//! on its paired reply; the turn's normalizer then delivers the thread's
//! lane in decode order (`delivery`) while the turn waits for its
//! terminal beside its own orders, sealing delivery at whichever comes
//! first. A close detaches with `thread/unsubscribe` and releases the
//! lease; the last lease's release retires the server.
//!
//! A stop order posts the turn's one interrupt intent, owned by the
//! connection (before acceptance it waits on the start's reply). Its
//! acknowledgement, the P7 window and steer are x.3.2 X4's: the turn ends
//! at the order's `close_by` (`uncertain`) unless its terminal comes
//! first.

use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{oneshot, watch};
use tokio::time::Instant;
use via_routes::codex::{
    AbnormalEnd, ClientId, CommitOutcome, Connection, ConnectionEnd, ConnectionLoss, FINISH_BY,
    LOSS_EVIDENCE, LaneEnd, LaneLease, LaunchError, LeaseSignal, LossCause, Purpose, RequestError,
    Response, RpcError, SandboxMode, ServerKey, ServerPin, Subscription, ThreadResult,
    ThreadSettings, TurnFolder, TurnStart, TurnStartResult, TurnWrites, WriteBounds,
    crash_on_panic, data, result, thread_resume, thread_start, turn_start,
};
use via_routes::{Retirement, SendOutcome, StoreFailure, WireCleanup};

use super::delivery::{
    Delivery, Evidence, Losses, Normalizing, Retained, Stop, UNKNOWN, losses as lock_losses,
};
use super::normalize::{self, DiscoveredModel, StructuredOutput, TurnNormalizer};
use super::plan::{self as codex_plan, Sandbox};
use super::{ADAPTER_VERSION, CodexAdapter, HARNESS, PerTurn, refusals};
use crate::driver::turn::{CLEANUP_ALLOWANCE, end_active};
use crate::driver::{
    Active, ConnectionPin, DriverState, ForceWatch, Prepared, Retiring, SessionDriver, TurnCx,
    TurnSpec, latch, lock, rejected,
};
use crate::harness::Harness;
use crate::instance::Incompatibility;
use crate::observation::{
    Acceptance, AdapterError, Identity, InstanceReport, Observation, ObservationItem, TurnEnd,
    TurnEvidence,
};
use crate::plan::{Bound, Inherit, RefusalKind};
use crate::runtime::event_stall;
use crate::{
    AcceptanceToken, Cleanup, Deadline, DriverFailure, DriverHealth, ProcessOwner, RouteError,
    RouteFailure, StartRejected, StopOrder, StopWatch, TurnNumber, VendorTurnId,
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
}

/// The session's connection generation. The fields drop in order: the
/// thread's registration closes, then the abnormal-end subscription, and
/// the server lease is released last.
struct Attached {
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
    /// The session's lease, held from its first join until close (AD16).
    lease: ServerPin,
}

/// An open thread: its ID and its registration on the connection.
pub(crate) struct Thread {
    id: String,
    lease: LaneLease,
}

/// The facts a turn takes from the generation it runs on.
struct Generation {
    number: u64,
    thread: Option<Arc<Thread>>,
    signal: Arc<LeaseSignal>,
}

impl CodexSession {
    pub(crate) fn new(adapter: Arc<CodexAdapter>) -> Self {
        Self {
            adapter,
            attached: Mutex::new(None),
            bound: Mutex::new(None),
            losses: Arc::new(Mutex::new(Losses::default())),
        }
    }

    fn attached(&self) -> std::sync::MutexGuard<'_, Option<Attached>> {
        // Each edit is one assignment: the state stays consistent.
        self.attached.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The registry's key for the session's server.
    fn key(&self, requested: Inherit) -> ServerKey {
        ServerKey(
            self.adapter
                .recipe(requested)
                .config_hash(ADAPTER_VERSION)
                .bytes(),
        )
    }

    /// AD16 `prepare`: a pin on the session's live server, else on a live
    /// or launching server of an equal key; `None` when the turn needs a
    /// connection slot.
    pub(crate) fn prepare(&self, requested: Inherit) -> Option<ServerPin> {
        let own = self
            .attached()
            .as_ref()
            .filter(|attached| usable(&attached.connection))
            .and_then(|attached| attached.lease.duplicate());
        own.or_else(|| self.adapter.servers().pin(&self.key(requested)))
    }

    /// Changes whenever `prepare`'s answer may change (C2 §3).
    pub(crate) fn readiness(&self) -> watch::Receiver<u64> {
        self.adapter.servers().epoch()
    }

    /// The close's detach (packet §2): the thread's unsubscribe intent
    /// (X0 item 8.3), its reply awaited by `deadline`; then the
    /// registration closes and the lease is released. Never a stdin
    /// close: the server is shared.
    pub(crate) async fn detach(&self, deadline: Deadline) {
        let Some(attached) = self.attached().take() else {
            return;
        };
        if let Some(thread) = &attached.thread
            && usable(&attached.connection)
            && let Some(reply) = attached.connection.unsubscribe(&thread.lease, deadline)
        {
            let _answered = tokio::time::timeout_at(deadline.instant(), reply).await;
        }
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
            });
        }
        let lease = pin.duplicate()?;
        let generation = {
            let mut state = driver.state();
            state.generation += 1;
            state.generation
        };
        let registered = Arc::new(AtomicBool::new(false));
        let signal = Arc::new(LeaseSignal::new(abnormal_handler(
            Arc::clone(&self.losses),
            Arc::clone(&driver.health),
            Arc::clone(&registered),
            generation,
        )));
        let subscription = connection.subscribe(Arc::clone(&signal));
        let replaced = attached.replace(Attached {
            thread: None,
            _subscription: subscription,
            signal: Arc::clone(&signal),
            registered: Arc::clone(&registered),
            connection: Arc::clone(connection),
            generation,
            lease,
        });
        drop(attached);
        drop(replaced);
        Some(Generation {
            number: generation,
            thread: None,
            signal,
        })
    }

    /// Keeps the thread this generation opened.
    fn opened(&self, connection: &Arc<Connection>, thread: &Arc<Thread>) {
        if let Some(attached) = self.attached().as_mut()
            && Arc::ptr_eq(&attached.connection, connection)
        {
            attached.thread = Some(Arc::clone(thread));
            attached.registered.store(true, Ordering::Release);
        }
    }

    /// The generation is unfit for the next turn: its registration closes
    /// (its later traffic is late) and the next turn opens the thread
    /// again as a new generation.
    fn quarantine(&self, connection: &Arc<Connection>) {
        let mut attached = self.attached();
        if attached
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.connection, connection))
        {
            let gone = attached.take();
            drop(attached);
            drop(gone);
        }
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
            lock_losses(&losses).note(generation, end.first_unqueued, UNKNOWN);
        }
        latch(&health, DriverFailure::OwnedTask);
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
    fn new(driver: &'a SessionDriver, turn: TurnNumber, done: watch::Sender<Retiring>) -> Self {
        Self {
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
/// left on the shared server is never erased by a later turn (ruling 6).
fn sticky(earlier: Retirement, later: Retirement) -> Retirement {
    let open = |facts: &Retirement| facts.launched && facts.cleanup != Some(WireCleanup::Quiescent);
    let uncertain = open(&earlier) || open(&later);
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
/// the daemon force and the session's cancellation.
struct Orders {
    stop: StopWatch,
    close: watch::Receiver<Option<StopOrder>>,
    wall: Deadline,
    cancel: tokio_util::sync::CancellationToken,
}

/// Why a turn is ending, and by when it must have ended.
#[derive(Clone, Copy, Debug)]
struct Ending {
    cause: EndCause,
    by: Deadline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EndCause {
    Stopped,
    Wall,
}

impl Orders {
    /// Resolves at the first order (the force excluded), with its cause
    /// and its own end.
    async fn ordered(&mut self) -> Ending {
        let Self {
            stop,
            close,
            wall,
            cancel,
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
    /// The settlement, which learns the same.
    settle: &'a Settle<'a>,
}

impl Turn<'_> {
    /// A byte of the turn was handed to Wire.
    fn launch(&mut self) {
        self.launched = true;
        self.settle.launched();
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
            terminal: None,
            instance: self.instance.clone(),
            leftovers: None,
            outcome: Err(AdapterError::Route(RouteFailure {
                cause,
                undecoded: None,
                // A server route's turn has no exit of its own (C2 §2).
                exit: None,
                launched: self.launched,
                cleanup: loss.map_or(live, |loss| Some(loss.cleanup)),
                forced: false,
                journal_uncertain,
                acknowledged: false,
                shared: true,
            })),
        }
    }

    /// A definite rejection before acceptance.
    fn rejected(&self, reason: StartRejected) -> TurnEnd {
        TurnEnd {
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
    /// the daemon force (X0: controls stay serviceable while delivery is
    /// blocked); an undelivered one latches the observation overflow. A
    /// send the force cut never reaches the sink.
    async fn emit(&self, observation: Observation, force: &mut ForceWatch) -> Result<(), Emit> {
        let item = ObservationItem {
            at: Instant::now(),
            vendor_turn: None,
            observation,
        };
        tokio::select! {
            biased;
            () = forced(force) => Err(Emit::Forced),
            sent = self.driver.observations.send(item, event_stall()) => sent.map_err(|_| {
                self.driver.fail(DriverFailure::ObservationOverflow);
                Emit::Undelivered
            }),
        }
    }
}

/// Why a pre-acceptance observation was not sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Emit {
    Undelivered,
    Forced,
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
        LaneEnd::Overflow => (RouteError::Overflow { turn }, None),
        LaneEnd::Lost(loss) => (loss_cause(&loss, turn), Some(loss)),
        LaneEnd::Retired => (RouteError::TransportLost { turn }, None),
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

/// Runs one submitted turn (C2 §4.1), inline but for its normalizer,
/// which the session's tracker owns. Its settlement runs however it ends.
pub(crate) async fn run_turn(
    driver: &SessionDriver,
    session: &CodexSession,
    spec: TurnSpec,
    cx: TurnCx,
) -> TurnEnd {
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
    lock_losses(&session.losses).latest = Some(cx.turn);
    let settle = Settle::new(driver, cx.turn, done);
    let end = turn(driver, session, (spec, sandbox), cx, close_rx, &settle).await;
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
        tool_grace: _,
        stop,
        mut force,
    } = cx;
    let mut orders = Orders {
        stop,
        close,
        wall,
        cancel: driver.cancel.clone(),
    };
    let mut facts = Turn {
        driver,
        session,
        number: turn,
        instance: None,
        launched: false,
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
    let catalog = adopt(&mut facts, &server);
    let Some(generation) = session.attach(&pin, &connection, driver) else {
        return facts.failed(RouteError::TransportLost { turn }, None);
    };
    let Ok(effort) = vendor_effort(spec.effort.as_deref(), &catalog, &driver.spec.model) else {
        return facts.rejected(StartRejected::InvalidParam { field: "effort" });
    };
    if let Err(end) = link(&facts, &connection, wall).await {
        return *end;
    }
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
            &ids,
            sandbox.mode,
            (&mut orders, &mut force, &mut writes),
        )
        .await;
        match opened {
            Ok(thread) => thread,
            Err(end) => return *end,
        }
    };
    let start = Started {
        thread: &thread,
        connection: &connection,
        generation: generation.number,
        signal: &generation.signal,
        sandbox: &sandbox,
        effort: effort.as_deref(),
        folder: &folder,
        schema: spec.output_schema.is_some(),
    };
    let end = run_started(
        &mut facts,
        &start,
        spec,
        (&activity, &mut orders, &mut force, &mut writes),
    )
    .await;
    drop(writes);
    if quarantines(&end) || !usable(&connection) {
        session.quarantine(&connection);
    }
    drop(pin);
    end
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
fn vendor_effort(
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
/// version) and caches its model catalog, which it returns.
fn adopt(facts: &mut Turn<'_>, server: &via_routes::codex::ServerFacts) -> Arc<[DiscoveredModel]> {
    let adapter = &facts.session.adapter;
    facts.instance = Some(instance_report(&server.user_agent));
    if let Some(version) = normalize::instance_version(&server.user_agent) {
        adapter
            .instances
            .record_version(HARNESS, &adapter.binary, version.to_owned());
    }
    let catalog: Arc<[DiscoveredModel]> = server.models.iter().map(normalize::discovered).collect();
    adapter.discovered(Arc::clone(&catalog));
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
/// key's, else a launch or join with the turn's connection slot.
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
                    "a new server needs a connection slot".to_owned(),
                ))));
            };
            if ensure_home(&adapter.vendor_home()).is_err() {
                return Err(Box::new(facts.failed(
                    RouteError::Store {
                        turn,
                        kind: StoreFailure::Evidence,
                    },
                    None,
                )));
            }
            let recipe = adapter.recipe(driver.spec.inherit.requested);
            let owner = ProcessOwner::Turn {
                session_id: driver.spec.session_id.clone(),
                turn,
            };
            let key = ServerKey(recipe.config_hash(ADAPTER_VERSION).bytes());
            match adapter
                .servers()
                .launch_or_join(key, recipe.process_spec(owner), capacity)
            {
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
        Ok(()) => Ok(pin),
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
        terminal: None,
        instance: facts.instance.clone(),
        leftovers: None,
        outcome: Err(AdapterError::Route(failure)),
    }
}

/// The cause of a turn its order ended before anything was sent.
fn unsent_cause(orders: &Orders, force: &ForceWatch, turn: TurnNumber) -> RouteError {
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
/// the confirmed thread, else a start, written under the turn's guard on
/// a lane subscribed to the generation's abnormal end; the reply's echoes
/// checked, its identity confirmed. Any failure drops the lane, which
/// closes a registration the reply made (X0 item 8.1).
async fn open_thread(
    facts: &mut Turn<'_>,
    ids: &Ids<'_>,
    mode: SandboxMode,
    (orders, force, writes): (&mut Orders, &mut ForceWatch, &mut TurnWrites),
) -> Result<Arc<Thread>, Box<TurnEnd>> {
    let driver = facts.driver;
    let turn = facts.number;
    let lease = ids.connection.open_lane(Some(&ids.generation.signal));
    let resume = driver.state().identity.clone();
    let settings = ThreadSettings {
        model: &driver.spec.model,
        cwd: &driver.spec.cwd,
        developer_instructions: driver.spec.instructions.as_deref(),
        sandbox: mode,
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
        Purpose::Opens(&lease),
        Some(writes),
    );
    let requested = requested.map_err(|error| Box::new(request_failed(facts, error)))?;
    facts.launch();
    let reply = await_reply(
        (requested.written, requested.reply),
        (orders, force),
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
    confirm(facts, (&opened, mode), resume, &lease, force).await?;
    let thread = Arc::new(Thread {
        id: opened.thread.id.clone(),
        lease,
    });
    facts.session.opened(ids.connection, &thread);
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
        .emit(Observation::IdentityConfirmed(identity), force)
        .await
    {
        Ok(()) => Ok(thread),
        Err(Emit::Undelivered) => Err(Box::new(facts.failed(RouteError::Overflow { turn }, None))),
        Err(Emit::Forced) => Err(Box::new(facts.failure(
            RouteError::ForceStopped { turn },
            None,
            None,
        ))),
    }
}

/// Checks an opened thread before its identity is confirmed: the echoed
/// policy, a resume's thread ID, and its registration on the connection.
async fn confirm(
    facts: &Turn<'_>,
    (opened, mode): (&ThreadResult, SandboxMode),
    resume: Option<String>,
    lease: &LaneLease,
    force: &mut ForceWatch,
) -> Result<(), Box<TurnEnd>> {
    let driver = facts.driver;
    let turn = facts.number;
    if let Some(field) = echo_differs(opened, mode) {
        let adapter = &facts.session.adapter;
        // The recipe key the refused handshake is cached under.
        let hash = adapter
            .recipe(driver.spec.inherit.requested)
            .config_hash(ADAPTER_VERSION)
            .hex();
        adapter.instances.record_refusal(
            &adapter.binary,
            hash,
            Incompatibility::ReadbackDiffers(field),
            std::time::Instant::now(),
        );
        return Err(Box::new(
            facts.failed(RouteError::HandshakeRefused { turn }, None),
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
        let _undelivered = facts.emit(mismatch, force).await;
        return Err(Box::new(TurnEnd {
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

/// The first echoed policy field that is not the one requested (packet §3:
/// never, the user reviewer, the bound's sandbox).
fn echo_differs(opened: &ThreadResult, mode: SandboxMode) -> Option<&'static str> {
    let sandbox = match mode {
        SandboxMode::ReadOnly => "readOnly",
        SandboxMode::WorkspaceWrite => "workspaceWrite",
        SandboxMode::DangerFullAccess => "dangerFullAccess",
    };
    if opened.approval_policy.as_str() != Some("never") {
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
        StartRejected::VendorError(error.code.to_string(), error.message.clone())
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
#[derive(Clone, Copy)]
enum Unanswered {
    /// The connection ended or its record went.
    Lost,
    /// The request was not written whole.
    NotWritten(SendOutcome),
    /// The daemon force.
    Forced,
    /// The turn's own order reached its end.
    Ended(EndCause),
}

/// A turn whose request went unanswered.
fn lost(facts: &Turn<'_>, connection: &Connection, cause: Unanswered) -> TurnEnd {
    let turn = facts.number;
    let ended = |facts: &Turn<'_>| match connection_loss(connection) {
        Some(loss) => facts.failed(loss_cause(&loss, turn), Some(loss)),
        None => facts.failed(RouteError::TransportLost { turn }, None),
    };
    match cause {
        Unanswered::NotWritten(SendOutcome::NotWritten) => {
            let unsent = Turn {
                launched: false,
                instance: facts.instance.clone(),
                ..*facts
            };
            ended(&unsent)
        }
        Unanswered::Lost | Unanswered::NotWritten(_) => ended(facts),
        // A stop's cleanup stays unproven: the written request may have
        // started work the vendor never reported (P7's acknowledgement).
        Unanswered::Forced => facts.failure(RouteError::ForceStopped { turn }, None, None),
        Unanswered::Ended(EndCause::Stopped) => {
            facts.failure(RouteError::Stopped { turn }, None, None)
        }
        Unanswered::Ended(EndCause::Wall) => {
            facts.failure(RouteError::Deadline { turn }, None, None)
        }
    }
}

/// Awaits a queued request's paired reply, bounded by the force and the
/// end of the turn's own order; `on_order` runs once, at the order.
async fn await_reply(
    (written, reply): (oneshot::Receiver<SendOutcome>, oneshot::Receiver<Response>),
    (orders, force): (&mut Orders, &mut ForceWatch),
    on_order: &mut (dyn FnMut(&Ending) + Send),
) -> Result<Response, Unanswered> {
    tokio::pin!(written);
    tokio::pin!(reply);
    let mut answered = false;
    let mut ending: Option<Ending> = None;
    loop {
        let end_at = ending.map(|ending| ending.by.instant());
        tokio::select! {
            biased;
            () = forced(force) => return Err(Unanswered::Forced),
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

/// A started turn's connection facts.
struct Started<'a> {
    thread: &'a Arc<Thread>,
    connection: &'a Arc<Connection>,
    generation: u64,
    /// The generation's abnormal-end signal: its decode positions.
    signal: &'a LeaseSignal,
    sandbox: &'a Sandbox,
    effort: Option<&'a str>,
    folder: &'a Arc<TurnFolder>,
    schema: bool,
}

/// Writes `turn/start` with the full frozen policy under the turn's guard;
/// a stop before its reply posts the delayed interrupt intent. On the
/// paired reply the turn's normalizer takes the lane (its acceptance
/// first) and the turn waits for its decision beside its orders.
async fn run_started(
    facts: &mut Turn<'_>,
    start: &Started<'_>,
    spec: TurnSpec,
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
    let requested = start.connection.request(
        |id| turn_start(id, &values, prompt),
        bounds,
        Purpose::Starts {
            lane: &start.thread.lease,
            turn,
        },
        Some(writes),
    );
    let requested = match requested {
        Ok(requested) => requested,
        Err(error) => return request_failed(facts, error),
    };
    facts.launch();
    let start_id = requested.id;
    let correlation = u64::try_from(start_id.get())
        .ok()
        .and_then(|id| AcceptanceToken::try_from(id).ok())
        .unwrap_or(AcceptanceToken::FIRST);
    let reply = await_reply(
        (requested.written, requested.reply),
        (orders, force),
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
        Err(cause) => return lost(facts, start.connection, cause),
    };
    let accepted = match reply.outcome {
        Ok(raw) => match result::<TurnStartResult>(&raw) {
            Ok(accepted) => accepted.turn.id,
            Err(_) => {
                return facts.failed(
                    RouteError::Protocol {
                        turn,
                        detail: "the turn/start reply is malformed",
                    },
                    None,
                );
            }
        },
        Err(error) => {
            return facts.rejected(StartRejected::VendorError(
                error.code.to_string(),
                error.message,
            ));
        }
    };
    let acceptance = Acceptance {
        correlation,
        vendor_turn_id: VendorTurnId::try_from(accepted.clone()).ok(),
        instance: facts.instance.clone(),
    };
    let delivery = normalize_on_tracker(facts, start, (&accepted, acceptance), activity);
    let accepted_turn = Accepted {
        id: accepted,
        start: start_id,
        delivery: &delivery,
    };
    let cut = wait(start, &accepted_turn, (orders, force)).await;
    settle_turn(facts, start, &accepted_turn, cut)
}

/// Starts the accepted turn's delivery: its normalizer, on the session's
/// tracker under `crash_on_panic` (X0 item 13.2), takes the lane from the
/// first message not yet taken, its acceptance first. The settlement
/// seals it however the turn ends.
fn normalize_on_tracker(
    facts: &Turn<'_>,
    start: &Started<'_>,
    (accepted, acceptance): (&str, Acceptance),
    activity: &crate::TurnActivity,
) -> Arc<Delivery> {
    let driver = facts.driver;
    let lane = start.thread.lease.lane();
    let before = lane
        .front_seq()
        .map_or_else(|| start.signal.enqueued(), |seq| seq.saturating_sub(1));
    let delivery = Delivery::new(before);
    facts.settle.deliver(&delivery);
    driver.tracker.spawn(crash_on_panic(
        Normalizing {
            delivery: Arc::clone(&delivery),
            lane: Arc::clone(lane),
            sink: driver.observations.clone(),
            normalizer: TurnNormalizer::new(start.schema),
            turn: facts.number,
            accepted: accepted.to_owned(),
            acceptance: Some(acceptance),
            evidence: Evidence {
                folder: Arc::clone(start.folder),
                connection: Arc::clone(start.connection),
                runtime: Arc::clone(&driver.runtime),
                session: driver.spec.session_id.clone(),
            },
            activity: activity.clone(),
            cancel: driver.cancel.clone(),
            health: Arc::clone(&driver.health),
        }
        .run(),
    ));
    delivery
}

/// An accepted turn's facts.
struct Accepted<'a> {
    id: String,
    start: ClientId,
    delivery: &'a Arc<Delivery>,
}

/// What ended an accepted turn's wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Cut {
    /// The daemon force.
    Forced,
    /// The delivery decided: the terminal, or why it stopped.
    Decided,
    /// The turn's own order reached its end.
    Order(EndCause),
    /// The connection failed and its evidence did not reach the turn in
    /// time (X0 items 13.1, 13.2).
    LossDeadline,
}

/// Waits for the turn's delivery to decide, beside its orders: never
/// behind the normalizer, so a blocked sink delays no control. A stop
/// order posts the turn's interrupt intent at once.
async fn wait(
    start: &Started<'_>,
    accepted: &Accepted<'_>,
    (orders, force): (&mut Orders, &mut ForceWatch),
) -> Cut {
    let failing = start.connection.failing();
    tokio::pin!(failing);
    let mut ending: Option<Ending> = None;
    let mut loss_at: Option<Instant> = None;
    loop {
        if accepted.delivery.decided() {
            return Cut::Decided;
        }
        let end_at = ending.map(|ending| ending.by.instant());
        tokio::select! {
            biased;
            () = forced(force) => return Cut::Forced,
            () = accepted.delivery.changed() => {}
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
/// (C1 §7.6 terminal first), then why delivery stopped, then the order.
fn settle_turn(
    facts: &Turn<'_>,
    start: &Started<'_>,
    accepted: &Accepted<'_>,
    cut: Cut,
) -> TurnEnd {
    let turn = facts.number;
    let sealed = accepted.delivery.seal();
    let terminal_decided = cut == Cut::Decided && sealed.terminal.is_some();
    let undelivered = sealed.partial || start.thread.lease.lane().front_seq().is_some();
    let abnormal = matches!(sealed.stop, Some(Stop::Lane(LaneEnd::Abnormal)));
    if !terminal_decided && (undelivered || abnormal || cut == Cut::LossDeadline) {
        lock_losses(&facts.session.losses).note(start.generation, sealed.position, UNKNOWN);
    }
    let reported = Some(if sealed.tools_open {
        WireCleanup::Uncertain
    } else {
        WireCleanup::Quiescent
    });
    let uncertain = |cause| facts.failure(cause, None, None);
    if cut == Cut::Forced {
        return uncertain(RouteError::ForceStopped { turn });
    }
    if let Some(retained) = sealed.terminal {
        return terminal_end(facts, retained, sealed.tools_open);
    }
    match (cut, sealed.stop) {
        (_, Some(Stop::Lane(end))) => {
            let (cause, loss) = lane_end(end, start.connection, turn);
            facts.failure(cause, loss, reported)
        }
        (_, Some(Stop::Protocol { detail, undecoded })) => {
            let mut end = facts.failure(RouteError::Protocol { turn, detail }, None, reported);
            if let Err(AdapterError::Route(failure)) = &mut end.outcome {
                failure.undecoded = undecoded;
            }
            end
        }
        (_, Some(Stop::Overflow)) => facts.failure(RouteError::Overflow { turn }, None, reported),
        (Cut::Order(EndCause::Wall), None) => uncertain(RouteError::Deadline { turn }),
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
        (Cut::Order(EndCause::Stopped) | Cut::Decided | Cut::Forced, None) => {
            uncertain(RouteError::Stopped { turn })
        }
    }
}

/// The turn's end with its retained terminal.
fn terminal_end(facts: &Turn<'_>, retained: Retained, tools_open: bool) -> TurnEnd {
    let Retained {
        mut terminal,
        structured,
    } = retained;
    terminal.structured_output = match structured {
        StructuredOutput::Json(json) => Some(json),
        StructuredOutput::NotRequested
        | StructuredOutput::Missing
        | StructuredOutput::NotJson
        | StructuredOutput::OverLimit => None,
    };
    TurnEnd {
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

/// A per-turn refusal as the definite rejection it is before submission.
fn start_rejected(kind: &RefusalKind, message: String) -> StartRejected {
    match kind {
        RefusalKind::BoundUnsupported => StartRejected::BoundUnsupported(message),
        RefusalKind::InvalidParam { field } => StartRejected::InvalidParam { field },
        RefusalKind::VendorOptionConflict => StartRejected::InvalidParam { field: "vendor" },
        RefusalKind::UnsupportedVerb
        | RefusalKind::HarnessUnavailable
        | RefusalKind::UnknownModel
        | RefusalKind::VersionRefused
        | RefusalKind::MissingCapability { .. } => StartRejected::Protocol(message),
    }
}
