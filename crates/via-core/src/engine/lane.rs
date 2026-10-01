//! A session's lane on its adapter driver (C2 §2; adapter design AD3, AD4,
//! AD16): the driver, opened at the session's first dispatch from its
//! durable route identity, the session's observation channel, the map from
//! vendor turns to turns, and what a turn's observations and its end
//! establish for its envelope.
//!
//! A lane outlives its dispatcher, so a later turn can pin the live
//! connection its driver keeps (AD16) and the confirmed identity is kept;
//! the session's close or final shutdown ends it. One lifecycle state,
//! under one lock with the channel's receiver, orders a turn's claim
//! against retirement (Sol r2 #1, #2). The lane's monitor (Sol r1 F2, F4)
//! drains the channel whenever no turn holds it, committing each durable
//! item as it arrives (C2 §2 session drain, decision H3 as narrowed), and
//! watches the driver's health throughout: a driver whose health failed is
//! retired once no turn holds the lane, and replaced at the session's next
//! turn (C2 §2 health).

use std::{
    collections::VecDeque,
    ops::Deref,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime},
};

use serde_json::{Map, Value};
use tokio::sync::{mpsc, watch};
use via_adapters::{
    Admitted, CancellationToken, CloseMode, DriverFailure, DriverHealth, Inherit, Observation,
    ObservationBudget, SessionCx, SessionDriver, SessionRef, SessionSpec, TaskTracker, UsageSample,
    VendorOptions, VendorTerminal, observation_channel_in,
};
use via_store::{SessionIdentity, SessionRoute};

use super::journal::SessionWrite;
use super::progress::UsageLedger;
use super::{Engine, SessionWriter, lock};
use crate::api::{
    AutoDeclined, DeniedAction, EventBody, Kept, StructuredOutputFile, Warning, rfc3339,
};
use crate::{Deadline, SessionId, TurnNumber};

/// Vendor turn IDs a lane remembers: late observations of older turns are
/// dropped.
pub(super) const VENDOR_TURNS: usize = 64;

/// Tombstones a lane keeps of vendor turns whose mapping expired (C2 §2):
/// 64-bit hashes, so the bound is small whatever the IDs' length.
const TOMBSTONES: usize = 1024;

/// How long retiring a failed driver waits for its close.
const REPLACE_CLOSE: Duration = Duration::from_secs(3);

/// How long final shutdown's session drain may take before Host
/// reconciliation: the lanes' monitors joined and a few Store commits
/// ([`Engine::drop_lanes`]).
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(1);

/// A lane's lifecycle (Sol r2 #1, #2). A turn claims the lane only from
/// `Idle`, before its driver is prepared, and gives it back when its claim
/// drops; retirement starts only from `Idle`, so it never closes a driver
/// a turn holds, and `Retired` is set only once the close ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Life {
    Idle,
    Claimed,
    Retiring,
    Retired,
}

/// The lifecycle and the session channel's receiver, under one lock.
struct Core {
    life: Life,
    /// Absent while the monitor or a turn holds it ([`Observed`]).
    receiver: Option<mpsc::Receiver<Admitted>>,
    /// A turn waits for the receiver: the monitor gives it up.
    wanted: bool,
    /// A turn holds the receiver.
    turn_holds: bool,
}

/// A turn's claim on its lane (Sol r2 #1), taken before the driver is
/// selected or prepared and held until the turn ends, however it ends; the
/// lane is `Idle` again when it drops.
pub(super) struct LaneClaim(Arc<Lane>);

impl LaneClaim {
    /// The claimed lane.
    #[cfg(test)]
    pub(super) fn lane(&self) -> &Arc<Lane> {
        &self.0
    }
}

impl Deref for LaneClaim {
    type Target = Lane;

    fn deref(&self) -> &Lane {
        &self.0
    }
}

impl Drop for LaneClaim {
    fn drop(&mut self) {
        {
            let mut core = lock(&self.0.core);
            if core.life == Life::Claimed {
                core.life = Life::Idle;
            }
        }
        self.0.changed.send_replace(());
    }
}

/// The session channel's receiver, taken from its lane and given back
/// when this drops, so what its holder did not receive stays for the
/// lane's next holder (Sol r2 #3).
pub(super) struct Observed<'a> {
    lane: &'a Lane,
    receiver: Option<mpsc::Receiver<Admitted>>,
    /// A turn's, not the monitor's or a closer's.
    turn: bool,
}

impl Observed<'_> {
    /// The next item, `None` once every sender is gone (cancel-safe).
    pub(super) async fn recv(&mut self) -> Option<Admitted> {
        match self.receiver.as_mut() {
            Some(receiver) => receiver.recv().await,
            None => None,
        }
    }

    /// An item already queued, if any.
    pub(super) fn try_recv(&mut self) -> Option<Admitted> {
        self.receiver.as_mut()?.try_recv().ok()
    }
}

impl Drop for Observed<'_> {
    fn drop(&mut self) {
        {
            let mut core = lock(&self.lane.core);
            if let Some(receiver) = self.receiver.take() {
                core.receiver = Some(receiver);
            }
            if self.turn {
                core.turn_holds = false;
            }
        }
        self.lane.changed.send_replace(());
    }
}

/// One session's driver and observation channel.
pub(super) struct Lane {
    pub(super) driver: SessionDriver,
    /// The session's route identity the driver was opened with.
    pub(super) reference: SessionRef,
    core: StdMutex<Core>,
    /// Marked at every change of `core` that a waiter needs: a claim and
    /// its end, a turn wanting the receiver and its return, and the end of
    /// retirement.
    changed: watch::Sender<()>,
    /// The session's observation byte budget, the same across replacement
    /// (Sol r2 #9).
    budget: ObservationBudget,
    state: StdMutex<LaneState>,
    /// Commits what arrives outside a running turn.
    writer: SessionWriter,
    /// Owns the retirement task.
    tracker: TaskTracker,
    /// The monitor's task, joined when the lane is replaced or removed.
    monitor: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    /// Ends the monitor.
    cancel: CancellationToken,
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
    /// The driver's first health failure, as the monitor saw it (C2 §2).
    first_cause: Option<DriverFailure>,
}

/// A confirmed vendor identity and its transcript hint (C1 §5, AD6).
#[derive(Clone, Debug)]
pub(super) struct Identity {
    pub(super) vendor_session_id: String,
    pub(super) transcript: Option<String>,
}

impl Identity {
    /// The session's identity columns this identity's open event writes
    /// (decision H3 as narrowed).
    pub(super) fn columns(&self) -> SessionIdentity {
        SessionIdentity {
            vendor_session_id: self.vendor_session_id.clone(),
            transcript: self.transcript.clone(),
        }
    }
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
        let identity = route
            .vendor_session_id
            .clone()
            .map(|vendor_session_id| Identity {
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
            first_cause: None,
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
    /// A new lane on `driver`, with its channel's `receiver` on the
    /// session's `budget`, in lifecycle `life`.
    fn new(
        (driver, reference): (SessionDriver, SessionRef),
        (receiver, budget): (mpsc::Receiver<Admitted>, ObservationBudget),
        (state, life): (LaneState, Life),
        (writer, tracker, cancel): (SessionWriter, TaskTracker, CancellationToken),
    ) -> Self {
        Self {
            driver,
            reference,
            core: StdMutex::new(Core {
                life,
                receiver: Some(receiver),
                wanted: false,
                turn_holds: false,
            }),
            changed: watch::Sender::new(()),
            budget,
            state: StdMutex::new(state),
            writer,
            tracker,
            monitor: StdMutex::new(None),
            cancel,
        }
    }

    fn life(&self) -> Life {
        lock(&self.core).life
    }

    fn health_failed(&self) -> bool {
        matches!(*self.driver.health().borrow(), DriverHealth::Failed { .. })
    }

    /// Whether the driver's health failed (C2 §2), or its retirement
    /// started: its connection is not used for another turn.
    pub(super) fn failed(&self) -> bool {
        matches!(self.life(), Life::Retiring | Life::Retired) || self.health_failed()
    }

    /// Claims the lane for a turn (Sol r2 #1): only from `Idle`, with the
    /// driver's health not failed, both read under the lifecycle lock.
    pub(super) fn claim(self: &Arc<Self>) -> Option<LaneClaim> {
        {
            let mut core = lock(&self.core);
            if core.life != Life::Idle || self.health_failed() {
                return None;
            }
            core.life = Life::Claimed;
        }
        self.changed.send_replace(());
        Some(LaneClaim(Arc::clone(self)))
    }

    /// Starts the lane's retirement from `Idle` (C2 §2 health, Sol r2 #2):
    /// a task the tracker owns closes the driver, releasing what it holds,
    /// its connection slot included, and only then marks the lane
    /// `Retired`. True once the lane is retiring or retired; false while a
    /// turn holds its claim.
    pub(super) fn begin_retire(self: &Arc<Self>) -> bool {
        {
            let mut core = lock(&self.core);
            match core.life {
                Life::Claimed => return false,
                Life::Retiring | Life::Retired => return true,
                Life::Idle => core.life = Life::Retiring,
            }
        }
        let lane = Arc::clone(self);
        self.tracker.spawn(async move {
            let deadline = Deadline::at(tokio::time::Instant::now() + REPLACE_CLOSE);
            let _report = lane.driver.close(CloseMode::Force, deadline).await;
            lock(&lane.core).life = Life::Retired;
            lane.changed.send_replace(());
        });
        true
    }

    /// Waits for the end of the lane's retirement, whoever started it:
    /// the shared completion every caller awaits.
    pub(super) async fn retired(&self) {
        let mut changes = self.changed.subscribe();
        while self.life() != Life::Retired {
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    /// Retires the lane once no turn holds it, and waits for the end.
    async fn retire_now(self: &Arc<Self>) {
        let mut changes = self.changed.subscribe();
        while !self.begin_retire() {
            if changes.changed().await.is_err() {
                return;
            }
        }
        self.retired().await;
    }

    /// The session channel's receiver for a running turn: the monitor
    /// gives it up, and it comes back to the lane when the turn drops it.
    pub(super) async fn observe(&self) -> Observed<'_> {
        lock(&self.core).wanted = true;
        self.changed.send_replace(());
        let mut changes = self.changed.subscribe();
        loop {
            {
                let mut core = lock(&self.core);
                if let Some(receiver) = core.receiver.take() {
                    core.wanted = false;
                    core.turn_holds = true;
                    return Observed {
                        lane: self,
                        receiver: Some(receiver),
                        turn: true,
                    };
                }
            }
            if changes.changed().await.is_err() {
                return Observed {
                    lane: self,
                    receiver: None,
                    turn: false,
                };
            }
        }
    }

    /// The receiver for the monitor, unless a turn wants it or the lane is
    /// retiring.
    fn take_for_monitor(&self) -> Option<Observed<'_>> {
        let mut core = lock(&self.core);
        if core.wanted || matches!(core.life, Life::Retiring | Life::Retired) {
            return None;
        }
        let receiver = core.receiver.take()?;
        Some(Observed {
            lane: self,
            receiver: Some(receiver),
            turn: false,
        })
    }

    /// The driver's first health failure the monitor saw.
    #[cfg(test)]
    pub(super) fn first_cause(&self) -> Option<DriverFailure> {
        lock(&self.state).first_cause.clone()
    }

    /// Whether a turn holds the session channel.
    #[cfg(test)]
    pub(super) fn turn_holds_channel(&self) -> bool {
        lock(&self.core).turn_holds
    }

    /// The session's byte budget.
    #[cfg(test)]
    pub(super) fn budget(&self) -> &ObservationBudget {
        &self.budget
    }

    /// The session drain (C2 §2 observations before turns, decision H3 as
    /// narrowed): an item received while no turn runs. A durable one is
    /// committed at once with its own attribution: an identity as the
    /// session's open event with its columns, a denial, decline or warning
    /// session-level, or late with its turn when it names an earlier turn.
    /// An expired one and every non-durable one (acceptance, progress,
    /// final text, steer report, vendor close, mismatch, late terminal) is
    /// dropped. A vendor close needs nothing of Core: the driver ends the
    /// connection, and the next turn reopens it. Its budget returns once
    /// it is handled.
    pub(super) async fn dispose(&self, admitted: Admitted) {
        let Admitted { item, permit } = admitted;
        let at = rfc3339(SystemTime::now());
        match &item.observation {
            Observation::IdentityConfirmed(identity) => {
                let confirmed = Self::confirmed(Some(self), identity);
                let version = identity.vendor_version.clone();
                let Some(body) = self.open_event(&identity.connection_id, &confirmed, version)
                else {
                    self.confirm(confirmed);
                    return;
                };
                let columns = Some(confirmed.columns());
                let written = self.writer.commit((body, &at, (None, false)), columns).await;
                if let SessionWrite::Committed = written {
                    self.opened(identity.connection_id.clone(), confirmed);
                }
            }
            Observation::ActionDenied(_)
            | Observation::RequestDeclined(_)
            | Observation::Warning(_) => {
                let vendor_turn = item
                    .vendor_turn
                    .as_ref()
                    .map(via_adapters::VendorTurnId::as_str);
                let attributed = match self.attribute(vendor_turn, None) {
                    Attribution::Late(turn) => (Some(turn.get()), true),
                    Attribution::Current | Attribution::Session => (None, false),
                    Attribution::Expired => return,
                };
                if let Some(body) = super::drive::held_body(&item.observation) {
                    self.writer.commit((body, &at, attributed), None).await;
                }
            }
            Observation::Accepted(_)
            | Observation::Progress(_)
            | Observation::FinalText(_)
            | Observation::SteerDelivered(_)
            | Observation::VendorClosed(_)
            | Observation::ResumeMismatch { .. }
            // Discarded until via-jm4.35: a late terminal's revision
            // write is not in the Store yet.
            | Observation::LateTerminal(_) => {}
        }
        drop(permit);
    }

    /// Commits or drops what the channel still has, once the lane's
    /// monitor is joined; a turn's holding of the receiver is left alone.
    async fn drain_rest(&self) {
        let receiver = lock(&self.core).receiver.take();
        let mut observed = Observed {
            lane: self,
            receiver,
            turn: false,
        };
        while let Some(admitted) = observed.try_recv() {
            self.dispose(admitted).await;
        }
    }

    /// Ends the monitor and waits for it: an item it is committing is
    /// finished, and it gives the receiver back.
    async fn stop_monitor(&self) {
        self.cancel.cancel();
        let monitor = lock(&self.monitor).take();
        if let Some(monitor) = monitor {
            let _joined = monitor.await;
        }
    }

    /// The lane's monitor (C2 §2; Sol r1 F2, F4, Sol r2 #1-#3), on the
    /// daemon's tracker for the lane's life. Whenever no turn wants the
    /// session channel it drains it, committing each durable item as it
    /// arrives ([`Self::dispose`]). It is also the independent health
    /// consumer: it keeps the driver's first failure and retires the failed
    /// driver once no turn holds the lane (an abandoned turn's claim is
    /// gone), without waiting for an observation or a dispatch. It ends
    /// with the driver's close, the start of retirement, or the lane's or
    /// the daemon's cancellation.
    pub(super) async fn monitor(self: Arc<Self>) {
        let mut health = self.driver.health();
        let mut changes = self.changed.subscribe();
        loop {
            let failed = match &*health.borrow_and_update() {
                DriverHealth::Open => false,
                DriverHealth::Failed { first_cause } => {
                    lock(&self.state)
                        .first_cause
                        .get_or_insert_with(|| first_cause.clone());
                    true
                }
                DriverHealth::Closed => return,
            };
            changes.borrow_and_update();
            if failed {
                // Test builds: the monitor holds between its health read
                // and its retirement (Sol r2 #1), until it is ended.
                #[cfg(feature = "test-failpoints")]
                tokio::select! {
                    _held = via_store::failpoint::hit_async("core.lane.retire") => {}
                    () = self.cancel.cancelled() => return,
                }
                if self.begin_retire() {
                    return;
                }
            }
            if let Some(observed) = self.take_for_monitor() {
                if !self
                    .drain_between(observed, failed, (&mut health, &mut changes))
                    .await
                {
                    return;
                }
                continue;
            }
            tokio::select! {
                () = self.cancel.cancelled() => return,
                moved = health.changed() => if moved.is_err() { return },
                bumped = changes.changed() => if bumped.is_err() { return },
            }
        }
    }

    /// Drains the session channel until a turn wants it, the driver's
    /// health changes, or a failed driver's claim ends; `false` once the
    /// monitor ends.
    async fn drain_between(
        &self,
        mut observed: Observed<'_>,
        failed: bool,
        (health, changes): (&mut watch::Receiver<DriverHealth>, &mut watch::Receiver<()>),
    ) -> bool {
        let mut open = true;
        loop {
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => return false,
                moved = health.changed() => return moved.is_ok(),
                bumped = changes.changed() => {
                    if bumped.is_err() {
                        return false;
                    }
                    let core = lock(&self.core);
                    if core.wanted || (failed && core.life == Life::Idle) {
                        return true;
                    }
                }
                admitted = observed.recv(), if open => match admitted {
                    Some(admitted) => self.dispose(admitted).await,
                    None => open = false,
                },
            }
        }
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

    /// A confirmed `identity` as the session's (C2 §2 delayed identity):
    /// the same vendor session keeps the transcript hint `lane` knows when
    /// the confirmation names none.
    pub(super) fn confirmed(
        lane: Option<&Self>,
        identity: &via_adapters::observation::Identity,
    ) -> Identity {
        let mut confirmed = Identity {
            vendor_session_id: identity.vendor_session_id.clone(),
            transcript: identity
                .transcript
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        };
        if let (None, Some(known)) = (&confirmed.transcript, lane.and_then(Self::identity))
            && known.vendor_session_id == confirmed.vendor_session_id
        {
            confirmed.transcript = known.transcript;
        }
        confirmed
    }

    /// The open event connection `generation`'s `confirmed` identity
    /// commits (C1 §6.1, C2 §2), unless the generation committed one:
    /// `session.opened` for the session's first, else `session.reopened`,
    /// with the handshake's `vendor_version`.
    pub(super) fn open_event(
        &self,
        generation: &str,
        confirmed: &Identity,
        vendor_version: Option<String>,
    ) -> Option<EventBody> {
        let first = self.opens(generation)?;
        let route = self.reference.route.clone();
        let vendor_session_id = confirmed.vendor_session_id.clone();
        Some(if first {
            EventBody::SessionOpened {
                route,
                vendor_session_id,
                vendor_version,
            }
        } else {
            EventBody::SessionReopened {
                route,
                vendor_session_id,
                vendor_version,
                reason: "resume",
            }
        })
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
    /// The turn's own committed adapter warnings of C1 §5's closed list,
    /// one per code, as its envelope's ([`Warning::adapter`]).
    pub(super) warnings: Vec<Warning>,
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
    /// `status` reports verification from.
    pub(super) fn kept_lane(&self, session: &SessionId) -> Option<Arc<Lane>> {
        lock(&self.lanes)
            .get(session)
            .filter(|lane| !lane.failed())
            .cloned()
    }

    /// The session's kept lane claimed for its next turn, before the
    /// driver is prepared (C2 §2 health, Sol r1 F3, Sol r2 #1). A lane
    /// whose driver failed is retired first, so its connection slot is
    /// free before the turn reserves one, and the turn opens a successor
    /// ([`Self::open_lane`]). Retirement is shared: whoever started it,
    /// this waits for its end.
    pub(super) async fn claim_lane(&self, session: &SessionId) -> Option<LaneClaim> {
        let lane = lock(&self.lanes).get(session).cloned()?;
        if let Some(claim) = lane.claim() {
            return Some(claim);
        }
        if lane.begin_retire() {
            lane.retired().await;
        }
        None
    }

    /// The session's lane for its submitted turn, which found none to
    /// claim, opened for the turn's `model` in `cwd` (C2 §2
    /// `open_session`, logical: no vendor I/O) from the session's stored
    /// route identity `route` (Sol r1 F12, decision H3), and claimed. A
    /// route identity the Store does not hold is not invented: the driver
    /// then has no adapter and refuses the turn. A retired lane it
    /// replaces is removed first (Sol r2 #3, #9): its monitor is joined,
    /// what its channel still has is committed or dropped, and the
    /// successor keeps its identity, its tombstones and the session's
    /// byte budget.
    pub(super) async fn open_lane(
        &self,
        session: &SessionId,
        route: &SessionRoute,
        model: &str,
        cwd: PathBuf,
    ) -> LaneClaim {
        let replaced = lock(&self.lanes).remove(session);
        let (state, budget) = match replaced {
            Some(lane) => {
                lane.retire_now().await;
                lane.stop_monitor().await;
                lane.drain_rest().await;
                let state = lock(&lane.state).successor();
                (state, lane.budget.clone())
            }
            None => (LaneState::recovered(route), ObservationBudget::new()),
        };
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
        let (sink, receiver) = observation_channel_in(&budget);
        let cx = SessionCx {
            observations: sink,
            tracker: self.tracker.clone(),
            cancel: self.cancel.child_token(),
        };
        let driver = self.adapter.open_session(&reference, spec, cx);
        let lane = self.install_lane(
            session,
            ((driver, reference), (receiver, budget)),
            (state, Life::Claimed),
        );
        LaneClaim(lane)
    }

    /// Restart recovery's resumed driver (C2 §2 Recover) becomes the
    /// session's lane, with the channel its recovery was given on the
    /// session's `budget`.
    pub(super) fn adopt_lane(
        &self,
        session: &SessionId,
        (driver, receiver, budget): (SessionDriver, mpsc::Receiver<Admitted>, ObservationBudget),
        (reference, route): (SessionRef, &SessionRoute),
    ) {
        self.install_lane(
            session,
            ((driver, reference), (receiver, budget)),
            (LaneState::recovered(route), Life::Idle),
        );
    }

    /// Makes a lane the session's and starts its monitor.
    fn install_lane(
        &self,
        session: &SessionId,
        (opened, channel): (
            (SessionDriver, SessionRef),
            (mpsc::Receiver<Admitted>, ObservationBudget),
        ),
        initial: (LaneState, Life),
    ) -> Arc<Lane> {
        let lane = Arc::new(Lane::new(
            opened,
            channel,
            initial,
            (
                self.session_writer(session),
                self.tracker.clone(),
                self.cancel.child_token(),
            ),
        ));
        lock(&self.lanes).insert(session.clone(), Arc::clone(&lane));
        let monitor = self.tracker.spawn(Arc::clone(&lane).monitor());
        *lock(&lane.monitor) = Some(monitor);
        lane
    }

    /// A session's close (C2 §2 Close): its driver is closed by
    /// `deadline`, releasing any connection it holds, unless its
    /// retirement already did, and the lane goes. Its monitor is joined
    /// and what the channel still has is committed or dropped, before
    /// `session.closed` (Sol r2 #3).
    pub(super) async fn close_lane(
        &self,
        session: &SessionId,
        mode: CloseMode,
        deadline: Deadline,
    ) {
        let Some(lane) = lock(&self.lanes).remove(session) else {
            return;
        };
        lane.stop_monitor().await;
        let retiring = {
            let mut core = lock(&lane.core);
            match core.life {
                Life::Retiring | Life::Retired => true,
                Life::Idle | Life::Claimed => {
                    core.life = Life::Retiring;
                    false
                }
            }
        };
        if retiring {
            lane.retired().await;
        } else {
            let _report = lane.driver.close(mode, deadline).await;
            lock(&lane.core).life = Life::Retired;
            lane.changed.send_replace(());
        }
        lane.drain_rest().await;
    }

    /// Final shutdown: every driver's owned work is cancelled and every
    /// lane dropped, before Host reconciliation. Each lane's monitor is
    /// joined and what its channel still has is committed or dropped
    /// first, by `by` and within [`SHUTDOWN_DRAIN`] (C2 §2 session drain;
    /// Sol r2 #3).
    pub(super) async fn drop_lanes(&self, by: tokio::time::Instant) {
        self.cancel.cancel();
        self.tracker.close();
        let lanes: Vec<Arc<Lane>> = lock(&self.lanes).drain().map(|(_, lane)| lane).collect();
        let by = by.min(tokio::time::Instant::now() + SHUTDOWN_DRAIN);
        let _bounded = tokio::time::timeout_at(by, async {
            for lane in &lanes {
                lane.stop_monitor().await;
                lane.drain_rest().await;
            }
        })
        .await;
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
