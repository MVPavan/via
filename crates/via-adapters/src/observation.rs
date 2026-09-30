//! C2 §4 observation and turn-end types (adapter design §3.2, AD4, AD6,
//! AD7, AD20). Types only: the driver that produces them comes with the
//! fake driver lane. Kept in this module, not re-exported at the crate
//! root, because the legacy `Observation` still lives there.

use std::path::PathBuf;

use serde_json::value::RawValue;
use tokio::time::Instant;

use crate::plan::{VersionStatus, Warning};
use crate::{AcceptanceToken, AdapterError, Cleanup, VendorTerminalStatus, VendorTurnId};

/// A vendor message's progress marks (C2 §4 `progress`); the arrival time
/// is the item's `at`. Replaces the legacy root `ProgressMarks` once Core
/// moves to this surface.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProgressMarks {
    /// Model output: text, reasoning or a tool request.
    pub model: bool,
    /// Tools started, as `(id, name)`.
    pub tools_started: Vec<(String, String)>,
    /// Ids of tools ended.
    pub tools_ended: Vec<String>,
    /// A per-model-call usage sample, never a cumulative total (AD6).
    pub usage: Option<UsageSample>,
}

/// One observation, stamped when the driver decoded it (C2 §4).
#[derive(Debug)]
pub struct ObservationItem {
    /// When the driver decoded it; Core records wall time.
    pub at: Instant,
    /// The vendor turn it belongs to, which Core maps to a turn number.
    pub vendor_turn: Option<VendorTurnId>,
    /// The payload.
    pub observation: Observation,
}

/// A C2 §4 observation.
#[derive(Debug)]
pub enum Observation {
    /// `turn.accepted`.
    Accepted(Acceptance),
    /// `session.vendor_identity_confirmed`.
    IdentityConfirmed(Identity),
    /// A vendor message's progress marks.
    Progress(ProgressMarks),
    /// One completed `final_text` piece.
    FinalText(String),
    /// `action.denied`.
    ActionDenied(Denial),
    /// `vendor.request_declined`.
    RequestDeclined(Decline),
    /// `steer.delivered`.
    SteerDelivered(SteerDelivery),
    /// `warning`.
    Warning(Warning),
    /// `session.vendor_closed`, with its reason.
    VendorClosed(String),
    /// `resume.mismatch`.
    ResumeMismatch {
        /// The vendor session ID VIA asked to continue.
        requested: String,
        /// The one the vendor returned.
        returned: String,
    },
    /// `turn.late_terminal`: only for a turn whose end carried no terminal.
    LateTerminal(VendorTerminal),
}

/// Acceptance of one submission, on vendor evidence.
#[derive(Debug)]
pub struct Acceptance {
    /// The token shared with the start result.
    pub correlation: AcceptanceToken,
    /// The vendor's turn ID, when the route has one.
    pub vendor_turn_id: Option<VendorTurnId>,
}

/// A confirmed vendor identity for the current connection generation.
#[derive(Debug)]
pub struct Identity {
    /// The confirmed vendor session ID.
    pub vendor_session_id: String,
    /// The connection generation that confirmed it.
    pub connection_id: String,
    /// The vendor transcript, when known.
    pub transcript: Option<PathBuf>,
}

/// An action class the vendor's own bound denied (C1 §5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DenialKind {
    /// A file write.
    FileWrite,
    /// A command.
    Command,
    /// Network access.
    Network,
    /// Anything else.
    Other,
}

/// One denied action.
#[derive(Debug)]
pub struct Denial {
    /// The action class.
    pub kind: DenialKind,
    /// What was denied, bounded.
    pub target: String,
    /// Why, bounded.
    pub reason: String,
}

/// One vendor request VIA declined.
#[derive(Debug)]
pub struct Decline {
    /// The vendor method.
    pub vendor_method: String,
    /// A bounded summary.
    pub summary: String,
    /// Whether the vendor was blocked on it.
    pub blocking: bool,
}

/// How steer input reached the vendor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteerDelivery {
    /// Injected into the active turn.
    Injected,
    /// Delivered with the named partial semantics.
    Partial(&'static str),
}

/// A normalized stop reason (AD5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    /// The model ended its turn.
    EndTurn,
    /// The step limit was reached.
    MaxSteps,
    /// A budget was exhausted.
    Budget,
    /// The model refused.
    Refusal,
    /// The turn was interrupted.
    Interrupted,
    /// The vendor reported an error.
    Error,
    /// Any other vendor reason, kept in `vendor_stop_reason`.
    Other,
}

/// A vendor's suggestion for the failure class; Core decides.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClassHint {
    /// Authentication failed.
    Auth,
    /// Rate limited.
    RateLimit,
    /// The context window was exceeded.
    ContextExceeded,
    /// A budget was exceeded.
    BudgetExceeded,
    /// Another vendor error.
    VendorError,
    /// A protocol contradiction.
    Protocol,
    /// The vendor returned a different session.
    ResumeMismatch,
}

/// One usage sample (AD6): keyed samples supersede, keyless ones add.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageSample {
    /// The vendor's sample key, if any.
    pub key: Option<String>,
    /// Input tokens.
    pub input: Option<u64>,
    /// Cached input tokens.
    pub cached_input: Option<u64>,
    /// Output tokens.
    pub output: Option<u64>,
    /// Reasoning output tokens.
    pub reasoning_output: Option<u64>,
    /// Total tokens.
    pub total: Option<u64>,
}

/// A vendor-reported cost.
#[derive(Clone, Debug, PartialEq)]
pub struct CostReport {
    /// US dollars.
    pub usd: f64,
    /// The declared cost scope.
    pub scope: String,
}

/// The one retained vendor terminal of a turn (AD4).
#[derive(Debug)]
pub struct VendorTerminal {
    /// When it was decoded.
    pub at: Instant,
    /// The vendor's terminal status.
    pub status: VendorTerminalStatus,
    /// The normalized stop reason.
    pub stop_reason: StopReason,
    /// The vendor's own stop reason.
    pub vendor_stop_reason: String,
    /// The vendor's failure code.
    pub vendor_code: Option<String>,
    /// The suggested failure class.
    pub class_hint: Option<ClassHint>,
    /// A bounded failure detail.
    pub detail: Option<String>,
    /// Structured output, validated by Core.
    pub structured_output: Option<Box<RawValue>>,
    /// Steps the vendor counted.
    pub steps: Option<u64>,
    /// The turn aggregate, superseding call samples.
    pub usage: Option<UsageSample>,
    /// The vendor's cost.
    pub cost: Option<CostReport>,
    /// Bounded vendor data (16 KiB) for the envelope's `vendor` member.
    pub vendor: Option<Box<RawValue>>,
}

/// The version an instance reported at its own handshake (AD7);
/// `Tested` or `Untested` only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceReport {
    /// The handshake version.
    pub vendor_version: Option<String>,
    /// Whether it is in the adapter's `checked` set.
    pub version_status: VersionStatus,
}

/// Where a leftover report was taken (AD20).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeftoverScope {
    /// After a per-turn process.
    Turn,
    /// After a server stopped.
    Server,
}

/// One process left over by the coding agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeftoverProcess {
    /// Its pid.
    pub pid: u32,
    /// The kernel's name, lossy UTF-8.
    pub comm: String,
    /// RFC 3339 UTC, second precision.
    pub started_at: String,
}

/// Processes the agent left behind (AD20).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeftoverReport {
    /// Where it was taken.
    pub scope: LeftoverScope,
    /// At most 16, oldest first.
    pub processes: Vec<LeftoverProcess>,
    /// Matches found; a lower bound when `incomplete`.
    pub total: u32,
    /// Detection did not finish.
    pub incomplete: bool,
}

/// Process and cleanup facts of a turn that ran to its end.
#[derive(Debug)]
pub struct TurnEvidence {
    /// The confirmed process exit, when there is one.
    pub exit: Option<via_routes::ExitReport>,
    /// Cleanup certainty.
    pub cleanup: Cleanup,
    /// A Host journal write had an uncertain outcome.
    pub journal_uncertain: bool,
}

/// The one result of `run_turn` (C2 §4.1).
#[derive(Debug)]
pub struct TurnEnd {
    /// The decoded vendor terminal, retained even when earlier observations
    /// could not be delivered.
    pub terminal: Option<VendorTerminal>,
    /// Set once the handshake was read, on every outcome (AD7).
    pub instance: Option<InstanceReport>,
    /// Per-turn routes on every outcome, and server loss (AD20).
    pub leftovers: Option<LeftoverReport>,
    /// Process and cleanup facts, or a typed failure.
    pub outcome: Result<TurnEvidence, AdapterError>,
}
