//! One Codex turn's delivery (x.3.2 X0 items 5, 10, 11, 12.5, 13.2): the
//! normalizer task that takes the registration's ingress lane in decode
//! order once the turn is accepted, and the turn's `DeliverySeal`.
//!
//! The normalizer runs on the session's tracker under `crash_on_panic`: a
//! panic in it is a VIA bug and aborts the daemon. It decodes each raw
//! message as it consumes it, so the message's staging permit is held
//! until then. A message of the turn is normalized and its observations
//! are handed to the C2 sink; the turn's terminal is retained in the
//! seal's slot, never sent. A message of another turn, or of none, gives
//! nothing to the turn; one that does not decode keeps its evidence in the
//! folder of the turn its correlation names, else in the server's, and
//! fails the generation `protocol`. A decline is reported only for the
//! turn it names, once its reply was written whole.
//!
//! The running turn never waits on the normalizer. It waits for the
//! seal's decision (the retained terminal, or why delivery stopped)
//! beside its own orders, and seals at whatever cutoff comes first: every
//! later output finds the seal and is refused, so nothing of the turn
//! reaches Core after `run_turn` returned. The seal reports the first
//! message it may have left undelivered, a conservative lower bound.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use via_routes::codex::{
    Connection, DECLINE_DEADLINE, Incoming, Lane, LaneEnd, LaneEvent, LaneItem, Notification,
    Routed, ServerRequest, TurnFolder, decode,
};

use super::normalize::{self, NormalizeError, Step, StructuredOutput, TurnNormalizer};
use crate::driver::latch;
use crate::observation::{
    Acceptance, Observation, ObservationItem, ObservationSink, Reserved, VendorTerminal, admitted,
};
use crate::runtime::event_stall;
use crate::{DriverFailure, DriverHealth, TurnActivity, TurnNumber, VendorTurnId};

/// `omitted` when the count of lost messages is unknown or saturated
/// (X0 item 10).
pub(crate) const UNKNOWN: u64 = u64::MAX;

/// The driver's sticky loss record (X0 item 10). C2 has no carrier for it
/// yet (`TurnEnd.loss`, `CloseReport.loss` and Core's `record_loss` are
/// x.3.2 X5's), so the driver holds it for diagnostics and tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObservationLoss {
    /// The turn the first loss affected.
    pub(crate) trigger: TurnNumber,
    /// Its connection generation.
    pub(crate) generation: u64,
    /// No message of the generation before it was lost.
    pub(crate) first_unqueued: u64,
    /// How many were lost; [`UNKNOWN`] when not known.
    pub(crate) omitted: u64,
}

/// The driver's loss facts, under a leaf lock: the record, and the
/// session's latest turn, which a new record names.
#[derive(Default)]
pub(crate) struct Losses {
    pub(crate) record: Option<ObservationLoss>,
    pub(crate) latest: Option<TurnNumber>,
}

impl Losses {
    /// Installs a loss of generation `generation` from `first_unqueued`
    /// on, or merges it into the record held (item 10's rule: the
    /// earliest position, the counts added or unknown, the trigger and
    /// generation kept).
    pub(crate) fn note(&mut self, generation: u64, first_unqueued: u64, omitted: u64) {
        if let Some(record) = self.record.as_mut() {
            record.first_unqueued = record.first_unqueued.min(first_unqueued);
            record.omitted = if record.omitted == UNKNOWN || omitted == UNKNOWN {
                UNKNOWN
            } else {
                record.omitted.saturating_add(omitted)
            };
            return;
        }
        if let Some(trigger) = self.latest {
            self.record = Some(ObservationLoss {
                trigger,
                generation,
                first_unqueued,
                omitted,
            });
        }
    }
}

/// Locks `losses`; each edit is one assignment.
pub(crate) fn losses(losses: &Mutex<Losses>) -> MutexGuard<'_, Losses> {
    losses.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The turn's retained terminal, never sent to the sink (C2 §4).
#[derive(Debug)]
pub(crate) struct Retained {
    pub(crate) terminal: VendorTerminal,
    pub(crate) structured: StructuredOutput,
}

/// Why delivery stopped before the turn's terminal.
#[derive(Debug)]
pub(crate) enum Stop {
    /// The lane ended after every message routed before its end.
    Lane(LaneEnd),
    /// A message of the generation contradicts the protocol or does not
    /// decode (X0 item 5 steps 5, 6): where its evidence was kept.
    Protocol {
        detail: &'static str,
        undecoded: Option<String>,
    },
    /// An ID past the normalizer's bounds, or the C2 sink stalled.
    Overflow,
}

/// What a seal found.
#[derive(Debug)]
pub(crate) struct Sealed {
    /// The first message the seal may have left undelivered.
    pub(crate) position: u64,
    /// The message being delivered was delivered only in part.
    pub(crate) partial: bool,
    pub(crate) terminal: Option<Retained>,
    pub(crate) tools_open: bool,
    pub(crate) stop: Option<Stop>,
}

struct Seal {
    sealed: Option<u64>,
    /// The decode sequence of the message being, or last, delivered.
    current: u64,
    /// Every output of `current` went out.
    complete: bool,
    terminal: Option<Retained>,
    tools_open: bool,
    stop: Option<Stop>,
}

/// One turn's `DeliverySeal` and decision slot (X0 item 13.2).
pub(crate) struct Delivery {
    seal: Mutex<Seal>,
    /// Wakes the turn when the decision changes.
    changed: Notify,
    /// Cancelled at the seal: a waiting output gives up at once.
    sealed: CancellationToken,
}

impl Delivery {
    /// A delivery whose last delivered message is `before`.
    pub(crate) fn new(before: u64) -> Arc<Self> {
        Arc::new(Self {
            seal: Mutex::new(Seal {
                sealed: None,
                current: before,
                complete: true,
                terminal: None,
                tools_open: false,
                stop: None,
            }),
            changed: Notify::new(),
            sealed: CancellationToken::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Seal> {
        // Each section is a few assignments: consistent across a panic.
        self.seal.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts delivering message `seq`; false once sealed.
    fn take(&self, seq: u64) -> bool {
        let mut seal = self.lock();
        if seal.sealed.is_some() {
            return false;
        }
        seal.current = seq;
        seal.complete = false;
        true
    }

    /// The message went out whole with no (further) output.
    fn complete(&self, tools_open: bool) -> bool {
        let mut seal = self.lock();
        if seal.sealed.is_some() {
            return false;
        }
        seal.complete = true;
        seal.tools_open = tools_open;
        true
    }

    /// Sends `item` into its reserved room unless sealed; `last` completes
    /// the message.
    fn send(&self, reserved: Reserved<'_>, item: ObservationItem, last: Option<bool>) -> bool {
        let mut seal = self.lock();
        if seal.sealed.is_some() {
            return false;
        }
        reserved.send(item);
        if let Some(tools_open) = last {
            seal.complete = true;
            seal.tools_open = tools_open;
        }
        true
    }

    /// Publishes the turn's terminal into the retained slot unless
    /// sealed; it completes its message.
    fn retain(&self, retained: Retained, tools_open: bool) -> bool {
        let mut seal = self.lock();
        if seal.sealed.is_some() {
            return false;
        }
        seal.terminal = Some(retained);
        seal.complete = true;
        seal.tools_open = tools_open;
        drop(seal);
        self.changed.notify_one();
        true
    }

    /// Records why delivery stopped, unless sealed.
    fn stop(&self, stop: Stop) {
        let mut seal = self.lock();
        if seal.sealed.is_some() || seal.stop.is_some() {
            return;
        }
        seal.stop = Some(stop);
        drop(seal);
        self.changed.notify_one();
    }

    /// Whether the turn's delivery reached a decision: its terminal, or
    /// why it stopped.
    pub(crate) fn decided(&self) -> bool {
        let seal = self.lock();
        seal.terminal.is_some() || seal.stop.is_some()
    }

    /// Resolves at the next decision change (a change since the last wait
    /// is kept).
    pub(crate) async fn changed(&self) {
        self.changed.notified().await;
    }

    /// Seals delivery: nothing more goes out. The position is fixed by
    /// the first call; the slots are taken once.
    pub(crate) fn seal(&self) -> Sealed {
        let mut seal = self.lock();
        let next = if seal.complete {
            seal.current.saturating_add(1)
        } else {
            seal.current
        };
        let position = *seal.sealed.get_or_insert(next);
        let sealed = Sealed {
            position,
            partial: !seal.complete,
            terminal: seal.terminal.take(),
            tools_open: seal.tools_open,
            stop: seal.stop.take(),
        };
        drop(seal);
        self.sealed.cancel();
        sealed
    }
}

/// Where a malformed message's evidence goes and which turn it names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Owner {
    /// The running turn.
    This,
    /// An earlier VIA turn of the session.
    Earlier(TurnNumber),
    /// A vendor turn the connection never mapped here.
    Unknown,
    /// No turn: thread-level traffic.
    Thread,
}

/// What the normalizer does after a message.
enum Flow {
    Next,
    Done,
}

/// [`Flow::Next`] while delivery is open.
fn flow(open: bool) -> Flow {
    if open { Flow::Next } else { Flow::Done }
}

/// The evidence folder of each turn a generation ran, by turn.
pub(crate) type Folders = Arc<Mutex<BTreeMap<TurnNumber, Arc<TurnFolder>>>>;

/// Where a malformed message's evidence is kept (X0 item 5): the turn's
/// folder, an earlier turn's of the generation, or the server folder.
pub(crate) struct Evidence {
    pub(crate) folder: Arc<TurnFolder>,
    pub(crate) connection: Arc<Connection>,
    pub(crate) earlier: Folders,
}

/// One accepted turn's normalizer task.
pub(crate) struct Normalizing {
    pub(crate) delivery: Arc<Delivery>,
    pub(crate) lane: Arc<Lane>,
    pub(crate) sink: ObservationSink,
    pub(crate) normalizer: TurnNormalizer,
    pub(crate) turn: TurnNumber,
    pub(crate) accepted: String,
    pub(crate) acceptance: Option<Acceptance>,
    pub(crate) evidence: Evidence,
    pub(crate) activity: TurnActivity,
    pub(crate) cancel: CancellationToken,
    pub(crate) health: Arc<watch::Sender<DriverHealth>>,
}

impl Normalizing {
    /// Delivers the acceptance, then the lane, until the turn's terminal,
    /// the seal, the lane's end or the session's cancellation.
    pub(crate) async fn run(mut self) {
        if let Some(acceptance) = self.acceptance.take()
            && !self.output(Observation::Accepted(acceptance), None).await
        {
            return;
        }
        loop {
            let event = tokio::select! {
                biased;
                () = self.delivery.sealed.cancelled() => return,
                () = self.cancel.cancelled() => return,
                event = self.lane.next() => event,
            };
            let item = match event {
                LaneEvent::Item(item) => *item,
                LaneEvent::End(end) => {
                    self.delivery.stop(Stop::Lane(end));
                    return;
                }
            };
            if matches!(self.message(item).await, Flow::Done) {
                return;
            }
        }
    }

    fn owner(&self, routed: &Routed) -> Owner {
        match (routed.owner, routed.turn.as_deref()) {
            (Some(owner), _) if owner == self.turn => Owner::This,
            (Some(owner), _) => Owner::Earlier(owner),
            (None, Some(turn)) if turn == self.accepted => Owner::This,
            (None, Some(_)) => Owner::Unknown,
            (None, None) => Owner::Thread,
        }
    }

    /// One lane item, decoded now; its staging permit goes with it.
    async fn message(&mut self, item: LaneItem) -> Flow {
        let owner = self.owner(item.routed());
        if !self.delivery.take(item.routed().seq) {
            return Flow::Done;
        }
        match item {
            LaneItem::Message(routed) => {
                let notification = match decode(routed.staged.bytes()) {
                    Ok(Incoming::Notification(notification)) => notification,
                    Ok(Incoming::Request(_) | Incoming::Response(_)) | Err(_) => {
                        return self.malformed(owner, routed.staged.bytes()).await;
                    }
                };
                drop(routed);
                self.notification(owner, &notification).await
            }
            LaneItem::Declined {
                routed,
                request,
                decoded_at,
                written,
            } => {
                let flow = self.declined(owner, &request, decoded_at, written).await;
                drop(routed);
                flow
            }
        }
    }

    /// X0 item 5 steps 5 and 6: the evidence goes to the turn the message
    /// names, else to the server folder; the generation fails `protocol`.
    async fn malformed(&self, owner: Owner, bytes: &[u8]) -> Flow {
        let undecoded = match owner {
            Owner::This => {
                let folder = &self.evidence.folder;
                folder.keep_undecoded(bytes, "the turn's message").await;
                folder.take_undecoded()
            }
            Owner::Earlier(turn) => {
                let folder = self
                    .evidence
                    .earlier
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&turn)
                    .cloned();
                match folder {
                    Some(folder) => {
                        folder
                            .keep_undecoded(bytes, "an earlier turn's message")
                            .await;
                        folder.take_undecoded()
                    }
                    None => None,
                }
            }
            Owner::Unknown | Owner::Thread => {
                self.evidence
                    .connection
                    .keep_undecoded(bytes, "the shared connection's message")
                    .await;
                Some(format!(
                    "{} bytes kept as the shared connection's evidence",
                    bytes.len()
                ))
            }
        };
        self.delivery.stop(Stop::Protocol {
            detail: "a message of the session's thread did not decode",
            undecoded,
        });
        Flow::Done
    }

    /// A decoded notification: the turn's, or thread-level, is
    /// normalized; another turn's gives the turn nothing.
    async fn notification(&mut self, owner: Owner, notification: &Notification) -> Flow {
        if matches!(owner, Owner::Earlier(_) | Owner::Unknown) {
            return flow(self.delivery.complete(self.normalizer.tools_open()));
        }
        let now = Instant::now();
        self.activity.record(now);
        let step = match self.normalizer.observe(notification, now) {
            Ok(step) => step,
            Err(NormalizeError::Protocol(detail)) => {
                self.delivery.stop(Stop::Protocol {
                    detail,
                    undecoded: None,
                });
                return Flow::Done;
            }
            Err(NormalizeError::Overflow) => {
                self.delivery.stop(Stop::Overflow);
                return Flow::Done;
            }
        };
        let tools_open = self.normalizer.tools_open();
        match step {
            Step::Activity => flow(self.delivery.complete(tools_open)),
            Step::Observations(observations) => {
                let count = observations.len();
                if count == 0 {
                    return flow(self.delivery.complete(tools_open));
                }
                for (index, observation) in observations.into_iter().enumerate() {
                    let last = (index + 1 == count).then_some(tools_open);
                    if !self.output(observation, last).await {
                        return Flow::Done;
                    }
                }
                Flow::Next
            }
            Step::Terminal {
                terminal,
                structured,
            } => {
                let retained = Retained {
                    terminal: *terminal,
                    structured,
                };
                self.delivery.retain(retained, tools_open);
                Flow::Done
            }
        }
    }

    /// X0 item 11: a placeholder of the turn is reported once its reply
    /// was written whole by `decoded_at + 5 s`; another turn's, or one
    /// naming none, gives the turn nothing.
    async fn declined(
        &mut self,
        owner: Owner,
        request: &ServerRequest,
        decoded_at: Instant,
        mut written: watch::Receiver<Option<bool>>,
    ) -> Flow {
        if owner != Owner::This {
            return flow(self.delivery.complete(self.normalizer.tools_open()));
        }
        self.activity.record(Instant::now());
        if self.normalizer.note_decline(request).is_err() {
            self.delivery.stop(Stop::Overflow);
            return Flow::Done;
        }
        let whole = tokio::select! {
            biased;
            () = self.delivery.sealed.cancelled() => return Flow::Done,
            outcome = tokio::time::timeout_at(
                decoded_at + DECLINE_DEADLINE,
                written.wait_for(Option::is_some),
            ) => outcome.is_ok_and(|outcome| outcome.is_ok_and(|outcome| *outcome == Some(true))),
        };
        let tools_open = self.normalizer.tools_open();
        if !whole {
            return flow(self.delivery.complete(tools_open));
        }
        let declined = Observation::RequestDeclined(normalize::decline(request));
        if self.output(declined, Some(tools_open)).await {
            Flow::Next
        } else {
            Flow::Done
        }
    }

    /// Hands one observation of the turn to the sink: its room reserved
    /// outside the seal, the send made under it. `last` (with the open
    /// tools) completes the message. A stall latches the observation
    /// overflow and stops delivery; false once nothing more goes out.
    async fn output(&self, observation: Observation, last: Option<bool>) -> bool {
        let item = ObservationItem {
            at: Instant::now(),
            vendor_turn: VendorTurnId::try_from(self.accepted.clone()).ok(),
            observation,
        };
        let reserved = tokio::select! {
            biased;
            () = self.delivery.sealed.cancelled() => return false,
            reserved = self.sink.reserve(&item, event_stall()) => reserved,
        };
        let Ok(reserved) = reserved else {
            latch(&self.health, DriverFailure::ObservationOverflow);
            self.delivery.stop(Stop::Overflow);
            return false;
        };
        if !self.delivery.send(reserved, item, last) {
            return false;
        }
        admitted().await;
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Delivery, Losses, ObservationLoss, Retained, UNKNOWN};
    use crate::TurnNumber;
    use crate::VendorTerminalStatus;
    use crate::observation::{
        Observation, ObservationItem, ObservationSink, StopReason, VendorTerminal,
        observation_channel,
    };

    fn item(text: &str) -> ObservationItem {
        ObservationItem {
            at: tokio::time::Instant::now(),
            vendor_turn: None,
            observation: Observation::FinalText(text.to_owned()),
        }
    }

    async fn send(delivery: &Delivery, sink: &ObservationSink, text: &str, last: bool) -> bool {
        let item = item(text);
        let reserved = sink.reserve(&item, Duration::from_secs(1)).await.unwrap();
        delivery.send(reserved, item, last.then_some(false))
    }

    fn terminal() -> Retained {
        Retained {
            terminal: VendorTerminal {
                at: tokio::time::Instant::now(),
                status: VendorTerminalStatus::Completed,
                stop_reason: StopReason::EndTurn,
                vendor_stop_reason: "completed".to_owned(),
                vendor_code: None,
                class_hint: None,
                detail: None,
                structured_output: None,
                structured_output_unparsed: None,
                steps: None,
                usage: None,
                cost: None,
                vendor: None,
            },
            structured: super::StructuredOutput::NotRequested,
        }
    }

    /// X0 item 13.2 (R9-2): a seal right after a message's final send
    /// reports the next position, and a second seal the same one.
    #[tokio::test]
    async fn seal_right_after_final_send_is_stable() {
        let (sink, mut received) = observation_channel();
        let delivery = Delivery::new(10);
        assert!(delivery.take(11));
        assert!(send(&delivery, &sink, "whole", true).await);
        let sealed = delivery.seal();
        assert_eq!((sealed.position, sealed.partial), (12, false));
        assert_eq!(delivery.seal().position, 12);
        assert!(received.try_recv().is_ok());
    }

    /// X0 item 13.2 (R8-4): sealed after the first of a message's two
    /// observations, the message is the first undelivered one; a message
    /// with no output before it does not move the position past it, and
    /// nothing more goes out.
    #[tokio::test]
    async fn seal_between_observations_of_one_message() {
        let (sink, mut received) = observation_channel();
        let delivery = Delivery::new(10);
        assert!(delivery.take(11));
        assert!(delivery.complete(false));
        assert!(delivery.take(12));
        assert!(send(&delivery, &sink, "first", false).await);
        let sealed = delivery.seal();
        assert_eq!((sealed.position, sealed.partial), (12, true));
        assert!(!send(&delivery, &sink, "second", true).await);
        assert!(!delivery.take(13));
        let delivered: Vec<_> = std::iter::from_fn(|| received.try_recv().ok()).collect();
        assert_eq!(delivered.len(), 1);
    }

    /// X0 item 13.2 (R8-3, R9-1): a terminal retained before the seal is
    /// the seal's; one published after it is refused, and decides nothing.
    #[test]
    fn retained_terminal_before_the_seal_only() {
        let delivery = Delivery::new(0);
        assert!(delivery.take(1));
        assert!(delivery.retain(terminal(), true));
        assert!(delivery.decided());
        let sealed = delivery.seal();
        assert!(sealed.terminal.is_some());
        assert!(sealed.tools_open);
        assert_eq!(sealed.position, 2);

        let late = Delivery::new(0);
        assert!(late.take(1));
        let sealed = late.seal();
        assert!(sealed.terminal.is_none());
        assert!(!late.retain(terminal(), false));
        assert!(!late.decided());
        assert!(late.seal().terminal.is_none());
    }

    /// X0 item 13.2: a full sink's wait gives up at the seal (the
    /// normalizer's output selects on it), and the room it waited for is
    /// never used.
    #[tokio::test]
    async fn seal_ends_an_output_waiting_on_a_full_sink() {
        let (sink, mut received) = observation_channel();
        let mut filled = 0_usize;
        loop {
            let item = item("fill");
            match tokio::time::timeout(
                Duration::from_millis(1),
                sink.reserve(&item, Duration::from_secs(10)),
            )
            .await
            {
                Ok(Ok(reserved)) => {
                    reserved.send(item);
                    filled += 1;
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert!(filled > 0);
        let delivery = Delivery::new(0);
        assert!(delivery.take(1));
        let waiting = {
            let delivery = &delivery;
            let sink = &sink;
            async move {
                let item = item("blocked");
                tokio::select! {
                    biased;
                    () = delivery.sealed.cancelled() => false,
                    reserved = sink.reserve(&item, Duration::from_secs(10)) => {
                        delivery.send(reserved.unwrap(), item, Some(false))
                    }
                }
            }
        };
        let sealing = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            delivery.seal()
        };
        let (sent, sealed) = tokio::join!(waiting, sealing);
        assert!(!sent);
        assert_eq!(sealed.position, 1);
        let drained: usize = std::iter::from_fn(|| received.try_recv().ok()).count();
        assert_eq!(drained, filled);
        assert!(received.try_recv().is_err());
    }

    /// X0 item 10 (R6-8): a later loss keeps the record's trigger and
    /// generation, the earliest position and an unknown count.
    #[test]
    fn successive_losses_keep_earliest_sequence() {
        let mut losses = Losses {
            record: None,
            latest: Some(TurnNumber::try_from(1).unwrap()),
        };
        losses.note(3, 101, UNKNOWN);
        losses.latest = Some(TurnNumber::try_from(2).unwrap());
        losses.note(3, 50, 4);
        assert_eq!(
            losses.record,
            Some(ObservationLoss {
                trigger: TurnNumber::try_from(1).unwrap(),
                generation: 3,
                first_unqueued: 50,
                omitted: UNKNOWN,
            })
        );
        let mut counted = Losses {
            record: None,
            latest: Some(TurnNumber::try_from(1).unwrap()),
        };
        counted.note(1, 7, 2);
        counted.note(1, 9, 3);
        assert_eq!(
            counted
                .record
                .map(|record| (record.first_unqueued, record.omitted)),
            Some((7, 5))
        );
    }
}
