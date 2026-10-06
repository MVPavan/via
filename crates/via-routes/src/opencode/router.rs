//! Generation-owned `OpenCode` routing and admission (`opencode.md` §§7.1–7.2, §9).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;
use via_wire::TurnNumber;

use crate::{Deadline, DecodeWatermark};

pub use super::bounds::StagingPermit;
use super::bounds::{self, Failure, Resource, Retained, Staging};
use super::declines::{CleanupWork, DeclineNotice, DeclineWork};
use super::events::{DecodeError, Event, EventData, ExecutionKind, InboxKind, StepKind, ToolKind};
use super::state::{InputPhase, InputState, SessionState};

mod interactive;

/// Per-session ingress item limit (`opencode.md` §9).
pub const LANE_MESSAGES: usize = 16;
// Per-session ingress retained-byte limit (`opencode.md` §9).
const LANE_BYTES: usize = 1024 * 1024;
/// A6 never-ask response budget from the original decode (`opencode.md` §11).
pub const DECLINE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct RejectionWindow {
    live: bool,
    earliest: Option<u64>,
    count: u64,
    wall: Option<Deadline>,
    inbox_cancel: bool,
    interrupt: bool,
    response_limit: Option<Instant>,
}

/// Session-local delivery accounting; inactive windows remain turn tombstones.
#[derive(Default)]
struct Rejections(Mutex<HashMap<TurnNumber, RejectionWindow>>);

impl Rejections {
    fn register(&self, owner: TurnNumber) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                owner,
                RejectionWindow {
                    live: true,
                    ..RejectionWindow::default()
                },
            );
    }

    fn settle(&self, owner: TurnNumber) {
        if let Some(window) = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&owner)
        {
            window.live = false;
        }
    }

    fn record(&self, owner: Option<TurnNumber>, order: u64) {
        let mut windows = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for (turn, window) in windows.iter_mut() {
            if owner.is_some_and(|owner| owner != *turn) || (owner.is_none() && !window.live) {
                continue;
            }
            // Known tombstoned owners still count as rejected late observations,
            // but only a live owner's observation can taint its usage window.
            window.count = window.count.saturating_add(1);
            if window.live {
                window.earliest = Some(
                    window
                        .earliest
                        .map_or(order, |earliest| earliest.min(order)),
                );
            }
            tracing::trace!(
                turn = ?turn,
                read_order = order,
                rejected_observations = window.count,
                "OpenCode observation was not delivered"
            );
        }
    }

    fn is_live(&self, owner: TurnNumber) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&owner)
            .is_some_and(|window| window.live)
    }

    fn earliest(&self, owner: TurnNumber) -> Option<u64> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&owner)?
            .earliest
    }

    fn record_response_limit(&self, owner: TurnNumber, at: Instant) {
        if let Some(window) = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&owner)
        {
            window.response_limit = Some(window.response_limit.map_or(at, |first| first.min(at)));
        }
    }

    fn response_limit(&self, owner: TurnNumber) -> Option<Instant> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&owner)?
            .response_limit
    }
}

/// One decoded event attributed before it is put into a lane.
#[derive(Clone, Debug)]
pub struct Routed {
    /// Typed payload, without echoed prompts or vendor error text.
    pub event: Event,
    /// Correlated VIA turn, including tombstones.
    pub owner: Option<TurnNumber>,
    /// Global stream/marker read order within this generation.
    pub read_order: u64,
    /// Admitted position in this owner's decode watermark, zero before tracking.
    pub position: u64,
    /// Actual event read instant; queuing never moves the idle deadline.
    pub decoded_at: Instant,
    /// The owner had already settled when this event was decoded.
    pub late: bool,
    /// Started assistant IDs in stream order from the unowned execution
    /// this input joined. Supplied only on its first owned inbox delivery;
    /// these are trusted step identity facts, never replayed observations.
    pub joined_steps: Vec<String>,
    /// Shared staging charge retained until every delivery copy is dropped (§9).
    pub staging: Option<StagingPermit>,
}

/// Entries share one ordered ingress path, including HTTP acceptance.
#[derive(Clone, Debug)]
pub enum LaneItem {
    /// SSE event.
    Event(Box<Routed>),
    /// A §9 HTTP-limit breach, affecting only the request's original turn (§8).
    ResponseLimit {
        /// Original caller turn, including a retained tombstone.
        owner: TurnNumber,
        /// Shared read-order position.
        read_order: u64,
        /// Core activity decode fence position.
        position: u64,
        /// The instant VIA detected the response limit.
        decoded_at: Instant,
        /// Shared retained-item budget (§9).
        staging: Option<StagingPermit>,
    },
    /// Inconclusive prompt response, ordered after all earlier ingress (§8).
    Inconclusive {
        /// Original caller turn.
        owner: TurnNumber,
        /// Shared read-order position.
        read_order: u64,
        /// Core activity decode fence position.
        position: u64,
        /// Response read instant.
        decoded_at: Instant,
        /// Shared retained-item budget (§9).
        staging: Option<StagingPermit>,
    },
    /// Never-ask reply result credited to its original owner (§11).
    Decline {
        /// Original caller turn, including a tombstone.
        owner: TurnNumber,
        /// Sanitized decline result.
        notice: Box<DeclineNotice>,
        /// Shared read-order position.
        read_order: u64,
        /// Core activity decode fence position.
        position: u64,
        /// Response read instant.
        decoded_at: Instant,
        /// Shared retained-item budget (§9).
        staging: Option<StagingPermit>,
    },
    /// Validated HTTP acceptance, deduplicated against inbox acceptance.
    Accepted {
        /// VIA turn.
        owner: TurnNumber,
        /// Caller input key.
        input_id: String,
        /// Marker's order under the router lock.
        read_order: u64,
        /// Admitted position in this owner's decode watermark, zero before tracking.
        position: u64,
        /// HTTP acceptance read instant.
        decoded_at: Instant,
        /// Shared retained-item budget (§9).
        staging: Option<StagingPermit>,
    },
}

/// Sticky session ingress failure; previously queued terminals remain readable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneFailure {
    /// Malformed known payload or non-increasing durable sequence.
    Protocol,
    /// A nonblocking ingress insertion failed.
    Overflow,
}

/// A malformed generation envelope cannot be attributed safely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouterFailure {
    /// A generation-wide retained-state or route-staging budget was exhausted (§9).
    Overflow,
    /// Protocol failure requiring generation loss/drain by its owner.
    Protocol,
}

#[derive(Default)]
struct Queue {
    items: VecDeque<(LaneItem, usize)>,
    bytes: usize,
    failure: Option<LaneFailure>,
    closed: bool,
    tracked: HashMap<TurnNumber, DecodeWatermark>,
}

/// A bounded lane; the SSE reader never waits for a driver.
pub struct Lane {
    queue: Mutex<Queue>,
    ready: Notify,
    rejections: Arc<Rejections>,
    staging: Arc<Staging>,
}

impl Lane {
    fn new(rejections: Arc<Rejections>, staging: Arc<Staging>) -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            ready: Notify::new(),
            rejections,
            staging,
        }
    }

    /// Associates admitted messages with a turn's Core activity decode fence.
    pub fn track(&self, owner: TurnNumber, watermark: DecodeWatermark) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tracked
            .insert(owner, watermark);
    }

    /// Takes the next item without waiting.
    pub fn pop(&self) -> Option<LaneItem> {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let (item, bytes) = queue.items.pop_front()?;
        queue.bytes = queue.bytes.saturating_sub(bytes);
        Some(item)
    }

    fn push(&self, mut item: LaneItem, bytes: usize, order: u64) {
        let owner = match &item {
            LaneItem::Event(routed) => routed.owner,
            LaneItem::Accepted { owner, .. }
            | LaneItem::ResponseLimit { owner, .. }
            | LaneItem::Inconclusive { owner, .. }
            | LaneItem::Decline { owner, .. } => Some(*owner),
        };
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        // Full ingress never blocks the SSE reader. The queue retains any
        // terminal admitted before failure; the refused order taints usage.
        if queue.closed || queue.failure.is_some() {
            self.rejections.record(owner, order);
            return;
        }
        if queue.items.len() >= LANE_MESSAGES || queue.bytes.saturating_add(bytes) > LANE_BYTES {
            queue.failure = Some(LaneFailure::Overflow);
            self.rejections.record(owner, order);
            drop(queue);
            self.ready.notify_waiters();
            return;
        }
        let Some(permit) = self.staging.reserve(bytes) else {
            self.rejections.record(owner, order);
            return;
        };
        match &mut item {
            LaneItem::Event(routed) => {
                routed.staging = Some(permit);
                if let Some(watermark) = routed.owner.and_then(|owner| queue.tracked.get(&owner)) {
                    routed.position = watermark.advance();
                }
            }
            LaneItem::Accepted {
                owner,
                position,
                staging,
                ..
            }
            | LaneItem::ResponseLimit {
                owner,
                position,
                staging,
                ..
            }
            | LaneItem::Inconclusive {
                owner,
                position,
                staging,
                ..
            }
            | LaneItem::Decline {
                owner,
                position,
                staging,
                ..
            } => {
                *staging = Some(permit);
                if let Some(watermark) = queue.tracked.get(owner) {
                    *position = watermark.advance();
                }
            }
        }
        queue.bytes += bytes;
        queue.items.push_back((item, bytes));
        drop(queue);
        self.ready.notify_one();
    }

    fn fail(&self, failure: LaneFailure, order: u64, owner: Option<TurnNumber>) {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if queue.failure.is_none() {
            queue.failure = Some(failure);
        }
        self.rejections.record(owner, order);
        drop(queue);
        self.ready.notify_waiters();
    }

    /// Waits for ordered data; after failure/close, drains queued entries first.
    pub async fn recv(&self) -> Option<LaneItem> {
        loop {
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(item) = self.pop() {
                return Some(item);
            }
            let ended = {
                let queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
                queue.closed || queue.failure.is_some() || self.staging.failure.get().is_some()
            };
            if ended {
                return None;
            }
            notified.await;
        }
    }

    /// First sticky ingress failure.
    pub fn failure(&self) -> Option<LaneFailure> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .failure
            .or_else(|| self.staging.failure.get().map(lane_failure))
    }

    /// Earliest rejected read order in this turn's live window (`opencode.md` §12; C2 AD6).
    pub fn earliest_unobserved(&self, owner: TurnNumber) -> Option<u64> {
        self.rejections.earliest(owner)
    }

    /// Positive §9 HTTP-limit evidence for this retained owner, independent of lane capacity (§8).
    pub fn response_limit(&self, owner: TurnNumber) -> Option<Instant> {
        self.rejections.response_limit(owner)
    }

    /// Counts a dropped observation without tainting unrelated or future turns (§7.1, C2 AD6).
    /// An unknown owner conservatively taints only this session's currently live turns.
    pub fn reject_unobserved(&self, owner: Option<TurnNumber>, order: u64) {
        self.rejections.record(owner, order);
    }

    /// Ends delivery, retaining items already admitted before the cutoff.
    pub fn close(&self) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
        self.ready.notify_waiters();
    }
}

#[derive(Default)]
struct Session {
    opened: bool,
    state: SessionState,
    lane: Option<Arc<Lane>>,
    inputs: HashMap<String, TurnNumber>,
    messages: HashMap<String, TurnNumber>,
    calls: HashMap<String, TurnNumber>,
    unowned_messages: HashSet<String>,
    unowned_step_order: Vec<String>,
    unowned_calls: HashSet<String>,
    compactions: HashMap<String, TurnNumber>,
    accepted: HashSet<TurnNumber>,
    delivered: HashSet<TurnNumber>,
    rejections: Arc<Rejections>,
    sent: HashSet<TurnNumber>,
    settled: HashSet<TurnNumber>,
    interactive: HashMap<(super::events::InteractiveKind, String), TurnNumber>,
    cleanup_expected: HashSet<String>,
}

/// One server generation's state; its owner serializes HTTP markers and stream decode.
pub struct Router {
    sessions: HashMap<String, Session>,
    children: HashMap<String, (String, TurnNumber)>,
    order: u64,
    changed: Arc<Notify>,
    retained: Retained,
    failure: Arc<Failure>,
    staging: Arc<Staging>,
    pending_interactive: HashMap<(String, super::events::InteractiveKind, String), DeclineWork>,
    decline_queue: VecDeque<DeclineWork>,
    cleanup_queue: VecDeque<CleanupWork>,
    needs_drain: AtomicBool,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    /// Empty state for a new generation.
    pub fn new() -> Self {
        let changed = Arc::new(Notify::new());
        let failure = Arc::new(Failure::new(changed.clone()));
        let staging = Arc::new(Staging::new(failure.clone()));
        Self {
            sessions: HashMap::new(),
            children: HashMap::new(),
            order: 0,
            changed,
            retained: Retained::default(),
            failure,
            staging,
            pending_interactive: HashMap::new(),
            decline_queue: VecDeque::new(),
            cleanup_queue: VecDeque::new(),
            needs_drain: AtomicBool::new(false),
        }
    }

    /// Sticky generation budget/protocol failure (`opencode.md` §9).
    pub fn failure(&self) -> Option<RouterFailure> {
        self.failure.get()
    }

    /// Lost execution state requires sticky generation drain (§7.1, §8).
    pub fn needs_drain(&self) -> bool {
        self.needs_drain.load(Ordering::Acquire)
    }

    fn reserve(&mut self, resource: Resource) -> bool {
        if self.failure().is_some() {
            return false;
        }
        match self.retained.reserve(resource) {
            Ok(()) => true,
            Err(cause) => {
                self.failure.set(cause);
                false
            }
        }
    }

    fn key(&mut self, id: &str) -> bool {
        if self.failure().is_some() {
            return false;
        }
        match self.retained.key(id) {
            Ok(()) => true,
            Err(cause) => {
                self.failure.set(cause);
                false
            }
        }
    }

    fn ensure_session(&mut self, id: &str) -> bool {
        if id.len() > bounds::ID_BYTES {
            self.failure.set(RouterFailure::Protocol);
            return false;
        }
        if self.sessions.get(id).is_some_and(|session| session.opened) {
            return true;
        }
        if !self.reserve(Resource::Session) {
            return false;
        }
        self.sessions.entry(id.to_owned()).or_default().opened = true;
        true
    }

    /// Registers a returned/read-back VIA-owned vendor session before setup (§5, §9).
    pub fn session_opened(&mut self, session: &str) {
        self.ensure_session(session);
    }

    /// Session lane for a newly opened driver; state and tombstones remain server-owned.
    pub fn attach(&mut self, session: &str) -> Arc<Lane> {
        if !self.ensure_session(session) {
            return Arc::new(Lane::new(Arc::default(), self.staging.clone()));
        }
        let session = self.sessions.entry(session.to_owned()).or_default();
        let lane = Arc::new(Lane::new(session.rejections.clone(), self.staging.clone()));
        session.lane = Some(lane.clone());
        lane
    }

    /// Driver detach cutoff; execution facts and correlation survive.
    pub fn detach(&mut self, session: &str) -> u64 {
        if let Some(session) = self.sessions.get_mut(session)
            && let Some(lane) = session.lane.take()
        {
            lane.close();
        }
        self.order
    }

    /// Registers a deterministic caller key before the HTTP request can be sent.
    pub fn register_turn(&mut self, session: &str, input_id: String, turn: TurnNumber) {
        if !self.ensure_session(session) {
            return;
        }
        if !self
            .sessions
            .get(session)
            .is_some_and(|session| session.inputs.contains_key(&input_id))
            && !self.key(&input_id)
        {
            return;
        }
        let session = self.sessions.entry(session.to_owned()).or_default();
        session.inputs.insert(input_id.clone(), turn);
        // Router serialization fixes the window before a prompt can be sent;
        // observations before registration cannot belong to the new turn.
        session.rejections.register(turn);
        session.state.last = Some(InputState {
            turn,
            input_id,
            phase: InputPhase::Registered,
        });
    }

    /// Registers the original operation budget for never-ask controls (§7.1, §11).
    pub fn register_turn_with_deadline(
        &mut self,
        session: &str,
        input_id: String,
        turn: TurnNumber,
        by: Deadline,
    ) {
        self.register_turn(session, input_id, turn);
        if let Some(session) = self.sessions.get(session)
            && let Some(window) = session
                .rejections
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_mut(&turn)
        {
            window.wall = Some(by);
        }
    }

    /// Marks the caller request sent without inventing acceptance.
    pub fn mark_sent(&mut self, session: &str, input: &str) {
        if let Some(session) = self.sessions.get_mut(session)
            && let Some(turn) = session.inputs.get(input)
            && !session.settled.contains(turn)
        {
            session.sent.insert(*turn);
        }
        self.phase(session, input, InputPhase::Sent);
    }

    /// Complete response proved no acceptance; successor rule may proceed.
    pub fn not_accepted(&mut self, session: &str, input: &str) {
        self.remove_sent(session, input);
        self.phase(session, input, InputPhase::NotAccepted);
    }

    /// Prompt was withdrawn before any byte could be sent.
    pub fn never_sent(&mut self, session: &str, input: &str) {
        self.remove_sent(session, input);
        self.phase(session, input, InputPhase::NeverSent);
    }

    fn remove_sent(&mut self, session: &str, input: &str) {
        if let Some(session) = self.sessions.get_mut(session)
            && let Some(turn) = session.inputs.get(input)
        {
            session.sent.remove(turn);
        }
    }

    fn phase(&mut self, session: &str, input: &str, phase: InputPhase) {
        if let Some(last) = self
            .sessions
            .get_mut(session)
            .and_then(|session| session.state.last.as_mut())
            && last.input_id == input
        {
            let advances = match phase {
                InputPhase::Sent => last.phase == InputPhase::Registered,
                InputPhase::NeverSent | InputPhase::NotAccepted => {
                    matches!(last.phase, InputPhase::Registered | InputPhase::Sent)
                }
                InputPhase::Registered
                | InputPhase::Accepted
                | InputPhase::Delivered
                | InputPhase::Ended => true,
            };
            if advances {
                last.phase = phase;
            }
        }
        self.changed.notify_waiters();
    }

    /// Counts a session request before send (`opencode.md` §7.2, §9).
    pub fn request_started(&mut self, session: &str) {
        self.request_started_for(Some(session));
    }

    /// Counts all HTTP requests, including setup before a vendor ID exists (§8, §9).
    pub fn request_started_for(&mut self, session: Option<&str>) {
        if !self.reserve(Resource::Request) {
            return;
        }
        if let Some(session) = session.and_then(|session| self.sessions.get_mut(session)) {
            session.state.pending_requests += 1;
        }
        self.changed.notify_waiters();
    }

    /// Complete response or proven withdrawal releases one pending request (§7.2).
    pub fn request_completed(&mut self, session: &str) {
        self.request_completed_for(Some(session));
    }

    /// Releases a counted HTTP request, including setup without a vendor ID (§8, §9).
    pub fn request_completed_for(&mut self, session: Option<&str>) {
        if let Some(session) = session.and_then(|session| self.sessions.get_mut(session)) {
            session.state.pending_requests = session.state.pending_requests.saturating_sub(1);
        }
        self.retained.release(Resource::Request);
        self.changed.notify_waiters();
    }

    /// Claims the generation's once-only reopen inbox cleanup, outside prompt admission.
    pub fn begin_reopen_cleanup(&mut self, session: &str) -> bool {
        if !self.ensure_session(session) {
            return false;
        }
        let state = &mut self.sessions.entry(session.to_owned()).or_default().state;
        if state.cleanup_started {
            return false;
        }
        state.cleanup_started = true;
        state.cleanup_pending = true;
        self.changed.notify_waiters();
        true
    }

    /// Releases the once-only cleanup fence after every stream proof, or when
    /// a new session needs no leftover read. Failure deliberately never calls it.
    pub fn finish_reopen_cleanup(&mut self, session: &str) {
        if !self.ensure_session(session) {
            return;
        }
        let state = &mut self.sessions.entry(session.to_owned()).or_default().state;
        state.cleanup_started = true;
        state.cleanup_pending = false;
        self.changed.notify_waiters();
    }

    /// Registers one recomputed own leftover before cancelling it (§7.2, §9).
    pub fn expect_cleanup_cancellation(&mut self, session: &str, input: &str) {
        if !self.ensure_session(session) {
            return;
        }
        if self.sessions.get(session).is_some_and(|session| {
            session.inputs.contains_key(input) || session.cleanup_expected.contains(input)
        }) {
            return;
        }
        if !self.key(input) {
            return;
        }
        self.sessions
            .entry(session.to_owned())
            .or_default()
            .cleanup_expected
            .insert(input.to_owned());
    }

    /// Stream evidence of a cleanup input's cancellation (a 204 is insufficient).
    pub fn cleanup_cancelled(&self, session: &str, input: &str) -> bool {
        self.sessions
            .get(session)
            .is_some_and(|session| session.state.cleanup_cancelled(input))
    }

    /// Validated HTTP acceptance enters the same ordering domain as SSE.
    pub fn accepted(&mut self, session_id: &str, input: &str, at: Instant) {
        self.order = self.order.saturating_add(1);
        let order = self.order;
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        let Some(owner) = session.inputs.get(input).copied() else {
            session.rejections.record(None, order);
            return;
        };
        if !session.accepted.insert(owner) {
            return;
        }
        accept_state(session, input);
        if let Some(lane) = &session.lane {
            lane.push(
                LaneItem::Accepted {
                    owner,
                    input_id: input.to_owned(),
                    read_order: order,
                    position: 0,
                    decoded_at: at,
                    staging: None,
                },
                input.len() + size_of::<LaneItem>(),
                order,
            );
        } else {
            session.rejections.record(Some(owner), order);
        }
        self.changed.notify_waiters();
    }

    /// Orders an inconclusive HTTP prompt response with SSE delivery (§8).
    pub fn inconclusive(&mut self, session_id: &str, input: &str, at: Instant) {
        self.order = self.order.saturating_add(1);
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        let Some(owner) = session.inputs.get(input).copied() else {
            return;
        };
        if let Some(lane) = &session.lane {
            lane.push(
                LaneItem::Inconclusive {
                    owner,
                    read_order: self.order,
                    position: 0,
                    decoded_at: at,
                    staging: None,
                },
                size_of::<LaneItem>(),
                self.order,
            );
        } else {
            session.rejections.record(Some(owner), self.order);
        }
        self.changed.notify_waiters();
    }

    fn reserve_event_keys(&mut self, session_id: &str, data: &EventData) -> bool {
        let Some(session) = self.sessions.get(session_id) else {
            return true;
        };
        let new = if let EventData::Step {
            kind: StepKind::Started,
            assistant_message_id,
            ..
        } = data
            && !session.messages.contains_key(assistant_message_id)
            && !session.unowned_messages.contains(assistant_message_id)
            && session.state.running
        {
            Some(assistant_message_id)
        } else if let EventData::Tool {
            kind: ToolKind::InputStarted | ToolKind::Called,
            assistant_message_id,
            call_id,
            ..
        } = data
            && !session.calls.contains_key(call_id)
            && !session.unowned_calls.contains(call_id)
            && (key_owner(session, assistant_message_id.as_deref(), None).is_some()
                || (session.state.running && session.state.execution_owner.is_none()))
        {
            Some(call_id)
        } else if let EventData::Compaction { key, .. } = data
            && !session.compactions.contains_key(key)
            && !session.inputs.contains_key(key)
            && !session.cleanup_expected.contains(key)
            && session.state.execution_owner.is_some()
        {
            Some(key)
        } else {
            None
        };
        let new = new.cloned();
        match new {
            Some(id) => self.key(&id),
            None => true,
        }
    }

    /// Whether a sent turn still has a result destination for a server-loss
    /// leftover report; execution admission facts never decide this boundary.
    pub fn has_loss_destination(&self) -> bool {
        self.sessions
            .values()
            .any(|session| !session.sent.is_empty())
    }

    /// Marks a turn settled while retaining every learned correlation key.
    pub fn settle(&mut self, session: &str, turn: TurnNumber) {
        if self
            .sessions
            .get(session)
            .is_some_and(|session| session.settled.contains(&turn))
        {
            return;
        }
        if !self.reserve(Resource::Tombstone) {
            return;
        }
        if let Some(session) = self.sessions.get_mut(session) {
            session.settled.insert(turn);
            session.sent.remove(&turn);
            session.rejections.settle(turn);
        }
        self.changed.notify_waiters();
    }

    /// Sent turns with unsettled result destinations keep a draining generation alive (§8).
    pub fn has_running_sent_turns(&self) -> bool {
        self.has_loss_destination()
    }

    /// A decoded known malformed payload fails only its driver's lane; the
    /// owner drains the generation. An unidentifiable envelope fails globally.
    pub fn malformed(&mut self, error: DecodeError) -> Result<(), RouterFailure> {
        self.order = self.order.saturating_add(1);
        match error {
            DecodeError::Generation => {
                // Missing routing identity could describe any currently live
                // turn, but cannot poison windows registered after this read.
                for session in self.sessions.values() {
                    session.rejections.record(None, self.order);
                }
                Err(RouterFailure::Protocol)
            }
            DecodeError::Session(session) => {
                self.protocol_failure(&session, self.order);
                self.changed.notify_waiters();
                Ok(())
            }
        }
    }

    fn protocol_failure(&self, session_id: &str, order: u64) {
        let (root, owner) = self
            .children
            .get(session_id)
            .map_or((session_id, None), |(root, owner)| {
                (root.as_str(), Some(*owner))
            });
        let Some(session) = self.sessions.get(root) else {
            return;
        };
        self.needs_drain.store(true, Ordering::Release);
        self.changed.notify_waiters();
        if owner.is_some_and(|owner| !session.rejections.is_live(owner)) {
            // A child's malformed interactive payload could belong to its
            // root owner only. An old child cannot fail the successor's lane.
            session.rejections.record(owner, order);
            return;
        }
        if let Some(lane) = &session.lane {
            lane.fail(LaneFailure::Protocol, order, owner);
        } else {
            session.rejections.record(owner, order);
        }
    }

    /// Dispatches one typed event, applying state before lane admission.
    pub fn dispatch(&mut self, event: Event, at: Instant) {
        self.order = self.order.saturating_add(1);
        let order = self.order;
        let Some(session_id) = event.session_id.as_ref() else {
            return;
        };
        if !event_fields_valid(&event) {
            if let Some(session) = self.sessions.get(session_id) {
                session.rejections.record(None, order);
            }
            self.failure.set(RouterFailure::Protocol);
            return;
        }
        if let EventData::Created {
            parent_id: Some(parent),
        } = &event.data
            && !self.children.contains_key(session_id)
            && let Some((root, owner)) = self.children.get(parent).cloned().or_else(|| {
                self.sessions
                    .get(parent)
                    .and_then(|session| session.state.execution_owner)
                    .map(|owner| (parent.clone(), owner))
            })
        {
            if !self.reserve(Resource::Child) || !self.key(session_id) {
                return;
            }
            self.children.insert(session_id.clone(), (root, owner));
            self.sessions.entry(session_id.clone()).or_default();
        }
        if let EventData::Interactive { .. } = &event.data {
            self.enqueue_decline(&event, at);
        }
        let native_notice = if let EventData::InteractiveSettled { kind, id } = &event.data {
            self.native_settlement(session_id, *kind, id, at, order)
        } else {
            false
        };
        let Some(session) = self.sessions.get_mut(session_id) else {
            // Foreign traffic has no driver or child ownership and no retained
            // session state. Interactive requests still used the reserved queue.
            return;
        };
        if let Some(seq) = event.seq {
            if session.state.last_seq.is_some_and(|last| seq <= last) {
                self.protocol_failure(session_id, order);
                self.changed.notify_waiters();
                return;
            }
            session.state.last_seq = Some(seq);
        }
        if native_notice {
            // The original owner's native settlement notice represents this
            // read. An anonymous duplicate must not taint a successor (§7.1, §11).
            return;
        }
        if let Some((parent, owner)) = self.children.get(session_id) {
            // Child traffic is excluded from root observations and usage,
            // except interactive requests credited to the originating turn.
            if !matches!(event.data, EventData::Interactive { .. }) {
                return;
            }
            let session = self.sessions.entry(parent.clone()).or_default();
            let late = session.settled.contains(owner);
            let bytes = retained_bytes(&event);
            if let Some(lane) = &session.lane {
                lane.push(
                    LaneItem::Event(Box::new(Routed {
                        event,
                        owner: Some(*owner),
                        read_order: order,
                        position: 0,
                        decoded_at: at,
                        late,
                        joined_steps: Vec::new(),
                        staging: None,
                    })),
                    bytes,
                    order,
                );
            } else {
                session.rejections.record(Some(*owner), order);
            }
            self.changed.notify_waiters();
            return;
        }
        self.dispatch_root(event, at, order);
    }

    // Apply a root session's execution facts before nonblocking lane admission (§7.2, §9).
    fn dispatch_root(&mut self, event: Event, at: Instant, order: u64) {
        let Some(session_id) = event.session_id.as_ref() else {
            return;
        };
        if !self.reserve_event_keys(session_id, &event.data) {
            if let Some(session) = self.sessions.get(session_id) {
                session.rejections.record(None, order);
            }
            return;
        }
        let session = self.sessions.entry(session_id.clone()).or_default();
        if matches!(event.data, EventData::Execution { .. }) {
            // Execution boundaries discard temporary keys that never acquired
            // an owning turn. Converted live/tombstoned keys stay charged (§9).
            self.retained.release_keys(
                session.unowned_messages.len() + session.unowned_calls.len(),
                session
                    .unowned_messages
                    .iter()
                    .chain(&session.unowned_calls)
                    .map(String::len)
                    .sum(),
            );
        }
        let joined_steps = take_joined_steps(session, &event.data);
        let owner = apply(session, &event.data);
        if owner.is_none() && observation(&event.data) {
            // A known observation without correlation might be a lost call
            // or part of an owned turn. Delivering a diagnostic is insufficient
            // evidence for that turn's usage accounting.
            session.rejections.record(None, order);
        }
        let late = owner.is_some_and(|owner| session.settled.contains(&owner));
        // State is already applied even when this insertion fails or no
        // driver is attached; only lane delivery can decide an outcome.
        let bytes = joined_steps
            .iter()
            .fold(retained_bytes(&event), |bytes, id| {
                bytes
                    .saturating_add(size_of::<String>())
                    .saturating_add(id.len())
            });
        if let Some(lane) = &session.lane {
            lane.push(
                LaneItem::Event(Box::new(Routed {
                    event,
                    owner,
                    read_order: order,
                    position: 0,
                    decoded_at: at,
                    late,
                    joined_steps,
                    staging: None,
                })),
                bytes,
                order,
            );
        } else {
            session.rejections.record(owner, order);
        }
        self.changed.notify_waiters();
    }

    /// Latest execution admission state; it never decides a driver outcome.
    pub fn state(&self, session: &str) -> Option<SessionState> {
        self.sessions
            .get(session)
            .map(|session| session.state.clone())
    }

    /// Whether the successor prompt can be sent now.
    pub fn eligible(&self, session: &str) -> bool {
        self.sessions
            .get(session)
            .is_none_or(|session| session.state.eligible())
    }

    /// Wakeup source for execution admission and reopen cleanup.
    pub fn notify(&self) -> Arc<Notify> {
        self.changed.clone()
    }
}

fn lane_failure(cause: RouterFailure) -> LaneFailure {
    match cause {
        RouterFailure::Protocol => LaneFailure::Protocol,
        RouterFailure::Overflow => LaneFailure::Overflow,
    }
}

fn event_fields_valid(event: &Event) -> bool {
    let mut fields = vec![event.id.as_deref(), event.session_id.as_deref()];
    match &event.data {
        EventData::Inbox { id, .. } | EventData::InteractiveSettled { id, .. } => {
            fields.push(Some(id));
        }
        EventData::Execution { error, reason, .. } => {
            fields.push(error.as_ref().map(|error| error.code.as_str()));
            fields.push(reason.as_deref());
        }
        EventData::Step {
            assistant_message_id,
            finish,
            ..
        } => {
            fields.push(Some(assistant_message_id));
            fields.push(finish.as_deref());
        }
        EventData::Text {
            assistant_message_id,
            ..
        } => fields.push(Some(assistant_message_id)),
        EventData::Tool {
            assistant_message_id,
            call_id,
            tool,
            error,
            ..
        } => {
            fields.push(assistant_message_id.as_deref());
            fields.push(Some(call_id));
            fields.push(tool.as_deref());
            fields.push(error.as_ref().map(|error| error.code.as_str()));
        }
        EventData::Compaction { key, .. } => fields.push(Some(key)),
        EventData::Interactive {
            id,
            action,
            message_id,
            call_id,
            ..
        } => {
            fields.push(Some(id));
            fields.push(action.as_deref());
            fields.push(message_id.as_deref());
            fields.push(call_id.as_deref());
        }
        EventData::Created { parent_id } => fields.push(parent_id.as_deref()),
        EventData::Activity {
            message_id,
            call_id,
        } => {
            fields.push(message_id.as_deref());
            fields.push(call_id.as_deref());
        }
    }
    fields
        .into_iter()
        .flatten()
        .all(|field| field.len() <= bounds::ID_BYTES)
}

fn accept_state(session: &mut Session, input: &str) {
    if let Some(last) = session.state.last.as_mut()
        && last.input_id == input
        && matches!(last.phase, InputPhase::Registered | InputPhase::Sent)
    {
        last.phase = InputPhase::Accepted;
    }
}

fn key_owner(session: &Session, message: Option<&str>, call: Option<&str>) -> Option<TurnNumber> {
    call.and_then(|call| session.calls.get(call).copied())
        .or_else(|| message.and_then(|message| session.messages.get(message).copied()))
}

fn apply_inbox(session: &mut Session, kind: InboxKind, id: &str) -> Option<TurnNumber> {
    let owner = session.inputs.get(id).copied();
    if kind == InboxKind::Cancelled && (owner.is_some() || session.cleanup_expected.contains(id)) {
        session.state.cancelled(id);
    }
    if owner.is_some_and(|owner| session.delivered.contains(&owner)) {
        // A delivered input keeps its execution tombstone forever. A repeated
        // inbox event cannot claim a later execution or change its admission facts.
        return owner;
    }
    match kind {
        InboxKind::Enqueued => {
            if let Some(owner) = owner {
                session.accepted.insert(owner);
                accept_state(session, id);
            }
        }
        InboxKind::Delivered => {
            // A first delivery remains an execution fact even after its result
            // settled unknown. Route subsequent observations late to that owner;
            // its execution terminal must release the successor rule (§7.2).
            if let Some(owner) = owner {
                session.delivered.insert(owner);
            }
            session.state.running = true;
            if session.state.execution_owner.is_none() {
                session.state.execution_owner = owner;
                if let Some(owner) = owner {
                    bind_unowned_keys(session, owner);
                }
            }
            if let Some(last) = session.state.last.as_mut()
                && last.input_id == id
            {
                last.phase = InputPhase::Delivered;
            }
        }
        InboxKind::Cancelled => {
            if let Some(last) = session.state.last.as_mut()
                && last.input_id == id
                && last.phase != InputPhase::Delivered
            {
                last.phase = InputPhase::Ended;
            }
        }
        InboxKind::Activity => {}
    }
    owner
}

fn take_joined_steps(session: &mut Session, data: &EventData) -> Vec<String> {
    let EventData::Inbox {
        kind: InboxKind::Delivered,
        id,
    } = data
    else {
        return Vec::new();
    };
    let Some(owner) = session.inputs.get(id) else {
        return Vec::new();
    };
    if session.state.execution_owner.is_none() && !session.delivered.contains(owner) {
        std::mem::take(&mut session.unowned_step_order)
    } else {
        Vec::new()
    }
}

fn bind_unowned_keys(session: &mut Session, owner: TurnNumber) {
    for message in session.unowned_messages.drain() {
        session.messages.entry(message).or_insert(owner);
    }
    for call in session.unowned_calls.drain() {
        session.calls.entry(call).or_insert(owner);
    }
}

fn apply_execution(session: &mut Session, kind: ExecutionKind) -> Option<TurnNumber> {
    let owner = session.state.execution_owner;
    session.unowned_messages.clear();
    session.unowned_step_order.clear();
    session.unowned_calls.clear();
    match kind {
        ExecutionKind::Started => {
            session.state.running = true;
            session.state.execution_owner = None;
            None
        }
        ExecutionKind::Succeeded | ExecutionKind::Failed | ExecutionKind::Interrupted => {
            session.state.running = false;
            session.state.execution_owner = None;
            if let Some(last) = session.state.last.as_mut()
                && Some(last.turn) == owner
            {
                last.phase = InputPhase::Ended;
            }
            owner
        }
    }
}

fn apply(session: &mut Session, data: &EventData) -> Option<TurnNumber> {
    match data {
        EventData::Inbox { kind, id } => apply_inbox(session, *kind, id),
        EventData::Execution { kind, .. } => apply_execution(session, *kind),
        EventData::Step {
            kind,
            assistant_message_id,
            ..
        } => {
            let owner = session
                .messages
                .get(assistant_message_id)
                .copied()
                .or_else(|| {
                    if *kind == StepKind::Started {
                        session.state.execution_owner
                    } else {
                        None
                    }
                });
            if *kind == StepKind::Started {
                if let Some(owner) = owner {
                    session
                        .messages
                        .entry(assistant_message_id.clone())
                        .or_insert(owner);
                } else if session.state.running
                    && session.state.execution_owner.is_none()
                    && session
                        .unowned_messages
                        .insert(assistant_message_id.clone())
                {
                    session
                        .unowned_step_order
                        .push(assistant_message_id.clone());
                }
            }
            owner
        }
        EventData::Text {
            assistant_message_id,
            ..
        } => session.messages.get(assistant_message_id).copied(),
        EventData::Tool {
            kind,
            assistant_message_id,
            call_id,
            ..
        } => {
            let owner = key_owner(session, assistant_message_id.as_deref(), Some(call_id));
            if matches!(kind, ToolKind::InputStarted | ToolKind::Called) {
                if let Some(owner) = owner {
                    session.calls.entry(call_id.clone()).or_insert(owner);
                } else if session.state.running && session.state.execution_owner.is_none() {
                    session.unowned_calls.insert(call_id.clone());
                }
            }
            owner
        }
        EventData::Compaction { key, .. } => {
            let owner = session
                .compactions
                .get(key)
                .copied()
                .or(session.state.execution_owner);
            if let Some(owner) = owner {
                session.compactions.entry(key.clone()).or_insert(owner);
            }
            owner
        }
        EventData::Interactive {
            kind,
            message_id,
            call_id,
            ..
        } => match kind {
            super::events::InteractiveKind::Permission => message_id
                .as_ref()
                .and_then(|message| session.messages.get(message).copied())
                .or_else(|| {
                    call_id
                        .as_ref()
                        .and_then(|call| session.calls.get(call).copied())
                }),
            super::events::InteractiveKind::Form => session.state.execution_owner,
        },
        EventData::InteractiveSettled { kind, id } => {
            session.interactive.get(&(*kind, id.clone())).copied()
        }
        EventData::Created { .. } => None,
        EventData::Activity {
            message_id,
            call_id,
        } => {
            if message_id.is_some() || call_id.is_some() {
                key_owner(session, message_id.as_deref(), call_id.as_deref())
            } else {
                session.state.execution_owner
            }
        }
    }
}

fn observation(data: &EventData) -> bool {
    match data {
        EventData::Step {
            kind: StepKind::Ended | StepKind::Failed,
            ..
        }
        | EventData::Text { .. }
        | EventData::Tool { .. }
        | EventData::Compaction { .. } => true,
        EventData::Inbox { .. }
        | EventData::Execution { .. }
        | EventData::Step {
            kind: StepKind::Started | StepKind::Activity,
            ..
        }
        | EventData::Interactive { .. }
        | EventData::InteractiveSettled { .. }
        | EventData::Created { .. }
        | EventData::Activity { .. } => false,
    }
}

fn retained_bytes(event: &Event) -> usize {
    let mut bytes = size_of::<Routed>()
        + event.id.as_ref().map_or(0, String::len)
        + event.session_id.as_ref().map_or(0, String::len);
    bytes += match &event.data {
        EventData::Inbox { id, .. } | EventData::InteractiveSettled { id, .. } => id.len(),
        EventData::Execution { error, reason, .. } => {
            error.as_ref().map_or(0, |error| error.code.len())
                + reason.as_ref().map_or(0, String::len)
        }
        EventData::Step {
            assistant_message_id,
            finish,
            ..
        } => assistant_message_id.len() + finish.as_ref().map_or(0, String::len),
        EventData::Text {
            assistant_message_id,
            text,
            ..
        } => assistant_message_id.len() + text.len(),
        EventData::Tool {
            assistant_message_id,
            call_id,
            tool,
            error,
            ..
        } => {
            assistant_message_id.as_ref().map_or(0, String::len)
                + call_id.len()
                + tool.as_ref().map_or(0, String::len)
                + error.as_ref().map_or(0, |error| error.code.len())
        }
        EventData::Compaction { key, .. } => key.len(),
        EventData::Interactive {
            id,
            message_id,
            call_id,
            action,
            ..
        } => {
            id.len()
                + action.as_ref().map_or(0, String::len)
                + message_id.as_ref().map_or(0, String::len)
                + call_id.as_ref().map_or(0, String::len)
        }
        EventData::Created { parent_id } => parent_id.as_ref().map_or(0, String::len),
        EventData::Activity {
            message_id,
            call_id,
        } => message_id.as_ref().map_or(0, String::len) + call_id.as_ref().map_or(0, String::len),
    };
    bytes
}

#[cfg(test)]
mod tests {
    use super::super::declines::{
        CleanupAction, DeclineDisposition, DeclineOutcome, DeclineSettlement,
    };
    use super::super::events::decode;
    use super::*;
    use serde_json::{Value, json};

    fn turn(number: u32) -> TurnNumber {
        TurnNumber::try_from(number).unwrap()
    }
    fn apply(router: &mut Router, session: &str, kind: &str, fields: Value, seq: Option<u64>) {
        let mut fields = fields;
        fields["sessionID"] = json!(session);
        let mut envelope = json!({"id":"evt_one","type":kind,"data":fields});
        if let Some(seq) = seq {
            envelope["durable"] = json!({"seq":seq});
        }
        match decode(&serde_json::to_vec(&envelope).unwrap()) {
            Ok(event) => router.dispatch(event, Instant::now()),
            Err(error) => router.malformed(error).unwrap(),
        }
    }
    fn owned(router: &mut Router, session: &str, input: &str, number: u32) {
        router.register_turn(session, input.into(), turn(number));
        apply(
            router,
            session,
            "session.execution.started",
            json!({}),
            None,
        );
        apply(
            router,
            session,
            "session.inbox.delivered",
            json!({"inboxID":input}),
            None,
        );
    }
    fn routed(item: Option<LaneItem>) -> Routed {
        match item.unwrap() {
            LaneItem::Event(routed) => *routed,
            LaneItem::Accepted { .. }
            | LaneItem::ResponseLimit { .. }
            | LaneItem::Inconclusive { .. }
            | LaneItem::Decline { .. } => panic!("unexpected marker"),
        }
    }

    #[test]
    fn oc09_response_limit_preserves_tombstone_owner_without_failing_successor() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        router.settle("ses_a", turn(1));
        router.register_turn("ses_a", "input_b".into(), turn(2));
        let detected_at = Instant::now();
        router.response_limit("ses_a", turn(1), detected_at);
        assert!(
            matches!(lane.pop(), Some(LaneItem::ResponseLimit { owner, .. }) if owner == turn(1))
        );
        assert!(router.needs_drain());
        assert_eq!(lane.failure(), None);
        assert_eq!(lane.response_limit(turn(1)), Some(detected_at));
        assert_eq!(lane.response_limit(turn(2)), None);
        assert_eq!(lane.earliest_unobserved(turn(2)), None);
        assert!(router.sessions["ses_a"].rejections.is_live(turn(2)));
    }

    #[test]
    fn oc09_session_state_limit_is_exact() {
        let mut router = Router::new();
        let observer = router.attach("ses_0");
        for number in 1..1024 {
            router.attach(&format!("ses_{number}"));
        }
        assert_eq!(observer.failure(), None);
        router.attach("ses_excess");
        assert_eq!(observer.failure(), Some(LaneFailure::Overflow));
        assert_eq!(router.sessions.len(), 1024);
    }

    #[test]
    fn oc09_tombstone_limit_is_exact() {
        let mut router = Router::new();
        let observer = router.attach("ses_a");
        for number in 1..=4096 {
            router.register_turn("ses_a", format!("input_{number}"), turn(number));
            router.settle("ses_a", turn(number));
        }
        assert_eq!(observer.failure(), None);
        router.register_turn("ses_a", "input_excess".into(), turn(4097));
        router.settle("ses_a", turn(4097));
        assert_eq!(observer.failure(), Some(LaneFailure::Overflow));
    }

    #[test]
    fn oc09_child_limit_is_independent_of_driver_sessions() {
        let mut router = Router::new();
        let observer = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        while observer.pop().is_some() {}
        for number in 0..4096 {
            apply(
                &mut router,
                &format!("child_{number}"),
                "session.created",
                json!({"parentID":"ses_a"}),
                None,
            );
        }
        assert_eq!(observer.failure(), None);
        apply(
            &mut router,
            "child_excess",
            "session.created",
            json!({"parentID":"ses_a"}),
            None,
        );
        assert_eq!(observer.failure(), Some(LaneFailure::Overflow));
        assert_eq!(router.children.len(), 4096);
    }

    #[test]
    fn oc09_incomplete_request_limit_is_server_scoped_and_releases_slots() {
        let mut router = Router::new();
        let observer = router.attach("ses_a");
        router.attach("ses_b");
        for _ in 0..32 {
            router.request_started("ses_a");
            router.request_started("ses_b");
        }
        assert_eq!(observer.failure(), None);
        router.request_completed("ses_a");
        router.request_started("ses_b");
        assert_eq!(observer.failure(), None);
        router.request_started("ses_a");
        assert_eq!(observer.failure(), Some(LaneFailure::Overflow));
    }

    #[test]
    fn oc09_cleanup_intents_share_pending_http_limit() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        for _ in 0..63 {
            router.request_started_for(None);
        }
        owned(&mut router, "ses_a", "input_a", 1);
        router.mark_sent("ses_a", "input_a");
        let by = Deadline::at(Instant::now() + Duration::from_secs(2));
        router.enqueue_cleanup("ses_a", turn(1), by);
        router.enqueue_cleanup("ses_a", turn(1), by);
        assert_eq!(
            router.failure(),
            None,
            "repeated cleanup reserves no second slot"
        );
        assert_eq!(router.state("ses_a").unwrap().pending_requests, 1);
        let work = router.pop_pending_cleanup().unwrap();
        assert!(matches!(work.action, CleanupAction::Interrupt));
        assert!(router.pop_pending_cleanup().is_none());
        owned(&mut router, "ses_b", "input_b", 1);
        router.mark_sent("ses_b", "input_b");
        router.enqueue_cleanup("ses_b", turn(1), by);
        assert_eq!(lane.failure(), Some(LaneFailure::Overflow));
        assert!(router.pop_pending_cleanup().is_none());
    }

    #[test]
    fn oc09_cleanup_cancel_race_keeps_tombstoned_input_authority() {
        let mut router = Router::new();
        router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        router.mark_sent("ses_a", "input_a");
        let by = Deadline::at(Instant::now() + Duration::from_secs(2));
        router.enqueue_cleanup("ses_a", turn(1), by);
        let work = router.pop_pending_cleanup().unwrap();
        assert!(matches!(work.action, CleanupAction::InputCancel));
        router.settle("ses_a", turn(1));
        apply(
            &mut router,
            "ses_a",
            "session.execution.started",
            json!({}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.inbox.delivered",
            json!({"inboxID":"input_a"}),
            None,
        );
        let interrupt = router.followup_cleanup(&work).unwrap();
        assert!(matches!(interrupt.action, CleanupAction::Interrupt));
        assert_eq!(interrupt.by.instant(), work.by.instant());
        assert_eq!(router.state("ses_a").unwrap().pending_requests, 2);
        assert!(router.followup_cleanup(&work).is_none());
    }

    #[test]
    fn oc09_cleanup_cancel_intent_never_claims_successor_execution() {
        let mut router = Router::new();
        router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        router.mark_sent("ses_a", "input_a");
        router.enqueue_cleanup(
            "ses_a",
            turn(1),
            Deadline::at(Instant::now() + Duration::from_secs(2)),
        );
        let work = router.pop_pending_cleanup().unwrap();
        router.settle("ses_a", turn(1));
        apply(
            &mut router,
            "ses_a",
            "session.inbox.cancelled",
            json!({"inboxID":"input_a"}),
            None,
        );
        router.request_completed("ses_a");
        assert!(router.eligible("ses_a"));
        owned(&mut router, "ses_a", "input_b", 2);
        assert!(router.followup_cleanup(&work).is_none());
        assert!(
            router.claim_interrupt("ses_a", turn(2)),
            "the successor stop remains unclaimed"
        );
    }

    #[test]
    fn oc09_pending_interactive_limit_is_exact() {
        let mut router = Router::new();
        let observer = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        while observer.pop().is_some() {}
        for number in 0..64 {
            apply(
                &mut router,
                "ses_a",
                "form.created",
                json!({"form":{"id":format!("form_{number}"),"sessionID":"ses_a"}}),
                None,
            );
            while observer.pop().is_some() {}
        }
        assert_eq!(observer.failure(), None);
        apply(
            &mut router,
            "ses_a",
            "form.created",
            json!({"form":{"id":"form_excess","sessionID":"ses_a"}}),
            None,
        );
        assert_eq!(observer.failure(), Some(LaneFailure::Overflow));
    }

    #[test]
    fn oc09_correlation_key_count_includes_every_step_of_one_turn() {
        let mut router = Router::new();
        let observer = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        while observer.pop().is_some() {}
        for number in 1..65536 {
            apply(
                &mut router,
                "ses_a",
                "session.step.started",
                json!({"assistantMessageID":format!("assistant_{number}")}),
                None,
            );
            while observer.pop().is_some() {}
        }
        assert_eq!(observer.failure(), None);
        apply(
            &mut router,
            "ses_a",
            "session.step.started",
            json!({"assistantMessageID":"assistant_excess"}),
            None,
        );
        assert_eq!(observer.failure(), Some(LaneFailure::Overflow));
    }

    #[test]
    fn oc09_correlation_key_byte_limit_is_exact() {
        let mut router = Router::new();
        let observer = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        while observer.pop().is_some() {}
        for number in 0..8192 {
            let size = if number == 8191 { 1017 } else { 1024 };
            let prefix = format!("assistant_{number}_");
            let id = format!("{prefix}{}", "a".repeat(size - prefix.len()));
            apply(
                &mut router,
                "ses_a",
                "session.step.started",
                json!({"assistantMessageID":id}),
                None,
            );
            while observer.pop().is_some() {}
        }
        assert_eq!(observer.failure(), None);
        apply(
            &mut router,
            "ses_a",
            "session.step.started",
            json!({"assistantMessageID":"excess"}),
            None,
        );
        assert_eq!(observer.failure(), Some(LaneFailure::Overflow));
    }

    #[test]
    fn oc09_retained_id_above_one_kib_is_protocol() {
        let mut router = Router::new();
        let observer = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        while observer.pop().is_some() {}
        apply(
            &mut router,
            "ses_a",
            "session.step.started",
            json!({"assistantMessageID":"a".repeat(1024)}),
            None,
        );
        assert_eq!(observer.failure(), None);
        while observer.pop().is_some() {}
        apply(
            &mut router,
            "ses_a",
            "session.step.started",
            json!({"assistantMessageID":"b".repeat(1025)}),
            None,
        );
        assert_eq!(observer.failure(), Some(LaneFailure::Protocol));
    }

    #[test]
    fn oc09_staging_item_limit_is_shared_across_lanes() {
        let mut router = Router::new();
        let observer = router.attach("ses_0");
        for number in 0..64 {
            let id = format!("ses_{number}");
            if number != 0 {
                router.attach(&id);
            }
            for _ in 0..16 {
                apply(&mut router, &id, "session.renamed", json!({}), None);
            }
        }
        assert_eq!(observer.failure(), None);
        router.attach("ses_excess");
        apply(
            &mut router,
            "ses_excess",
            "session.renamed",
            json!({}),
            None,
        );
        assert_eq!(observer.failure(), Some(LaneFailure::Overflow));
    }

    #[test]
    fn oc09_staging_byte_limit_is_shared_across_lanes() {
        let mut router = Router::new();
        let observer = router.attach("ses_0");
        for number in 0..4 {
            let id = format!("ses_{number}");
            if number != 0 {
                router.attach(&id);
            }
            let event = Event {
                id: None,
                session_id: Some(id),
                seq: None,
                data: EventData::Activity {
                    message_id: None,
                    call_id: None,
                },
            };
            let lane = router.sessions[&format!("ses_{number}")]
                .lane
                .clone()
                .unwrap();
            lane.push(
                LaneItem::Event(Box::new(Routed {
                    event,
                    owner: None,
                    read_order: 1,
                    position: 0,
                    decoded_at: Instant::now(),
                    late: false,
                    joined_steps: Vec::new(),
                    staging: None,
                })),
                1024 * 1024,
                1,
            );
        }
        assert_eq!(observer.failure(), None);
        let excess = router.attach("ses_excess");
        let event = Event {
            id: None,
            session_id: Some("ses_excess".into()),
            seq: None,
            data: EventData::Activity {
                message_id: None,
                call_id: None,
            },
        };
        excess.push(
            LaneItem::Event(Box::new(Routed {
                event,
                owner: None,
                read_order: 2,
                position: 0,
                decoded_at: Instant::now(),
                late: false,
                joined_steps: Vec::new(),
                staging: None,
            })),
            1,
            2,
        );
        assert_eq!(observer.failure(), Some(LaneFailure::Overflow));
    }

    #[test]
    fn oc07_native_settlement_suppresses_failed_decline_stop_without_http_proof() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        apply(
            &mut router,
            "ses_a",
            "form.created",
            json!({"form":{"id":"form_one","sessionID":"ses_a"}}),
            None,
        );
        let work = router.pop_pending_decline().unwrap();
        apply(
            &mut router,
            "ses_a",
            "form.cancelled",
            json!({"id":"form_one"}),
            None,
        );
        let error = via_wire::http::HttpError {
            response_status: None,
            sent: via_wire::http::Sent::Maybe,
            kind: via_wire::http::HttpFailure::Deadline,
        };
        assert!(matches!(
            router.finish_decline(&work, &Err(error), Instant::now()),
            DeclineDisposition::Settled
        ));
        let notice = std::iter::from_fn(|| lane.pop())
            .find_map(|item| match item {
                LaneItem::Decline { notice, .. } => Some(notice),
                LaneItem::Event(_)
                | LaneItem::ResponseLimit { .. }
                | LaneItem::Inconclusive { .. }
                | LaneItem::Accepted { .. } => None,
            })
            .unwrap();
        assert_eq!(notice.settlement, DeclineSettlement::Settled);
        assert_eq!(
            notice.outcome,
            Err(error),
            "native settlement must not fabricate HTTP 204"
        );
    }

    #[test]
    fn oc07_queued_native_settlement_retains_original_owner_notice() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        apply(
            &mut router,
            "ses_a",
            "session.step.started",
            json!({"assistantMessageID":"assistant_a"}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "permission.asked",
            json!({"id":"permission_a","action":"bash",
                   "source":{"messageID":"assistant_a","id":"call_a"}}),
            None,
        );
        // The pump has not claimed the work. Native settlement must withdraw
        // the HTTP request while releasing the original delivery's proof wait.
        apply(
            &mut router,
            "ses_a",
            "permission.replied",
            json!({"requestID":"permission_a","reply":"reject"}),
            None,
        );
        assert!(router.pop_pending_decline().is_none());
        let items = std::iter::from_fn(|| lane.pop()).collect::<Vec<_>>();
        assert!(
            !items.iter().any(|item| matches!(
                item,
                LaneItem::Event(event)
                    if matches!(event.event.data, EventData::InteractiveSettled { .. })
            )),
            "the delivered settlement notice replaces its anonymous event copy"
        );
        let (owner, notice) = items
            .into_iter()
            .find_map(|item| match item {
                LaneItem::Decline { owner, notice, .. } => Some((owner, notice)),
                LaneItem::Event(_)
                | LaneItem::ResponseLimit { .. }
                | LaneItem::Inconclusive { .. }
                | LaneItem::Accepted { .. } => None,
            })
            .expect("native settlement must retain its original owner's notice");
        assert_eq!(owner, turn(1));
        assert_eq!(notice.settlement, DeclineSettlement::Settled);
        assert_eq!(notice.outcome, Ok(DeclineOutcome::NativeSettled));
    }

    #[test]
    fn oc11_native_settlement_keeps_old_owner_off_successor_rejections() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        apply(
            &mut router,
            "ses_a",
            "form.created",
            json!({"form":{"id":"form_a","sessionID":"ses_a"}}),
            None,
        );
        let work = router.pop_pending_decline().unwrap();
        router.finish_decline(&work, &Ok(DeclineOutcome::Declined), Instant::now());
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        router.settle("ses_a", turn(1));
        while lane.pop().is_some() {}
        owned(&mut router, "ses_a", "input_b", 2);
        while lane.pop().is_some() {}
        lane.close();
        apply(
            &mut router,
            "ses_a",
            "form.cancelled",
            json!({"id":"form_a"}),
            None,
        );
        assert_eq!(lane.earliest_unobserved(turn(2)), None);
    }

    #[test]
    fn oc09_staging_covers_popped_items_held_by_delivery() {
        let mut router = Router::new();
        let observer = router.attach("ses_0");
        let mut held = Vec::new();
        for number in 0..64 {
            let id = format!("ses_{number}");
            let lane = if number == 0 {
                observer.clone()
            } else {
                router.attach(&id)
            };
            for _ in 0..16 {
                apply(&mut router, &id, "session.renamed", json!({}), None);
                held.push(lane.pop().unwrap());
            }
        }
        assert_eq!(observer.failure(), None);
        apply(&mut router, "ses_0", "session.renamed", json!({}), None);
        assert_eq!(observer.failure(), Some(LaneFailure::Overflow));
        drop(held);
    }

    #[test]
    fn oc09_unleased_foreign_traffic_retains_no_session_state() {
        let mut router = Router::new();
        apply(
            &mut router,
            "ses_foreign",
            "session.renamed",
            json!({}),
            None,
        );
        assert!(router.sessions.is_empty());
    }

    #[test]
    fn oc09_unowned_execution_keys_release_when_discarded() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        apply(
            &mut router,
            "ses_a",
            "session.execution.started",
            json!({}),
            None,
        );
        for number in 0..bounds::KEYS {
            apply(
                &mut router,
                "ses_a",
                "session.step.started",
                json!({"assistantMessageID": format!("step_{number}")}),
                None,
            );
            while lane.pop().is_some() {}
        }
        assert_eq!(router.failure(), None);
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        router.register_turn("ses_a", "input_a".to_owned(), turn(1));
        assert_eq!(
            router.failure(),
            None,
            "discarded keys must release their budget"
        );
    }

    #[test]
    fn oc07_descendant_request_remains_deduplicated_after_settlement() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        apply(
            &mut router,
            "child_a",
            "session.created",
            json!({"parentID":"ses_a"}),
            None,
        );
        apply(
            &mut router,
            "child_a",
            "form.created",
            json!({"form":{"id":"form_one","sessionID":"child_a"}}),
            None,
        );
        let work = router.pop_pending_decline().unwrap();
        assert_eq!(work.owner.as_ref().unwrap().turn, turn(1));
        router.finish_decline(&work, &Ok(DeclineOutcome::Declined), Instant::now());
        apply(
            &mut router,
            "child_a",
            "form.created",
            json!({"form":{"id":"form_one","sessionID":"child_a"}}),
            None,
        );
        assert!(router.pop_pending_decline().is_none());
        assert_eq!(lane.failure(), None);
    }

    #[test]
    fn oc11_rejections_outside_a_live_turn_never_taint_its_window() {
        let mut router = Router::new();
        apply(&mut router, "ses_a", "session.created", json!({}), None);
        apply(
            &mut router,
            "ses_a",
            "session.execution.started",
            json!({}),
            None,
        );
        let lane = router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        assert_eq!(lane.earliest_unobserved(turn(1)), None);
    }

    #[test]
    fn oc11_unrelated_session_rejection_never_taints_a_live_turn() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        router
            .malformed(DecodeError::Session("ses_b".into()))
            .unwrap();
        assert_eq!(lane.earliest_unobserved(turn(1)), None);
    }

    #[test]
    fn oc11_unknown_session_malformed_taints_only_currently_live_windows() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        assert_eq!(
            router.malformed(DecodeError::Generation),
            Err(RouterFailure::Protocol)
        );
        assert_eq!(lane.earliest_unobserved(turn(1)), Some(1));
        router.settle("ses_a", turn(1));
        router.register_turn("ses_a", "input_b".into(), turn(2));
        assert_eq!(lane.earliest_unobserved(turn(1)), Some(1));
        assert_eq!(lane.earliest_unobserved(turn(2)), None);
    }

    #[test]
    fn oc11_malformed_mapped_child_taints_only_its_live_root_owner() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        apply(
            &mut router,
            "ses_child",
            "session.created",
            json!({"parentID":"ses_a"}),
            None,
        );
        router
            .malformed(DecodeError::Session("ses_child".into()))
            .unwrap();
        assert_eq!(lane.failure(), Some(LaneFailure::Protocol));
        assert!(lane.earliest_unobserved(turn(1)).is_some());
        router.settle("ses_a", turn(1));
        router.detach("ses_a");
        let successor = router.attach("ses_a");
        router.register_turn("ses_a", "input_b".into(), turn(2));
        router
            .malformed(DecodeError::Session("ses_child".into()))
            .unwrap();
        assert_eq!(successor.failure(), None);
        assert_eq!(successor.earliest_unobserved(turn(2)), None);
    }

    #[test]
    fn oc11_prior_driver_drops_count_only_their_retained_owner_window() {
        let mut router = Router::new();
        let previous = router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        router.settle("ses_a", turn(1));
        router.detach("ses_a");
        let reopened = router.attach("ses_a");
        router.register_turn("ses_a", "input_b".into(), turn(2));
        reopened.reject_unobserved(Some(turn(1)), 7);
        assert_eq!(reopened.earliest_unobserved(turn(2)), None);
        assert_eq!(previous.earliest_unobserved(turn(1)), None);
        assert_eq!(
            reopened
                .rejections
                .0
                .lock()
                .unwrap()
                .get(&turn(1))
                .unwrap()
                .count,
            1
        );
        reopened.reject_unobserved(Some(turn(2)), 8);
        assert_eq!(reopened.earliest_unobserved(turn(2)), Some(8));
    }

    #[test]
    fn oc05_settled_never_delivered_input_ends_late_execution_before_successor() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        router.mark_sent("ses_a", "input_a");
        router.settle("ses_a", turn(1));
        assert!(!router.eligible("ses_a"));
        apply(
            &mut router,
            "ses_a",
            "session.execution.started",
            json!({}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.inbox.delivered",
            json!({"inboxID":"input_a"}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.step.started",
            json!({"assistantMessageID":"assistant_a"}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        assert!(router.eligible("ses_a"));
        let events: Vec<_> = std::iter::from_fn(|| lane.pop())
            .map(|item| routed(Some(item)))
            .collect();
        for event in &events[1..] {
            assert_eq!(event.owner, Some(turn(1)));
            assert!(event.late);
        }
        owned(&mut router, "ses_a", "input_b", 2);
        while lane.pop().is_some() {}
        apply(
            &mut router,
            "ses_a",
            "session.text.ended",
            json!({"assistantMessageID":"assistant_a","ordinal":0,"text":"late"}),
            None,
        );
        let old = routed(lane.pop());
        assert_eq!(old.owner, Some(turn(1)));
        assert!(old.late);
        assert_eq!(
            router.state("ses_a").unwrap().execution_owner,
            Some(turn(2))
        );
    }

    #[test]
    fn oc05_settled_never_delivered_input_cancellation_admits_successor() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        router.mark_sent("ses_a", "input_a");
        router.settle("ses_a", turn(1));
        apply(
            &mut router,
            "ses_a",
            "session.inbox.cancelled",
            json!({"inboxID":"input_a"}),
            None,
        );
        assert!(router.eligible("ses_a"));
        let cancelled = routed(lane.pop());
        assert_eq!(cancelled.owner, Some(turn(1)));
        assert!(cancelled.late);
        owned(&mut router, "ses_a", "input_b", 2);
        assert_eq!(
            router.state("ses_a").unwrap().execution_owner,
            Some(turn(2))
        );
    }

    #[test]
    fn oc06_grandchild_interactive_routes_to_root_owning_turn() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        apply(
            &mut router,
            "ses_child",
            "session.created",
            json!({"parentID":"ses_a"}),
            None,
        );
        apply(
            &mut router,
            "ses_grandchild",
            "session.created",
            json!({"parentID":"ses_child"}),
            None,
        );
        while lane.pop().is_some() {}
        let form = decode(
            &serde_json::to_vec(&json!({
                "type":"form.created",
                "data":{"form":{"id":"form_grandchild","sessionID":"ses_grandchild"}}
            }))
            .unwrap(),
        )
        .unwrap();
        router.dispatch(form, Instant::now());
        assert_eq!(routed(lane.pop()).owner, Some(turn(1)));
        apply(
            &mut router,
            "ses_grandchild",
            "session.text.ended",
            json!({"assistantMessageID":"grandchild_text","ordinal":0,"text":"excluded"}),
            None,
        );
        assert!(lane.pop().is_none());
    }

    #[test]
    fn oc06_interleaved_sessions_own_only_their_delivered_execution() {
        let mut router = Router::new();
        let a = router.attach("ses_a");
        let b = router.attach("ses_b");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        apply(
            &mut router,
            "ses_a",
            "session.execution.started",
            json!({}),
            None,
        );
        assert_eq!(routed(a.pop()).owner, None);
        apply(
            &mut router,
            "ses_a",
            "session.inbox.delivered",
            json!({"inboxID":"input_a"}),
            None,
        );
        owned(&mut router, "ses_b", "input_b", 2);
        assert_eq!(routed(a.pop()).owner, Some(turn(1)));
        assert_eq!(routed(b.pop()).owner, None);
        assert_eq!(routed(b.pop()).owner, Some(turn(2)));
        assert_eq!(
            router.state("ses_a").unwrap().execution_owner,
            Some(turn(1))
        );
        assert_eq!(
            router.state("ses_b").unwrap().execution_owner,
            Some(turn(2))
        );
    }

    #[test]
    fn oc05_detach_preserves_busy_state_and_dropped_terminal_releases_rule() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        router.detach("ses_a");
        assert!(!router.eligible("ses_a"));
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        let _reopened = router.attach("ses_a");
        assert!(router.eligible("ses_a"));
        assert!(lane.earliest_unobserved(turn(1)).is_some());
    }

    #[test]
    fn oc06_tombstones_keep_late_assistant_and_tool_events_off_successor() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        apply(
            &mut router,
            "ses_a",
            "session.step.started",
            json!({"assistantMessageID":"assistant_a"}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.tool.called",
            json!({"assistantMessageID":"assistant_a","id":"call_a"}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        router.settle("ses_a", turn(1));
        while lane.pop().is_some() {}
        owned(&mut router, "ses_a", "input_b", 2);
        while lane.pop().is_some() {}
        apply(
            &mut router,
            "ses_a",
            "session.tool.success",
            json!({"id":"call_a"}),
            None,
        );
        let tombstone = routed(lane.pop());
        assert_eq!(tombstone.owner, Some(turn(1)));
        assert!(tombstone.late);
    }

    #[test]
    fn oc06_child_interactive_routes_parent_but_child_text_is_excluded() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        apply(
            &mut router,
            "ses_child",
            "session.created",
            json!({"parentID":"ses_a"}),
            None,
        );
        while lane.pop().is_some() {}
        let form = decode(
            &serde_json::to_vec(&json!({
                "type":"form.created",
                "data":{"form":{"id":"form_child","sessionID":"ses_child"}}
            }))
            .unwrap(),
        )
        .unwrap();
        router.dispatch(form, Instant::now());
        assert_eq!(routed(lane.pop()).owner, Some(turn(1)));
        apply(
            &mut router,
            "ses_child",
            "session.text.ended",
            json!({"assistantMessageID":"child_text","ordinal":0,"text":"hidden child"}),
            None,
        );
        assert!(lane.pop().is_none());
    }

    #[test]
    fn oc06_nonincreasing_sequence_fails_session_gaps_do_not() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        apply(&mut router, "ses_a", "session.renamed", json!({}), Some(1));
        apply(&mut router, "ses_a", "session.renamed", json!({}), Some(4));
        assert_eq!(lane.failure(), None);
        apply(&mut router, "ses_a", "session.renamed", json!({}), Some(4));
        assert_eq!(lane.failure(), Some(LaneFailure::Protocol));
    }

    #[test]
    fn oc05_state_applies_terminal_before_lane_overflow_and_usage_is_tainted() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        for _ in 0..14 {
            apply(&mut router, "ses_a", "session.future", json!({}), None);
        }
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        assert_eq!(lane.failure(), Some(LaneFailure::Overflow));
        assert!(router.eligible("ses_a"));
        assert_eq!(lane.earliest_unobserved(turn(1)), Some(17));
    }
    #[test]
    fn oc06_decode_watermark_advances_on_admission_for_exact_owner_only() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        let first = DecodeWatermark::default();
        let second = DecodeWatermark::default();
        lane.track(turn(1), first.clone());
        lane.track(turn(2), second.clone());
        owned(&mut router, "ses_a", "input_a", 1);
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 0);
        assert_eq!(routed(lane.pop()).position, 0);
        assert_eq!(routed(lane.pop()).position, 1);
        apply(
            &mut router,
            "ses_a",
            "session.step.started",
            json!({"assistantMessageID":"assistant_a"}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        router.settle("ses_a", turn(1));
        while lane.pop().is_some() {}
        owned(&mut router, "ses_a", "input_b", 2);
        while lane.pop().is_some() {}
        apply(
            &mut router,
            "ses_a",
            "session.text.ended",
            json!({"assistantMessageID":"assistant_a","ordinal":0,"text":"late"}),
            None,
        );
        assert_eq!(first.get(), 4);
        assert_eq!(second.get(), 1);
        assert_eq!(routed(lane.pop()).position, 4);
    }
    #[test]
    fn oc10_loss_report_requires_sent_unsettled_destination_not_execution_state() {
        let mut router = Router::new();
        let _lane = router.attach("ses_a");
        assert!(!router.has_loss_destination());
        router.register_turn("ses_a", "input_a".into(), turn(1));
        assert!(!router.has_loss_destination());
        router.mark_sent("ses_a", "input_a");
        assert!(router.has_loss_destination());
        apply(
            &mut router,
            "ses_a",
            "session.execution.started",
            json!({}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.inbox.delivered",
            json!({"inboxID":"input_a"}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        assert!(router.eligible("ses_a"));
        assert!(router.has_loss_destination());
        router.settle("ses_a", turn(1));
        assert!(!router.has_loss_destination());
    }

    #[test]
    fn oc10_loss_report_excludes_withdrawn_and_proven_not_accepted_inputs() {
        let mut router = Router::new();
        router.register_turn("ses_a", "input_a".into(), turn(1));
        router.mark_sent("ses_a", "input_a");
        router.not_accepted("ses_a", "input_a");
        assert!(!router.has_loss_destination());
        router.register_turn("ses_a", "input_b".into(), turn(2));
        router.mark_sent("ses_a", "input_b");
        router.never_sent("ses_a", "input_b");
        assert!(!router.has_loss_destination());
        router.register_turn("ses_b", "input_c".into(), turn(3));
        router.mark_sent("ses_b", "input_c");
        router.settle("ses_a", turn(2));
        assert!(router.has_loss_destination());
        router.settle("ses_b", turn(3));
        assert!(!router.has_loss_destination());
    }
    #[test]
    fn oc05_reopen_cleanup_is_once_only_and_blocks_until_stream_proofs_finish() {
        let mut router = Router::new();
        assert!(router.begin_reopen_cleanup("ses_a"));
        assert!(!router.eligible("ses_a"));
        assert!(!router.begin_reopen_cleanup("ses_a"));
        assert!(!router.eligible("ses_a"));
        router.detach("ses_a");
        let _reopened = router.attach("ses_a");
        assert!(!router.eligible("ses_a"));
        router.finish_reopen_cleanup("ses_a");
        assert!(router.eligible("ses_a"));
        assert!(!router.begin_reopen_cleanup("ses_a"));
        router.finish_reopen_cleanup("ses_new");
        assert!(!router.begin_reopen_cleanup("ses_new"));
        assert!(router.eligible("ses_new"));
    }

    #[test]
    fn oc05_proven_never_sent_releases_provisional_sent_phase() {
        let mut router = Router::new();
        router.register_turn("ses_a", "input_a".into(), turn(1));
        router.mark_sent("ses_a", "input_a");
        assert!(!router.eligible("ses_a"));
        router.never_sent("ses_a", "input_a");
        assert!(router.eligible("ses_a"));
        assert!(!router.has_loss_destination());
    }

    #[test]
    fn oc06_keys_of_unowned_running_execution_bind_when_own_input_joins() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        apply(
            &mut router,
            "ses_a",
            "session.execution.started",
            json!({}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.step.started",
            json!({"assistantMessageID":"foreign_step"}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.tool.input.started",
            json!({"assistantMessageID":"foreign_step","id":"foreign_call","name":"shell"}),
            None,
        );
        while lane.pop().is_some() {}
        apply(
            &mut router,
            "ses_a",
            "session.inbox.delivered",
            json!({"inboxID":"input_a"}),
            None,
        );
        while lane.pop().is_some() {}
        apply(
            &mut router,
            "ses_a",
            "session.text.ended",
            json!({"assistantMessageID":"foreign_step","ordinal":0,"text":"joined answer"}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.tool.success",
            json!({"id":"foreign_call"}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.step.ended",
            json!({"assistantMessageID":"foreign_step","finish":"stop","tokens":{"input":7}}),
            None,
        );
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        for _ in 0..4 {
            assert_eq!(routed(lane.pop()).owner, Some(turn(1)));
        }
        assert!(router.eligible("ses_a"));
    }

    #[test]
    fn oc06_late_inbox_events_never_give_old_turn_a_new_execution() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        owned(&mut router, "ses_a", "input_a", 1);
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        router.settle("ses_a", turn(1));
        router.register_turn("ses_a", "input_b".into(), turn(2));
        apply(
            &mut router,
            "ses_a",
            "session.execution.started",
            json!({}),
            None,
        );
        while lane.pop().is_some() {}
        for kind in [
            "session.inbox.enqueued",
            "session.inbox.delivered",
            "session.inbox.cancelled",
        ] {
            apply(
                &mut router,
                "ses_a",
                kind,
                json!({"inboxID":"input_a"}),
                None,
            );
            let old = routed(lane.pop());
            assert_eq!(old.owner, Some(turn(1)));
            assert!(old.late);
            assert_eq!(router.state("ses_a").unwrap().execution_owner, None);
        }
        apply(
            &mut router,
            "ses_a",
            "session.inbox.delivered",
            json!({"inboxID":"input_b"}),
            None,
        );
        assert_eq!(
            router.state("ses_a").unwrap().execution_owner,
            Some(turn(2))
        );
        apply(
            &mut router,
            "ses_a",
            "session.execution.succeeded",
            json!({}),
            None,
        );
        assert_eq!(routed(lane.pop()).owner, Some(turn(2)));
        assert_eq!(routed(lane.pop()).owner, Some(turn(2)));
    }
    #[test]
    fn oc06_joined_step_metadata_keeps_original_start_order_without_replaying() {
        let mut router = Router::new();
        let lane = router.attach("ses_a");
        router.register_turn("ses_a", "input_a".into(), turn(1));
        apply(
            &mut router,
            "ses_a",
            "session.execution.started",
            json!({}),
            None,
        );
        for id in ["step_a", "step_b"] {
            apply(
                &mut router,
                "ses_a",
                "session.step.started",
                json!({"assistantMessageID":id}),
                None,
            );
        }
        while let Some(item) = lane.pop() {
            assert!(routed(Some(item)).owner.is_none());
        }
        apply(
            &mut router,
            "ses_a",
            "session.inbox.delivered",
            json!({"inboxID":"input_a"}),
            None,
        );
        let ownership = routed(lane.pop());
        assert_eq!(ownership.owner, Some(turn(1)));
        assert_eq!(ownership.joined_steps, vec!["step_a", "step_b"]);
        for id in ["step_a", "step_b"] {
            apply(
                &mut router,
                "ses_a",
                "session.text.ended",
                json!({"assistantMessageID":id,"ordinal":0,"text":id}),
                None,
            );
            let text = routed(lane.pop());
            assert_eq!(text.owner, Some(turn(1)));
            assert!(text.joined_steps.is_empty());
        }
        apply(
            &mut router,
            "ses_a",
            "session.inbox.delivered",
            json!({"inboxID":"input_a"}),
            None,
        );
        assert!(routed(lane.pop()).joined_steps.is_empty());
    }
}
