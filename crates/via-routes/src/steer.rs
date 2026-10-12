//! A turn's steer lane (C2 §2 independent lanes): the admitting
//! [`SteerSender`] the driver holds and the receiver its route serves.
//! Admission is bounded by the control lane's commands and encoded bytes;
//! the route marks what it established of each input as it happens, so a
//! caller whose turn ended unanswered can tell an acknowledged input from
//! one the vendor may have, or never had (critical r3 #1, r5 #1). The
//! route's encoding of a steer is its own: it sizes each input.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

/// C2 §2 independent lanes: control commands admitted at once.
pub const CONTROL_COMMANDS: usize = 8;

/// C2 §2 independent lanes: the admitted commands' total encoded bytes.
pub const CONTROL_BYTES: usize = 64 * 1024;

/// One steer input for the running turn, admitted by [`SteerSender`];
/// `reply` answers once the input was written whole and the vendor reported
/// its delivery, or with why it was not delivered. A reply dropped
/// unanswered means the turn ended first.
pub struct SteerRequest {
    /// The steer text.
    pub text: String,
    /// The vendor turn the caller means, if it names one.
    pub expected_vendor_turn: Option<String>,
    /// The caller's token for the request, which the route pairs with the
    /// vendor's delivery report.
    pub token: u64,
    /// The delivery answer.
    pub reply: oneshot::Sender<Result<(), SteerRefused>>,
    /// What the route established of the input ([`SteerAnswer`]).
    pub(crate) progress: Arc<SteerProgress>,
    /// The request's share of the control budget, returned when it is
    /// dropped.
    pub(crate) permit: ControlPermit,
}

/// One admitted command's share of the control budget (C2 §2): a command
/// slot and its encoded bytes, held until the command is resolved.
pub(crate) struct ControlPermit {
    _command: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

/// Why the route did not deliver a steer input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteerRefused {
    /// The turn is not accepted yet, or already ended.
    NotActive,
    /// The input was not written whole.
    NotWritten,
    /// The input names another vendor turn than the running one.
    TurnMismatch,
    /// The control lane's eight commands or 64 KiB are taken (C2 §2).
    OverCapacity,
}

/// The admitting side of a turn's steer lane (C2 §2 independent lanes): at
/// most [`CONTROL_COMMANDS`] requests and [`CONTROL_BYTES`] encoded in
/// total are outstanding, queued or awaiting their answer; anything more is
/// refused before it is enqueued.
#[derive(Clone)]
pub struct SteerSender {
    sender: mpsc::Sender<SteerRequest>,
    commands: Arc<Semaphore>,
    budget: Arc<Semaphore>,
    /// The route's encoded bytes of one input's text.
    encoded: fn(&str) -> usize,
}

/// A turn's steer lane: the admitting sender and the route's receiver;
/// `encoded` gives the route's encoded bytes of one input's text.
pub fn steer_lane(encoded: fn(&str) -> usize) -> (SteerSender, mpsc::Receiver<SteerRequest>) {
    let (sender, receiver) = mpsc::channel(CONTROL_COMMANDS);
    let commands = Arc::new(Semaphore::new(CONTROL_COMMANDS));
    let budget = Arc::new(Semaphore::new(CONTROL_BYTES));
    (
        SteerSender {
            sender,
            commands,
            budget,
            encoded,
        },
        receiver,
    )
}

impl SteerSender {
    /// Admits one steer input with its caller's `token`, or refuses it
    /// at once.
    pub fn send(
        &self,
        text: String,
        expected_vendor_turn: Option<String>,
        token: u64,
    ) -> Result<SteerAnswer, SteerRefused> {
        let command = Arc::clone(&self.commands)
            .try_acquire_owned()
            .map_err(|_| SteerRefused::OverCapacity)?;
        let encoded = (self.encoded)(&text);
        // A size past `u32` is past the budget too: both refuse it.
        let bytes = u32::try_from(encoded)
            .ok()
            .and_then(|bytes| Arc::clone(&self.budget).try_acquire_many_owned(bytes).ok())
            .ok_or(SteerRefused::OverCapacity)?;
        let permit = ControlPermit {
            _command: command,
            _bytes: bytes,
        };
        let (reply, answer) = oneshot::channel();
        let progress = Arc::new(SteerProgress::default());
        let request = SteerRequest {
            text,
            expected_vendor_turn,
            token,
            reply,
            progress: Arc::clone(&progress),
            permit,
        };
        match self.sender.try_send(request) {
            Ok(()) => Ok(SteerAnswer {
                reply: answer,
                progress,
            }),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SteerRefused::OverCapacity),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SteerRefused::NotActive),
        }
    }
}

/// What the route established of one steer input, as it happens (critical
/// r3 #1, r5 #1).
#[derive(Default)]
pub(crate) struct SteerProgress {
    /// The route started writing the input.
    write_started: AtomicBool,
    /// The vendor reported the input's delivery; set before the report is
    /// handed over, so before its observation can be emitted.
    acknowledged: AtomicBool,
}

impl SteerProgress {
    /// The route is about to write the input's first byte.
    pub(crate) fn mark_write_started(&self) {
        self.write_started.store(true, Ordering::Release);
    }

    /// The vendor acknowledged the input; marked before its report is
    /// handed over, whatever the write's answer.
    pub(crate) fn mark_acknowledged(&self) {
        self.acknowledged.store(true, Ordering::Release);
    }
}

/// An admitted steer input's answer, as its caller holds it (critical r3
/// #1, r5 #1): the route's reply, and what the route established of the
/// input, which tells a caller whose turn ended unanswered whether the
/// vendor acknowledged it, may have it, or never had it.
pub struct SteerAnswer {
    /// The route's reply; dropped unanswered when the turn ended first. The
    /// route answers an acknowledged input only once its write is answered
    /// too.
    pub reply: oneshot::Receiver<Result<(), SteerRefused>>,
    progress: Arc<SteerProgress>,
}

impl SteerAnswer {
    /// Whether the route started writing the input: it may have been
    /// written, in part or whole. False means it never left the control
    /// lane.
    #[must_use]
    pub fn write_started(&self) -> bool {
        self.progress.write_started.load(Ordering::Acquire)
    }

    /// Whether the vendor reported the input's delivery, whatever the
    /// route's reply: its `steer.delivered` report was then handed over.
    #[must_use]
    pub fn acknowledged(&self) -> bool {
        self.progress.acknowledged.load(Ordering::Acquire)
    }
}
