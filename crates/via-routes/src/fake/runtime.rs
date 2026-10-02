use std::future::Future;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};

use super::{FakeMessage, RouteMessage, TerminalStatus, TurnStart};
use crate::{
    Deadline, PrivateProcessSpec, ReprobeReport, RouteError, RouteFailure, RuntimeConfig,
    RuntimeResources, SendOutcome, StopSources, StopWatch, StoreFailure, TurnNumber, WireRecovery,
    WireShutdown,
};
use lane::{Facts, Interrupt, LaneState, steer_request, turn_result};
use via_wire::{
    CloseMode, CloseRequest, ExitReport, FailureCause, HostError, LatchState, PendingWrite,
    WireCleanup, WireCloseReport, WireError, WireFailure, WireMessages, WireParts, WireRuntime,
    WireSender, WireSignals,
};

mod lane;

pub use lane::{
    CONTROL_BYTES, CONTROL_COMMANDS, FakeLateTerminal, FakeRetired, FakeRetiredItem, FakeTerminal,
    FakeTurn, Lane, Retirement, SteerAnswer, SteerRefused, SteerRequest, SteerSender, steer_lane,
};

/// Final fake protocol evidence, including independently confirmed process exit.
#[derive(Clone, Debug)]
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
    /// Host stopped the group while its vendor was live (Host force
    /// evidence), as on a late terminal's force close.
    pub forced: bool,
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

    /// Sends one prompt after durable submission and awaits the paired
    /// terminal and real exit (adapter design §3.2), with the `lane`'s
    /// handshake, steer control, interrupt acknowledgement and persistent
    /// profile.
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
    /// [`RouteError::Stopped`]. `sources` reports an order set where a
    /// relayed `stop` has not caught up yet; the entry check and the
    /// launch gate read it too.
    ///
    /// The logical turn, with the terminal and handshake retained on every
    /// outcome (AD4, AD7), goes on `logical` when it ends: on the persistent
    /// profile that can be before its process is retired (C2 §4.1).
    /// Returns the process's retirement.
    pub async fn turn(
        &self,
        process: PrivateProcessSpec,
        start: TurnStart,
        hop: mpsc::Sender<RouteMessage>,
        (deadline, force, stop): (
            Deadline,
            watch::Receiver<Option<tokio::time::Instant>>,
            (StopWatch, StopSources),
        ),
        lane: Lane,
        logical: oneshot::Sender<FakeTurn>,
    ) -> Retirement {
        let persistent = lane.persistent;
        let (result, mut facts) = self
            .run(process, start, hop, (deadline, force, stop), lane, logical)
            .await;
        let retirement = Retirement::of(&result);
        if let Some(logical) = facts.logical.take() {
            // The driver's turn was dropped: nobody reads the logical turn.
            let _unread = logical.send(turn_result(result, facts, persistent));
        }
        retirement
    }

    /// The entry checks, then the turn while the waker runs.
    async fn run(
        &self,
        process: PrivateProcessSpec,
        start: TurnStart,
        hop: mpsc::Sender<RouteMessage>,
        (deadline, force, (stop, sources)): (
            Deadline,
            watch::Receiver<Option<tokio::time::Instant>>,
            (StopWatch, StopSources),
        ),
        lane: Lane,
        logical: oneshot::Sender<FakeTurn>,
    ) -> (Result<FakeRouteResult, RouteFailure>, Facts) {
        let turn = start.turn();
        let logical = Some(logical);
        let not_launched = |cause| RouteFailure {
            cause,
            undecoded: None,
            exit: None,
            launched: false,
            cleanup: None,
            forced: false,
            journal_uncertain: false,
            acknowledged: false,
            shared: false,
        };
        let unlaunched = |logical| Facts {
            logical,
            ..Facts::default()
        };
        if force.borrow().is_some() {
            return (
                Err(not_launched(RouteError::ForceStopped { turn })),
                unlaunched(logical),
            );
        }
        // An order set before submission reached Route: nothing starts, and
        // no anchor intent exists.
        if stop.borrow().is_some() || sources() {
            return (
                Err(not_launched(RouteError::Stopped { turn })),
                unlaunched(logical),
            );
        }
        let (wake, woken) = watch::channel(0_u64);
        let gate = {
            let force = force.clone();
            let stop = stop.clone();
            Arc::new(move || force.borrow().is_some() || stop.borrow().is_some() || sources())
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
            (lane, logical),
        );
        tokio::select! {
            result = turn_run => result,
            () = wake_on_order(stop, &wake) => unreachable!("the stop waker never returns"),
        }
    }

    /// [`Self::turn`] after its entry checks, while the waker runs. Every
    /// exit after the connection opened finishes it once, under the graceful
    /// close's `close_by`, the force close's cleanup deadline or the stop
    /// order's `close_by` (design §8.6).
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is a distinct input of the one turn"
    )]
    async fn run_turn(
        &self,
        process: PrivateProcessSpec,
        start: TurnStart,
        hop: &mpsc::Sender<RouteMessage>,
        deadline: Deadline,
        wire_signals: WireSignals,
        signals: Signals,
        (lane, logical): (Lane, Option<oneshot::Sender<FakeTurn>>),
    ) -> (Result<FakeRouteResult, RouteFailure>, Facts) {
        let turn = start.turn();
        let connection = match self
            .wire
            .open_connection(process, deadline, wire_signals)
            .await
        {
            Ok(connection) => connection,
            Err(error) => {
                return (
                    Err(acquire_failure(turn, &error, &signals.force)),
                    Facts {
                        logical,
                        ..Facts::default()
                    },
                );
            }
        };
        let WireParts { sender, messages } = connection.into_parts();
        let mut serving = Serving::new(turn, &sender, hop, deadline, signals, lane, logical);
        let result = Self::serve_turn(&mut serving, &sender, messages, start).await;
        (result, serving.lane.into_facts())
    }

    /// [`Self::run_turn`] once the connection is open: every exit finishes
    /// the connection.
    async fn serve_turn(
        serving: &mut Serving<'_>,
        sender: &WireSender,
        mut messages: WireMessages,
        start: TurnStart,
    ) -> Result<FakeRouteResult, RouteFailure> {
        let drive = Self::drive(serving, &mut messages, start);
        let failed = match Box::pin(drive).await {
            Ok(Finished::Result(result, close_by)) => {
                messages.finish(close_by).await;
                // Design §2 rule 4: the daemon force ends the turn even
                // after its terminal, once the terminal's data was handed on.
                return serving.unless_forced(result, sender.take_undecoded());
            }
            Ok(Finished::Late(terminal)) => {
                return Self::late(serving, sender, messages, terminal).await;
            }
            Ok(Finished::Kept(terminal, cleanup)) => {
                // C2 §4.1 (persistent profile): the logical turn ended and
                // the emulated server stays; its helper process is retired
                // apart from the turn (decision H1).
                serving.send_logical(Ok(FakeRouteResult {
                    status: terminal.status,
                    stop_reason: terminal.stop_reason.clone(),
                    vendor_code: terminal.vendor_code.clone(),
                    exit: ExitReport {
                        code: None,
                        signal: None,
                    },
                    cleanup,
                    journal_uncertain: false,
                    forced: false,
                }));
                let report = serving
                    .retire(sender, messages, (None, cleanup_deadline()))
                    .await;
                let exit = report.vendor_exit.unwrap_or(ExitReport {
                    code: None,
                    signal: None,
                });
                return Ok(terminal.result(exit, &report));
            }
            Err(failed) => failed,
        };
        // One cutoff (AD4): the wall's cleanup bound is 3 s from the wall,
        // for every step after it; a stop order's is its `close_by`.
        let cleanup = match failed.cause {
            RouteError::Deadline { .. } => {
                Deadline::at(serving.deadline.instant() + CLEANUP_ALLOWANCE)
            }
            RouteError::Protocol { .. }
            | RouteError::TransportLost { .. }
            | RouteError::ProcessExited { .. }
            | RouteError::Overflow { .. }
            | RouteError::Store { .. }
            | RouteError::Stopped { .. }
            | RouteError::ForceStopped { .. }
            | RouteError::ServerLost { .. }
            | RouteError::HandshakeRefused { .. }
            | RouteError::InvalidParam { .. }
            | RouteError::ResumeMismatch { .. } => failed.close_by.unwrap_or_else(cleanup_deadline),
        };
        if serving.keeps_server(&failed.cause) {
            // AD4 (persistent profile): at the wall, the cleanup step is the
            // vendor's soft stop; a stop order's `force_at` asks for no kill
            // (C2 §4.1). The logical turn ends; the helper is retired after.
            if matches!(failed.cause, RouteError::Deadline { .. }) {
                serving.soft_stop(&mut messages, cleanup).await;
            }
            serving.send_logical(Err(serving.kept_failure(failed.cause.clone())));
            let report = serving
                .retire(sender, messages, (failed.exit, cleanup))
                .await;
            return Err(RouteFailure {
                cause: failed.cause,
                undecoded: sender.take_undecoded(),
                exit: failed.exit.or(report.vendor_exit),
                launched: true,
                cleanup: Some(report.cleanup),
                forced: report.forced,
                journal_uncertain: report.journal_uncertain,
                acknowledged: false,
                shared: false,
            });
        }
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
            acknowledged: false,
            shared: false,
        })
    }

    /// Design §2 rule 3 [r1.23]: a decoded terminal whose finalization
    /// outlived the wall deadline. Host's force close starts at once and a
    /// message still held, such as the terminal with its final text, is
    /// delivered to the hop meanwhile, delivery-only; delivery, close and
    /// drain share one absolute deadline (runtime §5.2). A decoded terminal
    /// is never `Deadline`: delivery that cannot finish by then is
    /// `Overflow`, and the daemon force, set at any point, is
    /// `ForceStopped` (rule 4); either keeps the close's evidence.
    /// Otherwise the terminal is returned with that evidence.
    async fn late(
        serving: &mut Serving<'_>,
        sender: &WireSender,
        messages: WireMessages,
        terminal: FakeTerminal,
    ) -> Result<FakeRouteResult, RouteFailure> {
        // Test builds: the terminal is decoded and held, the late path
        // entered; nothing is closed or delivered yet.
        // A failpoint error only ends the pause.
        #[cfg(feature = "test-failpoints")]
        let _ = via_wire::failpoint::hit_async("routes.late.entered").await;
        let by = cleanup_deadline();
        let close = sender.close(CloseRequest {
            mode: CloseMode::Force,
            deadline: by,
        });
        let (report, delivered) = tokio::join!(close, serving.deliver_held(by));
        messages.finish(by).await;
        let result = terminal.result(
            report.vendor_exit.unwrap_or(ExitReport {
                code: None,
                signal: None,
            }),
            &report,
        );
        let undecoded = sender.take_undecoded();
        match delivered {
            Ok(()) => serving.unless_forced(result, undecoded),
            Err(cause) => Err(serving.failure_with(cause, &result, undecoded)),
        }
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
        serving.handshake(messages).await?;
        // Checked again after the handshake, immediately before the start:
        // an order set meanwhile sends nothing at all.
        if let Some(order) = serving.signals.stop.borrow().as_ref() {
            return Err(Failed::stopped(turn, order.close_by));
        }
        let start = start.into_message().map_err(Failed::from)?;
        // Submission begins: from here a stop order sends the interrupt.
        serving.submitted = true;
        serving.phase = Phase::Submitted;
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
                end @ (Next::Eof | Next::Unterminated) => return Err(serving.ended(end).await),
            };
            let terminal = terminal_evidence(&message);
            serving.held = Some(message);
            if let Some(terminal) = terminal {
                serving.retain(&terminal);
                break terminal;
            }
        };
        serving.terminated = true;
        if serving.lane.persistent {
            let cleanup = serving.persistent_end(messages, &terminal).await?;
            return Ok(Finished::Kept(terminal, cleanup));
        }
        match Self::finalize(serving, messages).await {
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
                Ok(Finished::Result(terminal.result(exit, &close), close_by))
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
    ) -> Result<ExitReport, Failed> {
        // Test builds: the terminal is decoded and finalization begins.
        #[cfg(feature = "test-failpoints")]
        let _ = via_wire::failpoint::hit_async("routes.finalize.entered").await;
        let turn = serving.turn;
        let close = serving.sender.close_input(serving.deadline);
        serving
            .serve(close)
            .await?
            .map_err(|error| Failed::from(wire_cause(turn, &error)))?;
        loop {
            match serving.next(messages).await? {
                Next::Message(message) => serving.held = Some(message),
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
    Late(FakeTerminal),
    /// The persistent profile's logical turn ended at its terminal (C2
    /// §4.1), with its cleanup; the server stays.
    Kept(FakeTerminal, WireCleanup),
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
    /// The connection's protocol phase; every read message is checked
    /// against it before any fact is recorded.
    phase: Phase,
    /// The start's write began: a stop order now sends the interrupt.
    submitted: bool,
    /// The terminal was read: a stop order no longer acts.
    terminated: bool,
    /// The pending interrupt write, kept pinned while other waits run.
    pending: Option<PendingWrite>,
    /// A decoded message waiting for room on the hop.
    held: Option<RouteMessage>,
    /// The C2 lane's state.
    lane: LaneState,
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
        lane: Lane,
        logical: Option<oneshot::Sender<FakeTurn>>,
    ) -> Self {
        Self {
            turn,
            sender,
            hop,
            deadline,
            latch: sender.latch(),
            signals,
            phase: Phase::Opening,
            submitted: false,
            terminated: false,
            pending: None,
            held: None,
            lane: LaneState::new(lane, logical),
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
    /// message. Delivery that cannot finish by `by` is `Overflow`.
    async fn deliver_held(&mut self, by: Deadline) -> Result<(), RouteError> {
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
                Err(_) => Err(self.hop_closed().cause),
            },
            () = tokio::time::sleep_until(by.instant()) => Err(RouteError::Overflow { turn }),
        }
    }

    /// `result`, unless the daemon force is set: then `ForceStopped` with
    /// the result's exit and close evidence (design §2 rule 4).
    fn unless_forced(
        &self,
        result: FakeRouteResult,
        undecoded: Option<String>,
    ) -> Result<FakeRouteResult, RouteFailure> {
        if self.signals.force.borrow().is_some() {
            let cause = RouteError::ForceStopped { turn: self.turn };
            return Err(self.failure_with(cause, &result, undecoded));
        }
        Ok(result)
    }

    /// A failure after a decoded terminal, with that terminal's exit and
    /// close evidence; the daemon force outranks any other `cause`.
    fn failure_with(
        &self,
        cause: RouteError,
        result: &FakeRouteResult,
        undecoded: Option<String>,
    ) -> RouteFailure {
        let cause = if self.signals.force.borrow().is_some() {
            RouteError::ForceStopped { turn: self.turn }
        } else {
            cause
        };
        let exit = result.exit;
        RouteFailure {
            cause,
            undecoded,
            exit: (exit.code.is_some() || exit.signal.is_some()).then_some(exit),
            launched: true,
            cleanup: Some(result.cleanup),
            forced: result.forced,
            journal_uncertain: result.journal_uncertain,
            acknowledged: false,
            shared: false,
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
            written = pending(self.pending.as_mut()), if self.pending.is_some() => {
                self.interrupt_written(&written).map(|()| None)
            }
            // C1 P7: a reported tool outlived the window (persistent profile).
            () = sleep_until_set(self.lane.grace), if self.lane.grace.is_some() && !self.lane.tools_settled() => {
                self.lane.grace_expired = true;
                Err(RouteError::Deadline { turn }.into())
            }
            written = pending(self.lane.steer_write.as_mut()), if self.lane.steer_write.is_some() => {
                self.steer_written(&written);
                Ok(None)
            }
            request = steer_request(self.lane.steer.as_mut()), if self.lane.steer.is_some() && self.lane.steer_reply.is_none() && self.lane.steer_write.is_none() => {
                match request {
                    Some(request) => self.on_steer(request),
                    None => self.lane.steer = None,
                }
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
                Ok(payload) => {
                    let interrupted = self.lane.interrupt != Interrupt::NotSent;
                    self.phase
                        .advance(&payload, turn, interrupted)
                        .map_err(Failed::from)?;
                    // The written steer a delivery report answers, taken
                    // before `note` may resolve it.
                    let steer = matches!(payload, FakeMessage::SteerDelivered { .. })
                        .then(|| self.lane.steer_token.take())
                        .flatten();
                    if !self.note(&payload)? {
                        // Recorded, not handed over (C2 §2 Reopen).
                        continue;
                    }
                    Ok(Next::Message(RouteMessage { payload, steer }))
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
    fn hop_closed(&self) -> Failed {
        let turn = self.turn;
        if self.signals.force.borrow().is_some() {
            RouteError::ForceStopped { turn }.into()
        } else {
            RouteError::Overflow { turn }.into()
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

/// S1's cleanup allowance after a failure (AD4's one cutoff after the wall).
const CLEANUP_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(3);

/// Bounds Host cleanup and the stdout drain separately from the turn deadline, which
/// may already have elapsed when cleanup starts.
fn cleanup_deadline() -> Deadline {
    Deadline::at(tokio::time::Instant::now() + CLEANUP_ALLOWANCE)
}

/// Connection-local protocol phase for the only turn on a fake connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Before the start: only the handshake and session-level messages.
    Opening,
    /// Start written; awaiting its paired acceptance.
    Submitted,
    /// Acceptance seen; observations and one terminal may follow.
    Accepted,
    /// Terminal seen; only late observations may follow.
    Terminated,
}

impl Phase {
    /// Checks one decoded message against the phase and advances it. An
    /// `interrupt_ack` is control evidence once Route sent its interrupt;
    /// an unknown message is activity only, in every phase (C2 A1).
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
            // The handshake is read before the start, never after (AD7).
            (FakeMessage::Hello(_), Self::Submitted | Self::Accepted | Self::Terminated) => {
                return Err(protocol(turn, "unexpected fake handshake"));
            }
            // Session-level and unknown messages may come at any time (C2
            // §4, A1).
            (FakeMessage::Hello(_), Self::Opening)
            | (
                FakeMessage::Identity { .. }
                | FakeMessage::VendorClosed { .. }
                | FakeMessage::Unknown { .. },
                _,
            )
            | (
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::Usage { .. }
                | FakeMessage::Denial { .. }
                | FakeMessage::Decline { .. }
                | FakeMessage::SteerDelivered { .. },
                Self::Accepted | Self::Terminated,
            ) => {}
            (
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::Usage { .. }
                | FakeMessage::Denial { .. }
                | FakeMessage::Decline { .. }
                | FakeMessage::SteerDelivered { .. },
                Self::Submitted,
            ) => return Err(protocol(turn, "fake observation before acceptance")),
            (
                FakeMessage::Accepted { .. }
                | FakeMessage::Terminal { .. }
                | FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::Usage { .. }
                | FakeMessage::Denial { .. }
                | FakeMessage::Decline { .. }
                | FakeMessage::SteerDelivered { .. },
                Self::Opening,
            ) => return Err(protocol(turn, "fake message before the start")),
        }
        Ok(())
    }
}

/// The terminal fields go into the route result. The final text is not
/// among them: the Adapter sends it as `final_text` observations from the
/// terminal message itself (Task 4 design §2.3).
impl FakeTerminal {
    fn result(self, exit: ExitReport, close: &WireCloseReport) -> FakeRouteResult {
        FakeRouteResult {
            status: self.status,
            stop_reason: self.stop_reason,
            vendor_code: self.vendor_code,
            exit,
            cleanup: close.cleanup,
            journal_uncertain: close.journal_uncertain,
            forced: close.forced,
        }
    }
}

fn terminal_evidence(message: &RouteMessage) -> Option<FakeTerminal> {
    match &message.payload {
        FakeMessage::Terminal {
            status,
            stop_reason,
            vendor_code,
            details,
            ..
        } => Some(FakeTerminal {
            at: tokio::time::Instant::now(),
            status: *status,
            stop_reason: stop_reason.clone(),
            vendor_code: vendor_code.clone(),
            details: details.clone(),
        }),
        FakeMessage::Accepted { .. }
        | FakeMessage::Text { .. }
        | FakeMessage::ToolStarted { .. }
        | FakeMessage::ToolEnded { .. }
        | FakeMessage::Usage { .. }
        | FakeMessage::InterruptAck { .. }
        | FakeMessage::Hello(_)
        | FakeMessage::Identity { .. }
        | FakeMessage::Denial { .. }
        | FakeMessage::Decline { .. }
        | FakeMessage::SteerDelivered { .. }
        | FakeMessage::VendorClosed { .. }
        | FakeMessage::Unknown { .. } => None,
    }
}

/// Resolves at `at`; never when it is unset.
async fn sleep_until_set(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
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
        acknowledged: false,
        shared: false,
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
    use super::lane::{Interrupt, acknowledges};
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

    /// C2 A1 rule 6: an unknown notification produces no observation and
    /// is admitted in every phase, before acceptance too.
    #[test]
    fn an_unknown_message_is_admitted_in_every_phase() {
        for phase in [Phase::Submitted, Phase::Accepted, Phase::Terminated] {
            let mut phase = { phase };
            assert_eq!(detail(&mut phase, OBSERVATIONS[4]), None, "{phase:?}");
        }
    }

    #[test]
    fn order_violations_fail_the_turn() {
        // The unknown message (the last) is admitted in every phase.
        for message in &OBSERVATIONS[..4] {
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

    /// C2 §7 item 10: vendor evidence acknowledges the interrupt only with
    /// its write confirmed: not while the write is pending, nor after it
    /// failed.
    #[test]
    fn an_acknowledgement_needs_the_confirmed_interrupt_write() {
        assert!(acknowledges(true, Interrupt::Written));
        for interrupt in [Interrupt::NotSent, Interrupt::Queued, Interrupt::Failed] {
            assert!(!acknowledges(true, interrupt), "{interrupt:?}");
        }
        assert!(!acknowledges(false, Interrupt::Written));
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
