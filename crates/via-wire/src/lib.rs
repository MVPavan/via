//! Wire owns framing, byte transport, and the raw log tap; it never interprets
//! protocol messages or manages processes.

pub use via_host::{
    CapacityToken, CleanupEvidence, CloseMode, CloseRequest, EnvAllowList, ExitReport, HostError,
    PrivateProcessSpec, ProcessOwner, TurnNumber,
};
pub use via_store::{AnchorCohort, ConnectionId, Deadline, RawRef, RuntimeResources, SessionId};

/// The maximum complete stdout frame, including its trailing LF.
pub const MAX_STDOUT_FRAME_BYTES: usize = 1024 * 1024;

/// A complete, newline-terminated vendor frame with bounded bytes.
#[derive(Eq, PartialEq)]
pub struct BoundedBytes(Vec<u8>);

impl BoundedBytes {
    /// Checks the complete-frame cap before retaining the input.
    pub fn try_from_frame(bytes: Vec<u8>) -> Result<Self, WireFailure> {
        if bytes.len() > MAX_STDOUT_FRAME_BYTES {
            return Err(WireFailure::FrameTooLarge);
        }
        if bytes.last() != Some(&b'\n') {
            return Err(WireFailure::UnterminatedFrame);
        }
        Ok(Self(bytes))
    }

    /// Returns the exact bytes, including LF.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A complete frame and its byte-accurate raw evidence reference.
/// The checked constructor is the only cross-crate construction path.
///
/// ```compile_fail
/// use via_wire::{BoundedBytes, ConnectionId, Frame, RawRef};
/// let bytes = BoundedBytes::try_from_frame(b"ok\n".to_vec()).unwrap();
/// let raw_ref = RawRef::new(ConnectionId::try_from("c_01").unwrap(), 0, 2).unwrap();
/// let _invalid = Frame { bytes, raw_ref };
/// ```
#[derive(Eq, PartialEq)]
pub struct Frame {
    /// Exact bytes delivered to Route.
    bytes: BoundedBytes,
    /// Reference to the corresponding raw stdout unit.
    raw_ref: RawRef,
}

impl Frame {
    /// Validates that the raw reference covers this exact frame length.
    pub fn new(bytes: BoundedBytes, raw_ref: RawRef) -> Result<Self, WireFailure> {
        if usize::try_from(raw_ref.byte_len()) != Ok(bytes.as_bytes().len()) {
            return Err(WireFailure::RawRangeMismatch);
        }
        Ok(Self { bytes, raw_ref })
    }

    /// Returns the exact frame bytes, including the trailing LF.
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }

    /// Returns the checked reference to these exact bytes.
    pub fn raw_ref(&self) -> &RawRef {
        &self.raw_ref
    }
}

/// Evidence about an outbound frame write, not permission to resend a turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendOutcome {
    /// The complete frame was written.
    Written,
    /// No bytes were written.
    NotWritten,
    /// Some bytes may have reached the peer.
    Indeterminate,
}

/// First transport failure retained independently of frame backpressure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireFailure {
    /// A stdout line crossed the hard frame cap.
    FrameTooLarge,
    /// EOF left a partial line that cannot be a protocol message.
    UnterminatedFrame,
    /// Frame bytes and raw reference did not agree.
    RawRangeMismatch,
    /// Raw evidence append or sync failed.
    RawStore,
    /// Staging or framed-data budget was exhausted.
    Overflow,
    /// The pipe or socket transport failed.
    Transport,
}

/// Independently observable connection health.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireHealth {
    /// Transport is currently open.
    Open,
    /// First failure and whether the raw log lost bytes.
    Failed {
        /// Latched first cause.
        cause: WireFailure,
        /// True when exact traffic could not be durably retained.
        raw_incomplete: bool,
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
    RawEvidence, RuntimeConfig, WireCloseReport, WireConnection, WireError, WireRecovery,
    WireRuntime, WireShutdown, WireSignals, WireTurnRecovery,
};
pub use via_host::{JournalSite, ReprobeReport};
pub use via_store::StoreError;

/// Internal hidden-anchor entrypoint, forwarded without a public process-control handle.
pub fn run_anchor_from_args(args: &[std::ffi::OsString]) -> i32 {
    via_host::run_anchor_from_args(args)
}
