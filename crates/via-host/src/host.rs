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
    AnchorIdentity, AnchorIntent, AnchorRecord, CommitOutcome, GroupAbsenceRecord, ProcessJournal,
    StoreFailureKind,
};

use crate::{
    CleanupEvidence, CleanupReason, CloseMode, CloseRequest, Deadline, ExitReport,
    PrivateProcessSpec, ProcessIdentity, linux,
    protocol::{self, Bootstrap, Reply, Request, VendorConfig},
};

/// Absence verification after a failed acquisition: close's cleanup
/// allowance (Route bounds every close by 3 s).
const FAILED_ACQUIRE_CLEANUP: Duration = Duration::from_secs(3);

/// One daemon-side Host instance tied to a validated private anchor directory.
#[derive(Clone)]
pub struct Host {
    journal: ProcessJournal,
    anchor_binary: PathBuf,
    anchor_dir: PathBuf,
    tasks: Arc<StdMutex<HostTasks>>,
    capacity: Capacity,
}

/// Capacity tokens by anchor id: one per group that may still live, dropped
/// exactly once, when Host proves that group absent (runtime §5: a timed-out
/// wait releases no admission capacity).
#[derive(Clone, Default)]
struct Capacity(Arc<StdMutex<HashMap<String, crate::CapacityToken>>>);

impl Capacity {
    fn hold(&self, anchor_id: String, token: crate::CapacityToken) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(anchor_id, token);
    }

    /// Releases the anchor's capacity once its group is proved absent.
    fn settle(&self, anchor_id: &str, cleanup: &CleanupEvidence) {
        if matches!(cleanup, CleanupEvidence::GroupAbsent(_)) {
            let token = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(anchor_id);
            drop(token);
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

/// Host launch, durable journal or private-control failure.
#[derive(Debug)]
pub enum HostError {
    /// OS or filesystem operation failed.
    Io(io::Error),
    /// Required Store mutation lacked a positive commit receipt.
    Store(&'static str),
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
    stream: UnixStream,
    pipes: OwnedPipes,
    version: u64,
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
        })
    }

    /// Starts one private group through the durable intent and ARM gate.
    pub async fn acquire(
        &self,
        spec: PrivateProcessSpec,
        deadline: Deadline,
    ) -> Result<AcquiredProcess, HostError> {
        let (_never, stop) = watch::channel(false);
        self.acquire_retaining(spec, deadline, &LaunchPipes::default(), &stop)
            .await
    }

    /// [`Host::acquire`] that moves the vendor pipes into `launch` just before
    /// ARM is sent: from then on the vendor may run and write to them, so they
    /// outlive an acquisition that fails or is abandoned after ARM, and the
    /// caller can still record that output. A successful acquisition takes
    /// them back.
    ///
    /// `stop` is the caller's force or Store-failure signal, checked at the
    /// last gate before ARM: set by then, no ARM is sent, the anchor control is
    /// dropped so its group stops, and the result is [`HostError::Stopped`].
    /// Set after the check, ARM won and the launch is in flight.
    pub async fn acquire_retaining(
        &self,
        spec: PrivateProcessSpec,
        deadline: Deadline,
        launch: &LaunchPipes,
        stop: &watch::Receiver<bool>,
    ) -> Result<AcquiredProcess, HostError> {
        if Instant::now() >= deadline.instant() {
            return Err(HostError::Deadline);
        }
        if !spec.program.is_absolute() || !spec.cwd.is_absolute() {
            return Err(HostError::Invalid(
                "vendor executable and cwd must be absolute",
            ));
        }
        if spec
            .env
            .entries()
            .iter()
            .any(|(name, _)| name == "VIA_PROCESS_MARKER")
        {
            return Err(HostError::Invalid("reserved vendor marker environment key"));
        }
        let mut started = None;
        let acquired = timeout_at(
            deadline.instant(),
            self.acquire_inner(spec, launch, stop, &mut started),
        )
        .await
        .map_err(|_| HostError::Deadline)
        .flatten();
        if acquired.is_err()
            && let Some((anchor_id, generation, identity)) = started
        {
            // Design §11: a failed acquisition whose anchor spawned proves the
            // group absent before it returns, as close does and within close's
            // cleanup allowance. The anchor control is dropped by now, so the
            // anchor exits on EOF and stops its group. Only `GroupAbsent`
            // releases the anchor's capacity; uncertainty keeps it.
            let cleanup = Deadline::at(Instant::now() + FAILED_ACQUIRE_CLEANUP);
            let evidence = wait_absence(&self.journal, &anchor_id, &generation, &identity, cleanup)
                .await
                .unwrap_or(CleanupEvidence::Uncertain(
                    CleanupReason::EvidenceStoreFailure,
                ));
            self.capacity.settle(&anchor_id, &evidence);
        }
        acquired
    }

    /// Holds capacity for a group this Host did not launch, such as one an
    /// earlier daemon left whose absence recovery did not prove; a later
    /// absence proof for `anchor_id` releases it.
    pub fn hold_capacity(&self, anchor_id: String, token: crate::CapacityToken) {
        self.capacity.hold(anchor_id, token);
    }

    async fn start_anchor(
        &self,
        owner: crate::ProcessOwner,
        capacity: Option<crate::CapacityToken>,
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
            owner_session: owner.session_id,
            owner_turn: owner.turn,
            uid: rustix::process::getuid().as_raw(),
            boot_id: linux::boot_id()?,
            pid_namespace: linux::pid_namespace()?,
        };
        let CommitOutcome::Committed(receipt) =
            self.journal.commit_anchor_intent(intent.clone()).await
        else {
            return Err(HostError::Store("anchor intent not durably committed"));
        };
        let bootstrap = Bootstrap {
            anchor_id: anchor_id.clone(),
            generation: generation.clone(),
            marker: marker.clone(),
            controller_pid: std::process::id(),
            socket_path: socket_path.clone(),
        };
        let bytes = serde_json::to_vec(&bootstrap).map_err(io::Error::other)?;
        let mut config = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&config_path)?;
        config.write_all(&bytes)?;
        config.sync_all()?;
        drop(config);

        let (pipes, anchor_process_id) = self.spawn_anchor(&config_path)?;
        // The group exists from here: its capacity stays with Host until
        // absence is proved. A failure above dropped it with no group.
        if let Some(token) = capacity {
            self.capacity.hold(anchor_id.clone(), token);
        }
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
        let CommitOutcome::Committed(version) = self
            .journal
            .commit_anchor_identified(
                &anchor_id,
                &generation,
                receipt.record_version,
                identity_to_store(&identity),
            )
            .await
        else {
            return Err(HostError::Store("anchor identity not durably committed"));
        };
        Ok(StartedAnchor {
            anchor_id,
            generation,
            identity,
            stream,
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
        stop: &watch::Receiver<bool>,
        started: &mut Option<(String, String, ProcessIdentity)>,
    ) -> Result<AcquiredProcess, HostError> {
        let StartedAnchor {
            anchor_id,
            generation,
            identity,
            mut stream,
            pipes,
            version,
        } = self
            .start_anchor(spec.owner.clone(), spec.capacity.take())
            .await?;
        *started = Some((anchor_id.clone(), generation.clone(), identity.clone()));
        let mut vendor_env = spec.env.entries().to_vec();
        vendor_env.push(("VIA_PROCESS_MARKER".into(), linux::random_hex()?.into()));
        let vendor = VendorConfig::from_parts(&spec.program, &spec.args, &spec.cwd, &vendor_env);
        let Reply::Configured =
            protocol::transact(&mut stream, &Request::Configure { vendor }, 65_536).await?
        else {
            return Err(HostError::Protocol("anchor configuration refused"));
        };
        let CommitOutcome::Committed(_) = self
            .journal
            .commit_arm_intent(&anchor_id, &generation, version)
            .await
        else {
            return Err(HostError::Store("ArmIntent lacks positive commit receipt"));
        };
        #[cfg(feature = "test-failpoints")]
        via_store::failpoint::hit_async("host.anchor.after_arm_intent_commit")
            .await
            .map_err(HostError::Io)?;
        // The pre-ARM launch gate: a stop set by now wins and nothing launches;
        // returning drops the anchor control, so the anchor exits on EOF and
        // stops its group. Past this check ARM wins and the launch is in flight.
        if *stop.borrow() {
            return Err(HostError::Stopped);
        }
        // This is the only ARM send for this generation; errors never cause retry.
        launch.put(pipes);
        let reply = protocol::transact(
            &mut stream,
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
        let CommitOutcome::Committed(()) = self
            .journal
            .commit_vendor_facts(&anchor_id, &generation, vendor_pid)
            .await
        else {
            return Err(HostError::Store("vendor facts not durably committed"));
        };
        let pipes = launch
            .take()
            .ok_or(HostError::Protocol("launch pipes already taken"))?;
        let stream = Arc::new(Mutex::new(ControlConnection {
            stream,
            reader: protocol::FrameReader::new(1024),
        }));
        let (sender, exits) = watch::channel(None);
        let control = ProcessControl {
            stream: stream.clone(),
            identity,
            anchor_id,
            generation,
            journal: self.journal.clone(),
            exit: exits.clone(),
            stop: Arc::default(),
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
        if Instant::now() >= deadline.instant() {
            return Err(HostError::Deadline);
        }
        // A read that never completed is a Store failure, not unproven cleanup.
        let records = timeout_at(
            deadline.instant(),
            self.journal.list_anchor_records_page(after, limit),
        )
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
        let _ = timeout_at(deadline.instant(), async {
            if let Ok(mut stream) = UnixStream::connect(&record.intent.socket_path).await
                && verify_peer_and_challenge(&mut stream, &identity)
                    .await
                    .is_ok()
            {
                let _ = protocol::transact(
                    &mut stream,
                    &Request::Stop {
                        generation: record.intent.generation.clone(),
                        deadline_monotonic_ns: monotonic_deadline(deadline),
                    },
                    1024,
                )
                .await;
            }
        })
        .await;
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
        // A close has no error channel: a Store failure stays uncertain here.
        let cleanup = wait_absence(
            &self.journal,
            &self.anchor_id,
            &self.generation,
            &self.identity,
            request.deadline,
        )
        .await
        .unwrap_or(CleanupEvidence::Uncertain(
            CleanupReason::EvidenceStoreFailure,
        ));
        self.capacity.settle(&self.anchor_id, &cleanup);
        CloseReport {
            cleanup,
            vendor_exit: *self.exit.borrow(),
            forced,
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
                let record = GroupAbsenceRecord {
                    anchor_id: anchor_id.to_owned(),
                    generation: generation.to_owned(),
                    boot_id: identity.boot_id.clone(),
                    pid_namespace: identity.pid_namespace.clone(),
                    pgid: identity.pgid,
                    observed_at: proof.observed_at().to_owned(),
                };
                // Proof observed too late to record is unproven, not a Store failure.
                if Instant::now() >= deadline.instant() {
                    return Ok(CleanupEvidence::Uncertain(CleanupReason::Deadline));
                }
                #[cfg(feature = "test-failpoints")]
                via_store::failpoint::hit_async("host.recovery.absence_commit")
                    .await
                    .map_err(|_| HostError::Store("group absence commit failed"))?;
                return match timeout_at(deadline.instant(), journal.commit_group_absence(record))
                    .await
                {
                    Ok(CommitOutcome::Committed(())) => Ok(evidence),
                    Ok(CommitOutcome::NotCommitted(_)) => {
                        Err(HostError::Store("group absence commit failed"))
                    }
                    Ok(CommitOutcome::Uncertain(_)) | Err(_) => {
                        Err(HostError::Store("group absence commit outcome uncertain"))
                    }
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
