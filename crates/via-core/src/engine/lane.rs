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
    Admitted, CloseMode, DriverHealth, Inherit, SessionCx, SessionDriver, SessionRef, SessionSpec,
    UsageSample, VendorOptions, VendorTerminal, observation_channel,
};
use via_store::SessionRoute;

use super::progress::UsageLedger;
use super::{Engine, lock};
use crate::api::{AutoDeclined, DeniedAction, Kept, StructuredOutputFile};
use crate::{Deadline, SessionId, TurnNumber};

/// Vendor turn IDs a lane remembers: late observations of older turns are
/// dropped.
pub(super) const VENDOR_TURNS: usize = 64;

/// Tombstones a lane keeps of vendor turns whose mapping expired (C2 §2):
/// 64-bit hashes, so the bound is small whatever the IDs' length.
const TOMBSTONES: usize = 1024;

/// How long replacing a failed driver waits for its close.
const REPLACE_CLOSE: Duration = Duration::from_secs(3);

/// One session's driver and observation channel.
pub(super) struct Lane {
    pub(super) driver: SessionDriver,
    /// The session's route identity the driver was opened with.
    pub(super) reference: SessionRef,
    /// The session channel's receiver; the session's dispatcher holds it
    /// for the whole of a turn.
    pub(super) observations: tokio::sync::Mutex<mpsc::Receiver<Admitted>>,
    state: StdMutex<LaneState>,
    /// The failed driver's close was started: the lane waits only for its
    /// successor.
    retired: std::sync::atomic::AtomicBool,
}

/// What the lane learned from the session's observations.
#[derive(Default)]
struct LaneState {
    /// Vendor turn IDs of the session's accepted turns, oldest first.
    turns: VecDeque<(String, TurnNumber)>,
    /// Hashes of vendor turn IDs whose mapping expired, oldest first.
    tombstones: VecDeque<u64>,
    /// The session's confirmed vendor identity.
    identity: Option<Identity>,
    /// A `session.opened` is committed: later generations reopen.
    opened: bool,
    /// The driver's connection generation whose identity is committed.
    committed: Option<String>,
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
    /// The running turn.
    Current,
    /// An earlier turn of the session, already ended: a late observation.
    Late(TurnNumber),
    /// No turn: the session's, a vendor turn genuinely unseen (`turn: null`).
    Session,
    /// A vendor turn whose mapping expired, past the bound or with a
    /// replaced lane: its traffic is dropped.
    Expired,
}

impl LaneState {
    /// A new lane's state after a daemon restart or a close (C2 §2,
    /// decision H3): the identity the session last committed, with its
    /// transcript.
    fn recovered(route: &SessionRoute) -> Self {
        let identity = route.vendor_session_id.clone().map(|vendor_session_id| Identity {
            vendor_session_id,
            transcript: route.transcript.clone(),
        });
        Self {
            opened: identity.is_some(),
            identity,
            ..Self::default()
        }
    }

    /// A replacing lane's state (C2 §2 health): the confirmed identity,
    /// with every vendor turn the failed lane mapped or tombstoned now
    /// tombstoned, so its traffic never becomes current or session-level.
    /// Its driver's connection generations are new.
    fn successor(&self) -> Self {
        let mut successor = Self {
            turns: VecDeque::new(),
            tombstones: self.tombstones.clone(),
            identity: self.identity.clone(),
            opened: self.opened,
            committed: None,
        };
        for (vendor_turn, _) in &self.turns {
            successor.tombstone(vendor_turn);
        }
        successor
    }

    /// Records `vendor_turn` as `turn`'s, the newest; the oldest mapping
    /// past the bound is tombstoned.
    fn map(&mut self, vendor_turn: &str, turn: TurnNumber) {
        if self.turns.iter().any(|(known, _)| known == vendor_turn) {
            return;
        }
        if self.turns.len() == VENDOR_TURNS
            && let Some((expired, _)) = self.turns.pop_front()
        {
            self.tombstone(&expired);
        }
        self.turns.push_back((vendor_turn.to_owned(), turn));
    }

    /// Keeps `vendor_turn`'s tombstone, the oldest dropped past the bound.
    fn tombstone(&mut self, vendor_turn: &str) {
        if self.tombstones.len() == TOMBSTONES {
            self.tombstones.pop_front();
        }
        self.tombstones.push_back(tombstone_of(vendor_turn));
    }

    /// The turn an item naming `vendor_turn` belongs to while `running`
    /// runs, or between turns with `None` (C2 §2): a mapped vendor turn is
    /// the running turn's or an earlier turn's (late), and a tombstoned
    /// one has expired. A genuinely unseen one, or none, is the running
    /// turn's while that turn has no vendor turn of its own yet (it names
    /// it at acceptance), else session-level.
    fn attribute(&self, vendor_turn: Option<&str>, running: Option<TurnNumber>) -> Attribution {
        if let Some(vendor_turn) = vendor_turn {
            let known = self
                .turns
                .iter()
                .find(|(known, _)| known == vendor_turn)
                .map(|(_, turn)| *turn);
            match known {
                Some(turn) if Some(turn) == running => return Attribution::Current,
                Some(turn) => return Attribution::Late(turn),
                None if self.tombstones.contains(&tombstone_of(vendor_turn)) => {
                    return Attribution::Expired;
                }
                None => {}
            }
        }
        match running {
            Some(running) if vendor_turn.is_none() || !self.mapped(running) => Attribution::Current,
            Some(_) | None => Attribution::Session,
        }
    }

    /// Whether `turn` has its vendor turn mapped.
    fn mapped(&self, turn: TurnNumber) -> bool {
        self.turns.iter().any(|(_, mapped)| *mapped == turn)
    }
}

/// The C2 §2 `SessionRef` of a session's stored route identity (decision
/// H3). What the Store does not hold is left empty, never invented: no
/// adapter serves an empty route.
pub(super) fn session_ref(route: &SessionRoute) -> SessionRef {
    SessionRef {
        harness: route.harness.clone(),
        route: route.route.clone().unwrap_or_default(),
        adapter_version: route.adapter_version.clone().unwrap_or_default(),
    }
}

/// A vendor turn ID's tombstone: its 64-bit hash, the same in every lane.
fn tombstone_of(vendor_turn: &str) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BuildHasherDefault::<DefaultHasher>::default().hash_one(vendor_turn)
}

impl Lane {
    /// Whether the driver's health failed (C2 §2), or its close after
    /// that started: its connection is not used for another turn.
    fn failed(&self) -> bool {
        self.retired.load(std::sync::atomic::Ordering::Acquire)
            || matches!(*self.driver.health().borrow(), DriverHealth::Failed { .. })
    }

    /// Closes the failed driver once (C2 §2 health), releasing what it
    /// holds, its connection slot included; its owned cleanup stays its own.
    async fn retire(&self) {
        if self.retired.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let deadline = Deadline::at(tokio::time::Instant::now() + REPLACE_CLOSE);
        let _report = self.driver.close(CloseMode::Force, deadline).await;
    }

    /// Records `vendor_turn` as `turn`'s, the newest.
    pub(super) fn map_vendor_turn(&self, vendor_turn: &str, turn: TurnNumber) {
        lock(&self.state).map(vendor_turn, turn);
    }

    /// The turn an item naming `vendor_turn` belongs to while `running`
    /// runs, or between turns with `None` ([`LaneState::attribute`]).
    pub(super) fn attribute(
        &self,
        vendor_turn: Option<&str>,
        running: Option<TurnNumber>,
    ) -> Attribution {
        lock(&self.state).attribute(vendor_turn, running)
    }

    /// The session's confirmed identity, as the lane last saw it.
    pub(super) fn identity(&self) -> Option<Identity> {
        lock(&self.state).identity.clone()
    }

    /// Records a newly confirmed identity.
    pub(super) fn confirm(&self, identity: Identity) {
        lock(&self.state).identity = Some(identity);
    }

    /// The event connection `generation`'s confirmed identity commits
    /// (C2 §2 delayed identity): `None` once the generation committed one,
    /// else whether it is the session's first (`session.opened`) or a
    /// later one (`session.reopened`).
    pub(super) fn opens(&self, generation: &str) -> Option<bool> {
        let state = lock(&self.state);
        (state.committed.as_deref() != Some(generation)).then_some(!state.opened)
    }

    /// Records the committed open of connection `generation` with its
    /// confirmed `identity`.
    pub(super) fn opened(&self, generation: String, identity: Identity) {
        let mut state = lock(&self.state);
        state.opened = true;
        state.committed = Some(generation);
        state.identity = Some(identity);
    }

    /// Whether this daemon committed the confirmed identity of the lane's
    /// connection (C1 §3.7 `vendor_identity_verified`, decision H3).
    pub(super) fn verified(&self) -> bool {
        lock(&self.state).committed.is_some()
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
    /// The vendor's own stop reason, kept on every outcome (AD4, #36).
    pub(super) vendor_stop_reason: String,
    /// Inline; `None` once it spilled to `structured_output_file`.
    pub(super) structured_output: Option<Value>,
    /// The durable `structured_output.json` of a spilled value (C1 §5).
    pub(super) structured_output_file: Option<StructuredOutputFile>,
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
            vendor_stop_reason: terminal.vendor_stop_reason.clone(),
            structured_output: terminal.structured_output.as_deref().and_then(parse),
            structured_output_file: None,
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

    /// Sol r1 F3 (C2 §2 health, AD16): retires the session's kept lane if
    /// its driver's health failed, before the next turn reserves a
    /// connection slot, so the failed driver's own slot is released first.
    /// The lane stays for [`Self::lane`] to replace with the identity and
    /// tombstones it holds.
    pub(super) async fn retire_failed_lane(&self, session: &SessionId) {
        let kept = lock(&self.lanes).get(session).cloned();
        if let Some(lane) = kept.filter(|lane| lane.failed()) {
            lane.retire().await;
        }
    }

    /// The session's lane for its submitted turn: the one it has, unless
    /// its driver's health failed, which is closed and replaced with the
    /// identity it confirmed (C2 §2 health); else a lane opened for the
    /// turn's `model` in `cwd` (C2 §2 `open_session`, logical: no vendor
    /// I/O), from the session's stored route identity `route` (Sol r1 F12,
    /// decision H3). A route identity the Store does not hold is not
    /// invented: the driver then has no adapter and refuses the turn.
    pub(super) async fn lane(
        &self,
        session: &SessionId,
        route: &SessionRoute,
        model: &str,
        cwd: PathBuf,
    ) -> Arc<Lane> {
        let kept = lock(&self.lanes).get(session).cloned();
        let mut state = LaneState::recovered(route);
        if let Some(lane) = kept {
            if !lane.failed() {
                return lane;
            }
            // C2 §2: no turn runs on a failed driver's connection.
            lock(&self.lanes).remove(session);
            state = lock(&lane.state).successor();
            lane.retire().await;
        }
        let reference = session_ref(route);
        let spec = SessionSpec {
            session_id: session.clone(),
            model: model.to_owned(),
            instructions: None,
            initial_bound: None,
            cwd,
            vendor: VendorOptions::new(),
            inherit: Inherit::OD2_DEFAULT,
            confirmed_vendor_session_id: state
                .identity
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
            reference,
            observations: tokio::sync::Mutex::new(receiver),
            state: StdMutex::new(state),
            retired: std::sync::atomic::AtomicBool::new(false),
        });
        lock(&self.lanes).insert(session.clone(), Arc::clone(&lane));
        lane
    }

    /// Restart recovery's resumed driver (C2 §2 Recover) becomes the
    /// session's lane, with the channel its recovery was given.
    pub(super) fn adopt_lane(
        &self,
        session: &SessionId,
        (driver, receiver): (SessionDriver, mpsc::Receiver<Admitted>),
        (reference, route): (SessionRef, &SessionRoute),
    ) {
        let lane = Arc::new(Lane {
            driver,
            reference,
            observations: tokio::sync::Mutex::new(receiver),
            state: StdMutex::new(LaneState::recovered(route)),
            retired: std::sync::atomic::AtomicBool::new(false),
        });
        lock(&self.lanes).insert(session.clone(), lane);
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

#[cfg(test)]
mod tests {
    use super::{Attribution, LaneState, VENDOR_TURNS};
    use crate::TurnNumber;

    fn turn(number: u32) -> TurnNumber {
        TurnNumber::try_from(number).expect("a turn number")
    }

    /// Sol r1 F6 (C2 §2 observations): a mapped vendor turn is the running
    /// turn's or an earlier turn's (late); a genuinely unseen one is the
    /// running turn's only before that turn has its own vendor turn, and
    /// otherwise, or between turns, session-level; one evicted past the
    /// bound, or held by a replaced lane, has expired.
    #[test]
    fn vendor_turns_attribute_current_late_session_or_expired() {
        let mut state = LaneState::default();
        // Before turn 1 maps its vendor turn, unseen traffic is its own.
        assert_eq!(
            state.attribute(Some("v1"), Some(turn(1))),
            Attribution::Current
        );
        assert_eq!(state.attribute(None, Some(turn(1))), Attribution::Current);
        state.map("v1", turn(1));
        assert_eq!(
            state.attribute(Some("v1"), Some(turn(1))),
            Attribution::Current
        );
        assert_eq!(
            state.attribute(Some("x"), Some(turn(1))),
            Attribution::Session
        );
        // Between turns.
        assert_eq!(
            state.attribute(Some("v1"), None),
            Attribution::Late(turn(1))
        );
        assert_eq!(state.attribute(Some("x"), None), Attribution::Session);
        assert_eq!(state.attribute(None, None), Attribution::Session);
        // `v1` is evicted by the bound's worth of later vendor turns.
        for number in 2..=u32::try_from(VENDOR_TURNS).expect("a small bound") + 1 {
            state.map(&format!("v{number}"), turn(number));
        }
        assert_eq!(
            state.attribute(Some("v1"), Some(turn(65))),
            Attribution::Expired
        );
        assert_eq!(
            state.attribute(Some("v2"), Some(turn(65))),
            Attribution::Late(turn(2))
        );
        // A replacing lane keeps none of the mappings: all expired.
        let successor = state.successor();
        for id in ["v1", "v2", "v65"] {
            assert_eq!(
                successor.attribute(Some(id), None),
                Attribution::Expired,
                "{id}"
            );
        }
        assert_eq!(successor.attribute(Some("x"), None), Attribution::Session);
    }
}
