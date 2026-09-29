use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::watch,
    time::timeout_at,
};

use super::{
    BoundedBytes, Deadline, PrivateProcessSpec, SendOutcome, VendorMessage, WireCleanup,
    WireFailure,
};
use via_host::{
    AcquireFailure, AcquiredProcess, CleanupEvidence, ExitReceiver, Host, LaunchPipes,
    ProcessControl,
};
use via_store::{EvidenceRoot, RuntimeResources};

/// The prefix of an undecoded message VIA keeps (design §7.3).
pub const UNDECODED_BYTES: usize = 64 * 1024;

/// Bound on writing `undecoded.bin` (design §7.3).
const UNDECODED_WRITE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long an acquisition may still finish once its caller is cancelled: a
/// normal one does, so its group is force-closed and proved absent; a stalled
/// one is abandoned well inside the 10 s final shutdown.
const CANCELLED_ACQUIRE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The signals Route hands Wire for one connection (design §2).
pub struct WireSignals {
    /// The daemon force watch: once set, every wait on the vendor ends with
    /// [`WireError::Cancelled`].
    pub force: watch::Receiver<Option<tokio::time::Instant>>,
    /// Route's wake: each change ends the current wait on the vendor once
    /// with [`WireError::Woken`], before any byte is read, so Route can act on
    /// its turn's stop order without losing bytes.
    pub wake: watch::Receiver<u64>,
    /// Host's pre-ARM gate (design §2 rule 1): true stops the launch.
    pub gate: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
}

/// Deployment paths for the sole Host anchor service.
pub struct RuntimeConfig {
    /// Same `via` binary used for the internal anchor entrypoint.
    pub anchor_binary: std::path::PathBuf,
    /// Validated private anchor socket/control directory.
    pub anchor_dir: std::path::PathBuf,
}

/// Wire owns the evidence root and process capabilities split from one
/// Store owner.
pub struct WireRuntime {
    evidence: EvidenceRoot,
    host: Host,
}

impl WireRuntime {
    /// Consumes the unopened Store bundle at the sole production split site.
    pub fn new(config: RuntimeConfig, resources: RuntimeResources) -> Result<Self, WireError> {
        let (evidence, journal) = resources.into_wire_parts();
        let host = Host::new(journal, config.anchor_binary, config.anchor_dir)?;
        Ok(Self { evidence, host })
    }

    /// Opens one private connection for the turn `spec.owner` names. First
    /// the turn's evidence folder is created on the blocking pool (design
    /// §7.2); a failure there is [`WireError::Evidence`] and nothing is
    /// acquired. Host then creates `stderr.log` in it for the vendor.
    /// Once `signals.force` is set, waits for vendor input, output or exit
    /// end with [`WireError::Cancelled`]. A failed acquisition is
    /// [`WireError::Acquire`] with Host's cleanup evidence.
    pub async fn open_connection(
        &self,
        mut spec: PrivateProcessSpec,
        deadline: Deadline,
        signals: WireSignals,
    ) -> Result<WireConnection, WireError> {
        let root = self.evidence.clone();
        let (session, turn) = (spec.owner.session_id.clone(), spec.owner.turn);
        let folder = tokio::task::spawn_blocking(move || root.create_turn(&session, turn))
            .await
            .map_err(|error| WireError::Evidence(std::io::Error::other(error)))?
            .map_err(WireError::Evidence)?;
        spec.stderr_path = folder.join("stderr.log");
        Box::pin(WireConnection::open(
            &self.host, spec, folder, deadline, signals,
        ))
        .await
    }

    /// Drains Host controls and tasks before the Store owner is released.
    pub async fn shutdown(
        &self,
        deadline: Deadline,
        turns: &[(via_store::SessionId, via_store::TurnNumber)],
    ) -> WireShutdown {
        summarize_shutdown(self.host.shutdown(deadline, turns).await)
    }

    /// Hands Host capacity for a group it did not launch (design §11).
    pub fn hold_capacity(
        &self,
        anchor_id: String,
        owner: via_store::SessionId,
        token: via_host::CapacityToken,
    ) {
        self.host.hold_capacity(anchor_id, owner, token);
    }

    /// One non-signalling re-probe pass over Host's held groups, optionally
    /// only one session's (design §8).
    pub async fn reprobe_held(
        &self,
        deadline: Deadline,
        owner: Option<via_store::SessionId>,
    ) -> Result<via_host::ReprobeReport, WireError> {
        self.host
            .reprobe_held(deadline, owner)
            .await
            .map_err(WireError::Host)
    }

    /// Held groups no live control owns (design §6.6).
    pub fn held_unproven(&self) -> usize {
        self.host.held_unproven()
    }

    /// Advances on every added holding (design §8).
    pub fn holdings_changed(&self) -> watch::Receiver<u64> {
        self.host.holdings_changed()
    }

    /// Groups whose cleanup a live control or acquisition still owns
    /// (design §6.4).
    pub fn pending_cleanup(&self) -> usize {
        self.host.pending_cleanup()
    }

    /// Subscribes Host's early stop to the daemon force signal (design §6.8),
    /// which carries the instant the force was raised.
    pub fn watch_force(&self, forced: watch::Receiver<Option<tokio::time::Instant>>) {
        self.host.watch_force(forced);
    }

    /// Reconciles one page of up to `limit` committed anchors after the
    /// `after` id, only through Host's verified path.
    pub async fn recover_page(
        &self,
        after: Option<String>,
        limit: u32,
        deadline: Deadline,
    ) -> Result<Vec<WireRecovery>, WireError> {
        self.host
            .recover_page(after, limit, deadline)
            .await
            .map(|reports| reports.into_iter().map(normalize_recovery).collect())
            .map_err(WireError::Host)
    }

    /// [`Self::recover_page`] of the anchors in `cohort` only.
    pub async fn recover_cohort_page(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: crate::AnchorCohort,
        deadline: Deadline,
    ) -> Result<Vec<WireRecovery>, WireError> {
        self.host
            .recover_cohort_page(after, limit, cohort, deadline)
            .await
            .map(|reports| reports.into_iter().map(normalize_recovery).collect())
            .map_err(WireError::Host)
    }
}

impl WireError {
    /// Durable Store state could not be read or written, as opposed to a
    /// deadline or process evidence that merely stays unproven.
    pub fn is_store_failure(&self) -> bool {
        matches!(
            self,
            Self::Evidence(_)
                | Self::Host(
                    via_host::HostError::Evidence(_)
                        | via_host::HostError::Store(_)
                        | via_host::HostError::StoreUnavailable(_)
                        | via_host::HostError::Journal { .. }
                )
        )
    }

    /// A Host journal write had an uncertain outcome, as a re-probe proof
    /// commit that outlived its pass: the daemon latches (design §7.2
    /// row 12).
    pub fn journal_uncertain(&self) -> bool {
        matches!(
            self,
            Self::Host(via_host::HostError::Journal {
                uncertain: true,
                ..
            })
        )
    }
}

/// Passive recovery fact without process signalling authority.
pub struct WireRecovery {
    /// Opaque committed anchor identifier.
    pub anchor_id: String,
    /// Opaque committed launch generation.
    pub generation: String,
    /// Owning VIA session.
    pub owner_session: via_store::SessionId,
    /// Owning turn.
    pub owner_turn: via_store::TurnNumber,
    /// Group cleanup certainty under Host's identity and absence checks.
    pub cleanup: super::WireCleanup,
    /// Host stopped the group while its vendor was live (Host force evidence).
    pub forced: bool,
}

/// Private-group close evidence without Host identity or signal authority.
pub struct WireCloseReport {
    /// Group cleanup certainty.
    pub cleanup: super::WireCleanup,
    /// Independently confirmed vendor exit if available.
    pub vendor_exit: Option<super::ExitReport>,
    /// Host's verified anchor accepted the stop while the vendor was live.
    pub forced: bool,
    /// The absence proof's commit had an uncertain outcome: the daemon must
    /// latch (design §7.2 row 12).
    pub journal_uncertain: bool,
}

/// Failure of a private byte transport; an uncertain write never permits resend.
#[derive(Debug, Error)]
pub enum WireError {
    /// Host could not acquire or supervise its private process.
    #[error("host acquisition failed: {0}")]
    Host(#[from] via_host::HostError),
    /// The turn's evidence folder could not be created (design §7.2):
    /// nothing was acquired.
    #[error("evidence folder not created: {0}")]
    Evidence(std::io::Error),
    /// A pipe operation failed.
    #[error("vendor pipe failed: {0}")]
    Io(#[from] std::io::Error),
    /// A pipe operation crossed its absolute deadline.
    #[error("vendor pipe deadline elapsed")]
    Deadline,
    /// The caller's cancel signal ended a wait on the vendor.
    #[error("vendor wait cancelled")]
    Cancelled,
    /// Route's wake ended a wait on the vendor before any byte was read.
    #[error("vendor wait woken")]
    Woken,
    /// Acquisition failed, was stopped at the gate or was cancelled, with
    /// the evidence Host's cleanup left (design §2 rule 1, §7.2 rows 3, 4).
    #[error("acquisition failed: {cause}")]
    Acquire {
        /// The acquisition's own failure.
        cause: Box<WireError>,
        /// ARM was sent: the vendor may have launched.
        launched: bool,
        /// Host's bounded absence verification; `None` when no anchor
        /// intent was committed, so no group can exist.
        cleanup: Option<WireCleanup>,
        /// Host's `Stop` stopped a live vendor.
        forced: bool,
        /// A journal write had an uncertain outcome: the daemon must latch.
        journal_uncertain: bool,
    },
    /// Vendor message contract failure.
    #[error("vendor message failure: {0:?}")]
    Message(WireFailure),
}

/// Exclusive transport for one private fake process. The vendor's stderr is
/// the turn's `stderr.log`, which Wire never reads.
pub struct WireConnection {
    stdin: Option<tokio::process::ChildStdin>,
    stdout: tokio::process::ChildStdout,
    stdout_eof: bool,
    unterminated_stdout: bool,
    buffered: Vec<u8>,
    /// The turn's evidence folder.
    folder: PathBuf,
    /// The note of the first undecoded message kept (design §7.3).
    undecoded: Option<String>,
    control: ProcessControl,
    exits: ExitReceiver,
    /// Caller's cancel signal; checked only where waiting loses no bytes.
    cancel: watch::Receiver<Option<tokio::time::Instant>>,
    /// Route's wake, likewise checked only where waiting loses no bytes.
    wake: watch::Receiver<u64>,
}

impl WireConnection {
    /// Acquires one Host-owned process after the caller's durable submission intent.
    async fn open(
        host: &Host,
        spec: PrivateProcessSpec,
        folder: PathBuf,
        deadline: Deadline,
        signals: WireSignals,
    ) -> Result<Self, WireError> {
        let WireSignals {
            force: mut cancel,
            wake,
            gate,
        } = signals;
        let launch = LaunchPipes::default();
        // The gate is checked inside Host just before ARM: set by then,
        // nothing launches.
        let mut acquire = Box::pin(host.acquire_retaining(spec, deadline, &launch, &*gate));
        let acquired = tokio::select! {
            // A force already set when the acquisition's own result is observed
            // came first.
            biased;
            () = cancelled(&mut cancel) => {
                // After the force, a failure within the grace (including the
                // acquisition deadline) is the force's, not its own cause.
                match tokio::time::timeout(CANCELLED_ACQUIRE_GRACE, &mut acquire).await {
                    Ok(Ok(acquired)) => Ok(acquired),
                    Ok(Err(failure)) => Err((WireError::Cancelled, evidence(&failure))),
                    // Abandoned mid-way: nothing proved the group absent.
                    Err(_) => Err((WireError::Cancelled, Evidence::abandoned())),
                }
            }
            acquired = &mut acquire => acquired.map_err(|failure| {
                let evidence = evidence(&failure);
                (WireError::Host(failure.error), evidence)
            }),
        };
        let AcquiredProcess {
            pipes,
            control,
            exits,
        } = match acquired {
            Ok(acquired) => acquired,
            Err((cause, evidence)) => {
                // Dropping the acquisition closes its anchor control: an anchor
                // that connected exits on EOF and stops its group, one that did
                // not at its own bootstrap deadline; Host recovery reports what
                // it can prove. After ARM the vendor pipes are still ours;
                // nothing keeps their bytes, so they are dropped.
                drop(acquire);
                let launched = launch.take().is_some();
                return Err(WireError::Acquire {
                    cause: Box::new(cause),
                    launched,
                    cleanup: evidence.cleanup,
                    forced: evidence.forced,
                    journal_uncertain: evidence.journal_uncertain,
                });
            }
        };
        Ok(Self {
            stdin: Some(pipes.stdin),
            stdout: pipes.stdout,
            stdout_eof: false,
            unterminated_stdout: false,
            buffered: Vec::new(),
            folder,
            undecoded: None,
            control,
            exits,
            cancel,
            wake,
        })
    }

    /// Writes one input message.
    pub async fn write_message(
        &mut self,
        message: &[u8],
        deadline: Deadline,
    ) -> Result<SendOutcome, WireError> {
        let mut written = 0;
        while written < message.len() {
            let Some(stdin) = self.stdin.as_mut() else {
                return Ok(SendOutcome::NotWritten);
            };
            // A pipe write is cancel-safe: a cancelled one wrote nothing.
            let write = tokio::select! {
                biased;
                () = cancelled(&mut self.cancel) => return Err(WireError::Cancelled),
                write = timeout_at(deadline.instant(), stdin.write(&message[written..])) => write,
            };
            let next = match write {
                Ok(Ok(0)) => {
                    self.stdin.take();
                    return Ok(if written == 0 {
                        SendOutcome::NotWritten
                    } else {
                        SendOutcome::Indeterminate
                    });
                }
                Ok(Ok(count)) => count,
                Ok(Err(error)) => {
                    self.stdin.take();
                    return Err(WireError::Io(error));
                }
                Err(_) => {
                    self.stdin.take();
                    return Ok(SendOutcome::Indeterminate);
                }
            };
            written += next;
        }
        // The whole input message (in S1 first the start carrying the prompt) is in the
        // vendor's stdin; nothing it answered is read yet.
        #[cfg(feature = "test-failpoints")]
        via_store::failpoint::hit_async("wire.prompt.after_write")
            .await
            .map_err(WireError::Io)?;
        Ok(SendOutcome::Written)
    }

    /// Drops only vendor stdin; output drains and Host supervision remain live.
    pub async fn close_input(&mut self, deadline: Deadline) -> Result<(), WireError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Ok(());
        };
        let result = timeout_at(deadline.instant(), stdin.shutdown()).await;
        self.stdin.take();
        result
            .map_err(|_| WireError::Deadline)?
            .map_err(WireError::Io)
    }

    /// Returns the next complete stdout message. A line over the cap and an
    /// unterminated tail at EOF are kept in `undecoded.bin` first (design
    /// §7.3); [`Self::take_undecoded`] then names them.
    pub async fn next_message(
        &mut self,
        deadline: Deadline,
    ) -> Result<Option<VendorMessage>, WireError> {
        loop {
            if let Some(index) = self.buffered.iter().position(|byte| *byte == b'\n') {
                if index >= super::MAX_STDOUT_MESSAGE_BYTES {
                    return Err(self.too_large().await);
                }
                let bytes: Vec<u8> = self.buffered.drain(..=index).collect();
                let bounded = BoundedBytes::try_from_message(bytes).map_err(WireError::Message)?;
                return Ok(Some(VendorMessage::new(bounded)));
            }
            if self.buffered.len() > super::MAX_STDOUT_MESSAGE_BYTES {
                return Err(self.too_large().await);
            }
            if self.stdout_eof && !self.buffered.is_empty() {
                self.unterminated_stdout = true;
                let tail = std::mem::take(&mut self.buffered);
                let length = tail.len();
                self.keep_undecoded(
                    &tail,
                    &format!("unterminated vendor message: {length} bytes"),
                )
                .await;
            }
            if self.stdout_eof {
                return if self.unterminated_stdout {
                    Err(WireError::Message(WireFailure::UnterminatedMessage))
                } else {
                    Ok(None)
                };
            }
            Box::pin(self.read_stdout(deadline, false)).await?;
        }
    }

    /// Keeps the over-cap line's first bytes from the assembly buffer.
    async fn too_large(&mut self) -> WireError {
        let line = std::mem::take(&mut self.buffered);
        self.keep_undecoded(
            &line,
            &format!(
                "vendor message over the {} byte cap",
                super::MAX_STDOUT_MESSAGE_BYTES
            ),
        )
        .await;
        WireError::Message(WireFailure::MessageTooLarge)
    }

    /// Writes the first 64 KiB of a message VIA cannot decode to the turn's
    /// `undecoded.bin` (design §7.3): `create_new`, so the first failure
    /// wins, one write on the blocking pool bounded by 2 s. `what` describes
    /// the message; the note it becomes names the file or the error, and is
    /// kept for [`Self::take_undecoded`]. Best effort: nothing fails here.
    pub async fn keep_undecoded(&mut self, bytes: &[u8], what: &str) {
        if self.undecoded.is_some() {
            return;
        }
        let path = self.folder.join("undecoded.bin");
        let prefix = bytes[..bytes.len().min(UNDECODED_BYTES)].to_vec();
        let kept = prefix.len();
        let target = path.clone();
        let write = tokio::task::spawn_blocking(move || write_new(&target, &prefix));
        let note = match tokio::time::timeout(UNDECODED_WRITE, write).await {
            Ok(Ok(Ok(()))) => format!("{what}; first {kept} in {}", path.display()),
            Ok(Ok(Err(error))) => format!("{what}; not saved: {error}"),
            Ok(Err(error)) => format!("{what}; not saved: {error}"),
            Err(_) => format!("{what}; not saved: the write outlived 2 s"),
        };
        self.undecoded = Some(note);
    }

    /// The note of the undecoded message kept for this turn, once.
    pub fn take_undecoded(&mut self) -> Option<String> {
        self.undecoded.take()
    }

    /// Reads and discards stdout until EOF or the cleanup deadline, after a
    /// failure, so the vendor never blocks on a full pipe while its group
    /// stops. Nothing is kept.
    pub async fn drain_to_eof(&mut self, deadline: Deadline) {
        self.stdin.take();
        self.buffered.clear();
        while !self.stdout_eof {
            // A read that is always ready must not extend the drain past its bound.
            if tokio::time::Instant::now() >= deadline.instant()
                || Box::pin(self.read_stdout(deadline, true)).await.is_err()
            {
                break;
            }
        }
    }

    /// Reads one stdout chunk of at most 8 KiB into the assembly buffer, or
    /// discards it in drain mode. Outside drain mode the cancel signal and
    /// Route's wake end the wait before any byte is read.
    async fn read_stdout(&mut self, deadline: Deadline, drain: bool) -> Result<(), WireError> {
        let mut out = [0; 8192];
        tokio::select! {
            () = cancelled(&mut self.cancel), if !drain => return Err(WireError::Cancelled),
            () = woken(&mut self.wake), if !drain => return Err(WireError::Woken),
            read = timeout_at(deadline.instant(), self.stdout.read(&mut out)) => {
                let count = read.map_err(|_| WireError::Deadline)??;
                if count == 0 {
                    self.stdout_eof = true;
                } else if !drain {
                    self.buffered.extend_from_slice(&out[..count]);
                }
            }
        }
        Ok(())
    }

    /// Observes Host-confirmed vendor exit without treating a terminal message as exit proof.
    pub async fn wait_exit(&mut self, deadline: Deadline) -> Result<super::ExitReport, WireError> {
        loop {
            // Copied out so no watch guard is held across the test seam's await.
            let recorded = *self.exits.borrow();
            if let Some(exit) = recorded {
                // A recorded exit is returned without consulting `cancel`: the
                // caller must read the daemon force after it (design §6.8).
                // Test builds pause here, exit recorded and not yet returned.
                #[cfg(feature = "test-failpoints")]
                via_store::failpoint::hit_async("wire.exit.observed")
                    .await
                    .map_err(WireError::Io)?;
                return Ok(exit);
            }
            let changed = tokio::select! {
                () = cancelled(&mut self.cancel) => return Err(WireError::Cancelled),
                () = woken(&mut self.wake) => return Err(WireError::Woken),
                changed = timeout_at(deadline.instant(), self.exits.changed()) => changed,
            };
            changed
                .map_err(|_| WireError::Deadline)?
                // Host dropped its exit supervision: transport loss, not a deadline.
                .map_err(|_| WireError::Message(WireFailure::Transport))?;
        }
    }

    /// Requests Host cleanup through the verified anchor.
    pub async fn close(&self, request: super::CloseRequest) -> WireCloseReport {
        let report = self.control.close(request).await;
        WireCloseReport {
            cleanup: wire_cleanup(&report.cleanup),
            vendor_exit: report.vendor_exit,
            forced: report.forced,
            journal_uncertain: report.journal_uncertain,
        }
    }
}

/// Creates `path` (new, 0600) holding `bytes`, then syncs it and its
/// folder, so the failure message may name it (coding style §7 "Write
/// order"). `create_new` is `O_EXCL`, which never follows a symlink.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let folder = path
        .parent()
        .ok_or_else(|| std::io::Error::other("undecoded.bin has no folder"))?;
    std::fs::File::open(folder)?.sync_all()
}

/// Host's evidence from a failed acquisition, as Wire passes it up.
struct Evidence {
    cleanup: Option<WireCleanup>,
    forced: bool,
    journal_uncertain: bool,
}

impl Evidence {
    /// An acquisition abandoned mid-way: its group, if any, is unproven.
    fn abandoned() -> Self {
        Self {
            cleanup: Some(WireCleanup::Uncertain),
            forced: false,
            journal_uncertain: false,
        }
    }
}

fn evidence(failure: &AcquireFailure) -> Evidence {
    Evidence {
        cleanup: failure.cleanup.as_ref().map(wire_cleanup),
        forced: failure.forced,
        journal_uncertain: failure.journal_uncertain,
    }
}

fn wire_cleanup(cleanup: &CleanupEvidence) -> WireCleanup {
    match cleanup {
        CleanupEvidence::GroupAbsent(_) => WireCleanup::Quiescent,
        CleanupEvidence::Uncertain(_) => WireCleanup::Uncertain,
    }
}

/// Resolves on the next change of Route's wake; never once its sender is gone.
async fn woken(wake: &mut watch::Receiver<u64>) {
    if wake.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Resolves once `cancel` is set; never when its sender is gone unset.
async fn cancelled(cancel: &mut watch::Receiver<Option<tokio::time::Instant>>) {
    if cancel.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn normalize_recovery(report: via_host::RecoveryReport) -> WireRecovery {
    WireRecovery {
        anchor_id: report.anchor_id,
        generation: report.generation,
        owner_session: report.owner_session,
        owner_turn: report.owner_turn,
        cleanup: match report.cleanup {
            via_host::CleanupEvidence::GroupAbsent(_) => super::WireCleanup::Quiescent,
            via_host::CleanupEvidence::Uncertain(_) => super::WireCleanup::Uncertain,
        },
        forced: report.forced,
    }
}

/// Keeps pending/failed joins and the named failure together on every path.
fn summarize_shutdown(report: via_host::ShutdownReport) -> WireShutdown {
    WireShutdown {
        recovery: report
            .recovery
            .into_iter()
            .map(|turn| WireTurnRecovery {
                owner_session: turn.owner_session,
                owner_turn: turn.owner_turn,
                cleanup: match turn.cleanup {
                    via_host::CleanupEvidence::GroupAbsent(_) => super::WireCleanup::Quiescent,
                    via_host::CleanupEvidence::Uncertain(_) => super::WireCleanup::Uncertain,
                },
                forced: turn.forced,
            })
            .collect(),
        anchors: report.anchors,
        uncertain_anchors: report.uncertain_anchors,
        pending_tasks: report.pending_tasks,
        failed_tasks: report.failed_tasks,
        failure: report.failure.map(|error| error.to_string()),
    }
}

/// Passive per-turn shutdown evidence without process signalling authority.
pub struct WireTurnRecovery {
    /// Owning VIA session.
    pub owner_session: via_store::SessionId,
    /// Owning turn.
    pub owner_turn: via_store::TurnNumber,
    /// Quiescent only when every anchor of the turn was proved absent.
    pub cleanup: super::WireCleanup,
    /// Host stopped a group of the turn while its vendor was live.
    pub forced: bool,
}

/// Passive shutdown evidence with no process-control capability.
pub struct WireShutdown {
    /// Per-turn evidence for the requested turns, before any failure.
    pub recovery: Vec<WireTurnRecovery>,
    /// Committed anchors reconciled.
    pub anchors: usize,
    /// Reconciled anchors without positive absence proof.
    pub uncertain_anchors: usize,
    /// Host-owned child/status tasks not joined by the bounded deadline.
    pub pending_tasks: usize,
    /// Host-owned tasks that panicked, were cancelled or failed their child wait.
    pub failed_tasks: usize,
    /// Bounded description of the deadline, Store or recovery failure, if any.
    pub failure: Option<String>,
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[test]
    fn recovery_failure_and_pending_owner_both_survive_the_summary() {
        let summary = summarize_shutdown(via_host::ShutdownReport {
            recovery: Vec::new(),
            anchors: 0,
            uncertain_anchors: 0,
            pending_tasks: 1,
            failed_tasks: 2,
            failure: Some(via_host::HostError::StoreUnavailable(
                via_store::StoreFailureKind::Open,
            )),
        });
        assert_eq!((summary.pending_tasks, summary.failed_tasks), (1, 2));
        assert!(
            summary
                .failure
                .is_some_and(|failure| failure.contains("journal"))
        );
    }
}

#[cfg(test)]
mod undecoded_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A private scratch folder, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> std::io::Result<Scratch> {
        let dir = std::env::temp_dir().join(format!(
            "via-wire-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir(&dir)?;
        Ok(Scratch(dir))
    }

    /// Coding style §7 "Write order": the file and its folder are synced
    /// before the failure note names the file. A folder that can be written
    /// but not opened for its sync makes the save fail.
    #[test]
    fn a_saved_undecoded_file_is_synced_with_its_folder() -> std::io::Result<()> {
        let folder = scratch("synced")?;
        let saved = folder.0.join("undecoded.bin");
        write_new(&saved, b"head")?;
        assert_eq!(std::fs::read(&saved)?, b"head");
        std::fs::remove_file(&saved)?;
        // Write and search only: the file can be created, the folder not
        // opened to sync it.
        std::fs::set_permissions(&folder.0, std::fs::Permissions::from_mode(0o300))?;
        let unsynced = write_new(&saved, b"head");
        std::fs::set_permissions(&folder.0, std::fs::Permissions::from_mode(0o700))?;
        assert!(unsynced.is_err(), "an unsynced folder was reported saved");
        Ok(())
    }
}
