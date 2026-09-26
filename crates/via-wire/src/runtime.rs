use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout_at,
};

use super::{
    BoundedBytes, ConnectionId, Deadline, Frame, PrivateProcessSpec, SendOutcome, WireFailure,
};
use via_host::{AcquiredProcess, ExitReceiver, Host, ProcessControl};
use via_store::{RawFactory, RawStream, RawWriter, RuntimeResources};

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
    pub async fn open_connection(
        &self,
        connection_id: ConnectionId,
        spec: PrivateProcessSpec,
        deadline: Deadline,
    ) -> Result<WireConnection, WireError> {
        WireConnection::open(&self.host, spec, self.raw.open(connection_id), deadline).await
    }

    /// Drains Host controls and tasks before the Store owner is released.
    pub async fn shutdown(&self, deadline: Deadline) -> Result<WireShutdown, WireError> {
        let report = self.host.shutdown(deadline).await?;
        Ok(WireShutdown {
            recovery: report
                .recovery
                .into_iter()
                .map(normalize_recovery)
                .collect(),
            pending_tasks: report.pending_tasks,
        })
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
}

/// Private-group close evidence without Host identity or signal authority.
pub struct WireCloseReport {
    /// Group cleanup certainty.
    pub cleanup: super::WireCleanup,
    /// Independently confirmed vendor exit if available.
    pub vendor_exit: Option<super::ExitReport>,
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
    control: ProcessControl,
    exits: ExitReceiver,
}

impl WireConnection {
    /// Acquires one Host-owned process after the caller's durable submission intent.
    async fn open(
        host: &Host,
        spec: PrivateProcessSpec,
        raw: RawWriter,
        deadline: Deadline,
    ) -> Result<Self, WireError> {
        let AcquiredProcess {
            pipes,
            control,
            exits,
        } = host.acquire(spec, deadline).await?;
        Ok(Self {
            stdin: Some(pipes.stdin),
            stdout: pipes.stdout,
            stderr: pipes.stderr,
            stdout_eof: false,
            stderr_eof: false,
            unterminated_stdout: false,
            buffered: Vec::new(),
            raw,
            control,
            exits,
        })
    }

    /// Writes one outbound frame and durably records only successfully written prefixes.
    pub async fn write_frame(
        &mut self,
        frame: &[u8],
        deadline: Deadline,
    ) -> Result<SendOutcome, WireError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Ok(SendOutcome::NotWritten);
        };
        let mut written = 0;
        while written < frame.len() {
            let next = match timeout_at(deadline.instant(), stdin.write(&frame[written..])).await {
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
            self.raw
                .append(RawStream::Stdin, frame[written..written + next].to_vec())
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
                let bytes: Vec<u8> = self.buffered.drain(..=index).collect();
                let bounded = BoundedBytes::try_from_frame(bytes).map_err(WireError::Frame)?;
                let token = self
                    .raw
                    .append(RawStream::Stdout, bounded.as_bytes().to_vec())
                    .await?;
                return Frame::new(bounded, token.raw_ref().clone())
                    .map(Some)
                    .map_err(WireError::Frame);
            }
            if self.buffered.len() > super::MAX_STDOUT_FRAME_BYTES {
                return Err(WireError::Frame(WireFailure::FrameTooLarge));
            }
            if self.stdout_eof && !self.buffered.is_empty() {
                self.raw
                    .append(RawStream::Stdout, std::mem::take(&mut self.buffered))
                    .await?;
                self.unterminated_stdout = true;
            }
            if self.stdout_eof && self.stderr_eof {
                return if self.unterminated_stdout {
                    Err(WireError::Frame(WireFailure::UnterminatedFrame))
                } else {
                    Ok(None)
                };
            }
            let mut out = [0; 8192];
            let mut err = [0; 8192];
            tokio::select! {
                read = timeout_at(deadline.instant(), self.stdout.read(&mut out)), if !self.stdout_eof => {
                    let count = read.map_err(|_| WireError::Deadline)??;
                    if count == 0 { self.stdout_eof = true; } else { self.buffered.extend_from_slice(&out[..count]); }
                }
                read = timeout_at(deadline.instant(), self.stderr.read(&mut err)), if !self.stderr_eof => {
                    let count = read.map_err(|_| WireError::Deadline)??;
                    if count == 0 { self.stderr_eof = true; } else {
                        self.raw.append(RawStream::Stderr, err[..count].to_vec()).await?;
                    }
                }
            }
        }
    }

    /// Observes Host-confirmed vendor exit without treating a terminal frame as exit proof.
    pub async fn wait_exit(&mut self, deadline: Deadline) -> Result<super::ExitReport, WireError> {
        loop {
            if let Some(exit) = *self.exits.borrow() {
                return Ok(exit);
            }
            timeout_at(deadline.instant(), self.exits.changed())
                .await
                .map_err(|_| WireError::Deadline)?
                .map_err(|_| WireError::Deadline)?;
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
        }
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
    }
}

/// Passive shutdown evidence with no process-control capability.
pub struct WireShutdown {
    /// Every committed anchor in the Store journal.
    pub recovery: Vec<WireRecovery>,
    /// Host-owned child/status tasks not joined by the bounded deadline.
    pub pending_tasks: usize,
}
