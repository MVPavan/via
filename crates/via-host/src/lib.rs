//! Host supervises vendor processes and endpoints; it never speaks a vendor
//! protocol or decides session policy.

use std::{ffi::OsString, fmt, path::PathBuf};

mod anchor;
mod host;
mod linux;
mod protocol;

pub use anchor::run_anchor_from_args;
pub(crate) use host::monotonic_remaining;
pub use host::{
    AcquiredProcess, CloseReport, ExitReceiver, Host, HostError, LaunchPipes, OwnedPipes,
    ProcessControl, RecoveryReport, ShutdownReport, TurnRecovery,
};

pub use via_store::{Deadline, SessionId, TurnNumber};

/// The one turn that owns a private fake process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOwner {
    /// VIA session that admitted the turn.
    pub session_id: SessionId,
    /// One-based turn number.
    pub turn: TurnNumber,
}

/// Explicit child environment; Host never inherits the daemon environment.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct EnvAllowList {
    entries: Vec<(OsString, OsString)>,
}

impl fmt::Debug for EnvAllowList {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvAllowList")
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

impl EnvAllowList {
    /// Creates an explicit environment from validated name-value pairs.
    pub fn try_from_entries(entries: Vec<(OsString, OsString)>) -> Result<Self, &'static str> {
        for (name, value) in &entries {
            if name.is_empty() || name.to_string_lossy().contains('=') {
                return Err("environment name must be nonempty and contain no equals sign");
            }
            if name.to_string_lossy().contains('\0') || value.to_string_lossy().contains('\0') {
                return Err("environment entries cannot contain NUL");
            }
        }
        Ok(Self { entries })
    }

    /// Returns only variables that may be passed to the child.
    pub fn entries(&self) -> &[(OsString, OsString)] {
        &self.entries
    }
}

/// Type-erased capacity a caller hands Host with a launch (T2-D, runtime §8):
/// Host owns it for the process group's life and drops it only when no group
/// was created or when it has positively proved the group absent.
pub type CapacityToken = Box<dyn Send + 'static>;

/// Immutable launch parameters for a VIA-owned private process.
pub struct PrivateProcessSpec {
    /// Executable path, passed directly without a shell.
    pub program: PathBuf,
    /// Argument vector, passed without shell interpretation.
    pub args: Vec<OsString>,
    /// Explicit working directory.
    pub cwd: PathBuf,
    /// Explicit allow-listed environment.
    pub env: EnvAllowList,
    /// Durable owning turn.
    pub owner: ProcessOwner,
    /// Capacity held for the group's life; dropped at once if no group starts.
    pub capacity: Option<CapacityToken>,
}

/// Requested scope of private process closure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseMode {
    /// Request vendor shutdown and wait until the absolute deadline.
    Graceful,
    /// Ask the verified anchor to stop its own process group.
    Force,
}

/// Close action and absolute deadline propagated through C2–C5.
#[derive(Clone, Copy, Debug)]
pub struct CloseRequest {
    /// Requested shutdown mode.
    pub mode: CloseMode,
    /// Absolute monotonic deadline, including cleanup allowance.
    pub deadline: Deadline,
}

/// Persistent identity of the group anchor, never of an arbitrary vendor PID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    /// Linux process id.
    pub pid: u32,
    /// Host-created process group id.
    pub pgid: u32,
    /// Linux user id.
    pub uid: u32,
    /// Boot id observed at launch.
    pub boot_id: String,
    /// PID namespace identity observed at launch.
    pub pid_namespace: String,
    /// Linux process start ticks.
    pub start_ticks: u64,
    /// Private marker never inherited by the vendor.
    pub marker: ProcessMarker,
}

/// Private anchor marker; diagnostic formatting never reveals it.
#[derive(Clone, Eq, PartialEq)]
pub struct ProcessMarker(String);

impl ProcessMarker {
    /// Holds a generated marker without exposing its value in diagnostics.
    pub fn try_from_generated(value: String) -> Result<Self, &'static str> {
        if value.is_empty() || value.len() > 128 {
            return Err("process marker must contain 1 to 128 bytes");
        }
        Ok(Self(value))
    }

    /// Returns the marker only for Host's control challenge and Store record.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProcessMarker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProcessMarker([redacted])")
    }
}

/// Evidence of a same-boot, same-namespace, nonsignalling ESRCH group query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupAbsenceProof {
    /// Full identity used to qualify the probe.
    anchor: ProcessIdentity,
    /// Immutable launch generation.
    generation: String,
    /// Observation time recorded for evidence, not for deadlines.
    observed_at: String,
}

impl GroupAbsenceProof {
    /// Returns the verified anchor identity underlying the absence proof.
    pub fn anchor(&self) -> &ProcessIdentity {
        &self.anchor
    }

    /// Returns the immutable launch generation.
    pub fn generation(&self) -> &str {
        &self.generation
    }

    /// Returns the recorded observation timestamp.
    pub fn observed_at(&self) -> &str {
        &self.observed_at
    }
}

/// Why private-group cleanup cannot be proven complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupReason {
    /// Anchor identity or challenge could not be verified.
    UnverifiedAnchor,
    /// The nonsignalling group query showed a present group.
    GroupPresent,
    /// Permission or namespace prevented an absence proof.
    ProbeDenied,
    /// Positive absence was observed but could not be recorded durably.
    EvidenceStoreFailure,
    /// The cleanup deadline passed before absence was proven.
    Deadline,
}

/// Evidence of cleanup, separate from vendor acceptance or terminal state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CleanupEvidence {
    /// Group absence was positively proven within the documented boundary.
    GroupAbsent(GroupAbsenceProof),
    /// Cleanup remains uncertain.
    Uncertain(CleanupReason),
}

/// Confirmed exit of an owned process, independent of turn disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExitReport {
    /// Exit code, when exited normally.
    pub code: Option<i32>,
    /// Signal number, when terminated by a signal.
    pub signal: Option<i32>,
}
