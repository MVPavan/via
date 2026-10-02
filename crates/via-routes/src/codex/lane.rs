//! A thread's ingress lane (x.3.2 X0 items 5, 10, 11, 12.5): the messages
//! the connection task routed to one registered thread, in decode order,
//! bounded at 16 messages and 1 MiB inside Wire's 1,024-message / 4 MiB
//! staging. Each routed message is kept raw with its staging permit until
//! the driver's normalizer consumes and decodes it, so the lanes count
//! against the staging; a decline's placeholder keeps the request's own
//! message, so it is charged one message and its bytes too. A full lane is
//! not waited on: the connection task never blocks on a driver. The lane
//! ends `Overflow` (the generation is quarantined) and later messages for
//! it are counted and dropped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use via_wire::{ExitReport, TurnNumber, VendorMessage, WireCleanup};

use super::ServerRequest;
use crate::DecodeWatermark;

/// The most messages a lane holds.
pub const LANE_MESSAGES: usize = 16;

/// The most message bytes a lane holds.
pub const LANE_BYTES: usize = 1024 * 1024;

/// One routed message, raw (item 12.5): decoded at consumption.
pub struct Routed {
    /// The raw message and its staging permit.
    pub staged: VendorMessage,
    /// The connection's decode sequence of the message (item 13.2).
    pub seq: u64,
    /// The vendor turn its correlation names, if any.
    pub turn: Option<String>,
    /// The VIA turn that vendor turn was accepted as, when the connection
    /// had mapped it at routing (packet §5).
    pub owner: Option<TurnNumber>,
    /// When the connection read it (C2 §4, runtime §8): its observations'
    /// instant, so time it waits in the lane moves no idle deadline.
    pub at: Instant,
    /// Its position under the lane's decode fence when the lane took it;
    /// `None` when no turn had fenced the lane. Set by [`Lane::push`].
    pub mark: Option<Mark>,
}

/// A routed message's position under its lane's decode fence (x.3.2
/// critical r2 #2, runtime §8).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mark {
    /// Which fence of the lane counted it ([`Lane::fence`]'s return).
    pub fence: u64,
    /// Its position in that fence's turn decode watermark.
    pub seq: u64,
}

/// One message routed to a thread.
pub enum LaneItem {
    /// A notification naming the thread.
    Message(Routed),
    /// A server request VIA declined at decode, in its decode position
    /// (item 11): the driver reports `vendor.request_declined` only once
    /// `written` says the reply was written whole, by `decoded_at + 5 s`.
    Declined {
        /// The request's own message: the placeholder's staging charge.
        routed: Routed,
        /// The request.
        request: ServerRequest,
        /// When it was decoded.
        decoded_at: Instant,
        /// `Some(true)` once the reply was written whole, `Some(false)`
        /// when it was not.
        written: watch::Receiver<Option<bool>>,
    },
}

impl LaneItem {
    /// The routing facts of the item.
    pub fn routed(&self) -> &Routed {
        match self {
            Self::Message(routed) | Self::Declined { routed, .. } => routed,
        }
    }

    fn routed_mut(&mut self) -> &mut Routed {
        match self {
            Self::Message(routed) | Self::Declined { routed, .. } => routed,
        }
    }
}

/// One lease's abnormal-end signal (item 13.2): registered with the
/// connection, outside its task, so a dead connection task's end reaches
/// the lease's driver without the data path.
pub struct LeaseSignal {
    /// The last decode sequence the demux queued into this lease's lanes.
    enqueued: AtomicU64,
    /// The driver's handler: synchronous, idempotent, never blocking.
    on_abnormal: Box<dyn Fn(AbnormalEnd) + Send + Sync>,
    /// The driver's handler of a lane overflow, called at once by the
    /// connection task as it drops a message (x.3.2 X3 fix r2 #1): the
    /// same contract.
    on_overflow: Option<Box<dyn Fn(AbnormalEnd) + Send + Sync>>,
}

impl LeaseSignal {
    /// A signal calling `on_abnormal` at the abnormal end.
    pub fn new(on_abnormal: impl Fn(AbnormalEnd) + Send + Sync + 'static) -> Self {
        Self {
            enqueued: AtomicU64::new(0),
            on_abnormal: Box::new(on_abnormal),
            on_overflow: None,
        }
    }

    /// The signal calling `on_overflow` too, whenever a lane of the lease
    /// drops a message after it overflowed (synchronous, idempotent,
    /// never blocking).
    #[must_use]
    pub fn on_overflow(
        mut self,
        on_overflow: impl Fn(AbnormalEnd) + Send + Sync + 'static,
    ) -> Self {
        self.on_overflow = Some(Box::new(on_overflow));
        self
    }

    /// The last decode sequence queued into this lease's lanes.
    pub fn enqueued(&self) -> u64 {
        self.enqueued.load(Ordering::Acquire)
    }

    pub(super) fn queued(&self, seq: u64) {
        self.enqueued.fetch_max(seq, Ordering::AcqRel);
    }

    pub(super) fn signal(&self) {
        (self.on_abnormal)(AbnormalEnd {
            first_unqueued: self.enqueued().saturating_add(1),
        });
    }

    pub(super) fn overflowed(&self) {
        if let Some(on_overflow) = &self.on_overflow {
            on_overflow(AbnormalEnd {
                first_unqueued: self.enqueued().saturating_add(1),
            });
        }
    }
}

/// The abnormal end of a connection task, as one lease learns it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AbnormalEnd {
    /// The first decode sequence not queued into the lease's lanes: no
    /// earlier message of the lease was lost with the task.
    pub first_unqueued: u64,
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
    /// The connection task itself failed (item 13.2): the lane ends with
    /// no boundary, and what was staged in the task is lost.
    Abnormal,
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

/// The running turn's decode fence on a lane.
#[derive(Default)]
struct Fence {
    /// The fences set so far: the current one's number.
    count: u64,
    /// The running turn's watermark.
    decoded: Option<DecodeWatermark>,
}

/// One thread's ingress lane: the connection task pushes, one driver
/// takes.
#[derive(Default)]
pub struct Lane {
    queue: Mutex<Queue>,
    ready: Notify,
    fence: Mutex<Fence>,
    /// Set, for good, when the lane overflowed: observable at once,
    /// whether or not anything takes the lane (x.3.2 X3 fix r2 #1).
    overflow: watch::Sender<bool>,
}

impl Lane {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        // A push or take is one in-place edit: the state stays consistent.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fences the lane for a turn whose decode watermark is `decoded`
    /// (x.3.2 critical r2 #2, runtime §8): each message the lane takes
    /// from now on advances it and carries its position, under the
    /// returned fence number, until the next turn's fence replaces it.
    pub fn fence(&self, decoded: DecodeWatermark) -> u64 {
        let mut fence = self.fence.lock().unwrap_or_else(PoisonError::into_inner);
        fence.count = fence.count.saturating_add(1);
        fence.decoded = Some(decoded);
        fence.count
    }

    /// Routes `item`, charged `bytes`. A lane already ended drops it
    /// (counted); one that would pass [`LANE_MESSAGES`] or [`LANE_BYTES`]
    /// ends `Overflow` and drops it: the connection task never waits. A
    /// message it takes advances the running turn's decode watermark, if
    /// a turn fenced the lane, and carries its position.
    /// Whether the lane took it.
    pub fn push(&self, mut item: LaneItem, bytes: usize) -> bool {
        let mut queue = self.queue();
        if queue.end.is_some() {
            queue.dropped = queue.dropped.saturating_add(1);
            return false;
        }
        if queue.items.len() >= LANE_MESSAGES || queue.bytes.saturating_add(bytes) > LANE_BYTES {
            queue.end = Some(LaneEnd::Overflow);
            queue.dropped = queue.dropped.saturating_add(1);
            drop(queue);
            self.overflow.send_replace(true);
            self.ready.notify_one();
            return false;
        }
        let routed = item.routed_mut();
        routed.mark = {
            let fence = self.fence.lock().unwrap_or_else(PoisonError::into_inner);
            fence.decoded.as_ref().map(|decoded| Mark {
                fence: fence.count,
                seq: decoded.advance(),
            })
        };
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

    /// Whether the lane overflowed, however much of it was taken since.
    pub fn overflowed_now(&self) -> bool {
        *self.overflow.borrow()
    }

    /// Resolves once the lane overflowed (at once if it had).
    pub async fn overflowed(&self) {
        let mut overflow = self.overflow.subscribe();
        if overflow.wait_for(|overflowed| *overflowed).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Whether the lane has ended (messages may still be queued).
    pub fn ended(&self) -> Option<LaneEnd> {
        self.queue().end
    }

    /// The messages dropped after the lane ended.
    pub fn dropped(&self) -> u64 {
        self.queue().dropped
    }

    /// The decode sequence of the first message the lane holds, if any.
    pub fn front_seq(&self) -> Option<u64> {
        self.queue()
            .items
            .front()
            .map(|(item, _)| item.routed().seq)
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
        Routed,
    };

    fn item(text: &str) -> LaneItem {
        let line = format!("{{\"method\":\"x\",\"params\":{{\"note\":\"{text}\"}}}}\n");
        LaneItem::Message(Routed {
            staged: VendorMessage::new(BoundedBytes::try_from_message(line.into_bytes()).unwrap()),
            seq: 1,
            turn: None,
            owner: None,
            at: tokio::time::Instant::now(),
            mark: None,
        })
    }

    fn note(event: Option<LaneEvent>) -> Option<String> {
        match event? {
            LaneEvent::Item(item) => {
                Some(String::from_utf8(item.routed().staged.bytes().to_vec()).unwrap())
            }
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
