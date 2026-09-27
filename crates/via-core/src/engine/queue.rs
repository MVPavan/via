//! Per-session dispatch (C1 §7.3): one turn runs at a time, in FIFO order,
//! and a turn behind a predecessor that did not settle cleanly is cancelled.

use std::sync::Arc;

use tokio::sync::watch;

use super::journal::Head;
use crate::TurnNumber;

/// Most queued turns one session holds (C1 P6).
pub(super) const SESSION_QUEUE_LIMIT: u32 = 8;

/// Most queued turns the daemon holds across sessions (runtime §8).
pub(super) const DAEMON_QUEUE_LIMIT: usize = 128;

/// How far a session's turns have settled for dispatch.
#[derive(Clone, Copy)]
struct Dispatch {
    /// Every turn up to this number has finished its drive.
    done: u32,
    /// A finished turn was not cleanly terminal (unknown, unresolved or forced):
    /// no later turn may be dispatched (C1 P6), so each is cancelled in order.
    cancel_queue: bool,
}

/// One session's dispatch gate and the event head every writer of it shares.
pub(super) struct Slot {
    pub(super) head: Arc<Head>,
    dispatch: watch::Sender<Dispatch>,
}

impl Slot {
    /// A session whose turns up to `done` are finished.
    pub(super) fn new(head: Arc<Head>, done: u32, cancel_queue: bool) -> Arc<Self> {
        Arc::new(Self {
            head,
            dispatch: watch::Sender::new(Dispatch { done, cancel_queue }),
        })
    }

    /// Waits until every earlier turn has finished; returns whether `turn`
    /// must be cancelled instead of dispatched.
    pub(super) async fn turn(&self, turn: TurnNumber) -> bool {
        let mut dispatch = self.dispatch.subscribe();
        // The sender lives in `self`, so the wait ends only by the condition.
        match dispatch
            .wait_for(|dispatch| dispatch.done >= turn.get() - 1)
            .await
        {
            Ok(dispatch) => dispatch.cancel_queue,
            Err(_) => true,
        }
    }

    /// Records that `turn` finished; without `clean`, later turns are cancelled.
    fn finish(&self, turn: TurnNumber, clean: bool) {
        self.dispatch.send_modify(|dispatch| {
            dispatch.done = dispatch.done.max(turn.get());
            dispatch.cancel_queue |= !clean;
        });
    }
}

/// Releases the next turn when a drive ends, however it ends; only a turn
/// marked clean lets its successor dispatch.
pub(super) struct Finish<'a> {
    slot: &'a Slot,
    turn: TurnNumber,
    pub(super) clean: bool,
}

impl<'a> Finish<'a> {
    pub(super) fn new(slot: &'a Slot, turn: TurnNumber) -> Self {
        Self {
            slot,
            turn,
            clean: false,
        }
    }
}

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.slot.finish(self.turn, self.clean);
    }
}
