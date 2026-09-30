use std::future::Future;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use super::{
    Deadline, FakeMessage, OutboundMessage, PrivateProcessSpec, ReprobeReport, RouteError,
    RouteFailure, RouteMessage, RuntimeConfig, RuntimeResources, SendOutcome, StopWatch,
    StoreFailure, TerminalStatus, TurnNumber, TurnStart, WireRecovery, WireShutdown,
};
use via_wire::{
    CloseMode, CloseRequest, ExitReport, FailureCause, HostError, LatchState, PendingWrite,
    WireCleanup, WireError, WireFailure, WireMessages, WireParts, WireRuntime, WireSender,
    WireSignals,
};

/// Final fake protocol evidence, including independently confirmed process exit.
pub struct FakeRouteResult {
    /// Vendor terminal status.
    pub status: TerminalStatus,
    /// Vendor stop reason retained verbatim.
    pub stop_reason: String,
    /// Optional vendor failure code.
    pub vendor_code: Option<String>,
    /// Host-confirmed vendor exit after stdin was half-closed; both fields
    /// are `None` when the wall deadline passed during finalization before
    /// an exit was confirmed.
    pub exit: ExitReport,
    /// Group cleanup certainty after the vendor's confirmed exit.
    pub cleanup: via_wire::WireCleanup,
    /// A Host journal write in the turn's cleanup had an uncertain outcome:
    /// the daemon must latch (design §7.2 row 12).
    pub journal_uncertain: bool,
}

/// One-start state machine and opaque Wire runtime for the private fake route.
pub struct FakeRoute {
    wire: WireRuntime,
}

impl FakeRoute {
    /// Forwards the unopened resources to Wire's sole bootstrap split.
    pub fn new(config: RuntimeConfig, resources: RuntimeResources) -> Result<Self, WireError> {
        let wire = WireRuntime::new(config, resources)?;
        Ok(Self { wire })
    }

    /// Sends one prompt after durable submission and awaits paired terminal and real exit.
    ///
    /// Every decoded message, including acceptance, the terminal and late
    /// observations after it, is sent on the `hop` in decode order. While
    /// the hop is full Route reads no further message, and a closed hop
    /// (the Adapter's stall, C2 A1) fails the turn as overflow. Every wait
    /// keeps the controls serviced (Task 4 design §9). On any failure the
    /// private group is force-closed and the connection finished under a
    /// separate cleanup bound.
    ///
    /// `force` set fails the turn [`RouteError::ForceStopped`]: before launch
    /// nothing starts; after it, the same cleanup follows. The daemon force
    /// overrides a stop order.
    ///
    /// `stop` is the turn's stop order (design §2): set before ARM, nothing
    /// launches; after ARM but before the start message, the group is
    /// force-closed at once; after it, one interrupt is sent, a terminal still
    /// ends the turn normally, and at `force_at` without one the group is
    /// force-closed under `close_by`. Either stop is
    /// [`RouteError::Stopped`].
    pub async fn execute(
        &self,
        process: PrivateProcessSpec,
        start: TurnStart,
        hop: mpsc::Sender<RouteMessage>,
        deadline: Deadline,
        force: watch::Receiver<Option<tokio::time::Instant>>,
        stop: StopWatch,
    ) -> Result<FakeRouteResult, RouteFailure> {
        let turn = start.turn();
        let not_launched = |cause| RouteFailure {
            cause,
            undecoded: None,
            exit: None,
            launched: false,
            cleanup: None,
            forced: false,
            journal_uncertain: false,
        };
        if force.borrow().is_some() {
            return Err(not_launched(RouteError::ForceStopped { turn }));
        }
        // An order set before submission reached Route: nothing starts, and
        // no anchor intent exists.
        if stop.borrow().is_some() {
            return Err(not_launched(RouteError::Stopped { turn }));
        }
        let (wake, woken) = watch::channel(0_u64);
        let gate = {
            let force = force.clone();
            let stop = stop.clone();
            Arc::new(move || force.borrow().is_some() || stop.borrow().is_some())
        };
        let signals = WireSignals {
            force: force.clone(),
            wake: woken.clone(),
            gate,
        };
        let turn_run = self.run_turn(
            process,
            start,
            &hop,
            deadline,
            signals,
            Signals {
                force,
                stop: stop.clone(),
                wake: woken,
            },
        );
        tokio::select! {
            result = turn_run => result,
            () = wake_on_order(stop, &wake) => unreachable!("the stop waker never returns"),
        }
    }

    /// [`Self::execute`] after its entry checks, while the waker runs. Every
    /// exit after the connection opened finishes it once, under the graceful
    /// close's `close_by`, the force close's cleanup deadline or the stop
    /// order's `close_by` (design §8.6).
    async fn run_turn(
        &self,
        process: PrivateProcessSpec,
        start: TurnStart,
        hop: &mpsc::Sender<RouteMessage>,
        deadline: Deadline,
        wire_signals: WireSignals,
        signals: Signals,
    ) -> Result<FakeRouteResult, RouteFailure> {
        let turn = start.turn();
        let WireParts {
            sender,
            mut messages,
        } = self
            .wire
            .open_connection(process, deadline, wire_signals)
            .await
            .map_err(|error| acquire_failure(turn, &error, &signals.force))?
            .into_parts();
        let mut serving = Serving::new(turn, &sender, hop, deadline, signals);
        let drive = Self::drive(&mut serving, &mut messages, start);
        let failed = match Box::pin(drive).await {
            Ok(Finished::Result(result, close_by)) => {
                messages.finish(close_by).await;
                return Ok(result);
            }
            Ok(Finished::Late(terminal)) => {
                // Design §2 rule 3 [r1.23]: a decoded terminal is returned
                // even though the wall deadline passed during finalization;
                // cleanup comes from Host's force close. A message still
                // held, such as the terminal with its final text, reaches the
                // hop first, within the same cleanup allowance and whatever
                // the force or the latch; if it cannot, that failure is the
                // turn's, never a completion.
                let cleanup = cleanup_deadline();
                match serving.deliver_held(cleanup).await {
                    Ok(()) => {
                        let report = sender
                            .close(CloseRequest {
                                mode: CloseMode::Force,
                                deadline: cleanup,
                            })
                            .await;
                        messages.finish(cleanup).await;
                        return Ok(terminal.result(
                            report.vendor_exit.unwrap_or(ExitReport {
                                code: None,
                                signal: None,
                            }),
                            report.cleanup,
                            report.journal_uncertain,
                        ));
                    }
                    // Cleanup gets its own bound below: the allowance has
                    // elapsed, and the force close still needs its time.
                    Err(failed) => failed,
                }
            }
            Err(failed) => failed,
        };
        // The turn deadline may already have elapsed; cleanup gets its own
        // bound, or the stop order's `close_by`.
        let cleanup = failed.close_by.unwrap_or_else(cleanup_deadline);
        let report = sender
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: cleanup,
            })
            .await;
        // The group is stopping; the reader reads its stdout to EOF and
        // discards it, so it never blocks. The original failure stays
        // authoritative.
        messages.finish(cleanup).await;
        Err(RouteFailure {
            cause: failed.cause,
            // The one message this turn could not decode, if any (design §7.3).
            undecoded: sender.take_undecoded(),
            exit: failed.exit.or(report.vendor_exit),
            launched: true,
            cleanup: Some(report.cleanup),
            forced: report.forced,
            journal_uncertain: report.journal_uncertain,
        })
    }

    /// Drains Host controls and reapers before Core releases the Store owner.
    pub async fn shutdown(
        &self,
        deadline: Deadline,
        turns: &[(crate::SessionId, crate::TurnNumber)],
    ) -> WireShutdown {
        self.wire.shutdown(deadline, turns).await
    }

    /// Hands Host capacity for a group it did not launch (design §11).
    pub fn hold_capacity(
        &self,
        anchor_id: String,
        owner: crate::SessionId,
        token: via_wire::CapacityToken,
    ) {
        self.wire.hold_capacity(anchor_id, owner, token);
    }

    /// Returns one page of passive Host recovery facts without exposing a
    /// signal handle: up to `limit` anchors after the `after` id.
    pub async fn recover_page(
        &self,
        after: Option<String>,
        limit: u32,
        deadline: Deadline,
    ) -> Result<Vec<WireRecovery>, WireError> {
        self.wire.recover_page(after, limit, deadline).await
    }

    /// [`Self::recover_page`] of the anchors in `cohort` only.
    pub async fn recover_cohort_page(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: via_wire::AnchorCohort,
        deadline: Deadline,
    ) -> Result<Vec<WireRecovery>, WireError> {
        self.wire
            .recover_cohort_page(after, limit, cohort, deadline)
            .await
    }

    /// One non-signalling re-probe pass over held groups, optionally only
    /// one session's (design §8).
    pub async fn reprobe_held(
        &self,
        deadline: Deadline,
        owner: Option<crate::SessionId>,
    ) -> Result<ReprobeReport, WireError> {
        self.wire.reprobe_held(deadline, owner).await
    }

    /// Held groups no live control owns (design §6.6).
    pub fn held_unproven(&self) -> usize {
        self.wire.held_unproven()
    }

    /// Advances on every added holding (design §8).
    pub fn holdings_changed(&self) -> watch::Receiver<u64> {
        self.wire.holdings_changed()
    }

    /// Positive evidence that a vendor of one of `anchors` is live (Task 4
    /// design §11.3 `process.alive`).
    pub fn live_armed(&self, anchors: &[String]) -> bool {
        self.wire.live_armed(anchors)
    }

    /// Groups whose cleanup a live control or acquisition still owns
    /// (design §6.4).
    pub fn pending_cleanup(&self) -> usize {
        self.wire.pending_cleanup()
    }

    /// Subscribes Host's early stop to the daemon force signal (design §6.8),
    /// which carries the instant the force was raised.
    pub fn watch_force(&self, forced: watch::Receiver<Option<tokio::time::Instant>>) {
        self.wire.watch_force(forced);
    }

    /// Writes the start, reads and forwards messages to the terminal, then
    /// finalizes. Every wait goes through [`Serving::serve`].
    async fn drive(
        serving: &mut Serving<'_>,
        messages: &mut WireMessages,
        start: TurnStart,
    ) -> Result<Finished, Failed> {
        let turn = serving.turn;
        // Design §2 rule 2: after ARM, an order set before the start message
        // is written: the start is not written and the group closes at once.
        if let Some(order) = serving.signals.stop.borrow().as_ref() {
            return Err(Failed::stopped(turn, order.close_by));
        }
        let start = start.into_message().map_err(Failed::from)?;
        // While the start is pending no message is read: nothing the vendor
        // answers is taken before its whole input is written.
        let write = serving.sender.write(start, serving.deadline);
        let sent = serving
            .serve(write)
            .await?
            .map_err(|error| Failed::from(wire_cause(turn, &error)))?;
        if sent != SendOutcome::Written {
            return Err(transport(turn).into());
        }
        let mut phase = Phase::Submitted;
        let terminal = loop {
            let message = match serving.next(messages).await? {
                Next::Message(message) => message,
                // EOF without a terminal: a Host-confirmed exit is
                // `ProcessExited`. F21: after an unterminated last line
                // (kept in `undecoded.bin`) the wait is bounded by the cleanup
                // allowance and anything but a confirmed exit is transport
                // loss, never a protocol failure.
                // Under the daemon force the exit is the force's own stop
                // (Host's early stop, design §6.8): the force row, never
                // `ProcessExited`.
                end @ (Next::Eof | Next::Unterminated) => {
                    let unterminated = matches!(end, Next::Unterminated);
                    let exit = serving.exit_before_terminal(unterminated).await?;
                    serving.after_terminal()?;
                    return Err(Failed {
                        cause: RouteError::ProcessExited { turn },
                        exit: Some(exit),
                        close_by: None,
                    });
                }
            };
            phase
                .advance(&message.payload, turn, serving.interrupted)
                .map_err(Failed::from)?;
            let terminal = terminal_evidence(&message);
            serving.held = Some(message);
            if let Some(terminal) = terminal {
                break terminal;
            }
        };
        serving.terminated = true;
        match Self::finalize(serving, messages, &mut phase).await {
            Ok(exit) => {
                let close_by = serving
                    .signals
                    .stop
                    .borrow()
                    .as_ref()
                    .map_or_else(cleanup_deadline, |order| order.close_by);
                let close = serving
                    .sender
                    .close(CloseRequest {
                        mode: CloseMode::Graceful,
                        deadline: close_by,
                    })
                    .await;
                Ok(Finished::Result(
                    terminal.result(exit, close.cleanup, close.journal_uncertain),
                    close_by,
                ))
            }
            Err(failed) if matches!(failed.cause, RouteError::Deadline { .. }) => {
                Ok(Finished::Late(terminal))
            }
            Err(failed) => Err(failed),
        }
    }

    /// Terminal is semantic completion, not transport EOF. Half-close input
    /// (fake finalization waits on it), then read stdout to EOF under the
    /// turn deadline: late observations are forwarded and anything that
    /// breaks the phase order, such as a second terminal, fails the turn. A
    /// stop order no longer forces the turn.
    async fn finalize(
        serving: &mut Serving<'_>,
        messages: &mut WireMessages,
        phase: &mut Phase,
    ) -> Result<ExitReport, Failed> {
        let turn = serving.turn;
        let close = serving.sender.close_input(serving.deadline);
        serving
            .serve(close)
            .await?
            .map_err(|error| Failed::from(wire_cause(turn, &error)))?;
        loop {
            match serving.next(messages).await? {
                Next::Message(message) => {
                    phase
                        .advance(&message.payload, turn, serving.interrupted)
                        .map_err(Failed::from)?;
                    serving.held = Some(message);
                }
                Next::Eof => break,
                Next::Unterminated => {
                    return Err(protocol(turn, "fake stdout ended inside a message").into());
                }
            }
        }
        serving.flush().await?;
        let exit = serving.sender.wait_exit(serving.deadline);
        let exit = serving
            .serve(exit)
            .await?
            .map_err(|error| Failed::from(wire_cause(turn, &error)))?;
        // Host's early stop raises the force before it stops the vendor
        // (design §6.8), so an exit it caused is read under a set force.
        // Wire hands back a recorded exit without consulting the force, so
        // read it here: the force row, never an exit status Route reports
        // as the vendor's own.
        serving.after_terminal()?;
        Ok(exit)
    }
}

/// How `drive` ended without a failure.
enum Finished {
    /// The normal path: terminal, exit and graceful close, with the close's
    /// bound for `finish`.
    Result(FakeRouteResult, Deadline),
    /// A decoded terminal whose finalization outlived the wall deadline.
    Late(TerminalEvidence),
}

/// The turn's control signals.
struct Signals {
    /// The daemon force.
    force: watch::Receiver<Option<tokio::time::Instant>>,
    /// The turn's stop order.
    stop: StopWatch,
    /// Route's wake for a new stop order or its `force_at`.
    wake: watch::Receiver<u64>,
}

/// Route's side of a running turn: every wait on the vendor, the Adapter
/// or a write goes through [`Self::serve`], so no wait hides a control
/// (Task 4 design §9).
struct Serving<'a> {
    turn: TurnNumber,
    sender: &'a WireSender,
    hop: &'a mpsc::Sender<RouteMessage>,
    deadline: Deadline,
    signals: Signals,
    latch: watch::Receiver<LatchState>,
    /// The one interrupt was enqueued.
    interrupted: bool,
    /// The terminal was read: a stop order no longer acts.
    terminated: bool,
    /// The pending interrupt write, kept pinned while other waits run.
    pending: Option<PendingWrite>,
    /// A decoded message waiting for room on the hop.
    held: Option<RouteMessage>,
}

/// What [`Serving::next`] read.
enum Next {
    /// One decoded vendor message.
    Message(RouteMessage),
    /// Stdout ended.
    Eof,
    /// Stdout ended inside a message; Wire kept its bytes (F21, design §7.3).
    Unterminated,
}

impl<'a> Serving<'a> {
    fn new(
        turn: TurnNumber,
        sender: &'a WireSender,
        hop: &'a mpsc::Sender<RouteMessage>,
        deadline: Deadline,
        signals: Signals,
    ) -> Self {
        Self {
            turn,
            sender,
            hop,
            deadline,
            latch: sender.latch(),
            signals,
            interrupted: false,
            terminated: false,
            pending: None,
            held: None,
        }
    }

    /// Awaits `op` while servicing every control, biased (design §9):
    /// (1) daemon force; (2) turn deadline; (3) the connection latch;
    /// (4) the hop closed → `Overflow`; (5) Route's wake → [`Self::on_wake`];
    /// (6) the pending interrupt completing; (7) room on the hop, which sends
    /// the held message; (8) `op`. Every arm is cancel-safe: `op` and the
    /// pending write stay pinned, the reserve holds no message.
    async fn serve<T>(&mut self, op: impl Future<Output = T>) -> Result<T, Failed> {
        let mut op = std::pin::pin!(op);
        loop {
            if let Some(output) = self.serve_once(op.as_mut()).await? {
                return Ok(output);
            }
        }
    }

    /// Serves until the held message is on the hop, reading nothing more:
    /// Route blocked on the hop stops calling `next_message` (design §2.3).
    async fn flush(&mut self) -> Result<(), Failed> {
        let mut never = std::pin::pin!(std::future::pending::<()>());
        while self.held.is_some() {
            self.serve_once(never.as_mut()).await?;
        }
        Ok(())
    }

    /// The late path's delivery (design §2 rule 3 [r1.23]): the held
    /// message, if any, goes on the hop as the reserve arm of
    /// [`Self::serve_once`] sends it. Only room on the hop, a closed hop or
    /// `by` ends the wait; no other control acts on an already decoded
    /// message.
    async fn deliver_held(&mut self, by: Deadline) -> Result<(), Failed> {
        let Some(message) = self.held.take() else {
            return Ok(());
        };
        let turn = self.turn;
        let hop = self.hop;
        tokio::select! {
            biased;
            permit = hop.reserve() => match permit {
                Ok(permit) => {
                    permit.send(message);
                    Ok(())
                }
                Err(_) => Err(self.hop_closed()),
            },
            () = tokio::time::sleep_until(by.instant()) => {
                Err(RouteError::Deadline { turn }.into())
            }
        }
    }

    /// One round of [`Self::serve`]: `Some` once `op` completed.
    async fn serve_once<T>(
        &mut self,
        op: std::pin::Pin<&mut impl Future<Output = T>>,
    ) -> Result<Option<T>, Failed> {
        let turn = self.turn;
        let hop = self.hop;
        tokio::select! {
            biased;
            () = forced(&mut self.signals.force) => Err(RouteError::ForceStopped { turn }.into()),
            () = tokio::time::sleep_until(self.deadline.instant()) => {
                Err(RouteError::Deadline { turn }.into())
            }
            cause = latched(&mut self.latch) => Err(wire_cause(turn, &cause.error()).into()),
            () = hop.closed() => Err(self.hop_closed()),
            () = woken(&mut self.signals.wake) => self.on_wake().map(|()| None),
            // Not written or cut short: `force_at` still bounds the turn.
            _unsent = pending(self.pending.as_mut()), if self.pending.is_some() => {
                self.pending = None;
                Ok(None)
            }
            permit = hop.reserve(), if self.held.is_some() => match (permit, self.held.take()) {
                (Ok(permit), Some(message)) => {
                    permit.send(message);
                    Ok(None)
                }
                (Ok(_), None) => Ok(None),
                (Err(_), _) => Err(self.hop_closed()),
            },
            output = op => Ok(Some(output)),
        }
    }

    /// Reads and decodes the next vendor message once the held one is on
    /// the hop. One Route cannot decode is kept in `undecoded.bin` first
    /// (design §7.3).
    async fn next(&mut self, messages: &mut WireMessages) -> Result<Next, Failed> {
        let turn = self.turn;
        loop {
            self.flush().await?;
            let message = match self.serve(messages.next_message()).await? {
                Ok(Some(message)) => message,
                Ok(None) => return Ok(Next::Eof),
                // Route's own wake arm acts on it; nothing was lost.
                Err(WireError::Woken) => continue,
                Err(WireError::Message(WireFailure::UnterminatedMessage)) => {
                    return Ok(Next::Unterminated);
                }
                Err(error) => return Err(Failed::from(wire_cause(turn, &error))),
            };
            return match FakeMessage::decode(message.bytes(), turn) {
                Ok(payload) => Ok(Next::Message(RouteMessage { payload })),
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
    fn hop_closed(&self) -> Failed {
        let turn = self.turn;
        if self.signals.force.borrow().is_some() {
            RouteError::ForceStopped { turn }.into()
        } else {
            RouteError::Overflow { turn }.into()
        }
    }

    /// Acts on a wake (design §2 rules 3 and 4): the daemon force wins;
    /// after the terminal nothing else acts; at `force_at` the group is
    /// force-closed under `close_by`; otherwise the first order enqueues the
    /// one interrupt. It never waits.
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
        if tokio::time::Instant::now() >= force_at.instant() {
            return Err(Failed::stopped(self.turn, close_by));
        }
        if !self.interrupted {
            self.interrupted = true;
            let interrupt = format!(
                "{{\"type\":\"interrupt\",\"id\":2,\"vendor_turn_id\":\"fake-turn-{}\"}}\n",
                self.turn.get()
            );
            self.pending = Some(self.sender.write(
                OutboundMessage::Interrupt(interrupt.into_bytes()),
                self.deadline,
            ));
        }
        Ok(())
    }

    /// After the terminal only the daemon force ends the turn early.
    fn after_terminal(&self) -> Result<(), Failed> {
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
            Ok(Err(error)) => Err(wire_cause(turn, &error).into()),
            Err(failed) if unterminated && matches!(failed.cause, RouteError::Deadline { .. }) => {
                Err(transport(turn).into())
            }
            Err(failed) => Err(failed),
        }
    }
}

/// Wakes Route whenever the stop order appears or changes, and again at
/// its `force_at`. Never returns.
async fn wake_on_order(mut stop: StopWatch, wake: &watch::Sender<u64>) {
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

/// First failure inside `drive`, before cleanup.
struct Failed {
    cause: RouteError,
    exit: Option<ExitReport>,
    /// A stop order's bound on the force close and drain.
    close_by: Option<Deadline>,
}

impl Failed {
    /// A stop order forcing the turn, closed under `close_by`.
    fn stopped(turn: TurnNumber, close_by: Deadline) -> Self {
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

/// Bounds Host cleanup and the stdout drain separately from the turn deadline, which
/// may already have elapsed when cleanup starts.
fn cleanup_deadline() -> Deadline {
    Deadline::at(tokio::time::Instant::now() + std::time::Duration::from_secs(3))
}

/// Connection-local protocol phase for the only turn on a fake connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Start written; awaiting its paired acceptance.
    Submitted,
    /// Acceptance seen; observations and one terminal may follow.
    Accepted,
    /// Terminal seen; only late observations may follow.
    Terminated,
}

impl Phase {
    /// Checks one decoded message against the phase and advances it. An
    /// `interrupt_ack` is control evidence once Route sent its interrupt.
    fn advance(
        &mut self,
        message: &FakeMessage,
        turn: TurnNumber,
        interrupted: bool,
    ) -> Result<(), RouteError> {
        match (message, *self) {
            (FakeMessage::InterruptAck { .. }, _) if interrupted => {}
            (FakeMessage::Accepted { .. }, Self::Submitted) => *self = Self::Accepted,
            (FakeMessage::Accepted { .. }, Self::Accepted | Self::Terminated) => {
                return Err(protocol(turn, "duplicate fake acceptance"));
            }
            (FakeMessage::Terminal { .. }, Self::Accepted) => *self = Self::Terminated,
            (FakeMessage::Terminal { .. }, Self::Submitted) => {
                return Err(protocol(turn, "fake terminal before acceptance"));
            }
            (FakeMessage::Terminal { .. }, Self::Terminated) => {
                return Err(protocol(turn, "duplicate fake terminal"));
            }
            (FakeMessage::InterruptAck { .. }, _) => {
                return Err(protocol(turn, "unsolicited fake interrupt acknowledgement"));
            }
            (
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::Usage { .. }
                | FakeMessage::Unknown { .. },
                Self::Submitted,
            ) => return Err(protocol(turn, "fake observation before acceptance")),
            (
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::Usage { .. }
                | FakeMessage::Unknown { .. },
                Self::Accepted | Self::Terminated,
            ) => {}
        }
        Ok(())
    }
}

/// Terminal fields retained for the route result. The final text is not
/// among them: the Adapter sends it as `final_text` observations from the
/// terminal message itself (Task 4 design §2.3).
struct TerminalEvidence {
    status: TerminalStatus,
    stop_reason: String,
    vendor_code: Option<String>,
}

impl TerminalEvidence {
    fn result(
        self,
        exit: ExitReport,
        cleanup: WireCleanup,
        journal_uncertain: bool,
    ) -> FakeRouteResult {
        FakeRouteResult {
            status: self.status,
            stop_reason: self.stop_reason,
            vendor_code: self.vendor_code,
            exit,
            cleanup,
            journal_uncertain,
        }
    }
}

fn terminal_evidence(message: &RouteMessage) -> Option<TerminalEvidence> {
    match &message.payload {
        FakeMessage::Terminal {
            status,
            stop_reason,
            vendor_code,
            ..
        } => Some(TerminalEvidence {
            status: *status,
            stop_reason: stop_reason.clone(),
            vendor_code: vendor_code.clone(),
        }),
        FakeMessage::Accepted { .. }
        | FakeMessage::Text { .. }
        | FakeMessage::ToolStarted { .. }
        | FakeMessage::ToolEnded { .. }
        | FakeMessage::Usage { .. }
        | FakeMessage::InterruptAck { .. }
        | FakeMessage::Unknown { .. } => None,
    }
}

/// Resolves once `force` is set; never when its sender is gone unset.
async fn forced(force: &mut watch::Receiver<Option<tokio::time::Instant>>) {
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
async fn pending(write: Option<&mut PendingWrite>) -> Result<SendOutcome, WireError> {
    match write {
        Some(write) => write.await,
        None => std::future::pending().await,
    }
}

/// A failed or stopped acquisition's cause and Host's evidence (design §2
/// rule 1, §7.2 rows 3 and 4). A gate stop is the daemon force's when that
/// is set, else the turn's stop order's.
fn acquire_failure(
    turn: TurnNumber,
    error: &WireError,
    force: &watch::Receiver<Option<tokio::time::Instant>>,
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
        wire_cause(turn, cause)
    };
    RouteFailure {
        cause,
        undecoded: None,
        exit: None,
        launched,
        cleanup,
        forced,
        journal_uncertain,
    }
}

/// Keeps a Wire failure's cause for Core's C1 class decision.
fn wire_cause(turn: TurnNumber, error: &WireError) -> RouteError {
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
        WireError::Acquire { cause, .. } => wire_cause(turn, cause),
        WireError::Message(WireFailure::UnterminatedMessage) => {
            protocol(turn, "fake stdout ended inside a message")
        }
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

fn protocol(turn: TurnNumber, detail: &'static str) -> RouteError {
    RouteError::Protocol { turn, detail }
}

fn transport(turn: TurnNumber) -> RouteError {
    RouteError::TransportLost { turn }
}

#[cfg(test)]
mod tests {
    use super::{
        FakeMessage, HostError, Phase, RouteError, StoreFailure, TurnNumber, WireError,
        WireFailure, wire_cause,
    };

    fn decode(json: &str) -> FakeMessage {
        FakeMessage::decode(json.as_bytes(), TurnNumber::try_from(1).unwrap()).unwrap()
    }

    fn detail(phase: &mut Phase, json: &str) -> Option<&'static str> {
        match phase.advance(&decode(json), TurnNumber::try_from(1).unwrap(), false) {
            Ok(()) => None,
            Err(RouteError::Protocol { detail, .. }) => Some(detail),
            Err(other) => panic!("unexpected route error {other:?}"),
        }
    }

    const ACCEPTED: &str = r#"{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}"#;
    const TERMINAL: &str = r#"{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"","stop_reason":"end_turn"}"#;
    const OBSERVATIONS: [&str; 5] = [
        r#"{"type":"text","vendor_turn_id":"fake-turn-1","text":"hi"}"#,
        r#"{"type":"usage","vendor_turn_id":"fake-turn-1","total_tokens":7}"#,
        r#"{"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t","name":"sh","input_summary":""}"#,
        r#"{"type":"tool_ended","vendor_turn_id":"fake-turn-1","tool_id":"t","status":"completed","output_summary":"","exit_code":null}"#,
        r#"{"type":"later","value":1}"#,
    ];

    #[test]
    fn observations_are_admitted_after_acceptance_and_after_terminal() {
        let mut phase = Phase::Submitted;
        assert_eq!(detail(&mut phase, ACCEPTED), None);
        for message in OBSERVATIONS {
            assert_eq!(detail(&mut phase, message), None);
        }
        assert_eq!(detail(&mut phase, TERMINAL), None);
        assert_eq!(phase, Phase::Terminated);
        for message in OBSERVATIONS {
            assert_eq!(detail(&mut phase, message), None, "late {message}");
        }
    }

    #[test]
    fn order_violations_fail_the_turn() {
        for message in OBSERVATIONS {
            assert_eq!(
                detail(&mut Phase::Submitted, message),
                Some("fake observation before acceptance")
            );
        }
        assert_eq!(
            detail(&mut Phase::Submitted, TERMINAL),
            Some("fake terminal before acceptance")
        );
        assert_eq!(
            detail(&mut Phase::Terminated, TERMINAL),
            Some("duplicate fake terminal")
        );
        for phase in [Phase::Accepted, Phase::Terminated] {
            assert_eq!(
                detail(&mut { phase }, ACCEPTED),
                Some("duplicate fake acceptance")
            );
        }
        let ack = r#"{"type":"interrupt_ack","id":2,"vendor_turn_id":"fake-turn-1"}"#;
        for phase in [Phase::Submitted, Phase::Accepted, Phase::Terminated] {
            assert_eq!(
                detail(&mut { phase }, ack),
                Some("unsolicited fake interrupt acknowledgement")
            );
        }
    }

    fn refused(input: &[u8]) -> Option<&'static str> {
        match FakeMessage::decode(input, TurnNumber::try_from(1).unwrap()) {
            Ok(_) => None,
            Err(RouteError::Protocol { detail, .. }) => Some(detail),
            Err(other) => panic!("unexpected route error {other:?}"),
        }
    }

    /// Task 4 design §2.2: what the fake keeps, and rules 1 and 3 plus
    /// UTF-8. Skipped fields are never decoded, whatever their type.
    #[test]
    fn decode_keeps_the_marks_and_bounds_short_fields() {
        let tool = |name: &str| {
            format!(
                r#"{{"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t","name":"{name}","input_summary":{{"x":[1]}}}}"#
            )
        };
        let kept = tool(&"n".repeat(1024));
        assert!(matches!(
            decode(&kept),
            FakeMessage::ToolStarted { ref name, .. } if name.len() == 1024
        ));
        assert_eq!(
            refused(tool(&"n".repeat(1025)).as_bytes()),
            Some("fake short field exceeds 1 KiB")
        );
        assert!(matches!(
            decode(r#"{"type":"text","vendor_turn_id":"fake-turn-1","text":1}"#),
            FakeMessage::Text { .. }
        ));
        assert!(matches!(
            decode(r#"{"type":"usage","vendor_turn_id":"fake-turn-1","total_tokens":9}"#),
            FakeMessage::Usage {
                total_tokens: 9,
                ..
            }
        ));
        let tag = |length: usize| format!(r#"{{"type":"{}","v":1}}"#, "u".repeat(length));
        assert!(matches!(
            decode(&tag(256)),
            FakeMessage::Unknown { ref vendor_type } if vendor_type.len() == 256
        ));
        assert_eq!(
            refused(tag(257).as_bytes()),
            Some("fake type tag exceeds 256 bytes")
        );
        let deep = format!(
            r#"{{"type":"text","vendor_turn_id":"fake-turn-1","x":{}{}}}"#,
            "[".repeat(64),
            "]".repeat(64)
        );
        assert_eq!(
            refused(deep.as_bytes()),
            Some("fake message exceeds JSON structure limits")
        );
        assert_eq!(
            refused(b"{\"type\":\"text\",\"vendor_turn_id\":\"fake-turn-1\",\"x\":\"\xff\"}"),
            Some("fake message is not UTF-8")
        );
    }

    /// Design §2 rule 3: once Route sent its interrupt, an `interrupt_ack`
    /// is control evidence in any phase, not a protocol error.
    #[test]
    fn an_interrupt_ack_is_admitted_once_an_interrupt_was_sent() {
        let turn = TurnNumber::try_from(1).unwrap();
        let ack = decode(r#"{"type":"interrupt_ack","id":2,"vendor_turn_id":"fake-turn-1"}"#);
        for phase in [Phase::Submitted, Phase::Accepted, Phase::Terminated] {
            let mut phase = { phase };
            assert_eq!(phase.advance(&ack, turn, true), Ok(()));
        }
    }

    /// Task 4 design §7.2: a folder or `stderr.log` that was not created is
    /// a non-latching Store failure; Host journal failures report their
    /// outcome [r5.5]; C1 §8.2: a message over the cap is `overflow`.
    #[test]
    fn store_failures_keep_their_kind() {
        let turn = TurnNumber::try_from(1).unwrap();
        for error in [
            WireError::Evidence(std::io::Error::from(std::io::ErrorKind::AlreadyExists)),
            WireError::Host(HostError::Evidence(std::io::Error::from(
                std::io::ErrorKind::AlreadyExists,
            ))),
        ] {
            assert_eq!(
                wire_cause(turn, &error),
                RouteError::Store {
                    turn,
                    kind: StoreFailure::Evidence
                }
            );
        }
        assert!(StoreFailure::WriterLost.latches() && !StoreFailure::Evidence.latches());
        for (uncertain, kind) in [
            (false, StoreFailure::NotCommitted),
            (true, StoreFailure::Uncertain),
        ] {
            let cause = wire_cause(
                turn,
                &WireError::Host(HostError::Journal {
                    site: via_wire::JournalSite::VendorFacts,
                    uncertain,
                }),
            );
            assert_eq!(cause, RouteError::Store { turn, kind });
        }
        assert_eq!(
            wire_cause(turn, &WireError::Message(WireFailure::MessageTooLarge)),
            RouteError::Overflow { turn }
        );
    }
}
