//! `codex::Feeder` (x.3.2 X0 item 12.3): Wire's only producer on a shared
//! connection. Callers queue here, and the connection task hands Wire one
//! control and one data message at a time: server-request replies first,
//! then the other controls (the handshake's requests, cleanup intents) in
//! order, and turn input in order. While a control is queued or in flight
//! the feeder holds a `DataHold`, so Wire's writer serves the control
//! before any unstarted data message. A reply has its own deadline, apart
//! from Wire's first-byte bound: one not written whole by it fails the
//! connection (packet §4).
//!
//! A turn's input items carry its [`WriteCancel`] (x.3.2 X3 §2.2): once
//! it is cancelled, the feeder hands none of them to Wire, and the ones it
//! handed are withdrawn, which wins before their first byte. A
//! `turn/start` pushes its turn's `Start` marker into its lane as it is
//! handed, refused by the lane's start gate.

use std::any::Any;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;

use tokio::sync::{Notify, oneshot, watch};
use tokio::time::Instant;
use via_wire::{
    DataHold, OutboundMessage, PendingWrite, SendOutcome, TurnNumber, WriteBounds, WriteTicket,
};

use super::lane::Lane;
use super::stdio::Stdio;
use crate::DecodeWatermark;

/// Which of the feeder's queues an item joins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Queue {
    /// A server-request reply: before any other control.
    Reply,
    /// Any other control: the handshake's requests, cleanup intents.
    Control,
    /// Turn input (`thread/start`, `thread/resume`, `turn/start`).
    Data,
}

/// Who learns an item's outcome.
pub(super) enum Answer {
    /// A caller awaiting the write.
    Write(oneshot::Sender<SendOutcome>),
    /// A decline's placeholder: `true` once written whole.
    Reply(watch::Sender<Option<bool>>),
}

impl Answer {
    fn send(self, outcome: SendOutcome) {
        match self {
            Self::Write(answer) => {
                // A caller that left wants no answer.
                let _gone = answer.send(outcome);
            }
            Self::Reply(written) => {
                written.send_replace(Some(outcome == SendOutcome::Written));
            }
        }
    }
}

/// One message to hand to Wire.
pub(super) struct Item {
    pub(super) message: OutboundMessage,
    pub(super) bounds: WriteBounds,
    pub(super) answer: Answer,
    /// The request record it writes, if any.
    pub(super) request: Option<i64>,
    /// A reply's own bound: written whole by then, or the connection fails.
    pub(super) deadline: Option<Instant>,
    /// The turn's write-cancel token, for turn input.
    pub(super) cancel: Option<Arc<WriteCancel>>,
    /// A `turn/start`'s marker, pushed as it is handed.
    pub(super) start: Option<StartMarker>,
}

/// A `turn/start`'s `Start` marker (x.3.2 X3 §2.1), for its lane.
pub(super) struct StartMarker {
    pub(super) lane: Arc<Lane>,
    pub(super) turn: TurnNumber,
    pub(super) decoded: DecodeWatermark,
    pub(super) cx: Box<dyn Any + Send + Sync>,
}

impl Item {
    /// The bytes of a control message; data counts none.
    fn control_bytes(&self) -> usize {
        match &self.message {
            OutboundMessage::Control(bytes) | OutboundMessage::Interrupt(bytes) => bytes.len(),
            OutboundMessage::Start { .. } => 0,
        }
    }
}

/// A write that ended: its request record, if any, and how; a reply's
/// bytes, once it no longer counts against the pending replies.
pub(super) struct Done {
    pub(super) request: Option<i64>,
    pub(super) outcome: SendOutcome,
    pub(super) reply: Option<usize>,
}

/// Why the pump stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PumpEnd {
    /// A reply was not written whole by its deadline.
    ReplyLate,
}

/// One turn's write-cancel token (x.3.2 X3 §2.2), created before any of
/// its input is queued. Lock order: the feeder's queues, then the token,
/// then Wire.
pub(super) struct WriteCancel {
    stdio: Arc<dyn Stdio>,
    state: Mutex<Cancel>,
}

#[derive(Default)]
struct Cancel {
    cancelled: bool,
    /// The tickets of the turn's items handed to Wire.
    tickets: Vec<WriteTicket>,
}

impl WriteCancel {
    pub(super) fn new(stdio: Arc<dyn Stdio>) -> Self {
        Self {
            stdio,
            state: Mutex::new(Cancel::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, Cancel> {
        // A flag and a list, each edited in one step.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Cancels the turn's writes, synchronously and idempotently: none is
    /// handed to Wire from now on, and every handed one is withdrawn, which
    /// wins before its first byte; a started one is finished whole.
    pub(super) fn cancel(&self) {
        let mut state = self.state();
        state.cancelled = true;
        for ticket in state.tickets.drain(..) {
            self.stdio.withdraw(ticket);
        }
    }
}

/// An item [`Feeder::hand`] refused: its turn's writes were cancelled.
struct Refused {
    answer: Answer,
    request: Option<i64>,
}

#[derive(Default)]
struct Queues {
    replies: VecDeque<Item>,
    controls: VecDeque<Item>,
    data: VecDeque<Item>,
    /// The connection ended: nothing more is taken.
    closed: bool,
}

impl Queues {
    fn controls_pending(&self) -> bool {
        !self.replies.is_empty() || !self.controls.is_empty()
    }

    /// The earliest reply deadline still queued.
    fn reply_deadline(&self) -> Option<Instant> {
        self.replies.iter().filter_map(|item| item.deadline).min()
    }
}

/// One write handed to Wire.
struct Flight {
    request: Option<i64>,
    answer: Answer,
    deadline: Option<Instant>,
    reply: Option<usize>,
    write: Pin<Box<PendingWrite>>,
}

/// The feeder of one connection.
pub(super) struct Feeder {
    stdio: Arc<dyn Stdio>,
    queues: Mutex<Queues>,
    wake: Notify,
}

impl Feeder {
    pub(super) fn new(stdio: Arc<dyn Stdio>) -> Self {
        Self {
            stdio,
            queues: Mutex::new(Queues::default()),
            wake: Notify::new(),
        }
    }

    fn queues(&self) -> std::sync::MutexGuard<'_, Queues> {
        // Every edit is a push, a removal or one map change: the queues
        // stay consistent across a panic elsewhere.
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `item`; whether it was queued. A closed feeder answers it
    /// `NotWritten` at once.
    pub(super) fn push(&self, queue: Queue, item: Item) -> bool {
        let mut queues = self.queues();
        if queues.closed {
            drop(queues);
            item.answer.send(SendOutcome::NotWritten);
            return false;
        }
        match queue {
            Queue::Reply => queues.replies.push_back(item),
            Queue::Control => queues.controls.push_back(item),
            Queue::Data => queues.data.push_back(item),
        }
        drop(queues);
        self.wake.notify_one();
        true
    }

    /// The connection ended: every queued item is answered `NotWritten`
    /// (nothing of it reached Wire) and later ones are refused. Returns
    /// their request records.
    pub(super) fn close(&self) -> Vec<i64> {
        let removed: Vec<Item> = {
            let mut queues = self.queues();
            queues.closed = true;
            let queues = &mut *queues;
            queues
                .replies
                .drain(..)
                .chain(queues.controls.drain(..))
                .chain(queues.data.drain(..))
                .collect()
        };
        removed
            .into_iter()
            .filter_map(|item| {
                item.answer.send(SendOutcome::NotWritten);
                item.request
            })
            .collect()
    }

    /// Hands a queued item to Wire, under the queues' lock, then its
    /// token's: refused if its turn's writes were cancelled, or a
    /// `turn/start` whose `Start` its lane refused (x.3.2 X3 §2.2: ended,
    /// full or gated); else its ticket is installed in the token.
    fn hand(&self, item: Item, reply: bool) -> Result<Flight, Refused> {
        let bytes = item.control_bytes();
        let mut token = item.cancel.as_deref().map(WriteCancel::state);
        let refused = token.as_ref().is_some_and(|token| token.cancelled)
            || item
                .start
                .is_some_and(|start| !start.lane.push_start(start.turn, start.decoded, start.cx));
        if refused {
            return Err(Refused {
                answer: item.answer,
                request: item.request,
            });
        }
        let write = self.stdio.write(item.message, item.bounds);
        if let Some(token) = token.as_mut() {
            token.tickets.push(write.ticket());
        }
        drop(token);
        Ok(Flight {
            request: item.request,
            answer: item.answer,
            deadline: item.deadline,
            reply: reply.then_some(bytes),
            write: Box::pin(write),
        })
    }

    /// Answers a write that ended and reports it; whether it was a reply
    /// not written whole, which fails the connection.
    fn land(flight: Flight, outcome: SendOutcome, done: &(dyn Fn(Done) + Sync)) -> bool {
        let late = flight.deadline.is_some() && outcome != SendOutcome::Written;
        flight.answer.send(outcome);
        done(Done {
            request: flight.request,
            outcome,
            reply: flight.reply,
        });
        late
    }

    /// The pump, polled by the connection task for its life: hands Wire
    /// one control and one data message at a time and answers each when
    /// Wire does, reporting it to `done`. It returns only when a reply was
    /// not written whole by its deadline.
    pub(super) async fn pump(&self, done: &(dyn Fn(Done) + Sync)) -> PumpEnd {
        let mut control: Option<Flight> = None;
        let mut data: Option<Flight> = None;
        let mut hold: Option<DataHold> = None;
        loop {
            #[cfg(feature = "test-failpoints")]
            if data.is_none() && !self.queues().data.is_empty() {
                // x.3.2 X3 S1: between a turn input's queueing and its
                // hand-off.
                let _ = crate::failpoint::hit_async("codex.feeder.queued").await;
            }
            let mut refused = Vec::new();
            let late_at = {
                let mut queues = self.queues();
                // The hold comes before the control's write: Wire never
                // claims unstarted data ahead of it.
                if (control.is_some() || queues.controls_pending()) && hold.is_none() {
                    hold = Some(self.stdio.hold_data());
                }
                while control.is_none()
                    && let Some((next, reply)) = match queues.replies.pop_front() {
                        Some(reply) => Some((reply, true)),
                        None => queues.controls.pop_front().map(|next| (next, false)),
                    }
                {
                    match self.hand(next, reply) {
                        Ok(flight) => control = Some(flight),
                        Err(item) => refused.push(item),
                    }
                }
                while data.is_none()
                    && let Some(next) = queues.data.pop_front()
                {
                    match self.hand(next, false) {
                        Ok(flight) => data = Some(flight),
                        Err(item) => refused.push(item),
                    }
                }
                if control.is_none() && !queues.controls_pending() {
                    hold = None;
                }
                let flying = control.as_ref().and_then(|flight| flight.deadline);
                match (flying, queues.reply_deadline()) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            };
            // Nothing of a refused item reached Wire.
            for item in refused {
                item.answer.send(SendOutcome::NotWritten);
                done(Done {
                    request: item.request,
                    outcome: SendOutcome::NotWritten,
                    reply: None,
                });
            }
            tokio::select! {
                biased;
                outcome = landed(control.as_mut()) => {
                    if let Some(flight) = control.take()
                        && Self::land(flight, outcome, done)
                    {
                        return PumpEnd::ReplyLate;
                    }
                }
                outcome = landed(data.as_mut()) => {
                    if let Some(flight) = data.take() {
                        Self::land(flight, outcome, done);
                    }
                }
                () = sleep_until(late_at) => return PumpEnd::ReplyLate,
                () = self.wake.notified() => {}
            }
        }
    }
}

/// A flight's outcome; pending forever with no flight. A Wire error is an
/// indeterminate write.
async fn landed(flight: Option<&mut Flight>) -> SendOutcome {
    let Some(flight) = flight else {
        return std::future::pending().await;
    };
    std::future::poll_fn(|cx| match flight.write.as_mut().poll(cx) {
        Poll::Ready(Ok(outcome)) => Poll::Ready(outcome),
        Poll::Ready(Err(_)) => Poll::Ready(SendOutcome::Indeterminate),
        Poll::Pending => Poll::Pending,
    })
    .await
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
