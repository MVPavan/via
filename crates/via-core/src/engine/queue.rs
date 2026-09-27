//! Per-session dispatch (C1 §7.3): one turn runs at a time, in FIFO order.
//! Whether a turn may run once its predecessors' drives are done is decided
//! from their durable state (`Engine::dispatchable`), never remembered here.

use std::sync::Arc;

use tokio::sync::watch;

use super::journal::Head;
use crate::TurnNumber;

/// Most queued turns one session holds (C1 P6), also enforced by Store.
pub(super) const SESSION_QUEUE_LIMIT: u32 = via_store::SESSION_QUEUE_LIMIT;

/// Most queued turns the daemon holds across sessions (runtime §8).
pub(super) const DAEMON_QUEUE_LIMIT: usize = 128;

/// One session's dispatch gate and the event head every writer of it shares.
pub(super) struct Slot {
    pub(super) head: Arc<Head>,
    /// Every turn up to this number has no drive left in this daemon.
    done: watch::Sender<u32>,
}

impl Slot {
    /// A session whose turns up to `done` have no drive left in this daemon.
    pub(super) fn new(head: Arc<Head>, done: u32) -> Arc<Self> {
        Arc::new(Self {
            head,
            done: watch::Sender::new(done),
        })
    }

    /// Waits until no earlier turn of the session has a drive left.
    pub(super) async fn turn(&self, turn: TurnNumber) {
        let mut done = self.done.subscribe();
        // The sender lives in `self`, so the wait ends only by the condition.
        let _ = done.wait_for(|done| *done >= turn.get() - 1).await;
    }

    /// Records that `turn` has no drive left: it finished, or no drive of this
    /// daemon will run it unless a keyed retry adopts it.
    pub(super) fn finish(&self, turn: TurnNumber) {
        self.done
            .send_modify(|done| *done = (*done).max(turn.get()));
    }
}

/// Releases the next turn when a drive ends, however it ends.
pub(super) struct Finish<'a> {
    slot: &'a Slot,
    turn: TurnNumber,
}

impl<'a> Finish<'a> {
    pub(super) fn new(slot: &'a Slot, turn: TurnNumber) -> Self {
        Self { slot, turn }
    }
}

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.slot.finish(self.turn);
    }
}
