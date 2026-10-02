//! A turn's hold on its connection (AD16): the slot and the pin, committed
//! once the logical turn kept its connection and was delivered whole.

use std::sync::{Arc, Mutex};

use via_routes::Retirement;

use super::{DriverState, lock};
use crate::CapacityToken;

/// One turn's hold on its connection (AD16), shared by the turn's task and
/// `run_turn`: the persistent profile's slot and pin are committed only
/// once the logical turn kept its server and the Adapter delivered all of
/// it. Its last owner drops it after the process's retirement and the end
/// of `run_turn`, whichever is later; dropped uncommitted, on any failure,
/// a dropped `run_turn` or an unwind, it invalidates the generation and
/// releases the slot then.
pub(crate) struct Reservation {
    state: Arc<Mutex<DriverState>>,
    generation: u64,
    slot: Option<CapacityToken>,
    persistent: bool,
    committed: bool,
}

impl Reservation {
    /// An uncommitted hold on connection `generation`, holding `slot`.
    pub(super) fn new(
        state: Arc<Mutex<DriverState>>,
        generation: u64,
        slot: Option<CapacityToken>,
        persistent: bool,
    ) -> Self {
        Self {
            state,
            generation,
            slot,
            persistent,
            committed: false,
        }
    }

    /// The logical turn kept its server: the slot and the pin are the
    /// session's, unless it closed or was cancelled meanwhile.
    pub(crate) fn commit(&mut self, cancelled: bool) {
        if !self.persistent || cancelled {
            return;
        }
        let mut state = lock(&self.state);
        if state.closed || state.generation != self.generation {
            return;
        }
        if let Some(slot) = self.slot.take() {
            state.capacity = Some(slot);
        }
        state.live = true;
        self.committed = true;
    }

    /// The logical turn ended its connection, or the Adapter could not
    /// deliver it: the generation is invalid now; its slot, the session's
    /// committed one included, goes with the reservation, after the
    /// process's retirement.
    pub(crate) fn release(&mut self) {
        self.invalidate();
    }

    /// Invalidates the generation and takes over its committed slot.
    fn invalidate(&mut self) {
        let committed = {
            let mut state = lock(&self.state);
            if state.generation == self.generation {
                state.live = false;
                state.capacity.take()
            } else {
                None
            }
        };
        if committed.is_some() {
            self.slot = committed;
        }
    }

    /// Records the turn's process retirement for a later close. A session
    /// closed meanwhile releases its committed slot now, the retirement
    /// done (C2 §2 Close).
    pub(crate) fn retired(&self, retirement: Retirement) {
        let released = {
            let mut state = lock(&self.state);
            state.retirement = Some(retirement);
            if state.closed && state.generation == self.generation {
                state.capacity.take()
            } else {
                None
            }
        };
        drop(released);
    }
}

impl Drop for Reservation {
    /// Uncommitted, the generation is invalidated; the slot it holds is
    /// released with the reservation's fields.
    fn drop(&mut self) {
        if !self.committed {
            self.invalidate();
        }
    }
}
