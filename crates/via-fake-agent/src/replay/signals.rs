//! Signals for replay: one watcher thread owns the handlers and publishes
//! each delivered signal to a queue of at most [`QUEUE`] entries, stamped
//! with its arrival, the instant the watcher publishes it (as stdin's
//! reader does for lines). An `await_signal` step takes the first queued
//! signal of its kind and completes at that arrival, not when the step
//! handles it, so an adapter that signals and then closes stdin at once
//! passes an `await_eof` that follows (via-jm4.33).
//!
//! The handlers are installed before [`Signals::start`] returns, so a
//! signal that comes before its step is held for it. Signals of a kind no
//! step awaits keep their default action.

use std::collections::{BTreeSet, VecDeque};
use std::future::poll_fn;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::Poll;
use std::thread;
use std::time::Instant;

use tokio::signal::unix::{SignalKind, signal};

use super::SignalName;

/// Most signals held for later steps; one more fails the next wait.
const QUEUE: usize = 64;

/// Arrivals not yet taken, and whether one was dropped for room.
#[derive(Default)]
struct Queue {
    arrivals: VecDeque<(Instant, SignalName)>,
    overflowed: bool,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    arrived: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        // Each critical section is a single push or removal.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The signals a fixture awaits, as stamped arrivals from the watcher.
pub(super) struct Signals {
    shared: Arc<Shared>,
    deadline: Instant,
}

impl Signals {
    /// Installs a handler for each name on the watcher thread and returns
    /// once they are installed. With no names, nothing is started.
    pub(super) fn start(names: BTreeSet<SignalName>, deadline: Instant) -> Result<Self, String> {
        let shared = Arc::new(Shared::default());
        if names.is_empty() {
            return Ok(Self { shared, deadline });
        }
        let watcher = Arc::clone(&shared);
        let (ready, installed) = mpsc::sync_channel(1);
        // Detached on purpose: it waits for signals until the process exits.
        thread::Builder::new()
            .spawn(move || watch(&names, &watcher, &ready))
            .map_err(|error| format!("cannot start the signal watcher: {error}"))?;
        installed
            .recv()
            .map_err(|_| "the signal watcher ended before installing handlers".to_owned())??;
        Ok(Self { shared, deadline })
    }

    /// Takes the first arrival of `name`, waiting until the run deadline,
    /// and returns its arrival instant.
    pub(super) fn take(&self, name: SignalName) -> Result<Instant, String> {
        let mut queue = self.shared.lock();
        loop {
            if queue.overflowed {
                return Err(format!("more than {QUEUE} signals were held"));
            }
            if let Some(at) = queue.arrivals.iter().position(|(_, seen)| *seen == name) {
                let (arrived, _) = queue
                    .arrivals
                    .remove(at)
                    .ok_or("the signal queue changed under its lock")?;
                return Ok(arrived);
            }
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("deadline passed while awaiting {}", name.text()));
            }
            queue = self
                .shared
                .arrived
                .wait_timeout(queue, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// Installs the handlers, reports the result on `ready`, then publishes
/// each arrival until a stream ends.
fn watch(
    names: &BTreeSet<SignalName>,
    shared: &Shared,
    ready: &mpsc::SyncSender<Result<(), String>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            // The receiver waits for exactly this message.
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    let installed: Result<Vec<_>, String> = {
        let _guard = runtime.enter();
        names
            .iter()
            .map(|name| {
                let kind = match name {
                    SignalName::Int => SignalKind::interrupt(),
                    SignalName::Term => SignalKind::terminate(),
                    SignalName::Usr1 => SignalKind::user_defined1(),
                };
                signal(kind)
                    .map(|stream| (*name, stream))
                    .map_err(|error| error.to_string())
            })
            .collect()
    };
    let mut streams = match installed {
        Ok(streams) => {
            if ready.send(Ok(())).is_err() {
                return;
            }
            streams
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    loop {
        let next = runtime.block_on(poll_fn(|cx| {
            for (name, stream) in &mut streams {
                if let Poll::Ready(delivered) = stream.poll_recv(cx) {
                    return Poll::Ready(delivered.map(|()| *name));
                }
            }
            Poll::Pending
        }));
        let Some(name) = next else {
            return;
        };
        let mut queue = shared.lock();
        if queue.arrivals.len() < QUEUE {
            queue.arrivals.push_back((Instant::now(), name));
        } else {
            queue.overflowed = true;
        }
        drop(queue);
        shared.arrived.notify_all();
    }
}
