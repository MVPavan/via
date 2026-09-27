//! Per-session dispatch state (C1 §7.3): the session's queue of receipted,
//! unsubmitted turns and whether its one dispatcher task exists. Whether the
//! queue head may run is decided from durable state (`Engine::decide`), never
//! remembered here.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use tokio::sync::Notify;

use super::journal::Head;
use super::lock;
use crate::TurnNumber;

/// Most queued turns one session holds (C1 P6), also enforced by Store.
pub(super) const SESSION_QUEUE_LIMIT: u32 = via_store::SESSION_QUEUE_LIMIT;

/// Most queued turns the daemon holds across sessions (runtime §8); also the
/// dispatcher-start channel's capacity.
pub(super) const DAEMON_QUEUE_LIMIT: usize = 128;

/// Active private connections daemon-wide (runtime §8, design §11).
pub(super) const CONNECTION_SLOTS: usize = 4;

/// First delay after a failed Store read or while waiting on an unowned predecessor.
const RETRY_MIN: Duration = Duration::from_millis(250);

/// Longest delay between retries.
const RETRY_MAX: Duration = Duration::from_secs(5);

/// Whether the session's dispatcher task exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Dispatcher {
    None,
    /// Requested from daemon main, not yet running.
    Starting,
    Live,
}

/// Mutable dispatch state of one session.
struct State {
    /// Receipted turns without a confirmed submission, in number order.
    queue: VecDeque<TurnNumber>,
    dispatcher: Dispatcher,
}

/// One session's queue, dispatcher wake and the event head every writer of
/// it shares. A clone of `head` is a writer lease: the slot is not retired
/// while anyone else holds one.
pub(super) struct Slot {
    pub(super) head: Arc<Head>,
    state: StdMutex<State>,
    wake: Notify,
}

impl Slot {
    pub(super) fn new(head: Arc<Head>) -> Arc<Self> {
        Arc::new(Self {
            head,
            state: StdMutex::new(State {
                queue: VecDeque::new(),
                dispatcher: Dispatcher::None,
            }),
            wake: Notify::new(),
        })
    }

    /// Adds a receipted turn in number order and wakes the
    /// dispatcher; true when no dispatcher exists and one must be started.
    pub(super) fn enqueue(&self, turn: TurnNumber) -> bool {
        let start = {
            let mut state = lock(&self.state);
            let at = state.queue.partition_point(|queued| *queued < turn);
            if state.queue.get(at) != Some(&turn) {
                state.queue.insert(at, turn);
            }
            let start = state.dispatcher == Dispatcher::None;
            if start {
                state.dispatcher = Dispatcher::Starting;
            }
            start
        };
        self.wake();
        start
    }

    /// Wakes the dispatcher; wakes before it waits coalesce into one.
    pub(super) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Waits for the next wake.
    pub(super) async fn woken(&self) {
        self.wake.notified().await;
    }

    /// The dispatcher task is running.
    pub(super) fn live(&self) {
        lock(&self.state).dispatcher = Dispatcher::Live;
    }

    /// The oldest queued turn.
    pub(super) fn front(&self) -> Option<TurnNumber> {
        lock(&self.state).queue.front().copied()
    }

    /// Every queued turn, oldest first.
    pub(super) fn queued(&self) -> Vec<TurnNumber> {
        lock(&self.state).queue.iter().copied().collect()
    }

    /// Removes a turn that left the queue: submitted, or durably cancelled.
    pub(super) fn pop(&self, turn: TurnNumber) {
        lock(&self.state).queue.retain(|queued| *queued != turn);
    }

    /// Marks the dispatcher gone if the queue is empty; the caller holds
    /// `admission`. Returns whether it exited.
    pub(super) fn exit(&self) -> bool {
        let mut state = lock(&self.state);
        if !state.queue.is_empty() {
            return false;
        }
        state.dispatcher = Dispatcher::None;
        true
    }

    /// No queued turn and no dispatcher.
    pub(super) fn idle(&self) -> bool {
        let state = lock(&self.state);
        state.queue.is_empty() && state.dispatcher == Dispatcher::None
    }

    /// Marks the dispatcher gone after a force stop or a Store failure,
    /// whatever it leaves queued: nothing restarts it, since receipts are refused.
    pub(super) fn stop(&self) {
        lock(&self.state).dispatcher = Dispatcher::None;
    }

    /// A dispatcher was requested but has not run.
    pub(super) fn starting(&self) -> bool {
        lock(&self.state).dispatcher == Dispatcher::Starting
    }

    /// Whether only the slot itself holds its head: no writer lease is out.
    pub(super) fn unleased(&self) -> bool {
        Arc::strong_count(&self.head) == 1
    }
}

/// Exponential retry delay for Store reads: 250 ms doubling to 5 s, reset
/// by a successful step.
pub(super) struct Backoff(Duration);

impl Backoff {
    pub(super) fn new() -> Self {
        Self(RETRY_MIN)
    }

    /// The delay to wait now; the next one doubles up to the cap.
    pub(super) fn next(&mut self) -> Duration {
        let delay = self.0;
        self.0 = (self.0 * 2).min(RETRY_MAX);
        delay
    }

    pub(super) fn reset(&mut self) {
        self.0 = RETRY_MIN;
    }
}
