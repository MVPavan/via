//! Core owns sessions, turns, admission, queues, and result envelopes; it
//! never maps vendor flags, parses vendor events, or starts processes.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use via_adapters::FakeConfig;
pub use via_adapters::{AcceptanceToken, CancelOutcome, Cleanup, StartOutcome};
pub use via_store::{CommitOutcome, Deadline, SessionId, StoreLock, TurnNumber};

/// Strict parameters for C1's mandatory first `hello` request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    /// The only supported API version is one.
    pub api_version: u32,
    /// The exact client binary version used for daemon compatibility checks.
    pub client_version: String,
    /// Client identity for diagnostics.
    pub client: String,
}

impl HelloParams {
    /// Validates the protocol version independently of JSON shape.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.api_version != 1 {
            return Err("api_version must be 1");
        }
        if self.client_version.is_empty() {
            return Err("client_version cannot be empty");
        }
        if self.client.is_empty() {
            return Err("client cannot be empty");
        }
        Ok(())
    }
}

/// Core's closed lifecycle state for one turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnState {
    /// Accepted, not yet submitted.
    Queued,
    /// Submission has begun and no terminal result is committed.
    Running,
    /// Terminal success.
    Completed,
    /// Terminal failure.
    Failed,
    /// Terminal cancellation.
    Cancelled,
    /// Submission or outcome cannot be established after a crash or loss.
    Unknown,
}

impl TurnState {
    /// Returns the exact state word for a C1 response.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

/// A C1 response state; future wire values remain readable without entering Core state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum C1TurnState {
    /// A lifecycle state Core understands.
    Known(TurnState),
    /// An additive future response state retained verbatim by clients.
    Other(String),
}

mod api;
mod engine;

/// Test builds only: the envelope with every member at its maximum (Task 4
/// design §6.4, §13.2).
#[cfg(feature = "test-failpoints")]
pub use engine::envelope_at_maximum;
pub use engine::{
    Connections, DaemonCounts, Engine, EngineShutdown, FailureBatches, FinalEntry, Handoff,
    Receipted, StopMode,
};

/// Test builds only: the named failpoint controller (runtime-contracts §11),
/// for the daemon's own seams in `via-cli` (design §10).
#[cfg(feature = "test-failpoints")]
pub use via_store::failpoint;
/// Design §10.2: the C1 reader scans each line before decoding it.
pub use via_store::json_limits;

/// Hidden executable entrypoint forwarded through the architecture layers.
pub fn run_anchor_from_args(args: &[std::ffi::OsString]) -> i32 {
    via_adapters::run_anchor_from_args(args)
}

pub use api::{
    ApiError, CancelParams, CloseMode, CloseParams, DEFAULT_CLOSE_DEADLINE_MS,
    DEFAULT_FORCE_AFTER_MS, DEFAULT_WAIT_MS, DaemonStatusParams, DaemonStopParams, EventsParams,
    ListParams, LogsParams, Named, REQUEST_LINE_MAX, ReadParams, ReceiptOutcome, ResumeParams,
    SpawnParams, StatusParams, SteerParams, Unpersisted, WaitParams, hash_handle, parse_address,
    retry_identity,
};

impl Serialize for C1TurnState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Known(state) => serializer.serialize_str(state.as_str()),
            Self::Other(raw) => serializer.serialize_str(raw),
        }
    }
}

impl<'de> Deserialize<'de> for C1TurnState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "queued" => Self::Known(TurnState::Queued),
            "running" => Self::Known(TurnState::Running),
            "completed" => Self::Known(TurnState::Completed),
            "failed" => Self::Known(TurnState::Failed),
            "cancelled" => Self::Known(TurnState::Cancelled),
            "unknown" => Self::Known(TurnState::Unknown),
            _ => Self::Other(value),
        })
    }
}
