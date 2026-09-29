//! The SQLite writer's request lanes (Task 4 design §6.1–§6.3, A2): four
//! FIFO lanes, each with its own slots and bytes, served Latch first, then
//! Lifecycle, then Internal and Public in turn. A push never blocks: a full
//! lane, the fence or a dead writer refuses it at once. Only queue and
//! counter updates run under the mutex; a refused or abandoned request, and
//! with it its reply, is dropped after the mutex is released.

use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
};

use crate::{StoreError, runtime::Command};

const MIB: usize = 1024 * 1024;

/// Which lane a request joins; a [`crate::StoreClient`] handle names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lane {
    /// The latch's failure-resolution unit: one request, 2 MiB.
    Latch,
    /// Every other call of final shutdown's pipeline: seven requests, 2 MiB.
    Lifecycle,
    /// Every other commit and read by Core, Route, Host or recovery: the 64
    /// requests and 4 MiB that Public does not use.
    Internal,
    /// Reads issued by C1 handlers: at most 32 of the 64, 4 KiB each.
    Public,
}

impl Lane {
    fn index(self) -> usize {
        match self {
            Self::Latch => 0,
            Self::Lifecycle => 1,
            Self::Internal => 2,
            Self::Public => 3,
        }
    }
}

/// Requests of the Latch lane.
const LATCH_SLOTS: usize = 1;
/// Requests of the Lifecycle lane.
const LIFECYCLE_SLOTS: usize = 7;
/// Bytes of the Latch lane and, separately, of the Lifecycle lane: each
/// holds one terminal (design §6.4).
const RESERVED_BYTES: usize = 2 * MIB;
/// Requests Internal and Public share.
const SHARED_SLOTS: usize = 64;
/// Bytes Internal and Public share.
const SHARED_BYTES: usize = 4 * MIB;
/// Most requests in the Public lane.
const PUBLIC_SLOTS: usize = 32;
/// Largest Public request.
const PUBLIC_ITEM_BYTES: usize = 4 * 1024;

#[derive(Default)]
struct Queue {
    items: VecDeque<(Command, usize)>,
    bytes: usize,
    /// Most requests this lane has held at once.
    #[cfg(feature = "test-failpoints")]
    peak: usize,
}

#[derive(Default)]
struct State {
    /// Latch, Lifecycle, Internal, Public ([`Lane::index`]).
    queues: [Queue; 4],
    /// Public is served next when both it and Internal hold requests.
    public_next: bool,
    /// `Store::drop` closed admission: later pushes are refused.
    fence: bool,
    /// The writer thread ended: later pushes lost their writer.
    dead: bool,
}

impl State {
    /// Whether a request of `bytes` fits `lane` now.
    fn fits(&self, lane: Lane, bytes: usize) -> bool {
        let queue = &self.queues[lane.index()];
        let [_, _, internal, public] = &self.queues;
        let shared = |extra: usize| {
            internal.items.len() + public.items.len() < SHARED_SLOTS
                && internal.bytes + public.bytes + extra <= SHARED_BYTES
        };
        match lane {
            Lane::Latch => queue.items.len() < LATCH_SLOTS && queue.bytes + bytes <= RESERVED_BYTES,
            Lane::Lifecycle => {
                queue.items.len() < LIFECYCLE_SLOTS && queue.bytes + bytes <= RESERVED_BYTES
            }
            Lane::Internal => shared(bytes),
            Lane::Public => {
                public.items.len() < PUBLIC_SLOTS && bytes <= PUBLIC_ITEM_BYTES && shared(bytes)
            }
        }
    }

    fn take(&mut self, lane: Lane) -> Option<Command> {
        let queue = &mut self.queues[lane.index()];
        let (command, bytes) = queue.items.pop_front()?;
        queue.bytes -= bytes;
        Some(command)
    }

    /// The next request in service order.
    fn next(&mut self) -> Option<Command> {
        if let Some(command) = self.take(Lane::Latch) {
            return Some(command);
        }
        if let Some(command) = self.take(Lane::Lifecycle) {
            return Some(command);
        }
        let order = if self.public_next {
            [Lane::Public, Lane::Internal]
        } else {
            [Lane::Internal, Lane::Public]
        };
        for lane in order {
            if let Some(command) = self.take(lane) {
                self.public_next = lane == Lane::Internal;
                return Some(command);
            }
        }
        None
    }
}

/// The writer's four request lanes, a fence and a death flag: one
/// `Mutex<State>` and a `Condvar` the writer waits on.
#[derive(Default)]
pub struct Lanes {
    state: Mutex<State>,
    ready: Condvar,
}

impl Lanes {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `command`, which encodes to `bytes`, on `lane` without
    /// blocking. After the fence, or on a full lane, it is `NotEnqueued`;
    /// once the writer died, `WriterLost`.
    pub(crate) fn push(
        &self,
        lane: Lane,
        command: Command,
        bytes: usize,
    ) -> Result<(), StoreError> {
        let refused = {
            let mut state = self.lock();
            if state.fence {
                Some((command, StoreError::NotEnqueued))
            } else if state.dead {
                Some((command, StoreError::WriterLost))
            } else if state.fits(lane, bytes) {
                let queue = &mut state.queues[lane.index()];
                queue.items.push_back((command, bytes));
                queue.bytes += bytes;
                #[cfg(feature = "test-failpoints")]
                {
                    queue.peak = queue.peak.max(queue.items.len());
                }
                None
            } else {
                Some((command, StoreError::NotEnqueued))
            }
        };
        // Dropped here, after the mutex: the reply with it.
        if let Some((command, error)) = refused {
            drop(command);
            return Err(error);
        }
        self.ready.notify_one();
        Ok(())
    }

    /// The writer's next request in service order, waiting while every
    /// lane is empty; `None` once the fence is set and every lane drained.
    pub(crate) fn pop(&self) -> Option<Command> {
        let mut state = self.lock();
        loop {
            if let Some(command) = state.next() {
                return Some(command);
            }
            if state.fence {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Sets the admission fence (design §6.3): requests accepted before it
    /// are still served in lane order.
    pub(crate) fn fence(&self) {
        self.lock().fence = true;
        self.ready.notify_one();
    }

    /// The writer ended (design §6.3): every queued request and `in_flight`
    /// fail `WriterLost`, their replies dropped after the mutex is released,
    /// and later pushes are `WriterLost`.
    pub(crate) fn die(&self, in_flight: Option<Command>) {
        let abandoned: Vec<Command> = {
            let mut state = self.lock();
            state.dead = true;
            state
                .queues
                .iter_mut()
                .flat_map(|queue| {
                    queue.bytes = 0;
                    queue.items.drain(..).map(|(command, _)| command)
                })
                .collect()
        };
        drop(abandoned);
        drop(in_flight);
    }

    /// Test builds: the most requests `lane` has held at once.
    #[cfg(feature = "test-failpoints")]
    pub fn peak(&self, lane: Lane) -> usize {
        self.lock().queues[lane.index()].peak
    }

    /// Test builds: whether `Store::drop` has set the fence.
    #[cfg(feature = "test-failpoints")]
    pub fn fenced(&self) -> bool {
        self.lock().fence
    }
}

/// Runs the writer thread's body: on any exit, unwinding included, it
/// marks the writer dead and fails what it holds (design §6.3).
pub(crate) struct DeadGuard<'a> {
    lanes: &'a Lanes,
    /// The request being served, while the writer holds it here.
    pub(crate) in_flight: Option<Command>,
}

impl<'a> DeadGuard<'a> {
    pub(crate) fn new(lanes: &'a Lanes) -> Self {
        Self {
            lanes,
            in_flight: None,
        }
    }
}

impl Drop for DeadGuard<'_> {
    fn drop(&mut self) {
        self.lanes.die(self.in_flight.take());
    }
}
