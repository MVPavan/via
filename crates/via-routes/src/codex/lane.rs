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

use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use via_wire::{ExitReport, TurnNumber, VendorMessage, WireCleanup};

use crate::DecodeWatermark;

/// The most messages a lane holds.
pub const LANE_MESSAGES: usize = 16;

/// The most message bytes a lane holds.
pub const LANE_BYTES: usize = 1024 * 1024;

/// A `Start` marker's charge against [`LANE_BYTES`] (x.3.2 X3 §2.3).
pub const START_BYTES: usize = 256;

/// A `Reply` marker's charge beside its ID's bytes, and the charge a
/// retained item grows by (x.3.2 X3 §2.3).
pub const ENTRY_BYTES: usize = 64;

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

/// One item of a thread's lane, in decode order: a routed message, a
/// decline's placeholder, or one of a turn's markers (x.3.2 X3 §2.1).
pub enum LaneItem {
    /// A notification naming the thread.
    Message(Routed),
    /// A server request VIA declined at decode, in its decode position
    /// (item 11), kept raw (x.3.2 X3 §5.3: its full decode is the
    /// consumer's): the driver reports `vendor.request_declined` only once
    /// `written` says the reply was written whole, by `decoded_at + 5 s`.
    Declined {
        /// The request's own message: the placeholder's staging charge.
        routed: Routed,
        /// When it was decoded.
        decoded_at: Instant,
        /// `Some(true)` once the reply was written whole, `Some(false)`
        /// when it was not.
        written: watch::Receiver<Option<bool>>,
    },
    /// A turn's `turn/start` is being handed to Wire: every later item is
    /// counted under its decode fence.
    Start(Start),
    /// The paired reply of a turn's `turn/start`, at its decode position.
    Reply(Reply),
}

/// A turn's `Start` marker (x.3.2 X3 §2.1).
pub struct Start {
    /// The VIA turn.
    pub turn: TurnNumber,
    /// The decode fence its push installed ([`Mark::fence`]).
    pub fence: u64,
    /// The driver's context for the turn, opaque to the route.
    pub cx: Box<dyn Any + Send + Sync>,
}

/// A turn's `Reply` marker (x.3.2 X3 §2.1).
pub struct Reply {
    /// The VIA turn.
    pub turn: TurnNumber,
    /// Its position under the lane's decode fence.
    pub mark: Option<Mark>,
    /// When the connection read the reply.
    pub at: Instant,
    /// The vendor turn ID it accepted; `None` for an error reply.
    pub accepted: Option<String>,
}

impl LaneItem {
    /// The routing facts of a message or a placeholder; a marker has none.
    pub fn routed(&self) -> Option<&Routed> {
        match self {
            Self::Message(routed) | Self::Declined { routed, .. } => Some(routed),
            Self::Start(_) | Self::Reply(_) => None,
        }
    }

    fn routed_mut(&mut self) -> Option<&mut Routed> {
        match self {
            Self::Message(routed) | Self::Declined { routed, .. } => Some(routed),
            Self::Start(_) | Self::Reply(_) => None,
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
    /// The driver's close cut the lane off (item 8.2): what the lane took
    /// before is the admitted prefix; later messages are dropped and
    /// counted ([`Lane::dropped`]).
    Closed,
    /// The registration's generation failed (x.3.2 X3 §4.2 step 3):
    /// later messages are dropped and counted.
    Quarantined,
}

/// What [`Lane::next`] returns.
pub enum LaneEvent {
    /// The next item, and its charge: it counts against the lane's bounds
    /// until the charge drops (x.3.2 X3 §2.3).
    Item(Box<LaneItem>, LaneCharge),
    /// The lane's end, once every earlier item was taken.
    End(LaneEnd),
}

/// One taken item's charge against its lane's bounds: one message and its
/// bytes, released as it drops (x.3.2 X3 §2.3: a retained item keeps it).
pub struct LaneCharge {
    lane: Arc<Lane>,
    bytes: usize,
}

impl Drop for LaneCharge {
    fn drop(&mut self) {
        let mut queue = self.lane.queue();
        queue.count = queue.count.saturating_sub(1);
        queue.bytes = queue.bytes.saturating_sub(self.bytes);
    }
}

#[derive(Default)]
struct Queue {
    items: VecDeque<(LaneItem, usize)>,
    /// The messages charged: queued, or taken and still charged.
    count: usize,
    /// Their bytes.
    bytes: usize,
    end: Option<LaneEnd>,
    /// Messages refused after the end.
    dropped: u64,
    /// The fences set so far: the current one's number.
    fence: u64,
    /// The current fence's turn watermark, from its `Start`'s push.
    decoded: Option<DecodeWatermark>,
    /// The start gate (x.3.2 X3 §2.2): the turn whose `Start` was pushed
    /// and whose `Reply` was not yet taken.
    open_start: Option<TurnNumber>,
    /// An item naming an unmapped turn was pushed while a start was open
    /// (x.3.2 X3 §3.2, the refusal check); cleared by the next `Start`.
    early_seen: bool,
}

impl Queue {
    /// Whether a message of `bytes` fits.
    fn fits(&self, bytes: usize) -> bool {
        self.count < LANE_MESSAGES && self.bytes.saturating_add(bytes) <= LANE_BYTES
    }

    /// The next position under the current fence, if one is set.
    fn mark(&self) -> Option<Mark> {
        self.decoded.as_ref().map(|decoded| Mark {
            fence: self.fence,
            seq: decoded.advance(),
        })
    }

    /// Queues `item`, charged.
    fn queue(&mut self, item: LaneItem, bytes: usize) {
        self.count += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        self.items.push_back((item, bytes));
    }
}

/// One thread's ingress lane: the connection task pushes, one consumer
/// takes.
#[derive(Default)]
pub struct Lane {
    queue: Mutex<Queue>,
    ready: Notify,
    /// Set, for good, when the lane overflowed: observable at once,
    /// whether or not anything takes the lane (x.3.2 X3 fix r2 #1).
    overflow: watch::Sender<bool>,
    /// Whether the start gate is set (x.3.2 X3 §2.2), for a waiting
    /// driver; changed only under the queue's lock.
    gate: watch::Sender<bool>,
}

impl Lane {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        // A push or take is one in-place edit: the state stays consistent.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends the lane `Overflow`, as a refused push does.
    fn overflow(&self, mut queue: std::sync::MutexGuard<'_, Queue>) {
        if queue.end.is_none() {
            queue.end = Some(LaneEnd::Overflow);
        }
        Self::open_gate(&self.gate, &mut queue);
        drop(queue);
        self.overflow.send_replace(true);
        self.ready.notify_one();
    }

    /// Opens the start gate.
    fn open_gate(gate: &watch::Sender<bool>, queue: &mut Queue) {
        queue.open_start = None;
        gate.send_replace(false);
    }

    /// Routes `item` (a message or a placeholder), charged `bytes`. A lane
    /// already ended drops it (counted); one that would pass
    /// [`LANE_MESSAGES`] or [`LANE_BYTES`] ends `Overflow` and drops it:
    /// the connection task never waits. A message it takes advances the
    /// current turn's decode watermark, if a `Start` fenced the lane, and
    /// carries its position. Whether the lane took it.
    pub fn push(&self, mut item: LaneItem, bytes: usize) -> bool {
        let mut queue = self.queue();
        if queue.end.is_some() {
            queue.dropped = queue.dropped.saturating_add(1);
            return false;
        }
        if !queue.fits(bytes) {
            queue.dropped = queue.dropped.saturating_add(1);
            self.overflow(queue);
            return false;
        }
        let mark = queue.mark();
        if let Some(routed) = item.routed_mut() {
            routed.mark = mark;
            if queue.open_start.is_some() && routed.turn.is_some() && routed.owner.is_none() {
                queue.early_seen = true;
            }
        }
        queue.queue(item, bytes);
        drop(queue);
        self.ready.notify_one();
        true
    }

    /// x.3.2 X3 §2.2: pushes turn `turn`'s `Start` marker, fencing the lane
    /// for its decode watermark `decoded`, and sets the start gate.
    /// Refused (the start is not written) once the lane ended, when it is
    /// full, or while the gate is set. Whether it was pushed.
    pub fn push_start(
        &self,
        turn: TurnNumber,
        decoded: DecodeWatermark,
        cx: Box<dyn Any + Send + Sync>,
    ) -> bool {
        let mut queue = self.queue();
        if queue.end.is_some() || queue.open_start.is_some() || !queue.fits(START_BYTES) {
            return false;
        }
        queue.fence = queue.fence.saturating_add(1);
        queue.decoded = Some(decoded);
        queue.open_start = Some(turn);
        queue.early_seen = false;
        self.gate.send_replace(true);
        let fence = queue.fence;
        queue.queue(LaneItem::Start(Start { turn, fence, cx }), START_BYTES);
        drop(queue);
        self.ready.notify_one();
        true
    }

    /// x.3.2 X3 §2.1: pushes the `Reply` marker of turn `turn`'s
    /// `turn/start`, read at `at`, at its decode position: one that does
    /// not fit ends the lane `Overflow`. Whether an item naming an
    /// unmapped turn was pushed while the start was open (the refusal
    /// check), and whether it was pushed.
    pub fn push_reply(
        &self,
        turn: TurnNumber,
        at: Instant,
        accepted: Option<String>,
    ) -> (bool, bool) {
        let mut queue = self.queue();
        let contradicted = queue.early_seen;
        if queue.end.is_some() {
            queue.dropped = queue.dropped.saturating_add(1);
            return (contradicted, false);
        }
        let bytes = ENTRY_BYTES.saturating_add(accepted.as_ref().map_or(0, String::len));
        if !queue.fits(bytes) {
            queue.dropped = queue.dropped.saturating_add(1);
            self.overflow(queue);
            return (contradicted, false);
        }
        let mark = queue.mark();
        queue.queue(
            LaneItem::Reply(Reply {
                turn,
                mark,
                at,
                accepted,
            }),
            bytes,
        );
        drop(queue);
        self.ready.notify_one();
        (contradicted, true)
    }

    /// x.3.2 X3 §2.2: turn `turn`'s start was positively not written after
    /// its `Start` was pushed: the gate opens if it is that turn's.
    /// Idempotent.
    pub fn start_unwritten(&self, turn: TurnNumber) {
        let mut queue = self.queue();
        if queue.open_start == Some(turn) {
            Self::open_gate(&self.gate, &mut queue);
        }
    }

    /// The turn whose start holds the gate, if any.
    pub fn open_start(&self) -> Option<TurnNumber> {
        self.queue().open_start
    }

    /// Resolves once the start gate is open (at once if it is).
    pub async fn start_gate(&self) {
        let mut gate = self.gate.subscribe();
        if gate.wait_for(|set| !*set).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// x.3.2 X3 §2.3 (r12 #2): a retained item's `charge` grows by
    /// `bytes`, without waiting. One that does not fit ends the lane
    /// `Overflow` at once, as a refused push does. Whether it grew.
    pub fn try_grow(&self, charge: &mut LaneCharge, bytes: usize) -> bool {
        let mut queue = self.queue();
        if queue.bytes.saturating_add(bytes) > LANE_BYTES {
            self.overflow(queue);
            return false;
        }
        queue.bytes = queue.bytes.saturating_add(bytes);
        charge.bytes = charge.bytes.saturating_add(bytes);
        true
    }

    /// The messages and bytes the lane holds charged.
    pub fn charged(&self) -> (usize, usize) {
        let queue = self.queue();
        (queue.count, queue.bytes)
    }

    /// Ends the lane with `end` after what it holds, unless it already
    /// ended: the first end stays. The start gate opens.
    pub fn end(&self, end: LaneEnd) {
        let mut queue = self.queue();
        if queue.end.is_none() {
            queue.end = Some(end);
        }
        Self::open_gate(&self.gate, &mut queue);
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
            .iter()
            .find_map(|(item, _)| item.routed().map(|routed| routed.seq))
    }

    /// Takes the next item without waiting, with its charge, or the end
    /// once the lane holds no more; `None` when it is empty and open.
    /// Taking the gate's turn's `Reply` opens the start gate.
    pub fn try_next(self: &Arc<Self>) -> Option<LaneEvent> {
        let mut queue = self.queue();
        let Some((item, bytes)) = queue.items.pop_front() else {
            return queue.end.map(LaneEvent::End);
        };
        if let LaneItem::Reply(reply) = &item
            && queue.open_start == Some(reply.turn)
        {
            Self::open_gate(&self.gate, &mut queue);
        }
        drop(queue);
        let charge = LaneCharge {
            lane: Arc::clone(self),
            bytes,
        };
        Some(LaneEvent::Item(Box::new(item), charge))
    }

    /// The next item, or the lane's end once it holds no more. Cancel
    /// safe: an item is taken only when this returns it.
    pub async fn next(self: &Arc<Self>) -> LaneEvent {
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
    use std::sync::Arc;

    use via_wire::{BoundedBytes, TurnNumber, VendorMessage, WireCleanup};

    use super::{
        ConnectionLoss, ENTRY_BYTES, LANE_BYTES, LANE_MESSAGES, Lane, LaneEnd, LaneEvent, LaneItem,
        LossCause, Mark, Routed, START_BYTES,
    };
    use crate::DecodeWatermark;

    fn item(text: &str) -> LaneItem {
        named(text, None)
    }

    /// A message naming vendor turn `turn`, unmapped.
    fn named(text: &str, turn: Option<&str>) -> LaneItem {
        let line = format!("{{\"method\":\"x\",\"params\":{{\"note\":\"{text}\"}}}}\n");
        LaneItem::Message(Routed {
            staged: VendorMessage::new(BoundedBytes::try_from_message(line.into_bytes()).unwrap()),
            seq: 1,
            turn: turn.map(str::to_owned),
            owner: None,
            at: tokio::time::Instant::now(),
            mark: None,
        })
    }

    fn turn(number: u32) -> TurnNumber {
        TurnNumber::try_from(number).unwrap()
    }

    fn lane() -> Arc<Lane> {
        Arc::new(Lane::default())
    }

    fn note(event: Option<LaneEvent>) -> Option<String> {
        match event? {
            LaneEvent::Item(item, _) => {
                Some(String::from_utf8(item.routed().unwrap().staged.bytes().to_vec()).unwrap())
            }
            LaneEvent::End(_) => None,
        }
    }

    fn ended(event: Option<LaneEvent>) -> Option<LaneEnd> {
        match event? {
            LaneEvent::End(end) => Some(end),
            LaneEvent::Item(..) => None,
        }
    }

    /// Sixteen messages fit; the seventeenth ends the lane `Overflow`
    /// without waiting, after the sixteen, and is dropped and counted.
    #[test]
    fn lane_overflows_past_sixteen_messages() {
        let lane = lane();
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
        let lane = lane();
        assert!(lane.push(item("big"), LANE_BYTES - 1));
        assert!(lane.push(item("one"), 1));
        assert!(!lane.push(item("two"), 1));
        assert_eq!(lane.ended(), Some(LaneEnd::Overflow));
    }

    /// An end comes after every message routed before it; the first end
    /// stays.
    #[tokio::test]
    async fn lane_end_follows_its_messages() {
        let lane = lane();
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
        let lane = lane();
        let taker = {
            let lane = Arc::clone(&lane);
            tokio::spawn(async move { note(Some(lane.next().await)) })
        };
        tokio::task::yield_now().await;
        assert!(lane.push(item("woken"), 1));
        assert!(taker.await.unwrap().unwrap().contains("woken"));
    }

    /// x.3.2 X3 §2.3 (owner's r10 ruling): a taken item keeps its charge
    /// against the lane's bounds until the charge drops, so retained items,
    /// queued items and markers share the lane's 16 messages.
    #[test]
    fn a_kept_charge_counts_against_the_lane() {
        let lane = lane();
        let mut kept = Vec::new();
        for index in 0..LANE_MESSAGES {
            assert!(lane.push(item(&index.to_string()), 10));
            let Some(LaneEvent::Item(_, charge)) = lane.try_next() else {
                panic!("no item");
            };
            kept.push(charge);
        }
        assert_eq!(lane.charged(), (LANE_MESSAGES, LANE_MESSAGES * 10));
        assert!(
            !lane.push(item("seventeenth"), 10),
            "the kept charges fill it"
        );
        assert_eq!(lane.ended(), Some(LaneEnd::Overflow));
        drop(kept);
        assert_eq!(lane.charged(), (0, 0));

        let lane = self::lane();
        assert!(lane.push(item("taken"), 10));
        drop(lane.try_next());
        assert_eq!(lane.charged(), (0, 0), "a dropped charge is released");
    }

    /// x.3.2 X3 §2.3 (r12 #2), S12 (d): a retained item's charge grows by
    /// 64 B without waiting. At `LANE_BYTES - 64` it fits; one byte more
    /// ends the lane `Overflow` at once, with no later push.
    #[test]
    fn retention_growth_ends_the_lane_past_its_bytes() {
        let lane = lane();
        assert!(lane.push(item("held"), LANE_BYTES - ENTRY_BYTES));
        let Some(LaneEvent::Item(_, mut charge)) = lane.try_next() else {
            panic!("no item");
        };
        assert!(lane.try_grow(&mut charge, ENTRY_BYTES));
        assert_eq!(lane.charged(), (1, LANE_BYTES));
        assert!(!lane.overflowed_now());
        drop(charge);

        let lane = self::lane();
        assert!(lane.push(item("held"), LANE_BYTES - ENTRY_BYTES + 1));
        let Some(LaneEvent::Item(_, mut charge)) = lane.try_next() else {
            panic!("no item");
        };
        assert!(!lane.try_grow(&mut charge, ENTRY_BYTES));
        assert!(lane.overflowed_now());
        assert_eq!(lane.ended(), Some(LaneEnd::Overflow));
    }

    /// x.3.2 X3 §2.2 (r10 #2): a `Start` sets the start gate and fences the
    /// lane; a second `Start` is refused while it is set. Taking the
    /// turn's `Reply` opens it, as does the start's positive `NotWritten`
    /// (only for that turn) and the lane's end, which then refuses every
    /// `Start`.
    #[tokio::test]
    async fn the_start_gate_admits_one_start() {
        let lane = lane();
        let decoded = DecodeWatermark::default();
        assert!(lane.push_start(turn(1), decoded.clone(), Box::new(())));
        assert_eq!(lane.open_start(), Some(turn(1)));
        assert!(
            !lane.push_start(turn(2), DecodeWatermark::default(), Box::new(())),
            "the gate refuses a second start"
        );
        assert!(lane.push(item("one"), 1));
        let (contradicted, pushed) = lane.push_reply(turn(1), tokio::time::Instant::now(), None);
        assert!(pushed && !contradicted);
        assert_eq!(decoded.get(), 2, "the message and the reply count");
        let Some(LaneEvent::Item(start, _)) = lane.try_next() else {
            panic!("no start");
        };
        let LaneItem::Start(start) = *start else {
            panic!("not a start");
        };
        assert_eq!(start.turn, turn(1));
        let Some(LaneEvent::Item(message, _)) = lane.try_next() else {
            panic!("no message");
        };
        assert_eq!(
            message.routed().unwrap().mark,
            Some(Mark {
                fence: start.fence,
                seq: 1
            })
        );
        let waiting = {
            let lane = Arc::clone(&lane);
            tokio::spawn(async move { lane.start_gate().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "the reply is not taken");
        let Some(LaneEvent::Item(reply, _)) = lane.try_next() else {
            panic!("no reply");
        };
        assert!(matches!(*reply, LaneItem::Reply(ref reply)
            if reply.mark == Some(Mark { fence: start.fence, seq: 2 })));
        assert_eq!(lane.open_start(), None, "taking the reply opens the gate");
        waiting.await.unwrap();

        assert!(lane.push_start(turn(2), DecodeWatermark::default(), Box::new(())));
        lane.start_unwritten(turn(1));
        assert_eq!(
            lane.open_start(),
            Some(turn(2)),
            "only the gate's turn opens it"
        );
        lane.start_unwritten(turn(2));
        assert_eq!(lane.open_start(), None);
        lane.start_unwritten(turn(2));

        assert!(lane.push_start(turn(3), DecodeWatermark::default(), Box::new(())));
        lane.end(LaneEnd::Closed);
        assert_eq!(lane.open_start(), None, "the end opens the gate");
        assert!(!lane.push_start(turn(4), DecodeWatermark::default(), Box::new(())));
    }

    /// x.3.2 X3 §2.3, r7 #8, r8 #8: with 16 data items queued a `Start` is
    /// refused (no launch) and the lane lives; with 15 it takes the 16th
    /// slot and its `Reply` overflows the lane.
    #[test]
    fn markers_count_against_the_lane() {
        let lane = lane();
        for index in 0..LANE_MESSAGES {
            assert!(lane.push(item(&index.to_string()), 1));
        }
        assert!(!lane.push_start(turn(1), DecodeWatermark::default(), Box::new(())));
        assert_eq!(lane.ended(), None, "a refused start ends nothing");
        assert_eq!(lane.open_start(), None);

        let lane = self::lane();
        for index in 0..LANE_MESSAGES - 1 {
            assert!(lane.push(item(&index.to_string()), 1));
        }
        assert!(lane.push_start(turn(1), DecodeWatermark::default(), Box::new(())));
        assert_eq!(
            lane.charged(),
            (LANE_MESSAGES, LANE_MESSAGES - 1 + START_BYTES)
        );
        let (_, pushed) = lane.push_reply(turn(1), tokio::time::Instant::now(), Some("u".into()));
        assert!(!pushed);
        assert_eq!(lane.ended(), Some(LaneEnd::Overflow));
        assert!(lane.overflowed_now());
    }

    /// x.3.2 X3 §3.2, the refusal check: an item naming an unmapped turn
    /// pushed while a start is open is seen by that start's reply; the next
    /// `Start` clears it, and one pushed with no start open is not seen.
    #[test]
    fn an_unmapped_item_under_an_open_start_is_seen() {
        let lane = lane();
        assert!(lane.push(named("before", Some("u")), 1));
        assert!(lane.push_start(turn(1), DecodeWatermark::default(), Box::new(())));
        assert!(lane.push(item("thread-level"), 1));
        let (contradicted, _) = lane.push_reply(turn(1), tokio::time::Instant::now(), None);
        assert!(!contradicted, "nothing unmapped while the start was open");
        while let Some(LaneEvent::Item(..)) = lane.try_next() {}

        assert!(lane.push_start(turn(2), DecodeWatermark::default(), Box::new(())));
        assert!(lane.push(named("early", Some("u2")), 1));
        let (contradicted, _) = lane.push_reply(turn(2), tokio::time::Instant::now(), None);
        assert!(contradicted);
        while let Some(LaneEvent::Item(..)) = lane.try_next() {}
        assert!(lane.push_start(turn(3), DecodeWatermark::default(), Box::new(())));
        let (contradicted, _) = lane.push_reply(turn(3), tokio::time::Instant::now(), None);
        assert!(!contradicted, "the next start clears it");
    }
}
