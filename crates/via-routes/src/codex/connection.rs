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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use tokio::time::Instant;
use via_wire::{
    Admitted, CloseMode, CloseRequest, CommitOutcome, Deadline, OutboundMessage, SendOutcome,
    ServerId, SessionId, TurnNumber, VendorMessage, WireCleanup, WireCloseReport, WireError,
    WireFailure, WireMessages, WireSender, WriteBounds,
};

use super::feeder::{Answer, Done, Feeder, Item, PumpEnd, Queue, WriteCancel};
use super::lane::{ConnectionLoss, Lane, LaneEnd, LaneItem, LeaseSignal, LossCause, Routed};
use super::stdio::Stdio;
use super::threads::{Budget, Route, ThreadTable};
use super::{
    ClientId, ClientIds, DeclineTable, EncodeError, Incoming, Response, Routing, ServerRequest,
    ThreadResult, TurnStartResult, decode, peek, result, thread_unsubscribe, turn_interrupt,
};

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
#[derive(Clone, Copy)]
pub enum Purpose<'a> {
    /// Nothing beyond its waiter.
    Plain,
    /// A `thread/start` or `thread/resume`: its reply registers `lane`
    /// under the returned thread, but only while its waiter still waits
    /// (item 9.1); otherwise the thread is kept as closed.
    Opens(&'a LaneLease),
    /// A `turn/start` on `lane`'s thread: its accepted turn is mapped to
    /// VIA turn `turn` until the connection retires (packet §5).
    Starts {
        /// The registration.
        lane: &'a LaneLease,
        /// The VIA turn.
        turn: TurnNumber,
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

/// What the connection keeps, under one std mutex never held across an
/// await.
struct State {
    ids: ClientIds,
    requests: HashMap<i64, Record>,
    threads: ThreadTable,
    budget: Budget,
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
        self.connection.state().threads.close_lane(self.id);
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
    }

    /// Marks the idle retirement begun: from now the end of stdout ends
    /// the connection as retired.
    pub fn retire(&self) {
        self.retiring.store(true, Ordering::Release);
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
        purpose: Purpose<'_>,
        writes: Option<&mut TurnWrites>,
    ) -> Result<Requested, RequestError> {
        let (id, message, reply) = {
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
            let pairing = match purpose {
                Purpose::Plain => Pairing::Plain,
                Purpose::Opens(lane) => Pairing::Opens { lane: lane.id },
                Purpose::Starts { lane, turn } => Pairing::Starts {
                    lane: lane.id,
                    thread: state.threads.thread(lane.id).unwrap_or_default().to_owned(),
                    turn,
                    interrupt: None,
                },
            };
            let charge = match &pairing {
                Pairing::Starts { thread, .. } => thread.len(),
                Pairing::Plain | Pairing::Opens { .. } => 0,
            };
            if !state.budget.charge(charge) {
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
            (id, message, reply)
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
            Purpose::Plain,
            None,
        )
        .ok()
        .map(|requested| requested.reply)
    }

    /// Drops request `id`'s record: its write was never made, so no reply
    /// comes. Its waiter sees the closed channel; a delayed interrupt on
    /// it is dropped (nothing to interrupt).
    fn forget(&self, id: i64) {
        let mut state = self.state();
        if let Some(record) = state.requests.remove(&id) {
            state.budget.release(record.charge);
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
    /// still waiting; a `turn/start` reply maps its turn, counts its
    /// acceptance, read at `at`, under the lane's fence and releases a
    /// delayed interrupt.
    fn pair(&self, id: i64, response: Response, at: Instant) -> Result<(), ConnectionFailure> {
        let (waiter, interrupt) = {
            let mut state = self.state();
            let state = &mut *state;
            let Some(record) = state.requests.remove(&id) else {
                return Err(ConnectionFailure::Protocol);
            };
            state.budget.release(record.charge);
            let waiting = record
                .waiter
                .as_ref()
                .is_some_and(|waiter| !waiter.is_closed());
            let mut interrupt = None;
            match (record.pairing, &response.outcome) {
                (Pairing::Opens { lane }, Ok(raw)) => {
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
                    Ok(raw),
                ) => {
                    if let Ok(accepted) = result::<TurnStartResult>(raw) {
                        // Kept whether or not the lane is still open: the
                        // turn's later traffic is late (packet §5).
                        if !thread.is_empty() {
                            let ids = (thread.as_str(), accepted.turn.id.as_str());
                            match state.threads.map_turn(lane, ids, turn, &mut state.budget) {
                                Ok(true) => {}
                                Ok(false) => return Err(ConnectionFailure::Overflow),
                                Err(()) => return Err(ConnectionFailure::Protocol),
                            }
                        }
                        // The acceptance is a message of the turn's fence,
                        // read now (x.3.2 X3 fix r2 #10).
                        if let Some(lane) = state.threads.lane(lane) {
                            lane.read_acceptance(at);
                        }
                        interrupt = delayed.map(|by| (thread, accepted.turn.id, by));
                    }
                }
                (Pairing::Plain | Pairing::Opens { .. } | Pairing::Starts { .. }, _) => {}
            }
            (record.waiter, interrupt)
        };
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

    /// [`Self::demux`]'s routing.
    fn route_message(&self, message: VendorMessage) -> Result<(), ConnectionFailure> {
        // The read instant (C2 §4): the routed message's, whatever it waits.
        let at = Instant::now();
        let seq = {
            let mut state = self.state();
            state.seq = state.seq.saturating_add(1);
            state.seq
        };
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
            Ok(Routing::Request) => match decode(message.bytes()) {
                Ok(Incoming::Request(request)) => self.decline(request, message, (seq, at)),
                Ok(Incoming::Response(_) | Incoming::Notification(_)) | Err(_) => {
                    self.keep_evidence(message.bytes());
                    Err(ConnectionFailure::Protocol)
                }
            },
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
    /// connection fails `overflow`.
    fn decline(
        &self,
        request: ServerRequest,
        message: VendorMessage,
        (seq, decoded_at): (u64, Instant),
    ) -> Result<(), ConnectionFailure> {
        let line = self.declines.reply(&request.id, &request.method);
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
            },
        );
        if !queued {
            self.written(&Done {
                request: None,
                outcome: SendOutcome::NotWritten,
                reply: Some(bytes),
            });
        }
        if let Route::Lane {
            lane,
            signal,
            owner,
        } = self.route(request.thread_id.as_deref(), request.turn_id.as_deref())
        {
            let size = message.bytes().len();
            let item = LaneItem::Declined {
                routed: Routed {
                    staged: message,
                    seq,
                    turn: request.turn_id.clone(),
                    owner,
                    at: decoded_at,
                    mark: None,
                },
                request,
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
        cause: ConnectionFailure,
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
        let drain = async {
            // Up to the boundary: a later cause is counted by `fail`; the
            // prefix still reaches its lanes.
            while let Admitted::Message(message) = messages.drain_admitted().await {
                if let Err(later) = self.demux(message) {
                    self.fail(later);
                }
            }
        };
        let evidence = self.state().evidence.take();
        let keep = async {
            if let Some(bytes) = evidence {
                self.stdio
                    .keep_undecoded(&bytes, "the shared connection's message")
                    .await;
            }
        };
        let (report, (), ()) = tokio::join!(close, drain, keep);
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
    }
}

/// Pushes `item` into `lane`, advancing its lease's sequence when taken;
/// one an overflowed lane dropped reaches the lease's driver at once
/// (x.3.2 X3 fix r2 #1).
fn push(lane: &Lane, signal: Option<&Arc<LeaseSignal>>, item: LaneItem, bytes: usize, seq: u64) {
    let taken = lane.push(item, bytes);
    let Some(signal) = signal else {
        return;
    };
    if taken {
        signal.queued(seq);
    } else if lane.overflowed_now() {
        signal.overflowed();
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
                    Err(error) => connection.fail(wire_failure(&error)),
                },
            }
        }
    };
    let end = if let Some(cause) = cause {
        ConnectionEnd::Failed(connection.fail_sequence(cause, &mut messages).await)
    } else {
        connection.finish(ConnectionEnd::Retired, LaneEnd::Retired);
        ConnectionEnd::Retired
    };
    messages
        .finish(Deadline::at(Instant::now() + LOSS_EVIDENCE))
        .await;
    end
}
