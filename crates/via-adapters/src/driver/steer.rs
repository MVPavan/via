//! The driver's steer bookkeeping (C2 §2 `steer`, `SteerInput.token`):
//! the running turn's callers waiting for their `steer.delivered`
//! observation's emission, and Route's refusals as the driver reports them.
//! Harness-neutral: a route that supports steer marks its inputs on the
//! steer lane (`via_routes::steer`); its normalizer answers the callers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use via_routes::SteerRefused;

use super::SteerError;

/// The running turn's steer callers waiting for their `steer.delivered`
/// observation's emission, by token (C2 `SteerInput.token`; critical r1
/// #5, r2 #1). The turn's normalizer answers each `true` once the
/// observation is on the session channel, `false` when it could not put it
/// there. The turn's end, by any path, closes the registry: every caller
/// left is answered, a caller still waiting for Route's answer learns the
/// turn ended (critical r3 #1), and none registers after ([`SteerTurn`]).
/// A caller holds only its receiver and retires its own entry when its
/// future is dropped ([`SteerWait`]), so the registry holds at most the
/// turn's live callers.
#[derive(Default)]
pub(crate) struct SteerEmissions {
    registry: Mutex<SteerRegistry>,
    /// Cancelled when the turn ends.
    ended: CancellationToken,
}

#[derive(Default)]
struct SteerRegistry {
    waiting: HashMap<u64, Waiting>,
    closed: bool,
}

/// One caller's entry: its answer's sender.
struct Waiting {
    sender: oneshot::Sender<bool>,
    /// Test builds: Route acknowledged the caller's input, so it waits on
    /// the emission alone (critical r3 #2).
    #[cfg(any(test, feature = "test-failpoints"))]
    acknowledged: bool,
}

impl SteerEmissions {
    /// Never held across an await; a poisoned registry is still consistent.
    fn lock(&self) -> MutexGuard<'_, SteerRegistry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers the caller of `token`; `None` once the turn ended.
    pub(super) fn wait(self: &Arc<Self>, token: u64) -> Option<SteerWait> {
        let mut registry = self.lock();
        if registry.closed {
            return None;
        }
        let (sender, receiver) = oneshot::channel();
        registry.waiting.insert(
            token,
            Waiting {
                sender,
                #[cfg(any(test, feature = "test-failpoints"))]
                acknowledged: false,
            },
        );
        Some(SteerWait {
            receiver,
            registry: Arc::downgrade(self),
            ended: self.ended.clone(),
            token,
        })
    }

    /// Answers the caller of `token`, if it still waits: whether its
    /// observation is on the session channel.
    pub(crate) fn answer(&self, token: u64, emitted: bool) {
        let waiting = self.lock().waiting.remove(&token);
        if let Some(waiting) = waiting {
            // The caller went away meanwhile: nobody waits for the answer.
            let _ = waiting.sender.send(emitted);
        }
    }

    /// The turn ended: every caller left learns its observation was not
    /// emitted, one waiting for Route's answer that the turn ended, and
    /// none registers after.
    fn close(&self) {
        let waiting = {
            let mut registry = self.lock();
            registry.closed = true;
            std::mem::take(&mut registry.waiting)
        };
        self.ended.cancel();
        for waiting in waiting.into_values() {
            let _ = waiting.sender.send(false);
        }
    }

    /// Test builds: marks the caller of `token` acknowledged by Route.
    #[cfg(any(test, feature = "test-failpoints"))]
    pub(super) fn acknowledged(&self, token: u64) {
        if let Some(waiting) = self.lock().waiting.get_mut(&token) {
            waiting.acknowledged = true;
        }
    }

    /// How many callers wait, and how many of them Route acknowledged.
    #[cfg(any(test, feature = "test-failpoints"))]
    pub(super) fn len(&self) -> (usize, usize) {
        let registry = self.lock();
        let acknowledged = registry
            .waiting
            .values()
            .filter(|waiting| waiting.acknowledged)
            .count();
        (registry.waiting.len(), acknowledged)
    }
}

/// Owned by a turn's `run_turn` for its life: dropped when the turn ends
/// by any path (its return, its future dropped, a forced stop or the
/// cutoff), it closes the turn's [`SteerEmissions`], so a steer never
/// outlives its turn (critical r2 #1, r3 #1).
pub(crate) struct SteerTurn(pub(crate) Arc<SteerEmissions>);

impl Drop for SteerTurn {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// One steer caller's wait for its observation's emission. It holds only
/// its receiver, a weak handle on the registry and the turn's end, so it
/// never keeps its own completion alive; dropped unanswered, it retires
/// its entry.
pub(super) struct SteerWait {
    receiver: oneshot::Receiver<bool>,
    pub(super) registry: Weak<SteerEmissions>,
    pub(super) ended: CancellationToken,
    token: u64,
}

impl SteerWait {
    /// Whether the observation was emitted; the turn's end without an
    /// answer is not.
    pub(super) async fn emitted(mut self) -> bool {
        (&mut self.receiver).await.unwrap_or(false)
    }
}

impl Drop for SteerWait {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.lock().waiting.remove(&self.token);
        }
    }
}

/// Route's refusal as the driver reports it.
pub(super) fn steer_error(refused: SteerRefused) -> SteerError {
    match refused {
        SteerRefused::NotActive => SteerError::NoActiveTurn,
        SteerRefused::NotWritten => SteerError::NotDelivered,
        SteerRefused::TurnMismatch => SteerError::TurnMismatch,
        SteerRefused::OverCapacity => SteerError::OverCapacity,
    }
}
