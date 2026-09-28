use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::watch,
    time::timeout_at,
};

use super::{
    BoundedBytes, ConnectionId, Deadline, Frame, PrivateProcessSpec, SendOutcome, WireCleanup,
    WireFailure,
};
use via_host::{
    AcquireFailure, AcquiredProcess, CleanupEvidence, ExitReceiver, Host, LaunchPipes, OwnedPipes,
    ProcessControl,
};
use via_store::{DurableRaw, RawFactory, RawStream, RawWriter, RuntimeResources};

/// Raw unit size for a retained line recorded by the failure drain.
const DRAIN_UNIT_BYTES: usize = 64 * 1024;

/// How long an acquisition may still finish once its caller is cancelled: a
/// normal one does, so its group is force-closed and proved absent; a stalled
/// one is abandoned well inside the 10 s final shutdown.
const CANCELLED_ACQUIRE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Bound on draining the vendor pipes of an acquisition that failed after ARM.
const LAUNCH_DRAIN: std::time::Duration = std::time::Duration::from_secs(3);

/// Whether every byte exchanged with the vendor reached the durable raw log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawEvidence {
    /// Every byte read or written was durably recorded.
    Complete,
    /// Some bytes were lost or left unread; counters cannot make the log complete.
    Incomplete,
}

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

/// Wire owns the raw and process capabilities split from one Store owner.
pub struct WireRuntime {
    raw: RawFactory,
    host: Host,
}

impl WireRuntime {
    /// Consumes the unopened Store bundle at the sole production split site.
    pub fn new(config: RuntimeConfig, resources: RuntimeResources) -> Result<Self, WireError> {
        let (raw, journal) = resources.into_wire_parts();
        let host = Host::new(journal, config.anchor_binary, config.anchor_dir)?;
        Ok(Self { raw, host })
    }

    /// Opens one private connection with a Store-owned synced raw writer.
    /// Once `signals.force` is set, waits for vendor input, output or exit
    /// end with [`WireError::Cancelled`]; bytes already read still reach the
    /// raw log. A failed acquisition is [`WireError::Acquire`] with Host's
    /// cleanup evidence.
    pub async fn open_connection(
        &self,
        connection_id: ConnectionId,
        spec: PrivateProcessSpec,
        deadline: Deadline,
        signals: WireSignals,
    ) -> Result<WireConnection, WireError> {
        Box::pin(WireConnection::open(
            &self.host,
            spec,
            self.raw.open(connection_id),
            deadline,
            signals,
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
            Self::Raw(_)
                | Self::Host(
                    via_host::HostError::Store(_)
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
    /// Raw evidence could not be persisted before delivery.
    #[error("raw evidence failed: {0}")]
    Raw(#[from] via_store::StoreError),
    /// A pipe operation failed.
    #[error("vendor pipe failed: {0}")]
    Io(#[from] std::io::Error),
    /// A pipe operation crossed its absolute deadline.
    #[error("vendor pipe deadline elapsed")]
    Deadline,
    /// A raw evidence append was not confirmed by its absolute deadline.
    #[error("raw evidence append deadline elapsed")]
    RawDeadline,
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
        /// Whether every byte the vendor wrote reached the raw log.
        raw: RawEvidence,
        /// Host's bounded absence verification; `None` when no anchor
        /// intent was committed, so no group can exist.
        cleanup: Option<WireCleanup>,
        /// Host's `Stop` stopped a live vendor.
        forced: bool,
        /// A journal write had an uncertain outcome: the daemon must latch.
        journal_uncertain: bool,
    },
    /// Frame contract failure.
    #[error("vendor frame failure: {0:?}")]
    Frame(WireFailure),
}

/// Exclusive transport for one private fake process and one raw connection.
pub struct WireConnection {
    stdin: Option<tokio::process::ChildStdin>,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    stdout_eof: bool,
    stderr_eof: bool,
    unterminated_stdout: bool,
    buffered: Vec<u8>,
    raw: RawWriter,
    /// Latched `Incomplete` once any byte read from or written to the vendor was not recorded.
    evidence: RawEvidence,
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
        raw: RawWriter,
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
                // it can prove. After ARM the vendor pipes are still ours.
                drop(acquire);
                let (launched, raw) = match launch.take() {
                    Some(pipes) => (true, Box::pin(drain_pipes(pipes, &raw)).await),
                    None => (false, RawEvidence::Complete),
                };
                return Err(WireError::Acquire {
                    cause: Box::new(cause),
                    launched,
                    raw,
                    cleanup: evidence.cleanup,
                    forced: evidence.forced,
                    journal_uncertain: evidence.journal_uncertain,
                });
            }
        };
        Ok(Self {
            stdin: Some(pipes.stdin),
            stdout: pipes.stdout,
            stderr: pipes.stderr,
            stdout_eof: false,
            stderr_eof: false,
            unterminated_stdout: false,
            buffered: Vec::new(),
            raw,
            evidence: RawEvidence::Complete,
            control,
            exits,
            cancel,
            wake,
        })
    }

    /// Writes one outbound frame and durably records only successfully written prefixes.
    pub async fn write_frame(
        &mut self,
        frame: &[u8],
        deadline: Deadline,
    ) -> Result<SendOutcome, WireError> {
        let mut written = 0;
        while written < frame.len() {
            let Some(stdin) = self.stdin.as_mut() else {
                return Ok(SendOutcome::NotWritten);
            };
            // A pipe write is cancel-safe: a cancelled one wrote nothing.
            let write = tokio::select! {
                biased;
                () = cancelled(&mut self.cancel) => return Err(WireError::Cancelled),
                write = timeout_at(deadline.instant(), stdin.write(&frame[written..])) => write,
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
            self.record(
                RawStream::Stdin,
                frame[written..written + next].to_vec(),
                deadline,
            )
            .await?;
            written += next;
        }
        // The whole frame (in S1 first the start carrying the prompt) is in the
        // vendor's stdin; nothing it answered is recorded yet.
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

    /// Returns a frame only after Store has synced its exact raw bytes.
    pub async fn next_frame(&mut self, deadline: Deadline) -> Result<Option<Frame>, WireError> {
        loop {
            if let Some(index) = self.buffered.iter().position(|byte| *byte == b'\n') {
                // An oversized line stays buffered so the failure drain records it.
                if index >= super::MAX_STDOUT_FRAME_BYTES {
                    return Err(WireError::Frame(WireFailure::FrameTooLarge));
                }
                let bytes: Vec<u8> = self.buffered.drain(..=index).collect();
                let bounded = BoundedBytes::try_from_frame(bytes).map_err(WireError::Frame)?;
                let token = self
                    .record(RawStream::Stdout, bounded.as_bytes().to_vec(), deadline)
                    .await?;
                return Frame::new(bounded, token.raw_ref().clone())
                    .map(Some)
                    .map_err(WireError::Frame);
            }
            if self.buffered.len() > super::MAX_STDOUT_FRAME_BYTES {
                return Err(WireError::Frame(WireFailure::FrameTooLarge));
            }
            if self.stdout_eof && !self.buffered.is_empty() {
                self.unterminated_stdout = true;
                let tail = std::mem::take(&mut self.buffered);
                self.record(RawStream::Stdout, tail, deadline).await?;
            }
            if self.stdout_eof && self.stderr_eof {
                return if self.unterminated_stdout {
                    Err(WireError::Frame(WireFailure::UnterminatedFrame))
                } else {
                    Ok(None)
                };
            }
            Box::pin(self.read_either(deadline, false)).await?;
        }
    }

    /// Records every remaining byte of both pipes, unframed, until both reach EOF or
    /// the cleanup deadline. Used after a failure, when framing no longer decides
    /// protocol meaning. A failed or expired append never stops the drain: later
    /// bytes are read and discarded until the same deadline, and the result says the
    /// raw log is incomplete.
    pub async fn drain_to_eof(&mut self, deadline: Deadline) -> RawEvidence {
        self.stdin.take();
        // A retained oversized line may exceed the raw unit cap; store it in 64 KiB units.
        let retained = std::mem::take(&mut self.buffered);
        for chunk in retained.chunks(DRAIN_UNIT_BYTES) {
            // A failure latches `Incomplete`; the remaining chunks are discarded.
            let _recorded = self
                .record(RawStream::Stdout, chunk.to_vec(), deadline)
                .await;
        }
        while !(self.stdout_eof && self.stderr_eof) {
            // A read that is always ready must not extend the drain past its bound.
            if tokio::time::Instant::now() >= deadline.instant() {
                self.evidence = RawEvidence::Incomplete;
                break;
            }
            match Box::pin(self.read_either(deadline, true)).await {
                // A raw failure is latched and later reads are discarded.
                Ok(()) | Err(WireError::Raw(_) | WireError::RawDeadline) => {}
                // Bytes may remain unread in the pipes.
                Err(_) => {
                    self.evidence = RawEvidence::Incomplete;
                    break;
                }
            }
        }
        self.evidence
    }

    /// Durably appends one unit unless the log already lost bytes; any failure
    /// latches `Incomplete`, because the unit's bytes can no longer be recorded.
    /// The wait for Store's worker ends at `deadline`, so a stalled worker cannot
    /// hold the caller past its bound; an unconfirmed unit counts as lost.
    async fn record(
        &mut self,
        stream: RawStream,
        bytes: Vec<u8>,
        deadline: Deadline,
    ) -> Result<DurableRaw, WireError> {
        if self.evidence == RawEvidence::Incomplete {
            return Err(WireError::Frame(WireFailure::RawStore));
        }
        let result = match timeout_at(deadline.instant(), self.raw.append(stream, bytes)).await {
            Ok(appended) => appended.map_err(WireError::Raw),
            Err(_) => Err(WireError::RawDeadline),
        };
        if result.is_err() {
            self.evidence = RawEvidence::Incomplete;
        }
        result
    }

    /// Reads one chunk from whichever open pipe is ready; stderr is always raw-logged
    /// at once, so neither stream waits for the other. At most one 8 KiB chunk is
    /// staged per call. Reads are cancel-safe, but dropping this future during a raw
    /// append loses that chunk; Route awaits it to completion or to the deadline.
    /// Once the raw log is incomplete, drain-mode chunks are read and discarded.
    /// Outside drain mode, the cancel signal ends the wait before any byte is read.
    async fn read_either(&mut self, deadline: Deadline, stdout_raw: bool) -> Result<(), WireError> {
        let mut out = [0; 8192];
        let mut err = [0; 8192];
        tokio::select! {
            () = cancelled(&mut self.cancel), if !stdout_raw => return Err(WireError::Cancelled),
            () = woken(&mut self.wake), if !stdout_raw => return Err(WireError::Woken),
            read = timeout_at(deadline.instant(), self.stdout.read(&mut out)), if !self.stdout_eof => {
                let count = read.map_err(|_| WireError::Deadline)??;
                if count == 0 {
                    self.stdout_eof = true;
                } else if stdout_raw {
                    if self.evidence == RawEvidence::Complete {
                        self.record(RawStream::Stdout, out[..count].to_vec(), deadline)
                            .await?;
                    }
                } else {
                    self.buffered.extend_from_slice(&out[..count]);
                }
            }
            read = timeout_at(deadline.instant(), self.stderr.read(&mut err)), if !self.stderr_eof => {
                let count = read.map_err(|_| WireError::Deadline)??;
                if count == 0 {
                    self.stderr_eof = true;
                } else if !stdout_raw || self.evidence == RawEvidence::Complete {
                    self.record(RawStream::Stderr, err[..count].to_vec(), deadline)
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Observes Host-confirmed vendor exit without treating a terminal frame as exit proof.
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
                .map_err(|_| WireError::Frame(WireFailure::Transport))?;
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

/// Records both vendor output pipes of a failed acquisition until EOF or a
/// bounded cleanup deadline: `Complete` only if both reached EOF and every
/// chunk was durably appended.
async fn drain_pipes(pipes: OwnedPipes, raw: &RawWriter) -> RawEvidence {
    let deadline = tokio::time::Instant::now() + LAUNCH_DRAIN;
    let OwnedPipes {
        stdin,
        mut stdout,
        mut stderr,
    } = pipes;
    drop(stdin);
    let (mut stdout_eof, mut stderr_eof) = (false, false);
    let mut evidence = RawEvidence::Complete;
    let (mut out, mut err) = ([0; 8192], [0; 8192]);
    while !(stdout_eof && stderr_eof) {
        let read = timeout_at(deadline, async {
            tokio::select! {
                read = stdout.read(&mut out), if !stdout_eof => (RawStream::Stdout, read),
                read = stderr.read(&mut err), if !stderr_eof => (RawStream::Stderr, read),
            }
        })
        .await;
        // A failed or late read may leave bytes unread in the pipes.
        let Ok((stream, Ok(count))) = read else {
            return RawEvidence::Incomplete;
        };
        let (bytes, eof) = if stream == RawStream::Stdout {
            (&out[..count], &mut stdout_eof)
        } else {
            (&err[..count], &mut stderr_eof)
        };
        if count == 0 {
            *eof = true;
        } else if evidence == RawEvidence::Complete
            && !matches!(
                timeout_at(deadline, raw.append(stream, bytes.to_vec())).await,
                Ok(Ok(_))
            )
        {
            // Later bytes are still read to EOF, but the log has a gap.
            evidence = RawEvidence::Incomplete;
        }
    }
    evidence
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
