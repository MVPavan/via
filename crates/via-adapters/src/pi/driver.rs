//! Pi's driver turn (C2 §2, §4, §4.1; packet §§2–7): one private
//! `pi --mode rpc` process per VIA turn. The pre-launch checks run first,
//! with no vendor process: the profile policy (§4.3), R1's predecessor
//! check (§7.4) and the version read (§3). Then the turn runs through the
//! Pi route on a task the session's tracker owns, each handed-over item
//! normalized in decode order; Route decides every handshake check, and
//! the turn's end maps its cause.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::launch::{self, Continue, Recipe, TOOLS, expected_session_id, recipe_key};
use super::normalize::{self, LaunchFacts, Normalizer};
use super::{HARNESS, PiAdapter, plan, profile};
use crate::driver::turn::{
    Abandonment, CLEANUP_ALLOWANCE, Normalize, Rest, controls, deliver_beside, earliest,
    end_active, merge_stops, ordered, unaccounted,
};
use crate::driver::{
    Active, DriverState, ForceWatch, Reservation, Retiring, SessionDriver, TurnCx, TurnSpec, latch,
    lock, rejected,
};
use crate::harness::Harness;
use crate::instance::Incompatibility;
use crate::observation::{
    AdapterError, InstanceReport, Observation, ObservationItem, TurnEnd, TurnEvidence,
};
use crate::plan::{ParamSizes, Refusal, RefusalKind, TurnParams};
use crate::runtime::cleanup;
use crate::{
    AcceptanceToken, Deadline, DriverFailure, DriverHealth, ProcessOwner, RouteError, RouteFailure,
    StartRejected, StopOrder, StopWatch, StoreFailure, VendorTerminalStatus, encoded_text_len,
};
use via_routes::StopSources;
use via_routes::pi::{PiExpect, PiItem, PiRoute, PiRouteResult, PiStart, PiTurn};

/// Packet §2.1: the VIA-owned diagnostic of an exit before every
/// handshake reply arrived; the vendor's words stay in `stderr.log`.
const EXITED_EARLY: &str = "pi exited before its handshake replies";

/// Packet §5.4: a command rejection keeps no vendor text or code.
const PROMPT_REFUSED: &str = "pi refused the prompt before starting a run";

/// The ID identity confirmations name for connection `generation`.
pub(crate) fn connection_id(generation: u64) -> String {
    format!("pi-{generation}")
}

/// Runs one submitted turn (C2 §4.1): its values are checked first, then
/// the pre-launch checks (packet §2.1 step 1), then the turn's process
/// runs through the Pi route on a tracker-owned task while each
/// handed-over item is normalized and delivered to the session channel.
/// Dropping this future leaves the task, which owns the turn's cleanup,
/// running.
pub(crate) async fn run_turn(
    driver: &SessionDriver,
    adapter: &PiAdapter,
    spec: TurnSpec,
    cx: TurnCx,
) -> TurnEnd {
    if let Some(refused) = refused_values(driver, &spec) {
        return refused;
    }
    let turn = cx.turn;
    let signals = (cx.stop.clone(), cx.force.clone(), cx.wall);
    let staged = match stage(driver, adapter, (turn, signals)).await {
        Ok(staged) => staged,
        Err(end) => return *end,
    };
    let instance = instance_of(adapter, staged.version.as_deref());
    let route = PiRoute::new(Arc::clone(&driver.runtime));
    // Packet §7.4 (R1): one non-signalling pass within the wall, ended
    // unlaunched by a stop, the force or the session's cancellation first
    // (picrit #2: a stalled Store reply never holds the controls). The
    // wall stays the check's own: a read that outlives it is `store`.
    let resolved = tokio::select! {
        resolved = route.predecessors_resolved(&driver.spec.session_id, turn, cx.wall) => resolved,
        () = controls((cx.stop.clone(), cx.force.clone()), driver.cancel.clone()) => {
            Err(ordered_cause(turn, (&cx.stop, &cx.force, &driver.cancel)))
        }
    };
    let mut end = match resolved {
        Ok(true) => {
            launched(
                driver,
                adapter,
                (spec, cx),
                (route, instance.clone(), staged),
            )
            .await
        }
        Ok(false) => rejected(AdapterError::Rejected {
            reason: StartRejected::UncertainPredecessor,
            evidence: TurnEvidence::no_launch(false),
        }),
        Err(cause) => unlaunched(cause),
    };
    end.instance.get_or_insert(instance);
    end
}

/// What the pre-launch filesystem step read and wrote (packet §2.1 step
/// 1): the profile check's record, the version, and VIA's own launch
/// state (§4.4).
struct Staged {
    /// `pi-profile.json`: the record of the check the launch passed.
    profile: Vec<u8>,
    version: Option<String>,
    session_dir: PathBuf,
    instructions: Option<PathBuf>,
}

/// Why the pre-launch filesystem step launches nothing.
enum Unstaged {
    /// Packet §4.3: the profile policy's VIA-owned reason.
    Profile(String),
    /// Packet §4.4: a managed directory down to the agent directory is
    /// not private (VIA's text naming it), before the version read.
    Unsafe(String),
    /// Packet §4.4: a launch-state directory is not private, after the
    /// version read.
    UnsafeState {
        detail: String,
        version: Option<String>,
    },
    /// VIA's own launch state could not be written; the version was read
    /// (packet §3).
    State { version: Option<String> },
}

/// Packet §3: the version read, recorded for planning and reported in
/// this turn's `InstanceReport` on every outcome after the read.
fn instance_of(adapter: &PiAdapter, version: Option<&str>) -> InstanceReport {
    if let Some(version) = version {
        adapter
            .instances
            .record_version(HARNESS, &adapter.binary, version.to_owned());
    }
    InstanceReport {
        version_status: plan::version_status(version),
        vendor_version: version.map(str::to_owned),
    }
}

/// The pre-launch filesystem step (packet §§2.1, 3, 4.3, 4.4): the
/// managed directories down to the agent directory (never cached), the
/// profile policy (never cached), the version read and VIA's launch state,
/// in that order, the last under `staging` ([`PiAdapter`]'s lock) until it
/// returns. Blocking I/O, run off the async workers.
fn staged(
    (vendor_state_dir, staging): (&Path, &std::sync::Mutex<()>),
    binary: &Path,
    session: &crate::SessionId,
    instructions: Option<&str>,
) -> Result<Staged, Unstaged> {
    let agent = match launch::managed(vendor_state_dir, &["pi", "agent"]) {
        Ok(agent) => agent,
        Err(launch::Unsafe::Refused(detail)) => {
            return Err(Unstaged::Unsafe(detail));
        }
        Err(launch::Unsafe::Io) => {
            return Err(Unstaged::Profile(
                "the agent directory cannot be read".to_owned(),
            ));
        }
    };
    let profile = profile::check(&agent, profile::daemon_uid())
        .map_err(Unstaged::Profile)?
        .record;
    let version = launch::read_version(binary);
    // A panicked holder left no state a later write does not replace.
    let prepared = {
        let _serialized = staging
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        launch::prepare(vendor_state_dir, session, instructions)
    };
    let (session_dir, instructions) = match prepared {
        Ok(prepared) => prepared,
        Err(launch::Unsafe::Refused(detail)) => {
            return Err(Unstaged::UnsafeState { detail, version });
        }
        Err(launch::Unsafe::Io) => return Err(Unstaged::State { version }),
    };
    Ok(Staged {
        profile,
        version,
        session_dir,
        instructions,
    })
}

/// Runs [`staged`] on one blocking task the session's tracker owns, and
/// ends the turn unlaunched if its stop, force, wall or the session's
/// cancellation comes first, while the task waits for an earlier one's
/// write too (the task finishes on its own, under the staging lock;
/// nothing it wrote launches anything).
async fn stage(
    driver: &SessionDriver,
    adapter: &PiAdapter,
    (turn, signals): (crate::TurnNumber, (StopWatch, ForceWatch, Deadline)),
) -> Result<Staged, Box<TurnEnd>> {
    let task = {
        let vendor_state_dir = adapter.vendor_state_dir.clone();
        let staging = Arc::clone(&adapter.staging);
        let binary = adapter.binary.clone();
        let session = driver.spec.session_id.clone();
        let instructions = driver.spec.instructions.clone();
        driver.tracker.spawn_blocking(move || {
            staged(
                (&vendor_state_dir, &staging),
                &binary,
                &session,
                instructions.as_deref(),
            )
        })
    };
    let cause = {
        let (stop, force, _) = &signals;
        let (stop, force, cancel) = (stop.clone(), force.clone(), driver.cancel.clone());
        move || ordered_cause(turn, (&stop, &force, &cancel))
    };
    let joined = tokio::select! {
        joined = task => joined,
        () = ordered(signals, driver.cancel.clone()) => {
            return Err(Box::new(unlaunched(cause())));
        }
    };
    match joined {
        Ok(Ok(staged)) => Ok(staged),
        Ok(Err(Unstaged::Profile(detail))) => {
            Err(Box::new(unlaunched(RouteError::HandshakeRefused {
                turn,
                detail: Some(format!("the Pi profile policy refused: {detail}")),
            })))
        }
        Ok(Err(Unstaged::Unsafe(detail))) => {
            Err(Box::new(unlaunched(RouteError::HandshakeRefused {
                turn,
                detail: Some(detail),
            })))
        }
        Ok(Err(Unstaged::UnsafeState { detail, version })) => {
            let mut end = unlaunched(RouteError::HandshakeRefused {
                turn,
                detail: Some(detail),
            });
            end.instance = Some(instance_of(adapter, version.as_deref()));
            Err(Box::new(end))
        }
        Ok(Err(Unstaged::State { version })) => {
            let mut end = unlaunched(RouteError::Store {
                turn,
                kind: StoreFailure::Evidence,
            });
            end.instance = Some(instance_of(adapter, version.as_deref()));
            Err(Box::new(end))
        }
        Err(_panicked) => {
            driver.fail(DriverFailure::OwnedTask);
            Err(Box::new(rejected(AdapterError::TaskFailed)))
        }
    }
}

/// The cause of a turn ordered to end before its launch: the daemon
/// force, a stop (Core's, or the session's cancellation), else its wall.
fn ordered_cause(
    turn: crate::TurnNumber,
    (stop, force, cancel): (&StopWatch, &ForceWatch, &CancellationToken),
) -> RouteError {
    if force.borrow().is_some() {
        RouteError::ForceStopped { turn }
    } else if stop.borrow().is_some() || cancel.is_cancelled() {
        RouteError::Stopped { turn }
    } else {
        RouteError::Deadline { turn }
    }
}

/// A failure before any launch, with no-launch evidence.
fn unlaunched(cause: RouteError) -> TurnEnd {
    rejected(AdapterError::Route(RouteFailure {
        cause,
        undecoded: None,
        exit: None,
        launched: false,
        cleanup: None,
        forced: false,
        journal_uncertain: false,
        acknowledged: false,
        shared: false,
        launch: None,
    }))
}

/// The turn from its connection on.
async fn launched(
    driver: &SessionDriver,
    adapter: &PiAdapter,
    (spec, cx): (TurnSpec, TurnCx),
    (route, instance, staged): (PiRoute, InstanceReport, Staged),
) -> TurnEnd {
    let TurnCx {
        turn,
        prepared,
        capacity,
        activity,
        wall,
        tool_grace: _,
        stop,
        force,
        stop_ack,
    } = cx;
    // A private route returns at acknowledgement: its report goes unused
    // (C2 §2 Interrupt).
    drop(stop_ack);
    let ordered = ordered((stop.clone(), force.clone(), wall), driver.cancel.clone());
    let (generation, capacity, reservation, _delivering) =
        match driver.connect((prepared, capacity), ordered).await {
            Ok(connection) => connection,
            Err(error) => return rejected(error),
        };
    let Launch {
        mut process,
        expect,
        session,
        session_dir,
        recipe,
        clamp,
    } = launch(driver, adapter, &spec, (turn, &staged));
    process.capacity = capacity;
    let (steer, _unused) = via_routes::steer::steer_lane(str::len);
    let (close, close_rx) = watch::channel(None);
    let (done, retiring) = watch::channel(Retiring::Running);
    {
        let mut state = driver.state();
        state.active = Some(Active::new(turn, steer, close));
        state.retiring = Some(retiring);
    }
    // Route reads ahead up to its own bound; one item waits here.
    let (hop, hop_rx) = mpsc::channel::<via_routes::Decoded<PiItem>>(1);
    let hop = via_routes::Hop::new(hop, activity.decode_watermark());
    let (end, end_rx) = oneshot::channel();
    driver.tracker.spawn(turn_task(TurnTask {
        route,
        process,
        start: PiStart::new(turn, spec.prompt),
        hop,
        signals: (wall, force.clone(), stop.clone()),
        close: close_rx.clone(),
        cancel: driver.cancel.clone(),
        input: (expect, end),
        reservation,
        state: Arc::clone(&driver.state),
        health: Arc::clone(&driver.health),
        done,
    }));
    let mut delivery = Delivery {
        normalizer: Normalizer::new(LaunchFacts {
            expected_session: session,
            connection_id: connection_id(generation),
            correlation: AcceptanceToken::FIRST,
            session_dir,
            cwd: driver.spec.cwd.clone(),
            instance,
        }),
        state: &driver.state,
        health: &driver.health,
    };
    // The wall's cutoff, or an earlier `close_by` (packet §7.1).
    let cut = by_orders(wall, (stop.clone(), close_rx.clone()), |_| {});
    let mut abandonment = Abandonment(Some(&driver.health));
    let (routed, rest) = deliver_beside(
        // A closed channel is the task ending without its turn.
        async { end_rx.await.ok() },
        hop_rx,
        &mut delivery,
        &driver.observations,
        &activity,
        (force.clone(), cut),
        &driver.health,
    )
    .await;
    abandonment.0 = None;
    let _active = Ending(&driver.state, turn);
    let Some(routed) = routed else {
        driver.fail(DriverFailure::OwnedTask);
        return rejected(AdapterError::TaskFailed);
    };
    if matches!(rest, Rest::Undelivered) {
        driver.fail(DriverFailure::ObservationOverflow);
    }
    keep_records(
        driver,
        turn,
        (&routed, staged.profile, delivery.normalizer.patch()),
        (wall, force, (stop, close_rx)),
    )
    .await;
    let ended = Ended {
        turn,
        clamp,
        recipe,
    };
    ended.end(adapter, &delivery.normalizer, routed, (&rest, &activity))
}

/// One launch's process, what Route checks at the handshake, the
/// session's IDs and the recipe key.
struct Launch {
    process: crate::PrivateProcessSpec,
    expect: PiExpect,
    /// The Pi session ID launched with.
    session: String,
    session_dir: PathBuf,
    recipe: String,
    /// The requested effort's clamp key ([`launch::clamp_key`]).
    clamp: Option<String>,
}

/// The launch of turn `turn` (packet §§2.2, 4) on the state [`staged`]
/// wrote: `--session` the confirmed vendor ID, else `--session-id` the ID
/// derived from the VIA session, never a new one after a failure.
fn launch(
    driver: &SessionDriver,
    adapter: &PiAdapter,
    spec: &TurnSpec,
    (turn, staged): (crate::TurnNumber, &Staged),
) -> Launch {
    let session_dir = staged.session_dir.clone();
    let confirmed = driver.state().identity.clone();
    let resume = confirmed.is_some();
    let session = confirmed.unwrap_or_else(|| expected_session_id(&driver.spec.session_id));
    let inherit = driver.spec.inherit.requested;
    let recipe = Recipe {
        model: &driver.spec.model,
        thinking: spec.effort.as_deref(),
        session_dir: &session_dir,
        session: if resume {
            Continue::Resume(&session)
        } else {
            Continue::New(&session)
        },
        inherit,
        instructions: staged.instructions.as_deref(),
        vendor_args: driver.spec.vendor_args.as_slice(),
    };
    let owner = ProcessOwner::Turn {
        session_id: driver.spec.session_id.clone(),
        turn,
    };
    let process = adapter.process_spec(owner, &driver.spec.cwd, &recipe);
    let (provider, model_id) = driver
        .spec
        .model
        .split_once('/')
        .unwrap_or(("", &driver.spec.model));
    let expect = PiExpect {
        session_id: session.clone(),
        provider: provider.to_owned(),
        model_id: model_id.to_owned(),
        thinking: spec.effort.clone(),
        tools: TOOLS.iter().map(|tool| (*tool).to_owned()).collect(),
    };
    Launch {
        process,
        expect,
        session,
        session_dir,
        recipe: recipe_key(inherit, &driver.spec.vendor_args),
        clamp: spec
            .effort
            .as_deref()
            .map(|effort| launch::clamp_key(&driver.spec.model, effort)),
    }
}

/// Q3, AD18: a value the route refuses rejects the turn before anything
/// launches.
fn refused_values(driver: &SessionDriver, spec: &TurnSpec) -> Option<TurnEnd> {
    let instructions = driver.spec.instructions.as_deref();
    let params = TurnParams {
        effort: spec.effort.clone(),
        bound: spec.bound.clone(),
        output_schema: spec.output_schema.is_some(),
        instructions: instructions.is_some(),
        max_steps: spec.max_steps,
        vendor: spec.vendor.clone(),
        // C2 §6.3: re-judged before every launch.
        vendor_args: driver.spec.vendor_args.clone(),
        sizes: ParamSizes {
            instructions: instructions.map_or(0, str::len),
            instructions_json: instructions.map_or(0, |text| encoded_text_len(text) + 2),
            prompt_json: encoded_text_len(&spec.prompt) + 2,
            ..ParamSizes::default()
        },
        ..TurnParams::default()
    };
    let refusal = PiAdapter::check_values(route(), &params)
        .into_iter()
        .next()?;
    Some(rejected(AdapterError::Rejected {
        reason: start_rejected(refusal),
        evidence: TurnEvidence::no_launch(false),
    }))
}

/// The route this adapter serves, as refusals name it.
fn route() -> &'static str {
    Harness::parse(HARNESS).map_or(HARNESS, Harness::route)
}

/// A per-turn refusal as the definite rejection it is before submission.
fn start_rejected(refusal: Refusal) -> StartRejected {
    match refusal.kind {
        RefusalKind::BoundUnsupported => StartRejected::BoundUnsupported(refusal.message),
        RefusalKind::InvalidParam { field } | RefusalKind::VendorOptionConflict { field } => {
            StartRejected::InvalidParam { field }
        }
        RefusalKind::UnsupportedVerb
        | RefusalKind::HarnessUnavailable
        | RefusalKind::UnknownModel
        | RefusalKind::VersionRefused
        | RefusalKind::MissingCapability { .. } => StartRejected::Protocol(refusal.message),
    }
}

/// Packet §§4.3, 4.7: the turn's `pi-profile.json` (the record of the
/// check its launch passed) and, once its handshake passed,
/// `pi-inventory.json`, in the evidence folder its launch created. Best
/// effort, on a blocking task the session's tracker owns, waited for only
/// within [`records_by`]'s bound (picrit #5: never past the turn's one
/// cutoff) and until the daemon force. A record the task has not begun by
/// then is skipped, and the task notes the skipped names in
/// [`RECORDS_SKIPPED`] once it can write; the turn's end never waits for
/// that. The turn stays active meanwhile ([`Ending`]).
async fn keep_records(
    driver: &SessionDriver,
    turn: crate::TurnNumber,
    (routed, profile, patch): (&PiTurn, Vec<u8>, Option<&via_routes::pi::SystemPatch>),
    (wall, force, orders): (Deadline, ForceWatch, (StopWatch, CloseWatch)),
) {
    let launched = match &routed.outcome {
        Ok(_) => true,
        Err(failure) => failure.launched,
    };
    if !launched {
        return;
    }
    let folder = driver
        .runtime
        .turn_evidence_path(&driver.spec.session_id, turn);
    let mut records = vec![("pi-profile.json", profile)];
    if let Some(record) = routed
        .handshake
        .as_ref()
        .map(|facts| normalize::inventory(patch, &facts.skills, &driver.spec.cwd))
    {
        records.push(("pi-inventory.json", record));
    }
    // From now, within the wall's cutoff; the writer has its bound
    // before it starts (picrit round 3, 4).
    let from = Deadline::at(tokio::time::Instant::now().min(wall.instant()));
    let cut = records_cut(from, (&orders.0, &orders.1), &force);
    let written = driver.tracker.spawn_blocking({
        let cut = cut.clone();
        move || write_records(&folder, records, &cut)
    });
    if !records_by(written, (from, force, orders), &cut).await {
        cut.now();
    }
}

/// The records' first bound, from the orders and the daemon force as they
/// stand: an already expired cutoff, or a force already raised, skips
/// every record (picrit round 3, 4; round 4, 3).
fn records_cut(
    from: Deadline,
    (stop, close): (&StopWatch, &CloseWatch),
    force: &ForceWatch,
) -> Cut {
    let at = if force.borrow().is_some() {
        tokio::time::Instant::now()
    } else {
        let orders = [
            stop.borrow().as_ref().map(|order| order.close_by),
            close.borrow().as_ref().map(|order| order.close_by),
        ];
        earliest_close(from, orders)
    };
    Cut(Arc::new(Mutex::new(at.into_std())))
}

/// Ends the turn's active state (steer lane and close order) when the
/// turn's end is decided, after its records: until then a direct close
/// still publishes its deadline to the delivery and records waits (picrit
/// round 2, D).
struct Ending<'a>(&'a Mutex<DriverState>, crate::TurnNumber);

impl Drop for Ending<'_> {
    fn drop(&mut self) {
        end_active(self.0, self.1);
    }
}

/// The records' cutoff as the writer checks it before each record: set
/// before the writer starts ([`records_cut`]), then the current bound
/// [`records_by`] publishes, so a writer resuming past it begins nothing
/// even before the waiter wakes (picrit round 2, D; round 3, 4).
#[derive(Clone)]
struct Cut(Arc<Mutex<std::time::Instant>>);

impl Cut {
    /// The bound is `at`.
    fn set(&self, at: tokio::time::Instant) {
        *self.at() = at.into_std();
    }

    /// The bound has passed.
    fn now(&self) {
        *self.at() = std::time::Instant::now();
    }

    /// Whether the bound has passed.
    fn passed(&self) -> bool {
        std::time::Instant::now() >= *self.at()
    }

    /// The bound, a poisoned lock read through.
    fn at(&self) -> std::sync::MutexGuard<'_, std::time::Instant> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The driver's close order, as [`TurnTask`] takes it.
type CloseWatch = watch::Receiver<Option<StopOrder>>;

/// Where the records' task notes the records it skipped.
const RECORDS_SKIPPED: &str = "pi-records-skipped.json";

/// Whether `written` finished within the records' bound: the earlier of
/// the cleanup allowance from now and the turn's one cutoff (the wall plus
/// [`CLEANUP_ALLOWANCE`], C2 §4.1), and the `close_by` of a stop or the
/// driver's close order, as they stand or arrive; the daemon force ends
/// the wait at once.
async fn records_by(
    written: impl std::future::Future,
    (from, mut force, orders): (Deadline, ForceWatch, (StopWatch, CloseWatch)),
    cut: &Cut,
) -> bool {
    let forced = async move {
        // A force sender gone unset: no force will come.
        if force.wait_for(Option::is_some).await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        biased;
        () = forced => false,
        _ = written => true,
        () = by_orders(from, orders, |bound| cut.set(bound)) => false,
    }
}

/// Resolves at the turn's one cutoff, [`CLEANUP_ALLOWANCE`] after `from`
/// (C2 §4.1), or at the earlier `close_by` of a stop or the driver's close
/// order, re-read as they change (packet §7.1, picrit round 2, B).
/// `publish` hears each bound as it is taken.
async fn by_orders(
    from: Deadline,
    (mut stop, mut close): (StopWatch, CloseWatch),
    mut publish: impl FnMut(tokio::time::Instant),
) {
    let (mut stop_open, mut close_open) = (true, true);
    loop {
        let orders = [
            stop.borrow_and_update()
                .as_ref()
                .map(|order| order.close_by),
            close
                .borrow_and_update()
                .as_ref()
                .map(|order| order.close_by),
        ];
        let bound = earliest_close(from, orders);
        publish(bound);
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(bound) => return,
            changed = stop.changed(), if stop_open => stop_open = changed.is_ok(),
            changed = close.changed(), if close_open => close_open = changed.is_ok(),
        }
    }
}

/// The turn's one cutoff, [`CLEANUP_ALLOWANCE`] after `from` (C2 §4.1),
/// or the earlier of `orders`' `close_by`.
fn earliest_close(from: Deadline, orders: [Option<Deadline>; 2]) -> tokio::time::Instant {
    orders
        .into_iter()
        .flatten()
        .map(Deadline::instant)
        .fold(from.instant() + CLEANUP_ALLOWANCE, std::cmp::Ord::min)
}

/// Writes `records` into `folder` in order; once `cut` has passed, those
/// not begun are skipped and named in [`RECORDS_SKIPPED`] instead.
fn write_records(folder: &Path, records: Vec<(&'static str, Vec<u8>)>, cut: &Cut) {
    // Test builds: a blocked evidence write (picrit #5).
    #[cfg(feature = "test-failpoints")]
    drop(via_routes::failpoint::hit("adapter.pi.records.write"));
    let mut skipped = Vec::new();
    for (name, bytes) in records {
        if cut.passed() {
            skipped.push(name);
        } else {
            write_record(&folder.join(name), &bytes);
        }
    }
    if !skipped.is_empty() {
        let note = serde_json::json!({
            "skipped": skipped,
            "reason": "not begun by the turn's cleanup cutoff",
        });
        write_record(&folder.join(RECORDS_SKIPPED), note.to_string().as_bytes());
    }
}

/// Writes one new 0600 evidence record; an error leaves it out (the
/// turn's outcome never depends on it).
fn write_record(path: &Path, bytes: &[u8]) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(bytes));
    // Best effort by design (packet §§4.3, 4.7): the record is evidence
    // beside the turn, never a condition of it.
    drop(written);
}

/// The turn's delivery side: the normalizer and where its facts outlive
/// the turn.
struct Delivery<'a> {
    normalizer: Normalizer,
    state: &'a Mutex<DriverState>,
    health: &'a watch::Sender<DriverHealth>,
}

impl Normalize for Delivery<'_> {
    type Message = PiItem;

    fn items(&mut self, item: PiItem, at: tokio::time::Instant) -> Vec<ObservationItem> {
        let observations = self.normalizer.item(item);
        for observation in &observations {
            if let Observation::IdentityConfirmed(identity) = observation {
                lock(self.state).identity = Some(identity.vendor_session_id.clone());
            }
            // C2 §2: the connection's identity is broken; no later turn
            // runs.
            if matches!(observation, Observation::ResumeMismatch { .. }) {
                latch(self.health, DriverFailure::ResumeMismatch);
            }
        }
        observations
            .into_iter()
            .map(|observation| ObservationItem {
                at,
                vendor_turn: None,
                observation,
            })
            .collect()
    }

    fn emitted(&mut self, _delivered: bool) {}
}

/// What the turn's end needs beside Route's.
struct Ended {
    turn: crate::TurnNumber,
    /// The requested effort's clamp key ([`launch::clamp_key`]), cached
    /// when Pi applied a different level; with none, the applied level
    /// goes to `vendor` data (packet §4.5).
    clamp: Option<String>,
    /// The launch's recipe key (C2 §5): a refusal is cached for it alone.
    recipe: String,
}

impl Ended {
    /// A launched handshake's refusal, cached (C2 §5) on this binary:
    /// `-ne`/`-np` not holding for the recipe (packet §3), or Pi clamping
    /// the requested effort for the model (packet §4.5).
    fn cache(&self, adapter: &PiAdapter, failure: &RouteFailure) {
        if !failure.launched {
            return;
        }
        let (key, cause) = if matches!(failure.cause, RouteError::HandshakeRefused { .. }) {
            (Some(&self.recipe), "get_commands")
        } else if matches!(
            failure.cause,
            RouteError::InvalidParam {
                field: "effort",
                ..
            }
        ) {
            (self.clamp.as_ref(), "thinkingLevel")
        } else {
            (None, "")
        };
        if let Some(key) = key {
            adapter.instances.record_refusal(
                &adapter.binary,
                key.clone(),
                Incompatibility::ReadbackDiffers(cause),
                plan::clock(),
            );
        }
    }

    /// The turn's one result (C2 §4.1): the terminal Route retained at
    /// `agent_settled`, mapped with the abort's facts (packet §§5.3, 7.1),
    /// and Route's outcome, its handshake causes as C2 rejections.
    fn end(
        self,
        adapter: &PiAdapter,
        normalizer: &Normalizer,
        routed: PiTurn,
        (rest, activity): (&Rest, &crate::TurnActivity),
    ) -> TurnEnd {
        let PiTurn {
            outcome,
            submitted,
            handshake,
            terminal,
            refused,
            abort,
        } = routed;
        let turn = self.turn;
        let vendor = handshake
            .as_ref()
            .filter(|_| self.clamp.is_none())
            .and_then(|facts| normalize::thinking_data(&facts.thinking_level));
        let lost = !accounted(normalizer, (rest, activity), &outcome);
        let terminal = terminal.map(|message| {
            let at = tokio::time::Instant::now();
            normalize::terminal(&message, abort, (normalizer.cost(), vendor), at)
        });
        let acknowledged = terminal
            .as_ref()
            .is_some_and(|terminal| terminal.status == VendorTerminalStatus::Interrupted);
        let forced = matches!(rest, Rest::Forced)
            || matches!(&outcome, Err(failure) if matches!(failure.cause, RouteError::ForceStopped { .. }));
        let rejection = |reason| TurnEnd {
            loss: None,
            aggregate: None,
            terminal: None,
            instance: None,
            leftovers: None,
            outcome: Err(AdapterError::Rejected {
                reason,
                evidence: evidence_of(&outcome),
            }),
        };
        if refused && !forced {
            return rejection(StartRejected::VendorError(None, PROMPT_REFUSED.to_owned()));
        }
        if let Err(failure) = &outcome {
            if matches!(failure.cause, RouteError::ResumeMismatch { .. }) {
                return TurnEnd {
                    loss: None,
                    aggregate: None,
                    terminal: None,
                    instance: None,
                    leftovers: None,
                    outcome: Err(AdapterError::ResumeMismatch {
                        evidence: TurnEvidence::of_failure(failure),
                    }),
                };
            }
            self.cache(adapter, failure);
            if let RouteError::InvalidParam { field, .. } = failure.cause {
                return rejection(StartRejected::InvalidParam { field });
            }
            if matches!(failure.cause, RouteError::ProcessExited { .. }) && !submitted {
                return rejection(StartRejected::Protocol(EXITED_EARLY.to_owned()));
            }
        }
        let routed = match (&outcome, rest) {
            (Err(failure), _) => Err(AdapterError::Route(RouteFailure {
                acknowledged,
                ..failure.clone()
            })),
            (Ok(result), Rest::Delivered) => Ok(evidence_of_result(result)),
            (Ok(result), Rest::Undelivered) => Err(AdapterError::Route(failure_of(
                RouteError::Overflow { turn },
                result,
                acknowledged,
            ))),
            (Ok(result), Rest::Forced) => Err(AdapterError::Route(failure_of(
                RouteError::ForceStopped { turn },
                result,
                acknowledged,
            ))),
        };
        let mut end = TurnEnd {
            loss: None,
            aggregate: None,
            terminal,
            instance: None,
            leftovers: None,
            outcome: routed,
        };
        // The all-null aggregate on the terminal, else on the end (C2 §5,
        // bead via-i5g).
        if lost {
            unaccounted(&mut end);
        }
        end
    }
}

/// Packet §5.5, the one accounting rule: the turn's tokens and cost are
/// known only when every message that could carry usage decoded with
/// usable usage and reached the normalizer. Otherwise they are
/// unavailable, with a terminal (its all-null usage) or without one (the
/// all-null [`TurnEnd::aggregate`]), whatever the delivered samples sum to.
fn accounted(
    normalizer: &Normalizer,
    (rest, activity): (&Rest, &crate::TurnActivity),
    outcome: &Result<PiRouteResult, RouteFailure>,
) -> bool {
    // Every sample normalized was usable: no call without usage (a null
    // sample, a compaction without usage, a hidden retry) and none held
    // for an acceptance that never came.
    normalizer.complete()
        // Every message Route decoded reached the normalizer: no overflow,
        // force or cutoff cut delivery.
        && activity.delivered() >= activity.decoded()
        && !matches!(rest, Rest::Undelivered)
        && match outcome {
            Ok(_) => true,
            // Overflow lost delivery. A message Route could not decode
            // (missing usage, a nonnumeric counter, any malformed, oversize
            // or unterminated record) may have carried usage: Route and
            // Wire note every such message (`undecoded.bin`, runtime C4),
            // whatever the failure's cause. A phase violation among
            // decoded messages loses nothing.
            Err(failure) => {
                failure.undecoded.is_none()
                    && !matches!(failure.cause, RouteError::Overflow { .. })
            }
        }
}

/// The exit Host confirmed, if any.
fn exit_of(result: &PiRouteResult) -> Option<via_routes::ExitReport> {
    (result.exit.code.is_some() || result.exit.signal.is_some()).then_some(result.exit)
}

fn evidence_of_result(result: &PiRouteResult) -> TurnEvidence {
    TurnEvidence {
        exit: exit_of(result),
        cleanup: cleanup(result.cleanup),
        journal_uncertain: result.journal_uncertain,
    }
}

/// Route's evidence of the turn, whatever its outcome.
fn evidence_of(outcome: &Result<PiRouteResult, RouteFailure>) -> TurnEvidence {
    match outcome {
        Ok(result) => evidence_of_result(result),
        Err(failure) => TurnEvidence::of_failure(failure),
    }
}

/// A failure of `cause` with a finalized turn's evidence.
fn failure_of(cause: RouteError, result: &PiRouteResult, acknowledged: bool) -> RouteFailure {
    RouteFailure {
        cause,
        undecoded: None,
        exit: exit_of(result),
        launched: true,
        cleanup: Some(result.cleanup),
        forced: result.forced,
        journal_uncertain: result.journal_uncertain,
        acknowledged,
        shared: false,
        launch: None,
    }
}

/// The failure a Route outcome latches in the health lane (C2 §2).
fn route_failure(turn: &PiTurn) -> Option<DriverFailure> {
    let Err(failure) = &turn.outcome else {
        return None;
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
        | RouteError::InvalidParam { .. } => None,
    }
}

/// One turn's route work, owned by the session's tracker.
struct TurnTask {
    route: PiRoute,
    process: crate::PrivateProcessSpec,
    start: PiStart,
    hop: via_routes::Hop<PiItem>,
    signals: (Deadline, ForceWatch, StopWatch),
    /// The driver's close order.
    close: watch::Receiver<Option<StopOrder>>,
    cancel: CancellationToken,
    input: (PiExpect, oneshot::Sender<PiTurn>),
    reservation: Reservation,
    state: Arc<Mutex<DriverState>>,
    health: Arc<watch::Sender<DriverHealth>>,
    done: watch::Sender<Retiring>,
}

/// Runs the turn through Route with Core's stop order merged with the
/// driver's close and the session's cancellation. Route's failure is
/// latched in the health lane at once, whatever becomes of `run_turn`;
/// then the process's retirement is recorded.
async fn turn_task(task: TurnTask) {
    let TurnTask {
        route,
        process,
        start,
        hop,
        signals: (wall, force, core_stop),
        close,
        cancel,
        input: (expect, end),
        reservation,
        state,
        health,
        done,
    } = task;
    let turn = start.turn();
    let (merged, merged_rx) =
        watch::channel(earliest(core_stop.borrow().clone(), close.borrow().clone()));
    // Route reads the sources too where the relay below may lag: an order
    // set before the launch gate wins there (design §2 rule 1).
    let sources: StopSources = {
        let (core, close, cancel) = (core_stop.clone(), close.clone(), cancel.clone());
        Arc::new(move || {
            core.borrow().is_some() || close.borrow().is_some() || cancel.is_cancelled()
        })
    };
    let (routed, routed_rx) = oneshot::channel();
    let route_turn = route.turn(
        process,
        start,
        hop,
        (wall, force, (merged_rx, sources)),
        (expect, routed),
    );
    let relay = async {
        let Ok(turn_result) = routed_rx.await else {
            return;
        };
        if let Some(cause) = route_failure(&turn_result) {
            latch(&health, cause);
        }
        // `run_turn` ends the active state after its records; dropped, it
        // never will: the retirement below is still owned here.
        if end.send(turn_result).is_err() {
            end_active(&state, turn);
        }
    };
    let retirement = tokio::select! {
        (retirement, ()) = async { tokio::join!(route_turn, relay) } => retirement,
        never = merge_stops(core_stop, close, &cancel, &merged) => match never {},
    };
    reservation.retired(retirement);
    drop(reservation);
    done.send_replace(Retiring::CleanedUp);
    done.send_replace(Retiring::Delivered);
}

#[cfg(test)]
mod tests {
    use super::{CLEANUP_ALLOWANCE, RECORDS_SKIPPED, records_cut, write_records};
    use crate::Deadline;
    use tokio::sync::watch;

    /// Picrit round 2, D, round 3, 4 and round 4, 3: the writer decides
    /// the skip itself, from the bound it is built with before it starts.
    /// A cutoff already expired, or a daemon force already raised, then
    /// begins no record, before any waiter publishes a bound or marks the
    /// records late; a cutoff still ahead writes them.
    #[tokio::test]
    async fn the_writer_skips_records_past_the_cutoff() {
        let (_stop_tx, stop) = watch::channel(None);
        let (_close_tx, close) = watch::channel(None);
        let (force_tx, force) = watch::channel(None);
        let records = || vec![("pi-profile.json", b"{}".to_vec())];

        // The turn's cutoff (from + the allowance) has passed.
        let expired = Deadline::at(
            tokio::time::Instant::now()
                .checked_sub(CLEANUP_ALLOWANCE + std::time::Duration::from_secs(1))
                .unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        write_records(
            dir.path(),
            records(),
            &records_cut(expired, (&stop, &close), &force),
        );
        assert!(!dir.path().join("pi-profile.json").exists());
        let note = std::fs::read_to_string(dir.path().join(RECORDS_SKIPPED)).unwrap();
        assert!(note.contains("pi-profile.json"), "{note}");

        let dir = tempfile::tempdir().unwrap();
        let now = Deadline::at(tokio::time::Instant::now());
        write_records(
            dir.path(),
            records(),
            &records_cut(now, (&stop, &close), &force),
        );
        assert!(dir.path().join("pi-profile.json").exists());
        assert!(!dir.path().join(RECORDS_SKIPPED).exists());

        // The force is already raised: the cutoff starts passed.
        force_tx.send_replace(Some(tokio::time::Instant::now()));
        let dir = tempfile::tempdir().unwrap();
        write_records(
            dir.path(),
            records(),
            &records_cut(now, (&stop, &close), &force),
        );
        assert!(!dir.path().join("pi-profile.json").exists());
        let note = std::fs::read_to_string(dir.path().join(RECORDS_SKIPPED)).unwrap();
        assert!(note.contains("pi-profile.json"), "{note}");
    }
}
