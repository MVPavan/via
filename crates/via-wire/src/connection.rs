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

use super::{BoundedBytes, Deadline, SendOutcome, VendorMessage, WireFailure};
use crate::runtime::{WireCloseReport, WireError, wire_cleanup};
use crate::split::{LineSplitter, Pushed};
use via_host::{ExitReceiver, ProcessControl};
use via_store::BlobTasks;

/// The prefix of an undecoded message VIA keeps (design §7.3).
pub const UNDECODED_BYTES: usize = 64 * 1024;

/// One stdout read (design §8.2).
const READ_BYTES: usize = 64 * 1024;

/// The message queue: at most 1,024 messages and 4 MiB (design §8.2, A47).
const QUEUE_MESSAGES: usize = 1024;
const QUEUE_BYTES: usize = 4 * 1024 * 1024;

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
    /// The stdout reader: a full queue is `Overflow`, a message over 1 MiB
    /// `MessageTooLarge`, a pipe read error `Transport`.
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

/// A write enqueued to the stdin writer (design §8.3). It is cancel-safe:
/// the caller keeps it pinned while it services other waits, and dropping
/// it never cuts a message the writer started.
pub struct PendingWrite(Pin<Box<dyn Future<Output = Result<SendOutcome, WireError>> + Send>>);

impl Future for PendingWrite {
    type Output = Result<SendOutcome, WireError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}

/// A job on the writer's data queue.
struct DataWrite {
    message: OutboundMessage,
    deadline: Deadline,
    reply: oneshot::Sender<Result<SendOutcome, WireError>>,
}

/// A job on the writer's control queue.
enum Control {
    Interrupt {
        bytes: Vec<u8>,
        deadline: Deadline,
        reply: oneshot::Sender<Result<SendOutcome, WireError>>,
    },
    /// A distinct control message holding its share of the budget.
    Message {
        /// Its place in the queue, by which its caller expires it.
        ticket: u64,
        bytes: Vec<u8>,
        deadline: Deadline,
        reply: oneshot::Sender<Result<SendOutcome, WireError>>,
    },
    /// Drop stdin once the current message is written.
    Close,
}

/// The note of the first undecoded message kept, and where it goes.
struct Undecoded {
    folder: PathBuf,
    /// The Store's owned blob steps, which run the file's write.
    tasks: BlobTasks,
    claimed: AtomicBool,
    note: StdMutex<Option<String>>,
}

/// State the connection's halves and tasks share.
struct Shared {
    latch: watch::Sender<LatchState>,
    /// Bytes of messages in the queue: counted before `try_send`, released
    /// on receive.
    queued_bytes: AtomicUsize,
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
    /// The writer's control queue and the control budget.
    control: ControlQueue,
    undecoded: Undecoded,
}

/// The writer's control queue (design §8.3): interrupts, distinct control
/// messages and the close, first in first out, beside the budget of the
/// distinct control messages outstanding (queued or being written), under
/// one lock. A queued control message is owned by whichever comes first of
/// its expiry (by its caller, or by the writer while it holds stdin) and the
/// writer's take, so it is answered and its share returned exactly once
/// (runtime §8, C2 §2).
#[derive(Default)]
struct ControlQueue {
    state: StdMutex<ControlState>,
    /// Wakes the writer, in `next` or in `expire_queued`, never both at
    /// once: a push before it waits leaves a permit, and `next` checks the
    /// queue before it waits.
    ready: Notify,
}

#[derive(Default)]
struct ControlState {
    jobs: VecDeque<Control>,
    /// The next control message's ticket.
    ticket: u64,
    /// The writer ended: nothing is queued any more.
    closed: bool,
    /// The distinct control messages outstanding and their bytes.
    messages: usize,
    bytes: usize,
}

impl ControlQueue {
    fn state(&self) -> std::sync::MutexGuard<'_, ControlState> {
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
    /// its ticket, or `None`, taking nothing, when it would pass
    /// [`CONTROL_MESSAGES`] or [`CONTROL_BYTES`] or the writer ended.
    fn push_message(
        &self,
        bytes: Vec<u8>,
        deadline: Deadline,
        reply: oneshot::Sender<Result<SendOutcome, WireError>>,
    ) -> Option<u64> {
        let mut state = self.state();
        let total = state.bytes.saturating_add(bytes.len());
        if state.closed || state.messages >= CONTROL_MESSAGES || total > CONTROL_BYTES {
            return None;
        }
        state.messages += 1;
        state.bytes = total;
        let ticket = state.ticket;
        state.ticket += 1;
        state.jobs.push_back(Control::Message {
            ticket,
            bytes,
            deadline,
            reply,
        });
        drop(state);
        self.ready.notify_one();
        Some(ticket)
    }

    /// Its caller's deadline passed: removes the message `ticket` if the
    /// writer has not taken it, returning its share; true when removed.
    fn expire(&self, ticket: u64) -> bool {
        let mut state = self.state();
        let queued = state.jobs.iter().position(
            |job| matches!(job, Control::Message { ticket: queued, .. } if *queued == ticket),
        );
        let Some(Control::Message { bytes, .. }) = queued.and_then(|at| state.jobs.remove(at))
        else {
            return false;
        };
        state.release(bytes.len());
        true
    }

    /// The writer's next control job, in queue order.
    async fn next(&self) -> Control {
        loop {
            if let Some(job) = self.state().jobs.pop_front() {
                return job;
            }
            self.ready.notified().await;
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

    /// Removes, under the lock, each queued message whose deadline passed,
    /// returning its share, and answers it `NotWritten`; the earliest
    /// deadline left.
    fn take_expired(&self) -> Option<tokio::time::Instant> {
        let now = tokio::time::Instant::now();
        let mut expired = Vec::new();
        let mut earliest: Option<tokio::time::Instant> = None;
        let mut state = self.state();
        let mut kept = VecDeque::with_capacity(state.jobs.len());
        while let Some(job) = state.jobs.pop_front() {
            match job {
                Control::Message {
                    bytes,
                    deadline,
                    reply,
                    ..
                } if deadline.instant() <= now => {
                    state.release(bytes.len());
                    expired.push(reply);
                }
                Control::Message { deadline, .. } => {
                    let at = deadline.instant();
                    earliest = Some(earliest.map_or(at, |earliest| earliest.min(at)));
                    kept.push_back(job);
                }
                job @ (Control::Interrupt { .. } | Control::Close) => kept.push_back(job),
            }
        }
        state.jobs = kept;
        drop(state);
        for reply in expired {
            let _ = reply.send(Ok(SendOutcome::NotWritten));
        }
        earliest
    }

    /// Returns the share of a message the writer resolved.
    fn release(&self, length: usize) {
        self.state().release(length);
    }

    /// The writer ended: nothing more is queued. Takes every queued job,
    /// each message's share returned.
    fn close(&self) -> VecDeque<Control> {
        let mut state = self.state();
        state.closed = true;
        let jobs = std::mem::take(&mut state.jobs);
        for job in &jobs {
            if let Control::Message { bytes, .. } = job {
                state.release(bytes.len());
            }
        }
        jobs
    }
}

impl ControlState {
    fn release(&mut self, length: usize) {
        self.messages = self.messages.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(length);
    }
}

impl Shared {
    /// Latches `cause` unless a failure came first; true when it won.
    fn fail(&self, cause: FailureCause) -> bool {
        self.latch.send_if_modified(|state| {
            if state.first.is_none() {
                state.first = Some(cause);
                true
            } else {
                false
            }
        })
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

    /// Writes the first 64 KiB of a message VIA cannot decode to the turn's
    /// `undecoded.bin` (design §7.3): the first message only, `create_new`,
    /// one owned blob step answered within 2 s (coding-style §5): one that
    /// overran stays owned until it ends. `what` describes the message; the
    /// note names the file or the error. Nothing fails here.
    async fn keep_undecoded(&self, bytes: &[u8], what: &str) {
        let undecoded = &self.undecoded;
        if undecoded.claimed.swap(true, Ordering::AcqRel) {
            return;
        }
        let path = undecoded.folder.join("undecoded.bin");
        let prefix = bytes[..bytes.len().min(UNDECODED_BYTES)].to_vec();
        let kept = prefix.len();
        let target = path.clone();
        let note = match undecoded
            .tasks
            .run(move || write_new(&target, &prefix))
            .await
        {
            Ok(()) => format!("{what}; first {kept} in {}", path.display()),
            Err(error) => format!("{what}; not saved: {error}"),
        };
        // Test builds: the save's outcome is known and not yet noted.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("wire.undecoded.before_note").await;
        *undecoded
            .note
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(note);
    }

    fn take_undecoded(&self) -> Option<String> {
        self.undecoded
            .note
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
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
    pub(crate) fn write(&self, message: OutboundMessage, deadline: Deadline) -> PendingWrite {
        let io = self.clone();
        PendingWrite(Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            let enqueued = match message {
                start @ OutboundMessage::Start { .. } => io
                    .data
                    .send(DataWrite {
                        message: start,
                        deadline,
                        reply,
                    })
                    .await
                    .is_ok(),
                OutboundMessage::Interrupt(bytes) => {
                    if bytes.len() > CONTROL_BYTES
                        || io.shared.interrupt_sent.swap(true, Ordering::AcqRel)
                    {
                        return Ok(SendOutcome::NotWritten);
                    }
                    io.shared.control.push(Control::Interrupt {
                        bytes,
                        deadline,
                        reply,
                    })
                }
                OutboundMessage::Control(bytes) => {
                    let Some(ticket) = io.shared.control.push_message(bytes, deadline, reply)
                    else {
                        return Ok(SendOutcome::NotWritten);
                    };
                    return io.control_answer(ticket, deadline, answer).await;
                }
            };
            if !enqueued {
                // The writer ended: nothing of this message was written.
                return Ok(SendOutcome::NotWritten);
            }
            // A writer stopped mid-message answers nothing: bytes may have
            // reached the vendor.
            answer.await.unwrap_or(Ok(SendOutcome::Indeterminate))
        }))
    }

    /// A queued control message's answer: it expires on its own deadline,
    /// even while another message holds stdin, unless the writer took it
    /// first, which then answers.
    async fn control_answer(
        &self,
        ticket: u64,
        deadline: Deadline,
        mut answer: oneshot::Receiver<Result<SendOutcome, WireError>>,
    ) -> Result<SendOutcome, WireError> {
        tokio::select! {
            biased;
            answered = &mut answer => return answered.unwrap_or(Ok(SendOutcome::Indeterminate)),
            () = sleep_until(deadline.instant()) => {}
        }
        if self.shared.control.expire(ticket) {
            return Ok(SendOutcome::NotWritten);
        }
        answer.await.unwrap_or(Ok(SendOutcome::Indeterminate))
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

    #[cfg(feature = "test-failpoints")]
    pub(crate) fn discarded(&self) -> u64 {
        self.shared.discarded.load(Ordering::Acquire)
    }

    #[cfg(feature = "test-failpoints")]
    pub(crate) fn queued_bytes(&self) -> usize {
        self.shared.queued_bytes.load(Ordering::Acquire)
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
    /// writer answered. `deadline` bounds the write itself: a message cut
    /// short by it closes stdin and answers `Indeterminate`, or `NotWritten`
    /// when nothing of it was written. A `Control` message differs: see
    /// [`OutboundMessage::Control`].
    pub fn write(&self, message: OutboundMessage, deadline: Deadline) -> PendingWrite {
        self.io.write(message, deadline)
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
        tokio::select! {
            biased;
            () = cancelled(&mut self.force) => Err(WireError::Cancelled),
            () = woken(&mut self.wake) => Err(WireError::Woken),
            message = self.queue.recv() => self.received(message),
            cause = latched(&mut self.latch) => Err(cause.error()),
        }
    }

    fn received(
        &mut self,
        message: Option<VendorMessage>,
    ) -> Result<Option<VendorMessage>, WireError> {
        if let Some(message) = &message {
            self.shared
                .queued_bytes
                .fetch_sub(message.bytes().len(), Ordering::AcqRel);
        }
        if let Some(cause) = self.shared.failure() {
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
    waits: Waits,
    stragglers: &Stragglers,
) -> WireConnection {
    let (io, messages) = connect(pipes.stdout, pipes.stdin, folder, waits, stragglers);
    WireConnection {
        sender: WireSender {
            io,
            process: Arc::new(Process { control, exits }),
        },
        messages,
    }
}

/// Starts the reader over `stdout` and the writer over `stdin`.
pub(crate) fn connect<R, W>(
    stdout: R,
    stdin: W,
    (folder, tasks): (PathBuf, BlobTasks),
    waits: Waits,
    stragglers: &Stragglers,
) -> (Io, WireMessages)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (latch, latch_rx) = watch::channel(LatchState::default());
    let shared = Arc::new(Shared {
        latch,
        queued_bytes: AtomicUsize::new(0),
        eof: AtomicBool::new(false),
        unterminated: AtomicBool::new(false),
        discarded: AtomicU64::new(0),
        interrupt_sent: AtomicBool::new(false),
        close_sent: AtomicBool::new(false),
        control: ControlQueue::default(),
        undecoded: Undecoded {
            folder,
            tasks,
            claimed: AtomicBool::new(false),
            note: StdMutex::new(None),
        },
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
    };
    (io, messages)
}

/// The stdout reader (design §8.2): reads up to 64 KiB into its fixed
/// buffer, splits on LF and queues each complete message with `try_send`,
/// never awaiting a consumer or the Store. A full queue or a message over
/// 1 MiB latches the failure and switches to discard mode: read to EOF,
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
    let mut splitter = LineSplitter::new();
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
        match splitter.push(&buffer[..count], |message| {
            enqueue(&shared, &queue, message)
        }) {
            Pushed::Consumed => {}
            Pushed::Refused => discard = true,
            Pushed::TooLarge(prefix) => {
                discard = true;
                let cause = FailureCause::Reader(WireFailure::MessageTooLarge);
                if shared.fail(cause) {
                    let what = format!(
                        "vendor message over the {} byte cap",
                        super::MAX_STDOUT_MESSAGE_BYTES
                    );
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

/// Counts `message` against the queue's bytes, then `try_send`s it; a
/// full queue latches `Overflow`. False stops the splitting.
fn enqueue(shared: &Shared, queue: &mpsc::Sender<VendorMessage>, message: Vec<u8>) -> bool {
    let length = message.len();
    let Ok(bounded) = BoundedBytes::try_from_message(message) else {
        shared.fail(FailureCause::Reader(WireFailure::MessageTooLarge));
        return false;
    };
    let queued = shared.queued_bytes.fetch_add(length, Ordering::AcqRel) + length;
    let sent = queued <= QUEUE_BYTES && queue.try_send(VendorMessage::new(bounded)).is_ok();
    if !sent {
        shared.queued_bytes.fetch_sub(length, Ordering::AcqRel);
        // A closed queue means the consumer is gone: only discard.
        if !queue.is_closed() {
            shared.fail(FailureCause::Reader(WireFailure::Overflow));
        }
    }
    sent
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
    deadline: Deadline,
    reply: oneshot::Sender<Result<SendOutcome, WireError>>,
    start: bool,
    /// A distinct control message's budgeted bytes, returned before its
    /// answer.
    budgeted: Option<usize>,
}

/// The stdin writer (design §8.3): owns stdin and writes one message at a
/// time, controls first between messages, never interleaving bytes. Every
/// write selects on the stop signal and its deadline; a message not written
/// whole closes stdin. On its end it drops stdin and marks it closed.
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
            control = queues.shared.control.next() => match control {
                Control::Interrupt { bytes, deadline, reply } => Job {
                    message: OutboundMessage::Interrupt(bytes),
                    deadline,
                    reply,
                    start: false,
                    budgeted: None,
                },
                Control::Message { bytes, deadline, reply, .. } => Job {
                    budgeted: Some(bytes.len()),
                    message: OutboundMessage::Control(bytes),
                    deadline,
                    reply,
                    start: false,
                },
                Control::Close => break,
            },
            data = queues.data.recv(), if data_open => {
                let Some(DataWrite { message, deadline, reply }) = data else {
                    data_open = false;
                    continue;
                };
                Job {
                    message,
                    deadline,
                    reply,
                    start: true,
                    budgeted: None,
                }
            }
        };
        let mut writing = Writing {
            stdin: &mut stdin,
            stop: &mut stop,
            deadline: job.deadline,
            whole: job.budgeted.is_some(),
            written: false,
            timed_out: false,
        };
        // While it holds stdin, queued control messages still expire.
        let outcome = tokio::select! {
            biased;
            outcome = writing.message(&job.message, &mut piece) => outcome,
            never = queues.shared.control.expire_queued() => match never {},
        };
        let (written, timed_out) = (writing.written, writing.timed_out);
        // Resolved, whatever the outcome: its share returns before the
        // answer, so a caller answered may enqueue the next at once.
        if let Some(length) = job.budgeted {
            queues.shared.control.release(length);
        }
        match outcome {
            Ok(true) => {
                // The whole input message (in S1 first the start carrying the
                // prompt) is in the vendor's stdin.
                #[cfg(feature = "test-failpoints")]
                if job.start
                    && let Err(error) =
                        via_store::failpoint::hit_async("wire.prompt.after_write").await
                {
                    let _ = job.reply.send(Err(WireError::Io(error)));
                    break;
                }
                let _ = job.start;
                let _ = job.reply.send(Ok(SendOutcome::Written));
            }
            // A control message's deadline passed before its first byte:
            // refused, and stdin stays open for the messages after it.
            Ok(false) if job.budgeted.is_some() && timed_out && !written => {
                let _ = job.reply.send(Ok(SendOutcome::NotWritten));
            }
            Ok(false) => {
                let _ = job.reply.send(Ok(if written {
                    SendOutcome::Indeterminate
                } else {
                    SendOutcome::NotWritten
                }));
                break;
            }
            Err(error) => {
                queues.shared.fail(FailureCause::Writer(error.kind()));
                let _ = job.reply.send(Err(WireError::Io(error)));
                break;
            }
        }
    }
    drop(stdin);
    refuse_queued(&mut queues);
    closed.send_replace(true);
}

/// The writer's end: nothing queued after it is written, so each queued
/// write answers `NotWritten` and a control message returns its share.
fn refuse_queued(queues: &mut Queues) {
    queues.data.close();
    for control in queues.shared.control.close() {
        match control {
            Control::Interrupt { reply, .. } | Control::Message { reply, .. } => {
                let _ = reply.send(Ok(SendOutcome::NotWritten));
            }
            Control::Close => {}
        }
    }
    while let Ok(data) = queues.data.try_recv() {
        let _ = data.reply.send(Ok(SendOutcome::NotWritten));
    }
}

/// One message being written.
struct Writing<'a, W> {
    stdin: &'a mut W,
    stop: &'a mut watch::Receiver<bool>,
    deadline: Deadline,
    /// A control message: the deadline bounds only the wait for its first
    /// byte, and one started is written whole, so a deadline never leaves a
    /// partial line nor closes a stdin other sessions may share.
    whole: bool,
    /// Some byte of the message was written.
    written: bool,
    /// The deadline cut the message.
    timed_out: bool,
}

impl<W: AsyncWrite + Unpin> Writing<'_, W> {
    /// Writes the whole message: true once complete, false when the stop
    /// signal or the deadline cut it.
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
            let pending = async move {
                match (whole, written) {
                    (true, false) => first_write(stdin, chunk, deadline).await,
                    (true, true) => Some(stdin.write(chunk).await),
                    (false, _) => timeout_at(deadline, stdin.write(chunk)).await.ok(),
                }
            };
            let write = tokio::select! {
                biased;
                () = stopped(self.stop) => return Ok(false),
                write = pending => write,
            };
            match write {
                None => {
                    self.timed_out = true;
                    return Ok(false);
                }
                Some(Ok(0)) => return Ok(false),
                Some(Ok(count)) => {
                    self.written = true;
                    offset += count;
                }
                Some(Err(error)) => return Err(error),
            }
        }
        Ok(true)
    }
}

/// A control message's first write: its expiry wins before every poll that
/// could write a byte, so an expired message never starts, though stdin is
/// writable; `None` once expired.
async fn first_write<W: AsyncWrite + Unpin>(
    stdin: &mut W,
    chunk: &[u8],
    deadline: tokio::time::Instant,
) -> Option<std::io::Result<usize>> {
    let expiry = sleep_until(deadline);
    tokio::pin!(expiry);
    std::future::poll_fn(|cx| {
        if tokio::time::Instant::now() >= deadline || expiry.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Pin::new(&mut *stdin).poll_write(cx, chunk).map(Some)
    })
    .await
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
#[cfg(feature = "test-failpoints")]
pub mod testing {
    use std::path::PathBuf;

    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::sync::watch;

    use super::{
        BlobTasks, Deadline, FailureCause, Io, OutboundMessage, PendingWrite, Stragglers, Waits,
        WireError, WireMessages, connect,
    };

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
    /// undecoded message in `folder`.
    pub fn pipes<R, W>(stdout: R, stdin: W, folder: PathBuf) -> TestPipes
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
        let (io, messages) = connect(stdout, stdin, (folder, tasks.clone()), waits, &stragglers);
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
        /// See `WireSender::write`.
        pub fn write(&self, message: OutboundMessage, deadline: Deadline) -> PendingWrite {
            self.io.write(message, deadline)
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
