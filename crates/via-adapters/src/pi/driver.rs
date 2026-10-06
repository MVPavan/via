//! Pi's driver turn (C2 §2, §4, §4.1; packet §§2–7): one private
//! `pi --mode rpc` process per VIA turn. The pre-launch checks run first,
//! with no vendor process: the profile policy (§4.3), R1's predecessor
//! check (§7.4) and the version read (§3). Then the turn runs through the
//! Pi route on a task the session's tracker owns, each handed-over item
//! normalized in decode order; Route decides every handshake check, and
//! the turn's end maps its cause.

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::launch::{self, Continue, Recipe, TOOLS, expected_session_id, recipe_key};
use super::normalize::{self, LaunchFacts, Normalizer};
use super::{HARNESS, PiAdapter, plan, profile};
use crate::driver::turn::{
    Abandonment, CLEANUP_ALLOWANCE, Normalize, Rest, deliver_beside, earliest, end_active,
    merge_stops, ordered,
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
    // Packet §4.3: never cached; nothing launches.
    if let Err(detail) = profile::check(
        &launch::agent_dir(&adapter.vendor_state_dir),
        profile::daemon_uid(),
    ) {
        return unlaunched(RouteError::HandshakeRefused {
            turn,
            detail: Some(format!("the Pi profile policy refused: {detail}")),
        });
    }
    let route = PiRoute::new(Arc::clone(&driver.runtime));
    // Packet §7.4 (R1): one non-signalling pass within the wall.
    match route
        .predecessors_resolved(&driver.spec.session_id, turn, cx.wall)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return rejected(AdapterError::Rejected {
                reason: StartRejected::UncertainPredecessor,
                evidence: TurnEvidence::no_launch(false),
            });
        }
        Err(cause) => return unlaunched(cause),
    }
    // Packet §3: before each launch, no process; reported on every
    // outcome from here.
    let version = launch::read_version(&adapter.binary);
    if let Some(version) = &version {
        adapter
            .instances
            .record_version(HARNESS, &adapter.binary, version.clone());
    }
    let instance = InstanceReport {
        version_status: plan::version_status(version.as_deref()),
        vendor_version: version,
    };
    let mut end = launched(driver, adapter, (spec, cx), route, instance.clone()).await;
    end.instance.get_or_insert(instance);
    end
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
    route: PiRoute,
    instance: InstanceReport,
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
    } = match launch(driver, adapter, &spec, turn) {
        Ok(launch) => launch,
        Err(end) => return *end,
    };
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
        signals: (wall, force.clone(), stop),
        close: close_rx,
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
    let cutoff = Deadline::at(wall.instant() + CLEANUP_ALLOWANCE);
    let mut abandonment = Abandonment(Some(&driver.health));
    let (routed, rest) = deliver_beside(
        // A closed channel is the task ending without its turn.
        async { end_rx.await.ok() },
        hop_rx,
        &mut delivery,
        &driver.observations,
        &activity,
        (force, cutoff),
        &driver.health,
    )
    .await;
    abandonment.0 = None;
    end_active(&driver.state, turn);
    let Some(routed) = routed else {
        driver.fail(DriverFailure::OwnedTask);
        return rejected(AdapterError::TaskFailed);
    };
    if matches!(rest, Rest::Undelivered) {
        driver.fail(DriverFailure::ObservationOverflow);
    }
    let ended = Ended {
        turn,
        effort: spec.effort.is_some(),
        recipe,
    };
    keep_records(driver, adapter, turn, &routed, delivery.normalizer.patch());
    ended.end(adapter, &delivery.normalizer, routed, &rest)
}

/// One launch's process, what Route checks at the handshake, the
/// session's IDs and the recipe key.
struct Launch {
    process: crate::PrivateProcessSpec,
    expect: PiExpect,
    /// The Pi session ID launched with.
    session: String,
    session_dir: std::path::PathBuf,
    recipe: String,
}

/// The launch of turn `turn` (packet §§2.2, 4): VIA's session directory
/// and frozen instructions written first; `--session` the confirmed
/// vendor ID, else `--session-id` the ID derived from the VIA session,
/// never a new one after a failure.
fn launch(
    driver: &SessionDriver,
    adapter: &PiAdapter,
    spec: &TurnSpec,
    turn: crate::TurnNumber,
) -> Result<Launch, Box<TurnEnd>> {
    let Ok((session_dir, instructions)) = launch::prepare(
        &adapter.vendor_state_dir,
        &driver.spec.session_id,
        driver.spec.instructions.as_deref(),
    ) else {
        // VIA's own state for the launch could not be written: nothing
        // launches.
        return Err(Box::new(unlaunched(RouteError::Store {
            turn,
            kind: StoreFailure::Evidence,
        })));
    };
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
        instructions: instructions.as_deref(),
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
    Ok(Launch {
        process,
        expect,
        session,
        session_dir,
        recipe: recipe_key(inherit, &driver.spec.vendor_args),
    })
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

/// Packet §§4.3, 4.7: the turn's `pi-profile.json` and, once its handshake
/// passed, `pi-inventory.json`, in the evidence folder its launch
/// created. Best effort: a record that cannot be written is left out.
fn keep_records(
    driver: &SessionDriver,
    adapter: &PiAdapter,
    turn: crate::TurnNumber,
    routed: &PiTurn,
    patch: Option<&via_routes::pi::SystemPatch>,
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
    // The profile as it stands now: the record of what the launch read.
    if let Ok(profile) = profile::check(
        &launch::agent_dir(&adapter.vendor_state_dir),
        profile::daemon_uid(),
    ) {
        write_record(&folder.join("pi-profile.json"), &profile.record);
    }
    if let Some(facts) = &routed.handshake {
        let record = normalize::inventory(patch, &facts.skills, &driver.spec.cwd);
        write_record(&folder.join("pi-inventory.json"), &record);
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
    /// Whether effort was requested: else the applied level goes to
    /// `vendor` data (packet §4.5).
    effort: bool,
    /// The launch's recipe key (C2 §5): a refusal is cached for it alone.
    recipe: String,
}

impl Ended {
    /// The turn's one result (C2 §4.1): the terminal Route retained at
    /// `agent_settled`, mapped with the abort's facts (packet §§5.3, 7.1),
    /// and Route's outcome, its handshake causes as C2 rejections.
    fn end(
        self,
        adapter: &PiAdapter,
        normalizer: &Normalizer,
        routed: PiTurn,
        rest: &Rest,
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
            .filter(|_| !self.effort)
            .and_then(|facts| normalize::thinking_data(&facts.thinking_level));
        let terminal = terminal.map(|message| {
            normalize::terminal(
                &message,
                abort,
                (normalizer.cost(), vendor),
                tokio::time::Instant::now(),
            )
        });
        let acknowledged = terminal
            .as_ref()
            .is_some_and(|terminal| terminal.status == VendorTerminalStatus::Interrupted);
        let forced = matches!(rest, Rest::Forced)
            || matches!(&outcome, Err(failure) if matches!(failure.cause, RouteError::ForceStopped { .. }));
        let rejection = |reason| TurnEnd {
            loss: None,
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
                    terminal: None,
                    instance: None,
                    leftovers: None,
                    outcome: Err(AdapterError::ResumeMismatch {
                        evidence: TurnEvidence::of_failure(failure),
                    }),
                };
            }
            if let RouteError::InvalidParam { field, .. } = failure.cause {
                return rejection(StartRejected::InvalidParam { field });
            }
            if matches!(failure.cause, RouteError::ProcessExited { .. }) && !submitted {
                return rejection(StartRejected::Protocol(EXITED_EARLY.to_owned()));
            }
            // Packet §3: `-ne`/`-np` did not hold for this binary and
            // recipe: cached (C2 §5).
            if matches!(failure.cause, RouteError::HandshakeRefused { .. }) && failure.launched {
                adapter.instances.record_refusal(
                    &adapter.binary,
                    self.recipe.clone(),
                    Incompatibility::ReadbackDiffers("get_commands"),
                    plan::clock(),
                );
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
        TurnEnd {
            loss: None,
            terminal,
            instance: None,
            leftovers: None,
            outcome: routed,
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
        end_active(&state, turn);
        // `run_turn` was dropped: the retirement below is still owned here.
        let _unread = end.send(turn_result);
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
