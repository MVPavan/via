use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc, watch};
use tokio::time::timeout_at;

use crate::{
    AcceptanceToken, Cleanup, Deadline, FakeAcceptanceObservation, FakeConfig, FakeObservation,
    FakeTerminalEvidence, Observation, ProcessOwner, ProgressMarks, ReprobeReport, RouteError,
    RouteFailure, RuntimeConfig, RuntimeResources, SessionId, StopWatch, TurnActivity, TurnNumber,
    VendorTerminalStatus, VendorTurnId, final_text_pieces,
};
use via_routes::{
    FakeMessage, FakeRoute, FakeRouteResult, RouteMessage, TerminalStatus, TurnStart, WireRecovery,
};

/// C2 A1: the observation channel holds at most 1,024 items ...
pub const OBSERVATION_ITEMS: usize = 1024;

/// ... and at most 4 MiB of them, counted by [`item_cost`].
pub const OBSERVATION_BYTES: usize = 4 * 1024 * 1024;

/// C2 A1: a delivery blocked this long without an item accepted fails the
/// turn `overflow`.
const EVENT_STALL: Duration = Duration::from_secs(10);

/// The stall bound: 10 s. Test builds only: `VIA_TEST_EVENT_STALL_MS`
/// lowers it (Task 4 design §13.1).
fn event_stall() -> Duration {
    #[cfg(feature = "test-failpoints")]
    if let Some(lowered) = std::env::var("VIA_TEST_EVENT_STALL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(lowered);
    }
    EVENT_STALL
}

/// How the Adapter's delivery after Route ended went.
enum Rest {
    /// Everything Route handed over reached Core.
    Delivered,
    /// A delivery failed: Core stalled or went away.
    Undelivered,
    /// The daemon force ended a delivery that had to wait.
    Forced,
}

/// One observation in Core's channel with its share of the drive's byte
/// budget (Task 4 design §2.3): Core holds `permit` until it has handled
/// the observation.
pub struct AdmittedObservation {
    /// The normalized observation.
    pub observation: FakeObservation,
    /// The item's bytes of the drive's 4 MiB budget.
    pub permit: OwnedSemaphorePermit,
}

/// The sending side of one drive's observation channel and its byte
/// budget; Core keeps the receiver.
#[derive(Clone)]
pub struct ObservationSink {
    sender: mpsc::Sender<AdmittedObservation>,
    budget: Arc<Semaphore>,
}

/// One drive's observation channel: 1,024 items and a 4 MiB byte budget
/// (C2 A1, Task 4 design §2.3).
pub fn observation_channel() -> (ObservationSink, mpsc::Receiver<AdmittedObservation>) {
    let (sender, receiver) = mpsc::channel(OBSERVATION_ITEMS);
    let budget = Arc::new(Semaphore::new(OBSERVATION_BYTES));
    (ObservationSink { sender, budget }, receiver)
}

/// Immutable fake deployment and Host paths supplied at daemon bootstrap.
pub struct AdapterRuntimeConfig {
    /// Opaque Wire deployment paths forwarded unopened through Route.
    pub runtime: RuntimeConfig,
    /// Fake-only fixture launch configuration.
    pub fake: FakeConfig,
}

/// Adapter construction or fake-drive failure without handle or prompt text.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// Lower protocol or process boundary failed, with its typed cause and evidence.
    #[error("fake route failed: {0}")]
    Route(#[from] RouteFailure),
    /// Lower runtime could not initialize.
    #[error("adapter runtime failed: {0}")]
    Open(#[from] via_routes::WireError),
    /// Fake launch settings are unavailable.
    #[error("fake route is unavailable")]
    Unavailable,
    /// A typed fake request or observation could not be represented.
    #[error("fake protocol identity is invalid")]
    Protocol,
}

impl AdapterError {
    /// Durable Store state could not be read or written; other failures leave
    /// evidence unproven without making Store unusable.
    pub fn is_store_failure(&self) -> bool {
        matches!(self, Self::Open(error) if error.is_store_failure())
    }

    /// A Host journal write had an uncertain outcome: the daemon latches
    /// (design §7.2 row 12).
    pub fn journal_uncertain(&self) -> bool {
        matches!(self, Self::Open(error) if error.journal_uncertain())
    }
}

/// Passive recovery facts for Core's later crash reconciliation.
pub struct FakeRecovery {
    /// Owning VIA session.
    pub session_id: SessionId,
    /// Opaque committed anchor identifier.
    pub anchor_id: String,
    /// Opaque launch generation.
    pub generation: String,
    /// Owning turn.
    pub turn: TurnNumber,
    /// Cleanup certainty under Host's validated group.
    pub cleanup: Cleanup,
    /// Host stopped the group while its vendor was live (Host force evidence).
    pub forced: bool,
}

/// Fake Adapter with an opaque Route/Wire runtime and immutable fixture policy.
pub struct AdapterRuntime {
    route: FakeRoute,
    fake: FakeConfig,
}

impl AdapterRuntime {
    /// Forwards the unopened Store resource bundle to Route and Wire.
    pub fn new(
        config: AdapterRuntimeConfig,
        resources: RuntimeResources,
    ) -> Result<Self, AdapterError> {
        let route = FakeRoute::new(config.runtime, resources)?;
        Ok(Self {
            route,
            fake: config.fake,
        })
    }

    /// Whether this daemon has the explicitly configured fake executable and fixture.
    pub fn fake_available(&self) -> bool {
        self.fake.is_available()
    }

    /// The fake route's default session working directory (design §11.1).
    pub fn fake_cwd(&self) -> &std::path::Path {
        self.fake.default_cwd()
    }

    /// Runs one submitted fake turn and delivers every observation to Core
    /// in decode order (Task 4 design §2.3, §9). Route hands one message at
    /// a time over a hop of one; its delivery acquires the items' bytes of
    /// the drive's budget, then sends each item, while `route` keeps being
    /// polled. A delivery blocked for the stall bound without an item
    /// accepted drops the hop's receiver, so Route fails the turn as
    /// overflow and still performs its cleanup. Each message's arrival
    /// moves `activity` (design §2.4), unknown types included. `force` set force-closes the
    /// turn through Route (C2 Close(Force)). `stop` is the turn's stop
    /// order, passed through to Route (design §2). The agent runs in the
    /// session's frozen `cwd` (design §11.1).
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is a distinct input of the one turn"
    )]
    pub async fn execute(
        &self,
        session_id: SessionId,
        turn: TurnNumber,
        (prompt, cwd): (String, std::path::PathBuf),
        observations: ObservationSink,
        activity: TurnActivity,
        deadline: Deadline,
        force: watch::Receiver<Option<tokio::time::Instant>>,
        stop: StopWatch,
        capacity: via_routes::CapacityToken,
    ) -> Result<FakeTerminalEvidence, AdapterError> {
        let owner = ProcessOwner {
            session_id: session_id.clone(),
            turn,
        };
        let mut process = self
            .fake
            .process_spec(owner, &cwd)
            .map_err(|_| AdapterError::Unavailable)?;
        // Host owns the connection slot for the group's life (design §11).
        process.capacity = Some(capacity);
        let start = TurnStart::new(session_id.as_str().to_owned(), turn, prompt)
            .map_err(|_| AdapterError::Protocol)?;
        let (hop, hop_rx) = mpsc::channel::<RouteMessage>(1);
        let mut forced_stop = force.clone();
        let route = self
            .route
            .execute(process, start, hop, deadline, force, stop);
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
                        // Route observes the closed hop as overflow, or as
                        // the force's stop under a force.
                        hop_rx = None;
                        delivered = false;
                    }
                }
                message = recv(hop_rx.as_mut()), if delivery.is_none() && hop_rx.is_some() => {
                    match message {
                        Some(message) => {
                            let at = tokio::time::Instant::now();
                            activity.record(at);
                            delivery = Some(Box::pin(deliver(message, at, observations.clone(), stall)));
                        }
                        None => hop_rx = None,
                    }
                }
                result = &mut route => break result,
            }
        };
        // Route ended: what it already handed over is still delivered. The
        // daemon force ends only a wait: data deliverable at once still goes.
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
                    if deliver(message, at, observations.clone(), stall)
                        .await
                        .is_err()
                    {
                        return Rest::Undelivered;
                    }
                }
            }
            Rest::Delivered
        };
        let rest = if delivered {
            tokio::select! {
                biased;
                rest = rest => rest,
                () = forced(&mut forced_stop) => Rest::Forced,
            }
        } else {
            Rest::Undelivered
        };
        // A route failure is the first cause. Undelivered data fails a
        // success as `Overflow`, and the daemon force that ended the
        // delivery as `ForceStopped` (design §2 rule 4); either keeps
        // Route's exit and close evidence.
        let result = match result {
            Ok(result) => result,
            Err(failure) => return Err(AdapterError::Route(failure)),
        };
        let cause = match rest {
            Rest::Delivered => return Ok(normalize_terminal(result)),
            Rest::Undelivered => RouteError::Overflow { turn },
            Rest::Forced => RouteError::ForceStopped { turn },
        };
        let exit = result.exit;
        Err(AdapterError::Route(RouteFailure {
            cause,
            undecoded: None,
            exit: (exit.code.is_some() || exit.signal.is_some()).then_some(exit),
            launched: true,
            cleanup: Some(result.cleanup),
            forced: result.forced,
            journal_uncertain: result.journal_uncertain,
        }))
    }

    /// Drains lower process owners before Store shutdown and returns passive facts.
    pub async fn shutdown(
        &self,
        deadline: Deadline,
        turns: &[(SessionId, TurnNumber)],
    ) -> FakeShutdown {
        let report = self.route.shutdown(deadline, turns).await;
        FakeShutdown {
            recovery: report
                .recovery
                .into_iter()
                .map(|turn| FakeTurnRecovery {
                    session_id: turn.owner_session,
                    turn: turn.owner_turn,
                    cleanup: match turn.cleanup {
                        via_routes::WireCleanup::Quiescent => Cleanup::Quiescent,
                        via_routes::WireCleanup::Uncertain => Cleanup::Uncertain,
                    },
                    forced: turn.forced,
                })
                .collect(),
            anchors: report.anchors,
            uncertain_anchors: report.uncertain_anchors,
            pending_tasks: report.pending_tasks,
            failed_tasks: report.failed_tasks,
            failure: report.failure,
        }
    }

    /// Hands Host capacity for a group it did not launch, such as one an
    /// earlier daemon left unproved (design §11).
    pub fn hold_capacity(
        &self,
        anchor_id: String,
        owner: SessionId,
        token: via_routes::CapacityToken,
    ) {
        self.route.hold_capacity(anchor_id, owner, token);
    }

    /// One non-signalling re-probe pass over held groups, optionally only
    /// one session's (design §8, and §4's bounded absence check before
    /// `Closed`). An uncertain proof commit is an error, which latches.
    pub async fn reprobe_held(
        &self,
        deadline: Deadline,
        owner: Option<SessionId>,
    ) -> Result<ReprobeReport, AdapterError> {
        self.route
            .reprobe_held(deadline, owner)
            .await
            .map_err(AdapterError::Open)
    }

    /// Held groups no live control owns: `connections.held_unproven`'s Host
    /// part (design §6.6).
    pub fn held_unproven(&self) -> usize {
        self.route.held_unproven()
    }

    /// Advances on every added holding: the re-probe loop resets its
    /// backoff when it changes (design §8).
    pub fn holdings_changed(&self) -> watch::Receiver<u64> {
        self.route.holdings_changed()
    }

    /// Positive evidence that a vendor of one of `anchors` is live (Task 4
    /// design §11.3 `process.alive`).
    pub fn live_armed(&self, anchors: &[String]) -> bool {
        self.route.live_armed(anchors)
    }

    /// Groups whose cleanup a live control or acquisition still owns, which
    /// block idle exit (design §6.4).
    pub fn pending_cleanup(&self) -> usize {
        self.route.pending_cleanup()
    }

    /// Subscribes Host's early-stop task to the daemon force signal (design
    /// §6.8), which carries the instant the force was raised (`None` until
    /// then); call once, from within the daemon's runtime.
    pub fn watch_force(&self, forced: watch::Receiver<Option<tokio::time::Instant>>) {
        self.route.watch_force(forced);
    }

    /// Recovers one page of committed anchors, up to `limit` after the
    /// `after` id, without giving Core process signalling authority.
    pub async fn recover_page(
        &self,
        after: Option<String>,
        limit: u32,
        deadline: Deadline,
    ) -> Result<Vec<FakeRecovery>, AdapterError> {
        self.route
            .recover_page(after, limit, deadline)
            .await
            .map(|reports| reports.into_iter().map(normalize_recovery).collect())
            .map_err(AdapterError::Open)
    }

    /// [`Self::recover_page`] of the anchors in `cohort` only: resumed
    /// paging never challenges an anchor committed after startup.
    pub async fn recover_cohort_page(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: via_routes::AnchorCohort,
        deadline: Deadline,
    ) -> Result<Vec<FakeRecovery>, AdapterError> {
        self.route
            .recover_cohort_page(after, limit, cohort, deadline)
            .await
            .map(|reports| reports.into_iter().map(normalize_recovery).collect())
            .map_err(AdapterError::Open)
    }
}

/// Resolves once `force` is set; never when its sender is gone unset.
async fn forced(force: &mut watch::Receiver<Option<tokio::time::Instant>>) {
    if force.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// A pending delivery, polled beside `route` (Task 4 design §9).
type Delivery = Pin<Box<dyn Future<Output = Result<(), Undelivered>> + Send>>;

/// A delivery that did not complete: stalled, or Core's channel is gone.
struct Undelivered;

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

/// An item's cost against the byte budget: `512 + Σ(64 + len)` over its
/// strings (Task 4 design §2.3).
fn item_cost(strings: &[&str]) -> usize {
    512 + strings
        .iter()
        .map(|string| 64 + string.len())
        .sum::<usize>()
}

/// Delivers one Route message (design §2.3): builds its observations and
/// sends each in order. The terminal message gives its completed final
/// text as `final_text` pieces, all before the route result, which is the
/// vendor terminal.
async fn deliver(
    message: RouteMessage,
    at: tokio::time::Instant,
    sink: ObservationSink,
    stall: Duration,
) -> Result<(), Undelivered> {
    if let FakeMessage::Terminal { final_text, .. } = &message.payload {
        for piece in final_text_pieces(final_text) {
            let observation = FakeObservation::Data {
                observation: Observation::FinalText(piece.to_owned()),
            };
            send(observation, &sink, stall).await?;
        }
        return Ok(());
    }
    let Some(observation) = normalize(message, at)? else {
        return Ok(());
    };
    send(observation, &sink, stall).await
}

/// Sends one observation: acquires the item's byte cost, then sends it
/// with the permit. Each item owns one stall deadline, set at its first
/// block, so the bound restarts once an item is accepted; at the deadline
/// it gives up.
async fn send(
    observation: FakeObservation,
    sink: &ObservationSink,
    stall: Duration,
) -> Result<(), Undelivered> {
    let mut stall_at = None;
    let wanted = u32::try_from(observation_cost(&observation)).map_err(|_| Undelivered)?;
    let permit = match Arc::clone(&sink.budget).try_acquire_many_owned(wanted) {
        Ok(permit) => permit,
        Err(TryAcquireError::NoPermits) => {
            let at = *stall_at.get_or_insert_with(|| tokio::time::Instant::now() + stall);
            timeout_at(at, Arc::clone(&sink.budget).acquire_many_owned(wanted))
                .await
                .map_err(|_| Undelivered)?
                .map_err(|_| Undelivered)?
        }
        Err(TryAcquireError::Closed) => return Err(Undelivered),
    };
    let item = AdmittedObservation {
        observation,
        permit,
    };
    match sink.sender.try_send(item) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(item)) => {
            let at = *stall_at.get_or_insert_with(|| tokio::time::Instant::now() + stall);
            timeout_at(at, sink.sender.send(item))
                .await
                .map_err(|_| Undelivered)?
                .map_err(|_| Undelivered)
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Err(Undelivered),
    }
}

/// Maps one decoded fake message to its C2 observation, if any (design
/// §2.3, §2.5): one item at most, only for an acceptance or a message with
/// a mark. Unknown messages move only the activity clock.
fn normalize(
    message: RouteMessage,
    at: tokio::time::Instant,
) -> Result<Option<FakeObservation>, Undelivered> {
    let marks = |marks: ProgressMarks| {
        Some(FakeObservation::Data {
            observation: Observation::Progress(marks),
        })
    };
    let empty = ProgressMarks {
        at,
        model: false,
        tools_started: Vec::new(),
        tools_ended: Vec::new(),
        usage: None,
    };
    Ok(match message.payload {
        // Route admits exactly one acceptance per turn.
        FakeMessage::Accepted { vendor_turn_id } => {
            Some(FakeObservation::Accepted(FakeAcceptanceObservation {
                correlation: AcceptanceToken::try_from(1).map_err(|_| Undelivered)?,
                vendor_turn_id: VendorTurnId::try_from(vendor_turn_id).map_err(|_| Undelivered)?,
            }))
        }
        FakeMessage::Text { .. } => marks(ProgressMarks {
            model: true,
            ..empty
        }),
        FakeMessage::ToolStarted { tool_id, name, .. } => marks(ProgressMarks {
            tools_started: vec![(tool_id, name)],
            ..empty
        }),
        FakeMessage::ToolEnded { tool_id, .. } => marks(ProgressMarks {
            tools_ended: vec![tool_id],
            ..empty
        }),
        FakeMessage::Usage { total_tokens, .. } => marks(ProgressMarks {
            usage: Some((None, total_tokens)),
            ..empty
        }),
        // Route rejects interrupt acknowledgements; the terminal's final
        // text is sent by `deliver`, its status is the route result; an
        // unknown message is activity only. The C2 lane's messages have no
        // legacy observation.
        FakeMessage::Terminal { .. }
        | FakeMessage::InterruptAck { .. }
        | FakeMessage::Hello(_)
        | FakeMessage::Identity { .. }
        | FakeMessage::Denial { .. }
        | FakeMessage::Decline { .. }
        | FakeMessage::SteerDelivered { .. }
        | FakeMessage::VendorClosed { .. }
        | FakeMessage::Unknown { .. } => None,
    })
}

/// The byte cost of an observation: its strings (design §2.3).
fn observation_cost(observation: &FakeObservation) -> usize {
    match observation {
        FakeObservation::Accepted(accepted) => item_cost(&[accepted.vendor_turn_id.as_str()]),
        FakeObservation::Data {
            observation: Observation::Progress(marks),
        } => {
            let mut strings: Vec<&str> = Vec::new();
            for (id, name) in &marks.tools_started {
                strings.push(id);
                strings.push(name);
            }
            strings.extend(marks.tools_ended.iter().map(String::as_str));
            if let Some((Some(key), _)) = &marks.usage {
                strings.push(key);
            }
            item_cost(&strings)
        }
        FakeObservation::Data {
            observation: Observation::FinalText(text),
        } => item_cost(&[text]),
    }
}

fn normalize_terminal(result: FakeRouteResult) -> FakeTerminalEvidence {
    let status = match result.status {
        TerminalStatus::Completed => VendorTerminalStatus::Completed,
        TerminalStatus::Interrupted => VendorTerminalStatus::Interrupted,
        TerminalStatus::Failed => VendorTerminalStatus::Failed,
    };
    FakeTerminalEvidence {
        status,
        stop_reason: result.stop_reason,
        vendor_code: result.vendor_code,
        exit: result.exit,
        cleanup: match result.cleanup {
            via_routes::WireCleanup::Quiescent => Cleanup::Quiescent,
            via_routes::WireCleanup::Uncertain => Cleanup::Uncertain,
        },
        journal_uncertain: result.journal_uncertain,
    }
}

fn normalize_recovery(report: WireRecovery) -> FakeRecovery {
    FakeRecovery {
        session_id: report.owner_session,
        anchor_id: report.anchor_id,
        generation: report.generation,
        turn: report.owner_turn,
        cleanup: match report.cleanup {
            via_routes::WireCleanup::Quiescent => Cleanup::Quiescent,
            via_routes::WireCleanup::Uncertain => Cleanup::Uncertain,
        },
        forced: report.forced,
    }
}

/// Passive per-turn shutdown recovery facts.
pub struct FakeTurnRecovery {
    /// Owning VIA session.
    pub session_id: SessionId,
    /// Owning turn.
    pub turn: TurnNumber,
    /// Quiescent only when every anchor of the turn was proved absent.
    pub cleanup: Cleanup,
    /// Host stopped a group of the turn while its vendor was live.
    pub forced: bool,
}

/// Passive shutdown status; no Host operation or signal handle escapes Adapter.
pub struct FakeShutdown {
    /// Per-turn recovery facts for the requested turns.
    pub recovery: Vec<FakeTurnRecovery>,
    /// Committed anchors reconciled.
    pub anchors: usize,
    /// Reconciled anchors without positive absence proof.
    pub uncertain_anchors: usize,
    /// Host tasks still pending at the shutdown deadline.
    pub pending_tasks: usize,
    /// Host tasks that panicked, were cancelled or failed their child wait.
    pub failed_tasks: usize,
    /// Bounded description of the deadline, Store or recovery failure, if any.
    pub failure: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        FakeMessage, OBSERVATION_BYTES, OBSERVATION_ITEMS, RouteMessage, deliver,
        observation_channel,
    };
    use std::time::Duration;

    fn tool(name: &str) -> RouteMessage {
        RouteMessage {
            payload: FakeMessage::ToolStarted {
                vendor_turn_id: "fake-turn-1".to_owned(),
                tool_id: "t".to_owned(),
                name: name.to_owned(),
            },
        }
    }

    /// Design §2.3 Bounds: an item costs `512 + Σ(64 + len)` of the drive's
    /// 4 MiB; with the receiver never drained, large items fill the budget
    /// well before 1,024 items and the next delivery stays blocked until
    /// the stall. The fake route's short fields (at most 1 KiB) cannot
    /// reach 4 MiB within 1,024 items, so the byte bound is checked here.
    #[test]
    fn the_byte_budget_admits_items_to_4_mib_then_the_next_stalls() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let name = "n".repeat(100 * 1024);
        let cost = 512 + (64 + 1) + (64 + name.len());
        let fits = OBSERVATION_BYTES / cost;
        assert!(fits < OBSERVATION_ITEMS);
        let (sink, receiver) = observation_channel();
        runtime.block_on(async {
            let stall = Duration::from_millis(50);
            for _ in 0..fits {
                let at = tokio::time::Instant::now();
                assert!(deliver(tool(&name), at, sink.clone(), stall).await.is_ok());
            }
            assert_eq!(receiver.len(), fits);
            let at = tokio::time::Instant::now();
            assert!(deliver(tool(&name), at, sink.clone(), stall).await.is_err());
            assert_eq!(receiver.len(), fits);
            // A small item still fits what is left.
            assert!(deliver(tool("n"), at, sink, stall).await.is_ok());
            assert_eq!(receiver.len(), fits + 1);
        });
    }
}
