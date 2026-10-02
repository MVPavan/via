//! Routes own protocol messages and request pairing; they never choose vendor
//! policy or supervise processes.

use thiserror::Error;

pub use via_wire::{
    AnchorCohort, CloseRequest, Deadline, ExitReport, OutboundMessage, SendOutcome, TurnNumber,
};

/// Task 4 design §2.2 rule 1: an ID, tool name, type tag, `stop_reason` or
/// `vendor_code` longer than this is `protocol`.
pub const SHORT_FIELD_MAX: usize = 1024;

/// Task 4 design §2.2: an unknown message's type tag is kept up to this.
pub const UNKNOWN_TAG_MAX: usize = 256;

/// C2 A1 bound on one encoded observation payload (Task 4 design §2.2
/// rule 2).
pub const MAX_OBSERVATION_BYTES: usize = 256 * 1024;

/// A route turn's typed failure cause, shared by every route; its text is
/// harness-neutral, since it reaches C1 `failure.message`.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RouteError {
    /// A known message was malformed or a response did not match this turn.
    #[error("protocol error in turn {turn:?}: {detail}")]
    Protocol {
        /// Turn whose connection produced the error.
        turn: TurnNumber,
        /// Bounded diagnostic, with no raw vendor message embedded.
        detail: &'static str,
    },
    /// The transport failed while ownership remained with Wire.
    #[error("transport lost in turn {turn:?}")]
    TransportLost {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// Host confirmed the process exited before terminal evidence.
    #[error("process exited in turn {turn:?}")]
    ProcessExited {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// A bounded route or observation queue was exhausted.
    #[error("route overflow in turn {turn:?}")]
    Overflow {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// A storage step the turn depends on failed: the evidence folder or
    /// `stderr.log` (Task 4 design §7.2) or a Host journal write (rows 3 and
    /// 4), with its classified outcome [r5.5].
    #[error("store failed in turn {turn:?}: {kind:?}")]
    Store {
        /// Affected turn.
        turn: TurnNumber,
        /// The classified failure; [`StoreFailure::latches`] tells Core to latch.
        kind: StoreFailure,
    },
    /// The turn's stop order was honoured (design §2): before launch nothing
    /// started; after it, the group was force-closed at `force_at` under
    /// `close_by` and stdout was drained.
    #[error("turn stopped in turn {turn:?}")]
    Stopped {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// Core's absolute turn deadline elapsed before terminal evidence and exit.
    #[error("turn deadline elapsed in turn {turn:?}")]
    Deadline {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// The caller's force stop ended the turn; its private group was
    /// force-closed and stdout drained when launched.
    #[error("turn force-stopped in turn {turn:?}")]
    ForceStopped {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// Host confirmed the persistent connection's server died before any
    /// terminal (C1 §7.6 `server_lost`).
    #[error("server lost in turn {turn:?}")]
    ServerLost {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// The handshake lacked a feature VIA relies on; the start was not
    /// written (AD7 `handshake_refused`).
    #[error("handshake refused in turn {turn:?}")]
    HandshakeRefused {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// The catalog the instance reported at its handshake lacks the turn's
    /// value; the start was not written (AD18 `invalid_params`).
    #[error("instance catalog lacks the turn's {field} in turn {turn:?}")]
    InvalidParam {
        /// Affected turn.
        turn: TurnNumber,
        /// The C1 parameter.
        field: &'static str,
    },
    /// An identity the vendor reported before the turn's terminal differs
    /// from the session's (C2 §2 Reopen `resume_mismatch`); nothing was
    /// replaced or resent.
    #[error("resume mismatch in turn {turn:?}")]
    ResumeMismatch {
        /// Affected turn.
        turn: TurnNumber,
        /// The vendor session ID VIA continues.
        requested: String,
        /// The one the vendor returned.
        returned: String,
    },
}

/// The classified Store failure behind [`RouteError::Store`] (design §7.1,
/// §7.2 rows 3, 4 and 6 [r5.5]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreFailure {
    /// The turn's evidence folder or `stderr.log` could not be created
    /// before launch: nothing was committed (Task 4 design §7.2).
    Evidence,
    /// A Host journal write was not committed (rows 3 and 4).
    NotCommitted,
    /// The SQLite writer's queue was full: never enqueued, not committed.
    NotEnqueued,
    /// The SQLite writer is gone: uncertain.
    WriterLost,
    /// The write's outcome is unknown.
    Uncertain,
}

impl StoreFailure {
    /// Whether the outcome is uncertain, so the daemon latches (design §7.4).
    pub fn latches(self) -> bool {
        matches!(self, Self::WriterLost | Self::Uncertain)
    }
}

/// Why a turn is asked to stop (design §2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopCause {
    /// A caller `cancel`.
    Cancel,
    /// A session `close`.
    Close,
    /// Core's idle deadline.
    IdleDeadline,
    /// The turn's own Store write failed (design §7.2 row 5).
    Store,
    /// Core refused the vendor's evidence as unrepresentable (a token
    /// count past `i64::MAX`): the turn fails `protocol`.
    Protocol,
}

/// A stop order for one submitted turn (design §2). Core owns the cause and
/// times; Route acts only on `force_at` and `close_by`.
#[derive(Clone, Debug)]
pub struct StopOrder {
    /// Why the turn stops.
    pub cause: StopCause,
    /// Wall time of the request.
    pub requested_at: String,
    /// When Route force-closes a turn with no terminal.
    pub force_at: Deadline,
    /// Absolute bound on the force close and drain.
    pub close_by: Deadline,
}

/// The turn's stop-order watch: `None` until Core orders a stop.
pub type StopWatch = tokio::sync::watch::Receiver<Option<StopOrder>>;

/// Whether an order is already set at the sources a relayed [`StopWatch`]
/// merges, read where the relay may not have caught up: Route's entry
/// check and the pre-ARM launch gate (design §2 rule 1).
pub type StopSources = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// A failed route turn: the first typed cause plus the evidence Route still holds
/// after its forced cleanup and bounded drain, and the two facts Core's stop
/// outcome needs (AD4 Core handoff).
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{cause}{}", undecoded_note(.undecoded.as_deref()))]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a distinct, independent fact of the evidence"
)]
pub struct RouteFailure {
    /// First cause; later cleanup failures never replace it.
    pub cause: RouteError,
    /// Where the message VIA could not decode was kept, or why not (Task 4
    /// design §7.3); part of the failure's message.
    pub undecoded: Option<String>,
    /// Host-confirmed vendor exit when one was observed.
    pub exit: Option<ExitReport>,
    /// A vendor may have launched: Host sent ARM for this turn.
    pub launched: bool,
    /// Cleanup certainty of Route's forced group close, when it ran one.
    pub cleanup: Option<WireCleanup>,
    /// Host stopped the group while its vendor was live (Host force
    /// evidence). On the persistent profile only the server's death or the
    /// daemon force says so: a shared server is never killed for a turn
    /// (C2 §4.1).
    pub forced: bool,
    /// A Host journal write in the turn's cleanup had an uncertain outcome:
    /// the daemon must latch (design §7.2 row 12).
    pub journal_uncertain: bool,
    /// The vendor acknowledged Route's written interrupt with an
    /// interrupted terminal within the cutoff.
    pub acknowledged: bool,
    /// The connection is a persistent server (the persistent profile).
    pub shared: bool,
}

/// `; <note>` when an undecoded message was kept, else nothing.
fn undecoded_note(note: Option<&str>) -> String {
    note.map(|note| format!("; {note}")).unwrap_or_default()
}

pub mod claude;
pub mod codex;
mod fake;
mod runtime;
pub mod steer;

pub use fake::{
    FakeClassHint, FakeCost, FakeDenialKind, FakeMessage, FakeRoute, FakeRouteResult, FakeTerminal,
    FakeTurn, FakeUsage, Handshake, Lane, RouteMessage, TerminalDetails, TerminalStatus, TurnStart,
    VENDOR_DATA_MAX,
};
pub use runtime::{Retirement, RouteRuntime};
pub use steer::{
    CONTROL_BYTES, CONTROL_COMMANDS, SteerAnswer, SteerRefused, SteerRequest, SteerSender,
    steer_lane,
};
pub use via_wire::StoreError;
/// Test builds only: the failpoint controller, for the layers above.
#[cfg(feature = "test-failpoints")]
pub use via_wire::failpoint;
pub use via_wire::{
    CapacityToken, EnvAllowList, PrivateProcessSpec, ProcessOwner, ReprobeReport, RuntimeConfig,
    RuntimeResources, SessionId, WireCleanup, WireError, WireRecovery, WireShutdown,
    WireTurnRecovery,
};

/// Internal hidden-anchor entrypoint forwarded through this architecture layer.
pub fn run_anchor_from_args(args: &[std::ffi::OsString]) -> i32 {
    via_wire::run_anchor_from_args(args)
}
