//! Adapters map canonical operations to vendor semantics and normalize events;
//! they never split bytes into messages, own processes, or decide admission.

use std::num::NonZeroU64;

pub use via_routes::{
    Deadline, MAX_OBSERVATION_BYTES, ReprobeReport, RouteError, RouteFailure, StopCause, StopOrder,
    StopWatch, StoreFailure, ToolStatus, TurnNumber,
};

/// Correlates a start reply with its acceptance observation within one turn.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AcceptanceToken(NonZeroU64);

impl TryFrom<u64> for AcceptanceToken {
    type Error = &'static str;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or("acceptance token must be nonzero")
    }
}

impl AcceptanceToken {
    /// Returns the request correlation number.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Vendor-scoped turn identifier, never used without the owning session.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VendorTurnId(String);

impl TryFrom<String> for VendorTurnId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err("vendor turn id cannot be empty");
        }
        Ok(Self(value))
    }
}

impl VendorTurnId {
    /// Returns the vendor's opaque identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Definite vendor rejection before acceptance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartRejected {
    /// The route cannot enforce the requested bound.
    BoundUnsupported(String),
    /// Vendor returned a definite error code and description.
    VendorError(String, String),
    /// The vendor session no longer exists.
    SessionGone,
    /// A known vendor response was malformed or contradictory.
    Protocol(String),
}

/// Evidence returned by C2's start operation.
#[derive(Clone, Debug)]
pub enum StartOutcome {
    /// Vendor evidence confirms acceptance of this one submission.
    Accepted {
        /// Same token used in the matching observation.
        correlation: AcceptanceToken,
        /// Vendor turn identifier when the route provides one.
        vendor_turn_id: Option<VendorTurnId>,
        /// Monotonic acceptance time; wall time is recorded separately.
        accepted_at: tokio::time::Instant,
    },
    /// Vendor definitively refused the submitted turn.
    Rejected(StartRejected),
    /// Input may have reached the vendor; never resend automatically.
    Unknown {
        /// Bounded diagnostic without prompt or handle.
        reason: String,
    },
}

/// Whether the turn's side effects are known to have stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cleanup {
    /// Positive group absence or every tool item finished.
    Quiescent,
    /// No further evidence can currently prove completion.
    Uncertain,
    /// A known tool item or cleanup operation remains open.
    Pending,
}

/// Evidence for a requested cancellation, separate from cleanup certainty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelOutcome {
    /// Sent, with no vendor acknowledgement yet.
    Requested,
    /// Vendor supplied terminal cancellation evidence.
    Acknowledged,
    /// Host positively reported its own-group force request.
    Forced,
    /// Cancellation outcome cannot be established.
    Unknown,
}

/// C2 interruption report that Core may use to select a disposition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterruptReport {
    /// Protocol/Host evidence about the cancellation request.
    pub outcome: CancelOutcome,
    /// Separate side-effect cleanup certainty.
    pub cleanup: Cleanup,
}

/// First failure retained by the independent driver-health lane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverFailure {
    /// Typed route protocol, transport, process or Store cause.
    Route(RouteError),
    /// Normalized observations could not be drained within the bound.
    ObservationOverflow,
}

/// Driver health remains observable even when data observations are full.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverHealth {
    /// Driver accepts commands.
    Open,
    /// First cause is latched.
    Failed {
        /// First health failure.
        first_cause: DriverFailure,
    },
    /// Driver and its owned tasks have exited.
    Closed,
}

mod fake_config;
mod runtime;

pub use fake_config::FakeConfig;
pub use runtime::{
    AdapterError, AdapterRuntime, AdapterRuntimeConfig, AdmittedObservation, FakeRecovery,
    FakeShutdown, FakeTurnRecovery, OBSERVATION_BYTES, OBSERVATION_ITEMS, ObservationSink,
    observation_channel,
};
pub use via_routes::{
    CapacityToken, EnvAllowList, PrivateProcessSpec, ProcessOwner, RuntimeConfig, RuntimeResources,
    SessionId, WireCleanup,
};

/// Fake terminal status as vendor evidence; Core chooses the C1 disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorTerminalStatus {
    /// Fake reported normal completion.
    Completed,
    /// Fake reported interruption.
    Interrupted,
    /// Fake reported failure.
    Failed,
}

/// A paired fake acceptance observation.
pub struct FakeAcceptanceObservation {
    /// Correlation ID shared with start outcome.
    pub correlation: AcceptanceToken,
    /// Vendor-scoped turn ID.
    pub vendor_turn_id: VendorTurnId,
}

/// One normalized C2 observation Core commits as a C1 §6.1 event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Observation {
    /// Incremental assistant text, at most one C2 payload bound per piece.
    AssistantText {
        /// Text in decode order.
        text: String,
    },
    /// A tool started inside the turn.
    ToolStarted {
        /// Vendor tool identifier.
        tool_id: String,
        /// Tool name.
        name: String,
        /// Bounded input summary.
        input_summary: String,
    },
    /// A tool ended inside the turn.
    ToolEnded {
        /// Vendor tool identifier.
        tool_id: String,
        /// Vendor completion status.
        status: ToolStatus,
        /// Bounded output summary.
        output_summary: String,
        /// Exit code when present.
        exit_code: Option<i32>,
    },
    /// Unknown vendor notification with its bounded payload prefix.
    VendorOther {
        /// Original vendor type tag.
        vendor_type: String,
        /// Encoded message prefix of at most 16 KiB.
        payload: String,
        /// Explicit marker that the prefix omitted bytes.
        truncated: bool,
    },
}

/// Adapter output to Core, in the order the driver decoded it (C2 §4).
pub enum FakeObservation {
    /// The paired acceptance, always before any other observation of the turn.
    Accepted(FakeAcceptanceObservation),
    /// A data observation.
    Data {
        /// Normalized payload.
        observation: Observation,
    },
}

/// Final fake evidence after the vendor exited; no Core state is chosen here.
pub struct FakeTerminalEvidence {
    /// Vendor terminal status.
    pub status: VendorTerminalStatus,
    /// Authoritative final text.
    pub final_text: String,
    /// Raw vendor stop reason.
    pub stop_reason: String,
    /// Optional vendor failure code.
    pub vendor_code: Option<String>,
    /// Independently confirmed vendor exit.
    pub exit: via_routes::ExitReport,
    /// Private-group cleanup certainty after vendor exit.
    pub cleanup: Cleanup,
    /// A Host journal write in the turn's cleanup had an uncertain outcome:
    /// the daemon must latch (design §7.2 row 12).
    pub journal_uncertain: bool,
}

/// Internal hidden-anchor entrypoint forwarded through this architecture layer.
pub fn run_anchor_from_args(args: &[std::ffi::OsString]) -> i32 {
    via_routes::run_anchor_from_args(args)
}
