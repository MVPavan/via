//! `opencode.md` §7–§11: ordered observations and finite failure evidence per turn.
//!
//! Terminal delivery freezes the lane until its turn seals. A later lane or
//! generation failure therefore cannot replace a terminal already retained.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use via_routes::codex::LossCause;
use via_routes::opencode::declines::DeclineNotice;
use via_routes::opencode::events::{EventData, InboxKind};
use via_routes::opencode::router::{
    LANE_MESSAGES, Lane, LaneFailure, LaneItem, Routed, StagingPermit,
};
use via_routes::opencode::{GenerationEnd, Server};

mod declines;
mod events;

use declines::PermissionProof;

use super::normalize::Normalizer;
use crate::driver::latch;
use crate::observation::{Acceptance, ObservationSink};
use crate::runtime::event_stall;
use crate::{
    AcceptanceToken, Cleanup, DriverFailure, DriverHealth, InstanceReport, Observation,
    ObservationItem, ObservationLoss, RouteError, TurnActivity, TurnNumber, VendorTerminal,
    VendorTerminalStatus, VendorTurnId,
};

/// `opencode.md` §7.1: whether the ordered consumer may take another entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryOutcome {
    Continue,
    Stopped,
}

impl DeliveryOutcome {
    fn is_stopped(self) -> bool {
        self == Self::Stopped
    }

    #[cfg(test)]
    fn is_continue(self) -> bool {
        self == Self::Continue
    }
}

/// Why ordered delivery ended without a retained terminal.
#[derive(Clone, Copy, Debug)]
pub(super) enum Stop {
    Lane(LaneFailure),
    Generation(GenerationEnd),
    Detached,
    SubmissionUnknown,
    DeclineFailed,
    ResponseLimit,
    TextOverflow,
}

/// A wakeup is a retained terminal or the end of the delivery path.
#[derive(Clone, Copy, Debug)]
pub(super) enum Decision {
    Terminal,
    Stopped,
}

/// The immutable facts taken by `run_turn` at its outcome boundary.
pub(super) struct Sealed {
    pub(super) cleanup: Cleanup,
    pub(super) terminal: Option<VendorTerminal>,
    pub(super) accepted: bool,
    pub(super) loss: Option<ObservationLoss>,
    pub(super) stop: Option<Stop>,
    pub(super) accounted: bool,
}

/// §6, §11: native requests and their HTTP proofs retain order until acceptance.
enum Early {
    Event(Box<Routed>),
    Decline {
        notice: Box<DeclineNotice>,
        read_order: u64,
        position: u64,
        decoded_at: Instant,
        staging: Option<StagingPermit>,
    },
}

impl Early {
    fn read_order(&self) -> u64 {
        match self {
            Self::Event(event) => event.read_order,
            Self::Decline { read_order, .. } => *read_order,
        }
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "delivery, acceptance, terminal history and completeness are independent evidence"
)]
struct State {
    normalizer: Normalizer,
    delivered_once: bool,
    tools: HashSet<String>,
    // §11: HTTP decline proof may follow its native tool failure and shutdown.
    permission_proofs: BTreeMap<(String, String), PermissionProof>,
    deferred: VecDeque<Routed>,
    accepted: bool,
    had_terminal: bool,
    terminal: Option<VendorTerminal>,
    post_cutoff: Option<VendorTerminal>,
    complete: bool,
    current: u64,
    last_read: u64,
    loss: Option<ObservationLoss>,
    stop: Option<Stop>,
    // Owned events that preceded the acceptance proof remain bounded by the
    // lane count. They cannot enter Core before `turn.accepted`.
    early: VecDeque<Early>,
}

/// One turn's delivery fence; it also owns its late-event normalizer.
pub(super) struct Delivery {
    input: VendorTurnId,
    turn: TurnNumber,
    instance: Option<InstanceReport>,
    activity: TurnActivity,
    generation: u64,
    lane: Arc<Lane>,
    state: Mutex<State>,
    changed: Notify,
    sealed: CancellationToken,
    cleanup_enabled: CancellationToken,
}

impl Delivery {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits without consuming the terminal, beside the driver's control path.
    pub(super) async fn decision(&self) -> Decision {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.lock();
                if state.terminal.is_some() && (state.complete || state.stop.is_some()) {
                    return Decision::Terminal;
                }
                if state.stop.is_some()
                    || self.lane.response_limit(self.turn).is_some()
                    || self.sealed.is_cancelled()
                {
                    return Decision::Stopped;
                }
            }
            changed.await;
        }
    }

    /// §7.1: the owner used by server-scoped HTTP response evidence.
    pub(super) fn owner(&self) -> TurnNumber {
        self.turn
    }

    /// §8–§9: wake after the server's owning-turn ledger records a response cap.
    pub(super) fn response_limit(&self) {
        self.changed.notify_waiters();
    }

    /// §9: final assistant text overflow belongs only to this turn.
    pub(super) fn text_overflow(&self) {
        let mut state = self.lock();
        if !self.sealed.is_cancelled() {
            state.stop = Some(Stop::TextOverflow);
            let position = state.current;
            self.loss(&mut state, position);
            self.changed.notify_waiters();
        }
    }

    /// §7.4: records actual first-byte interrupt evidence supplied by Wire.
    pub(super) fn note_interrupt_sent(&self) {
        self.lock().normalizer.note_interrupt_sent();
    }

    /// §7.4: the retained terminal starts the tool cleanup grace.
    pub(super) fn terminal_at(&self) -> Option<Instant> {
        self.lock().terminal.as_ref().map(|terminal| terminal.at)
    }

    /// §7.4: input delivery determines whether inbox cancellation is eligible.
    pub(super) fn delivered(&self) -> bool {
        self.lock().delivered_once
    }

    /// §7.4: acknowledgement is native evidence, independent of tool cleanup.
    pub(super) fn acknowledged_until(&self, cutoff: Option<crate::Deadline>) -> bool {
        self.lock().terminal.as_ref().is_some_and(|terminal| {
            terminal.status == VendorTerminalStatus::Interrupted
                && cutoff.is_none_or(|cutoff| terminal.at <= cutoff.instant())
        })
    }

    /// §7.4: cleanup can consume later tool ends without losing the retained terminal.
    pub(super) async fn cleanup(&self, by: crate::Deadline) -> Cleanup {
        self.cleanup_enabled.cancel();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.lock().tools.is_empty() {
                return Cleanup::Quiescent;
            }
            // Notify and timer do not consume cleanup evidence when cancelled.
            tokio::select! {
                () = &mut changed => {},
                () = tokio::time::sleep_until(by.instant()) => return Cleanup::Uncertain,
            }
        }
    }

    /// Cuts off live output atomically with the reserved observation send (§7.3).
    pub(super) fn seal(&self) -> Sealed {
        self.seal_until(None)
    }

    /// §7.4: post-cutoff native evidence belongs to the accepted turn's late path.
    pub(super) fn seal_until(&self, cutoff: Option<crate::Deadline>) -> Sealed {
        let mut state = self.lock();
        // §8–§9: a bounded marker may be omitted by a full lane. Its finite
        // owner ledger survives that loss and respects the caller's force cutoff.
        if self
            .lane
            .response_limit(self.turn)
            .is_some_and(|at| cutoff.is_none_or(|cutoff| at <= cutoff.instant()))
        {
            state.stop = Some(Stop::ResponseLimit);
        }
        if state
            .terminal
            .as_ref()
            .is_some_and(|terminal| cutoff.is_some_and(|cutoff| terminal.at > cutoff.instant()))
        {
            state.post_cutoff = state.terminal.take();
        }
        if !state.complete || !state.early.is_empty() || !state.deferred.is_empty() {
            let position = state.early.front().map_or(state.current, Early::read_order);
            self.loss(&mut state, position);
        }
        let accounted = honest_usage(
            state.terminal.is_some(),
            state.complete
                && state.loss.is_none()
                && state.early.is_empty()
                && state.deferred.is_empty(),
            state.last_read,
            self.lane.earliest_unobserved(self.turn),
        );
        self.sealed.cancel();
        state.had_terminal |= state.terminal.is_some();
        let result = Sealed {
            cleanup: if state.tools.is_empty() {
                Cleanup::Quiescent
            } else {
                Cleanup::Uncertain
            },
            terminal: state.terminal.take(),
            accepted: state.accepted,
            loss: state.loss,
            stop: state.stop,
            accounted,
        };
        self.changed.notify_waiters();
        result
    }

    fn loss(&self, state: &mut State, position: u64) {
        state.complete = false;
        if state.loss.is_none() {
            state.loss = Some(ObservationLoss {
                trigger: self.turn,
                generation: self.generation,
                first_unqueued: position.max(1),
                omitted: ObservationLoss::UNKNOWN,
            });
        }
    }

    fn stop(&self, stop: Stop) {
        let mut state = self.lock();
        if self.sealed.is_cancelled() || state.terminal.is_some() {
            return;
        }
        state.stop.get_or_insert(stop);
        let position = state.early.front().map_or_else(
            || {
                self.lane
                    .earliest_unobserved(self.turn)
                    .unwrap_or(state.current.saturating_add(1))
            },
            Early::read_order,
        );
        self.loss(&mut state, position);
        self.changed.notify_waiters();
    }
}

/// The session's only sink producer, preserving order across turn tombstones.
pub(super) struct Registration {
    pub(super) lane: Arc<Lane>,
    sink: ObservationSink,
    health: Arc<watch::Sender<DriverHealth>>,
    generation: u64,
    turns: Mutex<BTreeMap<TurnNumber, Arc<Delivery>>>,
    last_at: Mutex<Instant>,
}

/// C2 §3, §9: dropping the consumer closes late-output retention even on task cancellation.
struct OutputCutoff<'a>(&'a Registration);

impl Drop for OutputCutoff<'_> {
    fn drop(&mut self) {
        self.0.retire_output();
    }
}

impl Registration {
    pub(super) fn new(
        lane: Arc<Lane>,
        sink: ObservationSink,
        health: Arc<watch::Sender<DriverHealth>>,
        generation: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            lane,
            sink,
            health,
            generation,
            turns: Mutex::new(BTreeMap::new()),
            last_at: Mutex::new(Instant::now()),
        })
    }

    /// Installs the caller key before the prompt can be written.
    pub(super) fn admit(
        &self,
        input_id: String,
        turn: TurnNumber,
        activity: TurnActivity,
        instance: Option<InstanceReport>,
    ) -> Arc<Delivery> {
        // The owning driver supplies the deterministic, nonempty caller ID.
        #[expect(
            clippy::expect_used,
            reason = "driver-generated OpenCode input IDs are nonempty"
        )]
        let input = VendorTurnId::try_from(input_id).expect("nonempty OpenCode caller key");
        self.lane.track(turn, activity.decode_watermark());
        let delivery = Arc::new(Delivery {
            input,
            turn,
            instance,
            activity,
            generation: self.generation,
            lane: self.lane.clone(),
            state: Mutex::new(State {
                normalizer: Normalizer::new(),
                delivered_once: false,
                tools: HashSet::new(),
                permission_proofs: BTreeMap::new(),
                deferred: VecDeque::new(),
                accepted: false,
                had_terminal: false,
                terminal: None,
                post_cutoff: None,
                complete: true,
                current: 0,
                last_read: 0,
                loss: None,
                stop: None,
                early: VecDeque::new(),
            }),
            changed: Notify::new(),
            sealed: CancellationToken::new(),
            cleanup_enabled: CancellationToken::new(),
        });
        self.turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(turn, delivery.clone());
        delivery
    }

    fn turn(&self, owner: TurnNumber) -> Option<Arc<Delivery>> {
        self.turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&owner)
            .cloned()
    }

    fn fail(&self, stop: Stop) {
        let turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        for turn in turns.values() {
            turn.stop(stop);
            turn.lock().normalizer.retire_output();
        }
    }

    fn retire_output(&self) {
        let turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        for turn in turns.values() {
            turn.lock().normalizer.retire_output();
        }
    }

    fn live_turn(&self) -> Option<TurnNumber> {
        self.turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .find_map(|(number, delivery)| (!delivery.sealed.is_cancelled()).then_some(*number))
    }

    fn health_failure(&self, failure: LaneFailure) {
        let cause = match failure {
            LaneFailure::Overflow => DriverFailure::ObservationOverflow,
            LaneFailure::Protocol => {
                self.live_turn()
                    .map_or(DriverFailure::ObservationOverflow, |turn| {
                        DriverFailure::Route(RouteError::Protocol {
                            turn,
                            detail: "OpenCode session event protocol failure",
                        })
                    })
            }
        };
        latch(&self.health, cause);
    }

    fn generation_health(&self, end: GenerationEnd) {
        let GenerationEnd::Lost(loss) = end else {
            return;
        };
        let cause = self.live_turn().map_or_else(
            || match loss.cause {
                LossCause::ServerLost => DriverFailure::ServerLost,
                LossCause::Overflow | LossCause::Protocol => DriverFailure::ObservationOverflow,
                LossCause::TransportLost => DriverFailure::OwnedTask,
            },
            |turn| match generation_route_error(loss.cause, turn) {
                RouteError::ServerLost { .. } => DriverFailure::ServerLost,
                RouteError::Overflow { .. } => DriverFailure::ObservationOverflow,
                error @ (RouteError::Protocol { .. }
                | RouteError::TransportLost { .. }
                | RouteError::ProcessExited { .. }
                | RouteError::Store { .. }
                | RouteError::Stopped { .. }
                | RouteError::Deadline { .. }
                | RouteError::ForceStopped { .. }
                | RouteError::HandshakeRefused { .. }
                | RouteError::InvalidParam { .. }
                | RouteError::ResumeMismatch { .. }) => DriverFailure::Route(error),
            },
        );
        latch(&self.health, cause);
    }

    /// Ordered queue entries always precede the server's loss signal. A stopped
    /// consumer cannot leave a pending sink reservation capable of sending later.
    pub(super) async fn run(self: Arc<Self>, cancel: CancellationToken, server: Arc<Server>) {
        let _output_cutoff = OutputCutoff(&self);
        loop {
            if let Some(failure) = self.lane.failure() {
                self.health_failure(failure);
            }
            if let Some(item) = self.lane.pop() {
                if self.process(item).await.is_stopped() {
                    break;
                }
                continue;
            }
            // Lane recv removes an item only when ready; end/cancel waits consume no state.
            let next = tokio::select! {
                biased;
                item = self.lane.recv() => item,
                end = server.wait_end() => {
                    self.generation_health(end);
                    // The signal follows ingress admission; drain the queue
                    // before publishing it to any nonterminal turn.
                    while let Some(item) = self.lane.pop() {
                        if self.process(item).await.is_stopped() {
                            return;
                        }
                    }
                    if self.flush_deferred().await.is_stopped() {
                        return;
                    }
                    self.fail(Stop::Generation(end));
                    return;
                }
                () = cancel.cancelled() => {
                    self.fail(Stop::Detached);
                    return;
                }
            };
            if let Some(item) = next {
                if self.process(item).await.is_stopped() {
                    break;
                }
            } else {
                if self.flush_deferred().await.is_stopped() {
                    return;
                }
                self.fail(self.lane.failure().map_or(Stop::Detached, Stop::Lane));
                return;
            }
        }
    }

    /// §8–§9: cap notices preserve their owner, including post-cutoff observations.
    async fn response_limit_notice(
        &self,
        turn: &Arc<Delivery>,
        order: u64,
        position: u64,
        at: Instant,
    ) -> DeliveryOutcome {
        if turn.sealed.is_cancelled() {
            if !turn.lock().accepted {
                self.lane.reject_unobserved(Some(turn.turn), order);
                return DeliveryOutcome::Continue;
            }
            return self
                .send(
                    turn,
                    order,
                    at,
                    Observation::Warning(crate::Warning {
                        code: "http_response_limit",
                        message: "an HTTP response exceeded the OpenCode route bounds".into(),
                        data: None,
                    }),
                    true,
                )
                .await;
        }
        // The failure may be excluded by force_at at sealing. Count this
        // non-emitted warning only against its original owning turn window.
        self.lane.reject_unobserved(Some(turn.turn), order);
        turn.response_limit();
        turn.activity.delivered_through(position);
        DeliveryOutcome::Continue
    }

    async fn process(&self, item: LaneItem) -> DeliveryOutcome {
        let Some((owner, order, position, at)) = lane_identity(&item) else {
            return DeliveryOutcome::Continue;
        };
        let Some(turn) = self.turn(owner) else {
            // A replaced driver's tombstone is not this driver's observation.
            // Count its rejection against its own window, never a successor's.
            self.lane.reject_unobserved(Some(owner), order);
            return DeliveryOutcome::Continue;
        };
        turn.activity.record(at);
        if matches!(item, LaneItem::ResponseLimit { .. }) {
            return self.response_limit_notice(&turn, order, position, at).await;
        }
        if matches!(item, LaneItem::Inconclusive { .. }) {
            let mut state = turn.lock();
            if !state.accepted && !turn.sealed.is_cancelled() {
                state.stop = Some(Stop::SubmissionUnknown);
                turn.changed.notify_waiters();
            }
            turn.activity.delivered_through(position);
            return DeliveryOutcome::Continue;
        }
        if let LaneItem::Decline {
            notice, staging, ..
        } = item
        {
            {
                let mut state = turn.lock();
                if !state.accepted && !turn.sealed.is_cancelled() {
                    if state.early.len() >= LANE_MESSAGES {
                        self.loss_stop(&turn, &mut state, order);
                        return DeliveryOutcome::Stopped;
                    }
                    state.early.push_back(Early::Decline {
                        notice,
                        read_order: order,
                        position,
                        decoded_at: at,
                        staging,
                    });
                    return DeliveryOutcome::Continue;
                }
            }
            let outcome = self.decline(&turn, &notice, order, position, at).await;
            drop(staging);
            return outcome;
        }
        let accepted = match &item {
            LaneItem::Decline { .. }
            | LaneItem::ResponseLimit { .. }
            | LaneItem::Inconclusive { .. } => false,
            LaneItem::Accepted { .. } => true,
            LaneItem::Event(routed) => matches!(
                &routed.event.data,
                EventData::Inbox { kind: InboxKind::Enqueued, id } if id == turn.input.as_str()
            ),
        };
        if accepted {
            if self.accept(&turn, order, at).await.is_stopped() {
                return DeliveryOutcome::Stopped;
            }
            if self.release_early(&turn).await.is_stopped() {
                return DeliveryOutcome::Stopped;
            }
            // Acceptance can follow buffered owned events. Its watermark
            // cannot acknowledge those events until their outputs reach Core.
            turn.activity.delivered_through(position);
        }
        if let LaneItem::Event(event) = item {
            let event = *event;
            let pending = {
                let mut state = turn.lock();
                if !state.accepted && !turn.sealed.is_cancelled() {
                    if state.early.len() >= LANE_MESSAGES {
                        self.loss_stop(&turn, &mut state, event.read_order);
                        return DeliveryOutcome::Stopped;
                    }
                    state.early.push_back(Early::Event(Box::new(event.clone())));
                    true
                } else {
                    false
                }
            };
            if !pending {
                return self.event(&turn, event).await;
            }
        }
        DeliveryOutcome::Continue
    }

    /// §6, §11: release buffered observations only after acceptance reached Core.
    async fn release_early(&self, turn: &Arc<Delivery>) -> DeliveryOutcome {
        let early = { turn.lock().early.drain(..).collect::<Vec<_>>() };
        for item in early {
            let outcome = match item {
                Early::Event(event) => self.event(turn, *event).await,
                Early::Decline {
                    notice,
                    read_order,
                    position,
                    decoded_at,
                    staging,
                } => {
                    let outcome = self
                        .decline(turn, &notice, read_order, position, decoded_at)
                        .await;
                    drop(staging);
                    outcome
                }
            };
            if outcome.is_stopped() {
                return DeliveryOutcome::Stopped;
            }
        }
        DeliveryOutcome::Continue
    }

    async fn accept(&self, turn: &Arc<Delivery>, order: u64, at: Instant) -> DeliveryOutcome {
        {
            let mut state = turn.lock();
            if state.accepted
                || turn.sealed.is_cancelled()
                || matches!(state.stop, Some(Stop::SubmissionUnknown))
            {
                return DeliveryOutcome::Continue;
            }
            state.accepted = true;
            state.current = order;
            state.complete = false;
        }
        let observation = Observation::Accepted(Acceptance {
            correlation: AcceptanceToken::FIRST,
            vendor_turn_id: Some(turn.input.clone()),
            instance: turn.instance.clone(),
        });
        if self
            .send(turn, order, at, observation, false)
            .await
            .is_stopped()
        {
            return DeliveryOutcome::Stopped;
        }
        {
            let mut state = turn.lock();
            state.complete = true;
            state.last_read = state.last_read.max(order);
        }
        DeliveryOutcome::Continue
    }

    fn loss_stop(&self, turn: &Delivery, state: &mut State, order: u64) {
        turn.loss(state, order);
        state.stop = Some(Stop::Lane(LaneFailure::Overflow));
        latch(&self.health, DriverFailure::ObservationOverflow);
        turn.changed.notify_waiters();
    }

    async fn event(&self, turn: &Arc<Delivery>, event: Routed) -> DeliveryOutcome {
        let event = match self.defer_permission(turn, event) {
            declines::Deferral::Ready(event) => *event,
            declines::Deferral::Waiting => return DeliveryOutcome::Continue,
            declines::Deferral::Stopped => return DeliveryOutcome::Stopped,
        };
        let Some(events::Normalized {
            observations,
            terminal,
            late,
        }) = self.normalize_event(turn, &event)
        else {
            return DeliveryOutcome::Continue;
        };
        // Retain terminal evidence before attempting its final-text outputs.
        // A stalled output invalidates usage, but cannot erase that evidence.
        let mut terminal = terminal;
        let retained = if !late && terminal.is_some() {
            let mut state = turn.lock();
            if turn.sealed.is_cancelled() {
                false
            } else {
                state.terminal = terminal.take();
                true
            }
        } else {
            false
        };
        for observation in observations {
            if self
                .send(turn, event.read_order, event.decoded_at, observation, late)
                .await
                .is_stopped()
            {
                return DeliveryOutcome::Stopped;
            }
        }
        if let Some(terminal) = terminal
            && late
            && self.publish_late(turn, &event, terminal).await.is_stopped()
        {
            return DeliveryOutcome::Stopped;
        }
        {
            let mut state = turn.lock();
            if !turn.sealed.is_cancelled() {
                state.complete = true;
                state.last_read = state.last_read.max(event.read_order);
            }
        }
        turn.activity.delivered_through(event.position);
        turn.changed.notify_waiters();
        if retained {
            turn.changed.notify_waiters();
            // Later ingress failures and events remain behind the terminal until
            // `run_turn` has taken it. No server-scoped state decides this result.
            // Both waits only release the retained terminal's delivery fence.
            tokio::select! {
                () = turn.sealed.cancelled() => {},
                () = turn.cleanup_enabled.cancelled() => {},
            }
        }
        let post_cutoff = { turn.lock().post_cutoff.take() };
        if let Some(terminal) = post_cutoff
            && self.publish_late(turn, &event, terminal).await.is_stopped()
        {
            return DeliveryOutcome::Stopped;
        }
        DeliveryOutcome::Continue
    }

    async fn send(
        &self,
        turn: &Arc<Delivery>,
        order: u64,
        decoded_at: Instant,
        observation: Observation,
        late: bool,
    ) -> DeliveryOutcome {
        let at = {
            let mut previous = self.last_at.lock().unwrap_or_else(PoisonError::into_inner);
            *previous = (*previous).max(decoded_at);
            *previous
        };
        let item = ObservationItem {
            at,
            vendor_turn: Some(turn.input.clone()),
            observation,
        };
        // Reservation may be cancelled only before enqueue. The synchronous
        // send under the seal lock cannot race the driver's output cutoff.
        let reserved = tokio::select! {
            reserved = self.sink.reserve(&item, event_stall()) => reserved,
            () = turn.sealed.cancelled(), if !late => return DeliveryOutcome::Continue,
        };
        if let Ok(reserved) = reserved {
            let _state = turn.lock();
            if late || !turn.sealed.is_cancelled() {
                reserved.send(item);
            }
            DeliveryOutcome::Continue
        } else {
            let mut state = turn.lock();
            self.loss_stop(turn, &mut state, order);
            drop(state);
            self.fail(Stop::Lane(LaneFailure::Overflow));
            DeliveryOutcome::Stopped
        }
    }
}

/// §7.1: every routed marker carries the same session-lane ownership evidence.
fn lane_identity(item: &LaneItem) -> Option<(TurnNumber, u64, u64, Instant)> {
    match item {
        LaneItem::Decline {
            owner,
            read_order,
            position,
            decoded_at,
            ..
        }
        | LaneItem::Inconclusive {
            owner,
            read_order,
            position,
            decoded_at,
            ..
        }
        | LaneItem::ResponseLimit {
            owner,
            read_order,
            position,
            decoded_at,
            ..
        }
        | LaneItem::Accepted {
            owner,
            read_order,
            position,
            decoded_at,
            ..
        } => Some((*owner, *read_order, *position, *decoded_at)),
        LaneItem::Event(event) => {
            let owner = event.owner?;
            Some((owner, event.read_order, event.position, event.decoded_at))
        }
    }
}

/// `opencode.md` §10: one loss-cause mapping for health and turn outcomes.
pub(super) fn generation_route_error(cause: LossCause, turn: TurnNumber) -> RouteError {
    match cause {
        LossCause::ServerLost => RouteError::ServerLost { turn },
        LossCause::Protocol => RouteError::Protocol {
            turn,
            detail: "the server event envelope did not match the protocol",
        },
        LossCause::Overflow => RouteError::Overflow { turn },
        LossCause::TransportLost => RouteError::TransportLost { turn },
    }
}

/// C2 AD6, `opencode.md` §12: a retained terminal, complete delivery, and no rejection
/// in this turn's live window preceding the last event delivered by the turn.
fn honest_usage(terminal: bool, delivered: bool, last_read: u64, rejected: Option<u64>) -> bool {
    terminal && delivered && rejected.is_none_or(|order| order > last_read)
}

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;
