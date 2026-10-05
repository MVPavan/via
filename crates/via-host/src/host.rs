//! Daemon-side anchor launch, durable gate and verified cleanup.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    future::Future,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use process_wrap::tokio::{CommandWrap, ProcessGroup};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    process::{ChildStdin, ChildStdout},
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
    PrivateProcessSpec, ProcessIdentity, ProcessOwner, linux,
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
    /// Sticky: set once any Host journal outcome is uncertain (design item
    /// 2.6), see [`Host::journal_uncertain`].
    uncertain: watch::Sender<bool>,
}

/// Bound on the turn → server-anchor link read at final shutdown, inside
/// its own deadline (design item 6.3).
const LINK_READ: Duration = Duration::from_secs(1);

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
    /// The daemon force watch, set by [`Host::watch_force`] before its task
    /// is spawned. Ledger sections read it (a non-blocking `borrow`, no
    /// await, no other lock) to derive `stopping`, so no phase decision
    /// depends on when the early-stop task runs.
    force: Option<watch::Receiver<Option<Instant>>>,
    /// Sticky: the early stop's deadline (the force's instant plus 3 s),
    /// once any ledger section saw the force. See [`Ledger::stopping`].
    stopping: Option<Instant>,
    /// The controls that were `Armed` at the section that set `stopping`:
    /// the early-stop task's snapshot, taken once by
    /// [`Capacity::begin_stopping`]. Fixing it there keeps each entry with
    /// exactly one owner of its stop, however late the task runs.
    swept: Vec<LiveControl>,
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
    /// The group's exit watch, from `Armed` on: [`Host::live_armed`] reads
    /// it (Task 4 design §11.3).
    exit: Option<watch::Receiver<Option<ExitReport>>>,
    /// A shared server's group (runtime §5 AR6): its live control is not
    /// [`Host::pending_cleanup`], so an idle server never blocks idle exit.
    server: bool,
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
        owner: ProcessOwner,
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

    /// The sticky `stopping` deadline, set by the first ledger section that
    /// finds the force raised, whoever it is: a registration, an ARM gate, a
    /// `Spawned` marking or the early-stop task itself. The deadline is the
    /// force's instant plus 3 s and never comes from the clock here, so a
    /// delayed section gains no fresh budget (design §6.8). The section that
    /// sets it also fixes `swept`, the `Armed` controls it leaves to the
    /// early-stop task; an entry in any other phase is handled by its owner.
    /// Callers that change an entry's phase call this first, so the entry is
    /// never both `swept` and handled by its owner.
    fn stopping(&mut self) -> Option<Instant> {
        if self.stopping.is_none()
            && let Some(at) = self.force.as_ref().and_then(|force| *force.borrow())
        {
            self.stopping = Some(at + EARLY_STOP);
            self.swept = self
                .live
                .values()
                .filter(|control| control.phase == LaunchPhase::Armed)
                .cloned()
                .collect();
        }
        self.stopping
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
    /// The group's owner: a session-filtered re-probe counts and probes
    /// only `Turn` owners of that session [T3-S2 r2.5], never a shared
    /// server (design item 6.1).
    owner: ProcessOwner,
}

impl Capacity {
    fn lock(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn hold(&self, anchor_id: String, owner: ProcessOwner, token: crate::CapacityToken) {
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
    /// the force is already raised, so the caller stops at once. The force
    /// is read in the same section, not left to the early-stop task: a task
    /// that has not run yet cannot let a registration through.
    fn register(&self, anchor_id: &str, control: LiveControl) -> Option<Instant> {
        let mut ledger = self.lock();
        ledger
            .live
            .retain(|_, control| control.stream.strong_count() > 0);
        let stopping = ledger.stopping();
        ledger.live.insert(anchor_id.to_owned(), control);
        stopping
    }

    /// Sets `stopping` if the force is raised and takes the early-stop
    /// task's snapshot: the controls that were `Armed` when it was set, by
    /// this section or an earlier one (design §6.8 [r5.2, r6.1]). Returns
    /// `None` when the force is not raised, which the task's own wake rules
    /// out.
    fn begin_stopping(&self) -> Option<(Instant, Vec<LiveControl>)> {
        let mut ledger = self.lock();
        let deadline = ledger.stopping()?;
        let mut controls = std::mem::take(&mut ledger.swept);
        controls.retain(|control| control.stream.strong_count() > 0);
        Some((deadline, controls))
    }

    /// The early stop's deadline once the force is raised, for the caller's
    /// own stop check.
    fn stopping(&self) -> Option<Instant> {
        self.lock().stopping()
    }

    /// The ARM gate's ledger half: refused with the early stop's deadline
    /// once the force is raised, else the entry is `Arming`. The force is
    /// read in this section, so a force raised before it always refuses ARM.
    fn begin_arming(&self, anchor_id: &str) -> Result<(), Instant> {
        let mut ledger = self.lock();
        if let Some(deadline) = ledger.stopping() {
            return Err(deadline);
        }
        if let Some(control) = ledger.live.get_mut(anchor_id) {
            control.phase = LaunchPhase::Arming;
        }
        Ok(())
    }

    /// Marks the entry `Armed` once the vendor spawned; returns the early
    /// stop's deadline when the force is already raised, in which case the
    /// entry was not `Armed` when `stopping` was set and the owner sends
    /// `Stop` itself. `stopping` is derived before the phase changes, so
    /// this entry is never in the task's snapshot as well.
    fn armed(&self, anchor_id: &str, exit: watch::Receiver<Option<ExitReport>>) -> Option<Instant> {
        let mut ledger = self.lock();
        let stopping = ledger.stopping();
        if let Some(control) = ledger.live.get_mut(anchor_id) {
            control.phase = LaunchPhase::Armed;
            control.exit = Some(exit);
        }
        stopping
    }

    /// [`Host::live_armed`] under the ledger mutex.
    fn live_armed(&self, anchors: &[String]) -> bool {
        let ledger = self.lock();
        anchors.iter().any(|anchor_id| {
            ledger.live.get(anchor_id).is_some_and(|control| {
                control.phase == LaunchPhase::Armed
                    && control.stream.strong_count() > 0
                    && control
                        .exit
                        .as_ref()
                        .is_some_and(|exit| exit.borrow().is_none())
            })
        })
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

impl HostTasks {
    /// Owns `task` until its outcome is collected. Tracking first collects
    /// the tasks that already finished and prunes dropped controls, so live
    /// service keeps these registries bounded (coding style §5).
    fn track(&mut self, task: JoinHandle<TaskResult>) {
        self.collect_finished();
        self.prune_controls();
        self.running.push(TrackedTask::new(task));
    }

    /// Takes the outcome of each finished task without blocking; a failure
    /// joins the sticky `failed` count. A task whose handle a shutdown join
    /// holds is left to that join.
    fn collect_finished(&mut self) {
        let mut context = Context::from_waker(Waker::noop());
        let mut failed = 0;
        self.running.retain(|task| {
            if task.joined.load(Ordering::Acquire) {
                return false;
            }
            let Ok(mut handle) = task.handle.try_lock() else {
                return true;
            };
            if !handle.is_finished() {
                return true;
            }
            let Poll::Ready(result) = Pin::new(&mut *handle).poll(&mut context) else {
                return true;
            };
            task.joined.store(true, Ordering::Release);
            if !matches!(result, Ok(Ok(()))) {
                failed += 1;
            }
            false
        });
        self.failed += failed;
    }

    /// Drops the records of controls whose stream is gone. A dropped
    /// control's forced-stop fact that no close report handed to its owner
    /// moves to `forced`, where shutdown's reconciliation still reads it; a
    /// handed-off one is already the owner's, so live service keeps no fact
    /// per turn. A live control keeps its fact on its own `StopFacts`: its
    /// close may still report it.
    fn prune_controls(&mut self) {
        let forced = &mut self.forced;
        self.controls.retain(|control| {
            if control.stream.strong_count() > 0 {
                return true;
            }
            if control.stop.forced.load(Ordering::Acquire)
                && !control.stop.reported.load(Ordering::Acquire)
            {
                forced.insert(control.generation.clone());
            }
            false
        });
    }
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
    owner: ProcessOwner,
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
    /// A close report carried `forced` to the control's owner.
    reported: AtomicBool,
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
    /// A turn's link to the shared server anchor it runs on, before the
    /// turn's first vendor byte (design item 1).
    Link,
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
    /// The turn's `stderr.log` could not be created (design §7.2): nothing
    /// was committed or launched.
    Evidence(io::Error),
    /// The caller's stop signal was set at the pre-ARM gate: nothing launched.
    Stopped,
    /// Final shutdown could not read the turn → server-anchor links within
    /// its bound (design item 6.3): the requested turns without their own
    /// anchor records stay uncertain. Groups were still stopped.
    LinksUnread,
    /// The anchor directory is too deep for an anchor socket
    /// (`<anchor_dir>/<anchor id>.sock`) to fit the Unix socket path limit
    /// (bead via-dst): Host refuses to start rather than fail every launch.
    AnchorPathTooLong {
        /// The longest anchor socket path Host would bind.
        path: PathBuf,
    },
}

impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "Host I/O: {error}"),
            Self::Evidence(error) => write!(formatter, "stderr.log not created: {error}"),
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
                    JournalSite::Link => "turn link to its server not durably committed",
                })?;
                if *uncertain && *site != JournalSite::Absence {
                    formatter.write_str(" (outcome uncertain)")?;
                }
                Ok(())
            }
            Self::Deadline => formatter.write_str("Host deadline expired"),
            Self::Stopped => formatter.write_str("stopped before ARM"),
            Self::LinksUnread => formatter.write_str("turn links to server anchors unread"),
            Self::AnchorPathTooLong { path } => write!(
                formatter,
                "runtime directory too long: anchor socket path {} is {} bytes, over the Unix \
                 socket path limit; use a shorter runtime directory",
                path.display(),
                path.as_os_str().len()
            ),
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
    /// Owners of proofs observed whose commit was not committed; their
    /// tokens stay held and the next pass retries (design §7.2 row 12).
    /// Core records a `Turn` owner's failure against its session, and a
    /// `Server` owner's with daemon scope (design item 6.1). A pass ends at
    /// its first such proof, so this holds at most one owner, and a pass
    /// that returns an error has none: an error never hides a failed proof.
    pub not_committed: Vec<ProcessOwner>,
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
    reader: protocol::ControlReader,
    /// The stream may hold a partial request or an unread reply: an
    /// exchange was cancelled, timed out or failed. Set when an exchange
    /// starts and cleared once its reply is read, so a cancelled exchange
    /// leaves it set. A retired control is never read again: it is shut
    /// down, so the anchor sees control EOF and cleans up its group
    /// (runtime §5.1), and every later exchange fails at once and its
    /// caller takes its fallback.
    retired: bool,
}

impl ControlConnection {
    /// Admits one exchange, or refuses it on a retired control, which a
    /// cancelled exchange may have left open: it is shut down first.
    async fn begin(&mut self) -> io::Result<()> {
        if self.retired {
            self.retire().await;
            return Err(io::Error::other("anchor control retired"));
        }
        self.retired = true;
        Ok(())
    }

    /// Retires the control and shuts its stream down for writing: the
    /// anchor reads EOF, which is not a `Stop` and yields no forced
    /// evidence. Idempotent; a failed shutdown leaves the EOF to the drop.
    async fn retire(&mut self) {
        self.retired = true;
        let _ = self.stream.shutdown().await;
    }

    async fn transact(&mut self, request: &Request, max: usize) -> io::Result<Reply> {
        self.begin().await?;
        let exchanged = match protocol::write_message(&mut self.stream, request, max).await {
            Ok(()) => self.reader.read(&self.stream).await,
            Err(error) => Err(error),
        };
        match exchanged {
            Ok(Some(reply)) => {
                self.retired = false;
                Ok(reply)
            }
            Ok(None) => {
                self.retire().await;
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "anchor closed control",
                ))
            }
            Err(error) => {
                self.retire().await;
                Err(error)
            }
        }
    }

    /// Like [`Self::transact`], but only the wait for the reply ends at
    /// `deadline`: the request is written first, whether or not the
    /// deadline has passed, so a caller past its deadline still delivers it.
    /// A reply in hand only at or after `deadline` is late and is refused:
    /// Tokio polls the read before its timer, so a task that runs late would
    /// otherwise be handed a ready reply as if it were in time. A reply read,
    /// late or not, completes the exchange; no reply retires the control.
    async fn transact_by(
        &mut self,
        request: &Request,
        max: usize,
        deadline: Instant,
    ) -> io::Result<Reply> {
        self.begin().await?;
        if let Err(error) = protocol::write_message(&mut self.stream, request, max).await {
            self.retire().await;
            return Err(error);
        }
        let late = || io::Error::new(io::ErrorKind::TimedOut, "no reply by the deadline");
        let reply = match timeout_at(deadline, self.reader.read(&self.stream)).await {
            Ok(Ok(Some(reply))) => reply,
            Ok(Ok(None)) => {
                self.retire().await;
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "anchor closed control",
                ));
            }
            Ok(Err(error)) => {
                self.retire().await;
                return Err(error);
            }
            Err(_) => {
                self.retire().await;
                return Err(late());
            }
        };
        self.retired = false;
        if Instant::now() >= deadline {
            return Err(late());
        }
        Ok(reply)
    }
}

/// Verified live-anchor control capability; never selects an arbitrary PID.
#[derive(Clone)]
pub struct ProcessControl {
    stream: Arc<Mutex<ControlConnection>>,
    owner: ProcessOwner,
    identity: ProcessIdentity,
    anchor_id: String,
    generation: String,
    journal: ProcessJournal,
    exit: ExitReceiver,
    stop: Arc<StopFacts>,
    capacity: Capacity,
    uncertain: watch::Sender<bool>,
    /// Where the anchor's socket is, removed once its group is proved
    /// absent (bead via-c30).
    anchor_dir: PathBuf,
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
    /// The verified anchor's `Stopping { stopped_live }` reply to this
    /// close's own `Stop` (runtime §5 stop reply), or `None` when no valid
    /// reply arrived in time (lost, invalid, or past the deadline). Passive
    /// evidence beside `forced`, which it does not change.
    pub stopped_live: Option<bool>,
}

/// Recovery result for one committed anchor intent.
#[derive(Debug)]
pub struct RecoveryReport {
    /// Durable internal anchor identifier.
    pub anchor_id: String,
    /// Immutable launch generation used to correlate the durable proof.
    pub generation: String,
    /// Owner, the turn or the shared server, for passive Core correlation.
    pub owner: ProcessOwner,
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

/// Folds one anchor's facts into the requested turn's aggregate in
/// `recovery`, adding it on its first anchor.
fn fold_turn(
    recovery: &mut Vec<TurnRecovery>,
    (session, turn): (crate::SessionId, crate::TurnNumber),
    cleanup: CleanupEvidence,
    forced: bool,
) {
    match recovery
        .iter_mut()
        .find(|entry| entry.owner_session == session && entry.owner_turn == turn)
    {
        Some(entry) => {
            entry.anchors += 1;
            entry.forced |= forced;
            if matches!(entry.cleanup, CleanupEvidence::GroupAbsent(_)) {
                entry.cleanup = cleanup;
            }
        }
        None => recovery.push(TurnRecovery {
            owner_session: session,
            owner_turn: turn,
            anchors: 1,
            cleanup,
            forced,
        }),
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
        anchor_socket_fits(&anchor_dir)?;
        Ok(Self {
            journal,
            anchor_binary,
            anchor_dir,
            tasks: Arc::new(StdMutex::new(HostTasks::default())),
            capacity: Capacity::default(),
            retire: watch::Sender::new(false),
            uncertain: watch::Sender::new(false),
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
                let evidence = match wait_absence(
                    (&self.journal, &self.uncertain),
                    anchor_id,
                    generation,
                    identity,
                    cleanup,
                )
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
                settle(&self.capacity, &self.anchor_dir, anchor_id, &evidence);
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

    /// Awaits `write` where this Host observes it ([`observed`]), then
    /// [`committed`] with this Host's journal-uncertain watch.
    async fn commit<T>(
        &self,
        write: impl Future<Output = CommitOutcome<T>>,
        site: JournalSite,
    ) -> Result<T, HostError> {
        committed(
            observed(write, &self.uncertain).await,
            site,
            &self.uncertain,
        )
    }

    /// Holds capacity for a group this Host did not launch, such as one an
    /// earlier daemon left whose absence recovery did not prove, owned by
    /// `owner` (a turn or a shared server); a later absence proof for
    /// `anchor_id` releases it.
    pub fn hold_capacity(
        &self,
        anchor_id: String,
        owner: ProcessOwner,
        token: crate::CapacityToken,
    ) {
        self.capacity.hold(anchor_id, owner, token);
    }

    /// Sticky: becomes `true` once any Host journal operation's outcome is
    /// uncertain, whatever its owner and whether or not a requester still
    /// waits (design item 2.6; runtime §5 AR6). Core latches Store failure
    /// on it.
    pub fn journal_uncertain(&self) -> watch::Receiver<bool> {
        self.uncertain.subscribe()
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
    /// for an earlier daemon do not count, nor does a live shared server's
    /// control: an idle server does not block idle exit, and final shutdown
    /// stops it (runtime §5 AR6).
    pub fn pending_cleanup(&self) -> usize {
        let ledger = self.capacity.lock();
        let live = ledger
            .live
            .iter()
            .filter(|(anchor_id, control)| {
                !control.server
                    && control.stream.strong_count() > 0
                    && !ledger.acquiring.contains(*anchor_id)
            })
            .count();
        live + ledger.acquiring.len()
    }

    /// Test builds: how many owned tasks, live-control records and
    /// Host-wide forced-stop facts Host still keeps, as
    /// `(tasks, controls, forced)`.
    #[cfg(feature = "test-failpoints")]
    #[doc(hidden)]
    pub fn tracked(&self) -> (usize, usize, usize) {
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            tasks.running.len(),
            tasks.controls.len(),
            tasks.forced.len(),
        )
    }

    /// Positive evidence that a vendor of one of `anchors` is live (Task 4
    /// design §11.3 `process.alive`): its verified control is held, phase
    /// `Armed`, and its exit watch has not reported an exit.
    pub fn live_armed(&self, anchors: &[String]) -> bool {
        self.capacity.live_armed(anchors)
    }

    /// Subscribes Host's early-stop task to the daemon force signal (design
    /// §6.8 [r4.3, r5.1–r5.4]). On the signal it stops every live group in
    /// the ledger through its verified control, concurrently, each bounded
    /// at the signal time plus 3 s, recording the anchor's `stopped_live`
    /// in the control's stop facts; it polls no caller and commits nothing.
    /// The signal carries the instant Core raised the force (`None` until
    /// then), and that instant, not the one this task runs at, anchors the
    /// bound and every stop after it.
    ///
    /// The ledger holds the same signal, and its sections derive `stopping`
    /// from it (registration, the ARM gate, the `Spawned` marking), so a
    /// group is covered from the moment the force is raised, whether or not
    /// this task has run. The task stops the entries that were `Armed` when
    /// `stopping` was set; an owner stops an entry that became `Armed`
    /// after. The task is Host-owned: [`Host::shutdown`] retires it when
    /// force never came.
    pub fn watch_force(&self, mut forced: watch::Receiver<Option<Instant>>) {
        let ledger = self.capacity.clone();
        ledger.lock().force = Some(forced.clone());
        let mut retire = self.retire.subscribe();
        let task = tokio::spawn(async move {
            tokio::select! {
                biased;
                () = force_raised(&mut forced) => {}
                () = raised(&mut retire) => return Ok(()),
            }
            // Test builds: the force woke this task, which has taken nothing
            // yet; a pause here delays the task as a blocked runtime would.
            #[cfg(feature = "test-failpoints")]
            let _ = via_store::failpoint::hit_async("host.early_stop.woken").await;
            let Some((deadline, controls)) = ledger.begin_stopping() else {
                return Ok(());
            };
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
            .track(task);
    }

    /// Design §8: one non-signalling pass over held groups with no live
    /// control, optionally only `owner`'s. Each is probed with its durable
    /// full identity, or the in-memory one Host verified (§7.2 row 4); a
    /// same-boot `ESRCH` commits the proof, with that identity, and releases
    /// the token. No `Stop`, `Challenge` or other mutation is sent. A proof
    /// commit that is not committed keeps the token for the next pass and
    /// ends this one, reported in [`ReprobeReport::not_committed`]: an error
    /// later in the pass would replace the report and lose the failure. An
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
        let mut remaining: HashMap<String, (Option<(ProcessIdentity, String)>, ProcessOwner)> = {
            let ledger = self.capacity.lock();
            ledger
                .held
                .iter()
                .filter(|(anchor_id, held)| {
                    !ledger.busy(anchor_id)
                        && owner.as_ref().is_none_or(|owner| match &held.owner {
                            ProcessOwner::Turn { session_id, .. } => session_id == owner,
                            // A session's re-probe never waits on a shared
                            // server (design item 6.1).
                            ProcessOwner::Server { .. } => false,
                        })
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
                    Reprobed::NotCommitted => {
                        report.not_committed.push(owner);
                        return Ok(report);
                    }
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
            observed(self.journal.commit_group_absence(absence), &self.uncertain),
        )
        .await
        {
            Ok(CommitOutcome::Committed(())) => {
                settle(
                    &self.capacity,
                    &self.anchor_dir,
                    &record.intent.anchor_id,
                    &CleanupEvidence::GroupAbsent(proof),
                );
                Ok(Reprobed::Proved)
            }
            Ok(CommitOutcome::NotCommitted(_)) => Ok(Reprobed::NotCommitted),
            Ok(CommitOutcome::Uncertain(_)) | Err(_) => {
                self.uncertain.send_replace(true);
                Err(HostError::Journal {
                    site: JournalSite::Absence,
                    uncertain: true,
                })
            }
        }
    }

    async fn start_anchor(
        &self,
        owner: ProcessOwner,
        capacity: Option<crate::CapacityToken>,
        stderr: fs::File,
        state: &mut Acquisition,
    ) -> Result<StartedAnchor, HostError> {
        let anchor_id = linux::random_hex()?;
        let generation = linux::random_hex()?;
        let marker = linux::random_hex()?;
        let socket_path = anchor_socket(&self.anchor_dir, &anchor_id);
        let config_path = self.anchor_dir.join(format!("{anchor_id}.json"));
        let intent = AnchorIntent {
            anchor_id: anchor_id.clone(),
            generation: generation.clone(),
            marker: marker.clone(),
            socket_path: socket_path.clone(),
            owner: owner.clone(),
            uid: rustix::process::getuid().as_raw(),
            boot_id: linux::boot_id()?,
            pid_namespace: linux::pid_namespace()?,
        };
        let receipt = self
            .commit(
                self.journal.commit_anchor_intent(intent.clone()),
                JournalSite::AnchorIntent,
            )
            .await?;
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
        let (pipes, anchor_process_id) = self.spawn_anchor(&config_path, stderr)?;
        // The group exists from here: its capacity stays with Host until
        // absence is proved. A failure above dropped it with no group.
        let replaced = {
            let mut ledger = self.capacity.lock();
            ledger.acquiring.insert(anchor_id.clone());
            capacity.and_then(|token| ledger.hold(anchor_id.clone(), owner.clone(), token))
        };
        drop(replaced);
        state.spawned = Some(anchor_id.clone());
        let mut stream = connect_anchor(&socket_path).await?;
        let ready = protocol::read_message::<Reply>(&mut stream, 1024)
            .await?
            .ok_or(HostError::Protocol("anchor did not become ready"))?;
        let Reply::Ready {
            identity: wire_identity,
        } = ready
        else {
            return Err(HostError::Protocol("unexpected anchor ready message"));
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
            reader: protocol::ControlReader::new(1024),
            retired: false,
        }));
        let stop = Arc::new(StopFacts::default());
        let registered = self.capacity.register(
            &anchor_id,
            LiveControl {
                stream: Arc::downgrade(&control),
                generation: generation.clone(),
                stop: stop.clone(),
                phase: LaunchPhase::Verified,
                exit: None,
                server: matches!(owner, ProcessOwner::Server { .. }),
            },
        );
        if let Some(deadline) = registered {
            state.stop_early(deadline);
            return Err(HostError::Stopped);
        }
        let identified = self.journal.commit_anchor_identified(
            &anchor_id,
            &generation,
            receipt.record_version,
            identity_to_store(&identity),
        );
        let version = self.commit(identified, JournalSite::Identified).await?;
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

    /// Spawns the anchor with `stderr` as its standard error, which the
    /// vendor inherits (design §7.2); only stdin and stdout are pipes.
    fn spawn_anchor(
        &self,
        config_path: &PathBuf,
        stderr: fs::File,
    ) -> Result<(OwnedPipes, u32), HostError> {
        let mut command = CommandWrap::with_new(&self.anchor_binary, |command| {
            command
                .arg("__via_host_anchor")
                .arg(config_path)
                .env_clear()
                .current_dir(&self.anchor_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(stderr);
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
        };
        // Reap the anchor regardless of later journal/control failures; a failed
        // wait is a failed task, never successful reaping.
        let task = tokio::spawn(async move { anchor.wait().await.map(drop).map_err(drop) });
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .track(task);
        Ok((pipes, anchor_process_id))
    }

    async fn acquire_inner(
        &self,
        mut spec: PrivateProcessSpec,
        launch: &LaunchPipes,
        stopped: &(dyn Fn() -> bool + Send + Sync),
        state: &mut Acquisition,
    ) -> Result<AcquiredProcess, HostError> {
        // Before the anchor intent: a file that cannot be created leaves
        // nothing committed and nothing launched (design §7.2).
        let stderr = open_stderr(&spec.stderr_path).map_err(HostError::Evidence)?;
        let StartedAnchor {
            anchor_id,
            generation,
            identity,
            control,
            stop,
            pipes,
            version,
        } = self
            .start_anchor(spec.owner.clone(), spec.capacity.take(), stderr, state)
            .await?;
        let vendor = vendor_config(&spec)?;
        configure(&control, vendor).await?;
        let arm = self
            .journal
            .commit_arm_intent(&anchor_id, &generation, version);
        self.commit(arm, JournalSite::ArmIntent).await?;
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
        // The group's exit watch, which the ledger's `Armed` entry reads.
        let (sender, exits) = watch::channel(None);
        // Design §6.8 [r6.1]: armed right after `Spawned`; an early stop
        // that already took its snapshot missed this group, so the owner
        // stops it under the original deadline.
        if let Some(deadline) = self.capacity.armed(&anchor_id, exits.clone()) {
            let deadline = state.stop_early(deadline);
            state.forced = stop_through(&control, &generation, &stop, deadline).await;
            #[cfg(feature = "test-failpoints")]
            let _ = via_store::failpoint::hit_async("host.early_stop.sent").await;
            return Err(HostError::Stopped);
        }
        if let Err(error) = self
            .commit(
                self.journal
                    .commit_vendor_facts(&anchor_id, &generation, vendor_pid),
                JournalSite::VendorFacts,
            )
            .await
        {
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
        let control = ProcessControl {
            stream: control,
            owner: spec.owner,
            identity,
            anchor_id,
            generation,
            journal: self.journal.clone(),
            exit: exits.clone(),
            stop,
            capacity: self.capacity.clone(),
            uncertain: self.uncertain.clone(),
            anchor_dir: self.anchor_dir.clone(),
        };
        self.track_control(&control, sender);
        Ok(AcquiredProcess {
            pipes,
            control,
            exits,
        })
    }

    fn track_control(&self, control: &ProcessControl, sender: watch::Sender<Option<ExitReport>>) {
        let task = tokio::spawn(supervise_exit(
            Arc::downgrade(&control.stream),
            control.generation.clone(),
            sender,
        ));
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.track(task);
        tasks.controls.push(TrackedControl {
            stream: Arc::downgrade(&control.stream),
            owner: control.owner.clone(),
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
            tasks.prune_controls();
            // Shutdown's reconciliation reads the facts of the controls
            // still live, whether or not their closes finish in time.
            let HostTasks {
                controls, forced, ..
            } = &mut *tasks;
            for control in controls.iter() {
                if control.stop.forced.load(Ordering::Acquire) {
                    forced.insert(control.generation.clone());
                }
            }
            controls.clone()
        };
        for tracked in controls {
            if Instant::now() >= deadline.instant() {
                break;
            }
            if let Some(stream) = tracked.stream.upgrade() {
                let control = ProcessControl {
                    stream,
                    owner: tracked.owner,
                    identity: tracked.identity,
                    anchor_id: tracked.anchor_id,
                    generation: tracked.generation,
                    journal: self.journal.clone(),
                    exit: tracked.exit,
                    stop: tracked.stop,
                    capacity: self.capacity.clone(),
                    uncertain: self.uncertain.clone(),
                    anchor_dir: self.anchor_dir.clone(),
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
        // Design item 6.3: every control is closed first, so a failed or
        // slow link read never delays stopping a group.
        let links = self.read_links(turns, deadline).await;
        let (recovery, anchors, uncertain_anchors, failure) = match self
            .reconcile_turns(turns, links.as_ref().ok(), deadline)
            .await
        {
            Ok((recovery, anchors, uncertain)) => (recovery, anchors, uncertain, links.err()),
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

    /// The links of `turns` to server anchors, by anchor id, read in pages
    /// of [`via_store::SERVER_LINKS_LIMIT`] under `min(deadline, now + 1
    /// s)` (design item 6.3). A failed or timed-out read is
    /// [`HostError::LinksUnread`].
    async fn read_links(
        &self,
        turns: &[(crate::SessionId, crate::TurnNumber)],
        deadline: Deadline,
    ) -> Result<HashMap<String, Vec<(crate::SessionId, crate::TurnNumber)>>, HostError> {
        let by = deadline.instant().min(Instant::now() + LINK_READ);
        let read = async {
            let mut links: HashMap<String, Vec<_>> = HashMap::new();
            for chunk in turns.chunks(via_store::SERVER_LINKS_LIMIT) {
                for link in self.journal.server_links(chunk.to_vec()).await? {
                    links
                        .entry(link.anchor_id)
                        .or_default()
                        .push((link.session_id, link.turn));
                }
            }
            Ok::<_, StoreFailureKind>(links)
        };
        match timeout_at(by, read).await {
            Ok(Ok(links)) => Ok(links),
            Ok(Err(_)) | Err(_) => Err(HostError::LinksUnread),
        }
    }

    /// Reconciles every committed anchor page by page, keeping per-turn
    /// aggregates only for `turns` plus totals; returns `(turns, anchors,
    /// uncertain anchors)`. A server anchor's cleanup, never its `forced`,
    /// is folded into each requested turn `links` names (design item 6.3);
    /// with no links read, nothing is folded.
    async fn reconcile_turns(
        &self,
        turns: &[(crate::SessionId, crate::TurnNumber)],
        links: Option<&HashMap<String, Vec<(crate::SessionId, crate::TurnNumber)>>>,
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
                match &report.owner {
                    ProcessOwner::Turn { session_id, turn } => {
                        let owner = (session_id.clone(), *turn);
                        if turns.contains(&owner) {
                            fold_turn(&mut recovery, owner, report.cleanup, report.forced);
                        }
                    }
                    ProcessOwner::Server { .. } => {
                        for owner in links
                            .and_then(|links| links.get(&report.anchor_id))
                            .into_iter()
                            .flatten()
                        {
                            // Cleanup only: a server's `forced` is never a
                            // turn's.
                            fold_turn(&mut recovery, owner.clone(), report.cleanup.clone(), false);
                        }
                    }
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
            let owner = record.intent.owner.clone();
            let cleanup = self.recover_one(record, deadline).await?;
            settle(&self.capacity, &self.anchor_dir, &anchor_id, &cleanup);
            let forced = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forced
                .contains(&generation);
            results.push(RecoveryReport {
                anchor_id,
                generation,
                owner,
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
            (&self.journal, &self.uncertain),
            &record.intent.anchor_id,
            &record.intent.generation,
            &identity,
            deadline,
        )
        .await
    }
}

/// How long an admitted `Status` exchange may wait for its reply before the
/// control is retired. A52 measured anchor control replies of 115 ms or less
/// under the F24 flood.
const STATUS_REPLY_BOUND: Duration = Duration::from_secs(1);

/// Unit tests: exit-poll ticks that found the control busy.
#[cfg(test)]
static BUSY_SKIPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Polls `stream` for the vendor's exit and publishes it on `sender`. A
/// control busy with another exchange only skips a tick. Supervision ends
/// once the exit is seen, the stream is gone, or the control is retired: a
/// `Status` exchange that failed or had no reply within
/// [`STATUS_REPLY_BOUND`] retires it and shuts it down, as does an exchange
/// of another holder.
/// Only that last end, a real loss of control, drops `sender` under a live
/// turn, which Wire reports as a transport failure.
async fn supervise_exit(
    poll_stream: Weak<Mutex<ControlConnection>>,
    poll_generation: String,
    sender: watch::Sender<Option<ExitReport>>,
) -> TaskResult {
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let Some(stream) = poll_stream.upgrade() else {
            break;
        };
        let Ok(mut control) = stream.try_lock() else {
            #[cfg(test)]
            BUSY_SKIPS.fetch_add(1, Ordering::AcqRel);
            continue;
        };
        let request = Request::Status {
            generation: poll_generation.clone(),
        };
        // Cancelling the exchange at its bound leaves the control retired.
        let reply =
            tokio::time::timeout(STATUS_REPLY_BOUND, control.transact(&request, 1024)).await;
        let Ok(Ok(Reply::Status {
            exit_code,
            exit_signal,
            ..
        })) = reply
        else {
            control.retire().await;
            break;
        };
        let report = (exit_code.is_some() || exit_signal.is_some()).then_some(ExitReport {
            code: exit_code,
            signal: exit_signal,
        });
        drop(control);
        drop(stream);
        if let Some(report) = report {
            sender.send_replace(Some(report));
            break;
        }
    }
    Ok(())
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
        let stopped_live = stop_report(&self.stream, &self.generation, request.deadline).await;
        let forced = stopped_live == Some(true);
        if forced {
            self.stop.forced.store(true, Ordering::Release);
        }
        // A proof that did not commit stays unproven here; an uncertain
        // commit is reported so the daemon latches (design §7.2 row 12).
        let (cleanup, journal_uncertain) = match wait_absence(
            (&self.journal, &self.uncertain),
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
        settle(&self.capacity, &self.anchor_dir, &self.anchor_id, &cleanup);
        // The anchor repeats `stopped_live` on every Stop; an earlier early
        // stop's reply counts too (design §6.8 [r5.4]).
        let forced = forced || self.stop.forced.load(Ordering::Acquire);
        if forced {
            self.stop.reported.store(true, Ordering::Release);
        }
        CloseReport {
            cleanup,
            vendor_exit: *self.exit.borrow(),
            forced,
            journal_uncertain,
            stopped_live,
        }
    }

    /// Commits the link of the `running` turn `(session, turn)` to this
    /// shared server's anchor (design item 1, runtime §6 `server_turns`),
    /// before the turn's first vendor byte. A turn-owned control has no
    /// link: [`HostError::Invalid`]. Not committed is
    /// [`HostError::Journal`] at [`JournalSite::Link`]; an uncertain
    /// outcome, or no outcome by `deadline`, is the same with `uncertain`,
    /// and sets Host's journal-uncertain watch, as does dropping this future
    /// once its write is enqueued ([`observed`]).
    pub async fn link_turn(
        &self,
        session: &crate::SessionId,
        turn: crate::TurnNumber,
        deadline: Deadline,
    ) -> Result<(), HostError> {
        match &self.owner {
            ProcessOwner::Turn { .. } => {
                return Err(HostError::Invalid("a turn-owned process has no turn links"));
            }
            ProcessOwner::Server { .. } => {}
        }
        if Instant::now() >= deadline.instant() {
            return Err(HostError::Deadline);
        }
        let link = observed(
            self.journal
                .commit_server_turn(&self.anchor_id, session, turn),
            &self.uncertain,
        );
        let Ok(outcome) = timeout_at(deadline.instant(), link).await else {
            return Err(no_outcome(JournalSite::Link, &self.uncertain));
        };
        committed(outcome, JournalSite::Link, &self.uncertain)
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
    Ok(vendor_config_with(
        (&spec.program, &spec.args, &spec.cwd),
        &spec.env,
        linux::random_hex()?,
    ))
}

/// The vendor launch configuration with `marker` as `VIA_PROCESS_MARKER`.
fn vendor_config_with(
    (program, args, cwd): (&std::path::Path, &[std::ffi::OsString], &std::path::Path),
    env: &crate::EnvAllowList,
    marker: String,
) -> VendorConfig {
    let mut vendor_env = env.entries().to_vec();
    vendor_env.push(("VIA_PROCESS_MARKER".into(), marker.into()));
    VendorConfig::from_parts(program, args, cwd, &vendor_env)
}

impl PrivateProcessSpec {
    /// Whether a launch of `program` with `args` in `cwd` under `env`
    /// fits the anchor's control request cap: its encoded `Configure`
    /// request, with Host's process marker at the marker's fixed length
    /// and its largest encoding, is at most that cap. A launch that does
    /// not fit is refused by the anchor's control, after acquisition
    /// began; a caller checks first.
    pub fn configure_fits(
        program: &std::path::Path,
        args: &[std::ffi::OsString],
        cwd: &std::path::Path,
        env: &crate::EnvAllowList,
    ) -> bool {
        let marker = "f".repeat(linux::RANDOM_HEX_LEN);
        let vendor = vendor_config_with((program, args, cwd), env, marker);
        serde_json::to_vec(&Request::Configure { vendor })
            .is_ok_and(|bytes| bytes.len() <= protocol::REQUEST_MAX)
    }
}

/// Sends the vendor launch configuration through the verified control.
async fn configure(
    control: &Mutex<ControlConnection>,
    vendor: VendorConfig,
) -> Result<(), HostError> {
    let Reply::Configured = control
        .lock()
        .await
        .transact(&Request::Configure { vendor }, protocol::REQUEST_MAX)
        .await?
    else {
        return Err(HostError::Protocol("anchor configuration refused"));
    };
    Ok(())
}

/// Creates the turn's `stderr.log` (design §7.2): new, 0600, never through a
/// symlink. The operating system writes it; VIA never reads it.
fn open_stderr(path: &std::path::Path) -> io::Result<fs::File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
}

/// The anchor socket of `anchor_id` (runtime §6.1 `anchors/<anchor-id>.sock`).
fn anchor_socket(anchor_dir: &std::path::Path, anchor_id: &str) -> PathBuf {
    anchor_dir.join(format!("{anchor_id}.sock"))
}

/// Settles `anchor_id`'s group in the ledger ([`Capacity::settle`]) and,
/// once its absence is committed, removes its socket (bead via-c30): an
/// armed anchor ends by its own group KILL and never unlinks it. Only a
/// proved-absent anchor's socket is removed; nothing is swept by name.
fn settle(
    capacity: &Capacity,
    anchor_dir: &std::path::Path,
    anchor_id: &str,
    cleanup: &CleanupEvidence,
) {
    capacity.settle(anchor_id, cleanup);
    if matches!(cleanup, CleanupEvidence::GroupAbsent(_)) {
        remove_anchor_socket(anchor_dir, anchor_id);
    }
}

/// Removes a proved-absent anchor's socket, if it is still there. The id
/// comes from the journal: only one of Host's own fixed-length hex ids
/// names a path, and only a socket there is removed.
fn remove_anchor_socket(anchor_dir: &std::path::Path, anchor_id: &str) {
    if anchor_id.len() != linux::RANDOM_HEX_LEN
        || !anchor_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return;
    }
    let path = anchor_socket(anchor_dir, anchor_id);
    if fs::symlink_metadata(&path)
        .is_ok_and(|metadata| std::os::unix::fs::FileTypeExt::is_socket(&metadata.file_type()))
    {
        // Best effort: a socket left behind only refuses connections, and
        // its anchor is gone; the next proof of it (startup reconciliation)
        // tries again.
        let _ = fs::remove_file(&path);
    }
}

/// Every anchor socket has the same length, its id being
/// [`linux::RANDOM_HEX_LEN`] hex digits: one that fits the platform's Unix
/// socket address means all do (bead via-dst).
fn anchor_socket_fits(anchor_dir: &std::path::Path) -> Result<(), HostError> {
    let path = anchor_socket(anchor_dir, &"f".repeat(linux::RANDOM_HEX_LEN));
    match std::os::unix::net::SocketAddr::from_pathname(&path) {
        Ok(_) => Ok(()),
        Err(_) => Err(HostError::AnchorPathTooLong { path }),
    }
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
    (journal, uncertain): (&ProcessJournal, &watch::Sender<bool>),
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
                let commit = observed(journal.commit_group_absence(record), uncertain);
                let Ok(outcome) = timeout_at(deadline.instant(), commit).await else {
                    return Err(no_outcome(JournalSite::Absence, uncertain));
                };
                return committed(outcome, JournalSite::Absence, uncertain).map(|()| evidence);
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

/// A journal write's value, or its failure classified by outcome. An
/// uncertain outcome also sets Host's sticky journal-uncertain watch here,
/// where Host observes it (design item 2.6).
fn committed<T>(
    outcome: CommitOutcome<T>,
    site: JournalSite,
    uncertain: &watch::Sender<bool>,
) -> Result<T, HostError> {
    match outcome {
        CommitOutcome::Committed(value) => Ok(value),
        CommitOutcome::NotCommitted(_) => Err(HostError::Journal {
            site,
            uncertain: false,
        }),
        CommitOutcome::Uncertain(_) => {
            uncertain.send_replace(true);
            Err(HostError::Journal {
                site,
                uncertain: true,
            })
        }
    }
}

/// Awaits a journal write's outcome where Host observes it (design item
/// 2.6). The write's command is enqueued at its first poll; dropped after
/// that and before its outcome (the requester was cancelled, or a deadline
/// such as acquisition's cut it), the outcome is never observed and may be
/// a commit, so the watch is set, as for [`no_outcome`].
async fn observed<T>(
    write: impl Future<Output = CommitOutcome<T>>,
    uncertain: &watch::Sender<bool>,
) -> CommitOutcome<T> {
    let mut pending = Unobserved(Some(uncertain));
    let outcome = write.await;
    pending.0 = None;
    outcome
}

/// Sets the journal-uncertain watch when dropped still armed: see
/// [`observed`].
struct Unobserved<'a>(Option<&'a watch::Sender<bool>>);

impl Drop for Unobserved<'_> {
    fn drop(&mut self) {
        if let Some(uncertain) = self.0.take() {
            uncertain.send_replace(true);
        }
    }
}

/// A journal write with no outcome by its deadline: it may have committed,
/// so the watch is set as for an uncertain one.
fn no_outcome(site: JournalSite, uncertain: &watch::Sender<bool>) -> HostError {
    uncertain.send_replace(true);
    HostError::Journal {
        site,
        uncertain: true,
    }
}

/// Resolves once `signal` is set; never when its sender is gone unset.
async fn raised(signal: &mut watch::Receiver<bool>) {
    if signal.wait_for(|raised| *raised).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Resolves once the force is raised; never when its sender is gone
/// unraised.
async fn force_raised(signal: &mut watch::Receiver<Option<Instant>>) {
    if signal.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Sends `Stop` through a verified control; records and returns whether the
/// anchor stopped a live vendor. The one write is attempted whatever the
/// clock says: a `deadline` already past (a task or owner that ran late)
/// still gets its `Stop`, which the anchor honours at once, since it caps its
/// grace at the time left. Only the wait for the reply is bounded by
/// `deadline`, with no fresh allowance, so a late `Stop` whose reply is not
/// in hand before then records no forced evidence, including a reply that was
/// already waiting when the late task first polled it. The lock and the write are
/// not bounded by it: the control's other holders each run under their own
/// deadline, and a `Stop` message is far smaller than the socket buffer.
/// `Stop` is idempotent and only shortens the anchor's deadline, so a second
/// request through the same control owner changes nothing (runtime §5.1).
/// A retired control writes nothing and records no forced evidence; a
/// `Stop` whose reply missed `deadline` retires it.
async fn stop_through(
    control: &Mutex<ControlConnection>,
    generation: &str,
    stop: &StopFacts,
    deadline: Deadline,
) -> bool {
    let mut stream = control.lock().await;
    let request = Request::Stop {
        generation: generation.to_owned(),
        deadline_monotonic_ns: monotonic_deadline(deadline),
    };
    let reply = stream.transact_by(&request, 1024, deadline.instant()).await;
    drop(stream);
    let forced = matches!(reply, Ok(Reply::Stopping { stopped_live: true }));
    if forced {
        stop.forced.store(true, Ordering::Release);
    }
    forced
}

/// The close's `Stop` and the anchor's reply to it (runtime §5): only the
/// anchor knows whether the vendor was still live when its cleanup
/// signalled the group; Host's polled exit watch may be stale. The lock,
/// the write and the reply are all bounded by `deadline`; no reply by then,
/// or one in hand only at or after it ([`ControlConnection::transact_by`]'s
/// late check), is `None`.
async fn stop_report(
    control: &Mutex<ControlConnection>,
    generation: &str,
    deadline: Deadline,
) -> Option<bool> {
    let stopping = timeout_at(deadline.instant(), async {
        let mut stream = control.lock().await;
        stream
            .transact_by(
                &Request::Stop {
                    generation: generation.to_owned(),
                    deadline_monotonic_ns: monotonic_deadline(deadline),
                },
                1024,
                deadline.instant(),
            )
            .await
    })
    .await;
    // A `Stop` cut short by the deadline left the control retired: shut
    // it down now, unless a holder has it and will on its next exchange.
    if stopping.is_err()
        && let Ok(mut stream) = control.try_lock()
        && stream.retired
    {
        stream.retire().await;
    }
    match stopping {
        Ok(Ok(Reply::Stopping { stopped_live })) => Some(stopped_live),
        Ok(Ok(_) | Err(_)) | Err(_) => None,
    }
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
        let owner = ProcessOwner::Turn {
            session_id: crate::SessionId::try_from("s_0123456789ab")
                .unwrap_or_else(|_| unreachable!("valid session id")),
            turn: crate::TurnNumber::try_from(1).unwrap_or_else(|_| unreachable!("valid turn")),
        };
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

    /// A control over a socket pair, and the anchor's side of it.
    fn control_pair() -> (Arc<Mutex<ControlConnection>>, UnixStream) {
        let (ours, peer) = UnixStream::pair().expect("socket pair");
        let control = Arc::new(Mutex::new(ControlConnection {
            stream: ours,
            reader: protocol::ControlReader::new(1024),
            retired: false,
        }));
        (control, peer)
    }

    /// Whether the anchor's side received a `Stop` for `generation`.
    async fn received_stop(peer: &mut UnixStream, generation: &str) -> bool {
        matches!(
            tokio::time::timeout(
                Duration::from_secs(2),
                protocol::read_message::<Request>(peer, 1024)
            )
            .await,
            Ok(Ok(Some(Request::Stop { generation: sent, .. }))) if sent == generation
        )
    }

    fn already_past() -> Deadline {
        Deadline::at(
            Instant::now()
                .checked_sub(Duration::from_secs(1))
                .expect("a second before now"),
        )
    }

    /// x.3.2 X2 r1 #5 (runtime §5): a `Stop` reply already in hand when a
    /// close that runs late first polls it is past the deadline, so it is
    /// no stop report: `None`, never `Some(false)`, as for `stop_through`.
    #[tokio::test]
    async fn a_ready_stop_reply_polled_after_the_deadline_is_none() {
        let (control, mut peer) = control_pair();
        protocol::write_message(
            &mut peer,
            &Reply::Stopping {
                stopped_live: false,
            },
            1024,
        )
        .await
        .expect("the anchor's reply");
        // The reactor has seen the socket writable and the reply readable,
        // so the late close's first poll finds the reply in hand.
        {
            let held = control.lock().await;
            held.stream.writable().await.expect("writable");
            held.stream.readable().await.expect("readable");
        }
        let report = stop_report(&control, "g1", already_past()).await;
        assert_eq!(report, None);
        assert!(received_stop(&mut peer, "g1").await, "no Stop was written");
    }

    /// Sol review of Task 3 round 2, decision D5-3: a task or owner that runs
    /// after the recorded deadline still writes its one `Stop`, waits for no
    /// reply, and so records no forced evidence. The anchor never replies
    /// here, so the absence of evidence is not a race.
    #[tokio::test]
    async fn a_stop_past_its_deadline_is_written_and_records_no_evidence() {
        let (control, mut peer) = control_pair();
        let stop = StopFacts::default();
        let forced = stop_through(&control, "g1", &stop, already_past()).await;
        assert!(!forced);
        assert!(!stop.forced.load(Ordering::Acquire));
        assert!(received_stop(&mut peer, "g1").await, "no Stop was written");
    }

    /// The same, when the control is busy at the deadline: a holder such as
    /// the status poll or a concurrent close has the mutex, and the `Stop`
    /// still goes out once it is free.
    #[tokio::test]
    async fn a_stop_past_its_deadline_waits_for_a_busy_control_and_is_written() {
        let (control, mut peer) = control_pair();
        let stop = Arc::new(StopFacts::default());
        let busy = control.clone().lock_owned().await;
        let stopping = tokio::spawn({
            let (control, stop) = (control.clone(), stop.clone());
            async move { stop_through(&control, "g1", &stop, already_past()).await }
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        drop(busy);
        assert!(received_stop(&mut peer, "g1").await, "no Stop was written");
        assert!(!stopping.await.expect("stop task"));
        assert!(!stop.forced.load(Ordering::Acquire));
    }

    /// Sol review of Task 3 round 3: a reply that is already in hand when the
    /// deadline has passed is late. Tokio polls the read before the timer, so
    /// the wait alone would return it; the evidence is refused on the clock.
    /// The reply is written before `stop_through` runs and the socket is
    /// awaited readable, so its readiness is recorded and the reply is ready
    /// on the first poll, as when a blocked runtime lets both the reply and
    /// the deadline pass before the task runs. There is no race and no clock.
    #[tokio::test]
    async fn a_ready_reply_after_the_deadline_records_no_forced_evidence() {
        let (control, mut peer) = control_pair();
        protocol::write_message(&mut peer, &Reply::Stopping { stopped_live: true }, 1024)
            .await
            .expect("reply");
        control
            .lock()
            .await
            .stream
            .readable()
            .await
            .expect("the reply is readable");
        let stop = StopFacts::default();
        let forced = stop_through(&control, "g1", &stop, already_past()).await;
        assert!(
            !forced,
            "a reply past the deadline recorded forced evidence"
        );
        assert!(!stop.forced.load(Ordering::Acquire));
        assert!(received_stop(&mut peer, "g1").await, "no Stop was written");
    }

    /// A reply inside the deadline still records the forced evidence.
    #[tokio::test]
    async fn a_stopped_live_reply_inside_the_deadline_records_forced_evidence() {
        let (control, mut peer) = control_pair();
        let stop = StopFacts::default();
        let anchor = tokio::spawn(async move {
            let sent = protocol::read_message::<Request>(&mut peer, 1024).await;
            assert!(matches!(sent, Ok(Some(Request::Stop { .. }))));
            protocol::write_message(&mut peer, &Reply::Stopping { stopped_live: true }, 1024)
                .await
                .expect("reply");
            peer
        });
        let deadline = Deadline::at(Instant::now() + Duration::from_secs(5));
        assert!(stop_through(&control, "g1", &stop, deadline).await);
        assert!(stop.forced.load(Ordering::Acquire));
        drop(anchor.await.expect("anchor side"));
    }

    /// Reads the next request the anchor's side received, within 2 s.
    async fn next_request(peer: &mut UnixStream) -> Option<Request> {
        tokio::time::timeout(
            Duration::from_secs(2),
            protocol::read_message::<Request>(peer, 1024),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
    }

    fn exited() -> Reply {
        Reply::Status {
            pid: Some(1),
            exit_code: Some(0),
            exit_signal: None,
        }
    }

    /// S1 critic finding 4: a control lock held, as a `Stop` exchange holds
    /// it, only skips poll ticks; the exit is still reported once the lock
    /// is free. The lock is released only after polls met it on three
    /// ticks (S1-io r1 finding 2), whenever the poll first runs.
    #[tokio::test]
    async fn a_busy_control_lock_does_not_end_exit_supervision() {
        let (control, mut peer) = control_pair();
        let (sender, mut exits) = watch::channel(None);
        let busy = control.clone().lock_owned().await;
        let supervision = tokio::spawn(supervise_exit(
            Arc::downgrade(&control),
            "g1".to_owned(),
            sender,
        ));
        // Three ticks met the held lock: supervision outlived a lock busy
        // for longer than one poll's wait.
        let met = tokio::time::timeout(Duration::from_secs(2), async {
            while BUSY_SKIPS.load(Ordering::Acquire) < 3 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(met.is_ok(), "supervision stopped polling the held lock");
        drop(busy);
        assert!(
            matches!(next_request(&mut peer).await, Some(Request::Status { .. })),
            "no Status poll once the lock was free"
        );
        protocol::write_message(&mut peer, &exited(), 1024)
            .await
            .expect("reply");
        let seen = tokio::time::timeout(Duration::from_secs(2), exits.wait_for(Option::is_some))
            .await
            .map(|seen| seen.map(|exit| *exit));
        assert!(
            matches!(seen, Ok(Ok(Some(ExitReport { code: Some(0), .. })))),
            "the exit was not reported"
        );
        assert!(matches!(supervision.await, Ok(Ok(()))));
    }

    /// S1 critic finding 4: a `Status` reply later than its bound retires
    /// the control and ends supervision; a following `Stop` fails at once
    /// and never reads the stale `Status` reply as its own.
    #[tokio::test]
    async fn a_status_reply_past_its_bound_retires_the_control() {
        let (control, mut peer) = control_pair();
        let (sender, mut exits) = watch::channel(None);
        let supervision = tokio::spawn(supervise_exit(
            Arc::downgrade(&control),
            "g1".to_owned(),
            sender,
        ));
        assert!(matches!(
            next_request(&mut peer).await,
            Some(Request::Status { .. })
        ));
        // The anchor answers only after supervision gave up on the reply.
        let ended = tokio::time::timeout(Duration::from_secs(5), exits.changed()).await;
        assert!(matches!(ended, Ok(Err(_))), "supervision did not end");
        assert!(exits.borrow().is_none());
        drop(supervision.await);
        // S1-io r1 decision 3: retiring shut the control down, so the anchor
        // sees control EOF and cleans up its group (runtime §5.1).
        let eof = tokio::time::timeout(
            Duration::from_secs(2),
            protocol::read_message::<Request>(&mut peer, 1024),
        )
        .await;
        assert!(matches!(eof, Ok(Ok(None))), "the anchor saw no control EOF");
        protocol::write_message(&mut peer, &exited(), 1024)
            .await
            .expect("stale reply");
        let stop = Request::Stop {
            generation: "g1".to_owned(),
            deadline_monotonic_ns: u64::MAX,
        };
        let reply = control.lock().await.transact(&stop, 1024).await;
        assert!(
            !matches!(reply, Ok(Reply::Status { .. })),
            "the Stop read the stale Status reply as its own"
        );
        assert!(reply.is_err(), "a retired control answered");
    }

    /// S1 critic finding 4: an exchange cancelled after its write, as a
    /// close's deadline cancels its `Stop`, retires the control.
    #[tokio::test]
    async fn a_cancelled_exchange_retires_the_control() {
        let (control, mut peer) = control_pair();
        let stop = Request::Stop {
            generation: "g1".to_owned(),
            deadline_monotonic_ns: u64::MAX,
        };
        let cancelled = tokio::time::timeout(Duration::from_millis(50), async {
            control.lock().await.transact(&stop, 1024).await
        })
        .await;
        assert!(cancelled.is_err());
        assert!(matches!(
            next_request(&mut peer).await,
            Some(Request::Stop { .. })
        ));
        protocol::write_message(&mut peer, &Reply::Stopping { stopped_live: true }, 1024)
            .await
            .expect("late reply");
        let status = Request::Status {
            generation: "g1".to_owned(),
        };
        let reply = control.lock().await.transact(&status, 1024).await;
        assert!(
            !matches!(reply, Ok(Reply::Stopping { .. })),
            "the Status read the late Stop reply as its own"
        );
        assert!(reply.is_err(), "a retired control answered");
    }

    /// S1 critic finding 5: tracking a task first collects finished ones;
    /// a failed one is counted, and shutdown still reports it.
    #[tokio::test]
    async fn tracking_a_task_collects_finished_ones_and_keeps_failures() {
        let (host, _store, root) = host_fixture(true);
        let failed: JoinHandle<TaskResult> = tokio::spawn(async { Err(()) });
        let ended: JoinHandle<TaskResult> = tokio::spawn(async { Ok(()) });
        host.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .track(failed);
        host.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .track(ended);
        while !host
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
            .iter()
            .all(|task| {
                task.handle
                    .try_lock()
                    .is_ok_and(|handle| handle.is_finished())
            })
        {
            tokio::task::yield_now().await;
        }
        let release = hold(&host);
        let (release_next, held) = tokio::sync::oneshot::channel::<()>();
        host.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .track(tokio::spawn(async move {
                let _ = held.await;
                Ok(())
            }));
        {
            let tasks = host
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(tasks.running.len(), 2, "finished tasks were kept");
            assert_eq!(tasks.failed, 1, "the failed task was not counted");
        }
        assert!(release.send(()).is_ok() && release_next.send(()).is_ok());
        let report = host
            .shutdown(Deadline::at(Instant::now() + Duration::from_secs(1)), &[])
            .await;
        assert_eq!(
            (report.pending_tasks, report.failed_tasks),
            (0, 1),
            "{report:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// S1-io review r2 finding 1: an acquisition that tracks its tasks
    /// while another turn's close is between its `Stop` reply (`forced`
    /// set) and its report (`reported` set) must not copy that live
    /// control's fact; once the close reported it and the control is
    /// dropped, no fact is left Host-wide.
    #[tokio::test]
    async fn an_acquisition_during_a_close_keeps_no_handed_off_fact() {
        let mut tasks = HostTasks::default();
        let (control, _peer) = control_pair();
        let stop = Arc::new(StopFacts::default());
        let (_sender, exit) = watch::channel(None);
        tasks.controls.push(TrackedControl {
            stream: Arc::downgrade(&control),
            owner: ProcessOwner::Server {
                server_id: crate::ServerId::try_from("v_0123456789ab").expect("server id"),
            },
            identity: ProcessIdentity {
                pid: 1,
                pgid: 1,
                uid: 0,
                boot_id: "boot".to_owned(),
                pid_namespace: "pidns".to_owned(),
                start_ticks: 1,
                marker: crate::ProcessMarker::try_from_generated("m".to_owned()).expect("marker"),
            },
            anchor_id: "a1".to_owned(),
            generation: "g1".to_owned(),
            exit,
            stop: stop.clone(),
        });
        // The close has its `stopped_live` reply and waits for absence.
        stop.forced.store(true, Ordering::Release);
        tasks.track(tokio::spawn(async { Ok(()) }));
        // The close reports the fact to its owner, then the owner drops it.
        stop.reported.store(true, Ordering::Release);
        drop(control);
        tasks.track(tokio::spawn(async { Ok(()) }));
        assert!(tasks.controls.is_empty());
        assert!(
            tasks.forced.is_empty(),
            "a fact the close handed off stayed Host-wide: {:?}",
            tasks.forced
        );
    }

    /// A live control in `phase` over a socket pair; keep the returned
    /// handles alive, the ledger holds the control weakly.
    fn live_control(
        generation: &str,
    ) -> (LiveControl, (Arc<Mutex<ControlConnection>>, UnixStream)) {
        let (control, peer) = control_pair();
        let live = LiveControl {
            stream: Arc::downgrade(&control),
            generation: generation.to_owned(),
            stop: Arc::new(StopFacts::default()),
            phase: LaunchPhase::Verified,
            exit: None,
            server: false,
        };
        (live, (control, peer))
    }

    /// A ledger wired to a force watch, as `Host::watch_force` wires it.
    fn ledger_with_force() -> (Capacity, watch::Sender<Option<Instant>>) {
        let (force, signal) = watch::channel(None);
        let capacity = Capacity::default();
        capacity.lock().force = Some(signal);
        (capacity, force)
    }

    fn forced_long_ago() -> Instant {
        Instant::now()
            .checked_sub(Duration::from_secs(10))
            .expect("ten seconds before now")
    }

    /// Sol review of Task 3 round 2, decision D5-5: the deadline a ledger
    /// section derives is exactly the force's instant plus 3 s, whenever the
    /// section runs and whichever section it is; none reads the clock.
    #[tokio::test]
    async fn stopping_is_the_force_instant_plus_three_seconds_in_every_section() {
        let (capacity, force) = ledger_with_force();
        let (verified, _keep_first) = live_control("g1");
        assert_eq!(capacity.register("a1", verified), None);
        assert_eq!(capacity.stopping(), None);
        let at = forced_long_ago();
        force.send_replace(Some(at));
        let expected = Some(at + EARLY_STOP);
        assert_eq!(capacity.stopping(), expected);
        let (late, _keep_second) = live_control("g2");
        assert_eq!(capacity.register("a2", late), expected);
        assert_eq!(capacity.begin_arming("a1"), Err(at + EARLY_STOP));
        assert_eq!(capacity.armed("a1", watch::channel(None).1), expected);
        assert_eq!(
            capacity.begin_stopping().map(|(deadline, _)| deadline),
            expected
        );
    }

    /// Task 4 design §11.3: `process.alive` is positive evidence only: an
    /// `Armed` entry whose control is held and whose exit watch reported no
    /// exit. An exit observed while the control is still upgradeable, or a
    /// dropped control, is not alive.
    #[tokio::test]
    async fn live_armed_needs_a_held_armed_control_without_an_exit() {
        let (capacity, _force) = ledger_with_force();
        let (control, keep) = live_control("g1");
        let anchors = ["a1".to_owned()];
        assert_eq!(capacity.register("a1", control), None);
        assert!(!capacity.live_armed(&anchors), "verified is not armed");
        let (exit, exits) = watch::channel(None);
        assert_eq!(capacity.armed("a1", exits), None);
        assert!(capacity.live_armed(&anchors));
        assert!(!capacity.live_armed(&["a2".to_owned()]));
        exit.send_replace(Some(ExitReport {
            code: Some(0),
            signal: None,
        }));
        assert!(
            !capacity.live_armed(&anchors),
            "an observed exit is not alive"
        );
        let (control, keep_second) = live_control("g2");
        assert_eq!(capacity.register("a2", control), None);
        let (_exit, exits) = watch::channel(None);
        assert_eq!(capacity.armed("a2", exits), None);
        assert!(capacity.live_armed(&["a2".to_owned()]));
        drop(keep);
        drop(keep_second);
        assert!(
            !capacity.live_armed(&["a2".to_owned()]),
            "a dropped control is not alive"
        );
    }

    /// Decision D5-2: the ARM gate reads the force in its own ledger
    /// section. No early-stop task has run, and the gate still refuses.
    #[tokio::test]
    async fn the_arm_gate_refuses_once_the_force_is_raised_though_no_task_ran() {
        let (capacity, force) = ledger_with_force();
        let (control, _keep) = live_control("g1");
        assert_eq!(capacity.register("a1", control), None);
        force.send_replace(Some(Instant::now()));
        assert!(capacity.begin_arming("a1").is_err());
        let phase = capacity.lock().live["a1"].phase;
        assert_eq!(phase, LaunchPhase::Verified, "the refused entry advanced");
    }

    /// Before the force is raised the gate passes and marks the entry.
    #[tokio::test]
    async fn the_arm_gate_passes_before_the_force() {
        let (capacity, _force) = ledger_with_force();
        let (control, _keep) = live_control("g1");
        assert_eq!(capacity.register("a1", control), None);
        assert_eq!(capacity.begin_arming("a1"), Ok(()));
        assert_eq!(capacity.armed("a1", watch::channel(None).1), None);
        assert_eq!(capacity.lock().live["a1"].phase, LaunchPhase::Armed);
    }

    /// The control connections and their anchor-side ends, kept open by the
    /// test so the entries stay live.
    type Kept = Vec<(Arc<Mutex<ControlConnection>>, UnixStream)>;

    /// The registered entries of one ledger in each phase: `g_verified`
    /// before the gate, `g_arming` past it, `g_armed` after `Spawned`.
    fn one_entry_per_phase() -> (Capacity, watch::Sender<Option<Instant>>, Kept) {
        let (capacity, force) = ledger_with_force();
        let mut keep = Vec::new();
        for (anchor, phase) in [("a_v", 0), ("a_r", 1), ("a_a", 2)] {
            let (control, handles) = live_control(&format!("g_{anchor}"));
            assert_eq!(capacity.register(anchor, control), None);
            keep.push(handles);
            if phase >= 1 {
                assert_eq!(capacity.begin_arming(anchor), Ok(()));
            }
            if phase >= 2 {
                assert_eq!(capacity.armed(anchor, watch::channel(None).1), None);
            }
        }
        (capacity, force, keep)
    }

    fn generations(controls: &[LiveControl]) -> Vec<&str> {
        controls
            .iter()
            .map(|control| control.generation.as_str())
            .collect()
    }

    /// Design §6.8 [r5.2, r6.1], exactly one of three covers each entry,
    /// when the first section to see the force is the owner's `armed`
    /// marking rather than the task: the `Armed` entry is the task's, the
    /// `Arming` entry, marked `Armed` in the section that set `stopping`, is
    /// its owner's and is not also in the snapshot, and the `Verified`
    /// entry meets the gate refusal.
    #[tokio::test]
    async fn an_owner_that_sets_stopping_keeps_its_entry_out_of_the_snapshot() {
        let (capacity, force, _keep) = one_entry_per_phase();
        let at = Instant::now();
        force.send_replace(Some(at));
        assert_eq!(
            capacity.armed("a_r", watch::channel(None).1),
            Some(at + EARLY_STOP)
        );
        assert!(capacity.begin_arming("a_v").is_err());
        let (deadline, snapshot) = capacity.begin_stopping().expect("force is raised");
        assert_eq!(deadline, at + EARLY_STOP);
        assert_eq!(generations(&snapshot), ["g_a_a"]);
        assert!(
            capacity
                .begin_stopping()
                .is_some_and(|(_, again)| again.is_empty())
        );
    }

    /// The same, when the task is first: the snapshot is the `Armed`
    /// entry, and each entry that was not `Armed` finds `stopping` set at its
    /// own next section, whatever the task does after.
    #[tokio::test]
    async fn a_task_that_sets_stopping_leaves_the_other_entries_to_their_owners() {
        let (capacity, force, _keep) = one_entry_per_phase();
        let at = Instant::now();
        force.send_replace(Some(at));
        let (deadline, snapshot) = capacity.begin_stopping().expect("force is raised");
        assert_eq!(deadline, at + EARLY_STOP);
        assert_eq!(generations(&snapshot), ["g_a_a"]);
        assert_eq!(
            capacity.armed("a_r", watch::channel(None).1),
            Some(at + EARLY_STOP)
        );
        assert!(capacity.begin_arming("a_v").is_err());
        let (late, _keep_late) = live_control("g_late");
        assert_eq!(capacity.register("a_late", late), Some(at + EARLY_STOP));
    }

    /// A ledger no force watch is wired to never derives `stopping`.
    #[tokio::test]
    async fn a_ledger_without_a_force_watch_never_stops() {
        let capacity = Capacity::default();
        let (control, _keep) = live_control("g1");
        assert_eq!(capacity.register("a1", control), None);
        assert_eq!(capacity.begin_arming("a1"), Ok(()));
        assert_eq!(capacity.armed("a1", watch::channel(None).1), None);
        assert!(capacity.begin_stopping().is_none());
    }
}

#[cfg(test)]
mod configure_size {
    use std::ffi::OsString;
    use std::path::Path;

    use super::*;
    use crate::EnvAllowList;

    /// The largest second argument, in bytes, whose launch fits.
    fn largest_fitting(env: &EnvAllowList) -> usize {
        let fits = |n: usize| {
            PrivateProcessSpec::configure_fits(
                Path::new("/bin/claude"),
                &[OsString::from("-p"), OsString::from("z".repeat(n))],
                Path::new("/work"),
                env,
            )
        };
        let (mut low, mut high) = (0, protocol::REQUEST_MAX);
        while low < high {
            let mid = (low + high).div_ceil(2);
            if fits(mid) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        low
    }

    /// x.3.2 C3 (critical r1 #1): `configure_fits` is the anchor's own cap
    /// on the encoded request: at the largest fitting argument the request
    /// encodes within it with any marker (a random one, and the marker of
    /// largest encoding), and one byte more exceeds it under a marker of
    /// largest encoding, which a random marker can be.
    #[test]
    fn configure_fits_is_the_anchor_cap() {
        let env = EnvAllowList::try_from_entries(vec![("PATH".into(), "/usr/bin".into())])
            .expect("valid env");
        let n = largest_fitting(&env);
        assert!(n > 0 && n < protocol::REQUEST_MAX / 3, "{n}");
        let encoded = |n: usize, marker: String| {
            let vendor = vendor_config_with(
                (
                    Path::new("/bin/claude"),
                    &[OsString::from("-p"), OsString::from("z".repeat(n))],
                    Path::new("/work"),
                ),
                &env,
                marker,
            );
            serde_json::to_vec(&Request::Configure { vendor })
                .expect("encodes")
                .len()
        };
        let largest = || "f".repeat(linux::RANDOM_HEX_LEN);
        let random = linux::random_hex().expect("random marker");
        assert_eq!(random.len(), linux::RANDOM_HEX_LEN);
        assert!(encoded(n, random) <= protocol::REQUEST_MAX);
        assert!(encoded(n, largest()) <= protocol::REQUEST_MAX);
        assert!(encoded(n + 1, largest()) > protocol::REQUEST_MAX);
    }
}
