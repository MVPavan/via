//! A thread's ingress lane (x.3.2 X0 items 5, 10, 12.5): the messages the
//! connection task routed to one registered thread, in decode order,
//! bounded at 16 messages and 1 MiB inside Wire's 1,024-message / 4 MiB
//! staging. Each routed message keeps its staging permit until the
//! driver consumes it, so the lanes count against the staging too. A full
//! lane is not waited on: the connection task never blocks on a driver.
//! The lane ends `Overflow` (the generation is quarantined) and later
//! messages for it are counted and dropped.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use via_wire::{ExitReport, VendorMessage, WireCleanup};

use super::{Notification, ServerRequest};

/// The most messages a lane holds.
pub const LANE_MESSAGES: usize = 16;

/// The most message bytes a lane holds.
pub const LANE_BYTES: usize = 1024 * 1024;

/// One message routed to a thread.
pub enum LaneItem {
    /// A decoded notification naming the thread; its staged bytes are kept
    /// until the driver takes it.
    Notification {
        /// The notification.
        notification: Notification,
        /// The raw message and its staging permit.
        staged: VendorMessage,
    },
    /// A server request VIA declined at decode, in its decode position
    /// (item 11): the driver reports `vendor.request_declined` only once
    /// `written` says the reply was written whole, by `decoded_at + 5 s`.
    Declined {
        /// The request.
        request: ServerRequest,
        /// When it was decoded.
        decoded_at: Instant,
        /// `Some(true)` once the reply was written whole, `Some(false)`
        /// when it was not.
        written: watch::Receiver<Option<bool>>,
    },
    /// A message whose correlation names this thread but which is not a
    /// valid message of its method (item 5 step 5): an attributable decode
    /// failure of the generation.
    Malformed {
        /// The bytes, for the turn's `undecoded.bin`.
        staged: VendorMessage,
        /// The bounded diagnostic.
        detail: &'static str,
    },
}

/// Why a whole connection failed, as its sessions report it (item 13.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LossCause {
    /// An unattributable decode failure: `failed(protocol)`.
    Protocol,
    /// Staging, correlation or reply-bound exhaustion: `failed(overflow)`.
    Overflow,
    /// Host's evidence says the server died: `failed(server_lost)`.
    ServerLost,
    /// The transport ended with the server alive or unconfirmed:
    /// `unknown`.
    TransportLost,
}

/// A whole connection's failure, after its owned sequence ran.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionLoss {
    /// The disposition.
    pub cause: LossCause,
    /// Host's cleanup of the server's group: `Quiescent` only when it
    /// proved the group absent.
    pub cleanup: WireCleanup,
    /// Host's exit report for the server, when one was confirmed.
    pub exit: Option<ExitReport>,
    /// A Host journal write in the cleanup had an uncertain outcome.
    pub journal_uncertain: bool,
}

/// How a lane ended, after every message routed before it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneEnd {
    /// The lane was full: the generation is quarantined.
    Overflow,
    /// The whole connection failed.
    Lost(ConnectionLoss),
    /// The server retired with the lane still registered.
    Retired,
}

/// What [`Lane::next`] returns.
pub enum LaneEvent {
    /// The next routed message.
    Item(Box<LaneItem>),
    /// The lane's end, once every earlier message was taken.
    End(LaneEnd),
}

#[derive(Default)]
struct Queue {
    items: VecDeque<(LaneItem, usize)>,
    bytes: usize,
    end: Option<LaneEnd>,
    /// Messages refused after the end.
    dropped: u64,
}

/// One thread's ingress lane: the connection task pushes, one driver
/// takes.
#[derive(Default)]
pub struct Lane {
    queue: Mutex<Queue>,
    ready: Notify,
}

impl Lane {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        // A push or take is one in-place edit: the state stays consistent.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Routes `item`, charged `bytes`. A lane already ended drops it
    /// (counted); one that would pass [`LANE_MESSAGES`] or [`LANE_BYTES`]
    /// ends `Overflow` and drops it: the connection task never waits.
    /// Whether the lane took it.
    pub fn push(&self, item: LaneItem, bytes: usize) -> bool {
        let mut queue = self.queue();
        if queue.end.is_some() {
            queue.dropped = queue.dropped.saturating_add(1);
            return false;
        }
        if queue.items.len() >= LANE_MESSAGES || queue.bytes.saturating_add(bytes) > LANE_BYTES {
            queue.end = Some(LaneEnd::Overflow);
            queue.dropped = queue.dropped.saturating_add(1);
            drop(queue);
            self.ready.notify_one();
            return false;
        }
        queue.bytes = queue.bytes.saturating_add(bytes);
        queue.items.push_back((item, bytes));
        drop(queue);
        self.ready.notify_one();
        true
    }

    /// Ends the lane with `end` after what it holds, unless it already
    /// ended: the first end stays.
    pub fn end(&self, end: LaneEnd) {
        let mut queue = self.queue();
        if queue.end.is_none() {
            queue.end = Some(end);
        }
        drop(queue);
        self.ready.notify_one();
    }

    /// Whether the lane has ended (messages may still be queued).
    pub fn ended(&self) -> Option<LaneEnd> {
        self.queue().end
    }

    /// The messages dropped after the lane ended.
    pub fn dropped(&self) -> u64 {
        self.queue().dropped
    }

    /// Takes the next message without waiting, or the end once the lane
    /// holds no more; `None` when it is empty and open.
    pub fn try_next(&self) -> Option<LaneEvent> {
        let mut queue = self.queue();
        if let Some((item, bytes)) = queue.items.pop_front() {
            queue.bytes = queue.bytes.saturating_sub(bytes);
            return Some(LaneEvent::Item(Box::new(item)));
        }
        queue.end.map(LaneEvent::End)
    }

    /// The next message, or the lane's end once it holds no more. Cancel
    /// safe: a message is taken only when this returns it.
    pub async fn next(&self) -> LaneEvent {
        loop {
            if let Some(event) = self.try_next() {
                return event;
            }
            // One consumer: a push between the check and this wait left a
            // permit, so the wake is not lost.
            self.ready.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use via_wire::{BoundedBytes, VendorMessage, WireCleanup};

    use super::{
        ConnectionLoss, LANE_BYTES, LANE_MESSAGES, Lane, LaneEnd, LaneEvent, LaneItem, LossCause,
    };

    fn item(text: &str) -> LaneItem {
        let line = format!("{{\"method\":\"x\",\"params\":{{\"note\":\"{text}\"}}}}\n");
        LaneItem::Malformed {
            staged: VendorMessage::new(BoundedBytes::try_from_message(line.into_bytes()).unwrap()),
            detail: "test",
        }
    }

    fn note(event: Option<LaneEvent>) -> Option<String> {
        match event? {
            LaneEvent::Item(item) => match *item {
                LaneItem::Malformed { staged, .. } => {
                    Some(String::from_utf8(staged.bytes().to_vec()).unwrap())
                }
                LaneItem::Notification { .. } | LaneItem::Declined { .. } => None,
            },
            LaneEvent::End(_) => None,
        }
    }

    fn ended(event: Option<LaneEvent>) -> Option<LaneEnd> {
        match event? {
            LaneEvent::End(end) => Some(end),
            LaneEvent::Item(_) => None,
        }
    }

    /// Sixteen messages fit; the seventeenth ends the lane `Overflow`
    /// without waiting, after the sixteen, and is dropped and counted.
    #[test]
    fn lane_overflows_past_sixteen_messages() {
        let lane = Lane::default();
        for index in 0..LANE_MESSAGES {
            assert!(lane.push(item(&index.to_string()), 10), "{index}");
        }
        assert!(!lane.push(item("one more"), 10));
        assert_eq!(lane.ended(), Some(LaneEnd::Overflow));
        assert_eq!(lane.dropped(), 1);
        for index in 0..LANE_MESSAGES {
            assert!(
                note(lane.try_next())
                    .unwrap()
                    .contains(&format!("\"{index}\""))
            );
        }
        assert_eq!(ended(lane.try_next()), Some(LaneEnd::Overflow));
        assert!(!lane.push(item("after"), 10));
        assert_eq!(lane.dropped(), 2);
    }

    /// The byte bound ends the lane as the count does.
    #[test]
    fn lane_overflows_past_its_bytes() {
        let lane = Lane::default();
        assert!(lane.push(item("big"), LANE_BYTES - 1));
        assert!(lane.push(item("one"), 1));
        assert!(!lane.push(item("two"), 1));
        assert_eq!(lane.ended(), Some(LaneEnd::Overflow));
    }

    /// An end comes after every message routed before it; the first end
    /// stays.
    #[tokio::test]
    async fn lane_end_follows_its_messages() {
        let lane = Lane::default();
        assert!(lane.push(item("first"), 1));
        let loss = ConnectionLoss {
            cause: LossCause::ServerLost,
            cleanup: WireCleanup::Quiescent,
            exit: None,
            journal_uncertain: false,
        };
        lane.end(LaneEnd::Lost(loss));
        lane.end(LaneEnd::Retired);
        assert!(note(Some(lane.next().await)).unwrap().contains("first"));
        assert!(matches!(lane.next().await, LaneEvent::End(LaneEnd::Lost(ended)) if ended == loss));
    }

    /// A taker waiting on an empty lane wakes for the next push.
    #[tokio::test]
    async fn lane_wakes_its_taker() {
        let lane = std::sync::Arc::new(Lane::default());
        let taker = {
            let lane = std::sync::Arc::clone(&lane);
            tokio::spawn(async move { note(Some(lane.next().await)) })
        };
        tokio::task::yield_now().await;
        assert!(lane.push(item("woken"), 1));
        assert!(taker.await.unwrap().unwrap().contains("woken"));
    }
}
