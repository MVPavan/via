//! `codex::Connection` (vendors/codex.md §2, §4, §5; x.3.2 X0 items 5, 8,
//! 9, 11, 12, 13): one shared server's typed connection. One connection
//! task ([`serve`]) owns the unique message receiver: it peeks each
//! message's correlation fields, pairs every reply with its request record
//! by ID (IDs are connection-local, monotonic and never reused), routes
//! each notification raw into its registration's ingress lane, or drops and
//! counts it for a closed one before any full decode, and answers every
//! server request with its no-grant body or `-32601` under the exact
//! incoming ID on the control path within 5 s. The task also runs the
//! [`Feeder`], Wire's only producer, which every write goes through.
//! Drivers queue their requests with [`Connection::request`]; the
//! connection never waits on a driver.
//!
//! A whole-connection failure runs one owned sequence: a first-wins latch
//! that seals Wire's admission, Host's stop beside the drain of the
//! admitted prefix into the lanes, then the fan-out of the disposition to
//! every lane and waiter (item 13.1). When the task itself fails, the
//! registry runs [`Connection::abnormal`] instead (item 13.2).

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{Notify, oneshot, watch};
use tokio::time::Instant;
use via_wire::{
    Admitted, CloseMode, CloseRequest, CommitOutcome, Deadline, OutboundMessage, SendOutcome,
    ServerId, SessionId, TurnNumber, VendorMessage, WireCleanup, WireCloseReport, WireError,
    WireFailure, WireMessages, WireSender, WriteBounds,
};

use super::feeder::{Answer, Done, Feeder, Item, PumpEnd, Queue, StartMarker, WriteCancel};
use super::lane::{ConnectionLoss, Lane, LaneEnd, LaneItem, LeaseSignal, LossCause, Routed};
use super::stdio::Stdio;
use super::threads::{Budget, Route, ThreadTable};
use super::{
    ClientId, ClientIds, DeclineTable, EncodeError, Incoming, RequestId, Response, Routing,
    ThreadResult, TurnStartResult, decode, peek, result, thread_unsubscribe, turn_interrupt,
};
use crate::DecodeWatermark;

/// C2 A6: a server request is answered within this of its decode.
pub const DECLINE_DEADLINE: Duration = Duration::from_secs(5);

/// The most server-request replies pending at once, and their bytes
/// (packet §4); one more fails the connection `overflow`.
const REPLIES_MAX: usize = 8;
const REPLY_BYTES_MAX: usize = 64 * 1024;

/// How long a failed connection's evidence is waited for (item 13.1).
pub const LOSS_EVIDENCE: Duration = Duration::from_secs(5);

/// The far bound by which a started data message is written whole: the
/// connection's own, never a turn's (item 12.2).
pub const FINISH_BY: Duration = Duration::from_secs(3600);

/// The most bytes of an unattributable message kept as evidence.
const EVIDENCE_BYTES: usize = 64 * 1024;

/// Why a whole connection failed; the first one detected wins (item 13.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionFailure {
    /// Host reported the server's exit.
    Exited,
    /// A writer error, the end of stdout or a read error.
    Transport {
        /// Stdout ended, or the writer saw `EPIPE`.
        stdio_end: bool,
    },
    /// An unattributable decode failure (item 5 step 2).
    Protocol,
    /// Staging, correlation or reply-bound exhaustion, or an unanswered
    /// decline.
    Overflow,
    /// The connection task itself failed (item 13.2).
    Internal,
}

/// How the connection task ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionEnd {
    /// The server retired: stdout ended after its idle retirement began.
    Retired,
    /// The connection failed and its owned sequence ran.
    Failed(ConnectionLoss),
}

/// Why a request was not queued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestError {
    /// The connection failed or retired: nothing is admitted.
    Closed,
    /// The request IDs or the correlation budget are exhausted: the
    /// connection fails `overflow`.
    Exhausted,
    /// The request could not be encoded.
    Encode(EncodeError),
}

/// What a request's reply means to the connection when it pairs.
pub enum Purpose<'a> {
    /// Nothing beyond its waiter.
    Plain,
    /// A `thread/start` or `thread/resume`: its reply registers `lane`
    /// under the returned thread, but only while its waiter still waits
    /// (item 9.1); otherwise the thread is kept as closed. A resume
    /// carries its thread's [`Reservation`], which the record takes over
    /// as it is queued (x.3.2 X4 D3): the thread stays fenced until the
    /// reply, a positive `NotWritten` or the connection's end, whether or
    /// not its waiter still waits.
    Opens {
        /// The lane the reply registers.
        lane: &'a LaneLease,
        /// A resume's reservation of its thread.
        reservation: Option<Reservation>,
    },
    /// A `thread/unsubscribe` of `thread`: its record fences the thread
    /// until its reply, a positive `NotWritten` or the connection's end
    /// (x.3.2 X4 D3).
    Unsubscribes {
        /// The thread unsubscribed.
        thread: String,
    },
    /// A `turn/start` on `lane`'s thread: its accepted turn is mapped to
    /// VIA turn `turn` until the connection retires (packet §5). As it is
    /// handed to Wire its `Start` marker, carrying `cx`, fences the lane
    /// for the turn's watermark `decoded` (x.3.2 X3 §2.1); its reply's
    /// `Reply` marker follows in decode order.
    Starts {
        /// The registration.
        lane: &'a LaneLease,
        /// The VIA turn.
        turn: TurnNumber,
        /// The turn's decode watermark.
        decoded: DecodeWatermark,
        /// The driver's context for the turn, opaque to the route.
        cx: Box<dyn Any + Send + Sync>,
    },
}

/// A request queued on the connection: its ID, its write's outcome, and
/// the wait for its paired reply. Dropping the reply wait abandons the
/// record: a reply still pairs, and is counted and dropped.
pub struct Requested {
    /// The request's ID.
    pub id: ClientId,
    /// How Wire answered the write; closed when the connection ended
    /// before it did (an indeterminate write).
    pub written: oneshot::Receiver<SendOutcome>,
    /// The paired reply; closed when the connection ends first, or the
    /// write was never made.
    pub reply: oneshot::Receiver<Response>,
}

/// How a record's reply is paired.
enum Pairing {
    Plain,
    Opens {
        lane: u64,
        /// A resume's thread, fenced while the record lives (x.3.2 X4 D3).
        reserved: Option<String>,
    },
    /// A `thread/unsubscribe` of `thread`, server-owned: it fences the
    /// thread while it lives, outliving its lane (x.3.2 X4 D3).
    Unsubscribes {
        thread: String,
    },
    Starts {
        lane: u64,
        thread: String,
        turn: TurnNumber,
        /// An interrupt intent posted before the turn's ID was known
        /// (item 8.3): written once the reply names the turn.
        interrupt: Option<Deadline>,
    },
}

/// One client request record (item 9.1), kept until its reply, its write
/// was never made, or the connection's end; its waiter may be gone
/// (abandoned).
struct Record {
    waiter: Option<oneshot::Sender<Response>>,
    pairing: Pairing,
    /// The ID bytes charged against the correlation budget.
    charge: usize,
}

impl Pairing {
    /// The thread the record fences (x.3.2 X4 D3): a resume's or an
    /// unsubscribe's.
    fn fences(&self) -> Option<&str> {
        match self {
            Self::Opens {
                reserved: Some(thread),
                ..
            }
            | Self::Unsubscribes { thread } => Some(thread),
            Self::Plain | Self::Opens { reserved: None, .. } | Self::Starts { .. } => None,
        }
    }
}

/// What the connection keeps, under one std mutex never held across an
/// await.
struct State {
    ids: ClientIds,
    requests: HashMap<i64, Record>,
    threads: ThreadTable,
    budget: Budget,
    /// Each reserved thread and its reservation's ID (x.3.2 X4 D3),
    /// charged to the budget.
    reserved: HashMap<String, u64>,
    next_reservation: u64,
    /// The connection ended: no request or registration is admitted.
    ended: bool,
    /// The first unattributable message's bytes, for the server folder.
    evidence: Option<Vec<u8>>,
    counts: Counts,
    /// The decode sequence of the last admitted message.
    seq: u64,
    /// Every lease's abnormal-end signal (item 13.2).
    signals: HashMap<u64, Arc<LeaseSignal>>,
    next_signal: u64,
    /// Replies queued or being written, and their bytes.
    replies: (usize, usize),
    /// Messages fully routed: the unit tests' observable gate.
    #[cfg(test)]
    routed: u64,
}

/// The connection's diagnostic counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    /// Replies paired after their waiter was gone.
    pub abandoned: u64,
    /// Well-formed messages for a thread never registered.
    pub unknown_thread: u64,
    /// Messages for a closed registration, dropped before decoding.
    pub late_after_close: u64,
    /// Untagged connection-scoped traffic.
    pub untagged: u64,
    /// Later failure causes after the first was latched.
    pub later_failures: u64,
}

/// One shared server's connection: the control side the drivers and the
/// registry hold; the message side lives in [`serve`].
pub struct Connection {
    server: ServerId,
    stdio: Arc<dyn Stdio>,
    feeder: Feeder,
    declines: DeclineTable,
    state: Mutex<State>,
    /// The first failure, latched once (item 13.1).
    failure: watch::Sender<Option<ConnectionFailure>>,
    /// How the connection ended, once it did.
    end: watch::Sender<Option<ConnectionEnd>>,
    /// The idle retirement began: the end of stdout is not a failure.
    retiring: AtomicBool,
    /// The daemon is shutting down: Host's stop ending the transport is
    /// expected, so it writes no `via.log` line.
    shutting_down: AtomicBool,
    /// A driver posted its close (x.3.2 X3 §5.1): the connection task
    /// applies it between two routing operations.
    closes: Notify,
    /// Bumped after a term of a thread's fence cleared (x.3.2 X4 D3): a
    /// reservation released, a fencing record gone, a lane closed, the
    /// connection's end.
    epoch: watch::Sender<u64>,
}

/// Why a thread could not be reserved (x.3.2 X4 D3).
#[derive(Debug)]
pub enum Fenced {
    /// A reservation, a resume, an unsubscribe or an open registration of
    /// the thread is outstanding: reserve again once the epoch changes.
    Busy(watch::Receiver<u64>),
    /// The connection failed or ended: nothing is admitted.
    Ended,
}

/// One thread reserved on a connection (x.3.2 X4 D3): no other resume of
/// it is admitted until the reservation drops, or until the resume record
/// it is handed to resolves. Dropped untransferred, it releases the
/// thread.
pub struct Reservation {
    connection: Arc<Connection>,
    thread: String,
    id: u64,
    /// Taken over by a request record: the drop releases nothing.
    transferred: bool,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("thread", &self.thread)
            .field("id", &self.id)
            .field("transferred", &self.transferred)
            .finish_non_exhaustive()
    }
}

impl Reservation {
    /// The reserved thread.
    pub fn thread(&self) -> &str {
        &self.thread
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.transferred {
            return;
        }
        let released = {
            let mut state = self.connection.state();
            let held = state.reserved.get(&self.thread) == Some(&self.id);
            if held {
                state.reserved.remove(&self.thread);
                state.budget.release(self.thread.len());
            }
            held
        };
        if released {
            self.connection.bump();
        }
    }
}

/// A lane a driver opened on the connection: a registration once a
/// thread open's reply names its thread. Dropping it closes the
/// registration: the thread's later traffic is late (item 8.1).
pub struct LaneLease {
    connection: Arc<Connection>,
    id: u64,
    lane: Arc<Lane>,
}

impl LaneLease {
    /// The lane.
    pub fn lane(&self) -> &Arc<Lane> {
        &self.lane
    }

    /// The thread the lane is registered for, once it is.
    pub fn thread(&self) -> Option<String> {
        self.connection
            .state()
            .threads
            .thread(self.id)
            .map(str::to_owned)
    }

    /// The connection.
    pub fn connection(&self) -> &Arc<Connection> {
        &self.connection
    }
}

impl Drop for LaneLease {
    fn drop(&mut self) {
        let unregistered = self.connection.state().threads.close_lane(self.id);
        if unregistered {
            self.connection.bump();
        }
    }
}

/// A lease's abnormal-end subscription; dropping it unsubscribes.
pub struct Subscription {
    connection: Arc<Connection>,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.connection.state().signals.remove(&self.id);
    }
}

/// The owning guard of one turn's input writes (item 12.2), living in its
/// `run_turn`: it holds the turn's write-cancel token, which each of its
/// writes carries from its queueing (x.3.2 X3 §2.2). Dropped on return, on
/// the future's drop and on unwinding, it cancels: the feeder hands none of
/// the turn's queued items to Wire, and the ones handed are withdrawn
/// before their first byte; a started one is finished whole. Cleanup
/// intents are not in it.
pub struct TurnWrites {
    cancel: Arc<WriteCancel>,
}

impl TurnWrites {
    /// A guard for one turn's writes on `connection`.
    pub fn new(connection: &Arc<Connection>) -> Self {
        Self {
            cancel: Arc::new(WriteCancel::new(Arc::clone(&connection.stdio))),
        }
    }

    /// A handle that cancels the turn's writes from outside its run.
    pub fn canceller(&self) -> WriteCanceller {
        WriteCanceller(Arc::clone(&self.cancel))
    }
}

/// Cancels one turn's writes as its [`TurnWrites`] drop would, from
/// outside its run: a generation's failure stops an admitted turn's start
/// this way, before any await (x.3.2 X3 §4.2 step 4).
#[derive(Clone)]
pub struct WriteCanceller(Arc<WriteCancel>);

impl WriteCanceller {
    /// Cancels the turn's writes, synchronously and idempotently.
    pub fn cancel(&self) {
        self.0.cancel();
    }
}

impl Drop for TurnWrites {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Connection {
    /// The connection of server `server` over `sender`, answering server
    /// requests from `declines`.
    pub fn new(server: ServerId, sender: WireSender, declines: DeclineTable) -> Arc<Self> {
        Self::over(server, Arc::new(sender), declines)
    }

    /// The connection over any [`Stdio`]: test pipes in unit tests.
    pub(crate) fn over(
        server: ServerId,
        stdio: Arc<dyn Stdio>,
        declines: DeclineTable,
    ) -> Arc<Self> {
        Arc::new(Self {
            server,
            feeder: Feeder::new(Arc::clone(&stdio)),
            stdio,
            declines,
            state: Mutex::new(State {
                ids: ClientIds::default(),
                requests: HashMap::new(),
                threads: ThreadTable::default(),
                budget: Budget::default(),
                reserved: HashMap::new(),
                next_reservation: 0,
                ended: false,
                evidence: None,
                counts: Counts::default(),
                seq: 0,
                signals: HashMap::new(),
                next_signal: 0,
                replies: (0, 0),
                #[cfg(test)]
                routed: 0,
            }),
            failure: watch::Sender::new(None),
            end: watch::Sender::new(None),
            retiring: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            closes: Notify::new(),
            epoch: watch::Sender::new(0),
        })
    }

    /// Wakes every fence waiter: a term cleared (x.3.2 X4 D3). Called
    /// after the state's lock is released.
    fn bump(&self) {
        self.epoch
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// Reserves `thread` for a resume (x.3.2 X4 D3), in one step under the
    /// connection's lock: refused while a reservation, a resume record, an
    /// unsubscribe record or an open registration of it is outstanding
    /// (tombstones never fence), with the epoch subscribed under the same
    /// lock so no release is missed. The reservation is charged to the
    /// correlation budget; exhaustion fails the connection `overflow`.
    pub fn reserve(self: &Arc<Self>, thread: &str) -> Result<Reservation, Fenced> {
        let mut state = self.state();
        if state.ended || self.failure().is_some() {
            return Err(Fenced::Ended);
        }
        let fenced = state.reserved.contains_key(thread)
            || state.threads.is_open(thread)
            || state
                .requests
                .values()
                .any(|record| record.pairing.fences() == Some(thread));
        if fenced {
            return Err(Fenced::Busy(self.epoch.subscribe()));
        }
        if !state.budget.charge(thread.len()) {
            drop(state);
            self.fail(ConnectionFailure::Overflow);
            return Err(Fenced::Ended);
        }
        state.next_reservation = state.next_reservation.wrapping_add(1);
        let id = state.next_reservation;
        state.reserved.insert(thread.to_owned(), id);
        Ok(Reservation {
            connection: Arc::clone(self),
            thread: thread.to_owned(),
            id,
            transferred: false,
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // Every edit is a single insert, removal or count: the state stays
        // consistent across a panic elsewhere.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The server this connection talks to.
    pub fn server(&self) -> &ServerId {
        &self.server
    }

    /// The diagnostic counts so far.
    pub fn counts(&self) -> Counts {
        self.state().counts
    }

    /// The first failure latched, if any.
    pub fn failure(&self) -> Option<ConnectionFailure> {
        *self.failure.borrow()
    }

    /// How the connection ended, once it did.
    pub fn ended(&self) -> Option<ConnectionEnd> {
        *self.end.borrow()
    }

    /// Resolves once the connection ended, with how.
    pub async fn end(&self) -> ConnectionEnd {
        let mut end = self.end.subscribe();
        loop {
            if let Some(ended) = *end.borrow_and_update() {
                return ended;
            }
            if end.changed().await.is_err() {
                // The sender lives as long as `self`: unreachable, but
                // answered as a transport loss rather than a hang.
                return ConnectionEnd::Failed(abnormal_loss());
            }
        }
    }

    /// Resolves once a failure is latched or the connection ended: the
    /// start of a running turn's loss deadline (item 13.2).
    pub async fn failing(&self) {
        let mut failure = self.failure.subscribe();
        let mut end = self.end.subscribe();
        loop {
            if failure.borrow_and_update().is_some() || end.borrow_and_update().is_some() {
                return;
            }
            tokio::select! {
                changed = failure.changed() => if changed.is_err() { return },
                changed = end.changed() => if changed.is_err() { return },
            }
        }
    }

    /// Latches `cause` unless a cause is latched already (then it is only
    /// counted), sealing Wire's admission in the same step (item 13.1).
    pub fn fail(&self, cause: ConnectionFailure) {
        self.latch(cause);
    }

    /// [`Self::fail`]: whether `cause` latched, as the first.
    fn latch(&self, cause: ConnectionFailure) -> bool {
        let latched = self.failure.send_if_modified(|failure| {
            if failure.is_some() {
                return false;
            }
            *failure = Some(cause);
            self.stdio.seal();
            true
        });
        if !latched {
            let mut state = self.state();
            state.counts.later_failures = state.counts.later_failures.saturating_add(1);
        }
        latched
    }

    /// Marks the idle retirement begun: from now the end of stdout ends
    /// the connection as retired.
    pub fn retire(&self) {
        self.retiring.store(true, Ordering::Release);
    }

    /// Marks the daemon's shutdown begun (the registry's fence): a lost
    /// transport or server from now on is Host's stop, not news for
    /// `via.log`.
    pub(super) fn shutting_down(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    /// Links a turn to the connection's server (runtime §6).
    pub async fn link_turn(
        &self,
        session: &SessionId,
        turn: TurnNumber,
        deadline: Deadline,
    ) -> CommitOutcome<()> {
        self.stdio.link_turn(session, turn, deadline).await
    }

    /// Keeps `bytes` as the server folder's `undecoded.bin` (item 5).
    pub async fn keep_undecoded(&self, bytes: &[u8], what: &str) {
        self.stdio.keep_undecoded(bytes, what).await;
    }

    pub(super) async fn close_input(&self, deadline: Deadline) -> Result<(), WireError> {
        self.stdio.close_input(deadline).await
    }

    pub(super) async fn close(&self, request: CloseRequest) -> WireCloseReport {
        self.stdio.close(request).await
    }

    /// Queues one request: allocates its ID, records it, charging the
    /// correlation budget, then hands `encode`'s message to the feeder
    /// under `bounds`: data with the turn's input (in `writes`), a control
    /// otherwise. The record exists before the first byte, so a fast reply
    /// always pairs.
    pub fn request(
        &self,
        encode: impl FnOnce(ClientId) -> Result<OutboundMessage, EncodeError>,
        bounds: WriteBounds,
        mut purpose: Purpose<'_>,
        writes: Option<&mut TurnWrites>,
    ) -> Result<Requested, RequestError> {
        // Bound before the lock, so an untransferred reservation drops
        // (and takes the lock) only after it is released.
        let mut reservation = match &mut purpose {
            Purpose::Opens { reservation, .. } => reservation.take(),
            Purpose::Plain | Purpose::Unsubscribes { .. } | Purpose::Starts { .. } => None,
        };
        let (id, message, reply, start) = {
            let mut state = self.state();
            if state.ended || self.failure().is_some() {
                return Err(RequestError::Closed);
            }
            let Some(id) = state.ids.next() else {
                drop(state);
                self.fail(ConnectionFailure::Overflow);
                return Err(RequestError::Exhausted);
            };
            let message = encode(id).map_err(RequestError::Encode)?;
            let (pairing, start) = match purpose {
                Purpose::Plain => (Pairing::Plain, None),
                Purpose::Opens { lane, .. } => (
                    Pairing::Opens {
                        lane: lane.id,
                        reserved: reservation.as_ref().map(|held| held.thread.clone()),
                    },
                    None,
                ),
                Purpose::Unsubscribes { thread } => (Pairing::Unsubscribes { thread }, None),
                Purpose::Starts {
                    lane,
                    turn,
                    decoded,
                    cx,
                } => (
                    Pairing::Starts {
                        lane: lane.id,
                        thread: state.threads.thread(lane.id).unwrap_or_default().to_owned(),
                        turn,
                        interrupt: None,
                    },
                    Some(StartMarker {
                        lane: Arc::clone(&lane.lane),
                        turn,
                        decoded,
                        cx,
                    }),
                ),
            };
            let charge = match &pairing {
                Pairing::Starts { thread, .. }
                | Pairing::Unsubscribes { thread }
                | Pairing::Opens {
                    reserved: Some(thread),
                    ..
                } => thread.len(),
                Pairing::Plain | Pairing::Opens { reserved: None, .. } => 0,
            };
            // x.3.2 X4 D3: a resume's record takes over its reservation's
            // entry and charge under this lock, so the thread stays fenced
            // with no gap and the budget is charged once.
            let carried = reservation.as_mut().is_some_and(|held| {
                let carried = state.reserved.get(&held.thread) == Some(&held.id);
                if carried {
                    state.reserved.remove(&held.thread);
                    held.transferred = true;
                }
                carried
            });
            if !carried && !state.budget.charge(charge) {
                drop(state);
                self.fail(ConnectionFailure::Overflow);
                return Err(RequestError::Exhausted);
            }
            let (waiter, reply) = oneshot::channel();
            state.requests.insert(
                id.get(),
                Record {
                    waiter: Some(waiter),
                    pairing,
                    charge,
                },
            );
            (id, message, reply, start)
        };
        let queue = match message {
            OutboundMessage::Start { .. } => Queue::Data,
            OutboundMessage::Control(_) | OutboundMessage::Interrupt(_) => Queue::Control,
        };
        let (answer, written) = oneshot::channel();
        let queued = self.feeder.push(
            queue,
            Item {
                message,
                bounds,
                answer: Answer::Write(answer),
                request: Some(id.get()),
                deadline: None,
                cancel: writes.map(|writes| Arc::clone(&writes.cancel)),
                start,
            },
        );
        if !queued {
            // Refused by a closed feeder: no reply can come.
            self.forget(id.get());
        }
        Ok(Requested { id, written, reply })
    }

    /// Queues one notification (no reply) on the control path.
    pub fn notify(
        &self,
        line: Vec<u8>,
        bounds: WriteBounds,
    ) -> Result<oneshot::Receiver<SendOutcome>, RequestError> {
        if self.state().ended || self.failure().is_some() {
            return Err(RequestError::Closed);
        }
        let (answer, written) = oneshot::channel();
        self.feeder.push(
            Queue::Control,
            Item {
                message: OutboundMessage::Control(line),
                bounds,
                answer: Answer::Write(answer),
                request: None,
                deadline: None,
                cancel: None,
                start: None,
            },
        );
        Ok(written)
    }

    /// A new lane for a thread open, whose messages feed `signal`'s
    /// sequence (item 13.2).
    pub fn open_lane(self: &Arc<Self>, signal: Option<&Arc<LeaseSignal>>) -> LaneLease {
        let lane = Arc::new(Lane::default());
        let id = self
            .state()
            .threads
            .open_lane(Arc::clone(&lane), signal.cloned());
        LaneLease {
            connection: Arc::clone(self),
            id,
            lane,
        }
    }

    /// Subscribes a lease to the connection task's abnormal end (item
    /// 13.2) until the subscription is dropped.
    pub fn subscribe(self: &Arc<Self>, signal: Arc<LeaseSignal>) -> Subscription {
        let mut state = self.state();
        state.next_signal = state.next_signal.wrapping_add(1);
        let id = state.next_signal;
        state.signals.insert(id, signal);
        Subscription {
            connection: Arc::clone(self),
            id,
        }
    }

    /// Posts the stop interrupt intent (item 8.3) of `lane`'s turn
    /// started by the `turn/start` record `start`, at most once per turn:
    /// owned by the connection, never withdrawn, it survives the turn's
    /// settlement. With the turn's vendor ID `accepted` it is queued now;
    /// before it, it waits on the record and is queued once its reply
    /// names the turn (an error reply, or a start never written, drops
    /// it). Whether it was posted.
    pub fn interrupt(
        &self,
        lane: &LaneLease,
        start: ClientId,
        accepted: Option<&str>,
        by: Deadline,
    ) -> bool {
        self.intent(lane, (start, false), accepted, by)
    }

    /// Posts the generation's cleanup interrupt intent (item 8.3) for
    /// `lane`'s turn of record `start`, as [`Self::interrupt`] does, but
    /// once per lane, and nothing when that turn's stop already posted
    /// its interrupt. Whether it was posted.
    pub fn cleanup_interrupt(
        &self,
        lane: &LaneLease,
        start: ClientId,
        accepted: Option<&str>,
        by: Deadline,
    ) -> bool {
        self.intent(lane, (start, true), accepted, by)
    }

    /// [`Self::interrupt`], or with `cleanup` [`Self::cleanup_interrupt`].
    fn intent(
        &self,
        lane: &LaneLease,
        (start, cleanup): (ClientId, bool),
        accepted: Option<&str>,
        by: Deadline,
    ) -> bool {
        let thread = {
            let mut state = self.state();
            let taken = if cleanup {
                state.threads.take_cleanup(lane.id, start.get())
            } else {
                state.threads.take_interrupt(lane.id, start.get())
            };
            if state.ended || !taken {
                return false;
            }
            let Some(thread) = state.threads.thread(lane.id).map(str::to_owned) else {
                return false;
            };
            if accepted.is_none() {
                return match state.requests.get_mut(&start.get()) {
                    Some(Record {
                        pairing: Pairing::Starts { interrupt, .. },
                        ..
                    }) => {
                        *interrupt = Some(by);
                        true
                    }
                    Some(_) | None => false,
                };
            }
            thread
        };
        accepted.is_some_and(|turn| self.post_interrupt(&thread, turn, by))
    }

    /// Queues one `turn/interrupt` whose reply nobody waits for.
    fn post_interrupt(&self, thread: &str, turn: &str, by: Deadline) -> bool {
        self.request(
            |id| turn_interrupt(id, thread, turn).map(OutboundMessage::Control),
            WriteBounds::StartBy {
                start_by: by,
                finish_by: by,
            },
            Purpose::Plain,
            None,
        )
        .is_ok()
    }

    /// Posts `lane`'s unsubscribe intent (item 8.3, packet §2), at most
    /// once per lane: never withdrawn. Its reply, for a close to wait on.
    pub fn unsubscribe(
        &self,
        lane: &LaneLease,
        by: Deadline,
    ) -> Option<oneshot::Receiver<Response>> {
        let thread = {
            let mut state = self.state();
            if state.ended || !state.threads.take_unsubscribe(lane.id) {
                return None;
            }
            state.threads.thread(lane.id)?.to_owned()
        };
        self.request(
            |id| thread_unsubscribe(id, &thread).map(OutboundMessage::Control),
            WriteBounds::StartBy {
                start_by: by,
                finish_by: by,
            },
            Purpose::Unsubscribes {
                thread: thread.clone(),
            },
            None,
        )
        .ok()
        .map(|requested| requested.reply)
    }

    /// x.3.2 X3 §5.1: posts `lane`'s close for the connection task, which
    /// applies it between two routing operations: the lane is cut (its
    /// thread's later traffic is late) and ends `Closed` after what it
    /// took. Coalesced, once per lane; a lane gone is ignored.
    pub fn post_close(&self, lane: &LaneLease) {
        if self.state().threads.post_close(lane.id) {
            self.closes.notify_one();
        }
    }

    /// Applies every posted close (x.3.2 X3 §5.2), in the connection task.
    fn apply_closes(&self) {
        let cut = self.state().threads.apply_closes();
        for lane in cut {
            lane.end(LaneEnd::Closed);
        }
    }

    /// Drops request `id`'s record: its write was never made, so no reply
    /// comes. Its waiter sees the closed channel; a delayed interrupt on
    /// it is dropped (nothing to interrupt). A `turn/start`'s positive
    /// `NotWritten` opens its lane's start gate if its `Start` holds it
    /// (x.3.2 X3 §2.2), whether or not its driver still waits.
    fn forget(&self, id: i64) {
        let (unwritten, fenced) = {
            let mut state = self.state();
            let Some(record) = state.requests.remove(&id) else {
                return;
            };
            state.budget.release(record.charge);
            let fenced = record.pairing.fences().is_some();
            let unwritten = match record.pairing {
                Pairing::Starts { lane, turn, .. } => {
                    state.threads.lane(lane).map(|(lane, _)| (lane, turn))
                }
                Pairing::Plain | Pairing::Opens { .. } | Pairing::Unsubscribes { .. } => None,
            };
            (unwritten, fenced)
        };
        // x.3.2 X4 D3: the attempt never reached the vendor, so its
        // thread's fence clears at once.
        if fenced {
            self.bump();
        }
        if let Some((lane, turn)) = unwritten {
            lane.start_unwritten(turn);
        }
    }

    /// A write the feeder saw end.
    fn written(&self, done: &Done) {
        if let Some(bytes) = done.reply {
            let mut state = self.state();
            state.replies.0 = state.replies.0.saturating_sub(1);
            state.replies.1 = state.replies.1.saturating_sub(bytes);
        }
        if let (Some(id), SendOutcome::NotWritten) = (done.request, done.outcome) {
            self.forget(id);
        }
    }

    /// Pairs one reply with its record (item 9.1): an ID that is not one
    /// of this connection's outstanding requests is an unattributable
    /// failure. A thread open's reply registers its lane only for a waiter
    /// still waiting; a `turn/start` reply maps an accepted turn and
    /// releases a delayed interrupt, and, whatever it says, pushes the
    /// turn's `Reply` marker, read at `at`, into its lane (x.3.2 X3 §2.1):
    /// an error reply tells its waiter whether the lane took an item
    /// naming an unmapped turn while the start was open (the refusal
    /// check).
    fn pair(&self, id: i64, mut response: Response, at: Instant) -> Result<(), ConnectionFailure> {
        let (waiter, interrupt, marker, fenced) = {
            let mut state = self.state();
            let state = &mut *state;
            let Some(record) = state.requests.remove(&id) else {
                return Err(ConnectionFailure::Protocol);
            };
            state.budget.release(record.charge);
            let fenced = record.pairing.fences().is_some();
            let waiting = record
                .waiter
                .as_ref()
                .is_some_and(|waiter| !waiter.is_closed());
            let mut interrupt = None;
            let mut marker = None;
            match (record.pairing, &response.outcome) {
                (Pairing::Opens { lane, .. }, Ok(raw)) => {
                    if let Ok(opened) = result::<ThreadResult>(raw) {
                        let thread = &opened.thread.id;
                        let kept = if waiting {
                            state
                                .threads
                                .register(lane, thread, &mut state.budget)
                                .map(drop)
                        } else {
                            state.threads.forgo(thread, &mut state.budget)
                        };
                        if kept.is_err() {
                            return Err(ConnectionFailure::Overflow);
                        }
                    }
                }
                (
                    Pairing::Starts {
                        lane,
                        thread,
                        turn,
                        interrupt: delayed,
                    },
                    outcome,
                ) => {
                    let accepted = match outcome {
                        Ok(raw) => result::<TurnStartResult>(raw)
                            .ok()
                            .map(|accepted| accepted.turn.id),
                        Err(_) => None,
                    };
                    if let Some(accepted) = &accepted {
                        // Kept whether or not the lane is still open: the
                        // turn's later traffic is late (packet §5).
                        if !thread.is_empty() {
                            let ids = (thread.as_str(), accepted.as_str());
                            match state.threads.map_turn(lane, ids, turn, &mut state.budget) {
                                Ok(true) => {}
                                Ok(false) => return Err(ConnectionFailure::Overflow),
                                Err(()) => return Err(ConnectionFailure::Protocol),
                            }
                        }
                        interrupt = delayed.map(|by| (thread, accepted.clone(), by));
                    }
                    marker = state
                        .threads
                        .lane(lane)
                        .map(|(lane, signal)| (lane, signal, turn, accepted));
                }
                (Pairing::Plain | Pairing::Opens { .. } | Pairing::Unsubscribes { .. }, _) => {}
            }
            (record.waiter, interrupt, marker, fenced)
        };
        // x.3.2 X4 D3: the vendor answered; a registration the reply made
        // fences the thread from here on.
        if fenced {
            self.bump();
        }
        if let Some((lane, signal, turn, accepted)) = marker {
            let (contradicted, pushed) = lane.push_reply(turn, at, accepted);
            response.contradicted = contradicted && response.outcome.is_err();
            if !pushed
                && lane.overflowed_now()
                && let Some(signal) = signal
            {
                signal.overflowed(lane.overflow_owner());
            }
        }
        if let Some((thread, turn, by)) = interrupt {
            self.post_interrupt(&thread, &turn, by);
        }
        let Some(waiter) = waiter else {
            return Ok(());
        };
        if waiter.send(response).is_err() {
            // The waiter is gone: the reply is consumed and counted.
            let mut state = self.state();
            state.counts.abandoned = state.counts.abandoned.saturating_add(1);
        }
        Ok(())
    }

    /// Where a message naming `thread` and `turn` goes, counted when it
    /// goes nowhere.
    fn route(&self, thread: Option<&str>, turn: Option<&str>) -> Route {
        let mut state = self.state();
        let route = state.threads.route(thread, turn);
        let counts = &mut state.counts;
        match &route {
            Route::Lane { .. } => {}
            Route::Late => counts.late_after_close = counts.late_after_close.saturating_add(1),
            Route::Unknown => counts.unknown_thread = counts.unknown_thread.saturating_add(1),
            Route::Untagged => counts.untagged = counts.untagged.saturating_add(1),
        }
        route
    }

    /// Routes one admitted message (item 5): the peek first, then a reply
    /// to its record, a server request to its decline, a notification raw
    /// to its registration's lane. An unattributable message is the
    /// connection's failure; its bytes are kept for the server folder.
    fn demux(&self, message: VendorMessage) -> Result<(), ConnectionFailure> {
        let routed = self.route_message(message);
        #[cfg(test)]
        {
            let mut state = self.state();
            state.routed = state.routed.saturating_add(1);
        }
        routed
    }

    /// The messages [`Self::demux`] finished routing.
    #[cfg(all(test, feature = "test-failpoints"))]
    pub(super) fn routed(&self) -> u64 {
        self.state().routed
    }

    /// The threads reserved now (x.3.2 X4 D3).
    #[cfg(all(test, feature = "test-failpoints"))]
    pub(super) fn reservations(&self) -> usize {
        self.state().reserved.len()
    }

    /// [`Self::demux`]'s routing.
    fn route_message(&self, message: VendorMessage) -> Result<(), ConnectionFailure> {
        // The read instant (C2 §4): the routed message's, whatever it waits.
        let at = Instant::now();
        let seq = {
            let mut state = self.state();
            state.seq = state.seq.saturating_add(1);
            state.seq
        };
        if let Some(skipped) = message.skipped() {
            return Err(self.skipped(&skipped.head));
        }
        match peek(message.bytes()) {
            Ok(Routing::Response(id)) => {
                let paired = match decode(message.bytes()) {
                    Ok(Incoming::Response(response)) => self.pair(id, response, at),
                    Ok(Incoming::Request(_) | Incoming::Notification(_)) | Err(_) => {
                        Err(ConnectionFailure::Protocol)
                    }
                };
                if paired == Err(ConnectionFailure::Protocol) {
                    self.keep_evidence(message.bytes());
                }
                paired
            }
            Ok(Routing::Request {
                id,
                method,
                thread,
                turn,
            }) => self.decline((&id, &method), (thread, turn), message, (seq, at)),
            Ok(Routing::Notification { thread, turn }) => {
                if let Route::Lane {
                    lane,
                    signal,
                    owner,
                } = self.route(thread.as_deref(), turn.as_deref())
                {
                    let bytes = message.bytes().len();
                    let item = LaneItem::Message(Routed {
                        staged: message,
                        seq,
                        turn,
                        owner,
                        at,
                        mark: None,
                    });
                    push(&lane, signal.as_ref(), item, bytes, seq);
                }
                Ok(())
            }
            Err(_) => {
                self.keep_evidence(message.bytes());
                Err(ConnectionFailure::Protocol)
            }
        }
    }

    /// A line over the cap, skipped by Wire to its LF: unattributable
    /// whatever its bytes name (owner 2026-10-05, review cfix-3), so its
    /// head is the server's evidence and the connection fails `protocol`.
    /// Post-release, a streaming JSON depth and string tracker in Wire can
    /// attribute it to its turn.
    fn skipped(&self, head: &[u8]) -> ConnectionFailure {
        self.keep_evidence(head);
        ConnectionFailure::Protocol
    }

    /// Keeps the first unattributable message for the server folder.
    fn keep_evidence(&self, bytes: &[u8]) {
        let mut state = self.state();
        if state.evidence.is_none() {
            state.evidence = Some(bytes[..bytes.len().min(EVIDENCE_BYTES)].to_vec());
        }
    }

    /// Item 11: the reply with the exact incoming ID, queued first on the
    /// control path under its own 5 s bound, and a placeholder in the
    /// thread's lane at this decode position, keeping the request's
    /// message (its staging charge). Past the pending replies' bound the
    /// connection fails `overflow`. x.3.2 X3 §5.3: the request is routed
    /// by its peek; only an open registration's lane gets the raw
    /// placeholder, whose full decode is the consumer's. A cut, late or
    /// unknown thread's is answered and counted, never decoded.
    fn decline(
        &self,
        (id, method): (&RequestId, &str),
        (thread, turn): (Option<String>, Option<String>),
        message: VendorMessage,
        (seq, decoded_at): (u64, Instant),
    ) -> Result<(), ConnectionFailure> {
        let line = self.declines.reply(id, method);
        let bytes = line.len();
        {
            let mut state = self.state();
            let (count, pending) = state.replies;
            if count >= REPLIES_MAX || pending.saturating_add(bytes) > REPLY_BYTES_MAX {
                return Err(ConnectionFailure::Overflow);
            }
            state.replies = (count + 1, pending.saturating_add(bytes));
        }
        let deadline = decoded_at + DECLINE_DEADLINE;
        let (written, written_rx) = watch::channel(None);
        let queued = self.feeder.push(
            Queue::Reply,
            Item {
                message: OutboundMessage::Control(line),
                bounds: WriteBounds::StartBy {
                    start_by: Deadline::at(deadline),
                    finish_by: Deadline::at(deadline),
                },
                answer: Answer::Reply(written),
                request: None,
                deadline: Some(deadline),
                cancel: None,
                start: None,
            },
        );
        if !queued {
            self.written(&Done {
                request: None,
                outcome: SendOutcome::NotWritten,
                reply: Some(bytes),
            });
        }
        // x.3.2 X3 S7: between the reply's queueing and the placeholder's
        // push, where a test posts a close.
        #[cfg(feature = "test-failpoints")]
        {
            let _ = crate::failpoint::hit("codex.connection.decline");
        }
        if let Route::Lane {
            lane,
            signal,
            owner,
        } = self.route(thread.as_deref(), turn.as_deref())
        {
            let size = message.bytes().len();
            let item = LaneItem::Declined {
                routed: Routed {
                    staged: message,
                    seq,
                    turn,
                    owner,
                    at: decoded_at,
                    mark: None,
                },
                decoded_at,
                written: written_rx,
            };
            push(&lane, signal.as_ref(), item, size, seq);
        }
        Ok(())
    }

    /// The owned sequence of a failed connection (item 13.1), after the
    /// latch: Host's stop beside the drain of the admitted prefix into the
    /// lanes, then the disposition to every lane and waiter.
    async fn fail_sequence(
        &self,
        (cause, at): (ConnectionFailure, FailureAt),
        messages: &mut WireMessages,
    ) -> ConnectionLoss {
        // Test builds: a seam between the latch and the owned sequence,
        // where a test holds the failed connection's server live in the
        // registry (x.3.2 X3 fix r3 #5).
        #[cfg(feature = "test-failpoints")]
        {
            let _ = crate::failpoint::hit_async("codex.connection.fail_sequence").await;
        }
        // Nothing more is written: what is queued answers `NotWritten`.
        for id in self.feeder.close() {
            self.forget(id);
        }
        let loss_deadline = Deadline::at(Instant::now() + LOSS_EVIDENCE);
        let close = self.stdio.close(CloseRequest {
            mode: CloseMode::Force,
            deadline: loss_deadline,
        });
        let mut drained = None;
        let drain_and_keep = async {
            // Up to the boundary: a later cause is counted by `fail`; the
            // prefix still reaches its lanes.
            while let Admitted::Message(message) = messages.drain_admitted().await {
                if let Err(later) = self.demux(message) {
                    drained.get_or_insert(later);
                    self.fail(later);
                }
            }
            // After the drain, so a message it found unattributable is
            // kept too (review cfix-crit #2).
            let evidence = self.state().evidence.take();
            if let Some(bytes) = evidence {
                self.stdio
                    .keep_undecoded(&bytes, "the shared connection's message")
                    .await;
            }
        };
        let (report, ()) = tokio::join!(close, drain_and_keep);
        // A Wire stream failure comes after every admitted message: a
        // failure the drain found precedes it in stream order, and the
        // first in stream order is the connection's (review cfix-crit #2).
        let cause = match (at, drained) {
            (FailureAt::StreamEnd, Some(earlier)) => {
                self.failure.send_replace(Some(earlier));
                earlier
            }
            (FailureAt::StreamEnd | FailureAt::Elsewhere, _) => cause,
        };
        let loss = disposition(cause, &report);
        self.finish(ConnectionEnd::Failed(loss), LaneEnd::Lost(loss));
        loss
    }

    /// Ends every lane with `lane_end` and drops every record, so each
    /// waiter sees the end; then publishes `end`.
    fn finish(&self, end: ConnectionEnd, lane_end: LaneEnd) {
        let (lanes, records) = {
            let mut state = self.state();
            state.ended = true;
            state.reserved.clear();
            let lanes = state.threads.drain();
            let records: Vec<Record> = state.requests.drain().map(|(_, record)| record).collect();
            (lanes, records)
        };
        self.feeder.close();
        for lane in lanes {
            lane.end(lane_end);
        }
        // The end is published before the waiters see their closed
        // channels, so each reads it at once.
        self.end.send_replace(Some(end));
        drop(records);
        self.bump();
    }

    /// Item 13.2, the abnormal path: the connection task died with its
    /// receiver, so the registry's supervisor ends the connection here, in
    /// its step: every lane ends with no boundary, every waiter sees the
    /// end, nothing more is written, and every lease's driver is signalled
    /// at once, independently of the data path. A connection whose task
    /// had already published its end changes nothing.
    pub fn abnormal(&self) {
        let (lanes, records, signals) = {
            let mut state = self.state();
            if self.end.borrow().is_some() {
                return;
            }
            state.ended = true;
            state.reserved.clear();
            let lanes = state.threads.drain();
            let records: Vec<Record> = state.requests.drain().map(|(_, record)| record).collect();
            let signals: Vec<Arc<LeaseSignal>> = state.signals.values().cloned().collect();
            (lanes, records, signals)
        };
        self.feeder.close();
        // Every lease's driver latches its failure first, so the cause a
        // turn reports never races the lanes' end.
        for signal in signals {
            signal.signal();
        }
        for lane in lanes {
            lane.end(LaneEnd::Abnormal);
        }
        self.end
            .send_replace(Some(ConnectionEnd::Failed(abnormal_loss())));
        drop(records);
        self.bump();
        // The task that would have written the `via.log` line is gone:
        // this end writes it, even after the shutdown fence (a panic is
        // never Host's stop).
        let undecoded = self.stdio.take_undecoded();
        tracing::warn!(
            server = %self.server,
            cause = ?abnormal_loss().cause,
            undecoded = undecoded.as_deref().unwrap_or("none"),
            "shared server connection task ended abnormally"
        );
    }
}

/// Pushes `item` into `lane`, advancing its lease's sequence when taken;
/// one an overflowed lane dropped reaches the lease's driver at once
/// (x.3.2 X3 fix r2 #1), naming the lane's overflow owner (critical
/// review x5 r3), whichever item this was.
fn push(lane: &Lane, signal: Option<&Arc<LeaseSignal>>, item: LaneItem, bytes: usize, seq: u64) {
    let taken = lane.push(item, bytes);
    let Some(signal) = signal else {
        return;
    };
    if taken {
        signal.queued(seq);
    } else if lane.overflowed_now() {
        signal.overflowed(lane.overflow_owner());
    }
}

/// The loss a dead connection task leaves (item 13.2, R3-Q3): transport
/// lost, cleanup unproven.
fn abnormal_loss() -> ConnectionLoss {
    ConnectionLoss {
        cause: LossCause::TransportLost,
        cleanup: WireCleanup::Uncertain,
        exit: None,
        journal_uncertain: false,
    }
}

/// The disposition of a latched cause, with Host's stop report (item
/// 13.1 fan-out).
fn disposition(cause: ConnectionFailure, report: &WireCloseReport) -> ConnectionLoss {
    let cause = match cause {
        ConnectionFailure::Protocol => LossCause::Protocol,
        ConnectionFailure::Overflow => LossCause::Overflow,
        ConnectionFailure::Exited => LossCause::ServerLost,
        ConnectionFailure::Transport { stdio_end } => {
            if stdio_end && report.stopped_live == Some(false) {
                LossCause::ServerLost
            } else {
                LossCause::TransportLost
            }
        }
        ConnectionFailure::Internal => LossCause::TransportLost,
    };
    ConnectionLoss {
        cause,
        cleanup: report.cleanup,
        exit: report.vendor_exit,
        journal_uncertain: report.journal_uncertain,
    }
}

/// The daemon-level `via.log` line for a failed shared connection
/// (codex-server.md item 5; runtime §6.2): the server ID, the cause and
/// the note naming the server folder's `undecoded.bin`, or why it was not
/// saved. Only VIA's own text: never a vendor byte. Read after the reader
/// finished, so a save it began is noted. The transport or server lost
/// after the daemon's shutdown began is Host's stop: no line.
fn log_failure(connection: &Connection, cause: LossCause) {
    if connection.shutting_down.load(Ordering::Acquire)
        && matches!(cause, LossCause::TransportLost | LossCause::ServerLost)
    {
        return;
    }
    let undecoded = connection.stdio.take_undecoded();
    tracing::warn!(
        server = %connection.server,
        ?cause,
        undecoded = undecoded.as_deref().unwrap_or("none"),
        "shared server connection failed"
    );
}

/// Where a connection's first failure stands in its message stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureAt {
    /// Wire's stream failed: after every message it admitted.
    StreamEnd,
    /// Anywhere else: a routed message, a write, a driver's request.
    Elsewhere,
}

/// The cause a Wire error on the message side latches (item 13.1 table).
fn wire_failure(error: &WireError) -> ConnectionFailure {
    match error {
        WireError::Message(WireFailure::MessageTooLarge | WireFailure::UnterminatedMessage) => {
            ConnectionFailure::Protocol
        }
        WireError::Message(WireFailure::Overflow) => ConnectionFailure::Overflow,
        WireError::Io(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            ConnectionFailure::Transport { stdio_end: true }
        }
        WireError::Message(WireFailure::Transport)
        | WireError::Io(_)
        | WireError::Host(_)
        | WireError::Evidence(_)
        | WireError::Deadline
        | WireError::Cancelled
        | WireError::Woken
        | WireError::Acquire { .. } => ConnectionFailure::Transport { stdio_end: false },
    }
}

/// Test builds: the connection task's failure seam (item 13.2), hit
/// before each admitted message is routed.
#[cfg(feature = "test-failpoints")]
async fn panic_seam() {
    if crate::failpoint::hit_async("codex.connection.message")
        .await
        .is_err()
    {
        connection_task_failed();
    }
}

#[cfg(feature = "test-failpoints")]
#[expect(clippy::panic, reason = "the seam fails the connection task")]
fn connection_task_failed() {
    panic!("the connection task failed at its test seam");
}

/// The connection task: reads every message and routes it, and runs the
/// feeder, until stdout ends (a retirement) or a failure latches; then
/// runs the owned sequence. It holds the unique message receiver and
/// finishes it.
pub(super) async fn serve(
    connection: Arc<Connection>,
    mut messages: WireMessages,
) -> ConnectionEnd {
    // A cause latched outside this task (exhaustion at a driver's request)
    // wakes it: the seal stops admission, so no message would.
    let mut latched = connection.failure.subscribe();
    // Where the latched cause stands in the stream: after every admitted
    // message when Wire's stream failed (review cfix-crit #2).
    let mut at = FailureAt::Elsewhere;
    let done = |done: Done| connection.written(&done);
    let cause = {
        let pump = connection.feeder.pump(&done);
        tokio::pin!(pump);
        let mut pumping = true;
        loop {
            if let Some(cause) = connection.failure() {
                break Some(cause);
            }
            tokio::select! {
                biased;
                end = &mut pump, if pumping => {
                    pumping = false;
                    match end {
                        // Packet §4: a decline not written whole by its
                        // deadline fails the connection.
                        PumpEnd::ReplyLate => connection.fail(ConnectionFailure::Overflow),
                    }
                }
                _ = latched.changed() => {}
                // x.3.2 X3 §5.1: a posted close, ahead of the next message.
                () = connection.closes.notified() => connection.apply_closes(),
                next = messages.next_message() => match next {
                    Ok(Some(message)) => {
                        #[cfg(feature = "test-failpoints")]
                        panic_seam().await;
                        if let Err(cause) = connection.demux(message) {
                            connection.fail(cause);
                        }
                    }
                    Ok(None) if connection.retiring.load(Ordering::Acquire) => break None,
                    Ok(None) => connection.fail(ConnectionFailure::Transport { stdio_end: true }),
                    Err(error) => {
                        if connection.latch(wire_failure(&error)) {
                            at = FailureAt::StreamEnd;
                        }
                    }
                },
            }
        }
    };
    let end = if let Some(cause) = cause {
        ConnectionEnd::Failed(connection.fail_sequence((cause, at), &mut messages).await)
    } else {
        connection.finish(ConnectionEnd::Retired, LaneEnd::Retired);
        ConnectionEnd::Retired
    };
    messages
        .finish(Deadline::at(Instant::now() + LOSS_EVIDENCE))
        .await;
    if let ConnectionEnd::Failed(loss) = end {
        log_failure(&connection, loss.cause);
    }
    end
}
