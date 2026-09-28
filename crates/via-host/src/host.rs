//! Daemon-side anchor launch, durable gate and verified cleanup.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use process_wrap::tokio::{CommandWrap, ProcessGroup};
use tokio::{
    net::UnixStream,
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::{Mutex, watch},
    task::JoinHandle,
    time::{Instant, timeout_at},
};
use via_store::{
    AnchorCohort, AnchorIdentity, AnchorIntent, AnchorPhase, AnchorRecord, CommitOutcome,
    GroupAbsenceRecord, ProcessJournal, StoreFailureKind,
};

use crate::{
    CleanupEvidence, CleanupReason, CloseMode, CloseRequest, Deadline, ExitReport,
    PrivateProcessSpec, ProcessIdentity, linux,
    protocol::{self, Bootstrap, Reply, Request, VendorConfig},
};

/// Absence verification after a failed acquisition: close's cleanup
/// allowance (Route bounds every close by 3 s).
const FAILED_ACQUIRE_CLEANUP: Duration = Duration::from_secs(3);

/// Runtime §7 and design §6.8: Host stops every live group within 3 s of the
/// daemon force signal.
const EARLY_STOP: Duration = Duration::from_secs(3);

/// One daemon-side Host instance tied to a validated private anchor directory.
#[derive(Clone)]
pub struct Host {
    journal: ProcessJournal,
    anchor_binary: PathBuf,
    anchor_dir: PathBuf,
    tasks: Arc<StdMutex<HostTasks>>,
    capacity: Capacity,
    /// Retires the early-stop task at shutdown when force never came.
    retire: watch::Sender<bool>,
}

/// Host's ledger (design §8): capacity tokens by anchor id, one per group
/// that may still live, each dropped exactly once, when Host proves that
/// group absent (runtime §5: a timed-out wait releases no admission
/// capacity); plus the anchors whose acquisition is still in flight. A `std`
/// mutex, taken alone and never across an `.await`.
#[derive(Clone, Default)]
struct Capacity(Arc<StdMutex<Ledger>>);

#[derive(Default)]
struct Ledger {
    held: HashMap<String, Held>,
    /// Spawned anchors whose acquisition has not returned yet.
    acquiring: HashSet<String>,
    /// Verified controls, registered before ARM (design §6.8 [r5.2]), with
    /// their launch phase [r6.1]; an entry whose control was dropped is
    /// pruned lazily.
    live: HashMap<String, LiveControl>,
    /// Sticky: the early stop's deadline, once the force signal came.
    stopping: Option<Instant>,
    /// Advanced on every added holding: a held entry, or an uncertain
    /// settlement that leaves one for re-probe (design §8). Core's re-probe
    /// loop resets its backoff on it.
    holdings: watch::Sender<u64>,
}

/// A verified control and its launch phase.
#[derive(Clone)]
struct LiveControl {
    stream: Weak<Mutex<ControlConnection>>,
    generation: String,
    stop: Arc<StopFacts>,
    phase: LaunchPhase,
}

/// Where a verified control's launch is (design §6.8 [r6.1]). Only an
/// `Armed` anchor accepts `Stop`; before ARM it treats `Stop` as invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaunchPhase {
    /// Verified; ARM not yet authorized.
    Verified,
    /// Past the gate: ARM is being sent.
    Arming,
    /// The anchor confirmed the vendor spawn.
    Armed,
}

impl Ledger {
    /// Holds `token` for `owner`'s group until its absence is proved, and
    /// returns an entry it replaced. The caller drops that after the ledger
    /// guard: a recovered group's token takes the `RecoveredSlots` mutex,
    /// and the two are never nested (design §1).
    #[must_use = "drop a replaced entry after the ledger guard"]
    fn hold(
        &mut self,
        anchor_id: String,
        owner: crate::SessionId,
        token: crate::CapacityToken,
    ) -> Option<Held> {
        // A group an acquisition still owns is not a holding yet: its
        // uncertain settlement signals it.
        if !self.busy(&anchor_id) {
            self.holdings.send_modify(|generation| *generation += 1);
        }
        self.held.insert(
            anchor_id,
            Held {
                _token: token,
                identity: None,
                owner,
            },
        )
    }

    /// Whether an acquisition or a live control still owns the group.
    fn busy(&self, anchor_id: &str) -> bool {
        self.acquiring.contains(anchor_id)
            || self
                .live
                .get(anchor_id)
                .is_some_and(|control| control.stream.strong_count() > 0)
    }
}

/// One held group: its token and, for a group this Host launched, the
/// identity it verified at `Ready`, kept in memory even when no journal
/// write recorded it (design §7.2 row 4).
struct Held {
    /// Owned for its drop, which releases the capacity.
    _token: crate::CapacityToken,
    identity: Option<(ProcessIdentity, String)>,
    /// The session that owns the group, for a session-filtered re-probe's
    /// count [T3-S2 r2.5].
    owner: crate::SessionId,
}

impl Capacity {
    fn lock(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn hold(&self, anchor_id: String, owner: crate::SessionId, token: crate::CapacityToken) {
        let replaced = self.lock().hold(anchor_id, owner, token);
        drop(replaced);
    }

    /// Records the identity Host verified for a launched anchor.
    fn identify(&self, anchor_id: &str, identity: &ProcessIdentity, generation: &str) {
        if let Some(held) = self.lock().held.get_mut(anchor_id) {
            held.identity = Some((identity.clone(), generation.to_owned()));
        }
    }

    /// Registers a verified control; returns the early stop's deadline when
    /// it has already taken its snapshot, so the caller stops at once.
    fn register(&self, anchor_id: &str, control: LiveControl) -> Option<Instant> {
        let mut ledger = self.lock();
        ledger
            .live
            .retain(|_, control| control.stream.strong_count() > 0);
        ledger.live.insert(anchor_id.to_owned(), control);
        ledger.stopping
    }

    /// Sets the sticky `stopping` flag to `deadline`, the force's instant
    /// plus 3 s, and snapshots the armed live controls, atomically with
    /// registration, the ARM gate and the armed mark (design §6.8 [r5.2,
    /// r6.1]). The deadline is never taken from the clock here: a delayed
    /// task must not gain a fresh budget.
    fn begin_stopping(&self, deadline: Instant) -> (Instant, Vec<LiveControl>) {
        let mut ledger = self.lock();
        let deadline = *ledger.stopping.get_or_insert(deadline);
        let controls = ledger
            .live
            .values()
            .filter(|control| {
                control.phase == LaunchPhase::Armed && control.stream.strong_count() > 0
            })
            .cloned()
            .collect();
        (deadline, controls)
    }

    /// The early stop's deadline, once it set `stopping`.
    fn stopping(&self) -> Option<Instant> {
        self.lock().stopping
    }

    /// The ARM gate's ledger half: refused with the early stop's deadline
    /// once `stopping` is set, else the entry is `Arming`.
    fn begin_arming(&self, anchor_id: &str) -> Result<(), Instant> {
        let mut ledger = self.lock();
        if let Some(deadline) = ledger.stopping {
            return Err(deadline);
        }
        if let Some(control) = ledger.live.get_mut(anchor_id) {
            control.phase = LaunchPhase::Arming;
        }
        Ok(())
    }

    /// Marks the entry `Armed` once the vendor spawned; returns the early
    /// stop's deadline when it already took its snapshot, so the owner
    /// sends `Stop` itself.
    fn armed(&self, anchor_id: &str) -> Option<Instant> {
        let mut ledger = self.lock();
        if let Some(control) = ledger.live.get_mut(anchor_id) {
            control.phase = LaunchPhase::Armed;
        }
        ledger.stopping
    }

    /// Releases the anchor's capacity once its group is proved absent.
    fn settle(&self, anchor_id: &str, cleanup: &CleanupEvidence) {
        if matches!(cleanup, CleanupEvidence::GroupAbsent(_)) {
            let held = self.lock().held.remove(anchor_id);
            drop(held);
        } else {
            // The group stays held: now a holding only a re-probe can release.
            let ledger = self.lock();
            if ledger.held.contains_key(anchor_id) {
                ledger.holdings.send_modify(|generation| *generation += 1);
            }
        }
    }
}

/// Owned tasks and live controls, plus facts kept until the daemon exits.
#[derive(Default)]
struct HostTasks {
    running: Vec<Arc<TrackedTask>>,
    controls: Vec<TrackedControl>,
    /// Collected tasks that failed; a later shutdown still reports them.
    failed: usize,
    /// Generations whose verified anchor reported that the stop Host requested
    /// stopped a live vendor.
    forced: HashSet<String>,
}

/// Owned task outcome: an `Err` is a failed task, such as a failed child wait.
type TaskResult = Result<(), ()>;

struct TrackedTask {
    handle: Mutex<JoinHandle<TaskResult>>,
    joined: AtomicBool,
}

impl TrackedTask {
    fn new(handle: JoinHandle<TaskResult>) -> Arc<Self> {
        Arc::new(Self {
            handle: Mutex::new(handle),
            joined: AtomicBool::new(false),
        })
    }
}

#[derive(Clone)]
struct TrackedControl {
    stream: Weak<Mutex<ControlConnection>>,
    identity: ProcessIdentity,
    anchor_id: String,
    generation: String,
    exit: ExitReceiver,
    stop: Arc<StopFacts>,
}

/// How Host stopped one control's group, shared with its registration.
#[derive(Default)]
struct StopFacts {
    /// The verified anchor reported that Host's stop stopped a live vendor.
    forced: AtomicBool,
}

/// The Host journal write that failed (design §7.2 rows 3, 4 and 12).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalSite {
    /// Anchor intent, before any process exists (row 3).
    AnchorIntent,
    /// Verified anchor identity (row 4).
    Identified,
    /// ARM intent (row 4).
    ArmIntent,
    /// Vendor facts after ARM (row 4).
    VendorFacts,
    /// Group absence proof (row 12).
    Absence,
}

/// Host launch, durable journal or private-control failure.
#[derive(Debug)]
pub enum HostError {
    /// OS or filesystem operation failed.
    Io(io::Error),
    /// A required Store read did not complete.
    Store(&'static str),
    /// A Host journal write lacked a positive commit receipt: not committed,
    /// or `uncertain`, which latches the daemon (design §7.1).
    Journal {
        /// The failed write.
        site: JournalSite,
        /// The write may have committed.
        uncertain: bool,
    },
    /// Host could not read the required durable process journal.
    StoreUnavailable(StoreFailureKind),
    /// Caller supplied an invalid launch boundary.
    Invalid(&'static str),
    /// Caller supplied an expired absolute deadline.
    Deadline,
    /// Anchor identity or private protocol failed validation.
    Protocol(&'static str),
    /// The caller's stop signal was set at the pre-ARM gate: nothing launched.
    Stopped,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "Host I/O: {error}"),
            Self::Store(message) | Self::Invalid(message) | Self::Protocol(message) => {
                formatter.write_str(message)
            }
            Self::StoreUnavailable(kind) => {
                write!(formatter, "process journal unavailable: {kind:?}")
            }
            Self::Journal { site, uncertain } => {
                // The diagnostics of each site before the outcome split.
                formatter.write_str(match site {
                    JournalSite::AnchorIntent => "anchor intent not durably committed",
                    JournalSite::Identified => "anchor identity not durably committed",
                    JournalSite::ArmIntent => "ArmIntent lacks positive commit receipt",
                    JournalSite::VendorFacts => "vendor facts not durably committed",
                    JournalSite::Absence if *uncertain => "group absence commit outcome uncertain",
                    JournalSite::Absence => "group absence commit failed",
                })?;
                if *uncertain && *site != JournalSite::Absence {
                    formatter.write_str(" (outcome uncertain)")?;
                }
                Ok(())
            }
            Self::Deadline => formatter.write_str("Host deadline expired"),
            Self::Stopped => formatter.write_str("stopped before ARM"),
        }
    }
}

impl std::error::Error for HostError {}
impl From<io::Error> for HostError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// The daemon ends of the vendor's exclusive standard streams.
pub struct OwnedPipes {
    /// Vendor stdin writer, transferred once to Wire.
    pub stdin: ChildStdin,
    /// Vendor stdout reader, transferred once to Wire.
    pub stdout: ChildStdout,
    /// Vendor stderr reader, transferred once to Wire.
    pub stderr: ChildStderr,
}

/// The vendor pipes of an acquisition from ARM on, owned by its caller.
#[derive(Default)]
pub struct LaunchPipes(StdMutex<Option<OwnedPipes>>);

impl LaunchPipes {
    fn put(&self, pipes: OwnedPipes) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pipes);
    }

    /// The pipes of an acquisition that sent ARM and did not succeed; `None`
    /// if ARM was never sent (no vendor could have launched) or it succeeded.
    pub fn take(&self) -> Option<OwnedPipes> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// Confirmed vendor exit updates from Host's separate control path.
pub type ExitReceiver = watch::Receiver<Option<ExitReport>>;

/// A failed acquisition and the cleanup Host performed for it (design §2
/// rule 1, §7.2 rows 3 and 4).
#[derive(Debug)]
pub struct AcquireFailure {
    /// The acquisition's own failure.
    pub error: HostError,
    /// The failed acquisition's bounded absence verification; `None` when
    /// no anchor intent was committed, so no group can exist.
    pub cleanup: Option<CleanupEvidence>,
    /// Host's `Stop` through the verified control stopped a live vendor.
    pub forced: bool,
    /// A journal write of this acquisition or its cleanup had an uncertain
    /// outcome: the daemon must latch (design §7.1).
    pub journal_uncertain: bool,
}

impl std::fmt::Display for AcquireFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for AcquireFailure {}

/// One re-probe pass over held groups (design §8).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReprobeReport {
    /// Held groups without a live control, as the pass found them; with a
    /// session filter, only that session's (see [`Host::reprobe_held`]).
    pub held: usize,
    /// Groups proved absent and released by this pass.
    pub proved: usize,
    /// Owner sessions of proofs observed whose commit was not committed;
    /// their tokens stay held and the next pass retries (design §7.2 row
    /// 12). Core records each failure against its session.
    pub not_committed: Vec<crate::SessionId>,
}

/// Successfully launched process after durable vendor facts and pipe detachment.
pub struct AcquiredProcess {
    /// Exclusive vendor standard streams.
    pub pipes: OwnedPipes,
    /// Verified private anchor control.
    pub control: ProcessControl,
    /// Confirmed vendor exit, independent of protocol result.
    pub exits: ExitReceiver,
}

struct StartedAnchor {
    anchor_id: String,
    generation: String,
    identity: ProcessIdentity,
    control: Arc<Mutex<ControlConnection>>,
    stop: Arc<StopFacts>,
    pipes: OwnedPipes,
    version: u64,
}

/// What an acquisition reached, kept across its cancellation by the
/// deadline so the failure path can clean up and report evidence.
#[derive(Default)]
struct Acquisition {
    /// The anchor intent committed.
    intent: bool,
    /// The anchor process spawned.
    spawned: Option<String>,
    /// The anchor's verified identity, recorded before its commit.
    started: Option<(String, String, ProcessIdentity)>,
    /// Host's row-4 `Stop` stopped a live vendor.
    forced: bool,
    /// The one cleanup deadline, set before the cleanup starts: row 4's
    /// `Stop` (design §7.2 row 4), or the early stop's deadline once the
    /// acquisition observed `stopping` (§6.8). The absence check that
    /// follows uses what remains of it.
    cleanup_by: Option<Deadline>,
    /// The acquisition observed Host's early stop: any failure is then
    /// [`HostError::Stopped`] (design §6.8 [r6.1]).
    stopping: bool,
}

impl Acquisition {
    /// The acquisition observed Host's early stop (design §6.8): its
    /// failure is `Stopped`, and its cleanup (the EOF drop or the owner's
    /// `Stop`, then the absence check) runs under the early stop's own
    /// deadline, never a fresh allowance. Returns that deadline.
    fn stop_early(&mut self, deadline: Instant) -> Deadline {
        let deadline = Deadline::at(deadline);
        self.stopping = true;
        self.cleanup_by = Some(deadline);
        deadline
    }
}

/// One held group's re-probe result.
enum Reprobed {
    Proved,
    NotCommitted,
    Held,
}

struct ControlConnection {
    stream: UnixStream,
    reader: protocol::FrameReader,
}

impl ControlConnection {
    async fn transact(&mut self, request: &Request, max: usize) -> io::Result<Reply> {
        protocol::write_frame(&mut self.stream, request, max).await?;
        self.reader
            .read(&self.stream)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "anchor closed control"))
    }
}

/// Verified live-anchor control capability; never selects an arbitrary PID.
#[derive(Clone)]
pub struct ProcessControl {
    stream: Arc<Mutex<ControlConnection>>,
    identity: ProcessIdentity,
    anchor_id: String,
    generation: String,
    journal: ProcessJournal,
    exit: ExitReceiver,
    stop: Arc<StopFacts>,
    capacity: Capacity,
}

/// Process facts and cleanup evidence from a close request.
#[derive(Debug)]
pub struct CloseReport {
    /// Positive absence or explicit uncertainty.
    pub cleanup: CleanupEvidence,
    /// Last confirmed direct vendor exit, if known.
    pub vendor_exit: Option<ExitReport>,
    /// The verified anchor reported that its cleanup for this stop began while
    /// the vendor was live: Host force evidence, never proof of absence by itself.
    pub forced: bool,
    /// The absence proof's commit had an uncertain outcome: the daemon must
    /// latch (design §7.2 row 12).
    pub journal_uncertain: bool,
}

/// Recovery result for one committed anchor intent.
#[derive(Debug)]
pub struct RecoveryReport {
    /// Durable internal anchor identifier.
    pub anchor_id: String,
    /// Immutable launch generation used to correlate the durable proof.
    pub generation: String,
    /// Owning session for passive Core correlation.
    pub owner_session: crate::SessionId,
    /// Owning turn for passive Core correlation.
    pub owner_turn: crate::TurnNumber,
    /// Positive absence or explicit uncertainty.
    pub cleanup: CleanupEvidence,
    /// This Host's close stopped the group while its vendor was live, as the
    /// verified anchor reported. A control released unclosed carries no such
    /// evidence: the anchor's EOF cleanup has no reply. Kill evidence only
    /// with absence.
    pub forced: bool,
}

/// Shutdown recovery aggregate for one owning turn the caller asked about.
#[derive(Debug)]
pub struct TurnRecovery {
    /// Owning session.
    pub owner_session: crate::SessionId,
    /// Owning turn.
    pub owner_turn: crate::TurnNumber,
    /// Committed anchors of this turn that Host reconciled.
    pub anchors: usize,
    /// The last absence proof when every anchor was proved absent, otherwise
    /// the first uncertainty.
    pub cleanup: CleanupEvidence,
    /// Host force evidence for any of the turn's anchors.
    pub forced: bool,
}

impl TurnRecovery {
    fn fold(&mut self, report: RecoveryReport) {
        self.anchors += 1;
        self.forced |= report.forced;
        if matches!(self.cleanup, CleanupEvidence::GroupAbsent(_)) {
            self.cleanup = report.cleanup;
        }
    }
}

/// Bounded shutdown result, returned on every path; unfinished tasks retain
/// their Host owner while the daemon lives.
#[derive(Debug)]
pub struct ShutdownReport {
    /// Per-turn evidence for the requested turns only, established before any
    /// failure; the inventory is consumed page by page, never retained.
    pub recovery: Vec<TurnRecovery>,
    /// Committed anchors reconciled.
    pub anchors: usize,
    /// Reconciled anchors without positive absence proof.
    pub uncertain_anchors: usize,
    /// Retained reaper or status tasks whose result was not collected by the deadline.
    pub pending_tasks: usize,
    /// Tasks that panicked, were cancelled or failed their child wait, collected
    /// by this or any earlier shutdown call.
    pub failed_tasks: usize,
    /// The named deadline, Store or recovery failure that precluded a complete report.
    pub failure: Option<HostError>,
}

impl Host {
    /// Attaches Host to an existing private anchor directory and same-binary path.
    pub fn new(
        journal: ProcessJournal,
        anchor_binary: PathBuf,
        anchor_dir: PathBuf,
    ) -> Result<Self, HostError> {
        if !anchor_binary.is_absolute() {
            return Err(HostError::Invalid("anchor executable must be absolute"));
        }
        linux::secure_directory(&anchor_dir)?;
        Ok(Self {
            journal,
            anchor_binary,
            anchor_dir,
            tasks: Arc::new(StdMutex::new(HostTasks::default())),
            capacity: Capacity::default(),
            retire: watch::Sender::new(false),
        })
    }

    /// Starts one private group through the durable intent and ARM gate.
    pub async fn acquire(
        &self,
        spec: PrivateProcessSpec,
        deadline: Deadline,
    ) -> Result<AcquiredProcess, HostError> {
        self.acquire_retaining(spec, deadline, &LaunchPipes::default(), &|| false)
            .await
            .map_err(|failure| failure.error)
    }

    /// [`Host::acquire`] that moves the vendor pipes into `launch` just before
    /// ARM is sent: from then on the vendor may run and write to them, so they
    /// outlive an acquisition that fails or is abandoned after ARM, and the
    /// caller can still record that output. A successful acquisition takes
    /// them back.
    ///
    /// `stopped` is the caller's pre-ARM gate (design §2 rule 1): the daemon
    /// force watch and the turn's stop order. Checked at the last gate before
    /// ARM, together with Host's own early stop: true by then, no ARM is
    /// sent, the anchor control is dropped so its group stops, and the error
    /// is [`HostError::Stopped`]. Set after the check, ARM won and the launch
    /// is in flight.
    ///
    /// A failure carries the acquisition's own bounded absence verification
    /// and Host's force evidence (design §7.2 rows 3 and 4).
    pub async fn acquire_retaining(
        &self,
        spec: PrivateProcessSpec,
        deadline: Deadline,
        launch: &LaunchPipes,
        stopped: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<AcquiredProcess, Box<AcquireFailure>> {
        let refused = |error| {
            Box::new(AcquireFailure {
                error,
                cleanup: None,
                forced: false,
                journal_uncertain: false,
            })
        };
        if Instant::now() >= deadline.instant() {
            return Err(refused(HostError::Deadline));
        }
        if !spec.program.is_absolute() || !spec.cwd.is_absolute() {
            return Err(refused(HostError::Invalid(
                "vendor executable and cwd must be absolute",
            )));
        }
        if spec
            .env
            .entries()
            .iter()
            .any(|(name, _)| name == "VIA_PROCESS_MARKER")
        {
            return Err(refused(HostError::Invalid(
                "reserved vendor marker environment key",
            )));
        }
        let mut state = Acquisition::default();
        let acquired = timeout_at(
            deadline.instant(),
            self.acquire_inner(spec, launch, stopped, &mut state),
        )
        .await
        .map_err(|_| HostError::Deadline)
        .flatten();
        let result = match acquired {
            Ok(acquired) => Ok(acquired),
            Err(error) => Err(Box::new(self.failed_acquisition(error, &state).await)),
        };
        if let Some(anchor_id) = &state.spawned {
            self.capacity.lock().acquiring.remove(anchor_id);
        }
        result
    }

    /// Design §11 and §7.2 row 4: a failed acquisition whose anchor spawned
    /// proves the group absent before it returns, as close does and within
    /// close's cleanup allowance, from the identity Host verified even when
    /// no journal write recorded it. The anchor control is dropped by now,
    /// so the anchor exits on EOF and stops its group. Only `GroupAbsent`
    /// releases the anchor's capacity; uncertainty keeps it with its
    /// in-memory identity for re-probe (§8).
    async fn failed_acquisition(&self, error: HostError, state: &Acquisition) -> AcquireFailure {
        let error = if state.stopping {
            HostError::Stopped
        } else {
            error
        };
        let mut journal_uncertain = matches!(
            error,
            HostError::Journal {
                uncertain: true,
                ..
            }
        );
        let cleanup = match &state.started {
            Some((anchor_id, generation, identity)) => {
                let cleanup = state
                    .cleanup_by
                    .unwrap_or_else(|| Deadline::at(Instant::now() + FAILED_ACQUIRE_CLEANUP));
                let evidence =
                    match wait_absence(&self.journal, anchor_id, generation, identity, cleanup)
                        .await
                    {
                        Ok(evidence) => evidence,
                        Err(error) => {
                            journal_uncertain |= matches!(
                                error,
                                HostError::Journal {
                                    uncertain: true,
                                    ..
                                }
                            );
                            CleanupEvidence::Uncertain(CleanupReason::EvidenceStoreFailure)
                        }
                    };
                self.capacity.settle(anchor_id, &evidence);
                Some(evidence)
            }
            // An intent with no verified identity cannot be proved absent.
            None if state.intent => {
                Some(CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor))
            }
            None => None,
        };
        AcquireFailure {
            error,
            cleanup,
            forced: state.forced,
            journal_uncertain,
        }
    }

    /// Holds capacity for a group this Host did not launch, such as one an
    /// earlier daemon left whose absence recovery did not prove, owned by
    /// session `owner`; a later absence proof for `anchor_id` releases it.
    pub fn hold_capacity(
        &self,
        anchor_id: String,
        owner: crate::SessionId,
        token: crate::CapacityToken,
    ) {
        self.capacity.hold(anchor_id, owner, token);
    }

    /// Advances on every added holding (design §8): Core's re-probe loop
    /// resets its backoff to 1 s when it changes.
    pub fn holdings_changed(&self) -> watch::Receiver<u64> {
        self.capacity.lock().holdings.subscribe()
    }

    /// Held groups with no live control: the ledger entries only a proof
    /// can release (design §6.6 `connections.held_unproven`, §8).
    pub fn held_unproven(&self) -> usize {
        let ledger = self.capacity.lock();
        ledger
            .held
            .keys()
            .filter(|anchor_id| !ledger.busy(anchor_id))
            .count()
    }

    /// Groups this Host launched whose cleanup is still owned by a live
    /// control or an acquisition in flight: runtime §8 "pending cleanup",
    /// which blocks idle exit (design §6.4). Groups left `uncertain` or held
    /// for an earlier daemon do not count.
    pub fn pending_cleanup(&self) -> usize {
        let ledger = self.capacity.lock();
        let live = ledger
            .live
            .iter()
            .filter(|(anchor_id, control)| {
                control.stream.strong_count() > 0 && !ledger.acquiring.contains(*anchor_id)
            })
            .count();
        live + ledger.acquiring.len()
    }

    /// Subscribes Host's early-stop task to the daemon force signal (design
    /// §6.8 [r4.3, r5.1–r5.4]). On the signal it stops every live group in
    /// the ledger through its verified control, concurrently, each bounded
    /// at the signal time plus 3 s, recording the anchor's `stopped_live`
    /// in the control's stop facts; it polls no caller and commits nothing.
    /// The signal carries the instant Core raised the force (`None` until
    /// then), and that instant, not the one this task runs at, anchors the
    /// bound and every stop after it. A control registered after the
    /// snapshot is stopped at once. The task is Host-owned:
    /// [`Host::shutdown`] retires it when force never came.
    pub fn watch_force(&self, mut forced: watch::Receiver<Option<Instant>>) {
        let ledger = self.capacity.clone();
        let mut retire = self.retire.subscribe();
        let task = tokio::spawn(async move {
            let forced_at = tokio::select! {
                biased;
                at = raised_at(&mut forced) => at,
                () = raised(&mut retire) => return Ok(()),
            };
            // Test builds: the force woke this task, which has taken nothing
            // yet; a pause here delays the task as a blocked runtime would.
            #[cfg(feature = "test-failpoints")]
            let _ = via_store::failpoint::hit_async("host.early_stop.woken").await;
            let (deadline, controls) = ledger.begin_stopping(forced_at + EARLY_STOP);
            // Test builds: the snapshot is taken and no Stop sent yet.
            #[cfg(feature = "test-failpoints")]
            let _ = via_store::failpoint::hit_async("host.early_stop.snapshot").await;
            let mut stops = tokio::task::JoinSet::new();
            for control in controls {
                stops.spawn(early_stop(control, deadline));
            }
            while stops.join_next().await.is_some() {}
            Ok(())
        });
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
            .push(TrackedTask::new(task));
    }

    /// Design §8: one non-signalling pass over held groups with no live
    /// control, optionally only `owner`'s. Each is probed with its durable
    /// full identity, or the in-memory one Host verified (§7.2 row 4); a
    /// same-boot `ESRCH` commits the proof, with that identity, and releases
    /// the token. No `Stop`, `Challenge` or other mutation is sent. A proof
    /// commit that is not committed keeps the token for the next pass; an
    /// uncertain one is returned, and latches. Nothing is read while
    /// nothing is held; a group with no identity keeps its token.
    ///
    /// `held` counts every eligible group for `None`, and with `owner` only
    /// that session's, examined or not: a pass that ends before its pages
    /// did still counts them, so a caller never takes an unread group of the
    /// session for proved [T3-S2 r1.3, r2.5].
    pub async fn reprobe_held(
        &self,
        deadline: Deadline,
        owner: Option<crate::SessionId>,
    ) -> Result<ReprobeReport, HostError> {
        let mut remaining: HashMap<String, (Option<(ProcessIdentity, String)>, crate::SessionId)> = {
            let ledger = self.capacity.lock();
            ledger
                .held
                .iter()
                .filter(|(anchor_id, held)| {
                    !ledger.busy(anchor_id)
                        && owner.as_ref().is_none_or(|owner| *owner == held.owner)
                })
                .map(|(anchor_id, held)| {
                    (
                        anchor_id.clone(),
                        (held.identity.clone(), held.owner.clone()),
                    )
                })
                .collect()
        };
        let mut report = ReprobeReport {
            held: remaining.len(),
            ..ReprobeReport::default()
        };
        let mut after = None;
        while !remaining.is_empty() && Instant::now() < deadline.instant() {
            let page = timeout_at(
                deadline.instant(),
                self.journal.unproven_anchor_records_page(
                    after,
                    via_store::ANCHOR_PAGE_LIMIT,
                    owner.clone(),
                ),
            )
            .await
            .map_err(|_| HostError::Store("anchor journal page read timed out"))?
            .map_err(HostError::StoreUnavailable)?;
            let full = page.len() == via_store::ANCHOR_PAGE_LIMIT as usize;
            after = page.last().map(|record| record.intent.anchor_id.clone());
            for record in page {
                let Some((memory, owner)) = remaining.remove(&record.intent.anchor_id) else {
                    continue;
                };
                match self.reprobe_one(record, memory, deadline).await? {
                    Reprobed::Proved => report.proved += 1,
                    Reprobed::NotCommitted => report.not_committed.push(owner),
                    Reprobed::Held => {}
                }
            }
            if !full {
                break;
            }
        }
        Ok(report)
    }

    async fn reprobe_one(
        &self,
        record: AnchorRecord,
        memory: Option<(ProcessIdentity, String)>,
        deadline: Deadline,
    ) -> Result<Reprobed, HostError> {
        let generation = record.intent.generation.clone();
        let identity = match memory {
            Some((identity, remembered)) if remembered == generation => identity,
            _ => match record.identity.map(identity_from_store) {
                Some(Ok(identity)) => identity,
                // No identity: it cannot be probed and keeps its token.
                _ => return Ok(Reprobed::Held),
            },
        };
        if record.intent.marker != identity.marker.as_str()
            || record.intent.uid != identity.uid
            || record.intent.boot_id != identity.boot_id
            || record.intent.pid_namespace != identity.pid_namespace
        {
            return Ok(Reprobed::Held);
        }
        let CleanupEvidence::GroupAbsent(proof) = linux::probe_absence(&identity, &generation)
        else {
            return Ok(Reprobed::Held);
        };
        let absence = absence_record(&record.intent.anchor_id, &generation, &identity, &proof);
        match timeout_at(
            deadline.instant(),
            self.journal.commit_group_absence(absence),
        )
        .await
        {
            Ok(CommitOutcome::Committed(())) => {
                self.capacity.settle(
                    &record.intent.anchor_id,
                    &CleanupEvidence::GroupAbsent(proof),
                );
                Ok(Reprobed::Proved)
            }
            Ok(CommitOutcome::NotCommitted(_)) => Ok(Reprobed::NotCommitted),
            Ok(CommitOutcome::Uncertain(_)) | Err(_) => Err(HostError::Journal {
                site: JournalSite::Absence,
                uncertain: true,
            }),
        }
    }

    async fn start_anchor(
        &self,
        owner: crate::ProcessOwner,
        capacity: Option<crate::CapacityToken>,
        state: &mut Acquisition,
    ) -> Result<StartedAnchor, HostError> {
        let anchor_id = linux::random_hex()?;
        let generation = linux::random_hex()?;
        let marker = linux::random_hex()?;
        let socket_path = self.anchor_dir.join(format!("{anchor_id}.sock"));
        let config_path = self.anchor_dir.join(format!("{anchor_id}.json"));
        let intent = AnchorIntent {
            anchor_id: anchor_id.clone(),
            generation: generation.clone(),
            marker: marker.clone(),
            socket_path: socket_path.clone(),
            owner_session: owner.session_id.clone(),
            owner_turn: owner.turn,
            uid: rustix::process::getuid().as_raw(),
            boot_id: linux::boot_id()?,
            pid_namespace: linux::pid_namespace()?,
        };
        let receipt = committed(
            self.journal.commit_anchor_intent(intent.clone()).await,
            JournalSite::AnchorIntent,
        )?;
        state.intent = true;
        let bootstrap = Bootstrap {
            anchor_id: anchor_id.clone(),
            generation: generation.clone(),
            marker: marker.clone(),
            controller_pid: std::process::id(),
            socket_path: socket_path.clone(),
            #[cfg(feature = "test-failpoints")]
            failpoints: via_store::failpoint::activation(),
        };
        write_bootstrap(&config_path, &bootstrap)?;
        let (pipes, anchor_process_id) = self.spawn_anchor(&config_path)?;
        // The group exists from here: its capacity stays with Host until
        // absence is proved. A failure above dropped it with no group.
        let replaced = {
            let mut ledger = self.capacity.lock();
            ledger.acquiring.insert(anchor_id.clone());
            capacity.and_then(|token| ledger.hold(anchor_id.clone(), owner.session_id, token))
        };
        drop(replaced);
        state.spawned = Some(anchor_id.clone());
        let mut stream = connect_anchor(&socket_path).await?;
        let ready = protocol::read_frame::<Reply>(&mut stream, 1024)
            .await?
            .ok_or(HostError::Protocol("anchor did not become ready"))?;
        let Reply::Ready {
            identity: wire_identity,
        } = ready
        else {
            return Err(HostError::Protocol("unexpected anchor ready frame"));
        };
        let identity = wire_identity.into_public()?;
        verify_identity(&stream, &identity, anchor_process_id, &intent)?;
        // Design §7.2 row 4 [r3.12]: the verified identity is recorded for
        // cleanup before its commit, so a failed commit still proves absence.
        state.started = Some((anchor_id.clone(), generation.clone(), identity.clone()));
        self.capacity.identify(&anchor_id, &identity, &generation);
        // Design §6.8 [r5.2]: the verified control is in the ledger before
        // ARM, so Host's early stop reaches it; registered after the early
        // stop's snapshot, it is stopped at once (dropped: EOF before ARM).
        let control = Arc::new(Mutex::new(ControlConnection {
            stream,
            reader: protocol::FrameReader::new(1024),
        }));
        let stop = Arc::new(StopFacts::default());
        let registered = self.capacity.register(
            &anchor_id,
            LiveControl {
                stream: Arc::downgrade(&control),
                generation: generation.clone(),
                stop: stop.clone(),
                phase: LaunchPhase::Verified,
            },
        );
        if let Some(deadline) = registered {
            state.stop_early(deadline);
            return Err(HostError::Stopped);
        }
        let version = committed(
            self.journal
                .commit_anchor_identified(
                    &anchor_id,
                    &generation,
                    receipt.record_version,
                    identity_to_store(&identity),
                )
                .await,
            JournalSite::Identified,
        )?;
        Ok(StartedAnchor {
            anchor_id,
            generation,
            identity,
            control,
            stop,
            pipes,
            version,
        })
    }

    fn spawn_anchor(&self, config_path: &PathBuf) -> Result<(OwnedPipes, u32), HostError> {
        let mut command = CommandWrap::with_new(&self.anchor_binary, |command| {
            command
                .arg("__via_host_anchor")
                .arg(config_path)
                .env_clear()
                .current_dir(&self.anchor_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        command.wrap(ProcessGroup::leader());
        let mut anchor = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_file(config_path);
                return Err(error.into());
            }
        };
        let anchor_process_id = anchor
            .id()
            .ok_or(HostError::Protocol("missing anchor pid"))?;
        let pipes = OwnedPipes {
            stdin: anchor
                .stdin()
                .take()
                .ok_or(HostError::Protocol("missing vendor stdin pipe"))?,
            stdout: anchor
                .stdout()
                .take()
                .ok_or(HostError::Protocol("missing vendor stdout pipe"))?,
            stderr: anchor
                .stderr()
                .take()
                .ok_or(HostError::Protocol("missing vendor stderr pipe"))?,
        };
        // Reap the anchor regardless of later journal/control failures; a failed
        // wait is a failed task, never successful reaping.
        let task = tokio::spawn(async move { anchor.wait().await.map(drop).map_err(drop) });
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
            .push(TrackedTask::new(task));
        Ok((pipes, anchor_process_id))
    }

    async fn acquire_inner(
        &self,
        mut spec: PrivateProcessSpec,
        launch: &LaunchPipes,
        stopped: &(dyn Fn() -> bool + Send + Sync),
        state: &mut Acquisition,
    ) -> Result<AcquiredProcess, HostError> {
        let StartedAnchor {
            anchor_id,
            generation,
            identity,
            control,
            stop,
            pipes,
            version,
        } = self
            .start_anchor(spec.owner.clone(), spec.capacity.take(), state)
            .await?;
        let vendor = vendor_config(&spec)?;
        configure(&control, vendor).await?;
        committed(
            self.journal
                .commit_arm_intent(&anchor_id, &generation, version)
                .await,
            JournalSite::ArmIntent,
        )?;
        #[cfg(feature = "test-failpoints")]
        via_store::failpoint::hit_async("host.anchor.after_arm_intent_commit")
            .await
            .map_err(HostError::Io)?;
        // The pre-ARM launch gate: a stop set by now wins and nothing launches;
        // returning drops the anchor control, so the anchor exits on EOF and
        // stops its group. Past this check ARM wins and the launch is in flight.
        if stopped() {
            // Under Host's early stop the cleanup keeps its deadline (design
            // §6.8); only a caller-only stop gets the fresh allowance.
            if let Some(deadline) = self.capacity.stopping() {
                state.stop_early(deadline);
            }
            return Err(HostError::Stopped);
        }
        if let Err(deadline) = self.capacity.begin_arming(&anchor_id) {
            state.stop_early(deadline);
            return Err(HostError::Stopped);
        }
        // This is the only ARM send for this generation; errors never cause retry.
        launch.put(pipes);
        let reply = control
            .lock()
            .await
            .transact(
                &Request::Arm {
                    generation: generation.clone(),
                },
                1024,
            )
            .await?;
        let Reply::Spawned { pid: vendor_pid } = reply else {
            return Err(HostError::Protocol(
                "anchor did not confirm descriptor detachment",
            ));
        };
        // Design §6.8 [r6.1]: armed right after `Spawned`; an early stop
        // that already took its snapshot missed this group, so the owner
        // stops it under the original deadline.
        if let Some(deadline) = self.capacity.armed(&anchor_id) {
            let deadline = state.stop_early(deadline);
            state.forced = stop_through(&control, &generation, &stop, deadline).await;
            #[cfg(feature = "test-failpoints")]
            let _ = via_store::failpoint::hit_async("host.early_stop.sent").await;
            return Err(HostError::Stopped);
        }
        if let Err(error) = committed(
            self.journal
                .commit_vendor_facts(&anchor_id, &generation, vendor_pid)
                .await,
            JournalSite::VendorFacts,
        ) {
            // Design §7.2 row 4: after ARM the vendor runs; Host stops the
            // group through the still-live control and keeps its evidence.
            // The `Stop` and the absence check share this one deadline.
            let deadline = Deadline::at(Instant::now() + FAILED_ACQUIRE_CLEANUP);
            state.cleanup_by = Some(deadline);
            state.forced = stop_through(&control, &generation, &stop, deadline).await;
            if state.forced {
                self.tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .forced
                    .insert(generation);
            }
            return Err(error);
        }
        let pipes = launch
            .take()
            .ok_or(HostError::Protocol("launch pipes already taken"))?;
        let (sender, exits) = watch::channel(None);
        let control = ProcessControl {
            stream: control,
            identity,
            anchor_id,
            generation,
            journal: self.journal.clone(),
            exit: exits.clone(),
            stop,
            capacity: self.capacity.clone(),
        };
        self.track_control(&control, sender);
        Ok(AcquiredProcess {
            pipes,
            control,
            exits,
        })
    }

    fn track_control(&self, control: &ProcessControl, sender: watch::Sender<Option<ExitReport>>) {
        let poll_stream = Arc::downgrade(&control.stream);
        let poll_generation = control.generation.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let Some(stream) = poll_stream.upgrade() else {
                    break;
                };
                let reply = tokio::time::timeout(Duration::from_millis(100), async {
                    stream
                        .lock()
                        .await
                        .transact(
                            &Request::Status {
                                generation: poll_generation.clone(),
                            },
                            1024,
                        )
                        .await
                })
                .await;
                drop(stream);
                match reply {
                    Ok(Ok(Reply::Status {
                        exit_code,
                        exit_signal,
                        ..
                    })) if exit_code.is_some() || exit_signal.is_some() => {
                        let report = ExitReport {
                            code: exit_code,
                            signal: exit_signal,
                        };
                        sender.send_replace(Some(report));
                        break;
                    }
                    Ok(Ok(Reply::Status { .. })) => {}
                    _ => break,
                }
            }
            Ok(())
        });
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.running.push(TrackedTask::new(task));
        tasks.controls.push(TrackedControl {
            stream: Arc::downgrade(&control.stream),
            identity: control.identity.clone(),
            anchor_id: control.anchor_id.clone(),
            generation: control.generation.clone(),
            exit: control.exit.clone(),
            stop: control.stop.clone(),
        });
    }

    /// Closes known live controls, reconciles the journal page by page, and
    /// joins owned tasks. Evidence is kept only for the `turns` asked about.
    ///
    /// The report survives every failure: an expired deadline or recovery error
    /// keeps pending and failed join counts, and unjoined tasks stay owned here.
    pub async fn shutdown(
        &self,
        deadline: Deadline,
        turns: &[(crate::SessionId, crate::TurnNumber)],
    ) -> ShutdownReport {
        // Design §6.8 [r5.1]: an early-stop task that never saw the force
        // signal exits now; one already stopping keeps its bounded work.
        self.retire.send_replace(true);
        if Instant::now() >= deadline.instant() {
            let (pending_tasks, failed_tasks) = join_owned_tasks(&self.tasks, deadline).await;
            return ShutdownReport {
                recovery: Vec::new(),
                anchors: 0,
                uncertain_anchors: 0,
                pending_tasks,
                failed_tasks,
                failure: Some(HostError::Deadline),
            };
        }
        let controls = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let HostTasks {
                controls, forced, ..
            } = &mut *tasks;
            for control in controls.iter() {
                if control.stop.forced.load(Ordering::Acquire) {
                    forced.insert(control.generation.clone());
                }
            }
            controls.retain(|control| control.stream.strong_count() > 0);
            controls.clone()
        };
        for tracked in controls {
            if Instant::now() >= deadline.instant() {
                break;
            }
            if let Some(stream) = tracked.stream.upgrade() {
                let control = ProcessControl {
                    stream,
                    identity: tracked.identity,
                    anchor_id: tracked.anchor_id,
                    generation: tracked.generation,
                    journal: self.journal.clone(),
                    exit: tracked.exit,
                    stop: tracked.stop,
                    capacity: self.capacity.clone(),
                };
                let close = control
                    .close(CloseRequest {
                        mode: CloseMode::Force,
                        deadline,
                    })
                    .await;
                if close.forced {
                    self.tasks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .forced
                        .insert(control.generation);
                }
            }
        }
        let (recovery, anchors, uncertain_anchors, failure) =
            match self.reconcile_turns(turns, deadline).await {
                Ok((recovery, anchors, uncertain)) => (recovery, anchors, uncertain, None),
                Err(error) => (Vec::new(), 0, 0, Some(error)),
            };
        let (pending_tasks, failed_tasks) = join_owned_tasks(&self.tasks, deadline).await;
        let failure = failure.or((pending_tasks > 0).then_some(HostError::Deadline));
        ShutdownReport {
            recovery,
            anchors,
            uncertain_anchors,
            pending_tasks,
            failed_tasks,
            failure,
        }
    }

    /// Reconciles every committed anchor page by page, keeping per-turn
    /// aggregates only for `turns` plus totals; returns `(turns, anchors,
    /// uncertain anchors)`.
    async fn reconcile_turns(
        &self,
        turns: &[(crate::SessionId, crate::TurnNumber)],
        deadline: Deadline,
    ) -> Result<(Vec<TurnRecovery>, usize, usize), HostError> {
        let mut recovery: Vec<TurnRecovery> = Vec::new();
        let (mut anchors, mut uncertain) = (0, 0);
        let mut after = None;
        loop {
            let page = self
                .recover_page(after, via_store::ANCHOR_PAGE_LIMIT, deadline)
                .await?;
            let full = page.len() == via_store::ANCHOR_PAGE_LIMIT as usize;
            after = page.last().map(|report| report.anchor_id.clone());
            for report in page {
                anchors += 1;
                if !matches!(report.cleanup, CleanupEvidence::GroupAbsent(_)) {
                    uncertain += 1;
                }
                let owner = (report.owner_session.clone(), report.owner_turn);
                if !turns.contains(&owner) {
                    continue;
                }
                match recovery
                    .iter_mut()
                    .find(|turn| (&turn.owner_session, turn.owner_turn) == (&owner.0, owner.1))
                {
                    Some(turn) => turn.fold(report),
                    None => recovery.push(TurnRecovery {
                        owner_session: owner.0,
                        owner_turn: owner.1,
                        anchors: 1,
                        cleanup: report.cleanup,
                        forced: report.forced,
                    }),
                }
            }
            if !full {
                return Ok((recovery, anchors, uncertain));
            }
        }
    }

    /// Reconciles one page of up to `limit` committed anchor records after the
    /// `after` anchor id, in id order, one report per record.
    pub async fn recover_page(
        &self,
        after: Option<String>,
        limit: u32,
        deadline: Deadline,
    ) -> Result<Vec<RecoveryReport>, HostError> {
        self.reconcile_page(after, limit, None, deadline).await
    }

    /// [`Self::recover_page`] of the anchors in `cohort` only: resumed
    /// paging never reads, so never challenges, an anchor this daemon
    /// committed after startup (design §8).
    pub async fn recover_cohort_page(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: AnchorCohort,
        deadline: Deadline,
    ) -> Result<Vec<RecoveryReport>, HostError> {
        self.reconcile_page(after, limit, Some(cohort), deadline)
            .await
    }

    async fn reconcile_page(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: Option<AnchorCohort>,
        deadline: Deadline,
    ) -> Result<Vec<RecoveryReport>, HostError> {
        if Instant::now() >= deadline.instant() {
            return Err(HostError::Deadline);
        }
        let read = async {
            match cohort {
                Some(cohort) => {
                    self.journal
                        .list_cohort_records_page(after, limit, cohort)
                        .await
                }
                None => self.journal.list_anchor_records_page(after, limit).await,
            }
        };
        // A read that never completed is a Store failure, not unproven cleanup.
        let records = timeout_at(deadline.instant(), read)
            .await
            .map_err(|_| HostError::Store("anchor journal page read timed out"))?
            .map_err(HostError::StoreUnavailable)?;
        let mut results = Vec::with_capacity(records.len());
        for record in records {
            let anchor_id = record.intent.anchor_id.clone();
            let generation = record.intent.generation.clone();
            let owner_session = record.intent.owner_session.clone();
            let owner_turn = record.intent.owner_turn;
            let cleanup = self.recover_one(record, deadline).await?;
            self.capacity.settle(&anchor_id, &cleanup);
            let forced = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forced
                .contains(&generation);
            results.push(RecoveryReport {
                anchor_id,
                generation,
                owner_session,
                owner_turn,
                cleanup,
                forced,
            });
        }
        Ok(results)
    }

    /// Process-proof outcomes are `CleanupEvidence`; a failed or uncertain
    /// absence-proof commit is a Store failure.
    async fn recover_one(
        &self,
        record: AnchorRecord,
        deadline: Deadline,
    ) -> Result<CleanupEvidence, HostError> {
        let Some(stored_identity) = record.identity else {
            return Ok(CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor));
        };
        let Ok(identity) = identity_from_store(stored_identity) else {
            return Ok(CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor));
        };
        if record.intent.generation.is_empty()
            || record.intent.marker != identity.marker.as_str()
            || record.intent.uid != identity.uid
            || record.intent.boot_id != identity.boot_id
            || record.intent.pid_namespace != identity.pid_namespace
        {
            return Ok(CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor));
        }
        if let Some(absence) = record.absence {
            if absence.anchor_id != record.intent.anchor_id
                || absence.generation != record.intent.generation
                || absence.boot_id != identity.boot_id
                || absence.pid_namespace != identity.pid_namespace
                || absence.pgid != identity.pgid
                || absence.observed_at.is_empty()
                || linux::boot_id().ok().as_deref() != Some(identity.boot_id.as_str())
                || linux::pid_namespace().ok().as_deref() != Some(identity.pid_namespace.as_str())
            {
                return Ok(CleanupEvidence::Uncertain(CleanupReason::UnverifiedAnchor));
            }
            return Ok(CleanupEvidence::GroupAbsent(crate::GroupAbsenceProof {
                anchor: identity,
                generation: absence.generation,
                observed_at: absence.observed_at,
            }));
        }
        // Round-6 decision 1: only an anchor that ARM may have launched
        // (`arm_intent`) accepts `Stop`. Before ARM the anchor exits on its
        // control's EOF, so reconciliation opens no control connection and
        // only proves absence.
        let armed = match record.phase {
            AnchorPhase::ArmIntent => true,
            AnchorPhase::Intent | AnchorPhase::Identified => false,
        };
        let stopped = timeout_at(deadline.instant(), async {
            if armed
                && let Ok(mut stream) = UnixStream::connect(&record.intent.socket_path).await
                && verify_peer_and_challenge(&mut stream, &identity)
                    .await
                    .is_ok()
            {
                return protocol::transact(
                    &mut stream,
                    &Request::Stop {
                        generation: record.intent.generation.clone(),
                        deadline_monotonic_ns: monotonic_deadline(deadline),
                    },
                    1024,
                )
                .await
                .ok();
            }
            None
        })
        .await;
        // Reconciliation's own Stop evidence counts (runtime §6.2, design
        // §6.8 [r4.2, r6.3]).
        if matches!(stopped, Ok(Some(Reply::Stopping { stopped_live: true }))) {
            self.tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forced
                .insert(record.intent.generation.clone());
        }
        wait_absence(
            &self.journal,
            &record.intent.anchor_id,
            &record.intent.generation,
            &identity,
            deadline,
        )
        .await
    }
}

/// Collects owned task results until the deadline; returns `(pending, failed)`,
/// where `failed` counts every failure collected so far, by any call.
///
/// A task leaves the registry only once its result was collected, so a
/// cancelled caller or an expired deadline never detaches it, and its failure
/// is recorded in Host state before it leaves.
async fn join_owned_tasks(tasks: &Arc<StdMutex<HostTasks>>, deadline: Deadline) -> (usize, usize) {
    let running = tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .running
        .clone();
    for task in running {
        if task.joined.load(Ordering::Acquire) {
            continue;
        }
        // An expired deadline still collects tasks that already finished.
        let handle = if Instant::now() >= deadline.instant() {
            task.handle.try_lock().ok()
        } else {
            timeout_at(deadline.instant(), task.handle.lock())
                .await
                .ok()
        };
        let Some(mut handle) = handle else {
            continue;
        };
        if task.joined.load(Ordering::Acquire) {
            continue;
        }
        let outcome = if handle.is_finished() {
            Ok((&mut *handle).await)
        } else {
            timeout_at(deadline.instant(), &mut *handle).await
        };
        if let Ok(result) = outcome {
            let mut owned = tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            task.joined.store(true, Ordering::Release);
            if !matches!(result, Ok(Ok(()))) {
                owned.failed += 1;
            }
        }
    }
    let mut owned = tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    owned
        .running
        .retain(|task| !task.joined.load(Ordering::Acquire));
    (owned.running.len(), owned.failed)
}

impl ProcessControl {
    /// Returns the verified live anchor identity.
    pub fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    /// Requests shutdown through the live anchor and proves group absence when possible.
    pub async fn close(&self, request: CloseRequest) -> CloseReport {
        if request.mode == CloseMode::Graceful {
            let mut exit = self.exit.clone();
            let force_at = request
                .deadline
                .instant()
                .checked_sub(Duration::from_millis(400))
                .unwrap_or_else(Instant::now);
            wait_graceful_exit(&mut exit, force_at).await;
        }
        let stopping = timeout_at(request.deadline.instant(), async {
            let mut stream = self.stream.lock().await;
            stream
                .transact(
                    &Request::Stop {
                        generation: self.generation.clone(),
                        deadline_monotonic_ns: monotonic_deadline(request.deadline),
                    },
                    1024,
                )
                .await
        })
        .await;
        // Only the anchor knows whether the vendor was still live when its
        // cleanup signalled the group; Host's polled exit watch may be stale.
        let forced = matches!(stopping, Ok(Ok(Reply::Stopping { stopped_live: true })));
        if forced {
            self.stop.forced.store(true, Ordering::Release);
        }
        // A proof that did not commit stays unproven here; an uncertain
        // commit is reported so the daemon latches (design §7.2 row 12).
        let (cleanup, journal_uncertain) = match wait_absence(
            &self.journal,
            &self.anchor_id,
            &self.generation,
            &self.identity,
            request.deadline,
        )
        .await
        {
            Ok(evidence) => (evidence, false),
            Err(error) => (
                CleanupEvidence::Uncertain(CleanupReason::EvidenceStoreFailure),
                matches!(
                    error,
                    HostError::Journal {
                        uncertain: true,
                        ..
                    }
                ),
            ),
        };
        self.capacity.settle(&self.anchor_id, &cleanup);
        CloseReport {
            cleanup,
            vendor_exit: *self.exit.borrow(),
            // The anchor repeats `stopped_live` on every Stop; an earlier
            // early stop's reply counts too (design §6.8 [r5.4]).
            forced: forced || self.stop.forced.load(Ordering::Acquire),
            journal_uncertain,
        }
    }
}

async fn wait_graceful_exit(exit: &mut ExitReceiver, force_at: Instant) {
    while exit.borrow().is_none() && Instant::now() < force_at {
        if !matches!(timeout_at(force_at, exit.changed()).await, Ok(Ok(()))) {
            break;
        }
    }
}

/// The vendor launch configuration, with Host's own random
/// `VIA_PROCESS_MARKER` added to the allow-listed environment (design §9).
fn vendor_config(spec: &PrivateProcessSpec) -> Result<VendorConfig, HostError> {
    let mut vendor_env = spec.env.entries().to_vec();
    vendor_env.push(("VIA_PROCESS_MARKER".into(), linux::random_hex()?.into()));
    Ok(VendorConfig::from_parts(
        &spec.program,
        &spec.args,
        &spec.cwd,
        &vendor_env,
    ))
}

/// Sends the vendor launch configuration through the verified control.
async fn configure(
    control: &Mutex<ControlConnection>,
    vendor: VendorConfig,
) -> Result<(), HostError> {
    let Reply::Configured = control
        .lock()
        .await
        .transact(&Request::Configure { vendor }, 65_536)
        .await?
    else {
        return Err(HostError::Protocol("anchor configuration refused"));
    };
    Ok(())
}

/// Writes the anchor's private bootstrap file, synced, never over another.
fn write_bootstrap(path: &PathBuf, bootstrap: &Bootstrap) -> Result<(), HostError> {
    let bytes = serde_json::to_vec(bootstrap).map_err(io::Error::other)?;
    let mut config = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    config.write_all(&bytes)?;
    config.sync_all()?;
    Ok(())
}

async fn connect_anchor(path: &PathBuf) -> Result<UnixStream, HostError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() >= deadline => return Err(error.into()),
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

fn verify_identity(
    stream: &UnixStream,
    identity: &ProcessIdentity,
    expected_pid: u32,
    intent: &AnchorIntent,
) -> Result<(), HostError> {
    let peer = stream.peer_cred()?;
    if peer.pid() != i32::try_from(expected_pid).ok()
        || peer.uid() != intent.uid
        || identity.pid != expected_pid
        || identity.pgid != expected_pid
        || identity.uid != intent.uid
        || identity.boot_id != intent.boot_id
        || identity.pid_namespace != intent.pid_namespace
        || identity.marker.as_str() != intent.marker
        || identity.start_ticks == 0
    {
        return Err(HostError::Protocol("anchor identity mismatch"));
    }
    let (actual_pgid, actual_ticks) = linux::process_stat(expected_pid)?;
    if actual_pgid != identity.pgid
        || actual_ticks != identity.start_ticks
        || linux::uid_of(expected_pid)? != identity.uid
    {
        return Err(HostError::Protocol("anchor metadata mismatch"));
    }
    Ok(())
}

async fn verify_peer_and_challenge(
    stream: &mut UnixStream,
    identity: &ProcessIdentity,
) -> Result<(), HostError> {
    let peer = stream.peer_cred()?;
    if peer.pid() != i32::try_from(identity.pid).ok() || peer.uid() != identity.uid {
        return Err(HostError::Protocol("recovery peer mismatch"));
    }
    let nonce = linux::random_hex()?;
    let reply = protocol::transact(
        stream,
        &Request::Challenge {
            nonce: nonce.clone(),
            proof: protocol::challenge_proof(identity.marker.as_str(), &nonce),
        },
        1024,
    )
    .await?;
    let Reply::Challenge {
        nonce: returned,
        identity: reported,
    } = reply
    else {
        return Err(HostError::Protocol("challenge refused"));
    };
    let reported = reported.into_public()?;
    if returned != nonce || reported != *identity {
        return Err(HostError::Protocol("challenge identity mismatch"));
    }
    let (pgid, ticks) = linux::process_stat(identity.pid)?;
    if pgid != identity.pgid
        || ticks != identity.start_ticks
        || linux::uid_of(identity.pid)? != identity.uid
        || linux::boot_id()? != identity.boot_id
        || linux::pid_namespace()? != identity.pid_namespace
    {
        return Err(HostError::Protocol("recovery metadata mismatch"));
    }
    Ok(())
}

async fn wait_absence(
    journal: &ProcessJournal,
    anchor_id: &str,
    generation: &str,
    identity: &ProcessIdentity,
    deadline: Deadline,
) -> Result<CleanupEvidence, HostError> {
    loop {
        let evidence = linux::probe_absence(identity, generation);
        match &evidence {
            CleanupEvidence::GroupAbsent(proof) => {
                let record = absence_record(anchor_id, generation, identity, proof);
                // Proof observed too late to record is unproven, not a Store failure.
                if Instant::now() >= deadline.instant() {
                    return Ok(CleanupEvidence::Uncertain(CleanupReason::Deadline));
                }
                #[cfg(feature = "test-failpoints")]
                via_store::failpoint::hit_async("host.recovery.absence_commit")
                    .await
                    .map_err(|_| HostError::Journal {
                        site: JournalSite::Absence,
                        uncertain: false,
                    })?;
                return match timeout_at(deadline.instant(), journal.commit_group_absence(record))
                    .await
                {
                    Ok(outcome) => committed(outcome, JournalSite::Absence).map(|()| evidence),
                    Err(_) => Err(HostError::Journal {
                        site: JournalSite::Absence,
                        uncertain: true,
                    }),
                };
            }
            CleanupEvidence::Uncertain(CleanupReason::GroupPresent)
                if Instant::now() < deadline.instant() =>
            {
                tokio::time::sleep_until(
                    (Instant::now() + Duration::from_millis(20)).min(deadline.instant()),
                )
                .await;
            }
            CleanupEvidence::Uncertain(CleanupReason::GroupPresent) => {
                return Ok(CleanupEvidence::Uncertain(CleanupReason::Deadline));
            }
            CleanupEvidence::Uncertain(_) => return Ok(evidence),
        }
    }
}

/// The durable proof for an absence Host observed, carrying the identity it
/// probed with so an anchor still at `intent` phase records it (§7.2 row 4).
fn absence_record(
    anchor_id: &str,
    generation: &str,
    identity: &ProcessIdentity,
    proof: &crate::GroupAbsenceProof,
) -> GroupAbsenceRecord {
    GroupAbsenceRecord {
        anchor_id: anchor_id.to_owned(),
        generation: generation.to_owned(),
        boot_id: identity.boot_id.clone(),
        pid_namespace: identity.pid_namespace.clone(),
        pgid: identity.pgid,
        observed_at: proof.observed_at().to_owned(),
        identity: Some(identity_to_store(identity)),
    }
}

/// A journal write's value, or its failure classified by outcome.
fn committed<T>(outcome: CommitOutcome<T>, site: JournalSite) -> Result<T, HostError> {
    match outcome {
        CommitOutcome::Committed(value) => Ok(value),
        CommitOutcome::NotCommitted(_) => Err(HostError::Journal {
            site,
            uncertain: false,
        }),
        CommitOutcome::Uncertain(_) => Err(HostError::Journal {
            site,
            uncertain: true,
        }),
    }
}

/// Resolves once `signal` is set; never when its sender is gone unset.
async fn raised(signal: &mut watch::Receiver<bool>) {
    if signal.wait_for(|raised| *raised).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Resolves with the instant the force was raised; never when its sender is
/// gone unraised.
async fn raised_at(signal: &mut watch::Receiver<Option<Instant>>) -> Instant {
    if let Ok(at) = signal.wait_for(Option::is_some).await
        && let Some(at) = *at
    {
        return at;
    }
    std::future::pending().await
}

/// Sends `Stop` through a verified control, bounded by `deadline`; records
/// and returns whether the anchor stopped a live vendor. `Stop` is
/// idempotent and only shortens the anchor's deadline, so a second request
/// through the same control owner changes nothing (runtime §5.1).
async fn stop_through(
    control: &Mutex<ControlConnection>,
    generation: &str,
    stop: &StopFacts,
    deadline: Deadline,
) -> bool {
    let reply = timeout_at(deadline.instant(), async {
        control
            .lock()
            .await
            .transact(
                &Request::Stop {
                    generation: generation.to_owned(),
                    deadline_monotonic_ns: monotonic_deadline(deadline),
                },
                1024,
            )
            .await
    })
    .await;
    let forced = matches!(reply, Ok(Ok(Reply::Stopping { stopped_live: true })));
    if forced {
        stop.forced.store(true, Ordering::Release);
    }
    forced
}

/// One early stop (design §6.8): `Stop` through the live control, then the
/// per-group acknowledgement in test builds.
async fn early_stop(control: LiveControl, deadline: Instant) {
    let Some(stream) = control.stream.upgrade() else {
        return;
    };
    stop_through(
        &stream,
        &control.generation,
        &control.stop,
        Deadline::at(deadline),
    )
    .await;
    drop(stream);
    #[cfg(feature = "test-failpoints")]
    let _ = via_store::failpoint::hit_async("host.early_stop.sent").await;
}

fn identity_to_store(identity: &ProcessIdentity) -> AnchorIdentity {
    AnchorIdentity {
        pid: identity.pid,
        pgid: identity.pgid,
        uid: identity.uid,
        boot_id: identity.boot_id.clone(),
        pid_namespace: identity.pid_namespace.clone(),
        start_ticks: identity.start_ticks,
        marker: identity.marker.as_str().to_owned(),
    }
}

fn identity_from_store(identity: AnchorIdentity) -> Result<ProcessIdentity, HostError> {
    Ok(ProcessIdentity {
        pid: identity.pid,
        pgid: identity.pgid,
        uid: identity.uid,
        boot_id: identity.boot_id,
        pid_namespace: identity.pid_namespace,
        start_ticks: identity.start_ticks,
        marker: crate::ProcessMarker::try_from_generated(identity.marker)
            .map_err(HostError::Invalid)?,
    })
}

fn monotonic_deadline(deadline: Deadline) -> u64 {
    let remaining = deadline.instant().saturating_duration_since(Instant::now());
    monotonic_now()
        .and_then(|now| now.checked_add(u64::try_from(remaining.as_nanos()).ok()?))
        .unwrap_or(u64::MAX)
}

fn monotonic_now() -> Option<u64> {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let seconds = u64::try_from(time.tv_sec).ok()?;
    let nanoseconds = u64::try_from(time.tv_nsec).ok()?;
    seconds.checked_mul(1_000_000_000)?.checked_add(nanoseconds)
}

pub(crate) fn monotonic_remaining(deadline_ns: u64) -> Option<Duration> {
    Some(Duration::from_nanos(
        deadline_ns.saturating_sub(monotonic_now()?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capacity token that records, when dropped, whether the ledger
    /// mutex was held at that moment.
    struct Probe {
        ledger: Capacity,
        locked: Arc<AtomicBool>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            let held = self.ledger.0.try_lock().is_err();
            self.locked.store(held, Ordering::Release);
        }
    }

    /// T3-S3 round 1, decision 7 (design §1): holding an anchor again
    /// replaces its entry, and the replaced token, whose drop takes the
    /// `RecoveredSlots` mutex, is dropped after the ledger guard is gone,
    /// so the two locks are never nested.
    #[test]
    fn a_replaced_token_is_dropped_outside_the_ledger_lock() {
        let capacity = Capacity::default();
        let locked = Arc::new(AtomicBool::new(false));
        let owner = crate::SessionId::try_from("s_0123456789ab")
            .unwrap_or_else(|_| unreachable!("valid session id"));
        let first = Probe {
            ledger: capacity.clone(),
            locked: Arc::clone(&locked),
        };
        capacity.hold("a1".to_owned(), owner.clone(), Box::new(first));
        capacity.hold("a1".to_owned(), owner, Box::new(()));
        assert!(
            !locked.load(Ordering::Acquire),
            "the replaced token was dropped under the ledger mutex"
        );
    }

    #[tokio::test]
    async fn closed_exit_watch_does_not_spin_until_force_deadline() {
        let (sender, mut receiver) = watch::channel(None);
        drop(sender);
        tokio::time::timeout(
            Duration::from_millis(100),
            wait_graceful_exit(&mut receiver, Instant::now() + Duration::from_secs(2)),
        )
        .await
        .expect("closed watch must release graceful wait immediately");
    }

    #[tokio::test]
    async fn cancelling_join_keeps_reaper_owned_for_later_join() {
        let tasks = Arc::new(StdMutex::new(HostTasks::default()));
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let tracked = TrackedTask::new(tokio::spawn(async move {
            let _ = held.await;
            Ok(())
        }));
        tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
            .push(tracked.clone());
        let joining_tasks = tasks.clone();
        let joiner = tokio::spawn(async move {
            join_owned_tasks(
                &joining_tasks,
                Deadline::at(Instant::now() + Duration::from_secs(2)),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while tracked.handle.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown must be actively awaiting the reaper");
        joiner.abort();
        assert!(joiner.await.is_err());
        assert_eq!(
            tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .running
                .len(),
            1,
            "cancelled join cannot detach the retained child reaper"
        );
        release.send(()).expect("held task still owns receiver");
        assert_eq!(
            join_owned_tasks(
                &tasks,
                Deadline::at(Instant::now() + Duration::from_secs(1))
            )
            .await,
            (0, 0)
        );
    }

    /// Host over a private temporary Store; `open` false drops the Store owner
    /// so every journal read fails.
    fn host_fixture(open: bool) -> (Host, Option<via_store::Store>, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "via-host-unit-{}-{}",
            std::process::id(),
            linux::random_hex().unwrap_or_default()
        ));
        let create = |path: &PathBuf| {
            std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
                .create(path)
                .is_ok()
        };
        assert!(create(&root) && create(&root.join("state")) && create(&root.join("anchors")));
        let store = via_store::Store::open(&root.join("state")).ok();
        let journal = store
            .as_ref()
            .map(|store| store.runtime_resources().into_wire_parts().1);
        let host = journal
            .and_then(|journal| {
                Host::new(journal, PathBuf::from("/bin/true"), root.join("anchors")).ok()
            })
            .unwrap_or_else(|| unreachable!("fixture Host must open"));
        (host, open.then_some(store).flatten(), root)
    }

    fn hold(host: &Host) -> tokio::sync::oneshot::Sender<()> {
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        host.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
            .push(TrackedTask::new(tokio::spawn(async move {
                let _ = held.await;
                Ok(())
            })));
        release
    }

    #[tokio::test]
    async fn panicked_or_failed_tasks_count_as_failed_not_joined() {
        let tasks = Arc::new(StdMutex::new(HostTasks::default()));
        let panicked: JoinHandle<TaskResult> =
            tokio::spawn(async { std::panic::panic_any("owned task panicked") });
        // A reaper whose child wait failed reports Err, never success.
        let failed_wait: JoinHandle<TaskResult> = tokio::spawn(async { Err(()) });
        let ok: JoinHandle<TaskResult> = tokio::spawn(async { Ok(()) });
        tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
            .extend([panicked, failed_wait, ok].map(TrackedTask::new));
        let joined = join_owned_tasks(
            &tasks,
            Deadline::at(Instant::now() + Duration::from_secs(1)),
        )
        .await;
        assert_eq!(joined, (0, 2), "panic and failed wait must be failed joins");
    }

    #[tokio::test]
    async fn held_task_past_deadline_is_reported_pending_and_kept_owned() {
        let (host, _store, root) = host_fixture(true);
        let release = hold(&host);
        let report = host
            .shutdown(
                Deadline::at(Instant::now() + Duration::from_millis(50)),
                &[],
            )
            .await;
        assert_eq!((report.pending_tasks, report.failed_tasks), (1, 0));
        assert!(
            matches!(report.failure, Some(HostError::Deadline)),
            "{report:?}"
        );
        // An already expired deadline still returns the report, not only an error.
        let expired = host.shutdown(Deadline::at(Instant::now()), &[]).await;
        assert_eq!(expired.pending_tasks, 1);
        assert!(matches!(expired.failure, Some(HostError::Deadline)));
        assert!(release.send(()).is_ok(), "task must still own its receiver");
        let later = host
            .shutdown(Deadline::at(Instant::now() + Duration::from_secs(1)), &[])
            .await;
        assert_eq!(
            (later.pending_tasks, later.failed_tasks),
            (0, 0),
            "{later:?}"
        );
        assert!(later.failure.is_none(), "{later:?}");
        let _ = fs::remove_dir_all(root);
    }

    /// W1-D Sol finding 5: a failed join collected by one shutdown call stays
    /// in Host state, so every later call still reports it.
    #[tokio::test]
    async fn failed_join_is_reported_by_every_later_shutdown() {
        let (host, _store, root) = host_fixture(true);
        host.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
            .push(TrackedTask::new(tokio::spawn(async { Err(()) })));
        let deadline = || Deadline::at(Instant::now() + Duration::from_secs(1));
        let first = host.shutdown(deadline(), &[]).await;
        assert_eq!(
            (first.pending_tasks, first.failed_tasks),
            (0, 1),
            "{first:?}"
        );
        let second = host.shutdown(deadline(), &[]).await;
        assert_eq!(
            (second.pending_tasks, second.failed_tasks),
            (0, 1),
            "a later shutdown forgot the failed join: {second:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recovery_failure_keeps_pending_owner_in_report() {
        let (host, store, root) = host_fixture(false);
        assert!(store.is_none());
        let release = hold(&host);
        let report = host
            .shutdown(
                Deadline::at(Instant::now() + Duration::from_millis(100)),
                &[],
            )
            .await;
        assert!(
            matches!(report.failure, Some(HostError::StoreUnavailable(_))),
            "{report:?}"
        );
        assert_eq!(
            report.pending_tasks, 1,
            "error path must keep the pending owner"
        );
        drop(release);
        let _ = fs::remove_dir_all(root);
    }
}
