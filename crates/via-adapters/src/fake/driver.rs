//! The fake's driver turn (C2 §2, §4, §4.1; adapter design AD3–AD9):
//! launches the turn's process through the C2 route lane on a task the
//! session's tracker owns, normalizes each decoded message into session
//! observations in decode order, and builds the turn's one `TurnEnd`.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::{FakeAdapter, FakeProfile};
use crate::driver::turn::{
    Abandonment, CLEANUP_ALLOWANCE, Normalize, Rest, deliver_beside, earliest, end_active,
    merge_stops, ordered,
};
use crate::driver::{
    Active, Delivering, DriverState, ForceWatch, Prepared, Reservation, Retiring, SessionDriver,
    SteerEmissions, SteerTurn, TurnCx, TurnSpec, latch, lock, rejected,
};
use crate::harness::Harness;
use crate::observation::{
    Acceptance, AdapterError, ClassHint, CostReport, Decline, Denial, DenialKind, Identity,
    InstanceReport, Observation, ObservationItem, ObservationSink, ProgressMarks, SteerDelivery,
    SteerToken, StopReason, TurnEnd, TurnEvidence, Undelivered, UsageSample, VendorTerminal,
};
use crate::plan::{ParamSizes, Refusal, RefusalKind, TurnParams, VersionStatus};
use crate::runtime::{cleanup, event_stall};
use crate::{
    AcceptanceToken, Deadline, DriverFailure, DriverHealth, PrivateProcessSpec, ProcessOwner,
    RouteError, RouteFailure, StartRejected, StopOrder, StopWatch, VendorTerminalStatus,
    VendorTurnId, final_text_pieces,
};
use via_routes::{
    FakeClassHint, FakeDenialKind, FakeMessage, FakeRetired, FakeRetiredItem, FakeRoute,
    FakeTerminal, FakeTurn, FakeUsage, Handshake, Lane, Retirement, RouteMessage, StopSources,
    TerminalStatus, TurnStart, WireCleanup,
};

/// How often the idle source looks for its scenario gate.
const IDLE_POLL: Duration = Duration::from_millis(10);

/// The turn's reservation, shared by its task and `run_turn`.
type Shared = Arc<Mutex<Reservation>>;

/// Locks the shared reservation; no code panics while holding it.
fn held(reservation: &Shared) -> MutexGuard<'_, Reservation> {
    reservation.lock().unwrap_or_else(PoisonError::into_inner)
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
    let ordered = ordered((stop.clone(), force.clone(), wall), driver.cancel.clone());
    let connected = driver.connect((prepared, capacity), ordered).await;
    let (generation, capacity, reservation, delivering) = match connected {
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
    let (steer, steer_lane) = FakeRoute::steer_lane();
    let (close, close_rx) = watch::channel(None);
    let (done, retiring) = watch::channel(Retiring::Running);
    let active = Active::new(turn, steer, close);
    let steers = Arc::clone(&active.emissions);
    // Critical r2 #1: the turn's end, by any path, answers every steer
    // still waiting on it.
    let _steer_turn = SteerTurn(Arc::clone(&steers));
    let identity = {
        let mut state = driver.state();
        state.active = Some(active);
        state.retiring = Some(retiring);
        state.identity.clone()
    };
    // Route hands one message at a time: while it is full Route reads no
    // further message.
    let (hop, hop_rx) = mpsc::channel::<via_routes::Decoded<RouteMessage>>(1);
    let lane = Lane {
        persistent,
        handshake: profile.handshake.as_ref().map(|decl| decl.requires.clone()),
        tool_grace,
        steer: Some(steer_lane),
        identity,
        effort: spec.effort,
        retired: None,
    };
    let ((logical, logical_rx), (fence, fence_rx)) = (oneshot::channel(), oneshot::channel());
    driver.tracker.spawn(turn_task(TurnTask {
        route: FakeRoute::new(Arc::clone(&driver.runtime)),
        process,
        start,
        hop: via_routes::Hop::new(hop, activity.decode_watermark()),
        signals: (wall, force.clone(), stop),
        close: close_rx,
        cancel: driver.cancel.clone(),
        lane,
        logical,
        reservation: Arc::clone(&reservation),
        state: Arc::clone(&driver.state),
        reports: (Arc::clone(&driver.health), Arc::clone(&driver.journal)),
        observations: driver.observations.clone(),
        retiring: (done, fence_rx),
    }));
    let mut normalizer = Normalizer::new(generation, Arc::clone(&driver.state), profile, steers);
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
        (turn, generation, delivering, fence),
        &end,
        normalizer.vendor_closed,
    );
    end
}

/// The generation's delivery fence (C2 §2, §4 generation barrier) as the
/// retirement takes it: the barrier's acquisition, already queued.
type Fence = std::pin::Pin<Box<dyn Future<Output = OwnedMutexGuard<()>> + Send>>;

/// Hands the generation's delivery fence to the turn's retirement once the
/// turn's own delivery ended (critical fix r1 #2, #3): what the retired
/// helper reports follows the turn's observations, and the retirement holds
/// the fence until its delivery ends, so an idle close and a later
/// generation follow it too. The acquisition is queued here, before the
/// turn returns and its idle close may start, behind whatever holds the
/// barrier now; the turn itself never waits on it. That first poll runs
/// outside the task's cooperative budget, which, spent, would return before
/// queueing (critical fix r2 #1); the retirement's wait stays cooperative.
fn hand_fence(barrier: &Arc<tokio::sync::Mutex<()>>, fence: oneshot::Sender<Fence>) {
    let mut taking: Fence = Box::pin(Arc::clone(barrier).lock_owned());
    let mut queued = std::task::Context::from_waker(std::task::Waker::noop());
    let first = std::pin::pin!(tokio::task::unconstrained(taking.as_mut()));
    let taking: Fence = match first.poll(&mut queued) {
        std::task::Poll::Ready(guard) => Box::pin(std::future::ready(guard)),
        std::task::Poll::Pending => taking,
    };
    // A retirement that already ended takes nothing: the fence is freed.
    let _unheld = fence.send(taking);
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
        // The fake has no size limit: it never reads them.
        sizes: ParamSizes::default(),
        inherit: None,
        instructions: false,
        model: None,
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
    let owner = ProcessOwner::Turn {
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

/// The persistent profile after a turn returned: its retirement takes the
/// generation's delivery fence ([`hand_fence`]), a vendor close in the
/// turn ends its connection, and the scenario's idle close starts now.
/// Nothing on the per-turn profile. The turn's last delivery is done: it
/// no longer holds off an idle close (`delivering`, C2 D4).
fn after_persistent_turn(
    driver: &SessionDriver,
    adapter: &FakeAdapter,
    (turn, generation, delivering, fence): (
        crate::TurnNumber,
        u64,
        Delivering,
        oneshot::Sender<Fence>,
    ),
    end: &TurnEnd,
    vendor_closed: bool,
) {
    if !adapter.profile().persistent {
        return;
    }
    hand_fence(&driver.barrier, fence);
    drop(delivering);
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
    route: FakeRoute,
    process: PrivateProcessSpec,
    start: TurnStart,
    hop: via_routes::Hop<RouteMessage>,
    signals: (Deadline, ForceWatch, StopWatch),
    close: watch::Receiver<Option<StopOrder>>,
    cancel: CancellationToken,
    lane: Lane,
    logical: oneshot::Sender<FakeTurn>,
    reservation: Shared,
    state: Arc<Mutex<DriverState>>,
    /// The driver's health lane and its journal report.
    reports: (Arc<watch::Sender<DriverHealth>>, Arc<watch::Sender<bool>>),
    /// The session channel a retired helper's observations go on.
    observations: ObservationSink,
    /// How far the retirement is, and the generation's delivery fence once
    /// the turn's own delivery ended ([`hand_fence`]); the fence's sender
    /// dropped unsent, the turn abandoned it.
    retiring: (watch::Sender<Retiring>, oneshot::Receiver<Fence>),
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
        mut lane,
        logical,
        reservation,
        state,
        reports: (health, journal),
        observations,
        retiring: (done, fence),
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
    // C2 §4.1: what a persistent helper reports as it retires, one item
    // at a time, as the turn's own hop.
    let (items, items_rx) = mpsc::channel(1);
    let (failure, failure_rx) = oneshot::channel();
    let (cleaned, cleaned_rx) = oneshot::channel();
    lane.retired = persistent.then_some(FakeRetired {
        items,
        failure,
        cleaned,
    });
    let (inner, inner_rx) = oneshot::channel();
    let stop = (merged_rx, sources);
    let route_turn = route.turn(process, start, hop, (wall, force, stop), lane, inner);
    // Its own shares: the routed turn ends before the retirement settles.
    let relay = {
        let (health, state) = (Arc::clone(&health), Arc::clone(&state));
        let reservation = Arc::clone(&reservation);
        async move {
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
        }
    };
    let routed = async {
        tokio::select! {
            (retirement, ()) = async { tokio::join!(route_turn, relay) } => retirement,
            never = merge_stops(core_stop, close, &cancel, &merged) => match never {},
        }
    };
    let delivery = deliver_retired((items_rx, fence), &observations, &health);
    // Boxed: Route's turn is a large future.
    let routed = Box::pin(routed);
    let retiring = (routed, cleaned_rx, failure_rx);
    retire_beside(retiring, (delivery, &health), |retirement| {
        let uncertain = persistent
            && retirement_uncertain(
                &retirement,
                #[cfg(feature = "test-failpoints")]
                lock(&state).retirement_fault.as_deref(),
            );
        let reports = RetiredReports {
            health: &health,
            journal: &journal,
        };
        settle_retired((retirement, uncertain), reports);
        held(&reservation).retired(retirement);
        // An uncommitted slot is released with the last share, after the
        // cleanup and after `run_turn`.
        drop(reservation);
        done.send_replace(Retiring::CleanedUp);
    })
    .await;
    done.send_replace(Retiring::Delivered);
    // Test builds: the retirement's delivery ended.
    #[cfg(feature = "test-failpoints")]
    let _ = via_routes::failpoint::hit_async("adapter.fake.retirement_delivered").await;
}

/// Runs the turn through Route (`routed`) while delivering what its
/// retiring helper reports (`delivery`), so neither waits for the other.
/// `settle` runs once the process is retired: when Route reports its
/// close ended (`cleaned`), whatever its reading and the delivery are
/// doing, or else when Route's turn returns. A failure of the helper's
/// output fails the connection (`health`) as soon as its reading reports
/// it (`failure`), whatever the close is doing. A retirement's cleanup and
/// reports wait for no observation (critical r1 #4, fix r1 #1, fix r2 #1,
/// fix r3 #3, fix r4 #1). Returns once Route's turn returned, the
/// delivery ended and both reports resolved (critical fix r1 #1); the
/// retirement settles exactly once.
async fn retire_beside<F>(
    (routed, mut cleaned, mut failure): (
        F,
        oneshot::Receiver<Retirement>,
        oneshot::Receiver<RouteError>,
    ),
    (delivery, health): (impl Future<Output = ()>, &watch::Sender<DriverHealth>),
    settle: impl FnOnce(Retirement),
) where
    F: Future<Output = Retirement> + Unpin,
{
    let mut delivery = std::pin::pin!(delivery);
    // Dropped once it returned, with what it holds.
    let mut routed = Some(routed);
    let mut settle = Some(settle);
    let (mut delivered, mut reported, mut failed) = (false, false, false);
    // Each report is read until it resolves or its sender is gone, which
    // Route's return guarantees: none is lost to the others ending first.
    while routed.is_some() || !delivered || !reported || !failed {
        let ended = tokio::select! {
            biased;
            facts = &mut cleaned, if !reported => {
                reported = true;
                facts.ok()
            }
            cause = &mut failure, if !failed => {
                failed = true;
                if let Ok(cause) = cause {
                    latch(health, DriverFailure::Route(cause));
                }
                None
            }
            retirement = poll_routed(routed.as_mut()), if routed.is_some() => {
                routed = None;
                Some(retirement)
            }
            () = &mut delivery, if !delivered => {
                delivered = true;
                None
            }
        };
        if let Some(retirement) = ended
            && let Some(settle) = settle.take()
        {
            settle(retirement);
            // Test builds: the retirement's cleanup and reports are out.
            #[cfg(feature = "test-failpoints")]
            let _ = via_routes::failpoint::hit_async("adapter.fake.retirement_cleaned").await;
        }
    }
}

/// Awaits Route's turn, if it still runs; never ends without one.
async fn poll_routed<F: Future<Output = Retirement> + Unpin>(routed: Option<&mut F>) -> Retirement {
    match routed {
        Some(routed) => routed.await,
        None => std::future::pending().await,
    }
}

/// Where a persistent connection's retirement reports (C2 §2 health,
/// runtime §7).
#[derive(Clone, Copy)]
struct RetiredReports<'a> {
    health: &'a watch::Sender<DriverHealth>,
    journal: &'a watch::Sender<bool>,
}

/// Publishes a persistent connection's retirement reports at once: its
/// journal's uncertainty and its unproven cleanup (`uncertain`) each fail
/// the connection, apart from any observation (critical r1 #4).
fn settle_retired((retirement, uncertain): (Retirement, bool), reports: RetiredReports<'_>) {
    // A persistent connection's retirement journal is no turn's: its
    // uncertainty is reported apart from the cleanup's, before the health
    // failure that retires the lane (critical r1 #4).
    if retirement.launched && retirement.journal_uncertain {
        reports.journal.send_replace(true);
    }
    if uncertain {
        latch(reports.health, DriverFailure::RetirementUncertain);
    }
}

/// Sends what a retired helper reports on the session channel as Route
/// forwards it, in decode order (C2 §4.1): its durable observations
/// normalized as the turn's own are, and a late terminal (§4
/// `turn.late_terminal`), each charged to the session's budget. One the
/// channel does not take in time latches `overflow` and ends the
/// delivery; once the lane ended and its channel closed, the rest is
/// dropped (ruling G1). Either way Route then forwards nothing more.
/// Nothing is sent before the turn's own delivery ended (`fence`, critical
/// fix r1 #2, #3): the generation's delivery fence is then held until this
/// delivery ends; a turn that abandoned it leaves the delivery unfenced.
async fn deliver_retired(
    (mut items, fence): (mpsc::Receiver<FakeRetiredItem>, oneshot::Receiver<Fence>),
    sink: &ObservationSink,
    health: &watch::Sender<DriverHealth>,
) {
    let mut fence = Some(fence);
    // Held until the delivery ends.
    let mut _fenced = None;
    while let Some(retired) = items.recv().await {
        // Test builds: the delivery holds each retired item before its send.
        #[cfg(feature = "test-failpoints")]
        let _ = via_routes::failpoint::hit_async("adapter.fake.retirement_item").await;
        if let Some(fence) = fence.take()
            && let Ok(taking) = fence.await
        {
            _fenced = Some(taking.await);
        }
        // Normalized and stamped once the fence is taken: `at` never runs
        // back behind what the turn delivered meanwhile (C2 §4, critical
        // fix r2 #2).
        let (vendor_turn, observation) = match retired {
            FakeRetiredItem::Durable(message) => match durable(message.payload) {
                Some(durable) => durable,
                None => continue,
            },
            FakeRetiredItem::Terminal(late) => (
                late.vendor_turn_id,
                Observation::LateTerminal(vendor_terminal(late.terminal)),
            ),
        };
        let item = ObservationItem {
            at: tokio::time::Instant::now(),
            // Route pairs it with `fake-turn-N`, never empty.
            vendor_turn: VendorTurnId::try_from(vendor_turn).ok(),
            observation,
        };
        match sink.send(item, event_stall()).await {
            Ok(()) => {}
            Err(Undelivered::Stalled) => {
                latch(health, DriverFailure::ObservationOverflow);
                return;
            }
            Err(Undelivered::Closed) => return,
        }
    }
}

/// The persistent emulation's idle source (decision H1, C2 §4): once the
/// scenario's gate `release` exists, the emulated server closes its idle
/// session: the slot is released, the pin invalidated, and a session-level
/// `VendorClosed` sent; one the channel does not take latches `overflow`.
/// Started only after the turn before it returned; a later connection, a
/// turn holding the connection when it decides, or the session's
/// cancellation ends it. It holds the generation barrier
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
    // Test builds: `adapter.fake.idle_barrier_wait` acknowledges an idle
    // close that finds the barrier held.
    #[cfg(feature = "test-failpoints")]
    if barrier.try_lock().is_err() {
        let _ = via_routes::failpoint::hit_async("adapter.fake.idle_barrier_wait").await;
    }
    // Held until the close's item was delivered (C2 §4 generation barrier).
    let _barrier = tokio::select! {
        () = cancel.cancelled() => return,
        barrier = barrier.lock() => barrier,
    };
    let released = {
        let mut state = lock(&state);
        // Never during a turn (C2 D4): a vendor does not idle-close a
        // session it is serving, and the turn's observations stay in order.
        if state.generation != generation
            || !state.live
            || state.closed
            || state.delivering.is_some()
        {
            None
        } else {
            state.live = false;
            state.vendor_closed = true;
            Some(state.capacity.take())
        }
    };
    // Test builds: the idle close decided whether to close.
    #[cfg(feature = "test-failpoints")]
    let _ = via_routes::failpoint::hit_async("adapter.fake.idle_decided").await;
    let Some(released) = released else {
        return;
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
    let instance =
        handshake.map(|handshake| instance_report(&checked(adapter.profile()), handshake));
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

/// The profile's `checked` handshake versions (AD7); none without a
/// handshake.
fn checked(profile: &FakeProfile) -> Vec<String> {
    profile
        .handshake
        .as_ref()
        .map(|decl| decl.checked.clone())
        .unwrap_or_default()
}

/// The version a handshake reported (AD7): `tested` when the profile's
/// `checked` versions hold it, else `untested`.
fn instance_report(checked: &[String], handshake: Handshake) -> InstanceReport {
    let checked = handshake
        .vendor_version
        .as_ref()
        .is_some_and(|version| checked.contains(version));
    InstanceReport {
        vendor_version: handshake.vendor_version,
        version_status: if checked {
            VersionStatus::Tested
        } else {
            VersionStatus::Untested
        },
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
        structured_output_unparsed: None,
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
    /// The profile's `checked` versions (AD7).
    checked: Vec<String>,
    /// The instance's handshake, once Route forwarded it (AD7): its
    /// acceptance carries it (C2 §4 `turn.accepted`).
    instance: Option<InstanceReport>,
    /// The turn's steer callers awaiting their observation's emission.
    steers: Arc<SteerEmissions>,
    /// The tokens of the `steer.delivered` items being delivered.
    emitting: Vec<u64>,
}

impl Normalize for Normalizer {
    type Message = RouteMessage;

    /// The delivery of the last message's items ended, `delivered` or
    /// not: each steer whose `steer.delivered` it carried learns whether
    /// that observation is on the session channel (C2 `SteerInput.token`).
    fn emitted(&mut self, delivered: bool) {
        for token in self.emitting.drain(..) {
            self.steers.answer(token, delivered);
        }
    }

    /// One message's observations: at most one, or the terminal's final
    /// text pieces. Unknown messages, the handshake (kept for the
    /// acceptance) and the interrupt acknowledgement move only the activity
    /// clock.
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
        // A steer's delivery report carries the token Route paired it with.
        if let FakeMessage::SteerDelivered { vendor_turn_id } = &message.payload {
            let Some(token) = message.steer else {
                return Vec::new();
            };
            self.emitting.push(token);
            let observation = Observation::SteerDelivered {
                delivery: self.steer.clone(),
                token: SteerToken::new(token),
            };
            return vec![item(Some(vendor_turn_id.clone()), observation)];
        }
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
}

impl Normalizer {
    /// The normalizer of one turn on connection `generation`.
    fn new(
        generation: u64,
        state: Arc<Mutex<DriverState>>,
        profile: &FakeProfile,
        steers: Arc<SteerEmissions>,
    ) -> Self {
        Self {
            generation,
            state,
            steer: steer_delivery(profile),
            vendor_closed: false,
            checked: checked(profile),
            instance: None,
            steers,
            emitting: Vec::new(),
        }
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
                    instance: self.instance.clone(),
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
            payload @ (FakeMessage::Denial { .. } | FakeMessage::Decline { .. }) => {
                durable(payload).map(|(vendor_turn, observation)| (Some(vendor_turn), observation))
            }
            FakeMessage::VendorClosed { reason } => {
                self.vendor_closed = true;
                Some((None, Observation::VendorClosed(reason)))
            }
            FakeMessage::Hello(handshake) => {
                self.instance = Some(instance_report(&self.checked, handshake));
                None
            }
            // A steer's delivery report is taken with its token by `items`.
            FakeMessage::SteerDelivered { .. }
            | FakeMessage::Terminal { .. }
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

/// A durable message's observation and vendor turn (C2 §4.1): a denial or
/// a decline, whether the turn's own reader or a retirement read it.
fn durable(payload: FakeMessage) -> Option<(String, Observation)> {
    match payload {
        FakeMessage::Denial {
            vendor_turn_id,
            kind,
            target,
            reason,
        } => Some((
            vendor_turn_id,
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
            vendor_turn_id,
            Observation::RequestDeclined(Decline {
                vendor_method,
                summary,
                blocking,
            }),
        )),
        // Nothing else is durable.
        FakeMessage::Accepted { .. }
        | FakeMessage::Text { .. }
        | FakeMessage::Terminal { .. }
        | FakeMessage::ToolStarted { .. }
        | FakeMessage::ToolEnded { .. }
        | FakeMessage::Usage { .. }
        | FakeMessage::Hello(_)
        | FakeMessage::Identity { .. }
        | FakeMessage::SteerDelivered { .. }
        | FakeMessage::VendorClosed { .. }
        | FakeMessage::InterruptAck { .. }
        | FakeMessage::Unknown { .. } => None,
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

#[cfg(all(test, feature = "test-failpoints"))]
mod tests {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    use tokio::sync::watch;
    use via_routes::{FakeDenialKind, FakeMessage, FakeRetiredItem, RouteError, RouteMessage};

    use super::{
        RetiredReports, Retirement, RetirementFault, WireCleanup, deliver_retired, retire_beside,
        retirement_uncertain, settle_retired,
    };
    use crate::{
        DriverFailure, DriverHealth, OBSERVATION_BYTES, ObservationBudget, observation_channel_in,
    };

    /// Fix round 1 #1, round 2 #1 (C2 §2 health, runtime §7): a
    /// retirement's journal uncertainty and its unproven cleanup are
    /// published at once, and the retirement is recorded, while the
    /// helper's observations still wait on a saturated session channel. (A
    /// failure of its output is reported as soon as its reading reports it:
    /// `a_retirement_failure_is_reported_before_its_cleanup`.)
    #[test]
    fn a_saturated_channel_delays_no_retirement_report() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            for uncertain in [false, true] {
                let budget = ObservationBudget::new();
                let _saturated = budget
                    .charge(u32::try_from(OBSERVATION_BYTES).unwrap())
                    .unwrap();
                let (sink, _receiver) = observation_channel_in(&budget);
                let (health, _health) = watch::channel(DriverHealth::Open);
                let (journal, _journal) = watch::channel(false);
                let mut retirement = retirement(WireCleanup::Quiescent);
                retirement.journal_uncertain = uncertain;
                let denial = FakeMessage::Denial {
                    vendor_turn_id: "fake-turn-1".to_owned(),
                    kind: FakeDenialKind::Command,
                    target: "t".to_owned(),
                    reason: "r".to_owned(),
                };
                let (items, items_rx) = tokio::sync::mpsc::channel(1);
                items
                    .try_send(FakeRetiredItem::Durable(RouteMessage {
                        payload: denial,
                        steer: None,
                    }))
                    .unwrap();
                drop(items);
                let (_cleaned, cleaned_rx) = tokio::sync::oneshot::channel();
                let (_failure, failure_rx) = tokio::sync::oneshot::channel();
                let released = AtomicBool::new(false);
                let delivery = deliver_retired((items_rx, unfenced()), &sink, &health);
                let routed = Box::pin(async move { retirement });
                let retiring = (routed, cleaned_rx, failure_rx);
                let settle = retire_beside(retiring, (delivery, &health), |retirement| {
                    let reports = RetiredReports {
                        health: &health,
                        journal: &journal,
                    };
                    settle_retired((retirement, true), reports);
                    released.store(true, Ordering::Release);
                });
                let mut settle = std::pin::pin!(settle);
                let waited = tokio::time::timeout(Duration::from_millis(50), &mut settle).await;
                assert!(waited.is_err(), "the observation waits for the channel");
                assert_eq!(
                    *health.borrow(),
                    DriverHealth::Failed {
                        first_cause: DriverFailure::RetirementUncertain
                    }
                );
                assert_eq!(*journal.borrow(), uncertain);
                assert!(released.load(Ordering::Acquire));
            }
        });
    }

    /// Fix round 3 #3 (C2 §2 health, runtime §7): the retirement's
    /// cleanup and its reports are published once its physical close
    /// ended, while its reader is still blocked handing a message to a
    /// delivery that waits on a saturated session channel.
    #[test]
    fn a_blocked_retirement_reader_delays_no_cleanup() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let budget = ObservationBudget::new();
            let _saturated = budget
                .charge(u32::try_from(OBSERVATION_BYTES).unwrap())
                .unwrap();
            let (sink, _receiver) = observation_channel_in(&budget);
            let (health, _health) = watch::channel(DriverHealth::Open);
            let (journal, _journal) = watch::channel(false);
            let mut retirement = retirement(WireCleanup::Quiescent);
            retirement.journal_uncertain = true;
            let denial = || {
                FakeRetiredItem::Durable(RouteMessage {
                    payload: FakeMessage::Denial {
                        vendor_turn_id: "fake-turn-1".to_owned(),
                        kind: FakeDenialKind::Command,
                        target: "t".to_owned(),
                        reason: "r".to_owned(),
                    },
                    steer: None,
                })
            };
            let (items, items_rx) = tokio::sync::mpsc::channel(1);
            let read = std::sync::Arc::new(AtomicBool::new(false));
            let reading = std::sync::Arc::clone(&read);
            // The reader: the delivery takes the first message and waits on
            // the channel, the hand-over holds the second, and the third
            // waits for room.
            let reader = async move {
                for _ in 0..3 {
                    items.send(denial()).await.unwrap();
                }
                reading.store(true, Ordering::Release);
            };
            let routed = Box::pin(async move {
                reader.await;
                retirement
            });
            // Route's close ended: its facts come apart from the reading.
            let (cleaned, cleaned_rx) = tokio::sync::oneshot::channel();
            cleaned.send(retirement).unwrap();
            let (_failure, failure_rx) = tokio::sync::oneshot::channel();
            let released = AtomicBool::new(false);
            let delivery = deliver_retired((items_rx, unfenced()), &sink, &health);
            let retiring = (routed, cleaned_rx, failure_rx);
            let settle = retire_beside(retiring, (delivery, &health), |retirement| {
                let reports = RetiredReports {
                    health: &health,
                    journal: &journal,
                };
                settle_retired((retirement, true), reports);
                released.store(true, Ordering::Release);
            });
            let mut settle = std::pin::pin!(settle);
            let waited = tokio::time::timeout(Duration::from_millis(50), &mut settle).await;
            assert!(waited.is_err(), "the delivery waits for the channel");
            assert!(!read.load(Ordering::Acquire), "the reader is blocked");
            assert_eq!(
                *health.borrow(),
                DriverHealth::Failed {
                    first_cause: DriverFailure::RetirementUncertain
                }
            );
            assert!(*journal.borrow());
            assert!(released.load(Ordering::Acquire));
        });
    }

    /// Fix round 4 #1 (C2 §2 health, runtime §7): a failure of the
    /// retiring helper's output fails the connection as soon as its reading
    /// reports it, while Route's turn and its physical close still run.
    #[test]
    fn a_retirement_failure_is_reported_before_its_cleanup() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let budget = ObservationBudget::new();
            let (sink, _receiver) = observation_channel_in(&budget);
            let (health, _health) = watch::channel(DriverHealth::Open);
            let (items, items_rx) = tokio::sync::mpsc::channel(1);
            // Route's turn and its close still run.
            let routed = Box::pin(std::future::pending::<Retirement>());
            let (_cleaned, cleaned_rx) = tokio::sync::oneshot::channel();
            let (failure, failure_rx) = tokio::sync::oneshot::channel();
            let cause = RouteError::Protocol {
                turn: crate::TurnNumber::try_from(1).unwrap(),
                detail: "undecodable vendor message",
            };
            // The reading ended on a message it kept undecoded.
            failure.send(cause.clone()).unwrap();
            drop(items);
            let released = AtomicBool::new(false);
            let delivery = deliver_retired((items_rx, unfenced()), &sink, &health);
            let retiring = (routed, cleaned_rx, failure_rx);
            let settle = retire_beside(retiring, (delivery, &health), |_| {
                released.store(true, Ordering::Release);
            });
            let waited = tokio::time::timeout(Duration::from_millis(50), settle).await;
            assert!(waited.is_err(), "Route's turn still runs");
            assert!(!released.load(Ordering::Acquire), "the cleanup is pending");
            assert_eq!(
                *health.borrow(),
                DriverHealth::Failed {
                    first_cause: DriverFailure::Route(cause)
                }
            );
        });
    }

    /// Critical fix r1 #1 (C2 §2 health): a failure Route publishes in the
    /// same poll that returns its turn, once the delivery already ended, is
    /// still read and fails the connection; the retirement settles once.
    #[test]
    fn a_failure_published_as_route_returns_is_not_lost() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (health, _health) = watch::channel(DriverHealth::Open);
            let (cleaned, cleaned_rx) = tokio::sync::oneshot::channel::<Retirement>();
            let (failure, failure_rx) = tokio::sync::oneshot::channel();
            let cause = RouteError::Protocol {
                turn: crate::TurnNumber::try_from(7).unwrap(),
                detail: "phase",
            };
            let reported = cause.clone();
            // Pending once, so the delivery ends first; then Route publishes
            // its failure, drops its lane and returns, in one poll.
            let routed = Box::pin(async move {
                tokio::task::yield_now().await;
                failure.send(reported).unwrap();
                drop(cleaned);
                retirement(WireCleanup::Quiescent)
            });
            let settled = std::sync::atomic::AtomicUsize::new(0);
            let retiring = (routed, cleaned_rx, failure_rx);
            retire_beside(retiring, (async {}, &health), |_| {
                settled.fetch_add(1, Ordering::AcqRel);
            })
            .await;
            assert_eq!(settled.load(Ordering::Acquire), 1);
            assert_eq!(
                *health.borrow(),
                DriverHealth::Failed {
                    first_cause: DriverFailure::Route(cause)
                }
            );
        });
    }

    /// Critical fix r2 #1 (C2 §4 generation barrier): the fence's place in
    /// the barrier's queue is taken when it is handed over, even with the
    /// task's cooperative budget spent; once the barrier's holder releases
    /// it, nothing else takes it before the retirement.
    #[test]
    fn a_spent_budget_keeps_the_fences_place() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let barrier = std::sync::Arc::new(tokio::sync::Mutex::new(()));
            let holder = std::sync::Arc::clone(&barrier).lock_owned().await;
            let (fence, fence_rx) = tokio::sync::oneshot::channel();
            // Spends the task's budget on ready receives, without yielding.
            let (items, mut received) = tokio::sync::mpsc::channel(1);
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            while tokio::task::coop::has_budget_remaining() {
                items.try_send(()).unwrap();
                let receive = std::pin::pin!(received.recv());
                let _ = std::future::Future::poll(receive, &mut cx);
            }
            super::hand_fence(&barrier, fence);
            drop(holder);
            assert!(barrier.try_lock().is_err(), "the fence was overtaken");
            let taking = fence_rx.await.unwrap();
            drop(taking.await);
            assert!(barrier.try_lock().is_ok());
        });
    }

    /// Critical fix r2 #2 (C2 §4: `at` never decreases): a retired item is
    /// stamped once its fence is taken, never earlier, so it follows what
    /// the turn delivered meanwhile.
    #[test]
    fn a_retired_item_is_stamped_after_its_fence() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let budget = ObservationBudget::new();
            let (sink, mut receiver) = observation_channel_in(&budget);
            let (health, _health) = watch::channel(DriverHealth::Open);
            let (items, items_rx) = tokio::sync::mpsc::channel(1);
            items
                .try_send(FakeRetiredItem::Durable(RouteMessage {
                    payload: FakeMessage::Denial {
                        vendor_turn_id: "fake-turn-1".to_owned(),
                        kind: FakeDenialKind::Command,
                        target: "t".to_owned(),
                        reason: "r".to_owned(),
                    },
                    steer: None,
                }))
                .unwrap();
            drop(items);
            let (fence, fence_rx) = tokio::sync::oneshot::channel::<super::Fence>();
            let barrier = std::sync::Arc::new(tokio::sync::Mutex::new(()));
            let guard = std::sync::Arc::clone(&barrier).lock_owned().await;
            let delivery = deliver_retired((items_rx, fence_rx), &sink, &health);
            let turn = async {
                // The delivery waits on its fence meanwhile.
                tokio::time::sleep(Duration::from_millis(20)).await;
                let delivered = tokio::time::Instant::now();
                let _sent = fence.send(Box::pin(std::future::ready(guard)));
                delivered
            };
            let ((), delivered) = tokio::join!(delivery, turn);
            let retired = receiver.recv().await.unwrap();
            assert!(retired.item.at >= delivered, "stamped before its fence");
        });
    }

    /// A fence its turn abandoned: the delivery goes unfenced.
    fn unfenced() -> tokio::sync::oneshot::Receiver<super::Fence> {
        tokio::sync::oneshot::channel().1
    }

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
