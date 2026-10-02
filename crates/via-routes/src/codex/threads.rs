//! `codex::ThreadTable` (x.3.2 X0 items 5, 8.1, 9.1; vendors/codex.md §5):
//! one connection's lanes, its open thread registrations, and what it
//! keeps until it retires: each accepted turn's `(threadId, turnId)`
//! mapped to the lane and VIA turn that accepted it, and each thread whose
//! registration closed. Late traffic for them is dropped and counted
//! before any full decode. Records, mappings and closed threads share one
//! correlation budget.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use via_wire::TurnNumber;

use super::lane::{Lane, LeaseSignal};

/// The most correlation entries one connection keeps (packet §5).
pub const CORRELATION_ENTRIES: usize = 1024;

/// The most correlation bytes one connection keeps (packet §5).
pub const CORRELATION_BYTES: usize = 256 * 1024;

/// The fixed charge of one entry, beside its thread ID's bytes (item 9.1).
const ENTRY_BYTES: usize = 64;

/// The correlation budget: request records, turn mappings and closed
/// threads (item 9.1). Exhaustion fails the connection `overflow`.
#[derive(Debug, Default)]
pub(super) struct Budget {
    entries: usize,
    bytes: usize,
}

impl Budget {
    /// Charges one entry naming `text` bytes of IDs; false when it does
    /// not fit (nothing is charged).
    pub(super) fn charge(&mut self, text: usize) -> bool {
        let bytes = self.bytes.saturating_add(ENTRY_BYTES.saturating_add(text));
        if self.entries >= CORRELATION_ENTRIES || bytes > CORRELATION_BYTES {
            return false;
        }
        self.entries += 1;
        self.bytes = bytes;
        true
    }

    /// Releases one entry charged for `text` bytes.
    pub(super) fn release(&mut self, text: usize) {
        self.entries = self.entries.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(ENTRY_BYTES.saturating_add(text));
    }
}

/// One lane a driver opened (a registration once a thread names it).
struct LaneEntry {
    lane: Arc<Lane>,
    signal: Option<Arc<LeaseSignal>>,
    thread: Option<String>,
    /// The `turn/start` record of the last turn whose stop posted its
    /// interrupt (item 8.3): each turn's stop posts its own.
    interrupted: Option<i64>,
    /// The generation's one cleanup interrupt intent was taken (item 8.3).
    cleanup: bool,
    /// The lane's one unsubscribe intent was posted (item 8.3).
    unsubscribed: bool,
}

/// An accepted turn, kept until the connection retires.
#[derive(Clone, Copy, Debug)]
struct Mapping {
    lane: u64,
    turn: TurnNumber,
}

/// Where a routed message goes.
pub(super) enum Route {
    /// An open registration's lane.
    Lane {
        lane: Arc<Lane>,
        signal: Option<Arc<LeaseSignal>>,
        /// The VIA turn the message's vendor turn was accepted as.
        owner: Option<TurnNumber>,
    },
    /// A closed registration's (item 5 step 4): dropped and counted.
    Late,
    /// A thread never registered here.
    Unknown,
    /// No thread at all: connection-scoped traffic.
    Untagged,
}

/// One connection's thread table.
#[derive(Default)]
pub(super) struct ThreadTable {
    lanes: HashMap<u64, LaneEntry>,
    open: HashMap<String, u64>,
    turns: HashMap<(String, String), Mapping>,
    closed: HashSet<String>,
    next: u64,
}

impl ThreadTable {
    /// A new lane, not yet a registration.
    pub(super) fn open_lane(&mut self, lane: Arc<Lane>, signal: Option<Arc<LeaseSignal>>) -> u64 {
        self.next = self.next.wrapping_add(1);
        let id = self.next;
        self.lanes.insert(
            id,
            LaneEntry {
                lane,
                signal,
                thread: None,
                interrupted: None,
                cleanup: false,
                unsubscribed: false,
            },
        );
        id
    }

    /// Registers lane `id` for `thread` (packet §5: one open registration
    /// per thread). The registration is charged now for the closed thread
    /// it leaves. `Err(())` is a budget exhaustion; `Ok(false)` a lane gone
    /// or a thread already open on another lane.
    pub(super) fn register(
        &mut self,
        id: u64,
        thread: &str,
        budget: &mut Budget,
    ) -> Result<bool, ()> {
        if self.open.get(thread).is_some_and(|open| *open == id) {
            return Ok(true);
        }
        let Some(entry) = self.lanes.get_mut(&id) else {
            return Ok(false);
        };
        if entry.thread.is_some() || self.open.contains_key(thread) {
            return Ok(false);
        }
        // A thread closed here before reopens: its charge carries over.
        if !self.closed.remove(thread) && !budget.charge(thread.len()) {
            return Err(());
        }
        entry.thread = Some(thread.to_owned());
        self.open.insert(thread.to_owned(), id);
        Ok(true)
    }

    /// Remembers `thread`, which an open no waiter wanted named (item
    /// 9.1): its traffic is late, never unknown. `Err(())` is a budget
    /// exhaustion.
    pub(super) fn forgo(&mut self, thread: &str, budget: &mut Budget) -> Result<(), ()> {
        if self.open.contains_key(thread) || self.closed.contains(thread) {
            return Ok(());
        }
        if !budget.charge(thread.len()) {
            return Err(());
        }
        self.closed.insert(thread.to_owned());
        Ok(())
    }

    /// The thread lane `id` is registered for, if any.
    pub(super) fn thread(&self, id: u64) -> Option<&str> {
        self.lanes.get(&id)?.thread.as_deref()
    }

    /// Closes lane `id`: its thread, if registered, is closed here (its
    /// charge kept), and its mapped turns become late.
    pub(super) fn close_lane(&mut self, id: u64) {
        let Some(entry) = self.lanes.remove(&id) else {
            return;
        };
        if let Some(thread) = entry.thread
            && self.open.get(&thread).is_some_and(|open| *open == id)
        {
            self.open.remove(&thread);
            self.closed.insert(thread);
        }
    }

    /// Maps the accepted vendor turn `turn` of `thread`, started on lane
    /// `id`, to VIA turn `number` until the connection retires, charged
    /// for both IDs (packet §5): kept even when the lane closed before the
    /// reply, so the turn's later traffic is late. `Ok(false)` when the
    /// budget is exhausted; `Err(())` when the vendor turn is already
    /// mapped, a protocol failure (X0 §5): the first mapping stays.
    pub(super) fn map_turn(
        &mut self,
        id: u64,
        (thread, turn): (&str, &str),
        number: TurnNumber,
        budget: &mut Budget,
    ) -> Result<bool, ()> {
        let key = (thread.to_owned(), turn.to_owned());
        if self.turns.contains_key(&key) {
            return Err(());
        }
        if !budget.charge(thread.len().saturating_add(turn.len())) {
            return Ok(false);
        }
        self.turns.insert(
            key,
            Mapping {
                lane: id,
                turn: number,
            },
        );
        Ok(true)
    }

    /// Whether lane `id` may post the stop interrupt of the turn started
    /// by `turn/start` record `start`: once per turn; marks it posted.
    pub(super) fn take_interrupt(&mut self, id: u64, start: i64) -> bool {
        self.lanes
            .get_mut(&id)
            .is_some_and(|entry| entry.interrupted.replace(start) != Some(start))
    }

    /// Whether lane `id` may post the generation's cleanup interrupt for
    /// the turn of record `start`: once per lane, and not when that turn's
    /// stop already posted its interrupt; marks it taken.
    pub(super) fn take_cleanup(&mut self, id: u64, start: i64) -> bool {
        let Some(entry) = self.lanes.get_mut(&id) else {
            return false;
        };
        if std::mem::replace(&mut entry.cleanup, true) {
            return false;
        }
        entry.interrupted.replace(start) != Some(start)
    }

    /// Whether lane `id` may post its unsubscribe intent; marks it posted.
    pub(super) fn take_unsubscribe(&mut self, id: u64) -> bool {
        self.lanes
            .get_mut(&id)
            .is_some_and(|entry| !std::mem::replace(&mut entry.unsubscribed, true))
    }

    /// Item 5 steps 3–5: where a message naming `thread` and `turn` goes.
    pub(super) fn route(&self, thread: Option<&str>, turn: Option<&str>) -> Route {
        let Some(thread) = thread else {
            return Route::Untagged;
        };
        let mapped = turn.and_then(|turn| self.turns.get(&(thread.to_owned(), turn.to_owned())));
        if let Some(id) = self.open.get(thread)
            && let Some(entry) = self.lanes.get(id)
        {
            return match mapped {
                // An earlier generation's turn: its sink is closed.
                Some(mapping) if mapping.lane != *id => Route::Late,
                mapped => Route::Lane {
                    lane: Arc::clone(&entry.lane),
                    signal: entry.signal.clone(),
                    owner: mapped.map(|mapping| mapping.turn),
                },
            };
        }
        if mapped.is_some() || self.closed.contains(thread) {
            Route::Late
        } else {
            Route::Unknown
        }
    }

    /// Every lane, for the connection's end; the table keeps nothing.
    pub(super) fn drain(&mut self) -> Vec<Arc<Lane>> {
        self.open.clear();
        self.lanes.drain().map(|(_, entry)| entry.lane).collect()
    }
}
