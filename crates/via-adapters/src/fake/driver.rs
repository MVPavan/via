//! The fake's driver turn (C2 §2, §4, §4.1; adapter design AD3–AD9):
//! launches the turn's process through the C2 route lane on a task the
//! session's tracker owns, normalizes each decoded message into session
//! observations in decode order, and builds the turn's one `TurnEnd`.

use std::borrow::Cow;
use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::{FakeAdapter, FakeProfile};
use crate::driver::{
    Active, DriverState, ForceWatch, Prepared, Reservation, SessionDriver, TurnCx, TurnSpec, latch,
    lock, rejected,
};
use crate::harness::Harness;
use crate::observation::{
    Acceptance, AdapterError, ClassHint, CostReport, Decline, Denial, DenialKind, Identity,
    InstanceReport, Observation, ObservationItem, ObservationSink, ProgressMarks, SteerDelivery,
    StopReason, TurnEnd, TurnEvidence, Undelivered, UsageSample, VendorTerminal,
};
use crate::plan::{Refusal, RefusalKind, TurnParams, VersionStatus};
use crate::runtime::{cleanup, event_stall};
use crate::{
    AcceptanceToken, Deadline, DriverFailure, DriverHealth, PrivateProcessSpec, ProcessOwner,
    RouteError, RouteFailure, StartRejected, StopCause, StopOrder, StopWatch, VendorTerminalStatus,
    VendorTurnId, final_text_pieces,
};
use via_routes::{
    FakeClassHint, FakeDenialKind, FakeMessage, FakeRoute, FakeTerminal, FakeTurn, FakeUsage, Lane,
    Retirement, RouteMessage, StopSources, TerminalStatus, TurnStart, WireCleanup,
};

/// S1's cleanup allowance: the wall's one cutoff is this after the wall
/// (C2 §4.1).
const CLEANUP_ALLOWANCE: Duration = Duration::from_secs(3);

/// How often the idle source looks for its scenario gate.
const IDLE_POLL: Duration = Duration::from_millis(10);

/// How the delivery after Route ended went.
enum Rest {
    /// Everything Route handed over reached the session channel.
    Delivered,
    /// A delivery failed: Core stalled or went away.
    Undelivered,
    /// The daemon force ended a delivery that had to wait.
    Forced,
}

/// A pending delivery, polled beside the route (Task 4 design §9).
type Delivery = Pin<Box<dyn Future<Output = Result<(), Undelivered>> + Send>>;

/// The turn's reservation, shared by its task and `run_turn`.
type Shared = Arc<Mutex<Reservation>>;

/// Locks the shared reservation; no code panics while holding it.
fn held(reservation: &Shared) -> MutexGuard<'_, Reservation> {
    reservation.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Armed while `run_turn` awaits its result: dropped armed, the future was
/// abandoned, which latches its own first cause (C2 §2 health) before
/// Route sees the closed hop.
struct Abandonment<'a>(Option<&'a watch::Sender<DriverHealth>>);

impl Drop for Abandonment<'_> {
    fn drop(&mut self) {
        if let Some(health) = self.0 {
            latch(health, DriverFailure::TurnAbandoned);
        }
    }
}

/// The ID identity confirmations name for connection `generation`.
pub(crate) fn connection_id(generation: u64) -> String {
    format!("fake-{generation}")
}

/// Runs one submitted turn (C2 §4.1): per-turn values are checked before
/// anything launches (AD18, C2 §7 item 13); then the turn's process runs
/// through Route on a tracker-owned task while each decoded message is
/// delivered to the session channel. A delivery blocked for the stall bound
/// drops the hop, so Route fails the turn `overflow`, keeping the decoded
/// terminal (AD4). The persistent connection is committed only once the
/// whole logical turn was delivered. Dropping this future leaves the task,
/// which owns the turn's cleanup and its share of the reservation, running.
pub(crate) async fn run_turn(
    driver: &SessionDriver,
    adapter: &FakeAdapter,
    mut spec: TurnSpec,
    cx: TurnCx,
) -> TurnEnd {
    if let Some(refused) = refused_values(adapter, &spec) {
        return refused;
    }
    let TurnCx {
        turn,
        prepared,
        capacity,
        activity,
        wall,
        tool_grace,
        stop,
        force,
    } = cx;
    let first = matches!(prepared, Prepared::NeedsConnection);
    let ordered = ordered(stop.clone(), force.clone(), driver.cancel.clone());
    let connected = driver.connect((prepared, capacity), ordered).await;
    let (generation, capacity, reservation) = match connected {
        Ok(connection) => connection,
        Err(error) => return rejected(error),
    };
    let reservation = Arc::new(Mutex::new(reservation));
    let (process, start) = match launch_inputs(driver, adapter, &mut spec, (turn, first), capacity)
    {
        Ok(inputs) => inputs,
        Err(end) => return *end,
    };
    let profile = adapter.profile();
    let persistent = profile.persistent;
    let (steer, steer_lane) = via_routes::steer_lane();
    let (close, close_rx) = watch::channel(None);
    let (done, retiring) = watch::channel(false);
    let identity = {
        let mut state = driver.state();
        state.active = Some(Active { turn, steer, close });
        state.retiring = Some(retiring);
        state.identity.clone()
    };
    // Route hands one message at a time: while it is full Route reads no
    // further message.
    let (hop, hop_rx) = mpsc::channel::<RouteMessage>(1);
    let lane = Lane {
        persistent,
        handshake: profile.handshake.as_ref().map(|decl| decl.requires.clone()),
        tool_grace,
        steer: Some(steer_lane),
        identity,
        effort: spec.effort,
    };
    let (logical, logical_rx) = oneshot::channel();
    driver.tracker.spawn(turn_task(TurnTask {
        route: Arc::clone(&driver.route),
        process,
        start,
        hop,
        signals: (wall, force.clone(), stop),
        close: close_rx,
        cancel: driver.cancel.clone(),
        lane,
        logical,
        reservation: Arc::clone(&reservation),
        state: Arc::clone(&driver.state),
        reports: (Arc::clone(&driver.health), Arc::clone(&driver.journal)),
        done,
    }));
    let mut normalizer = Normalizer {
        generation,
        state: Arc::clone(&driver.state),
        steer: steer_delivery(profile),
        vendor_closed: false,
    };
    let cutoff = Deadline::at(wall.instant() + CLEANUP_ALLOWANCE);
    let mut abandonment = Abandonment(Some(&driver.health));
    let (result, rest) = deliver_beside(
        // A closed channel is the task ending without its turn, handled
        // below as an owned-task failure.
        async { logical_rx.await.ok() },
        hop_rx,
        &mut normalizer,
        &driver.observations,
        &activity,
        (force, cutoff),
        &driver.health,
    )
    .await;
    abandonment.0 = None;
    end_active(&driver.state, turn);
    let Some(result) = result else {
        // The task ended without its turn: it failed (C2 §2 health).
        driver.fail(DriverFailure::OwnedTask);
        return rejected(AdapterError::TaskFailed);
    };
    settle(driver, &reservation, &result, &rest, &normalizer);
    drop(reservation);
    report_mismatch(driver, &result, cutoff).await;
    let end = turn_end(adapter, turn, result, &rest, persistent);
    after_persistent_turn(
        driver,
        adapter,
        (turn, generation),
        &end,
        normalizer.vendor_closed,
    );
    end
}

/// After the final delivery: one that failed latches `overflow` (C2 §2),
/// and a kept server stays the session's only once its whole logical turn
/// was delivered, the vendor did not close it and it reported no other
/// session (AD16, C2 §2 Reopen); otherwise its generation is invalid.
fn settle(
    driver: &SessionDriver,
    reservation: &Shared,
    result: &FakeTurn,
    rest: &Rest,
    normalizer: &Normalizer,
) {
    if matches!(rest, Rest::Undelivered) {
        driver.fail(DriverFailure::ObservationOverflow);
    }
    if !result.server_kept {
        return;
    }
    let mut share = held(reservation);
    if matches!(rest, Rest::Delivered)
        && !normalizer.vendor_closed
        && result.late_mismatch.is_none()
    {
        share.commit(driver.cancel.is_cancelled());
    } else {
        share.release();
    }
}

/// AD18, C2 §7 item 13: a per-turn value the route refuses rejects the
/// turn before anything launches.
fn refused_values(adapter: &FakeAdapter, spec: &TurnSpec) -> Option<TurnEnd> {
    let params = TurnParams {
        effort: spec.effort.clone(),
        bound: spec.bound.clone(),
        output_schema: spec.output_schema.is_some(),
        max_steps: spec.max_steps,
        vendor: spec.vendor.clone(),
    };
    let refusal = adapter
        .check_turn(Harness::Fake.route(), &params)
        .into_iter()
        .next()?;
    Some(rejected(AdapterError::Rejected {
        reason: start_rejected(refusal),
        evidence: TurnEvidence::no_launch(false),
    }))
}

/// The turn's process and its C2 start. Per-turn profile: Host holds the
/// slot for the group's life; the persistent profile's slot stays with the
/// reservation (decision H1).
fn launch_inputs(
    driver: &SessionDriver,
    adapter: &FakeAdapter,
    spec: &mut TurnSpec,
    (turn, first): (crate::TurnNumber, bool),
    capacity: Option<crate::CapacityToken>,
) -> Result<(PrivateProcessSpec, TurnStart), Box<TurnEnd>> {
    let session_id = driver.spec.session_id.clone();
    let owner = ProcessOwner {
        session_id: session_id.clone(),
        turn,
    };
    let Ok(mut process) = adapter.process_spec(owner, &driver.spec.cwd) else {
        return Err(Box::new(rejected(AdapterError::Unavailable)));
    };
    process.capacity = capacity;
    let prompt = std::mem::take(&mut spec.prompt);
    let start = start_values(driver, adapter.profile(), spec, first).and_then(|values| {
        TurnStart::new(session_id.as_str().to_owned(), turn, prompt)?.with_values(values)
    });
    match start {
        Ok(start) => Ok((process, start)),
        Err(_) => Err(Box::new(rejected(AdapterError::Rejected {
            reason: StartRejected::Protocol("the fake start cannot be built".to_owned()),
            evidence: TurnEvidence::no_launch(false),
        }))),
    }
}

/// C2 §2 Reopen: a mismatching identity, the turn's failure or one after
/// its terminal, is reported on the session channel by the wall's cutoff;
/// one that cannot be is an overflow.
async fn report_mismatch(driver: &SessionDriver, result: &FakeTurn, cutoff: Deadline) {
    let ((
        Err(RouteFailure {
            cause:
                RouteError::ResumeMismatch {
                    requested,
                    returned,
                    ..
                },
            ..
        }),
        _,
    )
    | (_, Some((requested, returned)))) = (&result.outcome, &result.late_mismatch)
    else {
        return;
    };
    let mismatch = ObservationItem {
        at: tokio::time::Instant::now(),
        vendor_turn: None,
        observation: Observation::ResumeMismatch {
            requested: requested.clone(),
            returned: returned.clone(),
        },
    };
    let sent = tokio::time::timeout_at(
        cutoff.instant(),
        driver.observations.send(mismatch, event_stall()),
    )
    .await;
    if !matches!(sent, Ok(Ok(()))) {
        driver.fail(DriverFailure::ObservationOverflow);
    }
}

/// The persistent profile after a turn returned: a vendor close in the
/// turn ends its connection, and the scenario's idle close starts now.
/// Nothing on the per-turn profile.
fn after_persistent_turn(
    driver: &SessionDriver,
    adapter: &FakeAdapter,
    (turn, generation): (crate::TurnNumber, u64),
    end: &TurnEnd,
    vendor_closed: bool,
) {
    if !adapter.profile().persistent {
        return;
    }
    if vendor_closed {
        driver.state().vendor_closed = true;
        driver.disconnect(generation);
    }
    if let Some(idle) = &adapter.profile().idle_close
        && idle.after_turn == turn.get()
        && end.outcome.is_ok()
    {
        driver.tracker.spawn(idle_source(
            (Arc::clone(&driver.state), Arc::clone(&driver.barrier)),
            generation,
            (driver.observations.clone(), Arc::clone(&driver.health)),
            (
                adapter.sync_dir().join(format!("{}.release", idle.gate)),
                idle.reason.clone(),
            ),
            driver.cancel.clone(),
        ));
    }
}

/// Resolves once the turn is ordered to end: Core's stop order, the
/// daemon force, or the session's cancellation, which a driver close
/// includes.
async fn ordered(mut stop: StopWatch, mut force: ForceWatch, cancel: CancellationToken) {
    let stopped = async {
        if stop.wait_for(Option::is_some).await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let forced = async {
        if force.wait_for(Option::is_some).await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = stopped => {}
        () = forced => {}
        () = cancel.cancelled() => {}
    }
}

/// The C2 start's effective values (adapter design §3.2): the turn's own,
/// which `check_turn` admitted, and on a connection generation's first
/// turn the session's model and supported instructions.
fn start_values(
    driver: &SessionDriver,
    profile: &FakeProfile,
    spec: &TurnSpec,
    first: bool,
) -> Result<Map<String, Value>, &'static str> {
    let unencodable = |_| "a fake start value cannot be encoded";
    let mut values = Map::new();
    if let Some(effort) = &spec.effort {
        values.insert("effort".to_owned(), Value::from(effort.as_str()));
    }
    if let Some(bound) = &spec.bound {
        values.insert(
            "bound".to_owned(),
            serde_json::to_value(bound).map_err(unencodable)?,
        );
    }
    if let Some(schema) = &spec.output_schema {
        values.insert(
            "output_schema".to_owned(),
            serde_json::from_str(schema.get()).map_err(unencodable)?,
        );
    }
    if let Some(max_steps) = spec.max_steps {
        values.insert("max_steps".to_owned(), Value::from(max_steps));
    }
    if first {
        values.insert("model".to_owned(), Value::from(driver.spec.model.as_str()));
        if let Some(instructions) = &driver.spec.instructions
            && profile.capabilities.params.instructions.meets(true)
        {
            values.insert(
                "instructions".to_owned(),
                Value::from(instructions.as_str()),
            );
        }
    }
    Ok(values)
}

/// How the profile's steer support reports a delivery (C2 §2).
fn steer_delivery(profile: &FakeProfile) -> SteerDelivery {
    match &profile.capabilities.verbs.steer {
        crate::Support::Partial { semantics } => {
            SteerDelivery::Partial(Cow::Owned(semantics.clone()))
        }
        crate::Support::Native | crate::Support::Unsupported { .. } => SteerDelivery::Injected,
    }
}

/// The running turn's steer lane and close order end with its logical
/// turn; a later turn's are kept.
fn end_active(state: &Mutex<DriverState>, turn: crate::TurnNumber) {
    let mut state = lock(state);
    if state
        .active
        .as_ref()
        .is_some_and(|active| active.turn == turn)
    {
        state.active = None;
    }
}

/// The failure a Route result latches in the health lane (C2 §2):
/// protocol, transport loss, overflow, a Store failure, the server's loss
/// or a resume mismatch, the turn's own or one after its terminal.
fn route_failure(turn: &FakeTurn) -> Option<DriverFailure> {
    let Err(failure) = &turn.outcome else {
        return turn
            .late_mismatch
            .as_ref()
            .map(|_| DriverFailure::ResumeMismatch);
    };
    match &failure.cause {
        cause @ (RouteError::Protocol { .. }
        | RouteError::TransportLost { .. }
        | RouteError::Overflow { .. }
        | RouteError::Store { .. }) => Some(DriverFailure::Route(cause.clone())),
        RouteError::ServerLost { .. } => Some(DriverFailure::ServerLost),
        RouteError::ResumeMismatch { .. } => Some(DriverFailure::ResumeMismatch),
        RouteError::ProcessExited { .. }
        | RouteError::Stopped { .. }
        | RouteError::Deadline { .. }
        | RouteError::ForceStopped { .. }
        | RouteError::HandshakeRefused { .. }
        | RouteError::InvalidParam { .. } => turn
            .late_mismatch
            .as_ref()
            .map(|_| DriverFailure::ResumeMismatch),
    }
}

/// Test builds only: `VIA_TEST_FAKE_RETIREMENT_UNCERTAIN=<n>`, read once
/// per daemon's adapter, makes that daemon's `n`th launched persistent
/// retirement unproven, as no fake profile can (Sol r3 N10).
#[cfg(feature = "test-failpoints")]
pub(crate) struct RetirementFault {
    nth: u64,
    launched: std::sync::atomic::AtomicU64,
}

#[cfg(feature = "test-failpoints")]
impl RetirementFault {
    fn new(nth: u64) -> Self {
        Self {
            nth,
            launched: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The fault the environment names, if any.
    pub(crate) fn from_environment() -> Option<Arc<Self>> {
        std::env::var("VIA_TEST_FAKE_RETIREMENT_UNCERTAIN")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(|nth| Arc::new(Self::new(nth)))
    }

    /// Counts one launched retirement; true for the `n`th.
    fn injects(&self) -> bool {
        self.launched
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1)
            == self.nth
    }
}

/// Whether a persistent connection's helper retirement left its cleanup
/// unproven (decision H1): a health failure, as no turn reports it. In
/// test builds a [`RetirementFault`] adds its injected one; it never
/// hides a real one (Sol r3 N10).
fn retirement_uncertain(
    retirement: &Retirement,
    #[cfg(feature = "test-failpoints")] fault: Option<&RetirementFault>,
) -> bool {
    #[cfg(feature = "test-failpoints")]
    let injected = retirement.launched && fault.is_some_and(RetirementFault::injects);
    #[cfg(not(feature = "test-failpoints"))]
    let injected = false;
    injected
        || retirement.launched
            && (retirement.cleanup != Some(WireCleanup::Quiescent) || retirement.journal_uncertain)
}

/// One turn's route work, owned by the session's tracker.
struct TurnTask {
    route: Arc<FakeRoute>,
    process: PrivateProcessSpec,
    start: TurnStart,
    hop: mpsc::Sender<RouteMessage>,
    signals: (Deadline, ForceWatch, StopWatch),
    close: watch::Receiver<Option<StopOrder>>,
    cancel: CancellationToken,
    lane: Lane,
    logical: oneshot::Sender<FakeTurn>,
    reservation: Shared,
    state: Arc<Mutex<DriverState>>,
    /// The driver's health lane and its journal report.
    reports: (Arc<watch::Sender<DriverHealth>>, Arc<watch::Sender<bool>>),
    done: watch::Sender<bool>,
}

/// Runs the turn through Route with Core's stop order merged with the
/// driver's close and the session's cancellation. At the logical turn's
/// end its failure is latched in the health lane, at once and whatever
/// becomes of `run_turn`; a turn that ended its connection invalidates the
/// generation, and the turn goes to `run_turn`, which commits a kept one.
/// The process's retirement is recorded after it.
async fn turn_task(task: TurnTask) {
    let TurnTask {
        route,
        process,
        start,
        hop,
        signals: (wall, force, core_stop),
        close,
        cancel,
        lane,
        logical,
        reservation,
        state,
        reports: (health, journal),
        done,
    } = task;
    let turn = start.turn();
    let persistent = lane.persistent;
    let (merged, merged_rx) =
        watch::channel(earliest(core_stop.borrow().clone(), close.borrow().clone()));
    // Route reads the sources too where the relay below may lag: an order
    // set before the pre-ARM launch gate wins there (design §2 rule 1).
    let sources: StopSources = {
        let (core, close, cancel) = (core_stop.clone(), close.clone(), cancel.clone());
        Arc::new(move || {
            core.borrow().is_some() || close.borrow().is_some() || cancel.is_cancelled()
        })
    };
    let (inner, inner_rx) = oneshot::channel();
    let stop = (merged_rx, sources);
    let route_turn = route.turn(process, start, hop, (wall, force, stop), lane, inner);
    let relay = async {
        let Ok(turn_result) = inner_rx.await else {
            return;
        };
        if let Some(cause) = route_failure(&turn_result) {
            latch(&health, cause);
        }
        if !turn_result.server_kept {
            held(&reservation).release();
        }
        end_active(&state, turn);
        // `run_turn` was dropped: the retirement below is still owned here.
        let _unread = logical.send(turn_result);
    };
    let retirement = tokio::select! {
        (retirement, ()) = async { tokio::join!(route_turn, relay) } => retirement,
        never = merge_stops(core_stop, close, &cancel, &merged) => match never {},
    };
    // A persistent connection's retirement journal is no turn's: its
    // uncertainty is reported apart from the cleanup's, before the health
    // failure that retires the lane (critical r1 #4).
    if persistent && retirement.launched && retirement.journal_uncertain {
        journal.send_replace(true);
    }
    if persistent
        && retirement_uncertain(
            &retirement,
            #[cfg(feature = "test-failpoints")]
            lock(&state).retirement_fault.as_deref(),
        )
    {
        latch(&health, DriverFailure::RetirementUncertain);
    }
    held(&reservation).retired(retirement);
    // An uncommitted slot is released with the last share, after the
    // cleanup and after `run_turn`.
    drop(reservation);
    done.send_replace(true);
}

/// Keeps `merged` at the earliest of Core's stop order, the driver's close
/// order and, once the session is cancelled, an immediate close. Never
/// returns.
async fn merge_stops(
    mut core: StopWatch,
    mut close: watch::Receiver<Option<StopOrder>>,
    cancel: &CancellationToken,
    merged: &watch::Sender<Option<StopOrder>>,
) -> Infallible {
    let (mut core_open, mut close_open, mut cancelled) = (true, true, false);
    loop {
        let mut order = earliest(
            core.borrow_and_update().clone(),
            close.borrow_and_update().clone(),
        );
        if cancelled {
            let now = tokio::time::Instant::now();
            order = earliest(
                order,
                Some(StopOrder {
                    cause: StopCause::Close,
                    // Route acts only on the times; Core never sees this order.
                    requested_at: String::new(),
                    force_at: Deadline::at(now),
                    close_by: Deadline::at(now + CLEANUP_ALLOWANCE),
                }),
            );
        }
        merged.send_if_modified(|current| {
            if same_order(current.as_ref(), order.as_ref()) {
                false
            } else {
                *current = order;
                true
            }
        });
        tokio::select! {
            changed = core.changed(), if core_open => core_open = changed.is_ok(),
            changed = close.changed(), if close_open => close_open = changed.is_ok(),
            () = cancel.cancelled(), if !cancelled => cancelled = true,
            else => std::future::pending::<()>().await,
        }
    }
}

/// The order whose `force_at` comes first.
fn earliest(first: Option<StopOrder>, second: Option<StopOrder>) -> Option<StopOrder> {
    match (first, second) {
        (Some(first), Some(second)) => {
            Some(if second.force_at.instant() < first.force_at.instant() {
                second
            } else {
                first
            })
        }
        (first, None) => first,
        (None, second) => second,
    }
}

/// Whether two orders act the same: cause and times.
fn same_order(first: Option<&StopOrder>, second: Option<&StopOrder>) -> bool {
    match (first, second) {
        (Some(first), Some(second)) => {
            first.cause == second.cause
                && first.force_at.instant() == second.force_at.instant()
                && first.close_by.instant() == second.close_by.instant()
        }
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

/// The persistent emulation's idle source (decision H1, C2 §4): once the
/// scenario's gate `release` exists, the emulated server closes its idle
/// session: the slot is released, the pin invalidated, and a session-level
/// `VendorClosed` sent; one the channel does not take latches `overflow`.
/// Started only after the turn before it returned; a later connection or
/// the session's cancellation ends it. It holds the generation barrier
/// from its check until its item was delivered or given up, so the next
/// connection's first observation follows it.
async fn idle_source(
    (state, barrier): (Arc<Mutex<DriverState>>, Arc<tokio::sync::Mutex<()>>),
    generation: u64,
    (sink, health): (ObservationSink, Arc<watch::Sender<DriverHealth>>),
    (release, reason): (PathBuf, String),
    cancel: CancellationToken,
) {
    loop {
        // An unreadable sync directory reads as a gate still held.
        if tokio::fs::try_exists(&release).await.unwrap_or(false) {
            break;
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(IDLE_POLL) => {}
        }
    }
    // Held until the close's item was delivered (C2 §4 generation barrier).
    let _barrier = tokio::select! {
        () = cancel.cancelled() => return,
        barrier = barrier.lock() => barrier,
    };
    let released = {
        let mut state = lock(&state);
        if state.generation != generation || !state.live || state.closed {
            return;
        }
        state.live = false;
        state.vendor_closed = true;
        state.capacity.take()
    };
    drop(released);
    let closed = ObservationItem {
        at: tokio::time::Instant::now(),
        vendor_turn: None,
        observation: Observation::VendorClosed(reason),
    };
    tokio::select! {
        () = cancel.cancelled() => {}
        sent = sink.send(closed, event_stall()) => {
            // The close itself is already in effect; its item is lost.
            if sent.is_err() {
                latch(&health, DriverFailure::ObservationOverflow);
            }
        }
    }
}

/// Polls `route` while delivering what it hands over, then delivers the
/// rest by the wall's cutoff: data deliverable at once still goes under
/// the daemon force.
async fn deliver_beside(
    route: impl Future<Output = Option<FakeTurn>>,
    hop_rx: mpsc::Receiver<RouteMessage>,
    normalizer: &mut Normalizer,
    sink: &ObservationSink,
    activity: &crate::TurnActivity,
    (mut force, cutoff): (ForceWatch, Deadline),
    health: &watch::Sender<DriverHealth>,
) -> (Option<FakeTurn>, Rest) {
    tokio::pin!(route);
    let stall = event_stall();
    let mut hop_rx = Some(hop_rx);
    let mut delivery: Option<Delivery> = None;
    let mut delivered = true;
    let result = loop {
        tokio::select! {
            biased;
            outcome = poll_delivery(delivery.as_mut()), if delivery.is_some() => {
                delivery = None;
                if outcome.is_err() {
                    // Latched at once (C2 §2); Route observes the closed hop
                    // as overflow, or as the force's stop under a force.
                    latch(health, DriverFailure::ObservationOverflow);
                    hop_rx = None;
                    delivered = false;
                }
            }
            message = recv(hop_rx.as_mut()), if delivery.is_none() && hop_rx.is_some() => {
                match message {
                    Some(message) => {
                        let at = tokio::time::Instant::now();
                        activity.record(at);
                        let items = normalizer.items(message, at);
                        delivery = Some(Box::pin(send_all(items, sink.clone(), stall)));
                    }
                    None => hop_rx = None,
                }
            }
            result = &mut route => break result,
        }
    };
    if !delivered {
        return (result, Rest::Undelivered);
    }
    let rest = async {
        if let Some(delivery) = delivery
            && delivery.await.is_err()
        {
            return Rest::Undelivered;
        }
        if let Some(receiver) = hop_rx.as_mut() {
            while let Ok(message) = receiver.try_recv() {
                let at = tokio::time::Instant::now();
                activity.record(at);
                let items = normalizer.items(message, at);
                if send_all(items, sink.clone(), stall).await.is_err() {
                    return Rest::Undelivered;
                }
            }
        }
        Rest::Delivered
    };
    // One cutoff (C2 §4.1): no delivery outlives the wall plus 3 s.
    let rest = tokio::select! {
        biased;
        rest = tokio::time::timeout_at(cutoff.instant(), rest) => rest.unwrap_or(Rest::Undelivered),
        () = forced(&mut force) => Rest::Forced,
    };
    (result, rest)
}

/// The turn's one result (C2 §4.1). A Route failure is the first cause;
/// undelivered data fails a success `overflow`, and the daemon force that
/// ended the delivery `force_stopped`; either keeps Route's evidence. An
/// effort the instance's catalog lacks is the definite rejection it is.
fn turn_end(
    adapter: &FakeAdapter,
    number: crate::TurnNumber,
    turn: FakeTurn,
    rest: &Rest,
    persistent: bool,
) -> TurnEnd {
    let FakeTurn {
        terminal,
        handshake,
        acknowledged,
        outcome,
        ..
    } = turn;
    let instance = handshake.map(|handshake| {
        let checked = adapter.profile().handshake.as_ref().is_some_and(|decl| {
            handshake
                .vendor_version
                .as_ref()
                .is_some_and(|version| decl.checked.contains(version))
        });
        InstanceReport {
            vendor_version: handshake.vendor_version,
            version_status: if checked {
                VersionStatus::Tested
            } else {
                VersionStatus::Untested
            },
        }
    });
    let outcome = match outcome {
        Err(
            ref failure @ RouteFailure {
                cause: RouteError::InvalidParam { field, .. },
                ..
            },
        ) => Err(AdapterError::Rejected {
            reason: StartRejected::InvalidParam { field },
            evidence: TurnEvidence::of_failure(failure),
        }),
        Err(
            ref failure @ RouteFailure {
                cause: RouteError::ResumeMismatch { .. },
                ..
            },
        ) => Err(AdapterError::ResumeMismatch {
            evidence: TurnEvidence::of_failure(failure),
        }),
        Err(failure) => Err(AdapterError::Route(failure)),
        Ok(result) => {
            let exit =
                (result.exit.code.is_some() || result.exit.signal.is_some()).then_some(result.exit);
            let cause = match rest {
                Rest::Delivered => None,
                Rest::Undelivered => Some(RouteError::Overflow { turn: number }),
                Rest::Forced => Some(RouteError::ForceStopped { turn: number }),
            };
            match cause {
                None => Ok(TurnEvidence {
                    exit,
                    cleanup: cleanup(result.cleanup),
                    journal_uncertain: result.journal_uncertain,
                }),
                Some(cause) => Err(AdapterError::Route(RouteFailure {
                    cause,
                    undecoded: None,
                    exit,
                    launched: true,
                    cleanup: Some(result.cleanup),
                    forced: result.forced,
                    journal_uncertain: result.journal_uncertain,
                    acknowledged,
                    shared: persistent,
                })),
            }
        }
    };
    TurnEnd {
        terminal: terminal.map(vendor_terminal),
        instance,
        leftovers: None,
        outcome,
    }
}

/// Maps the decoded terminal to C2's (AD5, AD6, AD11): the vendor's stop
/// reason is kept verbatim beside its normalized one.
fn vendor_terminal(terminal: FakeTerminal) -> VendorTerminal {
    let details = *terminal.details;
    VendorTerminal {
        at: terminal.at,
        status: match terminal.status {
            TerminalStatus::Completed => VendorTerminalStatus::Completed,
            TerminalStatus::Interrupted => VendorTerminalStatus::Interrupted,
            TerminalStatus::Failed => VendorTerminalStatus::Failed,
        },
        stop_reason: stop_reason(&terminal.stop_reason),
        vendor_stop_reason: terminal.stop_reason,
        vendor_code: terminal.vendor_code,
        class_hint: details.class_hint.map(class_hint),
        detail: details.detail,
        structured_output: details.structured_output,
        steps: details.steps,
        usage: details.usage.map(usage),
        cost: details.cost.map(|cost| CostReport {
            usd: cost.usd,
            scope: cost.scope,
        }),
        vendor: details.vendor,
    }
}

/// The fake's stop reasons; any other is `Other`.
fn stop_reason(vendor: &str) -> StopReason {
    match vendor {
        "end_turn" => StopReason::EndTurn,
        "max_steps" => StopReason::MaxSteps,
        "budget" => StopReason::Budget,
        "refusal" => StopReason::Refusal,
        "interrupted" | "cancelled" => StopReason::Interrupted,
        "error" => StopReason::Error,
        _ => StopReason::Other,
    }
}

fn class_hint(hint: FakeClassHint) -> ClassHint {
    match hint {
        FakeClassHint::Auth => ClassHint::Auth,
        FakeClassHint::RateLimit => ClassHint::RateLimit,
        FakeClassHint::ContextExceeded => ClassHint::ContextExceeded,
        FakeClassHint::BudgetExceeded => ClassHint::BudgetExceeded,
        FakeClassHint::VendorError => ClassHint::VendorError,
        FakeClassHint::Protocol => ClassHint::Protocol,
        FakeClassHint::ResumeMismatch => ClassHint::ResumeMismatch,
    }
}

fn usage(sample: FakeUsage) -> UsageSample {
    UsageSample {
        key: sample.key,
        input: sample.input,
        cached_input: sample.cached_input,
        output: sample.output,
        reasoning_output: sample.reasoning_output,
        total: sample.total,
    }
}

/// A per-turn refusal as the definite rejection it is before submission.
fn start_rejected(refusal: Refusal) -> StartRejected {
    match refusal.kind {
        RefusalKind::BoundUnsupported => StartRejected::BoundUnsupported(refusal.message),
        RefusalKind::InvalidParam { field } => StartRejected::InvalidParam { field },
        RefusalKind::VendorOptionConflict => StartRejected::InvalidParam { field: "vendor" },
        RefusalKind::UnsupportedVerb
        | RefusalKind::HarnessUnavailable
        | RefusalKind::UnknownModel
        | RefusalKind::VersionRefused
        | RefusalKind::MissingCapability { .. } => StartRejected::Protocol(refusal.message),
    }
}

/// Turns decoded messages into observations (C2 §4).
struct Normalizer {
    /// The connection generation, named in identity confirmations.
    generation: u64,
    /// The driver's state, which keeps the confirmed identity.
    state: Arc<Mutex<DriverState>>,
    /// How a steer delivery is reported.
    steer: SteerDelivery,
    /// The vendor closed its session in this turn.
    vendor_closed: bool,
}

impl Normalizer {
    /// One message's observations: at most one, or the terminal's final
    /// text pieces. Unknown messages, the handshake and the interrupt
    /// acknowledgement move only the activity clock.
    fn items(&mut self, message: RouteMessage, at: tokio::time::Instant) -> Vec<ObservationItem> {
        // Route pairs every vendor turn ID with `fake-turn-N`, never empty:
        // the conversion cannot fail.
        let item = |vendor_turn: Option<String>, observation| ObservationItem {
            at,
            vendor_turn: vendor_turn.and_then(|id| VendorTurnId::try_from(id).ok()),
            observation,
        };
        // The terminal itself is retained in the turn's end (AD4); its
        // final text goes as pieces.
        if let FakeMessage::Terminal {
            vendor_turn_id,
            final_text,
            ..
        } = message.payload
        {
            return final_text_pieces(&final_text)
                .map(|piece| {
                    item(
                        Some(vendor_turn_id.clone()),
                        Observation::FinalText(piece.to_owned()),
                    )
                })
                .collect();
        }
        self.observation(message.payload)
            .map(|(vendor_turn, observation)| item(vendor_turn, observation))
            .into_iter()
            .collect()
    }

    /// A non-terminal message's observation and vendor turn, if any.
    fn observation(&mut self, payload: FakeMessage) -> Option<(Option<String>, Observation)> {
        let marks = |vendor_turn, marks| Some((Some(vendor_turn), Observation::Progress(marks)));
        match payload {
            FakeMessage::Accepted { vendor_turn_id } => {
                let accepted = Acceptance {
                    // Route admits exactly one acceptance per turn.
                    correlation: AcceptanceToken::FIRST,
                    // Paired with `fake-turn-N`: never empty.
                    vendor_turn_id: VendorTurnId::try_from(vendor_turn_id.clone()).ok(),
                };
                Some((Some(vendor_turn_id), Observation::Accepted(accepted)))
            }
            FakeMessage::Text { vendor_turn_id } => marks(
                vendor_turn_id,
                ProgressMarks {
                    model: true,
                    ..ProgressMarks::default()
                },
            ),
            FakeMessage::ToolStarted {
                vendor_turn_id,
                tool_id,
                name,
            } => marks(
                vendor_turn_id,
                ProgressMarks {
                    tools_started: vec![(tool_id, name)],
                    ..ProgressMarks::default()
                },
            ),
            FakeMessage::ToolEnded {
                vendor_turn_id,
                tool_id,
            } => marks(
                vendor_turn_id,
                ProgressMarks {
                    tools_ended: vec![tool_id],
                    ..ProgressMarks::default()
                },
            ),
            FakeMessage::Usage {
                vendor_turn_id,
                sample,
                ..
            } => marks(
                vendor_turn_id,
                ProgressMarks {
                    usage: Some(usage(sample)),
                    ..ProgressMarks::default()
                },
            ),
            FakeMessage::Identity {
                vendor_session_id,
                transcript,
            } => Some((None, self.identity(vendor_session_id, transcript))),
            FakeMessage::Denial {
                vendor_turn_id,
                kind,
                target,
                reason,
            } => Some((
                Some(vendor_turn_id),
                Observation::ActionDenied(Denial {
                    kind: denial_kind(kind),
                    target,
                    reason,
                }),
            )),
            FakeMessage::Decline {
                vendor_turn_id,
                vendor_method,
                summary,
                blocking,
            } => Some((
                Some(vendor_turn_id),
                Observation::RequestDeclined(Decline {
                    vendor_method,
                    summary,
                    blocking,
                }),
            )),
            FakeMessage::SteerDelivered { vendor_turn_id } => Some((
                Some(vendor_turn_id),
                Observation::SteerDelivered(self.steer.clone()),
            )),
            FakeMessage::VendorClosed { reason } => {
                self.vendor_closed = true;
                Some((None, Observation::VendorClosed(reason)))
            }
            FakeMessage::Terminal { .. }
            | FakeMessage::Hello(_)
            | FakeMessage::InterruptAck { .. }
            | FakeMessage::Unknown { .. } => None,
        }
    }

    /// C2 §2 "Reopen": Route fails a turn on an identity that differs from
    /// the session's before its terminal, and drops one after it
    /// (`resume_mismatch`), so every one handed over confirms it for this
    /// connection generation, and the session keeps it.
    fn identity(&self, vendor_session_id: String, transcript: Option<String>) -> Observation {
        lock(&self.state).identity = Some(vendor_session_id.clone());
        Observation::IdentityConfirmed(Identity {
            vendor_session_id,
            connection_id: connection_id(self.generation),
            transcript: transcript.map(PathBuf::from),
            // The fake's handshake carries none at confirmation.
            vendor_version: None,
        })
    }
}

fn denial_kind(kind: FakeDenialKind) -> DenialKind {
    match kind {
        FakeDenialKind::FileWrite => DenialKind::FileWrite,
        FakeDenialKind::Command => DenialKind::Command,
        FakeDenialKind::Network => DenialKind::Network,
        FakeDenialKind::Other => DenialKind::Other,
    }
}

/// Sends each item in order.
async fn send_all(
    items: Vec<ObservationItem>,
    sink: ObservationSink,
    stall: Duration,
) -> Result<(), Undelivered> {
    for item in items {
        sink.send(item, stall).await?;
    }
    Ok(())
}

async fn poll_delivery(delivery: Option<&mut Delivery>) -> Result<(), Undelivered> {
    match delivery {
        Some(delivery) => delivery.await,
        None => std::future::pending().await,
    }
}

async fn recv(receiver: Option<&mut mpsc::Receiver<RouteMessage>>) -> Option<RouteMessage> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => None,
    }
}

/// Resolves once `force` is set; never when its sender is gone unset.
async fn forced(force: &mut watch::Receiver<Option<tokio::time::Instant>>) {
    if force.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(all(test, feature = "test-failpoints"))]
mod tests {
    use super::{Retirement, RetirementFault, WireCleanup, retirement_uncertain};

    fn retirement(cleanup: WireCleanup) -> Retirement {
        Retirement {
            launched: true,
            exit: None,
            cleanup: Some(cleanup),
            forced: false,
            journal_uncertain: false,
        }
    }

    /// Sol r3 N10: the nth-retirement fault adds its injected uncertainty
    /// to the real one and never hides it, and each daemon's adapter
    /// counts its own launched retirements.
    #[test]
    fn the_retirement_fault_adds_to_real_uncertainty_per_adapter() {
        let fault = RetirementFault::new(2);
        assert!(
            retirement_uncertain(&retirement(WireCleanup::Uncertain), Some(&fault)),
            "a real uncertainty the fault does not inject stays"
        );
        assert!(
            retirement_uncertain(&retirement(WireCleanup::Quiescent), Some(&fault)),
            "the second launched retirement is injected"
        );
        assert!(!retirement_uncertain(
            &retirement(WireCleanup::Quiescent),
            Some(&fault)
        ));
        let other = RetirementFault::new(1);
        assert!(
            retirement_uncertain(&retirement(WireCleanup::Quiescent), Some(&other)),
            "another daemon's adapter counts from its own first"
        );
        assert!(!retirement_uncertain(
            &retirement(WireCleanup::Quiescent),
            None
        ));
    }
}
