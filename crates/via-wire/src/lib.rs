//! Wire owns message splitting, byte transport and the turn's evidence
//! folder; it never interprets protocol messages or manages processes.

pub use via_host::{
    CapacityToken, CleanupEvidence, CloseMode, CloseRequest, EnvAllowList, ExitReport, HostError,
    PrivateProcessSpec, ProcessOwner, TurnNumber,
};
pub use via_store::{AnchorCohort, Deadline, RuntimeResources, SessionId};

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
#[derive(Eq, PartialEq)]
pub struct VendorMessage {
    /// Exact bytes delivered to Route.
    bytes: BoundedBytes,
}

impl VendorMessage {
    /// Wraps one complete bounded message.
    pub fn new(bytes: BoundedBytes) -> Self {
        Self { bytes }
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

/// Independently observable connection health.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireHealth {
    /// Transport is currently open.
    Open,
    /// First failure.
    Failed {
        /// Latched first cause.
        cause: WireFailure,
    },
    /// Host confirmed exit of the owning process.
    Exited(ExitReport),
    /// All connection tasks have closed.
    Closed,
}

/// Passive cleanup certainty passed upward without Host signal authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireCleanup {
    /// Host proved the private group absent under its recorded identity.
    Quiescent,
    /// Positive absence could not be established.
    Uncertain,
}

mod runtime;

// Route needs the narrow connection handle returned by WireRuntime; its constructor
// and Host process control remain private to Wire.
pub use runtime::{
    RuntimeConfig, UNDECODED_BYTES, WireCloseReport, WireConnection, WireError, WireRecovery,
    WireRuntime, WireShutdown, WireSignals, WireTurnRecovery,
};
pub use via_host::{JournalSite, ReprobeReport};
pub use via_store::StoreError;
pub use via_store::json_limits;

/// Internal hidden-anchor entrypoint, forwarded without a public process-control handle.
pub fn run_anchor_from_args(args: &[std::ffi::OsString]) -> i32 {
    via_host::run_anchor_from_args(args)
}
