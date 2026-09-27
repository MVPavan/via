//! `daemon/stop` admission, forced-turn settlement and final shutdown.

use std::{
    sync::atomic::Ordering,
    time::{Duration, SystemTime},
};

use via_adapters::Cleanup;

use super::{Engine, Terminal, TurnRecord, lock};
use crate::api::{Cancel, EventBody, FailureClass, Warning, rfc3339};
use crate::{ApiError, DaemonStopParams, Deadline};

/// The C1 §3.14 stop mode Core accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopMode {
    /// No active work: final shutdown at once.
    Idle,
    /// Admission closed; accepted turns finish under their own deadlines first.
    Drain,
    /// Every running turn is closed with mode `force`; final shutdown at once.
    Force,
}

impl StopMode {
    /// The mode word used in the daemon's final shutdown summary.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Drain => "drain",
            Self::Force => "force",
        }
    }
}

/// Passive final-shutdown facts for the daemon's exit decision.
#[derive(Debug)]
pub struct EngineShutdown {
    /// Committed anchors Host reconciled.
    pub anchors: usize,
    /// Process owners whose group absence is unproved.
    pub uncertain_owners: usize,
    /// Host tasks whose result was not collected by the deadline.
    pub pending_tasks: usize,
    /// Host tasks that panicked, were cancelled or failed their child wait.
    pub failed_tasks: usize,
    /// Named Host deadline, Store or recovery failure.
    pub failure: Option<String>,
    /// Force-stopped turns whose cancelled terminal record did not commit.
    pub uncommitted_turns: usize,
    /// Receipted turns with no durable terminal record at final shutdown.
    pub unresolved_turns: usize,
}

impl EngineShutdown {
    /// Clean only with positive cleanup, every join collected and every record committed.
    pub fn is_clean(&self) -> bool {
        self.uncertain_owners == 0
            && self.pending_tasks == 0
            && self.failed_tasks == 0
            && self.failure.is_none()
            && self.uncommitted_turns == 0
            && self.unresolved_turns == 0
    }
}

impl Engine {
    /// Accepts a C1 §3.14 `daemon/stop` and closes admission to new work.
    ///
    /// A plain stop is refused while turns are active. A repeated request keeps
    /// the accepted mode, except that `force` escalates a drain.
    pub async fn request_stop(&self, params: &DaemonStopParams) -> Result<StopMode, ApiError> {
        if params.drain && params.force {
            return Err(ApiError::INVALID_PARAMS);
        }
        let _admission = self.admission.lock().await;
        let mut stop = lock(&self.stop);
        let requested = if params.force {
            StopMode::Force
        } else if params.drain {
            StopMode::Drain
        } else {
            StopMode::Idle
        };
        let mode = match *stop {
            Some(current) if requested != StopMode::Force => current,
            None if requested == StopMode::Idle && self.active() > 0 => {
                return Err(ApiError::SESSIONS_ACTIVE);
            }
            _ => requested,
        };
        *stop = Some(mode);
        if mode == StopMode::Force {
            self.force_requested_at
                .get_or_init(|| rfc3339(SystemTime::now()));
            self.force.send_replace(true);
        }
        Ok(mode)
    }

    /// The accepted `daemon/stop` mode, if any; daemon main reads it on notice.
    pub fn stop_mode(&self) -> Option<StopMode> {
        *lock(&self.stop)
    }

    /// Commits `cancel.settled` for a cancel Core requested at `requested_at`
    /// and returns the envelope's C1 §3.5 `cancel` object.
    pub(super) async fn settle(
        &self,
        record: &mut TurnRecord,
        requested_at: String,
        outcome: &'static str,
        cleanup: &'static str,
    ) -> Cancel {
        self.commit_event(record, EventBody::CancelSettled { outcome, cleanup }, None)
            .await;
        Cancel {
            outcome,
            cleanup,
            requested_at,
            settled_at: rfc3339(SystemTime::now()),
        }
    }

    /// Final shutdown: Host closes live controls, reconciles every anchor and
    /// joins its tasks; then force-stopped turns commit their terminal with that
    /// evidence, and every receipted turn is checked for a durable terminal.
    /// Every step shares the caller's single absolute deadline.
    pub async fn shutdown(&self, deadline: Deadline) -> EngineShutdown {
        // Unprovable group absence must not consume the time to commit terminals.
        let host_by = deadline
            .instant()
            .checked_sub(FORCED_COMMIT_RESERVE)
            .unwrap_or_else(tokio::time::Instant::now);
        let report = self.adapter.shutdown(Deadline::at(host_by)).await;
        let forced = std::mem::take(&mut *lock(&self.forced));
        let mut uncommitted_turns = 0;
        for turn in forced {
            let evidence = report.recovery.iter().find(|record| {
                record.session_id == turn.started.session && record.turn == turn.started.turn
            });
            // C1 §7.6 force row, outcome and cleanup kept independent. Host's
            // proof that its stop found the vendor live ends the turn `cancelled`
            // with `forced`. Before ARM no vendor could launch: `cancelled` and
            // `requested`, cleanup as proved (a complete journal without an
            // anchor intent has nothing to clean). Otherwise a vendor may have
            // run with neither stop nor terminal proved: the turn is `unknown`.
            // Each fact holds if Route's close or recovery proved it, so a failed
            // or late recovery never discards what Route's close proved.
            let quiescent = turn.close.quiescent
                || match evidence {
                    Some(record) => record.cleanup == Cleanup::Quiescent,
                    None => report.failure.is_none(),
                };
            let forced = turn.close.forced || evidence.is_some_and(|record| record.forced);
            let state = if forced || !turn.launched {
                "cancelled"
            } else {
                "unknown"
            };
            let (outcome, cleanup) = stop_outcome(quiescent, forced);
            let commit = async {
                let mut record = turn.record;
                let cancel = self
                    .settle(&mut record, turn.requested_at, outcome, cleanup)
                    .await;
                let mut terminal = Terminal {
                    state,
                    failure: None,
                    // C1 §7.6: an unconfirmed stop, like transport loss, is an error.
                    stop_reason: if state == "cancelled" {
                        "interrupted"
                    } else {
                        "error"
                    },
                    vendor_stop_reason: None,
                    final_text: String::new(),
                    exit: None,
                    raw_ref: None,
                    raw_incomplete: false,
                    warnings: Vec::new(),
                    cancel: Some(cancel),
                };
                if turn.raw_incomplete {
                    // `raw_log.incomplete` committed when the drive ended.
                    terminal.warnings.push(Warning::RAW_LOG_INCOMPLETE);
                }
                if record.store_failed {
                    // C1 §8.2: the durable stream already lost an event; a
                    // cancellation must not present it as a complete record.
                    terminal.fail(FailureClass::Store, "a turn event could not be recorded");
                }
                self.finish(&turn.started, record, terminal, true).await
            };
            if !matches!(
                tokio::time::timeout_at(deadline.instant(), commit).await,
                Ok(Ok(()))
            ) {
                uncommitted_turns += 1;
            }
        }
        let unresolved_turns = self.unresolved_turns(deadline).await;
        self.finalized.store(true, Ordering::Release);
        EngineShutdown {
            anchors: report.recovery.len(),
            uncertain_owners: report
                .recovery
                .iter()
                .filter(|record| record.cleanup != Cleanup::Quiescent)
                .count(),
            pending_tasks: report.pending_tasks,
            failed_tasks: report.failed_tasks,
            failure: report.failure,
            uncommitted_turns,
            unresolved_turns,
        }
    }

    /// Counts receipted turns with no durable terminal, re-reading the Store for
    /// each one not known to have committed (a failed commit may still have).
    async fn unresolved_turns(&self, deadline: Deadline) -> usize {
        let turns = self.unresolved.turns();
        let mut unresolved = 0;
        for (session, turn) in turns {
            let read =
                tokio::time::timeout_at(deadline.instant(), self.store.result(&session, turn));
            if let Ok(Ok(Some(_))) = read.await {
                self.unresolved.resolve(&session, turn);
            } else {
                unresolved += 1;
            }
        }
        unresolved
    }
}

/// C1 §7.4 outcome and §3.5 cleanup of a stop Core ordered, as independent
/// facts: `forced` needs Host's evidence that its stop found the vendor live,
/// otherwise the cancel was only `requested`; cleanup is `quiescent` only with
/// proved group absence.
pub(super) fn stop_outcome(quiescent: bool, forced: bool) -> (&'static str, &'static str) {
    (
        if forced { "forced" } else { "requested" },
        if quiescent { "quiescent" } else { "uncertain" },
    )
}

/// Part of the final deadline Host shutdown leaves for forced turns' terminals.
const FORCED_COMMIT_RESERVE: Duration = Duration::from_secs(1);
