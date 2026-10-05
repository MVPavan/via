//! A session's lane on its adapter driver (C2 §2; adapter design AD3, AD4,
//! AD16): the driver, opened at the session's first dispatch from its
//! durable route identity, the session's observation channel, the map from
//! vendor turns to turns, and what a turn's observations and its end
//! establish for its envelope.
//!
//! A lane outlives its dispatcher, so a later turn can pin the live
//! connection its driver keeps (AD16) and the confirmed identity is kept;
//! the session's close, its driver's failure, the idle lanes' bound (C2
//! §3 idle lanes) or final shutdown ends it.
//! One task on the daemon's tracker, the lane's actor (Sol r3 N1-N5),
//! owns the session channel's receiver and the lane's lifecycle for the
//! lane's whole life, and runs each operation on them to completion: a
//! submitted turn, the session drain between turns (C2 §2, decision H3 as
//! narrowed: each durable item committed as it arrives, one at a time),
//! the retirement of a driver whose health failed (C2 §2 health), the
//! session's close and final shutdown's drain. Callers hand an operation
//! over and await its completion; dropping a caller never cancels the
//! operation or strands what it holds. Only a turn's stop order, the
//! daemon's force and the drivers' cancellation stop work, inside the
//! actor. However the lane ends, what its channel still has is disposed of
//! before its end is published, and it stays the session's lane until then.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    ops::Deref,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use serde_json::{Map, Value};
use tokio::sync::{OwnedSemaphorePermit, mpsc, watch};
use via_adapters::{
    Admitted, CancellationToken, CloseMode, CloseReport, DriverFailure, DriverHealth, InheritPlan,
    OBSERVATION_BYTES, Observation, ObservationBudget, SessionCx, SessionDriver, SessionRef,
    SessionSpec, SteerToken, UsageSample, VendorTerminal, observation_channel_in,
};
use via_store::{SessionIdentity, SessionRoute, SteerOutcome};

use super::journal::SessionWrite;
use super::progress::UsageLedger;
use super::{Engine, SessionWriter, lock};
use crate::api::{
    AutoDeclined, DeniedAction, EventBody, Kept, StructuredOutputFile, Warning, rfc3339,
};
use crate::intake::{Effective, Frozen};
use crate::{Deadline, SessionId, TurnNumber};

/// Vendor turn IDs a lane remembers: late observations of older turns are
/// dropped.
pub(super) const VENDOR_TURNS: usize = 64;

/// Tombstones a lane keeps of vendor turns whose mapping expired (C2 §2):
/// 64-bit hashes, so the bound is small whatever the IDs' length. One more
/// overflows the lane: a tombstone is never reassigned (C2 §4.1).
pub(super) const TOMBSTONES: usize = 1024;

/// How long retiring a failed or replaced driver, or closing an idle
/// one, waits for its driver close; the lane's drain follows it, unbounded
/// by this.
const REPLACE_CLOSE: Duration = Duration::from_secs(3);

/// Idle session lanes kept daemon-wide (runtime §8; C2 §3 idle lanes):
/// lanes with no turn running or queued for their session and a drained
/// observation channel. Past it the least recently used one's driver is
/// closed, and the session's next dispatch reopens it from its stored
/// identity ([`Engine::evict_idle`]).
pub(super) const IDLE_LANES: usize = 32;

/// Session lanes resident daemon-wide (runtime §8): each holds one from its
/// creation until it has ended, whether serving, idle or retiring. A
/// dispatch that needs a new lane while none is free waits, its turn
/// still queued ([`Engine::reserve_resident`]), and has the least recently
/// used idle lane retired, within [`IDLE_LANES`] too, for its permit
/// ([`Engine::evict_idle`]); with none idle it waits for a lane to end.
pub(super) const RESIDENT_LANES: usize = 320;

/// A dispatch waiting for a resident lane ([`Engine::press_resident`]);
/// dropped when it stops waiting.
pub(super) struct Pressed<'a>(&'a AtomicUsize);

impl Drop for Pressed<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The daemon's lane use clock: each lane's last use is a tick of it, so
/// the least recently used idle lane is the one with the oldest.
static USES: AtomicU64 = AtomicU64::new(0);

/// A new tick of [`USES`].
fn use_tick() -> u64 {
    USES.fetch_add(1, Ordering::Relaxed)
}

/// How long final shutdown waits for the lanes' actors to finish their
/// drain before Host reconciliation ([`Engine::drop_lanes`]): a turn
/// ending under the drivers' cancellation and a few Store commits.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(1);

/// The sessions' lanes, shared with each lane's actor, which removes its
/// lane once its session's close ended it.
pub(super) type Lanes = Arc<StdMutex<HashMap<SessionId, Arc<Lane>>>>;

/// A turn handed to its lane's actor ([`Lane::hand_over`]), run on the
/// actor's [`Inbox`] to its end.
pub(super) type TurnJob = Box<dyn for<'a> FnOnce(&'a mut Inbox) -> TurnRun<'a> + Send>;

/// A handed-over turn's run.
pub(super) type TurnRun<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// `job` as a [`TurnJob`].
pub(super) fn turn_job<F>(job: F) -> TurnJob
where
    F: for<'a> FnOnce(&'a mut Inbox) -> TurnRun<'a> + Send + 'static,
{
    Box::new(job)
}

/// A lane's lifecycle, its actor's (Sol r2 #1, #2; Sol r3 N4): `Open`
/// while it serves; `Ending` once its retirement, eviction or close was
/// asked for, which the actor carries out once no turn holds a claim;
/// `Closing` once the actor started the driver close, its mode and
/// deadline final (critical r2 F6); `Ended` once the driver is closed and
/// what the channel had is disposed of: the lane's end, the completion
/// every caller awaits ([`Lane::retired`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Life {
    Open,
    Ending,
    Closing,
    Ended,
}

/// How a lane ends.
#[derive(Clone, Copy)]
enum Ending {
    /// Its driver failed, or a successor replaces it: closed `Force`
    /// within [`REPLACE_CLOSE`]. It stays the session's lane, so the
    /// session's next turn opens its successor from it.
    Retire,
    /// The session's close (C2 §2 Close), by its mode and deadline.
    Close(CloseMode, Deadline),
    /// The idle lanes' bound (C2 §3 idle lanes, runtime §8): closed
    /// `Graceful` by the deadline. A C1 close asked for before the driver
    /// close started replaces it ([`Lane::begin_close`]).
    Evict(Deadline),
}

/// The lifecycle, a turn's claim and a turn handed over, under one lock.
struct Core {
    life: Life,
    ending: Option<Ending>,
    /// A turn holds the lane, from before its driver is prepared until it
    /// ends (Sol r2 #1): the lane does not end meanwhile.
    claimed: bool,
    /// The session's close or the idle lanes' bound asked for the lane's
    /// end: the lane leaves the session's registration once it ended.
    removed: bool,
    /// The claimed turn handed to the actor, not yet started.
    job: Option<TurnJob>,
}

/// A turn's claim on its lane (Sol r2 #1), taken before the driver is
/// selected or prepared and held until the turn ends, however it ends: it
/// is handed to the actor with the turn, or dropped by a dispatch that
/// does not run. The lane does not end while it is held.
pub(super) struct LaneClaim(Arc<Lane>);

impl LaneClaim {
    /// The claimed lane.
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
        self.0.used.store(use_tick(), Ordering::Relaxed);
        lock(&self.0.core).claimed = false;
        self.0.changed.send_replace(());
        // The lane may be idle now (runtime §8).
        self.0.bound_idle();
    }
}

/// The session channel's receiver, its lane actor's own: a turn the actor
/// runs borrows it, and nothing else ever holds it.
pub(super) struct Inbox(Option<mpsc::Receiver<Admitted>>);

impl Inbox {
    /// No channel: a turn run after its lane ended.
    pub(super) const fn closed() -> Self {
        Self(None)
    }

    /// Test builds: an inbox on `receiver`.
    #[cfg(test)]
    pub(super) const fn of(receiver: mpsc::Receiver<Admitted>) -> Self {
        Self(Some(receiver))
    }

    /// The next item, `None` once every sender is gone (cancel-safe).
    pub(super) async fn recv(&mut self) -> Option<Admitted> {
        match self.0.as_mut() {
            Some(receiver) => receiver.recv().await,
            None => None,
        }
    }

    /// How many items the channel holds now.
    pub(super) fn len(&self) -> usize {
        self.0.as_ref().map_or(0, mpsc::Receiver::len)
    }

    /// An item already queued, if any.
    pub(super) fn try_recv(&mut self) -> Option<Admitted> {
        self.0.as_mut()?.try_recv().ok()
    }

    /// Closes the channel's admission: a send from now on is refused to
    /// its sender, and [`Self::recv`] returns what was admitted before,
    /// then `None` once no sender holds a slot.
    fn close(&mut self) {
        if let Some(receiver) = self.0.as_mut() {
            receiver.close();
        }
    }
}

/// Ready data items a drain handles before it services its controls,
/// deadlines and health again and yields (runtime §8; Sol r4 R5).
pub(super) const READY_ITEMS: usize = 128;

/// Counts one ready item `handled`; each [`READY_ITEMS`]th yields to the
/// runtime, so no backlog holds the task's thread.
pub(super) async fn ready_item(handled: &mut usize) {
    *handled += 1;
    if (*handled).is_multiple_of(READY_ITEMS) {
        tokio::task::yield_now().await;
    }
}

/// One session's driver and observation channel.
pub(super) struct Lane {
    pub(super) driver: SessionDriver,
    /// The session's route identity the driver was opened with.
    pub(super) reference: SessionRef,
    core: StdMutex<Core>,
    /// Marked at every change of `core` the actor or a waiter needs: a
    /// turn handed over, a claim released, an end asked for and the end;
    /// and when the lane's vendor turns fail it.
    changed: watch::Sender<()>,
    /// The session's observation byte budget, the same across replacement
    /// (Sol r2 #9).
    budget: ObservationBudget,
    state: StdMutex<LaneState>,
    /// Commits what arrives outside a running turn.
    writer: SessionWriter,
    /// The drivers' cancellation: final shutdown ends the actor's serving.
    cancel: CancellationToken,
    /// The driver's uncertain journal write was reported to the latch,
    /// once, by whichever of its consumers read it first ([`journal_watch`],
    /// [`Lane::journal_read`]).
    journal_reported: Arc<StdMutex<bool>>,
    /// The lane's last use, a tick of [`USES`]: its making, then each
    /// turn's claim released.
    used: AtomicU64,
    /// The daemon's Engine, whose idle lanes' bound the lane checks when
    /// it becomes idle ([`Lane::bound_idle`]).
    engine: Weak<Engine>,
    /// The driver close's report, once its actor closed it, when a C1
    /// close asked for that driver close or took it over before it started:
    /// that close takes it ([`Engine::close_lane`]).
    report: StdMutex<Option<CloseReport>>,
    /// The completion tickets of the lane's live steer requests (critical
    /// r2 #2).
    steers: Arc<SteerTickets>,
}

/// The tokens Core gives steer input, unique within every session (C2
/// `SteerInput.token`): one counter for the daemon.
static STEER_TOKENS: AtomicU64 = AtomicU64::new(0);

/// Critical r1 #5, r2 #2 (C1 §3.4, C2 `SteerInput.token`): the completion
/// ticket of each live steer request on the lane, by its token. A request
/// registers one before it calls `steer` and retires it when it ends by
/// any path ([`SteerTicket`]); consuming the token's `steer.delivered`
/// resolves it, committed or not, and an observation whose ticket is gone
/// just commits. The lane's end resolves every ticket left as not
/// committed. So the tickets are the lane's live steer requests, never
/// evicted: each holds one from its registration until it is answered.
#[derive(Default)]
struct SteerTickets(StdMutex<TicketBook>);

#[derive(Default)]
struct TicketBook {
    tickets: HashMap<SteerToken, (tokio::sync::oneshot::Sender<bool>, Option<SteerKey>)>,
    /// The lane ended: a later request's ticket is resolved at once.
    closed: bool,
    /// The last turn the lane settled, its observations drained: no
    /// delivery resolves a keyed steer of it or an earlier turn (K2 r4).
    settled: Option<TurnNumber>,
    /// Test builds: the turn the next ticket's drop settles once it
    /// released the book, before the ticket's fields go (K2 r5).
    #[cfg(test)]
    drop_gap: Option<TurnNumber>,
}

impl TicketBook {
    /// Whether no delivery can resolve `key` any more: its turn settled.
    fn settled(&self, key: &SteerKey) -> bool {
        self.settled.is_some_and(|settled| key.number <= settled)
    }
}

impl SteerTickets {
    fn book(&self) -> std::sync::MutexGuard<'_, TicketBook> {
        lock(&self.0)
    }

    /// Resolves the ticket of `token`, if its request still waits.
    fn resolve(&self, token: SteerToken, committed: bool) {
        let ticket = self.book().tickets.remove(&token);
        if let Some((ticket, _)) = ticket {
            // The request ended meanwhile: nobody waits for the answer.
            let _ = ticket.send(committed);
        }
    }

    /// Turn `turn` settled, its admitted observations drained (K2 r4 #1):
    /// a keyed steer of it whose request went away, which its ticket's drop
    /// published under the book's lock (K2 r5), is released, its ownership
    /// with it, since no delivery can resolve it now. One whose request
    /// still waits stays for that request, which takes it back on the
    /// driver's refusal or releases it when it goes away ([`SteerTicket`]).
    fn settle(&self, turn: TurnNumber) {
        let released: Vec<_> = {
            let mut book = self.book();
            book.settled = Some(turn);
            let orphaned: Vec<_> = book
                .tickets
                .iter()
                .filter(|(_, (request, key))| {
                    request.is_closed() && key.as_ref().is_some_and(|key| key.number <= turn)
                })
                .map(|(token, _)| *token)
                .collect();
            orphaned
                .into_iter()
                .filter_map(|token| book.tickets.remove(&token))
                .collect()
        };
        drop(released);
    }

    /// The lane ended: every ticket left is not committed.
    fn close(&self) {
        let tickets = {
            let mut book = self.book();
            book.closed = true;
            std::mem::take(&mut book.tickets)
        };
        for (ticket, _) in tickets.into_values() {
            let _ = ticket.send(false);
        }
    }
}

/// A keyed steer's key, the turn it steers and its ownership, which the
/// lane's ticket book holds under the input's token once the request
/// handed the steer over, so the commit of its `steer.delivered` records
/// its outcome (C1 §3.4, K2 r3). It is released, its ownership with it,
/// when no delivery can resolve it any more: the lane consumed the token's
/// report, committed or not; the request took the steer back after the
/// driver refused it ([`SteerTicket::reclaim`]); its turn settled, the
/// turn's observations drained, with its request gone, or its request went
/// away after that (K2 r4); or the lane ended, drained. The request's
/// ticket leaving before its turn settled does not release it.
pub(super) struct SteerKey {
    /// The steer's `op_key`.
    pub(super) op_key: String,
    /// The steered turn's address.
    pub(super) turn: String,
    /// The steered turn, whose settlement releases the steer (K2 r4).
    pub(super) number: TurnNumber,
    /// The steer's ownership, released when the lane resolves it.
    pub(super) owner: super::steer::Owner,
}

/// One live steer request's completion ticket on its lane (critical r2
/// #2): its token, which the request passes to `steer`, and whether the
/// token's `steer.delivered` committed. Dropped, an unkeyed one retires
/// itself; a keyed one is the lane's to resolve ([`SteerKey`]).
pub(super) struct SteerTicket {
    token: SteerToken,
    resolved: tokio::sync::oneshot::Receiver<bool>,
    tickets: Weak<SteerTickets>,
}

impl SteerTicket {
    /// The token the request's input carries.
    pub(super) const fn token(&self) -> SteerToken {
        self.token
    }

    /// Takes a keyed steer back from the lane after the driver refused its
    /// input, which the driver then reports no delivery of: its ticket
    /// leaves the book, and its ownership returns to the request, which
    /// records the refusal. `None` once the lane resolved it, or unkeyed.
    pub(super) fn reclaim(&self) -> Option<super::steer::Owner> {
        let tickets = self.tickets.upgrade()?;
        let (_, key) = tickets.book().tickets.remove(&self.token)?;
        key.map(|key| key.owner)
    }

    /// Whether the token's `steer.delivered` observation committed: the
    /// wait ends with its consumption, committed or not, or the lane's end.
    pub(super) async fn committed(&mut self) -> bool {
        (&mut self.resolved).await.unwrap_or(false)
    }
}

impl Drop for SteerTicket {
    /// Retires an unkeyed ticket, and a keyed one whose turn settled (K2
    /// r4). A keyed one of a turn not yet settled stays: the lane owns it
    /// until it resolves it (K2 r3). The request's end is published under
    /// the book's lock (K2 r5): a settlement after this drop decided sees
    /// the request gone, even before the ticket's receiver itself goes.
    fn drop(&mut self) {
        if let Some(tickets) = self.tickets.upgrade() {
            let mut book = tickets.book();
            self.resolved.close();
            let retired = book
                .tickets
                .get(&self.token)
                .is_some_and(|(_, key)| key.as_ref().is_none_or(|key| book.settled(key)));
            let released = retired.then(|| book.tickets.remove(&self.token));
            #[cfg(test)]
            let gap = book.drop_gap.take();
            drop(book);
            // Test builds: the turn settles between the book's release and
            // the end of this drop (K2 r5).
            #[cfg(test)]
            if let Some(turn) = gap {
                tickets.settle(turn);
            }
            drop(released);
        }
    }
}

/// What the lane learned from the session's observations.
#[derive(Default)]
struct LaneState {
    /// Vendor turn IDs of the session's accepted turns, oldest first, with
    /// the connection generation that accepted each.
    turns: VecDeque<(String, TurnNumber, u64)>,
    /// Hashes of vendor turn IDs whose mapping expired, oldest first, with
    /// the generation that accepted each.
    tombstones: VecDeque<(u64, u64)>,
    /// The driver's current connection generation, counted by the lane
    /// ([`Lane::new_generation`]): vendor turn ownership is scoped to it
    /// (C2 §2).
    generation: u64,
    /// The session's confirmed vendor identity.
    identity: Option<Identity>,
    /// A `session.opened` is committed: later generations reopen.
    opened: bool,
    /// The driver's connection generation whose identity is committed.
    committed: Option<String>,
    /// The driver's first health failure, as the lane's actor saw it (C2 §2).
    first_cause: Option<DriverFailure>,
    /// A vendor turn's mapping expired with every tombstone taken (C2
    /// §4.1): the lane fails ([`Lane::failed`]).
    overflowed: bool,
    /// An acceptance named a vendor turn already mapped to another turn,
    /// or tombstoned (C2 §4.1, Sol r3 N6): a protocol failure of the lane.
    collided: bool,
}

/// A confirmed vendor identity and its transcript hint (C1 §5, AD6).
#[derive(Clone, Debug)]
pub(super) struct Identity {
    pub(super) vendor_session_id: String,
    pub(super) transcript: Option<String>,
}

impl Identity {
    /// The identity the session last committed, with its transcript hint,
    /// as its columns hold it (decision H3).
    pub(super) fn stored(route: &SessionRoute) -> Option<Self> {
        route
            .vendor_session_id
            .clone()
            .map(|vendor_session_id| Self {
                vendor_session_id,
                transcript: route.transcript.clone(),
            })
    }

    /// The session's identity columns this identity's open event writes
    /// (decision H3 as narrowed).
    pub(super) fn columns(&self) -> SessionIdentity {
        SessionIdentity {
            vendor_session_id: self.vendor_session_id.clone(),
            transcript: self.transcript.clone(),
        }
    }
}

/// What an acceptance's vendor turn ID did to the lane's ownership
/// ([`LaneState::map`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Mapped {
    /// The ID is the turn's.
    Taken,
    /// The ID is another turn's, or tombstoned, in this generation (Sol r3
    /// N6): a protocol failure of the lane; nothing is mapped.
    Collided,
    /// The ID is the turn's, but every tombstone is taken (C2 §4.1): the
    /// lane overflowed.
    Exhausted,
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
        let identity = Identity::stored(route);
        Self {
            opened: identity.is_some(),
            identity,
            ..Self::default()
        }
    }

    /// A replacing lane's state (C2 §2 health): the confirmed identity
    /// only. Vendor turn ownership is scoped per connection generation
    /// (C2 §2), and the replacing driver's generations are new, so no ID
    /// the failed lane mapped or tombstoned can arrive on its channel; for
    /// the same reason a lane recovered at restart reconstructs none
    /// ([`Self::recovered`]). What the failed lane's channel still had was
    /// disposed under its own state first ([`Engine::open_lane`]).
    fn successor(&self) -> Self {
        Self {
            identity: self.identity.clone(),
            opened: self.opened,
            ..Self::default()
        }
    }

    /// Records `vendor_turn` as `turn`'s, the newest (at its acceptance);
    /// the oldest mapping past the bound is tombstoned. With every
    /// tombstone taken by the current connection generation, none is
    /// forgotten to make room: the lane overflows and keeps the mapping
    /// (C2 §4.1, Sol r2 #5). Only an ID unseen in the current generation
    /// is taken (Sol r3 N6): false, mapping nothing, for one it mapped to
    /// another turn or tombstoned; true for one already `turn`'s.
    ///
    /// Ownership is per connection generation (C2 §2, critical r1 #5): an
    /// ID an older generation mapped or tombstoned is the new generation's
    /// to take, and an older generation's tombstone is forgotten first to
    /// make room, so older generations never count toward exhaustion.
    /// Until then they keep late traffic attributed (AD4).
    fn map(&mut self, vendor_turn: &str, turn: TurnNumber) -> Mapped {
        let generation = self.generation;
        let hash = tombstone_of(vendor_turn);
        if self.tombstones.contains(&(hash, generation)) {
            return Mapped::Collided;
        }
        if let Some((_, known, _)) = self
            .turns
            .iter()
            .find(|(known, _, owner)| known == vendor_turn && *owner == generation)
        {
            return if *known == turn {
                Mapped::Taken
            } else {
                Mapped::Collided
            };
        }
        self.turns.retain(|(known, ..)| known != vendor_turn);
        self.tombstones.retain(|(known, _)| *known != hash);
        let mut mapped = Mapped::Taken;
        if self.turns.len() >= VENDOR_TURNS {
            if self.tombstones.len() >= TOMBSTONES {
                let older = self
                    .tombstones
                    .iter()
                    .position(|(_, owner)| *owner != generation);
                if let Some(older) = older {
                    self.tombstones.remove(older);
                } else {
                    self.overflowed = true;
                    mapped = Mapped::Exhausted;
                }
            }
            if mapped == Mapped::Taken
                && let Some((expired, _, owner)) = self.turns.pop_front()
            {
                self.tombstones.push_back((tombstone_of(&expired), owner));
            }
        }
        self.turns
            .push_back((vendor_turn.to_owned(), turn, generation));
        mapped
    }

    /// The turn an item naming `vendor_turn` belongs to while `running`
    /// runs, or between turns with `None` (C2 §2): a mapped vendor turn is
    /// the running turn's or an earlier turn's (late), and a tombstoned
    /// one has expired. An unfamiliar one is session-level, even before
    /// the running turn's acceptance: only the acceptance, correlated with
    /// the turn's start, makes an explicit ID current (Sol r2 #5, F6). An
    /// item naming none is the running turn's, else the session's. A
    /// tombstone is read first: no mapping overrides it (Sol r3 N6).
    fn attribute(&self, vendor_turn: Option<&str>, running: Option<TurnNumber>) -> Attribution {
        let Some(vendor_turn) = vendor_turn else {
            return running.map_or(Attribution::Session, |_| Attribution::Current);
        };
        let hash = tombstone_of(vendor_turn);
        if self.tombstones.iter().any(|(known, _)| *known == hash) {
            return Attribution::Expired;
        }
        let known = self
            .turns
            .iter()
            .find(|(known, ..)| known == vendor_turn)
            .map(|(_, turn, _)| *turn);
        match known {
            Some(turn) if Some(turn) == running => Attribution::Current,
            Some(turn) => Attribution::Late(turn),
            None => Attribution::Session,
        }
    }
}

/// One item's disposal under way ([`Lane::close_draining`]).
type Disposal<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Awaits the disposal under way, if any; never ends without one.
async fn disposal(current: Option<&mut Disposal<'_>>) {
    match current {
        Some(current) => current.await,
        None => std::future::pending().await,
    }
}

/// The driver's journal report's own consumer (critical r2 F3, C2 §2
/// `journal_uncertain`), on the daemon's tracker apart from the lane's
/// actor, so no turn job delays it: an uncertain Host journal write no
/// turn reports latches Store failure as soon as it is published, once
/// for the lane ([`SessionWriter::journal_uncertain`]). It ends once
/// reported, or once the driver's last publisher is gone.
async fn journal_watch(
    mut journal: watch::Receiver<bool>,
    writer: SessionWriter,
    reported: Arc<StdMutex<bool>>,
) {
    loop {
        if *journal.borrow_and_update() {
            writer.journal_uncertain(&reported).await;
            return;
        }
        if journal.changed().await.is_err() {
            return;
        }
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
///
/// Accepted limit (critical r1 #14): a tombstone keeps only this fixed-key
/// 64-bit `SipHash`, not the ID. Two distinct IDs with one hash would be one
/// tombstone; within the bounded set ([`TOMBSTONES`]) such a collision is
/// negligible, and vendor turn IDs are vendor-generated, never chosen by
/// the model.
fn tombstone_of(vendor_turn: &str) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BuildHasherDefault::<DefaultHasher>::default().hash_one(vendor_turn)
}

impl Lane {
    /// A new lane on `driver`, on the session's `budget`, claimed for a
    /// turn when `claimed`.
    fn new(
        (driver, reference): (SessionDriver, SessionRef),
        budget: ObservationBudget,
        (state, claimed): (LaneState, bool),
        (writer, cancel, engine): (SessionWriter, CancellationToken, Weak<Engine>),
    ) -> Self {
        Self {
            driver,
            reference,
            core: StdMutex::new(Core {
                life: Life::Open,
                ending: None,
                claimed,
                removed: false,
                job: None,
            }),
            changed: watch::Sender::new(()),
            budget,
            state: StdMutex::new(state),
            writer,
            cancel,
            journal_reported: Arc::new(StdMutex::new(false)),
            used: AtomicU64::new(use_tick()),
            engine,
            report: StdMutex::new(None),
            steers: Arc::default(),
        }
    }

    fn life(&self) -> Life {
        lock(&self.core).life
    }

    fn health_failed(&self) -> bool {
        matches!(*self.driver.health().borrow(), DriverHealth::Failed { .. })
    }

    /// Whether the lane's vendor turns overflowed its tombstones (C2
    /// §4.1, Sol r2 #5) or an acceptance collided with one it keeps (Sol
    /// r3 N6).
    fn overflowed(&self) -> bool {
        let state = lock(&self.state);
        state.overflowed || state.collided
    }

    /// Whether the driver's health failed (C2 §2), the lane overflowed, or
    /// its end was asked for: its connection is not used for another turn.
    pub(super) fn failed(&self) -> bool {
        self.life() != Life::Open || self.health_failed() || self.overflowed()
    }

    /// Claims the lane for a turn (Sol r2 #1): only while it is open and
    /// unclaimed, with the driver's health not failed and the lane not
    /// overflowed, all read under the lifecycle lock.
    pub(super) fn claim(self: &Arc<Self>) -> Option<LaneClaim> {
        {
            let mut core = lock(&self.core);
            if core.life != Life::Open || core.claimed || self.health_failed() || self.overflowed()
            {
                return None;
            }
            core.claimed = true;
        }
        Some(LaneClaim(Arc::clone(self)))
    }

    /// Asks for the lane's retirement (C2 §2 health, Sol r2 #2): its actor
    /// closes the driver, releasing what it holds, its harness-process slot
    /// included, disposes of what the channel still has, and only then
    /// publishes the lane's end. True once the lane is ending or ended;
    /// false while a turn holds its claim.
    pub(super) fn begin_retire(&self) -> bool {
        {
            let mut core = lock(&self.core);
            if core.life == Life::Open {
                if core.claimed {
                    return false;
                }
                core.life = Life::Ending;
                core.ending = Some(Ending::Retire);
            }
        }
        self.changed.send_replace(());
        true
    }

    /// Asks for the session's close (C2 §2 Close) by `mode` and
    /// `deadline`, unless the lane is already ending; either way the lane
    /// leaves the session's registration once it ended. A close joins an
    /// eviction (C2 §3 idle lanes, critical r2 F6): before the actor
    /// started the driver close, decided under the lifecycle lock, the
    /// close's mode and deadline replace the eviction's; after, the close
    /// waits for it, as for any other ending.
    fn begin_close(&self, mode: CloseMode, deadline: Deadline) {
        {
            let mut core = lock(&self.core);
            let evicting = matches!(core.ending, Some(Ending::Evict(_)));
            if core.life == Life::Open || core.life == Life::Ending && evicting {
                core.life = Life::Ending;
                core.ending = Some(Ending::Close(mode, deadline));
            }
            core.removed = true;
        }
        self.changed.send_replace(());
    }

    /// Whether the lane is idle (C2 §3 idle lanes), as far as the lane
    /// knows: open with a healthy driver, no turn holding or handed over,
    /// and no admitted item outstanding on its channel. The session's
    /// queue is the caller's to read ([`Engine::evict_idle`]).
    fn idle(&self) -> bool {
        let core = lock(&self.core);
        core.life == Life::Open
            && !core.claimed
            && core.job.is_none()
            && !self.health_failed()
            && !self.overflowed()
            && self.budget.available() == OBSERVATION_BYTES
    }

    /// The lane may have become idle (critical r2 F5): a turn released its
    /// claim, or the actor drained its channel between turns. The idle
    /// lanes' bound is enforced at once ([`Engine::evict_idle`]).
    fn bound_idle(&self) {
        if let Some(engine) = self.engine.upgrade() {
            engine.evict_idle();
        }
    }

    /// Asks for an idle lane's end (C2 §3 idle lanes, runtime §8): its
    /// actor closes the driver gracefully, disposes of what the channel
    /// still has and ends the lane, which then leaves the session's
    /// registration. That is not the session's close: it commits no
    /// session-close or eviction event (durable observations admitted
    /// meanwhile still commit as the lane drains; critical r2 F9), and the
    /// session's next dispatch opens a new driver from its stored
    /// identity. Only an open lane no turn holds
    /// or was handed is taken, under the lifecycle lock: a claim or an end
    /// asked for first keeps it.
    pub(super) fn begin_evict(&self) -> bool {
        {
            let mut core = lock(&self.core);
            if core.life != Life::Open || core.claimed || core.job.is_some() {
                return false;
            }
            let deadline = Deadline::at(tokio::time::Instant::now() + REPLACE_CLOSE);
            core.life = Life::Ending;
            core.ending = Some(Ending::Evict(deadline));
            core.removed = true;
        }
        self.changed.send_replace(());
        true
    }

    /// Test builds: the driver close the lane's end asked for, by mode and
    /// deadline, if any.
    #[cfg(all(test, feature = "test-failpoints"))]
    pub(super) fn close_order(&self) -> Option<(CloseMode, tokio::time::Instant)> {
        match lock(&self.core).ending? {
            Ending::Retire => None,
            Ending::Close(mode, deadline) => Some((mode, deadline.instant())),
            Ending::Evict(deadline) => Some((CloseMode::Graceful, deadline.instant())),
        }
    }

    /// Waits for the lane's end, whoever asked for it: the completion
    /// every caller shares.
    pub(super) async fn retired(&self) {
        let mut changes = self.changed.subscribe();
        while self.life() != Life::Ended {
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    /// Whether the lane's actor ended: its channel is drained to its end.
    fn ended(&self) -> bool {
        self.life() == Life::Ended
    }

    /// Records that `observation`, consumed without a commit, was a
    /// `steer.delivered` that did not commit, if it was one.
    pub(super) fn steer_dropped(&self, observation: &Observation) {
        if let Some(token) = observation.steer_token() {
            self.steer_outcome(token, false);
        }
    }

    /// The outcome the `steer.delivered` of `token` records when it is a
    /// keyed steer's still waiting for it (C1 §3.4).
    pub(super) fn keyed_outcome(
        &self,
        token: SteerToken,
        delivery: &via_adapters::SteerDelivery,
    ) -> Option<SteerOutcome> {
        let book = self.steers.book();
        let (_, key) = book.tickets.get(&token)?;
        key.as_ref().map(|key| SteerOutcome {
            op_key: key.op_key.clone(),
            result: super::steer::delivered(&key.turn, delivery),
        })
    }

    /// Turn `turn` settled, the lane having drained its admitted
    /// observations: its keyed steers no delivery can resolve now are
    /// released (K2 r4 #1).
    pub(super) fn settle_steers(&self, turn: TurnNumber) {
        self.steers.settle(turn);
    }

    /// Test builds: the next steer ticket's drop settles `turn` between
    /// its release of the book and its own end (K2 r5).
    #[cfg(test)]
    pub(super) fn settle_in_next_ticket_drop(&self, turn: TurnNumber) {
        self.steers.book().drop_gap = Some(turn);
    }

    /// Records whether the `steer.delivered` observation of `token`
    /// committed, once consumed on any of its paths (critical r1 #5): its
    /// request's ticket, if it still waits, learns it.
    pub(super) fn steer_outcome(&self, token: SteerToken, committed: bool) {
        self.steers.resolve(token, committed);
    }

    /// Registers a steer request's completion ticket, with a new token,
    /// before the request calls `steer` (critical r2 #2).
    #[cfg(test)]
    pub(super) fn steer_ticket(&self) -> SteerTicket {
        self.keyed_steer_ticket(None)
    }

    /// [`Self::steer_ticket`] for a steer keyed by `key`, if any: the
    /// commit of the token's `steer.delivered` records its outcome.
    pub(super) fn keyed_steer_ticket(&self, key: Option<SteerKey>) -> SteerTicket {
        let token = SteerToken::new(STEER_TOKENS.fetch_add(1, Ordering::Relaxed) + 1);
        let (sender, resolved) = tokio::sync::oneshot::channel();
        let mut book = self.steers.book();
        if !book.closed {
            book.tickets.insert(token, (sender, key));
        }
        SteerTicket {
            token,
            resolved,
            tickets: Arc::downgrade(&self.steers),
        }
    }

    /// The live steer tickets.
    #[cfg(test)]
    pub(super) fn steer_tickets(&self) -> usize {
        self.steers.book().tickets.len()
    }

    /// Retires the lane once no turn holds it, and waits for its end.
    pub(super) async fn retire_now(&self) {
        let mut changes = self.changed.subscribe();
        while !self.begin_retire() {
            if changes.changed().await.is_err() {
                return;
            }
        }
        self.retired().await;
    }

    /// Hands the claimed turn `job` to the lane's actor, which runs it to
    /// its end (Sol r3 N1). A lane that already ended gives it back.
    pub(super) fn hand_over(&self, job: TurnJob) -> Result<(), TurnJob> {
        {
            let mut core = lock(&self.core);
            if core.life == Life::Ended {
                return Err(job);
            }
            core.job = Some(job);
        }
        self.changed.send_replace(());
        Ok(())
    }

    /// The driver's first health failure the actor saw.
    #[cfg(test)]
    pub(super) fn first_cause(&self) -> Option<DriverFailure> {
        lock(&self.state).first_cause.clone()
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
    /// session-level, or late with its turn when it names an earlier turn;
    /// so is a steer report (Sol r1 #9). A late terminal of an earlier turn
    /// revises it (C1 §7.6). An expired one and every other non-durable
    /// one (acceptance, progress, final text, vendor close, mismatch) is
    /// dropped. A vendor close needs nothing of Core: the driver ends the
    /// connection, and the next turn reopens it. Its budget returns once
    /// it is handled.
    pub(super) async fn dispose(&self, admitted: Admitted) {
        // Test builds: the lane holds an item it has taken, unhandled.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.lane.dispose").await;
        let Admitted { item, permit } = admitted;
        let at = rfc3339(SystemTime::now());
        match &item.observation {
            // A stale generation's confirmation counts nothing (C2 §2).
            Observation::IdentityConfirmed(identity) if self.current(&identity.connection_id) => {
                let confirmed = Self::confirmed(Some(self), identity);
                let version = identity.vendor_version.clone();
                match self.open_event(&identity.connection_id, &confirmed, version) {
                    Some(body) => {
                        let columns = Some(confirmed.columns());
                        let written = self
                            .writer
                            .commit((body, &at, (None, false)), (columns, None))
                            .await;
                        if let SessionWrite::Committed = written {
                            self.opened(identity.connection_id.clone(), confirmed);
                        }
                    }
                    None if self.changes(&confirmed) => {
                        let written = self.writer.commit_columns(confirmed.columns()).await;
                        if let SessionWrite::Committed = written {
                            self.confirm(confirmed);
                        }
                    }
                    None => {}
                }
            }
            Observation::ActionDenied(_)
            | Observation::RequestDeclined(_)
            | Observation::Warning(_)
            | Observation::SteerDelivered { .. } => {
                let steer = item.observation.steer_token();
                let vendor_turn = item
                    .vendor_turn
                    .as_ref()
                    .map(via_adapters::VendorTurnId::as_str);
                let attributed = match self.attribute(vendor_turn, None) {
                    Attribution::Late(turn) => (Some(turn.get()), true),
                    Attribution::Current | Attribution::Session => (None, false),
                    Attribution::Expired => return self.steer_dropped(&item.observation),
                };
                if let Some(body) = super::drive::held_body(&item.observation) {
                    let keyed = if let Observation::SteerDelivered { delivery, token } =
                        &item.observation
                    {
                        self.keyed_outcome(*token, delivery)
                    } else {
                        None
                    };
                    let written = self
                        .writer
                        .commit((body, &at, attributed), (None, keyed))
                        .await;
                    if let Some(token) = steer {
                        self.steer_outcome(token, matches!(written, SessionWrite::Committed));
                    }
                }
            }
            // C1 §7.6: an ended turn's own late terminal revises it, when
            // its end retained none ([`Engine::revise`]).
            Observation::LateTerminal(terminal) => {
                let vendor_turn = item
                    .vendor_turn
                    .as_ref()
                    .map(via_adapters::VendorTurnId::as_str);
                if let Attribution::Late(turn) = self.attribute(vendor_turn, None)
                    && let Some(engine) = self.engine.upgrade()
                {
                    engine.revise(&self.writer.session, turn, terminal).await;
                }
            }
            Observation::IdentityConfirmed(_)
            | Observation::Accepted(_)
            | Observation::Progress(_)
            | Observation::FinalText(_)
            | Observation::VendorClosed(_)
            | Observation::ResumeMismatch { .. } => {}
        }
        drop(permit);
    }

    /// The lane's actor (C2 §2; Sol r1 F2, F4, Sol r2 #1-#3, Sol r3
    /// N1-N5, Sol r4 R2, R3, Sol r5 R8), on the daemon's tracker for the
    /// lane's life: it serves the lane until it ends ([`Self::serve`]).
    /// Then it closes the driver, draining the channel meanwhile
    /// ([`Self::close_draining`]), unless the drivers' cancellation ended
    /// the lane; closes the channel's admission and disposes of everything
    /// admitted before, to the channel's end, one item at a time (durable
    /// items committed, the rest dropped); runs a turn handed over
    /// meanwhile, with no channel; marks the lane ended; only then removes
    /// it from `lanes`, when its session closed or at the drivers'
    /// cancellation; and publishes the lane's end. That one completion serves every
    /// waiter: close, retirement, replacement and final shutdown. Nothing
    /// cancels the actor but the runtime's own end.
    async fn actor(
        self: Arc<Self>,
        mut inbox: Inbox,
        (lanes, session): (Lanes, SessionId),
        resident: OwnedSemaphorePermit,
    ) {
        let ending = self.serve(&mut inbox).await;
        // A C1 close owns the report of the driver close it asked for or
        // took over before it started (C1 §3.6; C2 §3 idle lanes, §4.2).
        // A retirement's or an idle-lane close's has no destination, even
        // when a C1 close joined it after it started: `leftovers` is null.
        let owned = matches!(ending, Some(Ending::Close(..)));
        let close = match ending {
            Some(Ending::Retire) => Some((
                CloseMode::Force,
                Deadline::at(tokio::time::Instant::now() + REPLACE_CLOSE),
            )),
            Some(Ending::Close(mode, deadline)) => Some((mode, deadline)),
            Some(Ending::Evict(deadline)) => Some((CloseMode::Graceful, deadline)),
            // The drivers' cancellation ends their work; Host
            // reconciliation owns their groups (final shutdown).
            None => None,
        };
        let mut disposing = None;
        if let Some((mode, deadline)) = close {
            let (report, current) = self.close_draining(&mut inbox, mode, deadline).await;
            disposing = current;
            if owned {
                *lock(&self.report) = Some(report);
            }
        }
        // Test builds: the actor holds before the channel's admission closes.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.lane.admission_close").await;
        // Admission closes first (Sol r4 R3): what the driver sends from now
        // on is refused at its sink, and an empty channel is not its end
        // until no sender holds a slot. A disposal the close's end found
        // under way is then finished, never cut off (fix r3 #2).
        inbox.close();
        // Test builds: admission is closed, a disposal under way unfinished.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.lane.admission_closed").await;
        if let Some(disposing) = disposing {
            disposing.await;
        }
        let mut handled = 0;
        while let Some(admitted) = inbox.recv().await {
            self.dispose(admitted).await;
            ready_item(&mut handled).await;
        }
        drop(inbox);
        // The driver's close may have retired its connection.
        self.journal_read(&mut self.driver.journal_uncertain())
            .await;
        // A turn handed over meanwhile (only at the drivers' cancellation:
        // an ending lane is never claimed) runs to its end before the
        // lane's end is published (Sol r4 R2); a later handover finds the
        // lane ended under the same lock.
        let removed = loop {
            let job = {
                let mut core = lock(&self.core);
                let job = core.job.take();
                if job.is_none() {
                    core.life = Life::Ended;
                }
                job.ok_or(core.removed)
            };
            match job {
                Ok(job) => job(&mut Inbox::closed()).await,
                Err(removed) => break removed,
            }
        };
        // Critical r2 #2: the channel is drained to its end; a steer
        // request still waiting learns its report never committed.
        self.steers.close();
        // Only an ended lane leaves the session's registration (Sol r5 R8),
        // when its session closed or the daemon's drivers were cancelled;
        // a retired one stays for its successor's state.
        if removed || self.cancel.is_cancelled() {
            let mut lanes = lock(&lanes);
            if lanes
                .get(&session)
                .is_some_and(|kept| std::ptr::eq(Arc::as_ptr(kept), Arc::as_ptr(&self)))
            {
                lanes.remove(&session);
            }
        }
        // The lane has ended, its drain done: its resident lane is free
        // (runtime §8, critical r3 #3).
        drop(resident);
        self.changed.send_replace(());
    }

    /// Closes the driver while disposing of what its channel receives
    /// meanwhile, one item at a time (fix r2 #1): what the driver still
    /// delivers before its close ends, such as a retired process's late
    /// observations, waits for no close. The close is polled apart from
    /// the disposal under way (fix r3 #2), so its absolute `deadline` (C1
    /// §3.6), which bounds the driver's close and its delivery, never
    /// waits behind Store work. Returns the close's report as soon as it
    /// ends, with the disposal then under way, which the caller finishes
    /// once the channel's admission closed: a commit is never cut off.
    async fn close_draining(
        &self,
        inbox: &mut Inbox,
        mode: CloseMode,
        deadline: Deadline,
    ) -> (CloseReport, Option<Disposal<'_>>) {
        // Test builds: the lane starts the driver's close.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.lane.close_draining").await;
        let mut closing = std::pin::pin!(self.driver.close(mode, deadline));
        let mut current: Option<Disposal<'_>> = None;
        let (mut open, mut handled) = (true, 0);
        loop {
            tokio::select! {
                biased;
                report = &mut closing => return (report, current),
                () = disposal(current.as_mut()), if current.is_some() => {
                    current = None;
                    ready_item(&mut handled).await;
                }
                admitted = inbox.recv(), if open && current.is_none() => match admitted {
                    Some(admitted) => current = Some(Box::pin(self.dispose(admitted))),
                    None => open = false,
                },
            }
        }
    }

    /// Serves the lane until it ends (Sol r3 N1-N3): runs each turn handed
    /// over to its end and, between turns, disposes of each item the
    /// channel receives, one at a time ([`Self::dispose`]). It is the
    /// driver's independent health consumer: it keeps the first failure,
    /// and a failed or overflowed lane ends once no turn holds its claim,
    /// without waiting for an observation or a dispatch (C2 §2 health; an
    /// abandoned turn's claim is gone). Returns how the lane ends; `None`
    /// at the drivers' cancellation (final shutdown). A turn or an item
    /// being handled is finished first: it is never cut off.
    async fn serve(&self, inbox: &mut Inbox) -> Option<Ending> {
        let mut health = self.driver.health();
        let mut changes = self.changed.subscribe();
        let (mut open, mut watched) = (true, true);
        // Each item is taken only after the job, the cancellation, the
        // health and the lane's end were checked again (runtime §8).
        let mut handled = 0;
        loop {
            changes.borrow_and_update();
            let job = lock(&self.core).job.take();
            if let Some(job) = job {
                job(inbox).await;
                continue;
            }
            if self.cancel.is_cancelled() {
                return None;
            }
            let failed = self.health_read(&mut health);
            // Test builds: the actor holds between its health read and the
            // lane's end (Sol r2 #1), until a dispatch asks for that end or
            // the drivers are cancelled.
            #[cfg(feature = "test-failpoints")]
            if failed {
                tokio::select! {
                    _held = via_store::failpoint::hit_async("core.lane.retire") => {}
                    _asked = changes.changed() => {}
                    () = self.cancel.cancelled() => return None,
                }
            }
            {
                let mut core = lock(&self.core);
                if !core.claimed && core.job.is_none() {
                    if failed && core.life == Life::Open {
                        core.life = Life::Ending;
                        core.ending = Some(Ending::Retire);
                    }
                    if let Some(ending) = core.ending {
                        // The driver close starts with this ending: a C1
                        // close from now on waits for it (critical r2 F6).
                        core.life = Life::Closing;
                        return Some(ending);
                    }
                }
            }
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => return None,
                moved = health.changed(), if watched => watched = moved.is_ok(),
                _bumped = changes.changed() => {}
                admitted = inbox.recv(), if open => match admitted {
                    Some(admitted) => {
                        self.dispose(admitted).await;
                        // Drained: the lane may be idle now (runtime §8).
                        if self.idle() {
                            self.bound_idle();
                        }
                        ready_item(&mut handled).await;
                    }
                    None => open = false,
                },
            }
        }
    }

    /// Reads the driver's journal report once its close is done (C2 §2):
    /// an uncertain Host journal write no turn reports latches Store
    /// failure (critical r1 #4, runtime §7), its phase one published before
    /// the lane's end ([`SessionWriter::journal_uncertain`]). The report's
    /// own consumer reads it as it comes ([`journal_watch`]).
    async fn journal_read(&self, journal: &mut watch::Receiver<bool>) {
        if *journal.borrow_and_update() {
            self.writer.journal_uncertain(&self.journal_reported).await;
        }
    }

    /// Reads the driver's health and keeps its first failure: whether the
    /// lane failed, by its health, its driver's close or its vendor turns
    /// (overflow, or a collision).
    fn health_read(&self, health: &mut watch::Receiver<DriverHealth>) -> bool {
        match &*health.borrow_and_update() {
            DriverHealth::Open => {
                let mut state = lock(&self.state);
                if state.overflowed {
                    state
                        .first_cause
                        .get_or_insert(DriverFailure::ObservationOverflow);
                }
                state.overflowed || state.collided
            }
            DriverHealth::Failed { first_cause } => {
                lock(&self.state)
                    .first_cause
                    .get_or_insert_with(|| first_cause.clone());
                true
            }
            DriverHealth::Closed => true,
        }
    }

    /// Records `vendor_turn` as `turn`'s, the newest ([`LaneState::map`]):
    /// [`Mapped::Collided`] for one the lane maps to another turn or
    /// tombstoned, which fails the lane (Sol r3 N6), and
    /// [`Mapped::Exhausted`] for an overflow (critical r1 #6). Either wakes
    /// the lane's actor, which retires the lane once its turn ends.
    pub(super) fn map_vendor_turn(&self, vendor_turn: &str, turn: TurnNumber) -> Mapped {
        let (mapped, failed) = {
            let mut state = lock(&self.state);
            let before = state.overflowed || state.collided;
            let mapped = state.map(vendor_turn, turn);
            state.collided |= mapped == Mapped::Collided;
            (mapped, (state.overflowed || state.collided) && !before)
        };
        if failed {
            self.changed.send_replace(());
        }
        mapped
    }

    /// The driver opens a new connection generation (C2 §2): vendor turn
    /// ownership is scoped to it ([`LaneState::map`]). The caller disposed
    /// of what the channel held from the older one first.
    pub(super) fn new_generation(&self) {
        lock(&self.state).generation += 1;
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

    /// Whether this daemon committed the confirmed identity of the driver's
    /// current connection generation (C1 §3.7 `vendor_identity_verified`,
    /// decision H3, Sol r2 #4): false from the start of a reopen until that
    /// generation's own confirmation commits, so a refusal before it
    /// leaves it false.
    pub(super) fn verified(&self) -> bool {
        let current = self.driver.connection_id();
        current.is_some() && lock(&self.state).committed == current
    }

    /// Whether `connection_id` names the driver's current connection
    /// generation (C2 §2 delayed identity: Core checks the current
    /// generation).
    pub(super) fn current(&self, connection_id: &str) -> bool {
        self.driver.connection_id().as_deref() == Some(connection_id)
    }

    /// Whether `confirmed`, confirmed again by the generation that
    /// committed the open event, differs from the identity the lane knows:
    /// its columns are written again, with no event.
    pub(super) fn changes(&self, confirmed: &Identity) -> bool {
        lock(&self.state).identity.as_ref().is_none_or(|known| {
            known.vendor_session_id != confirmed.vendor_session_id
                || known.transcript != confirmed.transcript
        })
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
    /// one per code, as its envelope's ([`Warning::adapter`]); and Core's
    /// `predecessor_cleanup_uncertain`, set at dispatch (C1 §7.3).
    pub(super) warnings: Vec<Warning>,
    /// The handshake version of the instance that ran the turn, and
    /// whether the adapter checked it (AD7).
    pub(super) instance: Option<(Option<String>, bool)>,
    /// The version of the adapter that ran the turn (AD12), when one did.
    pub(super) adapter_version: Option<String>,
    /// The retained vendor terminal's envelope facts (AD4).
    pub(super) retained: Option<Retained>,
    /// The turn's acceptance found every tombstone taken (C2 §4.1,
    /// critical r1 #6): the lane overflowed, and the turn fails `overflow`.
    pub(super) overflowed: bool,
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
    /// Why the structured output does not satisfy the turn's frozen
    /// `output_schema` (`"invalid"` or `"validation_limit"`), once checked
    /// before it is stored: the validation outcome, kept apart from the
    /// turn's failure class until its terminal is built (critical r1 #4).
    pub(super) output_invalid: Option<&'static str>,
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
            // A route's output that is no value is invalid as it stands
            // (C2 §2 `NotJson`, `OverLimit`); a value is checked later.
            output_invalid: terminal
                .structured_output_unparsed
                .map(via_adapters::UnparsedOutput::reason),
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
    /// whose driver failed is retired first, so its harness-process slot is
    /// free before the turn reserves one, and the turn opens a successor
    /// ([`Self::open_lane`]). Retirement is shared: whoever asked for it,
    /// this waits for its end.
    pub(super) async fn claim_lane(&self, session: &SessionId) -> Option<LaneClaim> {
        let lane = lock(&self.lanes).get(session).cloned()?;
        if let Some(claim) = lane.claim() {
            return Some(claim);
        }
        if lane.begin_retire() {
            // Test builds: the dispatch waits for its lane's end.
            #[cfg(feature = "test-failpoints")]
            let _ = via_store::failpoint::hit_async("core.lane.claim_wait").await;
            lane.retired().await;
        }
        None
    }

    /// The session's lane for its submitted turn, which found none to
    /// claim, opened for the turn's frozen values and the session's frozen
    /// `inherit` in `cwd` (C2 §2 `open_session`, logical: no vendor I/O;
    /// critical r1 #2) from the session's stored
    /// route identity `route` (Sol r1 F12, decision H3), and claimed. A
    /// route identity the Store does not hold is not invented: the driver
    /// then has no adapter and refuses the turn. A lane it replaces stays
    /// the session's until its actor ended it (Sol r2 #3, #9; Sol r3 N4),
    /// whether or not this caller still waits: its driver closed and what
    /// its channel had committed or dropped. Only then is the successor
    /// made, keeping its identity and the session's byte budget.
    pub(super) async fn open_lane(
        &self,
        session: &SessionId,
        ((route, effective, inherit), cwd): ((&SessionRoute, &Effective, InheritPlan), PathBuf),
        resident: OwnedSemaphorePermit,
    ) -> LaneClaim {
        let replaced = lock(&self.lanes).get(session).cloned();
        let (state, budget) = match replaced {
            Some(lane) => {
                lane.retire_now().await;
                let state = lock(&lane.state).successor();
                (state, lane.budget.clone())
            }
            None => (LaneState::recovered(route), ObservationBudget::new()),
        };
        let reference = session_ref(route);
        // Decision F12: what the session's plan froze at spawn.
        let frozen = Frozen::of(route);
        let spec = SessionSpec {
            session_id: session.clone(),
            model: effective.model().to_owned(),
            instructions: frozen.instructions,
            initial_bound: effective.bound().cloned(),
            cwd,
            vendor: frozen.vendor,
            inherit,
            confirmed_vendor_session_id: state
                .identity
                .as_ref()
                .map(|identity: &Identity| identity.vendor_session_id.clone()),
            allow_untested: frozen.allow_untested,
        };
        let (sink, receiver) = observation_channel_in(&budget);
        let cx = SessionCx {
            observations: sink,
            tracker: self.tracker.clone(),
            cancel: self.cancel.child_token(),
        };
        let driver = self.adapter.open_session(&reference, spec, cx);
        #[cfg(test)]
        self.faults.lanes_opened.fetch_add(1, Ordering::AcqRel);
        let lane = self.install_lane(
            session,
            ((driver, reference), (receiver, budget)),
            (state, true),
            resident,
        );
        LaneClaim(lane)
    }

    /// Restart recovery's resumed driver (C2 §2 Recover) becomes the
    /// session's lane, with the channel its recovery was given on the
    /// session's `budget`, holding a resident lane (runtime §8). Recovery
    /// adopts at most one lane per session with a turn the earlier daemon
    /// left unfinished, so at most `UNRESOLVED_LIMIT` (256), below
    /// [`RESIDENT_LANES`]; none free fails startup.
    pub(super) fn adopt_lane(
        &self,
        session: &SessionId,
        (driver, receiver, budget): (SessionDriver, mpsc::Receiver<Admitted>, ObservationBudget),
        (reference, route): (SessionRef, &SessionRoute),
    ) -> Result<(), String> {
        let resident = Arc::clone(&self.resident)
            .try_acquire_owned()
            .map_err(|_| format!("no resident lane for recovered session {session}"))?;
        self.install_lane(
            session,
            ((driver, reference), (receiver, budget)),
            (LaneState::recovered(route), false),
            resident,
        );
        self.evict_idle();
        Ok(())
    }

    /// Makes a lane the session's, in place of any it had, and starts its
    /// actor on the daemon's tracker.
    fn install_lane(
        &self,
        session: &SessionId,
        (opened, (receiver, budget)): (
            (SessionDriver, SessionRef),
            (mpsc::Receiver<Admitted>, ObservationBudget),
        ),
        initial: (LaneState, bool),
        resident: OwnedSemaphorePermit,
    ) -> Arc<Lane> {
        let lane = Arc::new(Lane::new(
            opened,
            budget,
            initial,
            (
                self.session_writer(session),
                self.cancel.child_token(),
                Weak::clone(&self.me),
            ),
        ));
        lock(&self.lanes).insert(session.clone(), Arc::clone(&lane));
        self.tracker.spawn(journal_watch(
            lane.driver.journal_uncertain(),
            self.session_writer(session),
            Arc::clone(&lane.journal_reported),
        ));
        self.tracker.spawn(Arc::clone(&lane).actor(
            Inbox(Some(receiver)),
            (Arc::clone(&self.lanes), session.clone()),
            resident,
        ));
        lane
    }

    /// A session's close (C2 §2 Close): its lane's actor closes the driver
    /// by `deadline`, releasing any connection it holds, unless the lane
    /// is already ending, disposes of what the channel still has and
    /// removes the lane, all before `session.closed` (Sol r2 #3); this
    /// waits for that end. A close joins an eviction ([`Lane::begin_close`]):
    /// it returns the report of the driver close it asked for or took over
    /// before it started; `None` for one it joined after it started, which
    /// stays an idle-lane close with no destination (C1 §3.6, C2 §3 idle
    /// lanes, critical r3 #7), and with no lane, as after an eviction that
    /// already ended. Dropping this future, as the close
    /// pass does at the daemon's force, cancels none of it (Sol r3 N4).
    pub(super) async fn close_lane(
        &self,
        session: &SessionId,
        mode: CloseMode,
        deadline: Deadline,
    ) -> Option<CloseReport> {
        let lane = lock(&self.lanes).get(session).cloned()?;
        lane.begin_close(mode, deadline);
        // Test builds: the close is the lane actor's request.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.lane.close_requested").await;
        lane.retired().await;
        let mut lanes = lock(&self.lanes);
        if lanes
            .get(session)
            .is_some_and(|kept| Arc::ptr_eq(kept, &lane))
        {
            lanes.remove(session);
        }
        lock(&lane.report).take()
    }

    /// Keeps the idle lanes within [`IDLE_LANES`] (runtime §8, C2 §3 idle
    /// lanes), whenever a lane may have become idle: a turn released its
    /// claim, a lane was adopted, an actor drained its channel between
    /// turns, or its session's slot became unoccupied (a queued turn
    /// cancelled or failed, a run or a close attempt finished;
    /// `Slot::emptied`). Afterwards exactly the bound's worth of idle lanes, or fewer
    /// when fewer are idle, remain; an evicting lane is no longer idle.
    /// Lanes that ended, such as retired ones, leave the registry: their
    /// channel was drained to its end, and the session's next dispatch
    /// opens a fresh lane from its stored identity. Past the bound, the
    /// least recently used idle lanes are taken through the live-close
    /// barrier ([`Lane::begin_evict`]), as many as exceed the bound, which
    /// their actors carry out; this waits for none of it. A lane whose session has a turn queued or
    /// running, or a close order, is not idle; one claimed, handed a turn
    /// or ending is never taken. A dispatch that finds its lane taken
    /// waits for its end, then opens the successor ([`Self::claim_lane`],
    /// [`Self::open_lane`]): one driver at a time, and no turn lost.
    ///
    /// Capacity pressure (critical r4 #2): each dispatch waiting for a
    /// resident lane ([`Self::press_resident`]) is owed one idle lane's
    /// retirement, within the bound too, less the registered lanes
    /// already ending, whose permits the waiters take in FIFO order. With
    /// none idle, a lane that becomes idle later is taken by this same
    /// check. Racing a waiter that is taking an ended lane's permit, one
    /// more idle lane than needed may be retired; its session reopens on
    /// its next dispatch.
    pub(super) fn evict_idle(&self) {
        // Selection and every eviction under one registry lock (critical
        // r2 F5): concurrent checks neither over- nor under-evict.
        let mut lanes = lock(&self.lanes);
        lanes.retain(|_, lane| !lane.ended());
        let mut idle: Vec<&Arc<Lane>> = {
            let sessions = lock(&self.sessions);
            lanes
                .iter()
                .filter(|(session, lane)| {
                    lane.idle() && sessions.get(*session).is_none_or(|slot| slot.unoccupied())
                })
                .map(|(_, lane)| lane)
                .collect()
        };
        let ending = lanes.values().filter(|lane| lane.life() != Life::Open);
        let owed = self
            .pressed
            .load(Ordering::SeqCst)
            .saturating_sub(ending.count());
        let excess = idle.len().saturating_sub(IDLE_LANES).max(owed);
        if excess == 0 {
            return;
        }
        idle.sort_unstable_by_key(|lane| lane.used.load(Ordering::Relaxed));
        for lane in idle.into_iter().take(excess) {
            lane.begin_evict();
        }
    }

    /// Registers a dispatch waiting for a resident lane, for as long as the
    /// returned guard lives, and has an idle lane retired for it
    /// ([`Self::evict_idle`], critical r4 #2).
    pub(super) fn press_resident(&self) -> Pressed<'_> {
        self.pressed.fetch_add(1, Ordering::SeqCst);
        self.evict_idle();
        Pressed(&self.pressed)
    }

    /// Test builds only: the adapter set's stand-in admission (x.3.2 X0
    /// item 0), installed on first call; drivers opened later take it.
    #[cfg(feature = "test-failpoints")]
    #[must_use]
    pub fn stand_in(&self) -> Arc<via_adapters::StandIn> {
        self.adapter.stand_in()
    }

    /// Test builds: the lanes registered, those of them whose actor has
    /// not ended, and the tasks on the daemon's tracker (each lane's actor
    /// and journal consumer, and its driver's owned tasks).
    #[cfg(feature = "test-failpoints")]
    #[must_use]
    pub fn lane_census(&self) -> (usize, usize, usize) {
        let lanes = lock(&self.lanes);
        let live = lanes.values().filter(|lane| !lane.ended()).count();
        (lanes.len(), live, self.tracker.len())
    }

    /// Whether `session`'s lane drain is complete (design §6.8 step 3, Sol
    /// r4 R4, Sol r5 R8): no lane of it that has not ended is registered,
    /// read under the registry's lock. A live lane stays registered until
    /// its actor ended, so a session is never closed before then: its lane
    /// may still commit what its channel had.
    pub(super) fn lane_drained(&self, session: &SessionId) -> bool {
        lock(&self.lanes)
            .get(session)
            .is_none_or(|lane| lane.ended())
    }

    /// Final shutdown: every driver's owned work is cancelled, before Host
    /// reconciliation. Each lane's actor finishes what it is doing and
    /// disposes of what its channel still has (C2 §2 session drain; Sol r2
    /// #3), then leaves the registry; this waits for them by `by` and
    /// within [`SHUTDOWN_DRAIN`], after letting each see the cancellation
    /// once, and lets go of the lanes that ended. Returns how many actors
    /// have not ended by then (Sol r3 N5): each still owns its work, which
    /// final shutdown reports as pending and leaves to it, and its lane
    /// stays registered, so its session stays open (Sol r5 R8).
    pub(super) async fn drop_lanes(&self, by: tokio::time::Instant) -> usize {
        self.cancel.cancel();
        self.tracker.close();
        let lanes: Vec<Arc<Lane>> = lock(&self.lanes).values().cloned().collect();
        let by = by.min(tokio::time::Instant::now() + SHUTDOWN_DRAIN);
        // An actor with nothing left ends at its next poll.
        tokio::task::yield_now().await;
        // Test builds: final shutdown waits for the lanes' actors.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.lane.drain_wait").await;
        let mut undrained = 0;
        for lane in lanes {
            // A lane that ended is ready at its first poll, even past `by`.
            if tokio::time::timeout_at(by, lane.retired()).await.is_err() {
                undrained += 1;
            }
        }
        lock(&self.lanes).retain(|_, lane| !lane.ended());
        undrained
    }
}

#[cfg(test)]
mod tests {
    use super::{Attribution, LaneState, Mapped, TOMBSTONES, VENDOR_TURNS};
    use crate::TurnNumber;

    fn turn(number: u32) -> TurnNumber {
        TurnNumber::try_from(number).expect("a turn number")
    }

    /// Sol r1 F6, Sol r2 #5 (C2 §2 observations): a mapped vendor turn is
    /// the running turn's or an earlier turn's (late); an unfamiliar one is
    /// session-level even before the running turn's acceptance (only the
    /// acceptance's correlation makes an explicit ID current), and an item
    /// naming none is the running turn's; one whose mapping expired past
    /// the bound is tombstoned.
    #[test]
    fn vendor_turns_attribute_current_late_session_or_expired() {
        let mut state = LaneState::default();
        // Before turn 1's acceptance maps its vendor turn, an unfamiliar
        // ID is the session's; an item naming none is the running turn's.
        assert_eq!(
            state.attribute(Some("v1"), Some(turn(1))),
            Attribution::Session
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
        // `v1` expires after the bound's worth of later vendor turns.
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
        assert!(!state.overflowed);
        // A replacing lane's driver has new connection generations, which
        // no earlier ID can reach (C2 §2): it keeps the identity only.
        let successor = state.successor();
        assert!(successor.turns.is_empty() && successor.tombstones.is_empty());
        assert_eq!(successor.opened, state.opened);
    }

    /// Sol r3 N6 (C2 §2, §4.1): mapping establishes ownership only for a
    /// genuinely unseen vendor turn ID. One already mapped to another turn
    /// keeps that turn, and a tombstoned one stays expired: no new mapping
    /// overrides either.
    #[test]
    fn a_mapped_or_tombstoned_vendor_turn_is_never_taken_again() {
        let mut state = LaneState::default();
        assert_eq!(state.map("v1", turn(1)), Mapped::Taken);
        assert_eq!(
            state.map("v1", turn(1)),
            Mapped::Taken,
            "the same turn's again"
        );
        assert_eq!(
            state.map("v1", turn(2)),
            Mapped::Collided,
            "another turn's is refused"
        );
        assert_eq!(
            state.attribute(Some("v1"), Some(turn(2))),
            Attribution::Late(turn(1))
        );
        for number in 2..=u32::try_from(VENDOR_TURNS).expect("a small bound") + 1 {
            let _ = state.map(&format!("v{number}"), turn(number));
        }
        assert_eq!(state.attribute(Some("v1"), None), Attribution::Expired);
        assert_eq!(
            state.map("v1", turn(70)),
            Mapped::Collided,
            "a tombstoned one is refused"
        );
        assert_eq!(
            state.attribute(Some("v1"), Some(turn(70))),
            Attribution::Expired,
            "a tombstone is never reassigned"
        );
    }

    /// Sol r2 #5 (C2 §4.1: retained tombstones cannot be reassigned): the
    /// tombstones' bound is never made room in by forgetting an accepted
    /// ID. The vendor turn that would need one more overflows the lane
    /// instead, and every earlier ID keeps its attribution.
    #[test]
    fn tombstone_exhaustion_overflows_instead_of_forgetting() {
        let mut state = LaneState::default();
        let bound = u32::try_from(VENDOR_TURNS + TOMBSTONES).expect("a small bound");
        for number in 1..=bound {
            state.map(&format!("v{number}"), turn(number));
        }
        assert!(!state.overflowed);
        assert_eq!(state.attribute(Some("v1"), None), Attribution::Expired);
        assert_eq!(
            state.map(&format!("v{}", bound + 1), turn(bound + 1)),
            Mapped::Exhausted
        );
        assert!(state.overflowed, "the bound overflows the lane");
        assert_eq!(state.attribute(Some("v1"), None), Attribution::Expired);
        assert_eq!(state.attribute(Some("v1024"), None), Attribution::Expired);
        assert_eq!(
            state.attribute(Some("v1025"), None),
            Attribution::Late(turn(1025))
        );
        assert_eq!(
            state.attribute(Some(&format!("v{}", bound + 1)), None),
            Attribution::Late(turn(bound + 1))
        );
    }

    /// Critical r1 #5 (C2 §2: ownership is per connection generation): an
    /// ID an older generation mapped or tombstoned is a new generation's to
    /// take, and older generations' tombstones never count toward the new
    /// one's exhaustion. Until a new generation takes an older ID, its late
    /// traffic keeps its attribution (AD4).
    #[test]
    fn ownership_is_per_connection_generation() {
        let mut state = LaneState::default();
        let bound = u32::try_from(VENDOR_TURNS + TOMBSTONES).expect("a small bound");
        for number in 1..=bound {
            state.map(&format!("v{number}"), turn(number));
        }
        state.generation += 1;
        assert_eq!(
            state.attribute(Some(&format!("v{bound}")), None),
            Attribution::Late(turn(bound)),
            "an older generation's late traffic keeps its turn"
        );
        let next = bound + 1;
        assert_eq!(
            state.map(&format!("v{next}"), turn(next)),
            Mapped::Taken,
            "older generations' tombstones do not exhaust the new one"
        );
        assert!(!state.overflowed);
        assert_eq!(
            state.map("v1", turn(next + 1)),
            Mapped::Taken,
            "a tombstoned ID"
        );
        assert_eq!(
            state.map(&format!("v{bound}"), turn(next + 2)),
            Mapped::Taken,
            "a mapped ID"
        );
        assert_eq!(
            state.attribute(Some("v1"), Some(turn(next + 1))),
            Attribution::Current
        );
        assert_eq!(
            state.map("v1", turn(next + 3)),
            Mapped::Collided,
            "the new generation's own ownership still holds"
        );
    }
}
