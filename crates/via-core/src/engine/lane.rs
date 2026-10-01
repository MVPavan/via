//! A session's lane on its adapter driver (C2 §2; adapter design AD3, AD4,
//! AD16): the driver, opened at the session's first dispatch from its
//! durable route identity, the session's observation channel, the map from
//! vendor turns to turns, and what a turn's observations and its end
//! establish for its envelope.
//!
//! A lane outlives its dispatcher, so a later turn can pin the live
//! connection its driver keeps (AD16) and the confirmed identity is kept;
//! the session's close or final shutdown ends it. A driver whose health
//! failed is closed and replaced at the session's next turn (C2 §2
//! health).

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use serde_json::{Map, Value};
use tokio::sync::mpsc;
use via_adapters::{
    Admitted, CloseMode, DriverHealth, FAKE, Harness, Inherit, SessionCx, SessionDriver,
    SessionRef, SessionSpec, UsageSample, VendorOptions, VendorTerminal, observation_channel,
};

use super::progress::UsageLedger;
use super::{Engine, lock};
use crate::api::{AutoDeclined, DeniedAction, Kept};
use crate::{Deadline, SessionId, TurnNumber};

/// Vendor turn IDs a lane remembers: late observations of older turns are
/// dropped.
const VENDOR_TURNS: usize = 64;

/// How long replacing a failed driver waits for its close.
const REPLACE_CLOSE: Duration = Duration::from_secs(3);

/// One session's driver and observation channel.
pub(super) struct Lane {
    pub(super) driver: SessionDriver,
    /// The session channel's receiver; the session's dispatcher holds it
    /// for the whole of a turn.
    pub(super) observations: tokio::sync::Mutex<mpsc::Receiver<Admitted>>,
    state: StdMutex<LaneState>,
}

/// What the lane learned from the session's observations.
#[derive(Default)]
struct LaneState {
    /// Vendor turn IDs of the session's accepted turns, oldest first.
    turns: VecDeque<(String, TurnNumber)>,
    /// The session's confirmed vendor identity.
    identity: Option<Identity>,
}

/// A confirmed vendor identity and its transcript hint (C1 §5, AD6).
#[derive(Clone, Debug)]
pub(super) struct Identity {
    pub(super) vendor_session_id: String,
    pub(super) transcript: Option<String>,
}

/// Which turn an observation belongs to (C1 §6.1, AD4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Attribution {
    /// The running turn, or the session between turns.
    Current,
    /// An earlier turn of the session, already ended: a late observation.
    Late(TurnNumber),
}

impl Lane {
    /// Whether the driver's health failed (C2 §2): its connection is not
    /// used for another turn.
    fn failed(&self) -> bool {
        matches!(*self.driver.health().borrow(), DriverHealth::Failed { .. })
    }

    /// Records `vendor_turn` as `turn`'s, the newest.
    pub(super) fn map_vendor_turn(&self, vendor_turn: &str, turn: TurnNumber) {
        let mut state = lock(&self.state);
        if state.turns.iter().any(|(known, _)| known == vendor_turn) {
            return;
        }
        if state.turns.len() == VENDOR_TURNS {
            state.turns.pop_front();
        }
        state.turns.push_back((vendor_turn.to_owned(), turn));
    }

    /// The turn an item naming `vendor_turn` belongs to while `running`
    /// runs, or between turns with `None`: a vendor turn of an earlier turn
    /// is late; one not yet known is the running turn's.
    pub(super) fn attribute(
        &self,
        vendor_turn: Option<&str>,
        running: Option<TurnNumber>,
    ) -> Attribution {
        let known = vendor_turn.and_then(|vendor_turn| {
            lock(&self.state)
                .turns
                .iter()
                .find(|(known, _)| known == vendor_turn)
                .map(|(_, turn)| *turn)
        });
        match known {
            Some(turn) if Some(turn) != running => Attribution::Late(turn),
            Some(_) | None => Attribution::Current,
        }
    }

    /// The session's confirmed identity, as the lane last saw it.
    pub(super) fn identity(&self) -> Option<Identity> {
        lock(&self.state).identity.clone()
    }

    /// Records a newly confirmed identity.
    pub(super) fn confirm(&self, identity: Identity) {
        lock(&self.state).identity = Some(identity);
    }
}

/// What a turn's observations and its end established for its envelope
/// (design §5.1 #33, AD4, AD6, AD7).
#[derive(Clone, Default)]
pub(super) struct VendorRecord {
    /// The session's confirmed identity, this turn's once it confirmed one.
    pub(super) identity: Option<Identity>,
    /// The turn-wide usage ledger.
    pub(super) ledger: UsageLedger,
    /// Denials the turn committed as `action.denied`.
    pub(super) denied: Kept<DeniedAction>,
    /// Declines the turn committed as `vendor.request_declined`.
    pub(super) declined: Kept<AutoDeclined>,
    /// The handshake version of the instance that ran the turn, and
    /// whether the adapter checked it (AD7).
    pub(super) instance: Option<(Option<String>, bool)>,
    /// The retained vendor terminal's envelope facts (AD4).
    pub(super) retained: Option<Retained>,
}

/// The retained vendor terminal's envelope facts.
#[derive(Clone, Default)]
pub(super) struct Retained {
    pub(super) structured_output: Option<Value>,
    pub(super) steps: Option<u64>,
    pub(super) usage: Option<UsageSample>,
    pub(super) cost: Option<(f64, String)>,
    /// The members of the terminal's vendor data object.
    pub(super) vendor: Map<String, Value>,
}

impl Retained {
    pub(super) fn of(terminal: &VendorTerminal) -> Self {
        let parse =
            |raw: &serde_json::value::RawValue| serde_json::from_str::<Value>(raw.get()).ok();
        let vendor = match terminal.vendor.as_deref().and_then(parse) {
            Some(Value::Object(members)) => members,
            _ => Map::new(),
        };
        Self {
            structured_output: terminal.structured_output.as_deref().and_then(parse),
            steps: terminal.steps,
            usage: terminal.usage.clone(),
            cost: terminal
                .cost
                .as_ref()
                .map(|cost| (cost.usd, cost.scope.clone())),
            vendor,
        }
    }
}

impl Engine {
    /// The session's kept lane, unless its driver's health failed: what
    /// `prepare` is asked at dispatch. Without one a new driver needs a
    /// connection.
    pub(super) fn kept_lane(&self, session: &SessionId) -> Option<Arc<Lane>> {
        lock(&self.lanes)
            .get(session)
            .filter(|lane| !lane.failed())
            .cloned()
    }

    /// The session's lane for its submitted turn: the one it has, unless
    /// its driver's health failed, which is closed and replaced with the
    /// identity it confirmed (C2 §2 health); else a lane opened for the
    /// turn's `model` in `cwd` (C2 §2 `open_session`, logical: no vendor
    /// I/O).
    ///
    /// The route identity is the fake's: intake admits only harnesses the
    /// adapter set serves, the fake alone in this build (api.rs stays
    /// fake-shaped at intake until chunk 5). The Store's one read of a
    /// session's frozen harness is `status`'s, which a corrupt queued row
    /// fails (design §7.3), so dispatch does not read it.
    pub(super) async fn lane(&self, session: &SessionId, model: &str, cwd: PathBuf) -> Arc<Lane> {
        let kept = lock(&self.lanes).get(session).cloned();
        let mut identity = None;
        if let Some(lane) = kept {
            if !lane.failed() {
                return lane;
            }
            // C2 §2: no turn runs on a failed driver's connection.
            lock(&self.lanes).remove(session);
            identity = lane.identity();
            let deadline = Deadline::at(tokio::time::Instant::now() + REPLACE_CLOSE);
            let _report = lane.driver.close(CloseMode::Force, deadline).await;
        }
        let reference = SessionRef {
            harness: FAKE.to_owned(),
            route: Harness::parse(FAKE).map_or("", Harness::route).to_owned(),
            // `open_session` reads only the harness and route; the
            // version check is `check_turn`'s (chunk 5).
            adapter_version: String::new(),
        };
        let spec = SessionSpec {
            session_id: session.clone(),
            model: model.to_owned(),
            instructions: None,
            initial_bound: None,
            cwd,
            vendor: VendorOptions::new(),
            inherit: Inherit::OD2_DEFAULT,
            confirmed_vendor_session_id: identity
                .as_ref()
                .map(|identity: &Identity| identity.vendor_session_id.clone()),
            allow_untested: false,
        };
        let (sink, receiver) = observation_channel();
        let cx = SessionCx {
            observations: sink,
            tracker: self.tracker.clone(),
            cancel: self.cancel.child_token(),
        };
        let lane = Arc::new(Lane {
            driver: self.adapter.open_session(&reference, spec, cx),
            observations: tokio::sync::Mutex::new(receiver),
            state: StdMutex::new(LaneState {
                turns: VecDeque::new(),
                identity,
            }),
        });
        lock(&self.lanes).insert(session.clone(), Arc::clone(&lane));
        lane
    }

    /// A session's close (C2 §2 Close): its driver is closed by
    /// `deadline`, releasing any connection it holds, and the lane goes.
    pub(super) async fn close_lane(
        &self,
        session: &SessionId,
        mode: CloseMode,
        deadline: Deadline,
    ) {
        let lane = lock(&self.lanes).remove(session);
        if let Some(lane) = lane {
            let _report = lane.driver.close(mode, deadline).await;
        }
    }

    /// Final shutdown: every driver's owned work is cancelled and every
    /// lane dropped, before Host reconciliation.
    pub(super) fn drop_lanes(&self) {
        self.cancel.cancel();
        self.tracker.close();
        lock(&self.lanes).clear();
    }
}
