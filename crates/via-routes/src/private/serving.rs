//! Route's side of a running private turn: every wait on the vendor, the
//! Adapter or a write goes through [`Serving::serve`], so no wait hides a
//! control (Task 4 design §9); the stop waker, the interrupt, and the
//! failure causes Wire's errors map to.

use std::collections::VecDeque;
use std::future::Future;

use tokio::sync::{mpsc, watch};

use super::{Closed, ForceWatch, PrivateProtocol};
use crate::{Deadline, RouteError, RouteFailure, SendOutcome, StopWatch, StoreFailure, TurnNumber};
use via_wire::{
    ExitReport, FailureCause, HostError, LatchState, PendingWrite, WireError, WireFailure,
    WireMessages, WireSender,
};

/// The bytes of decoded messages that may wait for room on the hop: the
/// route data bound (runtime §8).
const READ_AHEAD_BYTES: usize = 4 * 1024 * 1024;

/// Decoded messages waiting for room on the hop, in decode order, with
/// their encoded bytes: at most a protocol's read-ahead count and
/// [`READ_AHEAD_BYTES`]; one message alone may pass the bytes (Wire's
/// line cap bounds it).
pub(crate) struct ReadAhead<M> {
    held: VecDeque<(M, usize)>,
    bytes: usize,
}

impl<M> ReadAhead<M> {
    fn new() -> Self {
        Self {
            held: VecDeque::new(),
            bytes: 0,
        }
    }

    fn push(&mut self, (message, bytes): (M, usize)) {
        self.bytes = self.bytes.saturating_add(bytes);
        self.held.push_back((message, bytes));
    }

    fn pop(&mut self) -> Option<M> {
        let (message, bytes) = self.held.pop_front()?;
        self.bytes = self.bytes.saturating_sub(bytes);
        Some(message)
    }

    fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Whether a message of `bytes` may join, under `ahead` messages: it
    /// counts against the bytes before it is held.
    fn admits(&self, bytes: usize, ahead: usize) -> bool {
        self.held.is_empty()
            || (self.held.len() < ahead && self.bytes.saturating_add(bytes) <= READ_AHEAD_BYTES)
    }
}

/// S1's cleanup allowance after a failure (AD4's one cutoff after the wall).
pub(crate) const CLEANUP_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(3);

/// Bounds Host cleanup and the stdout drain separately from the turn
/// deadline, which may already have elapsed when cleanup starts.
pub(crate) fn cleanup_deadline() -> Deadline {
    Deadline::at(tokio::time::Instant::now() + CLEANUP_ALLOWANCE)
}

/// The turn's control signals.
pub(crate) struct Signals {
    /// The daemon force.
    pub(crate) force: ForceWatch,
    /// The turn's stop order.
    pub(crate) stop: StopWatch,
    /// Route's wake for a new stop order or its `force_at`.
    pub(crate) wake: watch::Receiver<u64>,
}

/// The one interrupt's write (design §2 rule 3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Interrupt {
    /// Not sent.
    NotSent,
    /// Enqueued; its write has not answered.
    Queued,
    /// Written whole.
    Written,
    /// Not written whole: the vendor was not asked.
    Failed,
}

/// The Adapter's stall (C2 A1), taken as an internal stop order: the
/// connection fails `overflow`, the vendor is interrupted through the
/// protocol's path, and the turn runs on with its observations discarded:
/// a terminal is still kept (AD4), the daemon force and stop orders are
/// still served, and at `force_at` without a terminal the group is
/// force-closed under `close_by`, the cleanup escalation's bounds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stall {
    force_at: Deadline,
    close_by: Deadline,
}

/// What [`Serving::next`] read: a message admitted for the hop, with its
/// encoded bytes.
pub(crate) enum Next<M> {
    /// One decoded vendor message.
    Message((M, usize)),
    /// Stdout ended.
    Eof,
    /// Stdout ended inside a message; Wire kept its bytes (F21, design §7.3).
    Unterminated,
}

/// First failure inside the lifecycle, before cleanup.
pub(crate) struct Failed {
    pub(crate) cause: RouteError,
    pub(crate) exit: Option<ExitReport>,
    /// A stop order's bound on the force close and drain.
    pub(crate) close_by: Option<Deadline>,
}

impl Failed {
    /// A stop order forcing the turn, closed under `close_by`.
    pub(crate) fn stopped(turn: TurnNumber, close_by: Deadline) -> Self {
        Self {
            cause: RouteError::Stopped { turn },
            exit: None,
            close_by: Some(close_by),
        }
    }
}

impl From<RouteError> for Failed {
    fn from(cause: RouteError) -> Self {
        Self {
            cause,
            exit: None,
            close_by: None,
        }
    }
}

/// Route's side of a running turn: every wait on the vendor, the Adapter
/// or a write goes through [`Self::serve`], so no wait hides a control
/// (Task 4 design §9).
pub(crate) struct Serving<'a, P: PrivateProtocol> {
    pub(crate) turn: TurnNumber,
    pub(crate) sender: &'a WireSender,
    pub(crate) hop: &'a mpsc::Sender<P::Message>,
    pub(crate) deadline: Deadline,
    pub(crate) signals: Signals,
    pub(crate) latch: watch::Receiver<LatchState>,
    /// The start's write began: a stop order now sends the interrupt.
    pub(crate) submitted: bool,
    /// The start was written whole: the Adapter's stall now interrupts
    /// the vendor ([`Stall`]); before, it fails the turn at once and the
    /// start, if not yet enqueued, is never written.
    pub(crate) written: bool,
    /// The terminal was read: a stop order no longer acts.
    pub(crate) terminated: bool,
    /// The one interrupt's write.
    pub(crate) interrupt: Interrupt,
    /// The pending interrupt write, kept pinned while other waits run.
    pub(crate) pending: Option<PendingWrite>,
    /// Decoded messages waiting for room on the hop.
    held: ReadAhead<P::Message>,
    /// The Adapter stalled: the hop is closed, and nothing more goes on it.
    pub(crate) stall: Option<Stall>,
    /// The protocol's state.
    pub(crate) lane: P,
}

impl<'a, P: PrivateProtocol> Serving<'a, P> {
    pub(crate) fn new(
        turn: TurnNumber,
        sender: &'a WireSender,
        hop: &'a mpsc::Sender<P::Message>,
        deadline: Deadline,
        signals: Signals,
        lane: P,
    ) -> Self {
        Self {
            turn,
            sender,
            hop,
            deadline,
            latch: sender.latch(),
            signals,
            submitted: false,
            written: false,
            terminated: false,
            interrupt: Interrupt::NotSent,
            pending: None,
            held: ReadAhead::new(),
            stall: None,
            lane,
        }
    }

    /// The protocol's state at the turn's end, with the interrupt's.
    pub(crate) fn into_lane(self) -> (P, Interrupt) {
        (self.lane, self.interrupt)
    }

    /// Queues a decoded message for the hop, behind those already held;
    /// after the Adapter's stall it is discarded.
    pub(crate) fn hold(&mut self, (message, bytes): (P::Message, usize)) {
        if self.stall.is_none() {
            self.held.push((message, bytes));
        }
    }

    /// Whether a decoded message waits for the hop.
    pub(crate) fn holds(&self) -> bool {
        !self.held.is_empty()
    }

    /// The oldest held message, its bytes released.
    fn take_held(&mut self) -> Option<P::Message> {
        self.held.pop()
    }

    /// Awaits `op` while servicing every control, biased (design §9):
    /// (1) daemon force; (2) turn deadline; (3) the connection latch;
    /// (4) the hop closed → `Overflow`; (5) Route's wake → [`Self::on_wake`];
    /// (6) the pending interrupt completing; (7) the protocol's own control
    /// events; (8) room on the hop, which sends the oldest held message;
    /// (9) `op`. Every arm is cancel-safe: `op` and the pending write stay
    /// pinned, the reserve holds no message.
    pub(crate) async fn serve<T>(&mut self, op: impl Future<Output = T>) -> Result<T, Failed> {
        let mut op = std::pin::pin!(op);
        loop {
            if let Some(output) = self.serve_once(op.as_mut()).await? {
                return Ok(output);
            }
        }
    }

    /// Serves until every held message is on the hop, reading nothing more:
    /// Route blocked on the hop stops calling `next_message` (design §2.3).
    pub(crate) async fn flush(&mut self) -> Result<(), Failed> {
        let mut never = std::pin::pin!(std::future::pending::<()>());
        while self.holds() {
            self.serve_once(never.as_mut()).await?;
        }
        Ok(())
    }

    /// Serves until the read-ahead has room: up to the terminal, the
    /// protocol's [`PrivateProtocol::READ_AHEAD`] messages; after it, none
    /// may wait, so what the turn's end hands on is on the hop first.
    async fn make_room(&mut self) -> Result<(), Failed> {
        let ahead = if self.terminated { 1 } else { P::READ_AHEAD };
        self.make_room_for(0, ahead).await
    }

    /// Serves until a message of `bytes` fits a read-ahead of `ahead`
    /// messages (review r1 #4: the incoming message counts before it is
    /// held).
    pub(crate) async fn make_room_for(&mut self, bytes: usize, ahead: usize) -> Result<(), Failed> {
        let mut never = std::pin::pin!(std::future::pending::<()>());
        while !self.held.admits(bytes, ahead) {
            self.serve_once(never.as_mut()).await?;
        }
        Ok(())
    }

    /// The late path's delivery (design §2 rule 3 [r1.23]): the held
    /// messages go on the hop in order as the reserve arm of
    /// [`Self::serve_once`] sends them. Only room on the hop, a closed hop
    /// or `by` ends the wait; no other control acts on an already decoded
    /// message. Delivery that cannot finish by `by` is `Overflow`. `last`,
    /// a decoded terminal still waiting for read-ahead room, goes last.
    pub(crate) async fn deliver_held(
        &mut self,
        by: Deadline,
        mut last: Option<P::Message>,
    ) -> Result<(), RouteError> {
        while let Some(message) = self.take_held().or_else(|| last.take()) {
            let turn = self.turn;
            let hop = self.hop;
            tokio::select! {
                biased;
                permit = hop.reserve() => match permit {
                    Ok(permit) => permit.send(message),
                    Err(_) => return Err(self.hop_closed().cause),
                },
                () = tokio::time::sleep_until(by.instant()) => {
                    return Err(RouteError::Overflow { turn });
                }
            }
        }
        Ok(())
    }

    /// `result`, unless the daemon force is set: then `ForceStopped` with
    /// the result's exit and close evidence (design §2 rule 4); or unless
    /// the Adapter stalled: then `Overflow`, with the same evidence.
    pub(crate) fn unless_forced(
        &self,
        result: P::Result,
        undecoded: Option<String>,
    ) -> Result<P::Result, RouteFailure> {
        if self.signals.force.borrow().is_some() || self.stall.is_some() {
            // `failure_with` lets the daemon force outrank the stall.
            let cause = RouteError::Overflow { turn: self.turn };
            return Err(self.failure_with(cause, &result, undecoded));
        }
        Ok(result)
    }

    /// A failure after a decoded terminal, with that terminal's exit and
    /// close evidence; the daemon force outranks any other `cause`.
    pub(crate) fn failure_with(
        &self,
        cause: RouteError,
        result: &P::Result,
        undecoded: Option<String>,
    ) -> RouteFailure {
        let cause = if self.signals.force.borrow().is_some() {
            RouteError::ForceStopped { turn: self.turn }
        } else {
            cause
        };
        let Closed {
            exit,
            cleanup,
            journal_uncertain,
            forced,
        } = P::evidence(result);
        RouteFailure {
            cause,
            undecoded,
            exit: (exit.code.is_some() || exit.signal.is_some()).then_some(exit),
            launched: true,
            cleanup: Some(cleanup),
            forced,
            journal_uncertain,
            acknowledged: false,
            shared: false,
        }
    }

    /// One round of [`Self::serve`]: `Some` once `op` completed.
    pub(crate) async fn serve_once<T>(
        &mut self,
        op: std::pin::Pin<&mut impl Future<Output = T>>,
    ) -> Result<Option<T>, Failed> {
        let turn = self.turn;
        let hop = self.hop;
        let has_event = self.lane.has_event();
        tokio::select! {
            biased;
            () = forced(&mut self.signals.force) => Err(RouteError::ForceStopped { turn }.into()),
            () = tokio::time::sleep_until(self.deadline.instant()) => {
                Err(RouteError::Deadline { turn }.into())
            }
            cause = latched(&mut self.latch) => Err(wire_cause::<P>(turn, &cause.error()).into()),
            () = hop.closed(), if self.stall.is_none() => self.on_hop_closed().map(|()| None),
            () = woken(&mut self.signals.wake) => self.on_wake().map(|()| None),
            () = stall_force(self.stall.as_ref(), self.terminated) => Err(self.stall_force()),
            written = pending(self.pending.as_mut()), if self.pending.is_some() => {
                self.interrupt_written(&written).map(|()| None)
            }
            event = self.lane.event(), if has_event => {
                P::on_event(self, event).map(|()| None)
            }
            permit = hop.reserve(), if self.holds() => match (permit, self.take_held()) {
                (Ok(permit), Some(message)) => {
                    permit.send(message);
                    Ok(None)
                }
                (Ok(_), None) => Ok(None),
                (Err(_), _) => self.on_hop_closed().map(|()| None),
            },
            output = op => Ok(Some(output)),
        }
    }

    /// Reads and decodes the next vendor message once the read-ahead has
    /// room, and has the protocol admit it. One Route cannot decode is
    /// kept in `undecoded.bin` first (design §7.3).
    pub(crate) async fn next(
        &mut self,
        messages: &mut WireMessages,
    ) -> Result<Next<P::Message>, Failed> {
        let turn = self.turn;
        loop {
            self.make_room().await?;
            let message = match self.serve(messages.next_message()).await? {
                Ok(Some(message)) => message,
                Ok(None) => return Ok(Next::Eof),
                // Route's own wake arm acts on it; nothing was lost.
                Err(WireError::Woken) => continue,
                Err(WireError::Message(WireFailure::UnterminatedMessage)) => {
                    return Ok(Next::Unterminated);
                }
                Err(error) => return Err(Failed::from(wire_cause::<P>(turn, &error))),
            };
            return match P::decode(message.bytes(), turn) {
                Ok(payload) => {
                    let bytes = message.bytes().len();
                    match P::admit(self, payload).await? {
                        Some(admitted) => {
                            // S1 rule 3 (review r2 #4): the turn's first
                            // terminal waits for room only once its reader
                            // retained it and marked the turn terminated.
                            if self.terminated || P::terminal(&admitted).is_none() {
                                self.make_room_for(bytes, P::READ_AHEAD).await?;
                            }
                            Ok(Next::Message((admitted, bytes)))
                        }
                        // Recorded, not handed over (C2 §2 Reopen).
                        None => continue,
                    }
                }
                Err(cause) => {
                    let what = format!(
                        "undecodable vendor message: {} bytes",
                        message.bytes().len()
                    );
                    self.sender.keep_undecoded(message.bytes(), &what).await;
                    Err(Failed::from(cause))
                }
            };
        }
    }

    /// A closed hop: the Adapter's stall (overflow), or its stop under the
    /// daemon force.
    pub(crate) fn hop_closed(&self) -> Failed {
        let turn = self.turn;
        if self.signals.force.borrow().is_some() {
            RouteError::ForceStopped { turn }.into()
        } else {
            RouteError::Overflow { turn }.into()
        }
    }

    /// The hop closed: before the terminal of a turn whose start was written
    /// whole and whose protocol interrupts on it, the Adapter's stall
    /// ([`Stall`]): the held messages are discarded and the one interrupt is
    /// sent; otherwise [`Self::hop_closed`].
    fn on_hop_closed(&mut self) -> Result<(), Failed> {
        let stalls = self.written
            && !self.terminated
            && self.signals.force.borrow().is_none()
            && self.lane.interrupts_on_stall();
        if !stalls {
            return Err(self.hop_closed());
        }
        let force_at = cleanup_deadline();
        self.stall = Some(Stall {
            force_at,
            close_by: Deadline::at(force_at.instant() + CLEANUP_ALLOWANCE),
        });
        self.held = ReadAhead::new();
        self.send_interrupt();
        Ok(())
    }

    /// The stall's `force_at` passed without a terminal: `Overflow`, the
    /// group force-closed under the stall's `close_by`.
    fn stall_force(&self) -> Failed {
        Failed {
            cause: RouteError::Overflow { turn: self.turn },
            exit: None,
            close_by: self.stall.map(|stall| stall.close_by),
        }
    }

    /// Acts on a wake (design §2 rules 2 to 4): the daemon force wins;
    /// after the terminal nothing else acts; before submission, or at
    /// `force_at`, the group is force-closed under `close_by`; otherwise the
    /// first order enqueues the one interrupt. It never waits.
    fn on_wake(&mut self) -> Result<(), Failed> {
        self.after_terminal()?;
        if self.terminated {
            return Ok(());
        }
        let Some((force_at, close_by)) = self
            .signals
            .stop
            .borrow()
            .as_ref()
            .map(|order| (order.force_at, order.close_by))
        else {
            return Ok(());
        };
        if !self.submitted || tokio::time::Instant::now() >= force_at.instant() {
            return Err(Failed::stopped(self.turn, close_by));
        }
        self.send_interrupt();
        Ok(())
    }

    /// After the terminal only the daemon force ends the turn early.
    pub(crate) fn after_terminal(&self) -> Result<(), Failed> {
        if self.signals.force.borrow().is_some() {
            return Err(RouteError::ForceStopped { turn: self.turn }.into());
        }
        Ok(())
    }

    /// Waits for Host's confirmed exit after EOF before a terminal. With
    /// `unterminated` (F21) the wait ends at the cleanup allowance and any
    /// failure of the exit wait is transport loss; otherwise its cause is
    /// kept.
    async fn exit_before_terminal(&mut self, unterminated: bool) -> Result<ExitReport, Failed> {
        self.after_terminal()?;
        let bound = if unterminated {
            Deadline::at(self.deadline.instant().min(cleanup_deadline().instant()))
        } else {
            self.deadline
        };
        let turn = self.turn;
        let exit = self.sender.wait_exit(bound);
        match self.serve(exit).await {
            Ok(Ok(exit)) => Ok(exit),
            Ok(Err(_)) if unterminated => Err(transport(turn).into()),
            Ok(Err(error)) => Err(wire_cause::<P>(turn, &error).into()),
            Err(failed) if unterminated && matches!(failed.cause, RouteError::Deadline { .. }) => {
                Err(transport(turn).into())
            }
            Err(failed) => Err(failed),
        }
    }

    /// The end of stdout before a terminal: a Host-confirmed exit is
    /// `ProcessExited` (the persistent profile's server loss), anything else
    /// as [`Self::exit_before_terminal`] decides. A protocol whose
    /// [`PrivateProtocol::bounded_exit`] holds waits for the exit only for
    /// the cleanup allowance: a live process that closed stdout is
    /// transport loss.
    pub(crate) async fn ended(&mut self, end: Next<P::Message>) -> Failed {
        let bounded = matches!(end, Next::Unterminated) || self.lane.bounded_exit();
        let exit = match self.exit_before_terminal(bounded).await {
            Ok(exit) => exit,
            Err(failed) => return failed,
        };
        if let Err(failed) = self.after_terminal() {
            return failed;
        }
        Failed {
            cause: RouteError::ProcessExited { turn: self.turn },
            exit: Some(exit),
            close_by: None,
        }
    }

    /// Enqueues the one interrupt (design §2 rule 3).
    pub(crate) fn send_interrupt(&mut self) {
        if self.interrupt != Interrupt::NotSent {
            return;
        }
        self.interrupt = Interrupt::Queued;
        let interrupt = P::interrupt(self);
        self.pending = Some(self.sender.write(interrupt, self.deadline));
    }

    /// The interrupt's write answered. One not written whole asked the
    /// vendor nothing: before the terminal, a stop order's force rule
    /// applies at once and the wall's soft stop ends as transport loss.
    fn interrupt_written(
        &mut self,
        outcome: &Result<SendOutcome, WireError>,
    ) -> Result<(), Failed> {
        self.pending = None;
        if matches!(outcome, Ok(SendOutcome::Written)) {
            self.interrupt = Interrupt::Written;
            return Ok(());
        }
        self.interrupt = Interrupt::Failed;
        if self.terminated {
            return Ok(());
        }
        let close_by = self
            .signals
            .stop
            .borrow()
            .as_ref()
            .map(|order| order.close_by);
        Err(match (close_by, self.stall) {
            (Some(close_by), _) => Failed::stopped(self.turn, close_by),
            (None, Some(_)) => self.stall_force(),
            (None, None) => transport(self.turn).into(),
        })
    }

    /// Serves until the interrupt's write answered.
    pub(crate) async fn settle_interrupt(&mut self) -> Result<(), Failed> {
        let mut never = std::pin::pin!(std::future::pending::<()>());
        while self.interrupt == Interrupt::Queued {
            self.serve_once(never.as_mut()).await?;
        }
        Ok(())
    }
}

/// Wakes Route whenever the stop order appears or changes, and again at
/// its `force_at`. Never returns.
pub(super) async fn wake_on_order(mut stop: StopWatch, wake: &watch::Sender<u64>) {
    loop {
        let force_at = stop
            .borrow_and_update()
            .as_ref()
            .map(|order| order.force_at.instant());
        if let Some(force_at) = force_at {
            wake.send_modify(|epoch| *epoch += 1);
            tokio::select! {
                () = tokio::time::sleep_until(force_at) => {
                    wake.send_modify(|epoch| *epoch += 1);
                    if stop.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
                changed = stop.changed() => {
                    if changed.is_err() {
                        tokio::time::sleep_until(force_at).await;
                        wake.send_modify(|epoch| *epoch += 1);
                        std::future::pending::<()>().await;
                    }
                }
            }
        } else if stop.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Resolves at the stall's `force_at` before the terminal; never otherwise.
async fn stall_force(stall: Option<&Stall>, terminated: bool) {
    match stall {
        Some(stall) if !terminated => tokio::time::sleep_until(stall.force_at.instant()).await,
        Some(_) | None => std::future::pending().await,
    }
}

/// Resolves once `force` is set; never when its sender is gone unset.
async fn forced(force: &mut ForceWatch) {
    if force.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Resolves on the connection's first failure; never when the latch is gone.
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

/// Polls the pending write, if any; never resolves without one.
pub(crate) async fn pending(write: Option<&mut PendingWrite>) -> Result<SendOutcome, WireError> {
    match write {
        Some(write) => write.await,
        None => std::future::pending().await,
    }
}

/// A failed or stopped acquisition's cause and Host's evidence (design §2
/// rule 1, §7.2 rows 3 and 4). A gate stop is the daemon force's when that
/// is set, else the turn's stop order's.
pub(super) fn acquire_failure<P: PrivateProtocol>(
    turn: TurnNumber,
    error: &WireError,
    force: &ForceWatch,
) -> RouteFailure {
    let (cause, launched, cleanup, forced, journal_uncertain) = if let WireError::Acquire {
        cause,
        launched,
        cleanup,
        forced,
        journal_uncertain,
    } = error
    {
        (
            cause.as_ref(),
            *launched,
            *cleanup,
            *forced,
            *journal_uncertain,
        )
    } else {
        (error, false, None, false, false)
    };
    let cause = if matches!(cause, WireError::Host(HostError::Stopped)) {
        if force.borrow().is_some() {
            RouteError::ForceStopped { turn }
        } else {
            RouteError::Stopped { turn }
        }
    } else {
        wire_cause::<P>(turn, cause)
    };
    RouteFailure {
        cause,
        undecoded: None,
        exit: None,
        launched,
        cleanup,
        forced,
        journal_uncertain,
        acknowledged: false,
        shared: false,
    }
}

/// Keeps a Wire failure's cause for Core's C1 class decision.
pub(crate) fn wire_cause<P: PrivateProtocol>(turn: TurnNumber, error: &WireError) -> RouteError {
    match error {
        // Design §7.2: the folder or `stderr.log` was not created; nothing
        // launched.
        WireError::Evidence(_) | WireError::Host(HostError::Evidence(_)) => RouteError::Store {
            turn,
            kind: StoreFailure::Evidence,
        },
        // Rows 3 and 4: a Host journal write that did not commit.
        WireError::Host(HostError::Journal { uncertain, .. }) => RouteError::Store {
            turn,
            kind: if *uncertain {
                StoreFailure::Uncertain
            } else {
                StoreFailure::NotCommitted
            },
        },
        // Every Wire call that reports a cause runs under the turn's work deadline,
        // so a read it outlived is C1 `deadline_wall`; so is an acquisition,
        // which Host bounds by the same deadline. The failure drain runs under
        // the cleanup deadline and never reports a cause.
        WireError::Deadline | WireError::Host(HostError::Deadline) => RouteError::Deadline { turn },
        WireError::Cancelled => RouteError::ForceStopped { turn },
        WireError::Acquire { cause, .. } => wire_cause::<P>(turn, cause),
        WireError::Message(WireFailure::UnterminatedMessage) => protocol(turn, P::UNTERMINATED),
        // C1 §8.2: a vendor message over 1 MiB is `overflow` too.
        WireError::Message(WireFailure::Overflow | WireFailure::MessageTooLarge) => {
            RouteError::Overflow { turn }
        }
        WireError::Message(WireFailure::Transport)
        | WireError::Io(_)
        | WireError::Host(_)
        | WireError::Woken => transport(turn),
    }
}

/// A protocol failure of `turn`.
pub(crate) fn protocol(turn: TurnNumber, detail: &'static str) -> RouteError {
    RouteError::Protocol { turn, detail }
}

/// A transport loss of `turn`.
pub(crate) fn transport(turn: TurnNumber) -> RouteError {
    RouteError::TransportLost { turn }
}

#[cfg(test)]
mod tests {
    use super::{READ_AHEAD_BYTES, ReadAhead};

    /// Review r1 #4: with the hop blocked, messages of about 0.9 MiB are
    /// held only while the next one fits: never more than 4 MiB.
    #[test]
    fn read_ahead_never_holds_more_than_its_bytes() {
        let mut ahead = ReadAhead::new();
        let message = 900 * 1024;
        for n in 0..10 {
            if !ahead.admits(message, 1024) {
                break;
            }
            ahead.push((n, message));
        }
        assert!(
            ahead.bytes <= READ_AHEAD_BYTES,
            "{} bytes held",
            ahead.bytes
        );
        assert_eq!(ahead.held.len(), 4);
        // One message alone may pass the bytes: nothing else waits.
        let mut alone = ReadAhead::new();
        assert!(alone.admits(READ_AHEAD_BYTES + 1, 1024));
        alone.push(((), READ_AHEAD_BYTES + 1));
        assert!(!alone.admits(1, 1024));
        assert_eq!(alone.pop(), Some(()));
        assert!(alone.is_empty());
    }
}
