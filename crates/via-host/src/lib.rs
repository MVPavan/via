//! Host supervises vendor processes and endpoints; it never speaks a vendor
//! protocol or decides session policy.

use std::{ffi::OsString, fmt, path::PathBuf};

mod anchor;
mod exec;
mod fence;
mod host;
mod linux;
mod protocol;
mod stderr_log;

pub use anchor::run_anchor_from_args;
pub use exec::run_exec_from_args;
pub(crate) use host::monotonic_remaining;
pub use host::{
    AcquireFailure, AcquiredProcess, CloseReport, ExitReceiver, Host, HostError, JournalSite,
    LaunchPipes, OwnedPipes, ProcessControl, RecoveryReport, ReprobeReport, ShutdownReport,
    TurnRecovery,
};

/// The owner of a private process group (runtime §5 AR6): the turn of a
/// per-turn route, or a shared server, which no turn owns. Host stays
/// protocol- and key-free: it only records the owner and, for a server,
/// commits each turn's link before the turn's first vendor byte
/// ([`ProcessControl::link_turn`]).
pub use via_store::ProcessOwner;
pub use via_store::{Deadline, ServerId, SessionId, TurnNumber};

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

/// The bounded cause of a Host or launch failure, carried to the turn it
/// ended (bead via-23b): the step that failed and, for an operating-system
/// error, its kind. It holds no vendor text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaunchCause {
    /// The failed step, such as `connect anchor socket`.
    pub step: &'static str,
    /// The operating-system error's kind, when the step failed with one.
    pub kind: Option<std::io::ErrorKind>,
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
    /// Durable owner: the turn, or the shared server (runtime §5 AR6).
    pub owner: ProcessOwner,
    /// The owner's `stderr.log` (design §7.2, runtime §4), in the turn's or
    /// the server's evidence folder: Host creates it and hands it to the
    /// anchor as standard error, which the vendor inherits. Unused with
    /// [`StderrCapture::CountOnly`].
    pub stderr_path: PathBuf,
    /// Capacity held for the group's life; dropped at once if no group starts.
    pub capacity: Option<CapacityToken>,
    /// The vendor cannot outlive its anchor (runtime §5 "Die with the
    /// anchor"): the anchor starts it through the same binary's exec entry
    /// with a `SIGKILL` parent-death signal. `false` for every route but
    /// `OpenCode`'s.
    pub die_with_anchor: bool,
    /// Exclusive launch lock (runtime §5): the lock file the anchor holds
    /// from `Configure` for its whole life, with the server record in it.
    /// `None` for every route but `OpenCode`'s; requires `die_with_anchor`.
    pub exclusive_lock: Option<PathBuf>,
    /// Best-effort version check the anchor runs before the lock (runtime
    /// §5). `None` for every route but `OpenCode`'s; requires
    /// `die_with_anchor`.
    pub version_probe: Option<VersionProbe>,
    /// `Log` (the owner's `stderr.log`, runtime §4) for every route but
    /// `OpenCode`'s, which sets `CountOnly`.
    pub stderr: StderrCapture,
}

/// A best-effort version check before launch (runtime §5 `version_probe`):
/// the anchor runs the spec's program with these arguments through the
/// exec entry, stdin `/dev/null`, stdout kept up to 256 bytes, stderr
/// discarded, and kills it at 2 s.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionProbe {
    /// The probe's arguments, such as `--version`.
    pub args: Vec<OsString>,
    /// The probe's explicit working directory.
    pub cwd: PathBuf,
    /// The probe's explicit environment.
    pub env: EnvAllowList,
    /// The exact trimmed stdout lines that admit the program.
    pub admitted: Vec<String>,
}

/// What becomes of the vendor's standard error (runtime §4).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StderrCapture {
    /// The owner's capped `stderr.log`.
    #[default]
    Log,
    /// No file: the anchor drains the pipe and keeps only its byte count,
    /// reported with the exit facts ([`ProcessControl::stderr_bytes`]).
    CountOnly,
}

/// Why the anchor refused a fenced launch (runtime §5 "Exclusive launch
/// lock" and "Die with the anchor"). Nothing was launched, except for
/// [`FenceRefusal::FenceRecordFailed`], whose child was killed and reaped
/// before it executed the vendor.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum FenceRefusal {
    /// VIA's credentials are not one unprivileged identity: its user IDs
    /// differ or are 0, its group IDs differ, or it has capabilities.
    PrivilegedVia,
    /// The program file has the set-user-ID or set-group-ID bit or a
    /// `security.capability` attribute.
    ProgramPrivileged,
    /// The program file's privileges could not be checked.
    ProgramUnchecked {
        /// The operating-system error number, when there was one.
        errno: Option<i32>,
    },
    /// The version check ran and exited 0 with output outside `admitted`
    /// (printable ASCII only; other bytes are `?`).
    ProbeRefused {
        /// The trimmed output.
        output: String,
    },
    /// The version check failed: a transient startup failure.
    ProbeFailed {
        /// How it failed.
        kind: ProbeFailure,
    },
    /// The lock file could not be opened as a private regular file of
    /// VIA's user.
    LockUnavailable {
        /// The operating-system error number, when there was one.
        errno: Option<i32>,
    },
    /// Another anchor holds the lock.
    LockHeld,
    /// The server record could not be read.
    RecordUnreadable {
        /// The operating-system error number, when there was one.
        errno: Option<i32>,
    },
    /// The recorded server is still present after 1 s.
    PredecessorAlive {
        /// The recorded pid.
        pid: u32,
        /// Its recorded start, in clock ticks after boot.
        start_ticks: u64,
    },
    /// The record comes from this boot but another PID namespace.
    PredecessorUncertain {
        /// The record's PID-namespace identity.
        namespace: String,
    },
    /// The anchor could not write the server record at ARM; it killed and
    /// reaped its child, which had not executed the vendor.
    FenceRecordFailed,
}

impl FenceRefusal {
    /// The launch step that refused (bead via-23b), for a `launch_failed`
    /// cause.
    pub fn step(&self) -> &'static str {
        match self {
            Self::PrivilegedVia => "check VIA credentials",
            Self::ProgramPrivileged | Self::ProgramUnchecked { .. } => {
                "check vendor program privileges"
            }
            Self::ProbeRefused { .. } | Self::ProbeFailed { .. } => "version check",
            Self::LockUnavailable { .. } => "open launch lock",
            Self::LockHeld => "take launch lock",
            Self::RecordUnreadable { .. } => "read server record",
            Self::PredecessorAlive { .. } | Self::PredecessorUncertain { .. } => {
                "predecessor check"
            }
            Self::FenceRecordFailed => "write server record",
        }
    }
}

impl fmt::Display for FenceRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrivilegedVia => {
                formatter.write_str("VIA runs with differing or privileged credentials")
            }
            Self::ProgramPrivileged => formatter
                .write_str("vendor program is set-user-ID, set-group-ID or has file capabilities"),
            Self::ProgramUnchecked { errno } => {
                write!(
                    formatter,
                    "vendor program privileges unchecked (errno {errno:?})"
                )
            }
            Self::ProbeRefused { output } => write!(formatter, "version check printed {output:?}"),
            Self::ProbeFailed { kind } => write!(formatter, "version check failed: {kind:?}"),
            Self::LockUnavailable { errno } => {
                write!(formatter, "launch lock unavailable (errno {errno:?})")
            }
            Self::LockHeld => formatter.write_str("launch lock held by another anchor"),
            Self::RecordUnreadable { errno } => {
                write!(formatter, "server record unreadable (errno {errno:?})")
            }
            Self::PredecessorAlive { pid, start_ticks } => write!(
                formatter,
                "previous server pid {pid} (start {start_ticks} ticks after boot) still present"
            ),
            Self::PredecessorUncertain { namespace } => write!(
                formatter,
                "server record from another PID namespace {namespace}"
            ),
            Self::FenceRecordFailed => formatter.write_str("server record not written"),
        }
    }
}

/// How a version check failed (runtime §5 `ProbeFailed { kind }`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProbeFailure {
    /// It could not be started.
    Spawn {
        /// The operating-system error number, when there was one.
        errno: Option<i32>,
    },
    /// It exited non-zero or by a signal (125, 126 and 127 are the exec
    /// entry's own failures).
    Exit {
        /// Exit code, when exited normally.
        code: Option<i32>,
        /// Signal number, when terminated by a signal.
        signal: Option<i32>,
    },
    /// It was still running at 2 s and was killed.
    Timeout,
    /// Its output exceeded 256 bytes.
    Overflow,
    /// Its output could not be read.
    Read,
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
