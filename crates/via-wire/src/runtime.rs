use thiserror::Error;
use tokio::sync::watch;

use super::{Deadline, PrivateProcessSpec, WireCleanup, WireFailure};
use crate::connection::{self, Stragglers, Waits, WireConnection, cancelled};
use via_host::{AcquireFailure, AcquiredProcess, CleanupEvidence, Host, LaunchPipes};
use via_store::{EvidenceRoot, RuntimeResources};

/// How long an acquisition may still finish once its caller is cancelled: a
/// normal one does, so its group is force-closed and proved absent; a stalled
/// one is abandoned well inside the 10 s final shutdown.
const CANCELLED_ACQUIRE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The signals Route hands Wire for one connection (design §2).
pub struct WireSignals {
    /// The daemon force watch: once set, the acquisition and
    /// [`crate::WireMessages::next_message`] end with [`WireError::Cancelled`].
    pub force: watch::Receiver<Option<tokio::time::Instant>>,
    /// Route's wake: each change ends the current `next_message` once with
    /// [`WireError::Woken`]; queued messages are kept, so nothing is lost.
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
    /// Connection tasks that missed their join bound (design §8.6).
    stragglers: Stragglers,
}

impl WireRuntime {
    /// Consumes the unopened Store bundle at the sole production split site.
    pub fn new(config: RuntimeConfig, resources: RuntimeResources) -> Result<Self, WireError> {
        let (evidence, journal) = resources.into_wire_parts();
        let host = Host::new(journal, config.anchor_binary, config.anchor_dir)?;
        Ok(Self {
            evidence,
            host,
            stragglers: Stragglers::default(),
        })
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
        Box::pin(open(
            &self.host,
            spec,
            folder,
            deadline,
            (signals, &self.stragglers),
        ))
        .await
    }

    /// Drains Host controls and tasks before the Store owner is released;
    /// connection tasks still unjoined by `deadline` count as pending.
    pub async fn shutdown(
        &self,
        deadline: Deadline,
        turns: &[(via_store::SessionId, via_store::TurnNumber)],
    ) -> WireShutdown {
        let mut summary = summarize_shutdown(self.host.shutdown(deadline, turns).await);
        self.stragglers.join_until(deadline).await;
        summary.pending_tasks += self.stragglers.pending();
        summary.failed_tasks += self.stragglers.failed();
        summary
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

    /// Positive evidence that a vendor of one of `anchors` is live (Task 4
    /// design §11.3 `process.alive`).
    pub fn live_armed(&self, anchors: &[String]) -> bool {
        self.host.live_armed(anchors)
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

/// Acquires one Host-owned process after the caller's durable submission
/// intent, then starts its reader and writer tasks.
async fn open(
    host: &Host,
    spec: PrivateProcessSpec,
    folder: std::path::PathBuf,
    deadline: Deadline,
    (signals, stragglers): (WireSignals, &Stragglers),
) -> Result<WireConnection, WireError> {
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
    let waits = Waits {
        force: cancel,
        wake,
    };
    Ok(connection::open(
        pipes, control, exits, folder, waits, stragglers,
    ))
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

pub(crate) fn wire_cleanup(cleanup: &CleanupEvidence) -> WireCleanup {
    match cleanup {
        CleanupEvidence::GroupAbsent(_) => WireCleanup::Quiescent,
        CleanupEvidence::Uncertain(_) => WireCleanup::Uncertain,
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
    /// Host-owned child/status tasks and connection tasks not joined by the
    /// bounded deadline.
    pub pending_tasks: usize,
    /// Host-owned tasks that panicked, were cancelled or failed their child
    /// wait, and connection tasks that panicked.
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
