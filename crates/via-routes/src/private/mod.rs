//! The private per-turn process lifecycle (S1 rules 1 to 4, F21, AD4's one
//! cutoff; x.3.2 ruling Q1): one VIA turn on its own private process, from
//! the entry checks through the start, the read loop, the terminal's
//! finalization or late path, and the process's cleanup. One hardened copy
//! serves the closed set of protocols that run this way, the fake's,
//! Claude Code's and Pi's ([`PrivateProtocol`]); each supplies only its encoding,
//! its decoding and admission rules, its terminal evidence and the fake's
//! persistent-profile emulation.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::{
    Deadline, PrivateProcessSpec, Retirement, RouteError, RouteFailure, RouteRuntime, SendOutcome,
    StopSources, StopWatch, TurnNumber,
};
use via_wire::{
    CloseMode, CloseRequest, ExitReport, OutboundMessage, WireCleanup, WireCloseReport,
    WireMessages, WireParts, WireSender, WireSignals, WriteBounds,
};

mod serving;

pub(crate) use serving::{
    CLEANUP_ALLOWANCE, Failed, Interrupt, Next, Serving, Signals, cleanup_deadline, pending,
    protocol, transport, wire_cause,
};
use serving::{acquire_failure, wake_on_order};

/// One hop item: what Route admitted, with the instant Route read it from
/// the vendor's stdout. A message's observations take that instant, not
/// the one the Adapter dequeues it at, so time it waits in Route's
/// read-ahead or on the hop moves no idle deadline (bead via-mnx, C2 §4,
/// runtime §8; x.3.2 critical r1 #3).
#[derive(Debug)]
pub struct Decoded<M> {
    /// The admitted message.
    pub item: M,
    /// When Route read it.
    pub at: tokio::time::Instant,
    /// The turn's [`DecodeWatermark`] once Route admitted it: its position
    /// in decode order. An item Route made itself carries the watermark
    /// as it stood, the decoded messages before it.
    pub seq: u64,
}

/// A turn's decode watermark (runtime §8; x.3.2 critical r2 #2): how many
/// vendor messages Route has read and admitted for the hop, advanced as
/// each is admitted, before it waits for read-ahead room or the hop. The
/// Adapter reports how far it delivered against it, so Core's idle
/// deadline is decided only once what Route read by then was reconciled.
/// The Adapter creates it with the turn's activity clock; Route only
/// advances it.
#[derive(Clone, Debug, Default)]
pub struct DecodeWatermark(Arc<std::sync::atomic::AtomicU64>);

impl DecodeWatermark {
    /// The messages admitted so far.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Counts one more admitted message; its position.
    pub(crate) fn advance(&self) -> u64 {
        self.0
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .saturating_add(1)
    }
}

/// What a turn hands the Adapter: the hop's sending side, and the turn's
/// decode watermark, which Route advances as it reads.
#[derive(Debug)]
pub struct Hop<M> {
    items: mpsc::Sender<Decoded<M>>,
    decoded: DecodeWatermark,
}

impl<M> Hop<M> {
    /// The hop `items` with the turn's watermark `decoded`.
    pub fn new(items: mpsc::Sender<Decoded<M>>, decoded: DecodeWatermark) -> Self {
        Self { items, decoded }
    }

    /// The hop's sending side.
    pub(crate) fn items(&self) -> &mpsc::Sender<Decoded<M>> {
        &self.items
    }

    /// `item`, read now: its position the watermark's next.
    pub(crate) fn read(&self, item: M, at: tokio::time::Instant) -> Decoded<M> {
        Decoded {
            item,
            at,
            seq: self.decoded.advance(),
        }
    }

    /// `item`, as of now: one Route made rather than read, behind every
    /// message admitted so far.
    pub(crate) fn made(&self, item: M) -> Decoded<M> {
        Decoded {
            item,
            at: tokio::time::Instant::now(),
            seq: self.decoded.get(),
        }
    }
}

/// The daemon force: `None` until raised, then the instant it was raised.
pub(crate) type ForceWatch = watch::Receiver<Option<tokio::time::Instant>>;

/// A protocol that runs one VIA turn per private process. The set is
/// closed: the fake's lane, Claude Code's and Pi's, crate-private, so the hop,
/// terminal and result types stay each protocol's own while the lifecycle
/// is shared. The implementor is the protocol's per-turn state.
pub(crate) trait PrivateProtocol: Sized + Send {
    /// What opens the protocol's state: its per-turn inputs and where the
    /// turn's result goes.
    type Input: Send;
    /// The source of the one start message.
    type Start: Send;
    /// A decoded message before its admission.
    type Payload: Send;
    /// What the hop carries to the Adapter.
    type Message: Send;
    /// The decoded terminal evidence the lifecycle keeps.
    type Terminal: Send;
    /// The route's result once the process was finalized.
    type Result: Send;
    /// A logical turn that ended with its server kept (the fake's
    /// persistent profile); uninhabited for a protocol without one.
    type Kept: Send;
    /// The token of a failure whose server stays (the fake's persistent
    /// profile); uninhabited for a protocol without one.
    type Keep: Send;
    /// An event of the protocol's own controls (the fake's steer lane and
    /// P7 bound); uninhabited for a protocol without any.
    type Event: Send;

    /// How many decoded messages may wait for room on the hop before Route
    /// stops reading, until the terminal; after it, one.
    const READ_AHEAD: usize;
    /// The protocol failure's detail when stdout ends inside a message.
    const UNTERMINATED: &'static str;

    /// The state for a turn whose connection opened.
    fn open(input: Self::Input) -> Self;
    /// Hands a turn whose connection never opened its result.
    fn unopened(input: Self::Input, result: Result<Self::Result, RouteFailure>);
    /// Hands an opened turn its result, unless the protocol already did,
    /// with the one interrupt's final state.
    fn finish(self, interrupt: Interrupt, result: Result<Self::Result, RouteFailure>);
    /// The process facts a result carries.
    fn evidence(result: &Self::Result) -> Closed;
    /// The result of a finalized `terminal`, with Host's `exit` and `close`.
    fn result(terminal: Self::Terminal, exit: ExitReport, close: &WireCloseReport) -> Self::Result;
    /// The start's turn.
    fn turn_of(start: &Self::Start) -> TurnNumber;
    /// The start as Wire writes it.
    fn start_message(start: Self::Start) -> Result<OutboundMessage, RouteError>;
    /// The start's write begins.
    fn submitted(&mut self);
    /// Decodes one message; an error is the turn's protocol failure, after
    /// Wire kept the bytes.
    fn decode(bytes: &[u8], turn: TurnNumber) -> Result<Self::Payload, RouteError>;
    /// Checks a decoded message against the connection's phase and records
    /// its facts; `None` keeps it from the hop. It may answer the vendor on
    /// the control lane first.
    fn admit<'s>(
        serving: &'s mut Serving<'_, Self>,
        payload: Self::Payload,
    ) -> impl Future<Output = Result<Option<Self::Message>, Failed>> + Send + 's;
    /// The terminal evidence a handed-over message carries, if any.
    fn terminal(message: &Self::Message) -> Option<Self::Terminal>;
    /// Keeps the decoded terminal (AD4).
    fn retain(serving: &mut Serving<'_, Self>, terminal: &Self::Terminal);
    /// Reads what the protocol needs before the start (AD7); nothing on a
    /// protocol whose handshake follows the start.
    fn handshake<'s>(
        serving: &'s mut Serving<'_, Self>,
        messages: &'s mut WireMessages,
    ) -> impl Future<Output = Result<(), Failed>> + Send + 's;
    /// The one interrupt's bytes, noted by the protocol as sent.
    fn interrupt(serving: &mut Serving<'_, Self>) -> OutboundMessage;
    /// Whether a live process's closed stdout is transport loss bounded
    /// by the cleanup allowance (the fake's persistent profile).
    fn bounded_exit(&self) -> bool;
    /// Whether the Adapter's stall before the terminal interrupts the
    /// vendor and runs the turn on as an internal stop order (C2 A1,
    /// [`serving::Stall`]); otherwise it fails the turn `overflow` at once.
    fn interrupts_on_stall(&self) -> bool;
    /// After the terminal: finalize the process, or end the logical turn
    /// with the server kept.
    fn after_terminal<'s>(
        serving: &'s mut Serving<'_, Self>,
        messages: &'s mut WireMessages,
        terminal: Self::Terminal,
    ) -> impl Future<Output = Result<AfterTerminal<Self>, Failed>> + Send + 's;
    /// The kept turn's end, then its process's retirement.
    fn kept<'s>(
        serving: &'s mut Serving<'_, Self>,
        sender: &'s WireSender,
        messages: WireMessages,
        kept: Self::Kept,
    ) -> impl Future<Output = Result<Self::Result, RouteFailure>> + Send + 's;
    /// Whether a failure leaves the server running.
    fn keeps_server(serving: &Serving<'_, Self>, cause: &RouteError) -> Option<Self::Keep>;
    /// A failure whose server stays: its logical end, then the retirement.
    fn keep<'s>(
        serving: &'s mut Serving<'_, Self>,
        connection: (&'s WireSender, WireMessages),
        keep: Self::Keep,
        failure: (Failed, Deadline),
    ) -> impl Future<Output = RouteFailure> + Send + 's;
    /// Whether [`Self::event`] may resolve now.
    fn has_event(&self) -> bool;
    /// The protocol's next control event; never resolves without one. It
    /// is cancel-safe.
    fn event(&mut self) -> impl Future<Output = Self::Event> + Send + '_;
    /// Acts on an event.
    fn on_event(serving: &mut Serving<'_, Self>, event: Self::Event) -> Result<(), Failed>;
}

/// What [`PrivateProtocol::after_terminal`] decided.
pub(crate) enum AfterTerminal<P: PrivateProtocol> {
    /// Finalize the process: stdin EOF, stdout to EOF, its exit.
    Finalize(P::Terminal),
    /// The logical turn ended; the server stays.
    Kept(P::Kept),
}

/// The process facts of a route result.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Closed {
    /// Host-confirmed exit; both fields `None` when none was confirmed.
    pub(crate) exit: ExitReport,
    /// Group cleanup certainty.
    pub(crate) cleanup: WireCleanup,
    /// A Host journal write had an uncertain outcome.
    pub(crate) journal_uncertain: bool,
    /// Host stopped the group while its vendor was live.
    pub(crate) forced: bool,
}

/// The turn's protocol state, or its inputs when no connection opened.
enum Lane<P: PrivateProtocol> {
    Unopened(P::Input),
    Opened((P, Interrupt)),
}

/// The facts of a route result.
fn retirement<P: PrivateProtocol>(result: &Result<P::Result, RouteFailure>) -> Retirement {
    match result {
        Ok(result) => {
            let closed = P::evidence(result);
            Retirement {
                launched: true,
                exit: (closed.exit.code.is_some() || closed.exit.signal.is_some())
                    .then_some(closed.exit),
                cleanup: Some(closed.cleanup),
                forced: closed.forced,
                journal_uncertain: closed.journal_uncertain,
            }
        }
        Err(failure) => Retirement {
            launched: failure.launched,
            exit: failure.exit,
            cleanup: failure.cleanup,
            forced: failure.forced,
            journal_uncertain: failure.journal_uncertain,
        },
    }
}

/// Sends one prompt after durable submission and awaits the paired
/// terminal and real exit (adapter design §3.2).
///
/// Every decoded message the protocol admits, including acceptance, the
/// terminal and late observations after it, is sent on the `hop` in decode
/// order. While [`PrivateProtocol::READ_AHEAD`] messages wait for room on
/// the hop Route reads no further message, and a closed hop (the Adapter's
/// stall, C2 A1) fails the turn as overflow: on a protocol that interrupts
/// on it, after an internal stop order ([`serving::Stall`]) that the turn
/// runs on to its terminal or escalation. Every wait keeps the controls
/// serviced (Task 4 design §9). On any failure the private group is
/// force-closed and the connection finished under a separate cleanup bound.
///
/// `force` set fails the turn [`RouteError::ForceStopped`]: before launch
/// nothing starts; after it, the same cleanup follows. The daemon force
/// overrides a stop order.
///
/// `stop` is the turn's stop order (design §2): set before ARM, nothing
/// launches; after ARM but before the start message, the group is
/// force-closed at once; after it, one interrupt is sent, a terminal still
/// ends the turn normally, and at `force_at` without one the group is
/// force-closed under `close_by`. Either stop is [`RouteError::Stopped`].
/// `sources` reports an order set where a relayed `stop` has not caught up
/// yet; the entry check and the launch gate read it too.
///
/// The protocol hands the turn its result ([`PrivateProtocol::finish`]);
/// this returns the process's retirement.
pub(crate) async fn turn<P: PrivateProtocol>(
    runtime: &RouteRuntime,
    process: PrivateProcessSpec,
    start: P::Start,
    hop: Hop<P::Message>,
    signals: (Deadline, ForceWatch, (StopWatch, StopSources)),
    input: P::Input,
) -> Retirement {
    let (result, lane) = run::<P>(runtime, process, start, hop, signals, input).await;
    let retirement = retirement::<P>(&result);
    match lane {
        Lane::Unopened(input) => P::unopened(input, result),
        Lane::Opened((lane, interrupt)) => lane.finish(interrupt, result),
    }
    retirement
}

/// The entry checks, then the turn while the waker runs.
async fn run<P: PrivateProtocol>(
    runtime: &RouteRuntime,
    process: PrivateProcessSpec,
    start: P::Start,
    hop: Hop<P::Message>,
    (deadline, force, (stop, sources)): (Deadline, ForceWatch, (StopWatch, StopSources)),
    input: P::Input,
) -> (Result<P::Result, RouteFailure>, Lane<P>) {
    let turn = P::turn_of(&start);
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
        launch: None,
    };
    if force.borrow().is_some() {
        return (
            Err(not_launched(RouteError::ForceStopped { turn })),
            Lane::Unopened(input),
        );
    }
    // An order set before submission reached Route: nothing starts, and
    // no anchor intent exists.
    if stop.borrow().is_some() || sources() {
        return (
            Err(not_launched(RouteError::Stopped { turn })),
            Lane::Unopened(input),
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
        inbound: via_wire::InboundBounds::DEFAULT,
    };
    let turn_run = run_turn::<P>(
        runtime,
        (process, start),
        &hop,
        deadline,
        signals,
        Signals {
            force,
            stop: stop.clone(),
            wake: woken,
        },
        input,
    );
    tokio::select! {
        result = turn_run => result,
        () = wake_on_order(stop, &wake) => unreachable!("the stop waker never returns"),
    }
}

/// [`turn`] after its entry checks, while the waker runs. Every exit after
/// the connection opened finishes it once, under the graceful close's
/// `close_by`, the force close's cleanup deadline or the stop order's
/// `close_by` (design §8.6).
async fn run_turn<P: PrivateProtocol>(
    runtime: &RouteRuntime,
    (process, start): (PrivateProcessSpec, P::Start),
    hop: &Hop<P::Message>,
    deadline: Deadline,
    wire_signals: WireSignals,
    signals: Signals,
    input: P::Input,
) -> (Result<P::Result, RouteFailure>, Lane<P>) {
    let turn = P::turn_of(&start);
    let connection = match runtime
        .wire()
        .open_connection(process, deadline, wire_signals)
        .await
    {
        Ok(connection) => connection,
        Err(error) => {
            return (
                Err(acquire_failure::<P>(turn, &error, &signals.force)),
                Lane::Unopened(input),
            );
        }
    };
    let WireParts { sender, messages } = connection.into_parts();
    let mut serving = Serving::new(turn, &sender, hop, deadline, signals, P::open(input));
    let result = serve_turn(&mut serving, &sender, messages, start).await;
    (result, Lane::Opened(serving.into_lane()))
}

/// [`run_turn`] once the connection is open: every exit finishes the
/// connection.
async fn serve_turn<P: PrivateProtocol>(
    serving: &mut Serving<'_, P>,
    sender: &WireSender,
    mut messages: WireMessages,
    start: P::Start,
) -> Result<P::Result, RouteFailure> {
    let drive = drive(serving, &mut messages, start);
    let failed = match Box::pin(drive).await {
        Ok(Finished::Result(result, close_by)) => {
            messages.finish(close_by).await;
            // Design §2 rule 4: the daemon force ends the turn even
            // after its terminal, once the terminal's data was handed on.
            return serving.unless_forced(result, sender.take_undecoded());
        }
        Ok(Finished::Late(terminal, last)) => {
            return late(serving, sender, messages, (terminal, last)).await;
        }
        Ok(Finished::Kept(kept)) => {
            return P::kept(serving, sender, messages, kept).await;
        }
        Err(failed) => failed,
    };
    // One cutoff (AD4): the wall's cleanup bound is 3 s from the wall,
    // for every step after it; a stop order's is its `close_by`.
    let cleanup = match failed.cause {
        RouteError::Deadline { .. } => Deadline::at(serving.deadline.instant() + CLEANUP_ALLOWANCE),
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
    if let Some(keep) = P::keeps_server(serving, &failed.cause) {
        return Err(P::keep(serving, (sender, messages), keep, (failed, cleanup)).await);
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
    // Decided once cleanup ended (review r3 #1): the daemon force, set at
    // any point before this return, decides the outcome (S1 rule 4); after
    // the Adapter's stall the cause is `overflow` unless a stop order's own
    // escalation governs.
    let cause = if serving.signals.force.borrow().is_some() {
        RouteError::ForceStopped { turn: serving.turn }
    } else if serving.stall.is_some()
        && !matches!(
            failed.cause,
            RouteError::ForceStopped { .. } | RouteError::Stopped { .. }
        )
    {
        RouteError::Overflow { turn: serving.turn }
    } else {
        failed.cause
    };
    Err(RouteFailure {
        cause,
        // The one message this turn could not decode, if any (design §7.3).
        undecoded: sender.take_undecoded(),
        exit: failed.exit.or(report.vendor_exit),
        launched: true,
        cleanup: Some(report.cleanup),
        forced: report.forced,
        journal_uncertain: report.journal_uncertain,
        acknowledged: false,
        shared: false,
        launch: None,
    })
}

/// Design §2 rule 3 [r1.23]: a decoded terminal whose finalization
/// outlived the wall deadline. Host's force close starts at once and the
/// messages still held, such as the terminal with its final text, are
/// delivered to the hop meanwhile, delivery-only; delivery, close and
/// drain share one absolute deadline (runtime §5.2). A decoded terminal is
/// never `Deadline`: delivery that cannot finish by then is `Overflow`, and
/// the daemon force, set at any point, is `ForceStopped` (rule 4); either
/// keeps the close's evidence. Otherwise the terminal is returned with that
/// evidence.
async fn late<P: PrivateProtocol>(
    serving: &mut Serving<'_, P>,
    sender: &WireSender,
    messages: WireMessages,
    (terminal, last): (P::Terminal, Option<Decoded<P::Message>>),
) -> Result<P::Result, RouteFailure> {
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
    let (report, delivered) = tokio::join!(close, serving.deliver_held(by, last));
    messages.finish(by).await;
    let result = P::result(
        terminal,
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

/// How `drive` ended without a failure.
enum Finished<P: PrivateProtocol> {
    /// The normal path: terminal, exit and graceful close, with the close's
    /// bound for `finish`.
    Result(P::Result, Deadline),
    /// A decoded terminal whose finalization, or whose wait for read-ahead
    /// room (then still to go on the hop, last), outlived the wall deadline.
    Late(P::Terminal, Option<Decoded<P::Message>>),
    /// The logical turn ended at its terminal with its server kept (C2
    /// §4.1).
    Kept(P::Kept),
}

/// Writes the start, reads and forwards messages to the terminal, then
/// finalizes. Every wait goes through [`Serving::serve`].
async fn drive<P: PrivateProtocol>(
    serving: &mut Serving<'_, P>,
    messages: &mut WireMessages,
    start: P::Start,
) -> Result<Finished<P>, Failed> {
    let turn = serving.turn;
    // Design §2 rule 2: after ARM, an order set before the start message
    // is written: the start is not written and the group closes at once.
    if let Some(order) = serving.signals.stop.borrow().as_ref() {
        return Err(Failed::stopped(turn, order.close_by));
    }
    P::handshake(serving, messages).await?;
    // Checked again after the handshake, immediately before the start:
    // an order set meanwhile sends nothing at all.
    if let Some(order) = serving.signals.stop.borrow().as_ref() {
        return Err(Failed::stopped(turn, order.close_by));
    }
    let start = P::start_message(start).map_err(Failed::from)?;
    // Submission begins: from here a stop order sends the interrupt.
    serving.submitted = true;
    serving.lane.submitted();
    // While the start is pending no message is read: nothing the vendor
    // answers is taken before its whole input is written.
    let write = serving
        .sender
        .write(start, WriteBounds::CutAt(serving.deadline));
    let sent = serving
        .serve(write)
        .await?
        .map_err(|error| Failed::from(wire_cause::<P>(turn, &error)))?;
    if sent != SendOutcome::Written {
        return Err(transport(turn).into());
    }
    // The vendor has the whole prompt: from here the Adapter's stall
    // interrupts it (review r3 #2).
    serving.written = true;
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
        let Some(terminal) = P::terminal(&message.0.item) else {
            serving.hold(message);
            continue;
        };
        // S1 rule 3 (review r2 #4): the terminal is kept and the turn
        // terminated before it waits for read-ahead room, so no stop order
        // acts on it and the wall takes the late path.
        P::retain(serving, &terminal);
        serving.terminated = true;
        match serving.make_room_for(message.1, P::READ_AHEAD).await {
            Ok(()) => serving.hold(message),
            Err(failed) if matches!(failed.cause, RouteError::Deadline { .. }) => {
                return Ok(Finished::Late(terminal, Some(message.0)));
            }
            Err(failed) => return Err(failed),
        }
        break terminal;
    };
    let terminal = match P::after_terminal(serving, messages, terminal).await? {
        AfterTerminal::Finalize(terminal) => terminal,
        AfterTerminal::Kept(kept) => return Ok(Finished::Kept(kept)),
    };
    match finalize(serving, messages).await {
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
                P::result(terminal, exit, &close),
                close_by,
            ))
        }
        Err(failed) if matches!(failed.cause, RouteError::Deadline { .. }) => {
            Ok(Finished::Late(terminal, None))
        }
        Err(failed) => Err(failed),
    }
}

/// Terminal is semantic completion, not transport EOF. Half-close input
/// (finalization waits on it), then read stdout to EOF under the turn
/// deadline: late observations are forwarded and anything that breaks the
/// phase order, such as a second terminal, fails the turn. A stop order no
/// longer forces the turn.
async fn finalize<P: PrivateProtocol>(
    serving: &mut Serving<'_, P>,
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
        .map_err(|error| Failed::from(wire_cause::<P>(turn, &error)))?;
    loop {
        match serving.next(messages).await? {
            Next::Message(message) => serving.hold(message),
            Next::Eof => break,
            Next::Unterminated => {
                return Err(protocol(turn, P::UNTERMINATED).into());
            }
        }
    }
    serving.flush().await?;
    let exit = serving.sender.wait_exit(serving.deadline);
    let exit = serving
        .serve(exit)
        .await?
        .map_err(|error| Failed::from(wire_cause::<P>(turn, &error)))?;
    // Host's early stop raises the force before it stops the vendor
    // (design §6.8), so an exit it caused is read under a set force.
    // Wire hands back a recorded exit without consulting the force, so
    // read it here: the force row, never an exit status Route reports
    // as the vendor's own.
    serving.after_terminal()?;
    Ok(exit)
}
