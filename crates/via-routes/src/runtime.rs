use std::sync::Arc;

use serde_json::to_vec;
use tokio::{
    sync::{mpsc, watch},
    time::timeout_at,
};

use super::{
    ConnectionId, Deadline, FakeMessage, FakeStart, PrivateProcessSpec, RawRef, ReprobeReport,
    RouteError, RouteFailure, RouteMessage, RuntimeConfig, RuntimeResources, SendOutcome,
    StopWatch, StoreFailure, TerminalStatus, TurnNumber, WireRecovery, WireShutdown,
};
use via_wire::{
    CloseMode, CloseRequest, ExitReport, HostError, RawEvidence, StoreError, WireCleanup,
    WireConnection, WireError, WireFailure, WireRuntime, WireSignals,
};

/// Final fake protocol evidence, including independently confirmed process exit.
pub struct FakeRouteResult {
    /// Vendor terminal status.
    pub status: TerminalStatus,
    /// Authoritative final text.
    pub final_text: String,
    /// Vendor stop reason retained verbatim.
    pub stop_reason: String,
    /// Optional vendor failure code.
    pub vendor_code: Option<String>,
    /// Raw reference to the terminal frame.
    pub terminal_raw: RawRef,
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
    /// Every decoded message, including acceptance, the terminal and late observations
    /// after it, is sent on `observations` in decode order with its synced raw span.
    /// When that channel is full the route waits, bounded by `deadline`; a dropped
    /// receiver fails the turn as overflow. On any failure the private group is
    /// force-closed and both pipes are drained under a separate cleanup bound.
    ///
    /// `force` set fails the turn [`RouteError::ForceStopped`]: before launch
    /// nothing starts; after it, frames already read are still forwarded, then
    /// the same cleanup records every remaining vendor byte or reports the raw
    /// log incomplete. The daemon force overrides a stop order.
    ///
    /// `stop` is the turn's stop order (design §2): set before ARM, nothing
    /// launches; after ARM but before the start frame, the group is
    /// force-closed at once; after it, one interrupt is sent, a terminal still
    /// ends the turn normally, and at `force_at` without one the group is
    /// force-closed under `close_by`. Either stop is
    /// [`RouteError::Stopped`].
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is a distinct input of the one turn"
    )]
    pub async fn execute(
        &self,
        connection_id: ConnectionId,
        process: PrivateProcessSpec,
        start: FakeStart,
        observations: mpsc::Sender<RouteMessage>,
        deadline: Deadline,
        force: watch::Receiver<Option<tokio::time::Instant>>,
        stop: StopWatch,
    ) -> Result<FakeRouteResult, RouteFailure> {
        let turn = start.turn();
        let not_launched = |cause| RouteFailure {
            cause,
            evidence: None,
            exit: None,
            raw_incomplete: false,
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
            wake: woken,
            gate,
        };
        let turn_run = self.run_turn(
            connection_id,
            process,
            start,
            &observations,
            deadline,
            signals,
            (force, stop.clone()),
        );
        tokio::select! {
            result = turn_run => result,
            () = wake_on_order(stop, &wake) => unreachable!("the stop waker never returns"),
        }
    }

    /// [`Self::execute`] after its entry checks, while the waker runs.
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is a distinct input of the one turn"
    )]
    async fn run_turn(
        &self,
        connection_id: ConnectionId,
        process: PrivateProcessSpec,
        start: FakeStart,
        observations: &mpsc::Sender<RouteMessage>,
        deadline: Deadline,
        signals: WireSignals,
        (force, stop): (watch::Receiver<Option<tokio::time::Instant>>, StopWatch),
    ) -> Result<FakeRouteResult, RouteFailure> {
        let turn = start.turn();
        let mut wire = self
            .wire
            .open_connection(connection_id, process, deadline, signals)
            .await
            .map_err(|error| acquire_failure(turn, &error, &force))?;
        let signals = (force, stop);
        let drive = Self::drive(&mut wire, start, observations, deadline, &signals);
        let failed = match Box::pin(drive).await {
            Ok(Finished::Result(result)) => return Ok(result),
            Ok(Finished::Late(terminal)) => {
                // Design §2 rule 3 [r1.23]: a decoded terminal is returned
                // even though the wall deadline passed during finalization;
                // cleanup comes from Host's force close.
                let report = wire
                    .close(CloseRequest {
                        mode: CloseMode::Force,
                        deadline: cleanup_deadline(),
                    })
                    .await;
                return Ok(terminal.result(
                    report.vendor_exit.unwrap_or(ExitReport {
                        code: None,
                        signal: None,
                    }),
                    report.cleanup,
                    report.journal_uncertain,
                ));
            }
            Err(failed) => failed,
        };
        // The turn deadline may already have elapsed; cleanup gets its own
        // bound, or the stop order's `close_by`.
        let cleanup = failed.close_by.unwrap_or_else(cleanup_deadline);
        let report = wire
            .close(CloseRequest {
                mode: CloseMode::Force,
                deadline: cleanup,
            })
            .await;
        // The group is stopping; keep both tails as raw evidence. The original
        // failure stays authoritative; the drain only reports lost bytes.
        let raw = Box::pin(wire.drain_to_eof(cleanup)).await;
        Err(RouteFailure {
            cause: failed.cause,
            evidence: failed.evidence,
            exit: failed.exit.or(report.vendor_exit),
            raw_incomplete: raw == RawEvidence::Incomplete,
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

    async fn drive(
        wire: &mut WireConnection,
        start: FakeStart,
        observations: &mpsc::Sender<RouteMessage>,
        deadline: Deadline,
        (force, stop): &(watch::Receiver<Option<tokio::time::Instant>>, StopWatch),
    ) -> Result<Finished, Failed> {
        let turn = start.turn();
        let mut force = force.clone();
        // Design §2 rule 2: after ARM, an order set before the start frame
        // is written: the start is not written and the group closes at once.
        if let Some(order) = stop.borrow().as_ref() {
            return Err(Failed::stopped(turn, order.close_by));
        }
        let mut bytes = to_vec(&start).map_err(|_| protocol(turn, "cannot encode fake start"))?;
        bytes.push(b'\n');
        let sent = wire
            .write_frame(&bytes, deadline)
            .await
            .map_err(|error| Failed::from(wire_cause(turn, &error)))?;
        if sent != SendOutcome::Written {
            return Err(transport(turn).into());
        }
        let mut control = Control {
            turn,
            force: force.clone(),
            stop: stop.clone(),
            interrupted: false,
        };
        let mut phase = Phase::Submitted;
        let terminal = loop {
            let message = match Box::pin(next_message(wire, turn, deadline)).await? {
                Next::Message(message) => message,
                Next::Woken => {
                    control.on_wake(wire, deadline).await?;
                    continue;
                }
                // EOF without a terminal: a Host-confirmed exit is
                // `ProcessExited`. F21: after an unterminated last line
                // (already in the raw log) the wait is bounded by the cleanup
                // allowance and anything but a confirmed exit is transport
                // loss, never a protocol failure.
                // Under the daemon force the exit is the force's own stop
                // (Host's early stop, design §6.8): the force row, never
                // `ProcessExited`.
                end @ (Next::Eof | Next::Unterminated) => {
                    control.after_terminal()?;
                    let unterminated = matches!(end, Next::Unterminated);
                    let exit = control.wait_exit(wire, deadline, unterminated).await?;
                    control.after_terminal()?;
                    return Err(Failed {
                        cause: RouteError::ProcessExited { turn },
                        evidence: None,
                        exit: Some(exit),
                        close_by: None,
                    });
                }
            };
            phase
                .advance(&message.payload, turn, control.interrupted)
                .map_err(|cause| Failed::cited(cause, &message.raw_ref))?;
            let terminal = terminal_evidence(&message);
            forward(observations, message, turn, deadline, &mut force).await?;
            if let Some(terminal) = terminal {
                break terminal;
            }
        };
        match Self::finalize(wire, turn, observations, deadline, &mut phase, &mut control).await {
            Ok(exit) => {
                let close_by = stop
                    .borrow()
                    .as_ref()
                    .map_or_else(cleanup_deadline, |order| order.close_by);
                let close = wire
                    .close(CloseRequest {
                        mode: CloseMode::Graceful,
                        deadline: close_by,
                    })
                    .await;
                Ok(Finished::Result(terminal.result(
                    exit,
                    close.cleanup,
                    close.journal_uncertain,
                )))
            }
            Err(failed) if matches!(failed.cause, RouteError::Deadline { .. }) => {
                Ok(Finished::Late(terminal))
            }
            Err(failed) => Err(failed),
        }
    }

    /// Terminal is semantic completion, not transport EOF. Half-close input
    /// (fake finalization waits on it), then drain both pipes to EOF under the
    /// turn deadline: late observations are forwarded and anything that
    /// breaks the phase order, such as a second terminal, fails the turn. A
    /// stop order no longer forces the turn.
    async fn finalize(
        wire: &mut WireConnection,
        turn: TurnNumber,
        observations: &mpsc::Sender<RouteMessage>,
        deadline: Deadline,
        phase: &mut Phase,
        control: &mut Control,
    ) -> Result<ExitReport, Failed> {
        wire.close_input(deadline)
            .await
            .map_err(|error| Failed::from(wire_cause(turn, &error)))?;
        let mut force = control.force.clone();
        loop {
            match Box::pin(next_message(wire, turn, deadline)).await? {
                Next::Message(message) => {
                    phase
                        .advance(&message.payload, turn, control.interrupted)
                        .map_err(|cause| Failed::cited(cause, &message.raw_ref))?;
                    forward(observations, message, turn, deadline, &mut force).await?;
                }
                Next::Woken => control.after_terminal()?,
                Next::Eof => break,
                Next::Unterminated => {
                    return Err(protocol(turn, "fake stdout ended inside a frame").into());
                }
            }
        }
        loop {
            match wire.wait_exit(deadline).await {
                // Host's early stop raises the force before it stops the
                // vendor (design §6.8), so an exit it caused is read under a
                // set force. Wire hands back a recorded exit without
                // consulting the force, so read it here: the force row, never
                // an exit status Route reports as the vendor's own.
                Ok(exit) => {
                    control.after_terminal()?;
                    return Ok(exit);
                }
                Err(WireError::Woken) => control.after_terminal()?,
                Err(error) => return Err(Failed::from(wire_cause(turn, &error))),
            }
        }
    }
}

/// How `drive` ended without a failure.
enum Finished {
    /// The normal path: terminal, exit and graceful close.
    Result(FakeRouteResult),
    /// A decoded terminal whose finalization outlived the wall deadline.
    Late(TerminalEvidence),
}

/// Route's side of a turn's stop order after the start frame was written.
struct Control {
    turn: TurnNumber,
    force: watch::Receiver<Option<tokio::time::Instant>>,
    stop: StopWatch,
    /// The one interrupt was sent.
    interrupted: bool,
}

impl Control {
    /// Acts on a wake before a terminal (design §2 rules 3 and 4): the daemon
    /// force wins; at `force_at` the group is force-closed under `close_by`;
    /// otherwise the first order sends the one interrupt and reading goes on.
    async fn on_wake(
        &mut self,
        wire: &mut WireConnection,
        deadline: Deadline,
    ) -> Result<(), Failed> {
        if self.force.borrow().is_some() {
            return Err(RouteError::ForceStopped { turn: self.turn }.into());
        }
        let Some((force_at, close_by)) = self
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
            let frame = format!(
                "{{\"type\":\"interrupt\",\"id\":2,\"vendor_turn_id\":\"fake-turn-{}\"}}\n",
                self.turn.get()
            );
            // Not written or cut short: `force_at` still bounds the turn. A
            // raw evidence failure fails the connection (design §7.2 row 6):
            // the group is force-closed under `now + 3 s` and the cause keeps
            // its Store kind, so `WriterLost` and `Uncertain` still latch.
            if let Err(error) = wire.write_frame(frame.as_bytes(), deadline).await
                && let Some(cause) = interrupt_failure(self.turn, &error)
            {
                return Err(cause.into());
            }
        }
        Ok(())
    }

    /// After the terminal only the daemon force ends the turn early.
    fn after_terminal(&self) -> Result<(), Failed> {
        if self.force.borrow().is_some() {
            return Err(RouteError::ForceStopped { turn: self.turn }.into());
        }
        Ok(())
    }

    /// Waits for Host's confirmed exit after EOF, acting on wakes. With
    /// `unterminated` (F21) the wait ends at the cleanup allowance and any
    /// failure is transport loss; otherwise its cause is kept.
    async fn wait_exit(
        &mut self,
        wire: &mut WireConnection,
        deadline: Deadline,
        unterminated: bool,
    ) -> Result<ExitReport, Failed> {
        let bound = if unterminated {
            Deadline::at(deadline.instant().min(cleanup_deadline().instant()))
        } else {
            deadline
        };
        loop {
            match wire.wait_exit(bound).await {
                Ok(exit) => return Ok(exit),
                Err(WireError::Woken) => self.on_wake(wire, deadline).await?,
                Err(WireError::Cancelled) => {
                    return Err(RouteError::ForceStopped { turn: self.turn }.into());
                }
                Err(_) if unterminated => return Err(transport(self.turn).into()),
                Err(error) => return Err(wire_cause(self.turn, &error).into()),
            }
        }
    }
}

/// Wakes Wire's current wait whenever the stop order appears or changes, and
/// again at its `force_at`. Never returns.
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

/// First failure inside `drive`, before cleanup adds raw completeness.
struct Failed {
    cause: RouteError,
    evidence: Option<RawRef>,
    exit: Option<ExitReport>,
    /// A stop order's bound on the force close and drain.
    close_by: Option<Deadline>,
}

impl Failed {
    /// A failure proved by one synced frame.
    fn cited(cause: RouteError, evidence: &RawRef) -> Self {
        Self {
            cause,
            evidence: Some(evidence.clone()),
            exit: None,
            close_by: None,
        }
    }

    /// A stop order forcing the turn, closed under `close_by`.
    fn stopped(turn: TurnNumber, close_by: Deadline) -> Self {
        Self {
            cause: RouteError::Stopped { turn },
            evidence: None,
            exit: None,
            close_by: Some(close_by),
        }
    }
}

impl From<RouteError> for Failed {
    fn from(cause: RouteError) -> Self {
        Self {
            cause,
            evidence: None,
            exit: None,
            close_by: None,
        }
    }
}

/// Bounds Host cleanup and the raw drain separately from the turn deadline, which
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
                | FakeMessage::UnknownNotification { .. },
                Self::Submitted,
            ) => return Err(protocol(turn, "fake observation before acceptance")),
            (
                FakeMessage::Text { .. }
                | FakeMessage::ToolStarted { .. }
                | FakeMessage::ToolEnded { .. }
                | FakeMessage::UnknownNotification { .. },
                Self::Accepted | Self::Terminated,
            ) => {}
        }
        Ok(())
    }
}

/// Terminal fields retained for the route result.
struct TerminalEvidence {
    status: TerminalStatus,
    final_text: String,
    stop_reason: String,
    vendor_code: Option<String>,
    raw_ref: RawRef,
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
            final_text: self.final_text,
            stop_reason: self.stop_reason,
            vendor_code: self.vendor_code,
            terminal_raw: self.raw_ref,
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
            final_text,
            stop_reason,
            vendor_code,
            ..
        } => Some(TerminalEvidence {
            status: *status,
            final_text: final_text.clone(),
            stop_reason: stop_reason.clone(),
            vendor_code: vendor_code.clone(),
            raw_ref: message.raw_ref.clone(),
        }),
        FakeMessage::Accepted { .. }
        | FakeMessage::Text { .. }
        | FakeMessage::ToolStarted { .. }
        | FakeMessage::ToolEnded { .. }
        | FakeMessage::InterruptAck { .. }
        | FakeMessage::UnknownNotification { .. } => None,
    }
}

/// What the next read produced.
enum Next {
    /// One decoded frame with its synced raw span.
    Message(RouteMessage),
    /// Both pipes reached EOF.
    Eof,
    /// Stdout ended inside a frame; its bytes are in the raw log (F21).
    Unterminated,
    /// Route's wake ended the wait before any byte was read.
    Woken,
}

/// Reads and decodes the next synced frame.
async fn next_message(
    wire: &mut WireConnection,
    turn: TurnNumber,
    deadline: Deadline,
) -> Result<Next, Failed> {
    let frame = match wire.next_frame(deadline).await {
        Ok(Some(frame)) => frame,
        Ok(None) => return Ok(Next::Eof),
        Err(WireError::Woken) => return Ok(Next::Woken),
        Err(WireError::Frame(WireFailure::UnterminatedFrame)) => return Ok(Next::Unterminated),
        Err(error) => return Err(Failed::from(wire_cause(turn, &error))),
    };
    let payload = FakeMessage::decode(frame.bytes(), turn)
        .map_err(|cause| Failed::cited(cause, frame.raw_ref()))?;
    Ok(Next::Message(RouteMessage {
        payload,
        raw_ref: frame.raw_ref().clone(),
    }))
}

/// Waits for observation capacity until the turn deadline; a consumer that neither
/// drains nor stays attached is an overflow, never a silent drop. A force ends
/// the wait: the unsent message's bytes are already in the raw log, and Route's
/// force close and drain follow.
async fn forward(
    observations: &mpsc::Sender<RouteMessage>,
    message: RouteMessage,
    turn: TurnNumber,
    deadline: Deadline,
    force: &mut watch::Receiver<Option<tokio::time::Instant>>,
) -> Result<(), Failed> {
    let sent = tokio::select! {
        // Capacity first: a draining consumer still receives frames already read.
        biased;
        sent = timeout_at(deadline.instant(), observations.send(message)) => sent,
        () = forced(force) => return Err(RouteError::ForceStopped { turn }.into()),
    };
    match sent {
        Ok(Ok(())) => Ok(()),
        // A forced Adapter stops taking messages; that is the force, not overflow.
        Ok(Err(_)) if force.borrow().is_some() => Err(RouteError::ForceStopped { turn }.into()),
        Ok(Err(_)) | Err(_) => Err(RouteError::Overflow { turn }.into()),
    }
}

/// Resolves once `force` is set; never when its sender is gone unset.
async fn forced(force: &mut watch::Receiver<Option<tokio::time::Instant>>) {
    if force.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
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
    let (cause, launched, raw, cleanup, forced, journal_uncertain) = if let WireError::Acquire {
        cause,
        launched,
        raw,
        cleanup,
        forced,
        journal_uncertain,
    } = error
    {
        (
            cause.as_ref(),
            *launched,
            *raw,
            *cleanup,
            *forced,
            *journal_uncertain,
        )
    } else {
        (error, false, RawEvidence::Complete, None, false, false)
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
        evidence: None,
        exit: None,
        raw_incomplete: raw == RawEvidence::Incomplete,
        launched,
        cleanup,
        forced,
        journal_uncertain,
    }
}

/// Classifies a failed raw append for Core (design §7.1 [r5.5, r5.6]): raw
/// I/O and a full raw queue are `Raw`; a lost raw thread is `WriterLost`.
fn raw_failure(error: &StoreError) -> StoreFailure {
    match error {
        StoreError::WriterLost => StoreFailure::WriterLost,
        StoreError::NotEnqueued => StoreFailure::NotEnqueued,
        StoreError::Uncertain(_) | StoreError::Corrupt(_) => StoreFailure::Uncertain,
        StoreError::Open(_)
        | StoreError::Write(_)
        | StoreError::Raw(_)
        | StoreError::Constraint(_)
        | StoreError::CorruptEvidence
        | StoreError::Refused(_) => StoreFailure::Raw,
    }
}

/// The cause a failed interrupt write ends the turn with (design §2 rule 3,
/// §7.2 row 6). The interrupt's raw record failed: the connection fails with
/// its classified cause. A transport failure is tolerated, because
/// `force_at` still bounds the turn.
fn interrupt_failure(turn: TurnNumber, error: &WireError) -> Option<RouteError> {
    match error {
        WireError::Raw(_)
        | WireError::RawDeadline
        | WireError::Frame(WireFailure::RawStore | WireFailure::RawRangeMismatch) => {
            Some(wire_cause(turn, error))
        }
        WireError::Host(_)
        | WireError::Io(_)
        | WireError::Deadline
        | WireError::Cancelled
        | WireError::Woken
        | WireError::Acquire { .. }
        | WireError::Frame(
            WireFailure::FrameTooLarge
            | WireFailure::UnterminatedFrame
            | WireFailure::Overflow
            | WireFailure::Transport,
        ) => None,
    }
}

/// Keeps a Wire failure's cause for Core's C1 class decision.
fn wire_cause(turn: TurnNumber, error: &WireError) -> RouteError {
    match error {
        WireError::Raw(error) => RouteError::Store {
            turn,
            kind: raw_failure(error),
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
        // so an append it outlived is C1 `deadline_wall`; so is an acquisition,
        // which Host bounds by the same deadline. The failure drain runs under
        // the cleanup deadline and reports lost bytes only, never a cause.
        WireError::Deadline | WireError::RawDeadline | WireError::Host(HostError::Deadline) => {
            RouteError::Deadline { turn }
        }
        WireError::Cancelled => RouteError::ForceStopped { turn },
        WireError::Acquire { cause, .. } => wire_cause(turn, cause),
        WireError::Frame(WireFailure::FrameTooLarge) => {
            protocol(turn, "fake stdout line exceeds the 1 MiB frame cap")
        }
        WireError::Frame(WireFailure::UnterminatedFrame) => {
            protocol(turn, "fake stdout ended inside a frame")
        }
        WireError::Frame(WireFailure::RawRangeMismatch | WireFailure::RawStore) => {
            RouteError::Store {
                turn,
                kind: StoreFailure::Raw,
            }
        }
        WireError::Frame(WireFailure::Overflow) => RouteError::Overflow { turn },
        WireError::Frame(WireFailure::Transport)
        | WireError::Io(_)
        | WireError::Host(_)
        | WireError::Woken => transport(turn),
    }
}

fn protocol(turn: TurnNumber, detail: &'static str) -> RouteError {
    RouteError::Protocol { turn, detail }
}

fn transport(turn: TurnNumber) -> RouteError {
    RouteError::TransportLost {
        turn,
        evidence: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FakeMessage, HostError, Phase, RouteError, StoreError, StoreFailure, TurnNumber, WireError,
        WireFailure, interrupt_failure, raw_failure, wire_cause,
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
    const OBSERVATIONS: [&str; 4] = [
        r#"{"type":"text","vendor_turn_id":"fake-turn-1","text":"hi"}"#,
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

    /// Design §7.1 [r5.5, r5.6]: raw failures keep their classified kind,
    /// and Host journal failures report their outcome.
    #[test]
    fn store_failures_keep_their_kind() {
        assert_eq!(
            raw_failure(&StoreError::Raw("io".into())),
            StoreFailure::Raw
        );
        assert_eq!(
            raw_failure(&StoreError::WriterLost),
            StoreFailure::WriterLost
        );
        assert_eq!(
            raw_failure(&StoreError::Uncertain("commit".into())),
            StoreFailure::Uncertain
        );
        assert!(StoreFailure::WriterLost.latches() && !StoreFailure::Raw.latches());
        let turn = TurnNumber::try_from(1).unwrap();
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
    }

    /// S1 round-1 decision 4: a raw failure at the interrupt write ends the
    /// turn with its Store kind, so `WriterLost` and `Uncertain` still latch;
    /// a transport failure there is tolerated.
    #[test]
    fn interrupt_write_failures_keep_their_store_kind() {
        let turn = TurnNumber::try_from(1).unwrap();
        for (error, kind) in [
            (StoreError::WriterLost, StoreFailure::WriterLost),
            (
                StoreError::Uncertain("commit".into()),
                StoreFailure::Uncertain,
            ),
            (StoreError::Raw("io".into()), StoreFailure::Raw),
        ] {
            assert_eq!(
                interrupt_failure(turn, &WireError::Raw(error)),
                Some(RouteError::Store { turn, kind })
            );
        }
        assert_eq!(
            interrupt_failure(turn, &WireError::Frame(WireFailure::RawStore)),
            Some(RouteError::Store {
                turn,
                kind: StoreFailure::Raw
            })
        );
        assert_eq!(
            interrupt_failure(turn, &WireError::RawDeadline),
            Some(RouteError::Deadline { turn })
        );
        for tolerated in [
            WireError::Io(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            WireError::Cancelled,
            WireError::Frame(WireFailure::Transport),
        ] {
            assert_eq!(interrupt_failure(turn, &tolerated), None);
        }
    }
}
