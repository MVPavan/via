//! One ordered consumer per `OpenCode` session, retained across turns.
//!
//! Terminal delivery freezes the lane until its turn seals. A later lane or
//! generation failure therefore cannot replace a terminal already retained.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use via_routes::codex::LossCause;
use via_routes::opencode::events::{EventData, InboxKind};
use via_routes::opencode::router::{Lane, LaneFailure, LaneItem, Routed};
use via_routes::opencode::{GenerationEnd, Server};

use super::normalize::Normalizer;
use crate::driver::latch;
use crate::observation::{Acceptance, ObservationSink};
use crate::runtime::event_stall;
use crate::{
    AcceptanceToken, DriverFailure, DriverHealth, InstanceReport, Observation, ObservationItem,
    ObservationLoss, RouteError, TurnActivity, TurnNumber, UsageSample, VendorTerminal,
    VendorTerminalStatus, VendorTurnId,
};

/// Why ordered delivery ended without a retained terminal.
#[derive(Clone, Copy, Debug)]
pub(super) enum Stop {
    Lane(LaneFailure),
    Generation(GenerationEnd),
    Detached,
}

/// A wakeup is a retained terminal or the end of the delivery path.
#[derive(Clone, Copy, Debug)]
pub(super) enum Decision {
    Terminal,
    Stopped,
}

/// The immutable facts taken by `run_turn` at its outcome boundary.
pub(super) struct Sealed {
    pub(super) terminal: Option<VendorTerminal>,
    pub(super) accepted: bool,
    pub(super) loss: Option<ObservationLoss>,
    pub(super) stop: Option<Stop>,
    pub(super) accounted: bool,
}

struct State {
    normalizer: Normalizer,
    accepted: bool,
    had_terminal: bool,
    terminal: Option<VendorTerminal>,
    complete: bool,
    current: u64,
    last_read: u64,
    loss: Option<ObservationLoss>,
    stop: Option<Stop>,
    // Owned events that preceded the acceptance proof remain bounded by the
    // lane count. They cannot enter Core before `turn.accepted`.
    early: VecDeque<Routed>,
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
                if state.stop.is_some() || self.sealed.is_cancelled() {
                    return Decision::Stopped;
                }
            }
            changed.await;
        }
    }

    /// Cuts off live output atomically with the reserved observation send.
    pub(super) fn seal(&self) -> Sealed {
        let mut state = self.lock();
        if !state.complete || !state.early.is_empty() {
            let position = state
                .early
                .front()
                .map_or(state.current, |event| event.read_order);
            self.loss(&mut state, position);
        }
        let accounted = honest_usage(
            state.terminal.is_some(),
            state.complete && state.early.is_empty(),
            state.last_read,
            self.lane.earliest_unobserved(),
        );
        self.sealed.cancel();
        state.had_terminal |= state.terminal.is_some();
        let result = Sealed {
            terminal: state.terminal.take(),
            accepted: state.accepted,
            loss: state.loss,
            stop: state.stop,
            accounted,
        };
        self.sealed.cancel();
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
                    .earliest_unobserved()
                    .unwrap_or(state.current.saturating_add(1))
            },
            |event| event.read_order,
        );
        self.loss(&mut state, position);
        self.changed.notify_waiters();
    }
}

/// The session's only sink producer, preserving order across turn tombstones.
pub(super) struct Registration {
    lane: Arc<Lane>,
    sink: ObservationSink,
    health: Arc<watch::Sender<DriverHealth>>,
    generation: u64,
    turns: Mutex<BTreeMap<TurnNumber, Arc<Delivery>>>,
    last_at: Mutex<Instant>,
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
                accepted: false,
                had_terminal: false,
                terminal: None,
                complete: true,
                current: 0,
                last_read: 0,
                loss: None,
                stop: None,
                early: VecDeque::new(),
            }),
            changed: Notify::new(),
            sealed: CancellationToken::new(),
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
        }
    }

    fn health_failure(&self, failure: LaneFailure) {
        let turn = self
            .turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .next_back()
            .copied();
        let cause = match failure {
            LaneFailure::Overflow => DriverFailure::ObservationOverflow,
            LaneFailure::Protocol => turn.map_or(DriverFailure::ObservationOverflow, |turn| {
                DriverFailure::Route(RouteError::Protocol {
                    turn,
                    detail: "OpenCode session event protocol failure",
                })
            }),
        };
        latch(&self.health, cause);
    }

    fn generation_health(&self, end: GenerationEnd) {
        let GenerationEnd::Lost(loss) = end else {
            return;
        };
        let turn = self
            .turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .next_back()
            .copied();
        let cause = match loss.cause {
            LossCause::ServerLost => DriverFailure::ServerLost,
            LossCause::Overflow => DriverFailure::ObservationOverflow,
            LossCause::Protocol => turn.map_or(DriverFailure::ObservationOverflow, |turn| {
                DriverFailure::Route(RouteError::Protocol {
                    turn,
                    detail: "OpenCode server event protocol failure",
                })
            }),
            LossCause::TransportLost => turn.map_or(DriverFailure::OwnedTask, |turn| {
                DriverFailure::Route(RouteError::TransportLost { turn })
            }),
        };
        latch(&self.health, cause);
    }

    /// Ordered queue entries always precede the server's loss signal. A stopped
    /// consumer cannot leave a pending sink reservation capable of sending later.
    pub(super) async fn run(self: Arc<Self>, cancel: CancellationToken, server: Arc<Server>) {
        loop {
            if let Some(failure) = self.lane.failure() {
                self.health_failure(failure);
            }
            if let Some(item) = self.lane.pop() {
                if !self.process(item).await {
                    break;
                }
                continue;
            }
            let next = tokio::select! {
                biased;
                item=self.lane.recv()=>item,
                end=server.wait_end()=>{
                    self.generation_health(end);
                    // The signal follows ingress admission; drain the queue
                    // before publishing it to any nonterminal turn.
                    while let Some(item)=self.lane.pop() {
                        if !self.process(item).await { return; }
                    }
                    self.fail(Stop::Generation(end));
                    return;
                }
                ()=cancel.cancelled()=>{self.fail(Stop::Detached);return;}
            };
            if let Some(item) = next {
                if !self.process(item).await {
                    break;
                }
            } else {
                self.fail(self.lane.failure().map_or(Stop::Detached, Stop::Lane));
                return;
            }
        }
    }

    async fn process(&self, item: LaneItem) -> bool {
        let (owner, order, position, at) = match &item {
            LaneItem::Accepted {
                owner,
                read_order,
                decoded_at,
                position,
                ..
            } => (*owner, *read_order, *position, *decoded_at),
            LaneItem::Event(event) => {
                let Some(owner) = event.owner else {
                    return true;
                };
                (owner, event.read_order, event.position, event.decoded_at)
            }
        };
        let Some(turn) = self.turn(owner) else {
            return true;
        };
        turn.activity.record(at);
        let accepted = match &item {
            LaneItem::Accepted { .. } => true,
            LaneItem::Event(routed) => matches!(
                &routed.event.data,
                EventData::Inbox { kind: InboxKind::Enqueued, id } if id == turn.input.as_str()
            ),
        };
        if accepted {
            if !self.accept(&turn, order, at).await {
                return false;
            }
            let early = { turn.lock().early.drain(..).collect::<Vec<_>>() };
            for event in early {
                if !self.event(&turn, event).await {
                    return false;
                }
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
                    if state.early.len() >= 16 {
                        self.loss_stop(&turn, &mut state, event.read_order);
                        return false;
                    }
                    state.early.push_back(event.clone());
                    true
                } else {
                    false
                }
            };
            if !pending {
                return self.event(&turn, event).await;
            }
        }
        true
    }

    async fn accept(&self, turn: &Arc<Delivery>, order: u64, at: Instant) -> bool {
        {
            let mut state = turn.lock();
            if state.accepted || turn.sealed.is_cancelled() {
                return true;
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
        if !self.send(turn, order, at, observation, false).await {
            return false;
        }
        {
            let mut state = turn.lock();
            state.complete = true;
            state.last_read = state.last_read.max(order);
        }
        true
    }

    fn loss_stop(&self, turn: &Delivery, state: &mut State, order: u64) {
        turn.loss(state, order);
        state.stop = Some(Stop::Lane(LaneFailure::Overflow));
        latch(&self.health, DriverFailure::ObservationOverflow);
        turn.changed.notify_waiters();
    }

    async fn event(&self, turn: &Arc<Delivery>, event: Routed) -> bool {
        let (observations, terminal, late) = {
            let mut state = turn.lock();
            if turn.sealed.is_cancelled() && !state.accepted {
                return true;
            }
            let late = turn.sealed.is_cancelled();
            if !late {
                state.current = event.read_order;
                state.complete = false;
            }
            // Joining an already-running execution transfers ordered step
            // identity metadata, never its earlier observations or samples.
            state.normalizer.register_started_steps(&event.joined_steps);
            let mut observations = state.normalizer.items(&event.event.data, event.decoded_at);
            let terminal = state
                .normalizer
                .terminal(&event.event.data, event.decoded_at);
            if terminal
                .as_ref()
                .is_some_and(|terminal| terminal.status == VendorTerminalStatus::Completed)
                && (!late || !state.had_terminal)
            {
                observations.extend(
                    state
                        .normalizer
                        .final_text()
                        .into_iter()
                        .map(Observation::FinalText),
                );
            }
            (observations, terminal, late)
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
            if !self
                .send(turn, event.read_order, event.decoded_at, observation, late)
                .await
            {
                return false;
            }
        }
        if let Some(mut terminal) = terminal
            && late
        {
            let eligible = {
                let mut state = turn.lock();
                if state.had_terminal {
                    false
                } else {
                    state.had_terminal = true;
                    true
                }
            };
            if eligible {
                terminal.usage = Some(UsageSample::default());
                terminal.cost = None;
                terminal.vendor = None;
                if !self
                    .send(
                        turn,
                        event.read_order,
                        event.decoded_at,
                        Observation::LateTerminal(terminal),
                        true,
                    )
                    .await
                {
                    return false;
                }
            }
        }
        {
            let mut state = turn.lock();
            if !turn.sealed.is_cancelled() {
                state.complete = true;
                state.last_read = state.last_read.max(event.read_order);
            }
        }
        turn.activity.delivered_through(event.position);
        if retained {
            turn.changed.notify_waiters();
            // Later ingress failures and events remain behind the terminal until
            // `run_turn` has taken it. No server-scoped state decides this result.
            turn.sealed.cancelled().await;
        }
        true
    }

    async fn send(
        &self,
        turn: &Arc<Delivery>,
        order: u64,
        decoded_at: Instant,
        observation: Observation,
        late: bool,
    ) -> bool {
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
            reserved=self.sink.reserve(&item,event_stall())=>reserved,
            ()=turn.sealed.cancelled(),if !late=>return true,
        };
        if let Ok(reserved) = reserved {
            let _state = turn.lock();
            if late || !turn.sealed.is_cancelled() {
                reserved.send(item);
            }
            true
        } else {
            let mut state = turn.lock();
            self.loss_stop(turn, &mut state, order);
            drop(state);
            self.fail(Stop::Lane(LaneFailure::Overflow));
            false
        }
    }
}

/// A positive accounting predicate: a retained terminal, complete delivery,
/// and no rejected route event preceding the last event delivered by the turn.
fn honest_usage(terminal: bool, delivered: bool, last_read: u64, rejected: Option<u64>) -> bool {
    terminal && delivered && rejected.is_none_or(|order| order > last_read)
}

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;
