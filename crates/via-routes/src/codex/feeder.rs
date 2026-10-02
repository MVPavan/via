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
//! The owning guard of item 12.2 calls [`Feeder::withdraw`]: a turn's
//! items still queued here are removed, and the ones handed to Wire are
//! withdrawn before their first byte.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;

use tokio::sync::{Notify, oneshot, watch};
use tokio::time::Instant;
use via_wire::{DataHold, OutboundMessage, PendingWrite, SendOutcome, WriteBounds, WriteTicket};

use super::stdio::Stdio;

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

struct Queued {
    key: u64,
    item: Item,
}

#[derive(Default)]
struct Queues {
    replies: VecDeque<Queued>,
    controls: VecDeque<Queued>,
    data: VecDeque<Queued>,
    /// The tickets of items handed to Wire and not yet answered, by key.
    handed: HashMap<u64, WriteTicket>,
    next: u64,
    /// The connection ended: nothing more is taken.
    closed: bool,
}

impl Queues {
    fn controls_pending(&self) -> bool {
        !self.replies.is_empty() || !self.controls.is_empty()
    }

    /// The earliest reply deadline still queued.
    fn reply_deadline(&self) -> Option<Instant> {
        self.replies
            .iter()
            .filter_map(|queued| queued.item.deadline)
            .min()
    }
}

/// One write handed to Wire.
struct Flight {
    key: u64,
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

    /// Queues `item`; its key, for a guard. A closed feeder answers it
    /// `NotWritten` at once.
    pub(super) fn push(&self, queue: Queue, item: Item) -> Option<u64> {
        let mut queues = self.queues();
        if queues.closed {
            drop(queues);
            item.answer.send(SendOutcome::NotWritten);
            return None;
        }
        let key = queues.next;
        queues.next = key.wrapping_add(1);
        let entry = Queued { key, item };
        match queue {
            Queue::Reply => queues.replies.push_back(entry),
            Queue::Control => queues.controls.push_back(entry),
            Queue::Data => queues.data.push_back(entry),
        }
        drop(queues);
        self.wake.notify_one();
        Some(key)
    }

    /// Item 12.2's withdrawal, synchronous: `keys`' items still queued are
    /// removed and answered `NotWritten`; the ones handed to Wire are
    /// withdrawn, which wins before their first byte. Returns the request
    /// records of the removed items: nothing of them was written.
    pub(super) fn withdraw(&self, keys: &[u64]) -> Vec<i64> {
        if keys.is_empty() {
            return Vec::new();
        }
        let (removed, tickets) = {
            let mut guard = self.queues();
            let queues = &mut *guard;
            let mut removed = Vec::new();
            for queue in [&mut queues.replies, &mut queues.controls, &mut queues.data] {
                let mut index = 0;
                while index < queue.len() {
                    if keys.contains(&queue[index].key) {
                        if let Some(queued) = queue.remove(index) {
                            removed.push(queued.item);
                        }
                    } else {
                        index += 1;
                    }
                }
            }
            let tickets: Vec<WriteTicket> = keys
                .iter()
                .filter_map(|key| queues.handed.get(key).cloned())
                .collect();
            (removed, tickets)
        };
        for ticket in tickets {
            self.stdio.withdraw(ticket);
        }
        removed
            .into_iter()
            .filter_map(|item| {
                item.answer.send(SendOutcome::NotWritten);
                item.request
            })
            .collect()
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
                .map(|queued| queued.item)
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

    /// Hands a queued item to Wire.
    fn hand(&self, queues: &mut Queues, entry: Queued, reply: bool) -> Flight {
        let Queued { key, item } = entry;
        let bytes = item.control_bytes();
        let write = self.stdio.write(item.message, item.bounds);
        queues.handed.insert(key, write.ticket());
        Flight {
            key,
            request: item.request,
            answer: item.answer,
            deadline: item.deadline,
            reply: reply.then_some(bytes),
            write: Box::pin(write),
        }
    }

    /// Answers a write that ended and reports it; whether it was a reply
    /// not written whole, which fails the connection.
    fn land(&self, flight: Flight, outcome: SendOutcome, done: &(dyn Fn(Done) + Sync)) -> bool {
        self.queues().handed.remove(&flight.key);
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
            let late_at = {
                let mut queues = self.queues();
                // The hold comes before the control's write: Wire never
                // claims unstarted data ahead of it.
                if (control.is_some() || queues.controls_pending()) && hold.is_none() {
                    hold = Some(self.stdio.hold_data());
                }
                if control.is_none() {
                    let next = match queues.replies.pop_front() {
                        Some(reply) => Some((reply, true)),
                        None => queues.controls.pop_front().map(|next| (next, false)),
                    };
                    if let Some((next, reply)) = next {
                        control = Some(self.hand(&mut queues, next, reply));
                    }
                }
                if data.is_none()
                    && let Some(next) = queues.data.pop_front()
                {
                    data = Some(self.hand(&mut queues, next, false));
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
            tokio::select! {
                biased;
                outcome = landed(control.as_mut()) => {
                    if let Some(flight) = control.take()
                        && self.land(flight, outcome, done)
                    {
                        return PumpEnd::ReplyLate;
                    }
                }
                outcome = landed(data.as_mut()) => {
                    if let Some(flight) = data.take() {
                        self.land(flight, outcome, done);
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
