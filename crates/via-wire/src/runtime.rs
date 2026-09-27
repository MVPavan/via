use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::watch,
    time::timeout_at,
};

use super::{
    BoundedBytes, ConnectionId, Deadline, Frame, PrivateProcessSpec, SendOutcome, WireFailure,
};
use via_host::{AcquiredProcess, ExitReceiver, Host, ProcessControl};
use via_store::{DurableRaw, RawFactory, RawStream, RawWriter, RuntimeResources};

/// Raw unit size for a retained line recorded by the failure drain.
const DRAIN_UNIT_BYTES: usize = 64 * 1024;

/// How long an acquisition may still finish once its caller is cancelled: a
/// normal one does, so its group is force-closed and proved absent; a stalled
/// one is abandoned well inside the 10 s final shutdown.
const CANCELLED_ACQUIRE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether every byte exchanged with the vendor reached the durable raw log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawEvidence {
    /// Every byte read or written was durably recorded.
    Complete,
    /// Some bytes were lost or left unread; counters cannot make the log complete.
    Incomplete,
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
    /// Once `cancel` is set, waits for vendor input, output or exit end with
    /// [`WireError::Cancelled`]; bytes already read still reach the raw log.
    pub async fn open_connection(
        &self,
        connection_id: ConnectionId,
        spec: PrivateProcessSpec,
        deadline: Deadline,
        cancel: watch::Receiver<bool>,
    ) -> Result<WireConnection, WireError> {
        WireConnection::open(
            &self.host,
            spec,
            self.raw.open(connection_id),
            deadline,
            cancel,
        )
        .await
    }

    /// Drains Host controls and tasks before the Store owner is released.
    pub async fn shutdown(&self, deadline: Deadline) -> WireShutdown {
        summarize_shutdown(self.host.shutdown(deadline).await)
    }

    /// Reconciles committed anchors only through Host's verified path.
    pub async fn recover(&self, deadline: Deadline) -> Result<Vec<WireRecovery>, WireError> {
        self.host
            .recover(deadline)
            .await
            .map(|reports| reports.into_iter().map(normalize_recovery).collect())
            .map_err(WireError::Host)
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
    /// The caller's cancel signal abandoned an acquisition after ARM: the
    /// vendor may have written output that no raw log recorded.
    #[error("acquisition cancelled after vendor launch")]
    CancelledAfterLaunch,
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
    cancel: watch::Receiver<bool>,
}

impl WireConnection {
    /// Acquires one Host-owned process after the caller's durable submission intent.
    async fn open(
        host: &Host,
        spec: PrivateProcessSpec,
        raw: RawWriter,
        deadline: Deadline,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<Self, WireError> {
        let armed = std::sync::atomic::AtomicBool::new(false);
        let acquire = host.acquire_marking_arm(spec, deadline, &armed);
        tokio::pin!(acquire);
        let acquired = tokio::select! {
            acquired = &mut acquire => acquired,
            () = cancelled(&mut cancel) => {
                // An abandoned acquisition drops its anchor control: an anchor
                // that connected exits on EOF, one that did not at its own
                // bootstrap deadline; Host recovery reports what it can prove.
                // After ARM the pipes carry vendor bytes nobody will record.
                tokio::time::timeout(CANCELLED_ACQUIRE_GRACE, &mut acquire)
                    .await
                    .map_err(|_| {
                        if armed.load(std::sync::atomic::Ordering::Acquire) {
                            WireError::CancelledAfterLaunch
                        } else {
                            WireError::Cancelled
                        }
                    })?
            }
        };
        let AcquiredProcess {
            pipes,
            control,
            exits,
        } = acquired?;
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
            if let Some(exit) = *self.exits.borrow() {
                return Ok(exit);
            }
            let changed = tokio::select! {
                () = cancelled(&mut self.cancel) => return Err(WireError::Cancelled),
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
            cleanup: match report.cleanup {
                via_host::CleanupEvidence::GroupAbsent(_) => super::WireCleanup::Quiescent,
                via_host::CleanupEvidence::Uncertain(_) => super::WireCleanup::Uncertain,
            },
            vendor_exit: report.vendor_exit,
            forced: report.forced,
        }
    }
}

/// Resolves once `cancel` is set; never when its sender is gone unset.
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    if cancel.wait_for(|cancel| *cancel).await.is_err() {
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
            .map(normalize_recovery)
            .collect(),
        pending_tasks: report.pending_tasks,
        failed_tasks: report.failed_tasks,
        failure: report.failure.map(|error| error.to_string()),
    }
}

/// Passive shutdown evidence with no process-control capability.
pub struct WireShutdown {
    /// Committed anchors reconciled before any failure.
    pub recovery: Vec<WireRecovery>,
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
