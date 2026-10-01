//! Adapters map canonical operations to vendor semantics and normalize events;
//! they never split bytes into messages, own processes, or decide admission.

use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub use via_routes::{
    Deadline, MAX_OBSERVATION_BYTES, ReprobeReport, RouteError, RouteFailure, StopCause, StopOrder,
    StopWatch, StoreFailure, TurnNumber,
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
    /// The token of a connection's one start request.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

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
    /// A per-turn parameter the route refuses (AD18).
    InvalidParam {
        /// The C1 parameter.
        field: &'static str,
    },
    /// Vendor returned a definite error code and description.
    VendorError(String, String),
    /// The vendor session no longer exists.
    SessionGone,
    /// A known vendor response was malformed or contradictory.
    Protocol(String),
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
    /// A task the driver owns ended without its result.
    OwnedTask,
    /// The `run_turn` future was dropped before its result.
    TurnAbandoned,
    /// Host confirmed the persistent connection's server died.
    ServerLost,
    /// The vendor returned another session than the one VIA continues.
    ResumeMismatch,
    /// A persistent connection's helper process was retired without proof
    /// that its group is gone, or with an uncertain Host journal write.
    RetirementUncertain,
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

mod capabilities;
mod config;
mod driver;
mod fake;
mod harness;
mod instance;
pub mod observation;
mod plan;
mod runtime;
mod set;

pub use capabilities::{
    BoundMode, Capabilities, ParamSupport, Support, UsageSupport, Verb, VerbReq, Verbs,
};
pub use config::{
    AdapterConfig, BOOTSTRAP_ENV, BootstrapEnv, ConfigError, FakeFixture, HarnessConfig,
    HarnessesError, HarnessesRule,
};
pub use driver::{
    CloseMode, CloseReport, ConnectionPin, ForceWatch, Prepared, Recovery, SessionCx,
    SessionDriver, SessionSpec, SteerError, SteerInput, TurnCx, TurnSpec,
};
pub use harness::{FAKE, HARNESSES, Harness, HarnessRow, harness_names};
pub use instance::{
    BinaryIdentity, Incompatibility, InstanceCache, REFUSAL_TTL, VERSIONS_KEPT, resolve_binary,
};
pub use observation::{
    AdapterError, Admitted, ClassHint, CostReport, Decline, Denial, DenialKind, InstanceReport,
    LeftoverReport, Observation, ObservationBudget, ObservationItem, ObservationSink,
    ProgressMarks, SteerDelivery, StopReason, TurnEnd, TurnEvidence, UsageSample, VendorTerminal,
    observation_channel, observation_channel_in,
};
pub use plan::{
    AdapterSet, Bound, CatalogModel, Category, CategoryDecl, DescribeRequest, Inherit,
    InheritState, ModelChoice, ModelEntry, ModelSource, Refusal, RefusalKind, RoutePlan, ServerKey,
    SessionRef, Switch, TurnParams, VendorOptions, VersionStatus, Warning, resolve_model,
};
pub use runtime::{
    AdapterShutdown, AnchorRecovery, AnchorTurnRecovery, OBSERVATION_BYTES, OBSERVATION_ITEMS,
};
pub use tokio_util::{sync::CancellationToken, task::TaskTracker};
pub use via_routes::{
    AnchorCohort, CapacityToken, EnvAllowList, ExitReport, PrivateProcessSpec, ProcessOwner,
    RuntimeConfig, RuntimeResources, SessionId, WireCleanup,
};

/// A vendor terminal's status as evidence; Core chooses the C1 disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorTerminalStatus {
    /// The vendor reported normal completion.
    Completed,
    /// The vendor reported interruption.
    Interrupted,
    /// The vendor reported failure.
    Failed,
}

/// `{"type":"final_text","text":""}`: an observation's bytes besides its
/// text's escaped characters.
const FINAL_TEXT_OVERHEAD: usize = 31;

/// A character's bytes in a JSON string, as `serde_json` escapes it.
fn escaped_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        '\0'..='\u{1f}' => 6,
        _ => character.len_utf8(),
    }
}

/// The bytes `text` encodes to inside a JSON string, quotes excluded, as
/// `serde_json` escapes it (design §2.3, §6.4).
pub fn encoded_text_len(text: &str) -> usize {
    text.chars().map(escaped_len).sum()
}

/// Cuts a completed final text into `final_text` pieces (design §2.3), each
/// cut at the last character whose escaped encoding keeps the whole
/// observation within [`MAX_OBSERVATION_BYTES`]; an empty text has none.
pub fn final_text_pieces(text: &str) -> impl Iterator<Item = &str> {
    let room = MAX_OBSERVATION_BYTES - FINAL_TEXT_OVERHEAD;
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut used = 0;
        let mut end = rest.len();
        for (at, character) in rest.char_indices() {
            used += escaped_len(character);
            if used > room {
                end = at;
                break;
            }
        }
        let (piece, tail) = rest.split_at(end);
        rest = tail;
        Some(piece)
    })
}

/// The turn's activity clock (design §2.4): the arrival of the last vendor
/// message attributed to the turn, unknown types included, as milliseconds
/// since the clock's base instant. The Adapter stores; Core reads.
#[derive(Clone, Debug)]
pub struct TurnActivity {
    base: tokio::time::Instant,
    last_ms: Arc<AtomicU64>,
}

impl TurnActivity {
    /// A clock whose base, and first reading, is `base`.
    pub fn new(base: tokio::time::Instant) -> Self {
        Self {
            base,
            last_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Records a message arrival at `at`.
    pub fn record(&self, at: tokio::time::Instant) {
        let millis =
            u64::try_from(at.saturating_duration_since(self.base).as_millis()).unwrap_or(u64::MAX);
        self.last_ms.fetch_max(millis, Ordering::Relaxed);
    }

    /// Milliseconds from the base to the last recorded arrival.
    pub fn last_ms(&self) -> u64 {
        self.last_ms.load(Ordering::Relaxed)
    }
}

/// Internal hidden-anchor entrypoint forwarded through this architecture layer.
pub fn run_anchor_from_args(args: &[std::ffi::OsString]) -> i32 {
    via_routes::run_anchor_from_args(args)
}
