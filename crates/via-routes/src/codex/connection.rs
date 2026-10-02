//! `codex::Connection` (vendors/codex.md §2, §4, §5; x.3.2 X0 items 5, 9,
//! 11, 12, 13): one shared server's typed connection. One connection task
//! ([`serve`]) owns the unique message receiver: it pairs every reply with
//! its request record by ID (IDs are connection-local, monotonic and never
//! reused), routes each notification by `threadId` into that thread's
//! ingress lane, and answers every server request with its no-grant body
//! or `-32601` under the exact incoming ID on the control path within 5 s.
//! Drivers write their requests through [`Connection::request`]; the
//! connection never waits on a driver.
//!
//! A whole-connection failure runs one owned sequence: a first-wins latch
//! that seals Wire's admission, Host's stop beside the drain of the
//! admitted prefix into the lanes, then the fan-out of the disposition to
//! every lane and waiter (item 13.1).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use tokio::time::Instant;
use via_wire::{
    Admitted, CloseMode, CloseRequest, DataHold, Deadline, OutboundMessage, PendingWrite,
    SendOutcome, ServerId, WireCleanup, WireCloseReport, WireError, WireFailure, WireMessages,
    WireSender, WriteBounds,
};

use super::lane::{ConnectionLoss, Lane, LaneEnd, LaneItem, LossCause};
use super::{
    ClientId, ClientIds, DeclineTable, EncodeError, Incoming, Notification, RequestId, Response,
    ServerRequest, ThreadResult, decode, peek_thread, result,
};

/// C2 A6: a server request is answered within this of its decode.
pub const DECLINE_DEADLINE: Duration = Duration::from_secs(5);

/// The most server-request replies pending at once, and their bytes
/// (packet §4); one more fails the connection `overflow`.
const REPLIES_MAX: usize = 8;
const REPLY_BYTES_MAX: usize = 64 * 1024;

/// How long a failed connection's evidence is waited for (item 13.1).
const LOSS_EVIDENCE: Duration = Duration::from_secs(5);

/// The far bound by which a started data message is written whole: the
/// connection's own, never a turn's (item 12.2).
pub const FINISH_BY: Duration = Duration::from_secs(3600);

/// The most closed threads remembered, so late traffic for them is
/// counted rather than reported as an unknown thread.
const TOMBSTONES: usize = 256;

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

/// Why a request was not written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestError {
    /// The connection failed or retired: nothing is admitted.
    Closed,
    /// The request IDs are exhausted: the connection fails `overflow`.
    Exhausted,
    /// The request could not be encoded.
    Encode(EncodeError),
}

/// A request admitted to the connection: its ID, its write, and the wait
/// for its paired reply. Dropping the reply wait abandons the record: a
/// reply still pairs, and is counted and dropped.
pub struct Requested {
    /// The request's ID.
    pub id: ClientId,
    /// The write.
    pub write: PendingWrite,
    /// The paired reply; closed when the connection ends first.
    pub reply: oneshot::Receiver<Response>,
}

/// One client request record (item 9.1), kept until its reply or the
/// connection's end; its waiter may be gone (abandoned).
struct Record {
    /// Answered with the paired reply; `None` for a fire-and-forget.
    waiter: Option<oneshot::Sender<Response>>,
    /// The lane a `thread/start` or `thread/resume` reply registers under
    /// the returned thread ID before the driver sees the reply (packet §5).
    opens: Option<Arc<Lane>>,
}

/// What the connection task keeps, under one std mutex never held across
/// an await.
struct State {
    ids: ClientIds,
    requests: HashMap<i64, Record>,
    threads: HashMap<String, Arc<Lane>>,
    /// Recently closed threads, oldest first.
    closed: VecDeque<String>,
    /// The connection ended: no request or registration is admitted.
    ended: bool,
    /// The first unattributable message's bytes, for the server folder.
    evidence: Option<Vec<u8>>,
    /// Diagnostics: replies whose waiter was gone, messages for unknown
    /// threads, for closed threads, and untagged connection traffic.
    counts: Counts,
}

/// The connection's diagnostic counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    /// Replies paired after their waiter was gone.
    pub abandoned: u64,
    /// Well-formed messages for a thread never registered.
    pub unknown_thread: u64,
    /// Messages for a thread already closed.
    pub late_after_close: u64,
    /// Untagged connection-scoped traffic.
    pub untagged: u64,
    /// Later failure causes after the first was latched.
    pub later_failures: u64,
}

/// One shared server's connection: the clonable control side the drivers
/// and the registry hold; the message side lives in [`serve`].
pub struct Connection {
    server: ServerId,
    sender: WireSender,
    declines: DeclineTable,
    state: Mutex<State>,
    /// The first failure, latched once (item 13.1).
    failure: watch::Sender<Option<ConnectionFailure>>,
    /// How the connection ended, once it did.
    end: watch::Sender<Option<ConnectionEnd>>,
    /// The idle retirement began: the end of stdout is not a failure.
    retiring: AtomicBool,
}

impl Connection {
    /// The connection of server `server` over `sender`, answering server
    /// requests from `declines`.
    pub fn new(server: ServerId, sender: WireSender, declines: DeclineTable) -> Arc<Self> {
        Arc::new(Self {
            server,
            sender,
            declines,
            state: Mutex::new(State {
                ids: ClientIds::default(),
                requests: HashMap::new(),
                threads: HashMap::new(),
                closed: VecDeque::new(),
                ended: false,
                evidence: None,
                counts: Counts::default(),
            }),
            failure: watch::Sender::new(None),
            end: watch::Sender::new(None),
            retiring: AtomicBool::new(false),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // Every edit is a single insert or remove: the state stays
        // consistent across a panic elsewhere.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The server this connection talks to.
    pub fn server(&self) -> &ServerId {
        &self.server
    }

    /// Wire's control half: link a turn, withdraw a write.
    pub fn sender(&self) -> &WireSender {
        &self.sender
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
                return ConnectionEnd::Failed(ConnectionLoss {
                    cause: LossCause::TransportLost,
                    cleanup: WireCleanup::Uncertain,
                    exit: None,
                    journal_uncertain: false,
                });
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
            self.sender.seal();
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

    /// Writes one request: allocates its ID, records it (with the lane its
    /// reply registers, for a thread open), then hands `encode`'s message
    /// to Wire under `bounds`. The record exists before the first byte, so
    /// a fast reply always pairs.
    pub fn request(
        &self,
        encode: impl FnOnce(ClientId) -> Result<OutboundMessage, EncodeError>,
        bounds: WriteBounds,
        opens: Option<Arc<Lane>>,
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
            let (waiter, reply) = oneshot::channel();
            state.requests.insert(
                id.get(),
                Record {
                    waiter: Some(waiter),
                    opens,
                },
            );
            (id, message, reply)
        };
        let write = self.sender.write(message, bounds);
        Ok(Requested { id, write, reply })
    }

    /// Writes one notification (no reply).
    pub fn notify(&self, line: Vec<u8>, bounds: WriteBounds) -> Result<PendingWrite, RequestError> {
        if self.state().ended || self.failure().is_some() {
            return Err(RequestError::Closed);
        }
        Ok(self.sender.write(OutboundMessage::Control(line), bounds))
    }

    /// Whether `thread` is registered to `lane`.
    pub fn registered(&self, thread: &str, lane: &Arc<Lane>) -> bool {
        self.state()
            .threads
            .get(thread)
            .is_some_and(|registered| Arc::ptr_eq(registered, lane))
    }

    /// Registers `lane` for `thread` unless another lane holds it; whether
    /// it did (packet §5: one open registration per thread).
    pub fn register(&self, thread: &str, lane: &Arc<Lane>) -> bool {
        let mut state = self.state();
        if state.ended {
            return false;
        }
        if let Some(registered) = state.threads.get(thread) {
            return Arc::ptr_eq(registered, lane);
        }
        state.closed.retain(|closed| closed != thread);
        state.threads.insert(thread.to_owned(), Arc::clone(lane));
        true
    }

    /// Removes `thread`'s registration if `lane` holds it, remembering the
    /// thread so its late traffic is counted (a tombstone).
    pub fn unregister(&self, thread: &str, lane: &Arc<Lane>) {
        let mut state = self.state();
        if state
            .threads
            .get(thread)
            .is_some_and(|registered| Arc::ptr_eq(registered, lane))
        {
            state.threads.remove(thread);
            if state.closed.len() == TOMBSTONES {
                state.closed.pop_front();
            }
            state.closed.push_back(thread.to_owned());
        }
    }

    /// Pairs one reply with its record (item 9.1): an ID that is not one of
    /// this connection's outstanding integers is an unattributable failure.
    /// A thread open's reply registers its lane first.
    fn pair(&self, response: Response) -> Result<(), ConnectionFailure> {
        let RequestId::Int(id) = response.id else {
            return Err(ConnectionFailure::Protocol);
        };
        let mut state = self.state();
        let Some(record) = state.requests.remove(&id) else {
            return Err(ConnectionFailure::Protocol);
        };
        if let (Some(lane), Ok(raw)) = (&record.opens, &response.outcome)
            && let Ok(opened) = result::<ThreadResult>(raw)
            && !state.threads.contains_key(&opened.thread.id)
        {
            state.closed.retain(|closed| *closed != opened.thread.id);
            state
                .threads
                .insert(opened.thread.id.clone(), Arc::clone(lane));
        }
        let Some(waiter) = record.waiter else {
            return Ok(());
        };
        drop(state);
        if waiter.send(response).is_err() {
            // The waiter is gone: the reply is consumed and counted.
            let mut state = self.state();
            state.counts.abandoned = state.counts.abandoned.saturating_add(1);
        }
        Ok(())
    }

    /// The lane of `thread`, or the count of where its message went.
    fn lane_of(&self, thread: Option<&str>) -> Option<Arc<Lane>> {
        let mut state = self.state();
        let Some(thread) = thread else {
            state.counts.untagged = state.counts.untagged.saturating_add(1);
            return None;
        };
        if let Some(lane) = state.threads.get(thread) {
            return Some(Arc::clone(lane));
        }
        if state.closed.iter().any(|closed| closed == thread) {
            state.counts.late_after_close = state.counts.late_after_close.saturating_add(1);
        } else {
            state.counts.unknown_thread = state.counts.unknown_thread.saturating_add(1);
        }
        None
    }

    /// Routes one admitted message (item 5): a reply to its record, a
    /// server request to its decline, a notification to its thread's lane.
    /// An unattributable message is the connection's failure; its bytes
    /// are kept for the server folder.
    fn demux(
        &self,
        message: via_wire::VendorMessage,
        replies: &mut Replies,
    ) -> Result<(), ConnectionFailure> {
        match decode(message.bytes()) {
            Ok(Incoming::Response(response)) => {
                let paired = self.pair(response);
                if paired.is_err() {
                    self.keep_evidence(message.bytes());
                }
                paired
            }
            Ok(Incoming::Request(request)) => self.decline(request, replies),
            Ok(Incoming::Notification(notification)) => {
                self.route(notification, message);
                Ok(())
            }
            Err(error) => {
                let Some(thread) = peek_thread(message.bytes()) else {
                    self.keep_evidence(message.bytes());
                    return Err(ConnectionFailure::Protocol);
                };
                if let Some(lane) = self.lane_of(Some(&thread)) {
                    let bytes = message.bytes().len();
                    lane.push(
                        LaneItem::Malformed {
                            staged: message,
                            detail: error.detail(),
                        },
                        bytes,
                    );
                }
                Ok(())
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

    fn route(&self, notification: Notification, staged: via_wire::VendorMessage) {
        if let Some(lane) = self.lane_of(notification.thread_id()) {
            let bytes = staged.bytes().len();
            lane.push(
                LaneItem::Notification {
                    notification,
                    staged,
                },
                bytes,
            );
        }
    }

    /// Item 11: the reply with the exact incoming ID, queued on the
    /// control path with priority over unstarted data, and a placeholder
    /// in the thread's lane at this decode position. Past the pending
    /// replies' bound the connection fails `overflow`.
    fn decline(
        &self,
        request: ServerRequest,
        replies: &mut Replies,
    ) -> Result<(), ConnectionFailure> {
        let decoded_at = Instant::now();
        let line = self.declines.reply(&request.id, &request.method);
        let bytes = line.len();
        if replies.pending.len() >= REPLIES_MAX
            || replies.bytes.saturating_add(bytes) > REPLY_BYTES_MAX
        {
            return Err(ConnectionFailure::Overflow);
        }
        let hold = self.sender.hold_data();
        let deadline = Deadline::at(decoded_at + DECLINE_DEADLINE);
        let write = self.sender.write(
            OutboundMessage::Control(line),
            WriteBounds::StartBy {
                start_by: deadline,
                finish_by: deadline,
            },
        );
        let (written, written_rx) = watch::channel(None);
        replies.bytes = replies.bytes.saturating_add(bytes);
        replies.pending.push(PendingReply {
            write: Box::pin(write),
            _hold: hold,
            written,
            bytes,
        });
        if let Some(lane) = self.lane_of(request.thread_id.as_deref()) {
            let size = request.method.len();
            lane.push(
                LaneItem::Declined {
                    request,
                    decoded_at,
                    written: written_rx,
                },
                size,
            );
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
        let loss_deadline = Deadline::at(Instant::now() + LOSS_EVIDENCE);
        let close = self.sender.close(CloseRequest {
            mode: CloseMode::Force,
            deadline: loss_deadline,
        });
        let drain = async {
            let mut ignored = Replies::default();
            // Up to the boundary: a later cause is counted by `fail`; the
            // prefix still reaches its lanes.
            while let Admitted::Message(message) = messages.drain_admitted().await {
                if let Err(later) = self.demux(message, &mut ignored) {
                    self.fail(later);
                }
            }
        };
        let evidence = self.state().evidence.take();
        let keep = async {
            if let Some(bytes) = evidence {
                self.sender
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
            let lanes: Vec<Arc<Lane>> = state.threads.drain().map(|(_, lane)| lane).collect();
            let records: Vec<Record> = state.requests.drain().map(|(_, record)| record).collect();
            (lanes, records)
        };
        for lane in lanes {
            lane.end(lane_end);
        }
        // The end is published before the waiters see their closed
        // channels, so each reads it at once.
        self.end.send_replace(Some(end));
        drop(records);
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

/// A server-request reply being written.
struct PendingReply {
    write: Pin<Box<PendingWrite>>,
    /// Keeps the writer off unstarted data until the reply is written.
    _hold: DataHold,
    written: watch::Sender<Option<bool>>,
    bytes: usize,
}

/// The replies the connection task is writing.
#[derive(Default)]
struct Replies {
    pending: Vec<PendingReply>,
    bytes: usize,
}

impl Replies {
    /// The next reply write to answer: whether it was written whole.
    fn next(&mut self) -> impl Future<Output = bool> + '_ {
        std::future::poll_fn(move |cx| {
            for index in 0..self.pending.len() {
                if let Poll::Ready(outcome) = self.pending[index].write.as_mut().poll(cx) {
                    let reply = self.pending.swap_remove(index);
                    self.bytes = self.bytes.saturating_sub(reply.bytes);
                    let written = matches!(outcome, Ok(SendOutcome::Written));
                    reply.written.send_replace(Some(written));
                    return Poll::Ready(written);
                }
            }
            Poll::Pending
        })
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

/// The connection task: reads every message, pairs, routes and declines,
/// and writes the decline replies, until stdout ends (a retirement) or a
/// failure latches; then runs the owned sequence. It holds the unique
/// message receiver and finishes it.
pub(super) async fn serve(
    connection: Arc<Connection>,
    mut messages: WireMessages,
) -> ConnectionEnd {
    let mut replies = Replies::default();
    // A cause latched outside this task (ID exhaustion at a driver's
    // request) wakes it: the seal stops admission, so no message would.
    let mut latched = connection.failure.subscribe();
    let cause = loop {
        if let Some(cause) = connection.failure() {
            break Some(cause);
        }
        tokio::select! {
            biased;
            written = replies.next(), if !replies.pending.is_empty() => {
                // Packet §4: a decline not written by its deadline fails
                // the connection.
                if !written {
                    connection.fail(ConnectionFailure::Overflow);
                }
            }
            _ = latched.changed() => {}
            next = messages.next_message() => match next {
                Ok(Some(message)) => {
                    if let Err(cause) = connection.demux(message, &mut replies) {
                        connection.fail(cause);
                    }
                }
                Ok(None) if connection.retiring.load(Ordering::Acquire) => break None,
                Ok(None) => connection.fail(ConnectionFailure::Transport { stdio_end: true }),
                Err(error) => connection.fail(wire_failure(&error)),
            },
        }
    };
    drop(replies);
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
