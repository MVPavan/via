//! Wire owns message splitting, byte transport and the turn's evidence
//! folder; it never interprets protocol messages or manages processes.

pub use via_host::{
    CapacityToken, CleanupEvidence, CloseMode, CloseRequest, EnvAllowList, ExitReport, HostError,
    PrivateProcessSpec, ProcessOwner, TurnNumber,
};
pub use via_store::{
    AnchorCohort, CommitOutcome, Deadline, RuntimeResources, ServerId, SessionId, StoreFailureKind,
};

/// The maximum complete stdout message, including its trailing LF.
pub const MAX_STDOUT_MESSAGE_BYTES: usize = 1024 * 1024;

/// The bytes of one complete, newline-terminated vendor message, bounded but
/// not yet decoded; Route may still reject them as malformed.
#[derive(Eq, PartialEq)]
pub struct BoundedBytes(Vec<u8>);

impl BoundedBytes {
    /// Checks the complete-message cap before retaining the input.
    pub fn try_from_message(bytes: Vec<u8>) -> Result<Self, WireFailure> {
        if bytes.len() > MAX_STDOUT_MESSAGE_BYTES {
            return Err(WireFailure::MessageTooLarge);
        }
        if bytes.last() != Some(&b'\n') {
            return Err(WireFailure::UnterminatedMessage);
        }
        Ok(Self(bytes))
    }

    /// Returns the exact bytes, including LF.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A complete, undecoded vendor message. The bounded constructor is the only
/// cross-crate construction path.
///
/// ```compile_fail
/// use via_wire::{BoundedBytes, VendorMessage};
/// let bytes = BoundedBytes::try_from_message(b"ok\n".to_vec()).unwrap();
/// let _invalid = VendorMessage { bytes };
/// ```
pub struct VendorMessage {
    /// Exact bytes delivered to Route.
    bytes: BoundedBytes,
    /// Its share of the connection's staging, held until the message is
    /// dropped (x.3.2 X0 item 12.5); none for a message not read by Wire.
    _permit: Option<connection::StagingPermit>,
}

impl PartialEq for VendorMessage {
    /// Equal bytes; the staging share is not part of the message.
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for VendorMessage {}

impl VendorMessage {
    /// Wraps one complete bounded message.
    pub fn new(bytes: BoundedBytes) -> Self {
        Self {
            bytes,
            _permit: None,
        }
    }

    /// A message Wire read, holding its staging share.
    pub(crate) fn staged(bytes: BoundedBytes, permit: connection::StagingPermit) -> Self {
        Self {
            bytes,
            _permit: Some(permit),
        }
    }

    /// Returns the exact message bytes, including the trailing LF.
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }
}

/// Evidence about an input message write, not permission to resend a turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendOutcome {
    /// The complete input message was written.
    Written,
    /// No bytes were written.
    NotWritten,
    /// Some bytes may have reached the peer.
    Indeterminate,
}

/// First transport failure retained independently of message backpressure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireFailure {
    /// A stdout line crossed the hard message cap.
    MessageTooLarge,
    /// EOF left a partial line that cannot be a protocol message.
    UnterminatedMessage,
    /// Staging or message staging budget was exhausted.
    Overflow,
    /// The pipe or socket transport failed.
    Transport,
}

/// Passive cleanup certainty passed upward without Host signal authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireCleanup {
    /// Host proved the private group absent under its recorded identity.
    Quiescent,
    /// Positive absence could not be established.
    Uncertain,
}

mod connection;
mod runtime;
mod split;

// Route needs the narrow connection handles returned by WireRuntime; their
// constructor and Host process control remain private to Wire.
#[cfg(feature = "test-failpoints")]
pub use connection::fallback_drops;
#[cfg(any(feature = "test-failpoints", feature = "test-support"))]
pub use connection::testing;
pub use connection::{
    Admitted, DataHold, FailureCause, LatchState, OutboundMessage, PendingWrite, TurnFolder,
    UNDECODED_BYTES, WireConnection, WireMessages, WireParts, WireSender, WriteBounds, WriteState,
    WriteTicket,
};
pub use runtime::{
    RuntimeConfig, WireCloseReport, WireError, WireRecovery, WireRuntime, WireShutdown,
    WireSignals, WireTurnRecovery,
};
pub use split::{LineSplitter, Pushed};
pub use via_host::{JournalSite, ReprobeReport};
pub use via_store::StoreError;
/// Test builds only: the named failpoint controller, for the layers above.
#[cfg(feature = "test-failpoints")]
pub use via_store::failpoint;
pub use via_store::json_limits;

/// Internal hidden-anchor entrypoint, forwarded without a public process-control handle.
pub fn run_anchor_from_args(args: &[std::ffi::OsString]) -> i32 {
    via_host::run_anchor_from_args(args)
}
