//! One private connection's byte transport (design §8): a stdout reader
//! task feeding a bounded message queue, a stdin writer task, one health
//! latch, and `finish` under one deadline. No wait here hides a control
//! (§9): every wait selects on the stop signal or its own deadline, and the
//! reader never awaits a consumer or the Store.

use std::collections::VecDeque;
use std::future::Future;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{sleep_until, timeout_at};

use super::{BoundedBytes, Deadline, InboundBounds, SendOutcome, VendorMessage, WireFailure};
use crate::runtime::{WireCloseReport, WireError, wire_cleanup};
use crate::split::{LineSplitter, Pushed};
use via_host::{ExitReceiver, ProcessControl};
use via_store::{BlobTasks, CommitOutcome, StoreFailureKind};

/// The prefix of an undecoded message VIA keeps (design §7.3).
pub const UNDECODED_BYTES: usize = 64 * 1024;

/// One stdout read (design §8.2).
const READ_BYTES: usize = 64 * 1024;

/// The message queue: at most 1,024 messages (design §8.2, A47) and the
/// connection's staging bytes ([`InboundBounds`]).
const QUEUE_MESSAGES: usize = 1024;

/// A streamed start writes its prompt in slices of at most 16 KiB, each
/// escaped into one reused buffer (design §8.3).
const PROMPT_SLICE: usize = 16 * 1024;

/// A control message is at most 64 KiB (design §8.3); the distinct
/// control messages outstanding on a connection are at most this many
/// bytes in total (runtime §8, C2 §2).
const CONTROL_BYTES: usize = 64 * 1024;

/// The distinct control messages outstanding on a connection, enqueued and
/// not yet answered (runtime §8, C2 §2).
const CONTROL_MESSAGES: usize = 8;

/// `finish` drains until this long before its deadline, then aborts and
/// joins until the deadline (design §8.6).
const FINISH_JOIN: Duration = Duration::from_millis(250);

/// A connection's first failure (design §8.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureCause {
    /// The stdout reader: a full queue is `Overflow`, a message over the
    /// connection's cap `MessageTooLarge`, a pipe read error `Transport`.
    Reader(WireFailure),
    /// A stdin write error; a group kill surfaces as `BrokenPipe`.
    Writer(std::io::ErrorKind),
}

impl FailureCause {
    /// The error a wait on the connection reports for this cause.
    pub fn error(self) -> WireError {
        match self {
            Self::Reader(failure) => WireError::Message(failure),
            Self::Writer(kind) => WireError::Io(std::io::Error::from(kind)),
        }
    }
}

/// The one health state of a connection (design §8.4): the first failure
/// wins; end of stdout is in-band, not a failure.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LatchState {
    /// The first failure, once one happened.
    pub first: Option<FailureCause>,
}

/// One input message for the stdin writer (design §8.3).
pub enum OutboundMessage {
    /// A start streamed from its parts, with no second whole copy: `prefix`,
    /// then `prompt` cut at character boundaries into 16 KiB slices, each
    /// written by `escape` into a reused buffer, then `suffix`. The protocol
    /// encoding is the Route's; Wire only slices and writes.
    Start {
        /// Bytes before the prompt.
        prefix: Vec<u8>,
        /// The prompt, escaped slice by slice.
        prompt: String,
        /// Bytes after the prompt, its LF included.
        suffix: Vec<u8>,
        /// Appends the encoding of one prompt slice to the buffer.
        escape: fn(&str, &mut Vec<u8>),
    },
    /// An interrupt of at most 64 KiB, written between messages. A second
    /// one is coalesced into the first: it is not written and answers
    /// `NotWritten`.
    Interrupt(Vec<u8>),
    /// A distinct control message, written whole between messages and
    /// never coalesced (runtime §8, C2 §2). At most eight are outstanding,
    /// 64 KiB in total; one past either is refused at once with
    /// `NotWritten`, nothing written. Its deadline bounds only the wait for
    /// its first byte, queued or at the writer: one expired before it
    /// answers `NotWritten`, returns its share, is never written, and
    /// stdin stays open; expiry wins over a writable stdin. One started is
    /// written whole, cut only by the connection's stop. A deadline thus
    /// never closes a stdin other sessions may share. A queued message
    /// expires at its deadline whether its write is polled, kept or
    /// dropped: the writer, while it holds stdin, removes it too.
    Control(Vec<u8>),
}

/// How a write is bounded (runtime §4; x.3.2 X0 item 12.2).
#[derive(Clone, Copy, Debug)]
pub enum WriteBounds {
    /// The private-route rule. A data message cut by the deadline ends the
    /// writer and drops stdin; a control message's deadline bounds only the
    /// wait for its first byte; an interrupt is cut by it.
    CutAt(Deadline),
    /// The shared-connection rule, for data and control messages alike:
    /// `start_by` bounds only the wait for the first byte. A withdrawn or
    /// unstarted-expired message is not written and stdin stays open; a
    /// started data message is written whole by `finish_by`, and one cut
    /// there ends the writer as a `CutAt` cut does.
    StartBy {
        /// The first byte's bound.
        start_by: Deadline,
        /// The whole data message's bound, the connection's own far deadline.
        finish_by: Deadline,
    },
}

impl WriteBounds {
    /// The deadline of the first byte, or of an interrupt's whole write.
    fn first_byte(self) -> Deadline {
        match self {
            Self::CutAt(deadline) => deadline,
            Self::StartBy { start_by, .. } => start_by,
        }
    }
}

/// Where one write is (x.3.2 X0 item 12.2). Withdrawal, expiry and the
/// first byte written are decided under the write queue's one lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteState {
    /// Not yet taken by the writer.
    Queued,
    /// Taken by the writer to be written next; nothing written yet, and
    /// still withdrawable.
    Claimed,
    /// A byte was written: the message is finished whole.
    Started,
    /// Answered.
    Done(SendOutcome),
    /// Withdrawn before its first byte: `NotWritten`, stdin open.
    Withdrawn,
    /// Its first byte's deadline passed first: `NotWritten`, stdin open.
    Expired,
}

/// One write's handle for [`WireSender::withdraw`] (x.3.2 X0 item 12.2).
#[derive(Clone)]
pub struct WriteTicket(Arc<JobCell>);

impl WriteTicket {
    /// The write's state now.
    pub fn state(&self) -> WriteState {
        self.0.get()
    }
}

/// A write's state. Changed only under the write queue's lock (the cell's
/// own lock nests inside it), except by the writer for writes that are not
/// withdrawable, which nothing else changes.
struct JobCell {
    state: StdMutex<WriteState>,
    /// A distinct control message or `StartBy` data: [`WireSender::withdraw`]
    /// can take it back before its first byte.
    withdrawable: bool,
}

impl JobCell {
    fn new(withdrawable: bool) -> Arc<Self> {
        Arc::new(Self {
            state: StdMutex::new(WriteState::Queued),
            withdrawable,
        })
    }

    fn get(&self) -> WriteState {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, state: WriteState) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
    }
}

/// A write enqueued to the stdin writer (design §8.3). It is cancel-safe:
/// the caller keeps it pinned while it services other waits, and dropping
/// it never cuts a message the writer started, nor withdraws one.
pub struct PendingWrite {
    ticket: WriteTicket,
    future: Pin<Box<dyn Future<Output = Result<SendOutcome, WireError>> + Send>>,
}

impl PendingWrite {
    /// The write's ticket, for [`WireSender::withdraw`].
    pub fn ticket(&self) -> WriteTicket {
        self.ticket.clone()
    }
}

impl Future for PendingWrite {
    type Output = Result<SendOutcome, WireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

/// A write's answer.
type Reply = oneshot::Sender<Result<SendOutcome, WireError>>;

/// A `CutAt` job on the writer's data channel.
struct DataWrite {
    message: OutboundMessage,
    deadline: Deadline,
    reply: Reply,
    cell: Arc<JobCell>,
}

/// A job on the writer's control queue.
enum Control {
    Interrupt {
        bytes: Vec<u8>,
        deadline: Deadline,
        reply: Reply,
        cell: Arc<JobCell>,
    },
    /// A distinct control message holding its share of the budget.
    Message {
        /// Its state, by which its caller expires or withdraws it.
        cell: Arc<JobCell>,
        bytes: Vec<u8>,
        deadline: Deadline,
        reply: Reply,
    },
    /// Drop stdin once the current message is written.
    Close,
}

/// The ticketed data slot (x.3.2 X0 item 12.2): one `StartBy` data
/// message, kept here until its first byte, while the writer has claimed it
/// too, so it holds the slot's one place.
struct DataSlot {
    cell: Arc<JobCell>,
    /// The message and its answer; with the writer while it is claimed, or
    /// while the writer returns it to the slot.
    job: Option<(OutboundMessage, Reply)>,
    start_by: Deadline,
    finish_by: Deadline,
}

/// The note of the first undecoded message kept, and where it goes.
struct Undecoded {
    folder: PathBuf,
    /// The Store's owned blob steps, which run the file's write.
    tasks: BlobTasks,
    claimed: AtomicBool,
    note: StdMutex<Option<String>>,
}

/// The staging budget of messages read and not yet dropped (runtime §4,
/// design §8.2): 1,024 messages and the connection's staging bytes.
struct Staging {
    messages: AtomicUsize,
    bytes: AtomicUsize,
    /// [`InboundBounds::staging_bytes`].
    limit: usize,
}

impl Staging {
    /// An empty budget of `limit` bytes.
    fn new(limit: usize) -> Self {
        Self {
            messages: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            limit,
        }
    }
}

/// One staged message's share of [`Staging`], returned when the message is
/// dropped, not when it is received (x.3.2 X0 item 12.5).
pub(crate) struct StagingPermit {
    staging: Arc<Staging>,
    bytes: usize,
}

impl StagingPermit {
    /// Takes one message of `bytes`, or `None`, taking nothing, past either
    /// bound.
    fn reserve(staging: &Arc<Staging>, bytes: usize) -> Option<Self> {
        let messages = staging.messages.fetch_add(1, Ordering::AcqRel) + 1;
        let total = staging.bytes.fetch_add(bytes, Ordering::AcqRel) + bytes;
        let permit = Self {
            staging: Arc::clone(staging),
            bytes,
        };
        // Past a bound the permit is dropped at once, returning its share.
        (messages <= QUEUE_MESSAGES && total <= staging.limit).then_some(permit)
    }
}

impl Drop for StagingPermit {
    fn drop(&mut self) {
        self.staging.messages.fetch_sub(1, Ordering::AcqRel);
        self.staging.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Stdout admission (x.3.2 X0 item 13.1): the reader admits each complete
/// message under this lock; the first seal stops admission, and later
/// messages are discarded and counted.
#[derive(Default)]
struct Admission {
    sealed: bool,
    /// Bytes of complete messages discarded after the seal.
    discarded: u64,
}

/// State the connection's halves and tasks share.
struct Shared {
    latch: watch::Sender<LatchState>,
    /// Messages read and not yet dropped, with their bytes.
    staging: Arc<Staging>,
    /// The largest complete stdout message ([`InboundBounds`]).
    message_bytes: usize,
    admission: StdMutex<Admission>,
    /// Stdout ended; set before the reader drops its queue sender.
    eof: AtomicBool,
    /// Stdout ended inside a message (in-band `Unterminated`).
    unterminated: AtomicBool,
    /// Bytes the reader discarded after a failure.
    discarded: AtomicU64,
    /// An interrupt was enqueued (coalescing).
    interrupt_sent: AtomicBool,
    /// A close of stdin was enqueued (coalescing).
    close_sent: AtomicBool,
    /// The writer's queue and the control budget.
    control: WriteQueue,
    undecoded: Undecoded,
}

/// The writer's queue (design §8.3; x.3.2 X0 item 12.2): interrupts,
/// distinct control messages and the close, first in first out, the
/// ticketed data slot, the data holds, and the budget of the distinct
/// control messages outstanding (queued or being written), under one lock.
/// A queued message is owned by whichever comes first of its expiry (by its
/// caller, or by the writer), its withdrawal and the writer's claim, so it
/// is answered and its share returned exactly once (runtime §8, C2 §2).
#[derive(Default)]
struct WriteQueue {
    state: StdMutex<QueueState>,
    /// Wakes the writer, in `next` or in `expire_queued`, never both at
    /// once: a push before it waits leaves a permit, and `next` checks the
    /// queue before it waits.
    ready: Notify,
    /// Wakes a writer waiting for a claimed job's first byte: a withdrawal
    /// or a new hold.
    changed: Notify,
}

#[derive(Default)]
struct QueueState {
    jobs: VecDeque<Control>,
    data: Option<DataSlot>,
    /// Live [`DataHold`]s: while any exists the writer claims no data job.
    holds: usize,
    /// The writer ended: nothing is queued any more.
    closed: bool,
    /// The distinct control messages outstanding and their bytes.
    messages: usize,
    bytes: usize,
}

/// What the writer claimed.
enum Claimed {
    Control(Control),
    Data {
        cell: Arc<JobCell>,
        message: OutboundMessage,
        reply: Reply,
        start_by: Deadline,
        finish_by: Deadline,
    },
}

/// One attempt at a claimed job's first byte, decided under the queue lock.
enum FirstByte {
    /// The attempt wrote, or the pipe failed.
    Wrote(std::io::Result<usize>),
    /// Withdrawn, or its deadline passed, before any byte: `NotWritten`.
    Refused,
    /// A data job met a hold before any byte: back in its slot, `Queued`.
    Unclaimed,
}

impl WriteQueue {
    fn state(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues an interrupt or the close; false once the writer ended.
    fn push(&self, control: Control) -> bool {
        let mut state = self.state();
        if state.closed {
            return false;
        }
        state.jobs.push_back(control);
        drop(state);
        self.ready.notify_one();
        true
    }

    /// Queues a distinct control message with its share of the budget;
    /// false, taking nothing, when it was withdrawn before, would pass
    /// [`CONTROL_MESSAGES`] or [`CONTROL_BYTES`], or the writer ended.
    fn push_message(
        &self,
        cell: &Arc<JobCell>,
        bytes: Vec<u8>,
        deadline: Deadline,
        reply: Reply,
    ) -> bool {
        let mut state = self.state();
        if cell.get() != WriteState::Queued {
            return false;
        }
        let total = state.bytes.saturating_add(bytes.len());
        if state.closed || state.messages >= CONTROL_MESSAGES || total > CONTROL_BYTES {
            cell.set(WriteState::Done(SendOutcome::NotWritten));
            return false;
        }
        state.messages += 1;
        state.bytes = total;
        state.jobs.push_back(Control::Message {
            cell: Arc::clone(cell),
            bytes,
            deadline,
            reply,
        });
        drop(state);
        self.ready.notify_one();
        true
    }

    /// Puts a `StartBy` data message in the slot; false, taking nothing,
    /// when it was withdrawn before, the slot is taken, or the writer ended.
    fn push_data(
        &self,
        cell: &Arc<JobCell>,
        message: OutboundMessage,
        (start_by, finish_by): (Deadline, Deadline),
        reply: Reply,
    ) -> bool {
        let mut state = self.state();
        if cell.get() != WriteState::Queued {
            return false;
        }
        if state.closed || state.data.is_some() {
            cell.set(WriteState::Done(SendOutcome::NotWritten));
            return false;
        }
        state.data = Some(DataSlot {
            cell: Arc::clone(cell),
            job: Some((message, reply)),
            start_by,
            finish_by,
        });
        drop(state);
        self.ready.notify_one();
        true
    }

    /// Takes back the queued or claimed write `cell` before its first byte
    /// (x.3.2 X0 item 12.2): it is `Withdrawn`, answers `NotWritten` and
    /// returns its share; stdin stays open. A started, answered, expired or
    /// not withdrawable write is left as it is and its state returned.
    fn withdraw(&self, cell: &Arc<JobCell>) -> WriteState {
        let mut state = self.state();
        let now = cell.get();
        if !cell.withdrawable || !matches!(now, WriteState::Queued | WriteState::Claimed) {
            return now;
        }
        let at = state.jobs.iter().position(
            |job| matches!(job, Control::Message { cell: queued, .. } if Arc::ptr_eq(queued, cell)),
        );
        let mut answer = None;
        if let Some(Control::Message { bytes, reply, .. }) = at.and_then(|at| state.jobs.remove(at))
        {
            state.release(bytes.len());
            answer = Some(reply);
        }
        if state
            .data
            .as_ref()
            .is_some_and(|slot| Arc::ptr_eq(&slot.cell, cell))
        {
            answer = state
                .data
                .take()
                .and_then(|slot| slot.job)
                .map(|(_, reply)| reply);
        }
        // Not yet queued, claimed, or on its way back to the slot: whoever
        // holds it next sees the state and answers.
        cell.set(WriteState::Withdrawn);
        drop(state);
        if let Some(reply) = answer {
            let _ = reply.send(Ok(SendOutcome::NotWritten));
        }
        self.changed.notify_waiters();
        WriteState::Withdrawn
    }

    /// Its caller's first-byte deadline passed: expires the write `cell` if
    /// the writer has not claimed it, returning its share; true when it did.
    fn expire(&self, cell: &Arc<JobCell>) -> bool {
        let mut state = self.state();
        if cell.get() != WriteState::Queued {
            return false;
        }
        let at = state.jobs.iter().position(
            |job| matches!(job, Control::Message { cell: queued, .. } if Arc::ptr_eq(queued, cell)),
        );
        if let Some(Control::Message { bytes, .. }) = at.and_then(|at| state.jobs.remove(at)) {
            state.release(bytes.len());
        } else if state
            .data
            .as_ref()
            .is_some_and(|slot| Arc::ptr_eq(&slot.cell, cell))
        {
            state.data = None;
        } else {
            return false;
        }
        cell.set(WriteState::Expired);
        true
    }

    /// Claims the writer's next job under the lock: any control job first,
    /// then the slot's data only while no hold exists.
    fn claim(&self) -> Option<Claimed> {
        let mut state = self.state();
        if let Some(job) = state.jobs.pop_front() {
            match &job {
                Control::Interrupt { cell, .. } | Control::Message { cell, .. } => {
                    cell.set(WriteState::Claimed);
                }
                Control::Close => {}
            }
            return Some(Claimed::Control(job));
        }
        if state.holds > 0 {
            return None;
        }
        let slot = state.data.as_mut()?;
        let (message, reply) = slot.job.take()?;
        // Test builds: between the empty-holds check and the claim, inside
        // the lock, so no hold can be taken here.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit("wire.queue.claim");
        slot.cell.set(WriteState::Claimed);
        Some(Claimed::Data {
            cell: Arc::clone(&slot.cell),
            message,
            reply,
            start_by: slot.start_by,
            finish_by: slot.finish_by,
        })
    }

    /// The writer's next job, in queue order; queued deadlines are swept
    /// while it waits, so a data message no hold lets through still expires.
    async fn next(&self) -> Claimed {
        loop {
            let earliest = self.take_expired();
            if let Some(claimed) = self.claim() {
                return claimed;
            }
            let pushed = self.ready.notified();
            match earliest {
                Some(earliest) => {
                    tokio::select! {
                        () = sleep_until(earliest) => {}
                        () = pushed => {}
                    }
                }
                None => pushed.await,
            }
        }
    }

    /// Services queued deadlines while the writer holds stdin; never
    /// resolves. Each queued message whose deadline passed is removed, its
    /// share returned and `NotWritten` answered, to nobody if its caller is
    /// gone; then it waits for the earliest deadline left, or a push.
    async fn expire_queued(&self) -> std::convert::Infallible {
        loop {
            let earliest = self.take_expired();
            // A push consumes the writer's wake; `next` checks the queue
            // before it waits, so nothing is lost.
            let pushed = self.ready.notified();
            match earliest {
                Some(earliest) => {
                    tokio::select! {
                        () = sleep_until(earliest) => {}
                        () = pushed => {}
                    }
                }
                None => pushed.await,
            }
        }
    }

    /// Removes, under the lock, each queued message whose first-byte
    /// deadline passed, returning its share, and answers it `NotWritten`;
    /// the earliest deadline left.
    fn take_expired(&self) -> Option<tokio::time::Instant> {
        let now = tokio::time::Instant::now();
        let mut expired = Vec::new();
        let mut earliest: Option<tokio::time::Instant> = None;
        let mut sooner = |at: tokio::time::Instant| {
            earliest = Some(earliest.map_or(at, |earliest| earliest.min(at)));
        };
        let mut state = self.state();
        let mut kept = VecDeque::with_capacity(state.jobs.len());
        while let Some(job) = state.jobs.pop_front() {
            match job {
                Control::Message {
                    cell,
                    bytes,
                    deadline,
                    reply,
                } if deadline.instant() <= now => {
                    state.release(bytes.len());
                    cell.set(WriteState::Expired);
                    expired.push(reply);
                }
                Control::Message { deadline, .. } => {
                    sooner(deadline.instant());
                    kept.push_back(job);
                }
                job @ (Control::Interrupt { .. } | Control::Close) => kept.push_back(job),
            }
        }
        state.jobs = kept;
        let queued = state
            .data
            .as_ref()
            .filter(|slot| slot.cell.get() == WriteState::Queued)
            .map(|slot| slot.start_by.instant());
        match queued {
            Some(at) if at <= now => {
                if let Some(slot) = state.data.take() {
                    slot.cell.set(WriteState::Expired);
                    // A job on its way back is answered by the writer.
                    expired.extend(slot.job.map(|(_, reply)| reply));
                }
            }
            Some(at) => sooner(at),
            None => {}
        }
        drop(state);
        for reply in expired {
            let _ = reply.send(Ok(SendOutcome::NotWritten));
        }
        earliest
    }

    /// One attempt at the claimed job `cell`'s first byte, under the lock
    /// (x.3.2 X0 item 12.2, F1): refused once withdrawn or past `deadline`;
    /// a data job returns to its slot while a hold exists; otherwise one
    /// non-blocking `poll_write`, and a byte written makes it `Started`
    /// before the lock is released.
    fn attempt_first<W: AsyncWrite + Unpin>(
        &self,
        cx: &mut Context<'_>,
        (stdin, chunk): (&mut W, &[u8]),
        cell: &Arc<JobCell>,
        (deadline, data): (tokio::time::Instant, bool),
    ) -> Poll<FirstByte> {
        let mut state = self.state();
        let slot_is_job = |state: &QueueState| {
            state
                .data
                .as_ref()
                .is_some_and(|slot| Arc::ptr_eq(&slot.cell, cell))
        };
        if cell.get() != WriteState::Claimed {
            return Poll::Ready(FirstByte::Refused);
        }
        if tokio::time::Instant::now() >= deadline {
            cell.set(WriteState::Expired);
            if slot_is_job(&state) {
                state.data = None;
            }
            return Poll::Ready(FirstByte::Refused);
        }
        if data && state.holds > 0 {
            cell.set(WriteState::Queued);
            return Poll::Ready(FirstByte::Unclaimed);
        }
        match Pin::new(stdin).poll_write(cx, chunk) {
            Poll::Ready(Ok(count)) if count > 0 => {
                cell.set(WriteState::Started);
                if slot_is_job(&state) {
                    state.data = None;
                }
                Poll::Ready(FirstByte::Wrote(Ok(count)))
            }
            Poll::Ready(written) => Poll::Ready(FirstByte::Wrote(written)),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Returns an unclaimed data job to its slot; its answer back when it
    /// was withdrawn or expired meanwhile, for the writer to give.
    fn unclaim(
        &self,
        cell: &Arc<JobCell>,
        message: OutboundMessage,
        reply: Reply,
    ) -> Option<Reply> {
        let mut state = self.state();
        let slot = state
            .data
            .as_mut()
            .filter(|slot| Arc::ptr_eq(&slot.cell, cell) && cell.get() == WriteState::Queued);
        match slot {
            Some(slot) => {
                slot.job = Some((message, reply));
                drop(state);
                self.ready.notify_one();
                None
            }
            None => Some(reply),
        }
    }

    /// A [`DataHold`] is taken.
    fn hold(&self) {
        self.state().holds += 1;
        self.changed.notify_waiters();
    }

    /// A [`DataHold`] is dropped: the writer may claim data again.
    fn unhold(&self) {
        let mut state = self.state();
        state.holds = state.holds.saturating_sub(1);
        drop(state);
        self.ready.notify_one();
    }

    /// Returns the share of a message the writer resolved.
    fn release(&self, length: usize) {
        self.state().release(length);
    }

    /// The writer ended: nothing more is queued. Takes every queued job,
    /// each message's share returned, and the slot's data.
    fn close(&self) -> (VecDeque<Control>, Option<DataSlot>) {
        let mut state = self.state();
        state.closed = true;
        let jobs = std::mem::take(&mut state.jobs);
        for job in &jobs {
            if let Control::Message { bytes, .. } = job {
                state.release(bytes.len());
            }
        }
        (jobs, state.data.take())
    }
}

impl QueueState {
    fn release(&mut self, length: usize) {
        self.messages = self.messages.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(length);
    }
}

/// Holds the writer off data messages it has not started (x.3.2 X0 item
/// 12.3): while any hold exists it claims no data job, and returns a
/// claimed one to its slot before its first byte. Dropping it lets data
/// through again.
pub struct DataHold(Arc<Shared>);

impl Drop for DataHold {
    fn drop(&mut self) {
        self.0.control.unhold();
    }
}

/// What [`WireMessages::drain_admitted`] yields (x.3.2 X0 item 13.1).
pub enum Admitted {
    /// A complete message admitted before the seal.
    Message(VendorMessage),
    /// The admitted prefix is over.
    Boundary {
        /// A lower bound on bytes read and not delivered, as of this
        /// boundary: complete messages the reader split after the seal,
        /// plus whole reads it skipped after its own failure. It omits the
        /// bytes of the read that failed it (the refused or oversized
        /// message, the partial assembly and the rest of that read buffer),
        /// so it can be zero although output was lost. A later boundary
        /// may report more, as the reader goes on counting until EOF.
        discarded_bytes: u64,
    },
}

impl Shared {
    /// Latches `cause` unless a failure came first; true when it won. A
    /// reader failure also stops admission, as a seal does (x.3.2 X0 item
    /// 13.1): its reader admits nothing more.
    fn fail(&self, cause: FailureCause) -> bool {
        if matches!(cause, FailureCause::Reader(_)) {
            self.seal();
        }
        self.latch.send_if_modified(|state| {
            if state.first.is_none() {
                state.first = Some(cause);
                true
            } else {
                false
            }
        })
    }

    /// Stops stdout admission; idempotent (x.3.2 X0 item 13.1).
    fn seal(&self) {
        self.admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sealed = true;
    }

    /// The boundary's `discarded_bytes` ([`Admitted::Boundary`]): complete
    /// messages split after the seal and whole reads skipped after the
    /// reader's failure; not the read that failed it.
    fn undelivered(&self) -> u64 {
        let sealed = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .discarded;
        sealed.saturating_add(self.discarded.load(Ordering::Acquire))
    }

    fn failure(&self) -> Option<FailureCause> {
        self.latch.borrow().first
    }

    /// Enqueues a close of stdin once.
    fn request_close(&self) {
        if !self.close_sent.swap(true, Ordering::AcqRel) {
            // A closed queue means the writer already ended and dropped
            // stdin.
            let _ = self.control.push(Control::Close);
        }
    }

    async fn keep_undecoded(&self, bytes: &[u8], what: &str) {
        self.undecoded.keep(bytes, what).await;
    }

    fn take_undecoded(&self) -> Option<String> {
        self.undecoded.take()
    }
}

impl Undecoded {
    fn new(folder: PathBuf, tasks: BlobTasks) -> Self {
        Self {
            folder,
            tasks,
            claimed: AtomicBool::new(false),
            note: StdMutex::new(None),
        }
    }

    /// Writes the first 64 KiB of a message VIA cannot decode to the
    /// folder's `undecoded.bin` (design §7.3): the first message only,
    /// `create_new`, one owned blob step answered within 2 s (coding-style
    /// §5): one that overran stays owned until it ends. `what` describes
    /// the message; the note names the file or the error. Nothing fails
    /// here.
    async fn keep(&self, bytes: &[u8], what: &str) {
        if self.claimed.swap(true, Ordering::AcqRel) {
            return;
        }
        let path = self.folder.join("undecoded.bin");
        let prefix = bytes[..bytes.len().min(UNDECODED_BYTES)].to_vec();
        let kept = prefix.len();
        let target = path.clone();
        let note = match self.tasks.run(move || write_new(&target, &prefix)).await {
            Ok(()) => format!("{what}; first {kept} in {}", path.display()),
            Err(error) => format!("{what}; not saved: {error}"),
        };
        // Test builds: the save's outcome is known and not yet noted.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("wire.undecoded.before_note").await;
        *self.note.lock().unwrap_or_else(PoisonError::into_inner) = Some(note);
    }

    fn take(&self) -> Option<String> {
        self.note
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// One turn's evidence folder on a shared route (x.3.2 X0 item 1.4):
/// `<state>/evidence/<session_id>/<turn>/`, created by
/// [`crate::WireRuntime::turn_folder`]. A connection's own folder is its
/// server's; a message whose correlation names this turn keeps its
/// undecoded prefix here.
pub struct TurnFolder {
    undecoded: Undecoded,
}

impl TurnFolder {
    pub(crate) fn new(folder: PathBuf, tasks: BlobTasks) -> Self {
        Self {
            undecoded: Undecoded::new(folder, tasks),
        }
    }

    /// The folder.
    pub fn path(&self) -> &Path {
        &self.undecoded.folder
    }

    /// Keeps the first 64 KiB of a message Route cannot decode in this
    /// turn's `undecoded.bin` (design §7.3); only the first is kept. Best
    /// effort, bounded by 2 s.
    pub async fn keep_undecoded(&self, bytes: &[u8], what: &str) {
        self.undecoded.keep(bytes, what).await;
    }

    /// The note naming the kept message or why it was not kept, once.
    pub fn take_undecoded(&self) -> Option<String> {
        self.undecoded.take()
    }
}

/// The input half shared by [`WireSender`] and test pipes.
#[derive(Clone)]
pub(crate) struct Io {
    shared: Arc<Shared>,
    data: mpsc::Sender<DataWrite>,
    closed: watch::Receiver<bool>,
}

impl Io {
    /// Enqueues `message` under `bounds` (runtime §4): the write enqueues
    /// when first polled, and its ticket exists at once.
    pub(crate) fn write(&self, message: OutboundMessage, bounds: WriteBounds) -> PendingWrite {
        let withdrawable = match (&message, bounds) {
            (OutboundMessage::Control(_), _)
            | (OutboundMessage::Start { .. }, WriteBounds::StartBy { .. }) => true,
            (OutboundMessage::Start { .. }, WriteBounds::CutAt(_))
            | (OutboundMessage::Interrupt(_), _) => false,
        };
        let cell = JobCell::new(withdrawable);
        let ticket = WriteTicket(Arc::clone(&cell));
        let io = self.clone();
        let deadline = bounds.first_byte();
        let future = Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            let enqueued = match (message, bounds) {
                (start @ OutboundMessage::Start { .. }, WriteBounds::CutAt(deadline)) => io
                    .data
                    .send(DataWrite {
                        message: start,
                        deadline,
                        reply,
                        cell,
                    })
                    .await
                    .is_ok(),
                (
                    start @ OutboundMessage::Start { .. },
                    WriteBounds::StartBy {
                        start_by,
                        finish_by,
                    },
                ) => {
                    if !io
                        .shared
                        .control
                        .push_data(&cell, start, (start_by, finish_by), reply)
                    {
                        return Ok(SendOutcome::NotWritten);
                    }
                    return io.queued_answer(&cell, start_by, answer).await;
                }
                (OutboundMessage::Interrupt(bytes), _) => {
                    if bytes.len() > CONTROL_BYTES
                        || io.shared.interrupt_sent.swap(true, Ordering::AcqRel)
                    {
                        cell.set(WriteState::Done(SendOutcome::NotWritten));
                        return Ok(SendOutcome::NotWritten);
                    }
                    io.shared.control.push(Control::Interrupt {
                        bytes,
                        deadline,
                        reply,
                        cell,
                    })
                }
                (OutboundMessage::Control(bytes), _) => {
                    if !io
                        .shared
                        .control
                        .push_message(&cell, bytes, deadline, reply)
                    {
                        return Ok(SendOutcome::NotWritten);
                    }
                    return io.queued_answer(&cell, deadline, answer).await;
                }
            };
            if !enqueued {
                // The writer ended: nothing of this message was written.
                return Ok(SendOutcome::NotWritten);
            }
            // A writer stopped mid-message answers nothing: bytes may have
            // reached the vendor.
            answer.await.unwrap_or(Ok(SendOutcome::Indeterminate))
        });
        PendingWrite { ticket, future }
    }

    /// A queued control or `StartBy` data message's answer: it expires on
    /// its own first-byte deadline, even while another message holds stdin
    /// or a hold keeps data back, unless the writer claimed it first, which
    /// then answers.
    async fn queued_answer(
        &self,
        cell: &Arc<JobCell>,
        deadline: Deadline,
        mut answer: oneshot::Receiver<Result<SendOutcome, WireError>>,
    ) -> Result<SendOutcome, WireError> {
        tokio::select! {
            biased;
            answered = &mut answer => return answered.unwrap_or(Ok(SendOutcome::Indeterminate)),
            () = sleep_until(deadline.instant()) => {}
        }
        if self.shared.control.expire(cell) {
            return Ok(SendOutcome::NotWritten);
        }
        answer.await.unwrap_or(Ok(SendOutcome::Indeterminate))
    }

    pub(crate) fn withdraw(&self, ticket: WriteTicket) -> WriteState {
        let WriteTicket(cell) = ticket;
        self.shared.control.withdraw(&cell)
    }

    pub(crate) fn hold_data(&self) -> DataHold {
        self.shared.control.hold();
        DataHold(Arc::clone(&self.shared))
    }

    pub(crate) fn seal(&self) {
        self.shared.seal();
    }

    pub(crate) async fn close_input(&self, deadline: Deadline) -> Result<(), WireError> {
        self.shared.request_close();
        let mut closed = self.closed.clone();
        match timeout_at(deadline.instant(), closed.wait_for(|closed| *closed)).await {
            // A writer that ended without the mark dropped stdin with it.
            Ok(_) => Ok(()),
            Err(_) => Err(WireError::Deadline),
        }
    }

    pub(crate) fn failure(&self) -> Option<FailureCause> {
        self.shared.failure()
    }

    pub(crate) fn latch(&self) -> watch::Receiver<LatchState> {
        self.shared.latch.subscribe()
    }

    pub(crate) async fn keep_undecoded(&self, bytes: &[u8], what: &str) {
        self.shared.keep_undecoded(bytes, what).await;
    }

    pub(crate) fn take_undecoded(&self) -> Option<String> {
        self.shared.take_undecoded()
    }

    #[cfg(any(feature = "test-failpoints", feature = "test-support"))]
    pub(crate) fn discarded(&self) -> u64 {
        self.shared.discarded.load(Ordering::Acquire)
    }

    #[cfg(any(feature = "test-failpoints", feature = "test-support"))]
    pub(crate) fn queued_bytes(&self) -> usize {
        self.shared.staging.bytes.load(Ordering::Acquire)
    }
}

/// The clonable control half of a connection (design §8.1): stdin writes,
/// close of input, Host close, vendor exit, health and the undecoded note.
#[derive(Clone)]
pub struct WireSender {
    io: Io,
    process: Arc<Process>,
}

struct Process {
    control: ProcessControl,
    exits: ExitReceiver,
}

impl WireSender {
    /// Enqueues one input message; the returned write resolves once the
    /// writer answered. Under [`WriteBounds::CutAt`] the deadline bounds a
    /// data message's write itself: a message cut short by it closes stdin
    /// and answers `Indeterminate`, or `NotWritten` when nothing of it was
    /// written. A `Control` message differs: see
    /// [`OutboundMessage::Control`]. Under [`WriteBounds::StartBy`] a data
    /// message takes the ticketed data slot, one at a time (a second while
    /// it is taken answers `NotWritten` at once), and follows the control
    /// rule (x.3.2 X0 item 12.2).
    pub fn write(&self, message: OutboundMessage, bounds: WriteBounds) -> PendingWrite {
        self.io.write(message, bounds)
    }

    /// Takes back a control or `StartBy` data write before its first byte:
    /// from `Queued` or `Claimed` it is `Withdrawn`, answers `NotWritten`,
    /// and stdin stays open; a started or answered write, or one not
    /// withdrawable, is left as it is and its state returned (x.3.2 X0
    /// item 12.2).
    pub fn withdraw(&self, ticket: WriteTicket) -> WriteState {
        self.io.withdraw(ticket)
    }

    /// Keeps the writer off unstarted data messages while it lives, so
    /// control messages go first (x.3.2 X0 item 12.3).
    pub fn hold_data(&self) -> DataHold {
        self.io.hold_data()
    }

    /// Synchronous and idempotent: the first call stops admitting stdout
    /// messages; the prefix is kept for [`WireMessages::drain_admitted`].
    /// The failure's disposition is the route's (x.3.2 X0 item 13.1).
    pub fn seal(&self) {
        self.io.seal();
    }

    /// Commits the link of the `running` turn `(session, turn)` to this
    /// connection's shared server anchor, before the turn's first vendor
    /// byte (x.3.2 X0 item 1). A turn-owned connection has none:
    /// `NotCommitted`. A link with no outcome by `deadline` is `Uncertain`.
    pub async fn link_turn(
        &self,
        session: &via_store::SessionId,
        turn: via_store::TurnNumber,
        deadline: Deadline,
    ) -> CommitOutcome<()> {
        match self
            .process
            .control
            .link_turn(session, turn, deadline)
            .await
        {
            Ok(()) => CommitOutcome::Committed(()),
            Err(via_host::HostError::Journal {
                uncertain: true, ..
            }) => CommitOutcome::Uncertain(StoreFailureKind::Write),
            Err(_) => CommitOutcome::NotCommitted(StoreFailureKind::Write),
        }
    }

    /// Closes vendor stdin once the current message is written;
    /// idempotent, and acknowledged after the endpoint dropped. Output
    /// reading and Host supervision go on.
    pub async fn close_input(&self, deadline: Deadline) -> Result<(), WireError> {
        self.io.close_input(deadline).await
    }

    /// Requests Host cleanup through the verified anchor.
    pub async fn close(&self, request: super::CloseRequest) -> WireCloseReport {
        let report = self.process.control.close(request).await;
        WireCloseReport {
            cleanup: wire_cleanup(&report.cleanup),
            vendor_exit: report.vendor_exit,
            forced: report.forced,
            journal_uncertain: report.journal_uncertain,
            stopped_live: report.stopped_live,
        }
    }

    /// Observes Host-confirmed vendor exit without treating a terminal
    /// message as exit proof. A recorded exit is returned without
    /// consulting the daemon force: the caller reads the force after it
    /// (design §6.8).
    pub async fn wait_exit(&self, deadline: Deadline) -> Result<super::ExitReport, WireError> {
        let mut exits = self.process.exits.clone();
        loop {
            // Copied out so no watch guard is held across the test seam's await.
            let recorded = *exits.borrow_and_update();
            if let Some(exit) = recorded {
                // Test builds pause here, exit recorded and not yet returned.
                #[cfg(feature = "test-failpoints")]
                via_store::failpoint::hit_async("wire.exit.observed")
                    .await
                    .map_err(WireError::Io)?;
                return Ok(exit);
            }
            timeout_at(deadline.instant(), exits.changed())
                .await
                .map_err(|_| WireError::Deadline)?
                // Host dropped its exit supervision: transport loss.
                .map_err(|_| WireError::Message(WireFailure::Transport))?;
        }
    }

    /// The connection's first failure, if any.
    pub fn failure(&self) -> Option<FailureCause> {
        self.io.failure()
    }

    /// A receiver of the connection's latch, for waits that select on it.
    pub fn latch(&self) -> watch::Receiver<LatchState> {
        self.io.latch()
    }

    /// Keeps the first 64 KiB of a message Route cannot decode in the turn's
    /// `undecoded.bin` (design §7.3); only the connection's first undecoded
    /// message is kept. Best effort, bounded by 2 s.
    pub async fn keep_undecoded(&self, bytes: &[u8], what: &str) {
        self.io.keep_undecoded(bytes, what).await;
    }

    /// The note naming the kept undecoded message or why it was not kept,
    /// once. Read it after [`WireMessages::finish`], which joins the reader.
    pub fn take_undecoded(&self) -> Option<String> {
        self.io.take_undecoded()
    }
}

/// The unique message half of a connection (design §8.1): the queue, the
/// reader and writer tasks, and the stop signal. It owns the connection's
/// life; [`Self::finish`] is its only normal end.
pub struct WireMessages {
    queue: mpsc::Receiver<VendorMessage>,
    tasks: JoinSet<()>,
    stop: watch::Sender<bool>,
    shared: Arc<Shared>,
    latch: watch::Receiver<LatchState>,
    force: watch::Receiver<Option<tokio::time::Instant>>,
    wake: watch::Receiver<u64>,
    stragglers: Stragglers,
    finished: bool,
    /// A message [`Self::next_message`] dequeued, then withheld because a
    /// failure latched meanwhile: admitted before the seal, it is the
    /// drain's first ([`Self::drain_admitted`]). Test builds also keep a
    /// message here across the `wire.messages.received` pause.
    held: Option<VendorMessage>,
}

impl WireMessages {
    /// The next complete stdout message, `None` at the end of stdout
    /// (design §8.5). It never reads a pipe: the daemon force ends it with
    /// [`WireError::Cancelled`] and Route's wake with [`WireError::Woken`],
    /// losing nothing. After a latched failure it reports that failure and
    /// queued messages are dropped. Stdout ending inside a message is
    /// `Message(UnterminatedMessage)`, its bytes kept in `undecoded.bin`.
    pub async fn next_message(&mut self) -> Result<Option<VendorMessage>, WireError> {
        if let Some(cause) = self.shared.failure() {
            return Err(cause.error());
        }
        // Test builds: a message an earlier call dequeued and was dropped
        // at `wire.messages.received` holding comes first.
        #[cfg(feature = "test-failpoints")]
        if let Some(message) = self.held.take() {
            return Ok(Some(message));
        }
        tokio::select! {
            biased;
            () = cancelled(&mut self.force) => Err(WireError::Cancelled),
            () = woken(&mut self.wake) => Err(WireError::Woken),
            message = self.queue.recv() => {
                // Test builds: a message is dequeued, not yet returned. It
                // waits in `held`, so a call dropped at the pause loses
                // nothing.
                #[cfg(feature = "test-failpoints")]
                let message = {
                    self.held = message;
                    let _ = via_store::failpoint::hit_async("wire.messages.received").await;
                    self.held.take()
                };
                self.received(message)
            }
            cause = latched(&mut self.latch) => Err(cause.error()),
        }
    }

    fn received(
        &mut self,
        message: Option<VendorMessage>,
    ) -> Result<Option<VendorMessage>, WireError> {
        if let Some(cause) = self.shared.failure() {
            // In the queue, so admitted before any seal: kept for the
            // drain, never dropped (x.3.2 X0 item 13.1 exact prefix).
            if message.is_some() {
                self.held = message;
            }
            return Err(cause.error());
        }
        match message {
            Some(message) => Ok(Some(message)),
            None if self.shared.unterminated.load(Ordering::Acquire) => {
                Err(WireError::Message(WireFailure::UnterminatedMessage))
            }
            None if self.shared.eof.load(Ordering::Acquire) => Ok(None),
            // The reader ended without reaching EOF or latching a cause.
            None => {
                let cause = FailureCause::Reader(WireFailure::Transport);
                self.shared.fail(cause);
                Err(self.shared.failure().unwrap_or(cause).error())
            }
        }
    }

    /// After a seal (Route's, or the end of Wire's own reader on its
    /// failure): the complete messages admitted before it, in order, then
    /// [`Admitted::Boundary`]; it never waits for more output (x.3.2 X0
    /// item 13.1). It seals first, so a drain alone also stops admission.
    /// A latched failure does not hide the prefix, nor a message
    /// [`Self::next_message`] withheld from it. Nothing is sealed or
    /// dequeued until the future is polled, and it completes at its first
    /// poll, so a drain dropped unpolled, or losing a `select!`, loses
    /// nothing.
    pub fn drain_admitted(&mut self) -> impl Future<Output = Admitted> + Send + '_ {
        std::future::poll_fn(move |_| {
            self.shared.seal();
            let next = match self
                .held
                .take()
                .map_or_else(|| self.queue.try_recv().ok(), Some)
            {
                Some(message) => Admitted::Message(message),
                None => Admitted::Boundary {
                    discarded_bytes: self.shared.undelivered(),
                },
            };
            std::task::Poll::Ready(next)
        })
    }

    /// Ends the connection under one absolute deadline (design §8.6): the
    /// stop is already ordered (Route's close, Host's kill or vendor exit),
    /// so input is closed; drain until stdout EOF and the writer's end, or
    /// `deadline − 250 ms`; then set the stop signal, abort, and join until
    /// `deadline`. A task still unjoined moves to the runtime, which joins
    /// it as it ends and reports it at shutdown.
    /// A `finish` cancelled before its handoff leaves `finished` unset, so
    /// `Drop` hands the tasks over instead.
    pub async fn finish(mut self, deadline: Deadline) {
        self.shared.request_close();
        let drain_by = deadline
            .instant()
            .checked_sub(FINISH_JOIN)
            .unwrap_or_else(|| deadline.instant());
        let _drained = timeout_at(drain_by, self.stragglers.join_all(&mut self.tasks)).await;
        self.stop.send_replace(true);
        self.tasks.abort_all();
        let _joined = timeout_at(
            deadline.instant(),
            self.stragglers.join_all(&mut self.tasks),
        )
        .await;
        self.stragglers.adopt(std::mem::take(&mut self.tasks));
        self.finished = true;
    }
}

impl Drop for WireMessages {
    /// A connection dropped without [`WireMessages::finish`]: its tasks
    /// are aborted and handed over the same way, and test builds count it.
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.stop.send_replace(true);
        self.tasks.abort_all();
        self.stragglers.adopt(std::mem::take(&mut self.tasks));
        #[cfg(feature = "test-failpoints")]
        {
            FALLBACK_DROPS.fetch_add(1, Ordering::AcqRel);
            let _ = via_store::failpoint::hit("wire.fallback_drop");
        }
    }
}

/// Connections dropped without `finish` in this process (test builds).
#[cfg(feature = "test-failpoints")]
static FALLBACK_DROPS: AtomicUsize = AtomicUsize::new(0);

/// Test builds: how many `WireMessages` were dropped without `finish` in
/// this process (design §8.6). Every normal test expects zero.
#[cfg(feature = "test-failpoints")]
pub fn fallback_drops() -> usize {
    FALLBACK_DROPS.load(Ordering::Acquire)
}

/// Connection tasks that missed their owner's join bound (design §8.6,
/// coding style §5): the runtime keeps them, joins them as they end and
/// reports the rest at shutdown. It also counts connection tasks whose join
/// reported a panic; a task aborted by its owner's bound is the designed
/// end, not a failure.
#[derive(Clone, Default)]
pub(crate) struct Stragglers {
    sets: Arc<StdMutex<Vec<JoinSet<()>>>>,
    failed: Arc<AtomicUsize>,
}

impl Stragglers {
    pub(crate) fn adopt(&self, tasks: JoinSet<()>) {
        let mut sets = self.sets.lock().unwrap_or_else(PoisonError::into_inner);
        if !tasks.is_empty() {
            sets.push(tasks);
        }
        self.reap(&mut sets);
    }

    /// Tasks not yet ended.
    pub(crate) fn pending(&self) -> usize {
        let mut sets = self.sets.lock().unwrap_or_else(PoisonError::into_inner);
        self.reap(&mut sets);
        sets.iter().map(JoinSet::len).sum()
    }

    /// Connection tasks whose join reported a panic.
    pub(crate) fn failed(&self) -> usize {
        self.failed.load(Ordering::Acquire)
    }

    /// Joins every task that ends by `deadline`.
    /// The sets are taken out while they are joined; the guard returns
    /// every unjoined set, also when this future is cancelled.
    pub(crate) async fn join_until(&self, deadline: Deadline) {
        let mut joining = Joining {
            owner: self,
            sets: std::mem::take(&mut *self.sets.lock().unwrap_or_else(PoisonError::into_inner)),
        };
        for set in &mut joining.sets {
            let _joined = timeout_at(deadline.instant(), self.join_all(set)).await;
        }
    }

    /// Joins `tasks` until none is left, counting panics.
    async fn join_all(&self, tasks: &mut JoinSet<()>) {
        while let Some(joined) = tasks.join_next().await {
            self.record(joined);
        }
    }

    fn reap(&self, sets: &mut Vec<JoinSet<()>>) {
        for set in sets.iter_mut() {
            while let Some(joined) = set.try_join_next() {
                self.record(joined);
            }
        }
        sets.retain(|set| !set.is_empty());
    }

    fn record(&self, joined: Result<(), tokio::task::JoinError>) {
        if joined.is_err_and(|error| error.is_panic()) {
            self.failed.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Straggler sets taken out for a join (coding style §5): dropped, on
/// completion or on cancellation, it hands every set with a task left back
/// to its owner.
struct Joining<'a> {
    owner: &'a Stragglers,
    sets: Vec<JoinSet<()>>,
}

impl Drop for Joining<'_> {
    fn drop(&mut self) {
        let mut left = std::mem::take(&mut self.sets);
        left.retain(|set| !set.is_empty());
        self.owner
            .sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(left);
    }
}

/// Both halves of an open connection (design §8.1).
pub struct WireParts {
    /// The clonable control half.
    pub sender: WireSender,
    /// The unique message half.
    pub messages: WireMessages,
}

/// An open private connection; [`Self::into_parts`] splits it.
pub struct WireConnection {
    sender: WireSender,
    messages: WireMessages,
}

impl WireConnection {
    /// Splits the connection into its control and message halves.
    pub fn into_parts(self) -> WireParts {
        WireParts {
            sender: self.sender,
            messages: self.messages,
        }
    }
}

/// The signals a connection's waits select on.
pub(crate) struct Waits {
    pub(crate) force: watch::Receiver<Option<tokio::time::Instant>>,
    pub(crate) wake: watch::Receiver<u64>,
}

/// Starts the reader and writer tasks over a Host-acquired process.
pub(crate) fn open(
    pipes: via_host::OwnedPipes,
    control: ProcessControl,
    exits: ExitReceiver,
    folder: (PathBuf, BlobTasks),
    (waits, bounds): (Waits, InboundBounds),
    stragglers: &Stragglers,
) -> WireConnection {
    let (io, messages) = connect(
        pipes.stdout,
        pipes.stdin,
        folder,
        (waits, bounds),
        stragglers,
    );
    WireConnection {
        sender: WireSender {
            io,
            process: Arc::new(Process { control, exits }),
        },
        messages,
    }
}

/// Starts the reader over `stdout` and the writer over `stdin`, within
/// `bounds`.
pub(crate) fn connect<R, W>(
    stdout: R,
    stdin: W,
    (folder, tasks): (PathBuf, BlobTasks),
    (waits, bounds): (Waits, InboundBounds),
    stragglers: &Stragglers,
) -> (Io, WireMessages)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (latch, latch_rx) = watch::channel(LatchState::default());
    let shared = Arc::new(Shared {
        latch,
        staging: Arc::new(Staging::new(bounds.staging_bytes)),
        message_bytes: bounds.message_bytes,
        admission: StdMutex::default(),
        eof: AtomicBool::new(false),
        unterminated: AtomicBool::new(false),
        discarded: AtomicU64::new(0),
        interrupt_sent: AtomicBool::new(false),
        close_sent: AtomicBool::new(false),
        control: WriteQueue::default(),
        undecoded: Undecoded::new(folder, tasks),
    });
    let (queue_tx, queue) = mpsc::channel(QUEUE_MESSAGES);
    let (data_tx, data_rx) = mpsc::channel(1);
    let (stop, stop_rx) = watch::channel(false);
    let (closed_tx, closed) = watch::channel(false);
    let mut tasks = JoinSet::new();
    tasks.spawn(read_stdout(
        stdout,
        Arc::clone(&shared),
        queue_tx,
        stop_rx.clone(),
    ));
    tasks.spawn(write_stdin(
        stdin,
        Queues {
            data: data_rx,
            shared: Arc::clone(&shared),
        },
        stop_rx,
        closed_tx,
    ));
    let io = Io {
        shared: Arc::clone(&shared),
        data: data_tx,
        closed,
    };
    let messages = WireMessages {
        queue,
        tasks,
        stop,
        shared,
        latch: latch_rx,
        force: waits.force,
        wake: waits.wake,
        stragglers: stragglers.clone(),
        finished: false,
        held: None,
    };
    (io, messages)
}

/// The stdout reader (design §8.2): reads up to 64 KiB into its fixed
/// buffer, splits on LF and queues each complete message with `try_send`,
/// never awaiting a consumer or the Store. A full queue or a message over
/// the connection's cap latches the failure and switches to discard mode: read to EOF,
/// count the bytes, keep nothing, so the vendor never blocks on its pipe.
/// An oversized message's prefix save runs beside those reads, owned by
/// this loop, and is awaited before EOF is recorded.
async fn read_stdout<R: AsyncRead + Unpin>(
    mut stdout: R,
    shared: Arc<Shared>,
    queue: mpsc::Sender<VendorMessage>,
    mut stop: watch::Receiver<bool>,
) {
    let mut buffer = vec![0_u8; READ_BYTES];
    let mut splitter = LineSplitter::within(shared.message_bytes);
    let mut discard = false;
    // The oversized prefix's save, bounded by its blob step's 2 s. It is
    // polled with the reads, so discard reads go on while it waits. A stop
    // drops it, as the abort that follows the stop would; the blob step
    // itself stays owned by the Store pool.
    let mut saving: Option<Pin<Box<dyn Future<Output = ()> + Send>>> = None;
    loop {
        let read = tokio::select! {
            biased;
            () = stopped(&mut stop) => return,
            () = async {
                match saving.as_mut() {
                    Some(save) => save.await,
                    None => std::future::pending().await,
                }
            }, if saving.is_some() => {
                saving = None;
                continue;
            }
            read = stdout.read(&mut buffer) => read,
        };
        let count = match read {
            Ok(0) => break,
            Ok(count) => count,
            Err(_) => {
                shared.fail(FailureCause::Reader(WireFailure::Transport));
                if let Some(save) = saving {
                    save.await;
                }
                return;
            }
        };
        if discard {
            shared
                .discarded
                .fetch_add(u64::try_from(count).unwrap_or(u64::MAX), Ordering::AcqRel);
            continue;
        }
        // A refusal or an oversized message switches to discard mode
        // without counting this read: `discarded_bytes` is a lower bound.
        match splitter.push(&buffer[..count], |message| {
            enqueue(&shared, &queue, message)
        }) {
            Pushed::Consumed => {}
            Pushed::Refused => discard = true,
            Pushed::TooLarge(prefix) => {
                discard = true;
                let cause = FailureCause::Reader(WireFailure::MessageTooLarge);
                if shared.fail(cause) {
                    let what = format!("vendor message over the {} byte cap", shared.message_bytes);
                    let shared = Arc::clone(&shared);
                    saving = Some(Box::pin(async move {
                        shared.keep_undecoded(&prefix, &what).await;
                    }));
                }
            }
        }
    }
    // Finalization names `undecoded.bin` from the note: settle it first.
    if let Some(save) = saving {
        save.await;
    }
    if !discard && let Some(tail) = splitter.finish() {
        let length = tail.len();
        shared
            .keep_undecoded(
                &tail,
                &format!("unterminated vendor message: {length} bytes"),
            )
            .await;
        shared.unterminated.store(true, Ordering::Release);
    }
    shared.eof.store(true, Ordering::Release);
}

/// Admits `message` under the admission lock (x.3.2 X0 item 13.1): after
/// a seal it is discarded and counted, and reading goes on. Otherwise it
/// takes its staging permit, then is `try_send`ed; a full staging budget
/// or queue latches `Overflow`. False stops the splitting.
fn enqueue(shared: &Shared, queue: &mpsc::Sender<VendorMessage>, message: Vec<u8>) -> bool {
    let length = message.len();
    let mut admission = shared
        .admission
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if admission.sealed {
        admission.discarded = admission
            .discarded
            .saturating_add(u64::try_from(length).unwrap_or(u64::MAX));
        return true;
    }
    let cause = match BoundedBytes::try_from_message_within(message, shared.message_bytes) {
        Ok(bounded) => {
            let sent = StagingPermit::reserve(&shared.staging, length).is_some_and(|permit| {
                queue
                    .try_send(VendorMessage::staged(bounded, permit))
                    .is_ok()
            });
            // A closed queue means the consumer is gone: only discard.
            if sent || queue.is_closed() {
                return sent;
            }
            WireFailure::Overflow
        }
        Err(_) => WireFailure::MessageTooLarge,
    };
    // `fail` seals, which takes this lock.
    drop(admission);
    shared.fail(FailureCause::Reader(cause));
    false
}

/// The writer's two queues: the data channel and the shared control queue.
struct Queues {
    data: mpsc::Receiver<DataWrite>,
    shared: Arc<Shared>,
}

/// A writer aborted mid-wait still closes the control queue, so no queued
/// job outlives it: each one's answer is dropped (`Indeterminate`), as its
/// channel's would be, and each message's share returns.
impl Drop for Queues {
    fn drop(&mut self) {
        drop(self.shared.control.close());
    }
}

/// One message for the writer and where its answer goes.
struct Job {
    message: OutboundMessage,
    /// A `CutAt` data message's cut, an interrupt's cut, or a whole
    /// message's first-byte bound.
    deadline: Deadline,
    reply: Reply,
    cell: Arc<JobCell>,
    start: bool,
    /// A distinct control message's budgeted bytes, returned before its
    /// answer.
    budgeted: Option<usize>,
    /// A distinct control message or `StartBy` data: its first byte is
    /// decided under the queue lock, and once started it is written whole.
    whole: Option<Whole>,
}

/// How a whole message is finished once started.
#[derive(Clone, Copy)]
enum Whole {
    /// A control message: cut only by the connection's stop.
    Control,
    /// `StartBy` data: by `finish_by`, the connection's own far deadline.
    Data { finish_by: Deadline },
}

impl Job {
    /// The writer's job for a claim; `None` for the close.
    fn claimed(claimed: Claimed) -> Option<Self> {
        Some(match claimed {
            Claimed::Control(Control::Interrupt {
                bytes,
                deadline,
                reply,
                cell,
            }) => Self {
                message: OutboundMessage::Interrupt(bytes),
                deadline,
                reply,
                cell,
                start: false,
                budgeted: None,
                whole: None,
            },
            Claimed::Control(Control::Message {
                cell,
                bytes,
                deadline,
                reply,
            }) => Self {
                budgeted: Some(bytes.len()),
                message: OutboundMessage::Control(bytes),
                deadline,
                reply,
                cell,
                start: false,
                whole: Some(Whole::Control),
            },
            Claimed::Control(Control::Close) => return None,
            Claimed::Data {
                cell,
                message,
                reply,
                start_by,
                finish_by,
            } => Self {
                message,
                deadline: start_by,
                reply,
                cell,
                start: true,
                budgeted: None,
                whole: Some(Whole::Data { finish_by }),
            },
        })
    }

    /// The writer's job for a `CutAt` data message.
    fn cut_at(data: DataWrite) -> Self {
        data.cell.set(WriteState::Claimed);
        Self {
            message: data.message,
            deadline: data.deadline,
            reply: data.reply,
            cell: data.cell,
            start: true,
            budgeted: None,
            whole: None,
        }
    }
}

/// The stdin writer (design §8.3): owns stdin and writes one message at a
/// time, controls first between messages, never interleaving bytes. Every
/// write selects on the stop signal and its deadline; a message not written
/// whole closes stdin, while a whole message refused before its first byte
/// leaves it open. On its end it drops stdin and marks it closed.
async fn write_stdin<W: AsyncWrite + Unpin>(
    mut stdin: W,
    mut queues: Queues,
    mut stop: watch::Receiver<bool>,
    closed: watch::Sender<bool>,
) {
    let mut piece = Vec::new();
    let mut data_open = true;
    loop {
        let job = tokio::select! {
            biased;
            () = stopped(&mut stop) => break,
            claimed = queues.shared.control.next() => match Job::claimed(claimed) {
                Some(job) => job,
                None => break,
            },
            data = queues.data.recv(), if data_open => {
                let Some(data) = data else {
                    data_open = false;
                    continue;
                };
                Job::cut_at(data)
            }
        };
        if !write_job((&mut stdin, &mut stop), &queues.shared, job, &mut piece).await {
            break;
        }
    }
    drop(stdin);
    refuse_queued(&mut queues);
    closed.send_replace(true);
}

/// Writes one job and answers it; false when the writer must end and drop
/// stdin.
async fn write_job<W: AsyncWrite + Unpin>(
    (stdin, stop): (&mut W, &mut watch::Receiver<bool>),
    shared: &Shared,
    job: Job,
    piece: &mut Vec<u8>,
) -> bool {
    let mut writing = Writing {
        stdin,
        stop,
        queue: &shared.control,
        cell: &job.cell,
        deadline: job.deadline,
        whole: job.whole,
        written: false,
        refused: None,
    };
    // While it holds stdin, queued messages still expire.
    let outcome = tokio::select! {
        biased;
        outcome = writing.message(&job.message, piece) => outcome,
        never = shared.control.expire_queued() => match never {},
    };
    let (written, refused) = (writing.written, writing.refused);
    // Resolved, whatever the outcome: its share returns before the answer,
    // so a caller answered may enqueue the next at once.
    if let Some(length) = job.budgeted {
        shared.control.release(length);
    }
    let (answer, go_on) = match outcome {
        Ok(true) => {
            // The whole input message (in S1 first the start carrying the
            // prompt) is in the vendor's stdin.
            #[cfg(feature = "test-failpoints")]
            if job.start
                && let Err(error) = via_store::failpoint::hit_async("wire.prompt.after_write").await
            {
                job.cell.set(WriteState::Done(SendOutcome::Indeterminate));
                let _ = job.reply.send(Err(WireError::Io(error)));
                return false;
            }
            let _ = job.start;
            (Ok(SendOutcome::Written), true)
        }
        // A hold came before the data's first byte: back to its slot.
        Ok(false) if refused == Some(Refusal::Unclaimed) => {
            if let Some(reply) = shared.control.unclaim(&job.cell, job.message, job.reply) {
                let _ = reply.send(Ok(SendOutcome::NotWritten));
            }
            return true;
        }
        // Withdrawn, or its deadline passed, before its first byte:
        // refused, and stdin stays open for the messages after it.
        Ok(false) if refused == Some(Refusal::Refused) => {
            let _ = job.reply.send(Ok(SendOutcome::NotWritten));
            return true;
        }
        Ok(false) if written => (Ok(SendOutcome::Indeterminate), false),
        Ok(false) => (Ok(SendOutcome::NotWritten), false),
        Err(error) => {
            shared.fail(FailureCause::Writer(error.kind()));
            (Err(WireError::Io(error)), false)
        }
    };
    job.cell.set(WriteState::Done(match &answer {
        Ok(outcome) => *outcome,
        Err(_) => SendOutcome::Indeterminate,
    }));
    let _ = job.reply.send(answer);
    go_on
}

/// The writer's end: nothing queued after it is written, so each queued
/// write answers `NotWritten` and a control message returns its share.
fn refuse_queued(queues: &mut Queues) {
    let refuse = |cell: &JobCell, reply: Reply| {
        cell.set(WriteState::Done(SendOutcome::NotWritten));
        let _ = reply.send(Ok(SendOutcome::NotWritten));
    };
    queues.data.close();
    let (jobs, slot) = queues.shared.control.close();
    for control in jobs {
        match control {
            Control::Interrupt { reply, cell, .. } | Control::Message { reply, cell, .. } => {
                refuse(&cell, reply);
            }
            Control::Close => {}
        }
    }
    if let Some(DataSlot {
        cell,
        job: Some((_, reply)),
        ..
    }) = slot
    {
        refuse(&cell, reply);
    }
    while let Ok(data) = queues.data.try_recv() {
        refuse(&data.cell, data.reply);
    }
}

/// Why a whole message ended before its first byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Refusal {
    /// Withdrawn or expired: `NotWritten`, stdin open.
    Refused,
    /// A data job met a hold: back in its slot.
    Unclaimed,
}

/// One message being written.
struct Writing<'a, W> {
    stdin: &'a mut W,
    stop: &'a mut watch::Receiver<bool>,
    queue: &'a WriteQueue,
    cell: &'a Arc<JobCell>,
    deadline: Deadline,
    /// A whole message: the deadline bounds only the wait for its first
    /// byte, decided under the queue lock, and one started is written
    /// whole, so a deadline never leaves a partial line nor closes a stdin
    /// other sessions may share.
    whole: Option<Whole>,
    /// Some byte of the message was written.
    written: bool,
    /// A whole message ended before its first byte.
    refused: Option<Refusal>,
}

impl<W: AsyncWrite + Unpin> Writing<'_, W> {
    /// Writes the whole message: true once complete, false when the stop
    /// signal or the deadline cut it, or it was refused before its first
    /// byte.
    async fn message(
        &mut self,
        message: &OutboundMessage,
        piece: &mut Vec<u8>,
    ) -> std::io::Result<bool> {
        match message {
            OutboundMessage::Interrupt(bytes) | OutboundMessage::Control(bytes) => {
                self.put(bytes).await
            }
            OutboundMessage::Start {
                prefix,
                prompt,
                suffix,
                escape,
            } => {
                if !self.put(prefix).await? {
                    return Ok(false);
                }
                let mut start = 0;
                while start < prompt.len() {
                    let mut end = (start + PROMPT_SLICE).min(prompt.len());
                    while !prompt.is_char_boundary(end) {
                        end -= 1;
                    }
                    piece.clear();
                    escape(&prompt[start..end], piece);
                    if !self.put(piece).await? {
                        return Ok(false);
                    }
                    start = end;
                }
                self.put(suffix).await
            }
        }
    }

    async fn put(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        let mut offset = 0;
        while offset < bytes.len() {
            // A pipe write is cancel-safe: a cancelled one wrote nothing.
            let (whole, written) = (self.whole, self.written);
            let (deadline, stdin, chunk) =
                (self.deadline.instant(), &mut *self.stdin, &bytes[offset..]);
            let (queue, cell) = (self.queue, self.cell);
            let pending = async move {
                match (whole, written) {
                    (Some(whole), false) => {
                        let data = matches!(whole, Whole::Data { .. });
                        match first_byte(queue, (stdin, chunk), cell, (deadline, data)).await {
                            FirstByte::Wrote(write) => Ok(Some(write)),
                            FirstByte::Refused => Err(Refusal::Refused),
                            FirstByte::Unclaimed => Err(Refusal::Unclaimed),
                        }
                    }
                    (Some(Whole::Data { finish_by }), true) => {
                        Ok(timeout_at(finish_by.instant(), stdin.write(chunk))
                            .await
                            .ok())
                    }
                    (Some(Whole::Control), true) => Ok(Some(stdin.write(chunk).await)),
                    (None, _) => Ok(timeout_at(deadline, stdin.write(chunk)).await.ok()),
                }
            };
            let write = tokio::select! {
                biased;
                () = stopped(self.stop) => return Ok(false),
                write = pending => write,
            };
            match write {
                Err(refusal) => {
                    self.refused = Some(refusal);
                    return Ok(false);
                }
                Ok(None | Some(Ok(0))) => return Ok(false),
                Ok(Some(Ok(count))) => {
                    if !self.written && self.whole.is_none() {
                        self.cell.set(WriteState::Started);
                    }
                    self.written = true;
                    offset += count;
                }
                Ok(Some(Err(error))) => return Err(error),
            }
        }
        Ok(true)
    }
}

/// A whole message's first byte (x.3.2 X0 item 12.2): each attempt is
/// decided under the queue lock ([`WriteQueue::attempt_first`]); between
/// attempts it waits for stdin, a withdrawal or hold, or its deadline, so
/// an expired or withdrawn message never starts, though stdin is writable.
async fn first_byte<W: AsyncWrite + Unpin>(
    queue: &WriteQueue,
    (stdin, chunk): (&mut W, &[u8]),
    cell: &Arc<JobCell>,
    (deadline, data): (tokio::time::Instant, bool),
) -> FirstByte {
    loop {
        let changed = queue.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let attempt = std::future::poll_fn(|cx| {
            queue.attempt_first(cx, (&mut *stdin, chunk), cell, (deadline, data))
        });
        tokio::select! {
            biased;
            first = attempt => return first,
            () = changed => {}
            () = sleep_until(deadline) => {}
        }
    }
}

/// Resolves once the stop signal is set, or its sender is gone.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

/// Resolves on the first latched failure; never when the latch is gone.
async fn latched(latch: &mut watch::Receiver<LatchState>) -> FailureCause {
    // The watch guard is dropped before any further await.
    let first = latch
        .wait_for(|state| state.first.is_some())
        .await
        .map(|state| state.first);
    match first {
        Ok(first) => first.unwrap_or(FailureCause::Reader(WireFailure::Transport)),
        Err(_) => std::future::pending().await,
    }
}

/// Resolves on the next change of Route's wake; never once its sender is gone.
async fn woken(wake: &mut watch::Receiver<u64>) {
    if wake.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Resolves once `cancel` is set; never when its sender is gone unset.
pub(crate) async fn cancelled(cancel: &mut watch::Receiver<Option<tokio::time::Instant>>) {
    if cancel.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Creates `path` (new, 0600) holding `bytes`, then syncs it and its
/// folder, so the failure message may name it (coding style §7 "Write
/// order"). `create_new` is `O_EXCL`, which never follows a symlink.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let folder = path
        .parent()
        .ok_or_else(|| std::io::Error::other("undecoded.bin has no folder"))?;
    std::fs::File::open(folder)?.sync_all()
}

/// Test builds: a connection over any pipes, without Host (design §13.1).
#[cfg(any(feature = "test-failpoints", feature = "test-support"))]
pub mod testing {
    use std::path::PathBuf;

    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::sync::watch;

    use super::{
        BlobTasks, DataHold, Deadline, FailureCause, InboundBounds, Io, OutboundMessage,
        PendingWrite, Stragglers, Waits, WireError, WireMessages, WriteBounds, WriteState,
        WriteTicket, connect,
    };

    /// The Store a layer above opens for a test runtime of its own (x.3.2
    /// X4): only Wire and Host may name the Store in production, so the
    /// test builds of the layers above reach it here, never through a
    /// dependency of their own. Its owner keeps it open while the
    /// runtime's tasks run.
    pub use via_store::Store;

    /// A connection's message half and its input, over test pipes.
    pub struct TestPipes {
        /// The unique message half.
        pub messages: WireMessages,
        /// The input half and the stragglers of the connection's runtime.
        pub input: TestInput,
    }

    /// The input half of test pipes.
    pub struct TestInput {
        io: Io,
        stragglers: Stragglers,
        tasks: BlobTasks,
    }

    /// Starts a connection's tasks over `stdout` and `stdin`, keeping any
    /// undecoded message in `folder`, within the default bounds.
    pub fn pipes<R, W>(stdout: R, stdin: W, folder: PathBuf) -> TestPipes
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        pipes_within(stdout, stdin, folder, InboundBounds::DEFAULT)
    }

    /// [`pipes`] within `bounds`.
    pub fn pipes_within<R, W>(
        stdout: R,
        stdin: W,
        folder: PathBuf,
        bounds: InboundBounds,
    ) -> TestPipes
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let stragglers = Stragglers::default();
        // Senders dropped: neither force nor wake ever fires.
        let waits = Waits {
            force: watch::channel(None).1,
            wake: watch::channel(0).1,
        };
        let tasks = BlobTasks::default();
        let (io, messages) = connect(
            stdout,
            stdin,
            (folder, tasks.clone()),
            (waits, bounds),
            &stragglers,
        );
        TestPipes {
            messages,
            input: TestInput {
                io,
                stragglers,
                tasks,
            },
        }
    }

    impl TestInput {
        /// See `WireSender::write`, under [`WriteBounds::CutAt`].
        pub fn write(&self, message: OutboundMessage, deadline: Deadline) -> PendingWrite {
            self.io.write(message, WriteBounds::CutAt(deadline))
        }

        /// See `WireSender::write`.
        pub fn write_bounded(&self, message: OutboundMessage, bounds: WriteBounds) -> PendingWrite {
            self.io.write(message, bounds)
        }

        /// See `WireSender::withdraw`.
        pub fn withdraw(&self, ticket: WriteTicket) -> WriteState {
            self.io.withdraw(ticket)
        }

        /// See `WireSender::hold_data`.
        pub fn hold_data(&self) -> DataHold {
            self.io.hold_data()
        }

        /// See `WireSender::seal`.
        pub fn seal(&self) {
            self.io.seal();
        }

        /// See `WireSender::close_input`.
        pub async fn close_input(&self, deadline: Deadline) -> Result<(), WireError> {
            self.io.close_input(deadline).await
        }

        /// See `WireSender::failure`.
        pub fn failure(&self) -> Option<FailureCause> {
            self.io.failure()
        }

        /// See `WireSender::take_undecoded`.
        pub fn take_undecoded(&self) -> Option<String> {
            self.io.take_undecoded()
        }

        /// Bytes the reader discarded after a failure.
        pub fn discarded(&self) -> u64 {
            self.io.discarded()
        }

        /// Bytes of messages the reader queued and nobody received yet.
        pub fn queued_bytes(&self) -> usize {
            self.io.queued_bytes()
        }

        /// Owned blob steps still running, such as a held `undecoded.bin`
        /// write.
        pub fn blob_tasks(&self) -> usize {
            self.tasks.outstanding()
        }

        /// Tasks handed to the runtime and not yet ended.
        pub fn stragglers(&self) -> usize {
            self.stragglers.pending()
        }

        /// Connection tasks whose join reported a panic.
        pub fn failed_joins(&self) -> usize {
            self.stragglers.failed()
        }

        /// Joins handed-over tasks that end by `deadline`.
        pub async fn join_stragglers(&self, deadline: Deadline) {
            self.stragglers.join_until(deadline).await;
        }
    }
}

#[cfg(test)]
mod undecoded_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A private scratch folder, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> std::io::Result<Scratch> {
        let dir = std::env::temp_dir().join(format!(
            "via-wire-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir(&dir)?;
        Ok(Scratch(dir))
    }

    /// Coding style §7 "Write order": the file and its folder are synced
    /// before the failure note names the file. A folder that can be written
    /// but not opened for its sync makes the save fail.
    #[test]
    #[expect(
        clippy::print_stderr,
        reason = "a skipped check under root is reported"
    )]
    fn a_saved_undecoded_file_is_synced_with_its_folder() -> std::io::Result<()> {
        let folder = scratch("synced")?;
        let saved = folder.0.join("undecoded.bin");
        write_new(&saved, b"head")?;
        assert_eq!(std::fs::read(&saved)?, b"head");
        std::fs::remove_file(&saved)?;
        // Write and search only: the file can be created, the folder not
        // opened to sync it.
        std::fs::set_permissions(&folder.0, std::fs::Permissions::from_mode(0o300))?;
        // Root (CAP_DAC_OVERRIDE) opens the folder anyway: the premise fails.
        let bypassed = std::fs::File::open(&folder.0).is_ok();
        let unsynced = write_new(&saved, b"head");
        std::fs::set_permissions(&folder.0, std::fs::Permissions::from_mode(0o700))?;
        if bypassed {
            eprintln!("skipped: this process bypasses file permissions (root)");
        } else {
            assert!(unsynced.is_err(), "an unsynced folder was reported saved");
        }
        Ok(())
    }
}
