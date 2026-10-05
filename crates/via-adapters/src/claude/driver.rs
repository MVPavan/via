//! Claude's driver turn (C2 §2, §4, §4.1; packet §§2–7; adapter design
//! AD3–AD9, AD19): one private `claude -p` process per VIA turn, run
//! through the Claude route on a task the session's tracker owns. Each
//! item the route hands over is normalized in decode order; a verdict
//! the normalizer reaches before the terminal (another session, a refused
//! handshake, a protocol contradiction, a tracking overflow) stops the
//! vendor with the AD19 order (the interrupt, then stdin EOF after its
//! result, then S1's close) and ends the turn with that cause, never a
//! resend.

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::launch::{Continue, Recipe, expected_session_id, recipe_key};
use super::normalize::{Batch, End, LaunchFacts, Normalizer};
use super::{ClaudeAdapter, HARNESS};
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
    AdapterError, Observation, ObservationItem, TurnEnd, TurnEvidence, VendorTerminal,
};
use crate::plan::{Category, InheritState, ParamSizes, Refusal, RefusalKind, TurnParams, Warning};
use crate::runtime::cleanup;
use crate::{
    AcceptanceToken, Deadline, DriverFailure, DriverHealth, ProcessOwner, RouteError, RouteFailure,
    StartRejected, StopCause, StopOrder, StopWatch,
};
use via_routes::StopSources;
use via_routes::claude::{
    ClaudeItem, ClaudeRoute, ClaudeRouteResult, ClaudeStart, ClaudeTurn, Message,
};

/// The warning a session-cumulative cost lower than the session's last
/// one gives (packet §5: an unexpected counter reset warns and keeps the
/// reported value). Not in C1 §5's closed list: Core keeps it as a durable
/// `warning` event, and no envelope carries it.
pub(crate) const COST_COUNTER_RESET: &str = "cost_counter_reset";

/// The ID identity confirmations name for connection `generation`.
pub(crate) fn connection_id(generation: u64) -> String {
    format!("claude-{generation}")
}

/// Runs one submitted turn (C2 §4.1): its values are checked against the
/// argument budget before anything launches (ruling Q3, AD18); then the
/// turn's process runs through the Claude route on a tracker-owned task
/// while each handed-over item is normalized and delivered to the session
/// channel. Dropping this future leaves the task, which owns the turn's
/// cleanup, running.
pub(crate) async fn run_turn(
    driver: &SessionDriver,
    adapter: &ClaudeAdapter,
    spec: TurnSpec,
    cx: TurnCx,
) -> TurnEnd {
    if let Some(refused) = refused_values(driver, &spec) {
        return refused;
    }
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
        facts,
        recipe,
    } = match launch(driver, adapter, &spec, turn, generation) {
        Ok(launch) => launch,
        Err(end) => return *end,
    };
    process.capacity = capacity;
    let (steer, _unused) = via_routes::steer::steer_lane(str::len);
    let (close, close_rx) = watch::channel(None);
    let (abort, abort_rx) = watch::channel(None);
    let (done, retiring) = watch::channel(Retiring::Running);
    {
        let mut state = driver.state();
        state.active = Some(Active::new(turn, steer, close));
        state.retiring = Some(retiring);
    }
    // Route reads ahead up to its own bound; one item waits here.
    let (hop, hop_rx) = mpsc::channel::<via_routes::Decoded<ClaudeItem>>(1);
    let hop = via_routes::Hop::new(hop, activity.decode_watermark());
    let (end, end_rx) = oneshot::channel();
    driver.tracker.spawn(turn_task(TurnTask {
        route: ClaudeRoute::new(Arc::clone(&driver.runtime)),
        process,
        start: ClaudeStart::new(turn, spec.prompt),
        hop,
        signals: (wall, force.clone(), stop),
        stops: (close_rx, abort_rx),
        cancel: driver.cancel.clone(),
        end,
        reservation,
        state: Arc::clone(&driver.state),
        health: Arc::clone(&driver.health),
        done,
    }));
    let mut delivery = Delivery {
        normalizer: Normalizer::new(facts),
        reports: Reports {
            adapter,
            state: &driver.state,
            health: &driver.health,
            recipe,
        },
        abort,
        wall,
        turn,
        verdict: None,
        terminal: None,
        saw_result: false,
        version_recorded: false,
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
    delivery.turn_end(routed, &rest)
}

/// One launch's process, the normalizer's facts and the recipe key.
struct Launch {
    process: crate::PrivateProcessSpec,
    facts: LaunchFacts,
    recipe: String,
}

/// The launch of turn `turn` on connection `generation` (packet §§2, 4;
/// ruling Q4): `--resume` the session's confirmed vendor ID, else
/// `--session-id` the UUID derived from the VIA session, never a new one
/// after a failure; the session's frozen settings and the turn's values.
fn launch(
    driver: &SessionDriver,
    adapter: &ClaudeAdapter,
    spec: &TurnSpec,
    turn: crate::TurnNumber,
    generation: u64,
) -> Result<Launch, Box<TurnEnd>> {
    let confirmed = driver.state().identity.clone();
    let resume = confirmed.is_some();
    let expected = confirmed.unwrap_or_else(|| expected_session_id(&driver.spec.session_id));
    let inherit = driver.spec.inherit.requested;
    let schema = spec.output_schema.is_some();
    let extra_write_dirs = spec
        .bound
        .as_ref()
        .or(driver.spec.initial_bound.as_ref())
        .map_or(&[][..], |bound| bound.extra_write_dirs.as_slice());
    let recipe = Recipe {
        model: &driver.spec.model,
        session: if resume {
            Continue::Resume(&expected)
        } else {
            Continue::New(&expected)
        },
        mode: adapter.mode,
        inherit,
        extra_write_dirs,
        instructions: driver.spec.instructions.as_deref(),
        effort: spec.effort.as_deref(),
        output_schema: spec.output_schema.as_deref(),
        max_steps: spec.max_steps,
    };
    let owner = ProcessOwner::Turn {
        session_id: driver.spec.session_id.clone(),
        turn,
    };
    let Ok(process) = adapter.process_spec(owner, &driver.spec.cwd, &recipe) else {
        // Core hands down a validated schema: one that does not parse
        // starts nothing.
        return Err(Box::new(rejected(AdapterError::Rejected {
            reason: StartRejected::InvalidParam {
                field: "output_schema",
            },
            evidence: TurnEvidence::no_launch(false),
        })));
    };
    Ok(Launch {
        process,
        facts: LaunchFacts {
            expected_session: expected,
            resume,
            connection_id: connection_id(generation),
            correlation: AcceptanceToken::FIRST,
            schema,
            mcp: inherit.get(Category::McpServers) != InheritState::Off,
        },
        recipe: recipe_key(adapter.mode, inherit, schema),
    })
}

/// Q3, AD18: a value past the argument budget, or one the route refuses,
/// rejects the turn before anything launches, with the sizes the launch
/// would pass.
fn refused_values(driver: &SessionDriver, spec: &TurnSpec) -> Option<TurnEnd> {
    let params = TurnParams {
        effort: spec.effort.clone(),
        bound: spec.bound.clone(),
        output_schema: spec.output_schema.is_some(),
        instructions: driver.spec.instructions.is_some(),
        max_steps: spec.max_steps,
        vendor: spec.vendor.clone(),
        sizes: ParamSizes {
            instructions: driver.spec.instructions.as_deref().map_or(0, str::len),
            output_schema: spec
                .output_schema
                .as_deref()
                .map_or(0, |raw| raw.get().len()),
            ..ParamSizes::default()
        },
        ..TurnParams::default()
    };
    let refusal = ClaudeAdapter::check_values(route(), &params)
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
        RefusalKind::InvalidParam { field } => StartRejected::InvalidParam { field },
        RefusalKind::VendorOptionConflict => StartRejected::InvalidParam { field: "vendor" },
        RefusalKind::UnsupportedVerb
        | RefusalKind::HarnessUnavailable
        | RefusalKind::UnknownModel
        | RefusalKind::VersionRefused
        | RefusalKind::MissingCapability { .. } => StartRejected::Protocol(refusal.message),
    }
}

/// Where the turn's facts outlive it: the instance cache, the driver's
/// state and health.
struct Reports<'a> {
    adapter: &'a ClaudeAdapter,
    state: &'a Mutex<DriverState>,
    health: &'a watch::Sender<DriverHealth>,
    /// The launch's recipe key (C2 §5): a refusal is cached for it alone.
    recipe: String,
}

/// How the normalizer ended the turn before, or beside, its terminal.
#[derive(Debug)]
enum Verdict {
    /// A pre-init rejection.
    Rejected(StartRejected),
    /// Another session than the expected one.
    ResumeMismatch,
    /// The handshake check refused the instance (cached).
    Refused(Incompatibility),
    /// A protocol contradiction the normalizer found.
    Protocol(&'static str),
    /// A tracking overflow.
    Overflow,
}

/// The turn's delivery side: the normalizer, the verdict and the
/// terminal it kept, and the abort order a verdict sends Route.
struct Delivery<'a> {
    normalizer: Normalizer,
    reports: Reports<'a>,
    /// The driver's own stop order: AD19's soft stop for a verdict.
    abort: watch::Sender<Option<StopOrder>>,
    wall: Deadline,
    turn: crate::TurnNumber,
    verdict: Option<Verdict>,
    /// The one retained terminal (AD4), kept beside an overflow.
    terminal: Option<Box<VendorTerminal>>,
    /// A `result` reached the normalizer.
    saw_result: bool,
    version_recorded: bool,
}

impl Normalize for Delivery<'_> {
    type Message = ClaudeItem;

    /// One item's observations. After a verdict nothing more is
    /// normalized: the vendor is being stopped.
    fn items(&mut self, item: ClaudeItem, at: tokio::time::Instant) -> Vec<ObservationItem> {
        if self.verdict.is_some() {
            return Vec::new();
        }
        let batch = match item {
            ClaudeItem::InterruptSent(id) => {
                self.normalizer.interrupt_sent(id);
                return Vec::new();
            }
            // Route wrote the decline whole: only now is it reported and
            // its call's denial suppressed (Q9).
            ClaudeItem::Declined(request) => {
                let mut batch = self
                    .normalizer
                    .message(Message::ControlRequest(request), at);
                if let Some(pending) = batch.decline.take() {
                    let declined = self.normalizer.declined(pending);
                    batch.observations.extend(declined.observations);
                    batch.end = batch.end.or(declined.end);
                }
                batch
            }
            ClaudeItem::Message(message) => {
                self.saw_result |= matches!(message, Message::Result(_));
                self.normalizer.message(message, at)
            }
        };
        self.absorb(batch)
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

impl Delivery<'_> {
    /// Takes one batch's facts: the instance's version, a confirmed
    /// identity, the terminal (with the cost check), and an end.
    fn absorb(&mut self, batch: Batch) -> Vec<Observation> {
        let Batch {
            mut observations,
            decline: _,
            terminal,
            end,
        } = batch;
        if !self.version_recorded
            && let Some(version) = self
                .normalizer
                .instance()
                .and_then(|instance| instance.vendor_version)
        {
            self.version_recorded = true;
            let adapter = self.reports.adapter;
            adapter
                .instances
                .record_version(HARNESS, &adapter.binary, version);
        }
        for observation in &observations {
            if let Observation::IdentityConfirmed(identity) = observation {
                lock(self.reports.state).identity = Some(identity.vendor_session_id.clone());
            }
        }
        if let Some(terminal) = terminal {
            observations.extend(self.cost_reset(&terminal));
            self.terminal = Some(terminal);
        }
        match end {
            None | Some(End::Terminal) => {}
            Some(End::Rejected(reason)) => self.verdict = Some(Verdict::Rejected(reason)),
            Some(End::ResumeMismatch) => self.fail(Verdict::ResumeMismatch),
            Some(End::Refused(cause)) => {
                let adapter = self.reports.adapter;
                adapter.instances.record_refusal(
                    &adapter.binary,
                    self.reports.recipe.clone(),
                    cause,
                    super::plan::clock(),
                );
                self.fail(Verdict::Refused(cause));
            }
            Some(End::Protocol(why)) => self.fail(Verdict::Protocol(why)),
            Some(End::Overflow) => self.fail(Verdict::Overflow),
            // C2 §2: the retained terminal and outcome stand; the
            // connection's identity is broken, so no later turn runs.
            Some(End::MismatchAfterTerminal) => {
                latch(self.reports.health, DriverFailure::ResumeMismatch);
            }
        }
        observations
    }

    /// Packet §5: a session-cumulative cost below the session's last one
    /// warns; the reported value stands, and is the session's last.
    fn cost_reset(&self, terminal: &VendorTerminal) -> Option<Observation> {
        let usd = terminal.cost.as_ref()?.usd;
        let last = lock(self.reports.state).cost.replace(usd);
        last.filter(|last| usd < *last).map(|last| {
            Observation::Warning(Warning {
                code: COST_COUNTER_RESET,
                message: format!(
                    "the vendor's session-cumulative cost went down from {last} to {usd} USD; \
                     the reported value is kept"
                ),
                data: None,
            })
        })
    }

    /// A verdict before or beside the terminal: health latches at once
    /// (C2 §2), and the vendor is stopped with the AD19 order: Route's one
    /// interrupt now, the force rule after the cleanup allowance.
    fn fail(&mut self, verdict: Verdict) {
        let cause = match &verdict {
            Verdict::ResumeMismatch => DriverFailure::ResumeMismatch,
            Verdict::Refused(cause) => DriverFailure::Route(RouteError::Protocol {
                turn: self.turn,
                detail: refused(*cause),
            }),
            Verdict::Protocol(why) => DriverFailure::Route(RouteError::Protocol {
                turn: self.turn,
                detail: why,
            }),
            Verdict::Overflow => DriverFailure::ObservationOverflow,
            Verdict::Rejected(_) => return,
        };
        latch(self.reports.health, cause);
        self.verdict = Some(verdict);
        let now = tokio::time::Instant::now();
        let force_at = (now + CLEANUP_ALLOWANCE).min(self.wall.instant());
        self.abort.send_replace(Some(StopOrder {
            cause: StopCause::Protocol,
            // Route acts only on the times; Core never sees this order.
            requested_at: String::new(),
            // Claude's own abort: no provenance reader sees it (x.3.2 X4
            // D4.2).
            attached: now,
            force_at: Deadline::at(force_at),
            close_by: Deadline::at(force_at + CLEANUP_ALLOWANCE),
        }));
    }

    /// The turn's one result (C2 §4.1): a verdict is the first cause, with
    /// Route's process evidence; otherwise Route's outcome, failed
    /// `overflow` or `force_stopped` when its delivery did not finish. A
    /// terminal Route read that never reached the normalizer is normalized
    /// now for the retained terminal alone (AD4).
    fn turn_end(mut self, routed: ClaudeTurn, rest: &Rest) -> TurnEnd {
        let ClaudeTurn { outcome, result } = routed;
        if !self.saw_result
            && self.verdict.is_none()
            && let Some(result) = result
        {
            let batch = self
                .normalizer
                .message(Message::Result(result), tokio::time::Instant::now());
            if let Some(terminal) = batch.terminal {
                self.terminal = Some(terminal);
            }
        }
        let instance = self.normalizer.instance();
        let acknowledged = self.normalizer.acknowledged();
        let turn = self.turn;
        let failure = |cause| failure_of(cause, &outcome, acknowledged);
        // S1 rule 4: the daemon force decides the outcome, even beside a
        // verdict whose abort came first; health keeps the verdict's cause.
        let forced = matches!(rest, Rest::Forced)
            || matches!(&outcome, Err(failure) if matches!(failure.cause, RouteError::ForceStopped { .. }));
        if forced && self.verdict.is_some() {
            return TurnEnd {
                loss: None,
                terminal: self.terminal.map(|terminal| *terminal),
                instance,
                leftovers: None,
                outcome: Err(AdapterError::Route(failure(RouteError::ForceStopped {
                    turn,
                }))),
            };
        }
        let (terminal, outcome) = match self.verdict {
            Some(Verdict::Rejected(reason)) => {
                let evidence = evidence_of(&outcome);
                (None, Err(AdapterError::Rejected { reason, evidence }))
            }
            Some(Verdict::ResumeMismatch) => (
                None,
                Err(AdapterError::ResumeMismatch {
                    evidence: evidence_of(&outcome),
                }),
            ),
            Some(Verdict::Refused(cause)) => (
                None,
                Err(AdapterError::Route(failure(RouteError::Protocol {
                    turn,
                    detail: refused(cause),
                }))),
            ),
            Some(Verdict::Protocol(why)) => (
                self.terminal,
                Err(AdapterError::Route(failure(RouteError::Protocol {
                    turn,
                    detail: why,
                }))),
            ),
            Some(Verdict::Overflow) => (
                self.terminal,
                Err(AdapterError::Route(failure(RouteError::Overflow { turn }))),
            ),
            None => {
                let routed = match (&outcome, rest) {
                    (Err(failure), _) => Err(AdapterError::Route(RouteFailure {
                        acknowledged,
                        ..failure.clone()
                    })),
                    (Ok(result), Rest::Delivered) => Ok(TurnEvidence {
                        exit: exit_of(result),
                        cleanup: cleanup(result.cleanup),
                        journal_uncertain: result.journal_uncertain,
                    }),
                    (Ok(_), Rest::Undelivered) => {
                        Err(AdapterError::Route(failure(RouteError::Overflow { turn })))
                    }
                    (Ok(_), Rest::Forced) => {
                        Err(AdapterError::Route(failure(RouteError::ForceStopped {
                            turn,
                        })))
                    }
                };
                (self.terminal, routed)
            }
        };
        TurnEnd {
            loss: None,
            terminal: terminal.map(|terminal| *terminal),
            instance,
            leftovers: None,
            outcome,
        }
    }
}

/// The detail of a refused handshake: the start was written, so the turn
/// fails `protocol` with no resend (packet §3).
fn refused(cause: Incompatibility) -> &'static str {
    match cause {
        Incompatibility::FeatureAbsent(_) => "the vendor's handshake lacks a feature VIA relies on",
        Incompatibility::ReadbackDiffers(_) => {
            "the vendor's handshake readback differs from the launch"
        }
    }
}

/// The exit Host confirmed, if any.
fn exit_of(result: &ClaudeRouteResult) -> Option<via_routes::ExitReport> {
    (result.exit.code.is_some() || result.exit.signal.is_some()).then_some(result.exit)
}

/// Route's evidence of the turn, whatever its outcome.
fn evidence_of(outcome: &Result<ClaudeRouteResult, RouteFailure>) -> TurnEvidence {
    match outcome {
        Ok(result) => TurnEvidence {
            exit: exit_of(result),
            cleanup: cleanup(result.cleanup),
            journal_uncertain: result.journal_uncertain,
        },
        Err(failure) => TurnEvidence::of_failure(failure),
    }
}

/// A failure of `cause` with Route's evidence, whatever its outcome.
fn failure_of(
    cause: RouteError,
    outcome: &Result<ClaudeRouteResult, RouteFailure>,
    acknowledged: bool,
) -> RouteFailure {
    match outcome {
        Ok(result) => RouteFailure {
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
        },
        Err(failure) => RouteFailure {
            cause,
            acknowledged,
            ..failure.clone()
        },
    }
}

/// The failure a Route outcome latches in the health lane (C2 §2).
fn route_failure(turn: &ClaudeTurn) -> Option<DriverFailure> {
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
    route: ClaudeRoute,
    process: crate::PrivateProcessSpec,
    start: ClaudeStart,
    hop: via_routes::Hop<ClaudeItem>,
    signals: (Deadline, ForceWatch, StopWatch),
    /// The driver's close order and its verdict's abort order.
    stops: (
        watch::Receiver<Option<StopOrder>>,
        watch::Receiver<Option<StopOrder>>,
    ),
    cancel: CancellationToken,
    end: oneshot::Sender<ClaudeTurn>,
    reservation: Reservation,
    state: Arc<Mutex<DriverState>>,
    health: Arc<watch::Sender<DriverHealth>>,
    done: watch::Sender<Retiring>,
}

/// Runs the turn through Route with Core's stop order merged with the
/// driver's close, the verdict's abort and the session's cancellation.
/// Route's failure is latched in the health lane at once, whatever becomes
/// of `run_turn`; then the process's retirement is recorded.
async fn turn_task(task: TurnTask) {
    let TurnTask {
        route,
        process,
        start,
        hop,
        signals: (wall, force, core_stop),
        stops: (close, abort),
        cancel,
        end,
        reservation,
        state,
        health,
        done,
    } = task;
    let turn = start.turn();
    let driver_order = earliest(close.borrow().clone(), abort.borrow().clone());
    let (driver_stop, driver_stop_rx) = watch::channel(driver_order.clone());
    let (merged, merged_rx) = watch::channel(earliest(core_stop.borrow().clone(), driver_order));
    // Route reads the sources too where the relays below may lag: an
    // order set before the launch gate wins there (design §2 rule 1).
    let sources: StopSources = {
        let (core, close, abort, cancel) = (
            core_stop.clone(),
            close.clone(),
            abort.clone(),
            cancel.clone(),
        );
        Arc::new(move || {
            core.borrow().is_some()
                || close.borrow().is_some()
                || abort.borrow().is_some()
                || cancel.is_cancelled()
        })
    };
    let (routed, routed_rx) = oneshot::channel();
    let route_turn = route.turn(
        process,
        start,
        hop,
        (wall, force, (merged_rx, sources)),
        routed,
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
    // The driver's two orders never end: no cancellation of their own.
    let uncancelled = CancellationToken::new();
    let retirement = tokio::select! {
        (retirement, ()) = async { tokio::join!(route_turn, relay) } => retirement,
        never = merge_stops(close, abort, &uncancelled, &driver_stop) => match never {},
        never = merge_stops(core_stop, driver_stop_rx, &cancel, &merged) => match never {},
    };
    reservation.retired(retirement);
    drop(reservation);
    done.send_replace(Retiring::CleanedUp);
    done.send_replace(Retiring::Delivered);
}
