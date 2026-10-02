//! Codex's driver turn (C2 §2, §4, §4.1; vendors/codex.md §2–§5; x.3.2
//! X0 items 1, 2, 8, 11, 12): one VIA turn on a shared `codex app-server`.
//!
//! The session leases a server from its first join until its close (AD16):
//! `prepare` pins the session's live server, or a live one of an equal key;
//! otherwise the turn launches one, or joins one launching. On each
//! connection generation the session opens its thread once, by
//! `thread/start` or, when an identity was confirmed, by `thread/resume` of
//! that exact thread; the reply's policy echoes are checked. Every turn is a
//! `turn/start` with the full frozen policy, accepted on its paired reply;
//! the thread's ingress lane is then normalized in decode order until the
//! turn's own `turn/completed`. A close detaches with `thread/unsubscribe`
//! and releases the lease; the last lease's release retires the server.
//!
//! Interrupt, the P7 window and steer are x.3.2 X4's: here a stop order
//! after acceptance writes one `turn/interrupt` and the turn ends at the
//! order's `close_by` (`uncertain`) unless the vendor's terminal comes
//! first.

use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::watch;
use tokio::time::Instant;
use via_routes::codex::{
    CommitOutcome, Connection, ConnectionLoss, DECLINE_DEADLINE, FINISH_BY, Lane, LaneEnd,
    LaneEvent, LaneItem, LossCause, Notification, PendingWrite, RequestError, Response, RpcError,
    SandboxMode, ServerFacts, ServerKey, ServerPin, ThreadResult, ThreadSettings, TurnFolder,
    TurnStart, TurnStartResult, WriteBounds, data, result, thread_resume, thread_start,
    thread_unsubscribe, turn_interrupt, turn_start,
};
use via_routes::{OutboundMessage, SendOutcome, StoreFailure};

use super::normalize::{self, DiscoveredModel, Step, StructuredOutput, TurnNormalizer};
use super::plan::{self as codex_plan, Sandbox};
use super::{ADAPTER_VERSION, CodexAdapter, HARNESS, PerTurn, refusals};
use crate::driver::turn::{CLEANUP_ALLOWANCE, end_active};
use crate::driver::{
    Active, ConnectionPin, DriverState, ForceWatch, Prepared, Retiring, SessionDriver, TurnCx,
    TurnSpec, lock, rejected,
};
use crate::harness::Harness;
use crate::instance::Incompatibility;
use crate::observation::{
    Acceptance, AdapterError, Identity, InstanceReport, Observation, ObservationItem, TurnEnd,
    TurnEvidence, VendorTerminal,
};
use crate::plan::{Bound, Inherit, RefusalKind};
use crate::runtime::event_stall;
use crate::{
    AcceptanceToken, Cleanup, Deadline, DriverFailure, ProcessOwner, RouteError, RouteFailure,
    StartRejected, StopOrder, StopWatch, TurnNumber, VendorTurnId,
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
}

/// The session's connection generation.
struct Attached {
    /// The session's lease, held from its first join until close (AD16).
    lease: ServerPin,
    connection: Arc<Connection>,
    generation: u64,
    /// The thread this generation opened, once it did.
    thread: Option<Thread>,
}

/// An open thread and the ingress lane its traffic is routed to.
#[derive(Clone)]
struct Thread {
    id: String,
    lane: Arc<Lane>,
}

impl CodexSession {
    pub(crate) fn new(adapter: Arc<CodexAdapter>) -> Self {
        Self {
            adapter,
            attached: Mutex::new(None),
            bound: Mutex::new(None),
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

    /// The close's detach (packet §2): `thread/unsubscribe` of the
    /// session's thread, its reply awaited by `deadline`, then the lease is
    /// released. Never a stdin close: the server is shared.
    pub(crate) async fn detach(&self, deadline: Deadline) {
        let Some(attached) = self.attached().take() else {
            return;
        };
        if let Some(thread) = &attached.thread
            && usable(&attached.connection)
        {
            let bounds = WriteBounds::StartBy {
                start_by: deadline,
                finish_by: deadline,
            };
            let unsubscribe = attached.connection.request(
                |id| thread_unsubscribe(id, &thread.id).map(OutboundMessage::Control),
                bounds,
                None,
            );
            if let Ok(requested) = unsubscribe {
                let answered = async {
                    let _written = requested.write.await;
                    let _reply = requested.reply.await;
                };
                let _answered = tokio::time::timeout_at(deadline.instant(), answered).await;
            }
            attached.connection.unregister(&thread.id, &thread.lane);
        }
        drop(attached);
    }

    /// The generation and thread of `connection`, attaching the session to
    /// it as a new generation when it is not the one attached (the earlier
    /// lease is released).
    fn attach(
        &self,
        pin: &ServerPin,
        connection: &Arc<Connection>,
        state: &Mutex<DriverState>,
    ) -> Option<(u64, Option<Thread>)> {
        let mut attached = self.attached();
        if let Some(current) = attached.as_ref()
            && Arc::ptr_eq(&current.connection, connection)
        {
            return Some((current.generation, current.thread.clone()));
        }
        let lease = pin.duplicate()?;
        let generation = {
            let mut state = lock(state);
            state.generation += 1;
            state.generation
        };
        let replaced = attached.replace(Attached {
            lease,
            connection: Arc::clone(connection),
            generation,
            thread: None,
        });
        drop(attached);
        drop(replaced);
        Some((generation, None))
    }

    /// Keeps the thread this generation opened.
    fn opened(&self, connection: &Arc<Connection>, thread: &Thread) {
        if let Some(attached) = self.attached().as_mut()
            && Arc::ptr_eq(&attached.connection, connection)
        {
            attached.thread = Some(thread.clone());
        }
    }

    /// The generation's thread is unusable (its lane ended): the next turn
    /// opens it again on a new connection.
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

/// Whether a connection still takes requests.
fn usable(connection: &Connection) -> bool {
    connection.failure().is_none() && connection.ended().is_none()
}

/// Ends the turn's logical lanes when the turn returns or is dropped: its
/// close order goes, and a waiting close sees the turn settled.
struct Settle<'a> {
    state: &'a Mutex<DriverState>,
    turn: TurnNumber,
    done: watch::Sender<Retiring>,
}

impl Drop for Settle<'_> {
    fn drop(&mut self) {
        end_active(self.state, self.turn);
        self.done.send_replace(Retiring::CleanedUp);
        self.done.send_replace(Retiring::Delivered);
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
}

impl Turn<'_> {
    /// The turn's end with `cause`, latched in the health lane as C2 §2
    /// latches a route failure.
    fn failed(&self, cause: RouteError, loss: Option<ConnectionLoss>) -> TurnEnd {
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
                exit: loss.and_then(|loss| loss.exit),
                launched: self.launched,
                cleanup: loss.map(|loss| loss.cleanup),
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

    /// Sends one observation of the turn; an undelivered one latches the
    /// observation overflow.
    async fn emit(&self, observation: Observation, vendor_turn: Option<&str>) -> Result<(), ()> {
        let item = ObservationItem {
            at: Instant::now(),
            vendor_turn: vendor_turn.and_then(|id| VendorTurnId::try_from(id.to_owned()).ok()),
            observation,
        };
        self.driver
            .observations
            .send(item, event_stall())
            .await
            .map_err(|_| self.driver.fail(DriverFailure::ObservationOverflow))
    }
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

/// How an ended lane fails the turn.
fn lane_end(end: LaneEnd, turn: TurnNumber) -> (RouteError, Option<ConnectionLoss>) {
    match end {
        LaneEnd::Overflow => (RouteError::Overflow { turn }, None),
        LaneEnd::Lost(loss) => (loss_cause(&loss, turn), Some(loss)),
        LaneEnd::Retired => (RouteError::TransportLost { turn }, None),
    }
}

/// The handshake's instance report (C2 §5 OD1): the `userAgent` version
/// and its status.
fn instance_report(facts: &ServerFacts) -> InstanceReport {
    let version = normalize::instance_version(&facts.user_agent).map(str::to_owned);
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

/// Runs one submitted turn (C2 §4.1), inline: nothing outlives it but the
/// session's lease and the connection's own tasks.
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
    let (close, close_rx) = watch::channel(None);
    let Some(done) = activate(driver, turn, close) else {
        return rejected(AdapterError::Rejected {
            reason: StartRejected::SessionGone,
            evidence: TurnEvidence::no_launch(false),
        });
    };
    let _settle = Settle {
        state: &driver.state,
        turn,
        done,
    };
    let mut orders = Orders {
        stop,
        close: close_rx,
        wall,
        cancel: driver.cancel.clone(),
    };
    let mut facts = Turn {
        driver,
        session,
        number: turn,
        instance: None,
        launched: false,
    };
    let folder = match folder(&facts).await {
        Ok(folder) => folder,
        Err(end) => return *end,
    };
    let pin = match join(&facts, (prepared, capacity), (&mut orders, &mut force)).await {
        Ok(pin) => pin,
        Err(end) => return *end,
    };
    let Some((connection, server)) = pin.live() else {
        return facts.failed(RouteError::TransportLost { turn }, None);
    };
    let catalog = adopt(&mut facts, &server);
    let Some((generation, thread)) = session.attach(&pin, &connection, &driver.state) else {
        return facts.failed(RouteError::TransportLost { turn }, None);
    };
    let Ok(effort) = vendor_effort(spec.effort.as_deref(), &catalog, &driver.spec.model) else {
        return facts.rejected(StartRejected::InvalidParam { field: "effort" });
    };
    if let Err(end) = link(&facts, &connection, wall).await {
        return *end;
    }
    let ids = Ids {
        connection: &connection,
        generation,
    };
    let thread = match thread {
        Some(thread) => thread,
        None => {
            match open_thread(&mut facts, &ids, sandbox.mode, (&mut orders, &mut force)).await {
                Ok(thread) => thread,
                Err(end) => return *end,
            }
        }
    };
    let start = Started {
        thread: &thread,
        connection: &connection,
        sandbox: &sandbox,
        effort: effort.as_deref(),
        folder: &folder,
        schema: spec.output_schema.is_some(),
    };
    let end = run_started(
        &mut facts,
        &start,
        spec,
        (&activity, &mut orders, &mut force),
    )
    .await;
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
fn adopt(facts: &mut Turn<'_>, server: &ServerFacts) -> Arc<[DiscoveredModel]> {
    let adapter = &facts.session.adapter;
    facts.instance = Some(instance_report(server));
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
        .sender()
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
    facts: &Turn<'_>,
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
                Err(failure) => return Err(Box::new(launch_failed(facts, failure))),
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
        Err(Some(failure)) => Err(Box::new(launch_failed(facts, failure))),
        // The turn's own order ended its wait: nothing of it was sent.
        Err(None) => Err(Box::new(
            facts.failed(unsent_cause(orders, force, turn), None),
        )),
    }
}

/// A launch's failure as the waiting turn reports it (C2 §2 health).
fn launch_failed(facts: &Turn<'_>, failure: via_routes::codex::LaunchFailure) -> TurnEnd {
    let failure = failure.route_failure(facts.number);
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
    generation: u64,
}

/// Opens the session's thread on this generation (packet §3): a resume of
/// the confirmed thread, else a start; the reply's echoes checked, its
/// identity confirmed.
async fn open_thread(
    facts: &mut Turn<'_>,
    ids: &Ids<'_>,
    mode: SandboxMode,
    (orders, force): (&mut Orders, &mut ForceWatch),
) -> Result<Thread, Box<TurnEnd>> {
    let driver = facts.driver;
    let turn = facts.number;
    let lane = Arc::new(Lane::default());
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
        Some(Arc::clone(&lane)),
    );
    let requested = requested.map_err(|error| Box::new(request_failed(facts, error)))?;
    facts.launched = true;
    let reply = match await_reply(requested.write, requested.reply, (orders, force)).await {
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
    let thread = Thread {
        id: opened.thread.id.clone(),
        lane,
    };
    confirm(facts, ids, (&opened, mode), resume, &thread)?;
    facts.session.opened(ids.connection, &thread);
    driver.state().identity = Some(thread.id.clone());
    let identity = Identity {
        vendor_session_id: thread.id.clone(),
        connection_id: connection_id(ids.generation),
        transcript: None,
        vendor_version: facts
            .instance
            .as_ref()
            .and_then(|instance| instance.vendor_version.clone()),
    };
    if facts
        .emit(Observation::IdentityConfirmed(identity), None)
        .await
        .is_err()
    {
        return Err(Box::new(facts.failed(RouteError::Overflow { turn }, None)));
    }
    Ok(thread)
}

/// Checks an opened thread before its identity is confirmed: the echoed
/// policy, a resume's thread ID, and its registration on the connection.
fn confirm(
    facts: &Turn<'_>,
    ids: &Ids<'_>,
    (opened, mode): (&ThreadResult, SandboxMode),
    resume: Option<String>,
    thread: &Thread,
) -> Result<(), Box<TurnEnd>> {
    let driver = facts.driver;
    let turn = facts.number;
    if let Some(field) = echo_differs(opened, mode) {
        ids.connection.unregister(&thread.id, &thread.lane);
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
    if let Some(requested) = resume
        && requested != thread.id
    {
        ids.connection.unregister(&thread.id, &thread.lane);
        driver.fail(DriverFailure::ResumeMismatch);
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
    if !ids.connection.registered(&thread.id, &thread.lane) {
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
    /// The connection ended: its loss, when it failed.
    Lost(Option<ConnectionLoss>),
    /// The start was not written whole.
    NotWritten(SendOutcome),
    /// The daemon force.
    Forced,
    /// The turn's own order reached its end.
    Ended(EndCause),
}

/// A turn whose request went unanswered.
fn lost(facts: &Turn<'_>, connection: &Connection, cause: Unanswered) -> TurnEnd {
    let turn = facts.number;
    match cause {
        Unanswered::Lost(loss) => {
            let loss = loss.or_else(|| match connection.ended() {
                Some(via_routes::codex::ConnectionEnd::Failed(loss)) => Some(loss),
                _ => None,
            });
            match loss {
                Some(loss) => facts.failed(loss_cause(&loss, turn), Some(loss)),
                None => facts.failed(RouteError::TransportLost { turn }, None),
            }
        }
        Unanswered::NotWritten(SendOutcome::NotWritten) => {
            let unsent = Turn {
                launched: false,
                instance: facts.instance.clone(),
                ..*facts
            };
            unsent.failed(RouteError::TransportLost { turn }, None)
        }
        Unanswered::NotWritten(_) => facts.failed(RouteError::TransportLost { turn }, None),
        Unanswered::Forced => facts.failed(RouteError::ForceStopped { turn }, None),
        Unanswered::Ended(EndCause::Stopped) => facts.failed(RouteError::Stopped { turn }, None),
        Unanswered::Ended(EndCause::Wall) => facts.failed(RouteError::Deadline { turn }, None),
    }
}

/// Awaits a written request's paired reply, bounded by the force and the
/// end of the turn's own order.
async fn await_reply(
    write: PendingWrite,
    reply: tokio::sync::oneshot::Receiver<Response>,
    (orders, force): (&mut Orders, &mut ForceWatch),
) -> Result<Response, Unanswered> {
    tokio::pin!(write);
    tokio::pin!(reply);
    let mut written = false;
    let mut ending: Option<Ending> = None;
    loop {
        let end_at = ending.map(|ending| ending.by.instant());
        tokio::select! {
            biased;
            () = forced(force) => return Err(Unanswered::Forced),
            outcome = &mut write, if !written => match outcome {
                Ok(SendOutcome::Written) => written = true,
                Ok(other) => return Err(Unanswered::NotWritten(other)),
                Err(_) => return Err(Unanswered::NotWritten(SendOutcome::Indeterminate)),
            },
            answered = &mut reply => return answered.map_err(|_| Unanswered::Lost(None)),
            found = orders.ordered(), if ending.is_none() => ending = Some(found),
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
    thread: &'a Thread,
    connection: &'a Arc<Connection>,
    sandbox: &'a Sandbox,
    effort: Option<&'a str>,
    folder: &'a TurnFolder,
    schema: bool,
}

/// Writes `turn/start` with the full frozen policy, accepts on its paired
/// reply, then normalizes the thread's lane until the turn's terminal.
async fn run_started(
    facts: &mut Turn<'_>,
    start: &Started<'_>,
    spec: TurnSpec,
    (activity, orders, force): (&crate::TurnActivity, &mut Orders, &mut ForceWatch),
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
    let requested = start
        .connection
        .request(|id| turn_start(id, &values, prompt), bounds, None);
    let requested = match requested {
        Ok(requested) => requested,
        Err(error) => return request_failed(facts, error),
    };
    facts.launched = true;
    let correlation = u64::try_from(requested.id.get())
        .ok()
        .and_then(|id| AcceptanceToken::try_from(id).ok())
        .unwrap_or(AcceptanceToken::FIRST);
    let reply = match await_reply(requested.write, requested.reply, (orders, force)).await {
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
    if facts
        .emit(Observation::Accepted(acceptance), Some(&accepted))
        .await
        .is_err()
    {
        return facts.failed(RouteError::Overflow { turn }, None);
    }
    let mut running = Running {
        normalizer: TurnNormalizer::new(start.schema),
        accepted,
        terminal: None,
    };
    running.run(facts, start, (activity, orders, force)).await
}

/// An accepted turn's delivery state.
struct Running {
    normalizer: TurnNormalizer,
    accepted: String,
    /// The one retained terminal (AD4): a second never replaces it.
    terminal: Option<VendorTerminal>,
}

/// How the delivery loop ended.
enum Ended {
    Terminal,
    Failed(RouteError, Option<ConnectionLoss>),
    Undecoded(&'static str),
    Order(EndCause),
    Forced,
}

impl Running {
    async fn run(
        &mut self,
        facts: &Turn<'_>,
        start: &Started<'_>,
        (activity, orders, force): (&crate::TurnActivity, &mut Orders, &mut ForceWatch),
    ) -> TurnEnd {
        let turn = facts.number;
        let mut ending: Option<Ending> = None;
        let ended = loop {
            let end_at = ending.map(|ending| ending.by.instant());
            tokio::select! {
                biased;
                () = forced(force) => break Ended::Forced,
                event = start.thread.lane.next() => {
                    activity.record(Instant::now());
                    if let Some(ended) = self.take(facts, start, event).await {
                        break ended;
                    }
                }
                found = orders.ordered(), if ending.is_none() => {
                    ending = Some(found);
                    interrupt(start, &self.accepted, found.by);
                }
                () = sleep_until(end_at), if end_at.is_some() => {
                    break Ended::Order(ending.map_or(EndCause::Stopped, |ending| ending.cause));
                }
            }
        };
        let uncertain = |cause| facts.failed(cause, None);
        match ended {
            Ended::Terminal => TurnEnd {
                terminal: self.terminal.take(),
                instance: facts.instance.clone(),
                leftovers: None,
                outcome: Ok(TurnEvidence {
                    exit: None,
                    cleanup: if self.normalizer.tools_open() {
                        Cleanup::Uncertain
                    } else {
                        Cleanup::Quiescent
                    },
                    journal_uncertain: false,
                }),
            },
            Ended::Failed(cause, loss) => {
                let mut end = facts.failed(cause, loss);
                end.terminal = self.terminal.take();
                end
            }
            Ended::Undecoded(detail) => {
                let mut end = facts.failed(RouteError::Protocol { turn, detail }, None);
                if let Err(AdapterError::Route(failure)) = &mut end.outcome {
                    failure.undecoded = start.folder.take_undecoded();
                }
                end
            }
            Ended::Order(EndCause::Stopped) => uncertain(RouteError::Stopped { turn }),
            Ended::Order(EndCause::Wall) => uncertain(RouteError::Deadline { turn }),
            Ended::Forced => uncertain(RouteError::ForceStopped { turn }),
        }
    }

    /// Takes one lane event; the loop's end, if it is one.
    async fn take(
        &mut self,
        facts: &Turn<'_>,
        start: &Started<'_>,
        event: LaneEvent,
    ) -> Option<Ended> {
        let turn = facts.number;
        let item = match event {
            LaneEvent::Item(item) => *item,
            LaneEvent::End(end) => {
                let (cause, loss) = lane_end(end, turn);
                return Some(Ended::Failed(cause, loss));
            }
        };
        match item {
            LaneItem::Notification {
                notification,
                staged,
            } => {
                let ended = self.notification(facts, &notification).await;
                drop(staged);
                ended
            }
            LaneItem::Declined {
                request,
                decoded_at,
                mut written,
            } => {
                if self.normalizer.note_decline(&request).is_err() {
                    return Some(Ended::Failed(RouteError::Overflow { turn }, None));
                }
                // Reported only once written whole by its deadline (packet
                // §4): a failed write fails the connection instead.
                let whole = tokio::time::timeout_at(
                    decoded_at + DECLINE_DEADLINE,
                    written.wait_for(Option::is_some),
                )
                .await
                .is_ok_and(|outcome| outcome.is_ok_and(|outcome| *outcome == Some(true)));
                if whole {
                    let declined = Observation::RequestDeclined(normalize::decline(&request));
                    if facts.emit(declined, Some(&self.accepted)).await.is_err() {
                        return Some(Ended::Failed(RouteError::Overflow { turn }, None));
                    }
                }
                None
            }
            LaneItem::Malformed { staged, detail } => {
                start
                    .folder
                    .keep_undecoded(staged.bytes(), "the turn's message")
                    .await;
                Some(Ended::Undecoded(detail))
            }
        }
    }

    /// One notification of the thread: another turn's is dropped; this
    /// turn's is normalized and delivered.
    async fn notification(
        &mut self,
        facts: &Turn<'_>,
        notification: &Notification,
    ) -> Option<Ended> {
        let turn = facts.number;
        if notification.turn_id().is_some_and(|id| id != self.accepted) {
            return None;
        }
        let step = match self.normalizer.observe(notification, Instant::now()) {
            Ok(step) => step,
            Err(normalize::NormalizeError::Protocol(detail)) => {
                return Some(Ended::Failed(RouteError::Protocol { turn, detail }, None));
            }
            Err(normalize::NormalizeError::Overflow) => {
                return Some(Ended::Failed(RouteError::Overflow { turn }, None));
            }
        };
        match step {
            Step::Activity => None,
            Step::Observations(observations) => {
                for observation in observations {
                    if facts.emit(observation, Some(&self.accepted)).await.is_err() {
                        return Some(Ended::Failed(RouteError::Overflow { turn }, None));
                    }
                }
                None
            }
            Step::Terminal {
                mut terminal,
                structured,
            } => {
                if self.terminal.is_none() {
                    terminal.structured_output = match structured {
                        StructuredOutput::Json(json) => Some(json),
                        StructuredOutput::NotRequested
                        | StructuredOutput::Missing
                        | StructuredOutput::NotJson
                        | StructuredOutput::OverLimit => None,
                    };
                    self.terminal = Some(*terminal);
                }
                Some(Ended::Terminal)
            }
        }
    }
}

/// Writes one `turn/interrupt` of the accepted turn on the control path,
/// unanswered here: its terminal ends the turn.
fn interrupt(start: &Started<'_>, accepted: &str, by: Deadline) {
    let bounds = WriteBounds::StartBy {
        start_by: by,
        finish_by: by,
    };
    let written = start.connection.request(
        |id| turn_interrupt(id, &start.thread.id, accepted).map(OutboundMessage::Control),
        bounds,
        None,
    );
    // The write proceeds without its future; its reply is paired and
    // dropped as abandoned.
    drop(written);
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
