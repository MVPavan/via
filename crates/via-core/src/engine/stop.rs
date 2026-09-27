//! `daemon/stop` admission, forced-turn settlement and final shutdown.

use std::{
    sync::atomic::Ordering,
    time::{Duration, SystemTime},
};

use via_adapters::Cleanup;

use std::sync::Arc;

use super::drive::FORCE_CLOSE_REASON;
use super::journal::{self, Head};
use super::{Admission, Engine, Terminal, TurnRecord, lock};
use crate::api::{Cancel, Event, EventBody, FailureClass, Warning, rfc3339};
use crate::{ApiError, DaemonStopParams, Deadline, SessionId, TurnNumber};

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
    /// A state write failed or was uncertain (runtime §7).
    pub store_failed: bool,
    /// Sessions whose dispatcher was requested but never ran.
    pub unstarted_dispatchers: usize,
    /// Sessions a force stop found with dispatch state that could not be
    /// closed durably (C1 §3.14).
    pub unclosed_sessions: usize,
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
            && !self.store_failed
            && self.unstarted_dispatchers == 0
            && self.unclosed_sessions == 0
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
        let requested = if params.force {
            StopMode::Force
        } else if params.drain {
            StopMode::Drain
        } else {
            StopMode::Idle
        };
        let mode = {
            let mut stop = lock(&self.stop);
            // `active` counts orphans and turns whose commits failed, so an
            // idle stop cannot skip them.
            let mode = match *stop {
                Some(current) if requested != StopMode::Force => current,
                None if requested == StopMode::Idle && self.active() > 0 => {
                    return Err(ApiError::SESSIONS_ACTIVE);
                }
                _ => requested,
            };
            *stop = Some(mode);
            mode
        };
        // `stop` is released first: dispatchers and the reconciler wake on the watch.
        if mode == StopMode::Force {
            // The sessions final shutdown's closure pass closes (C1 §3.14).
            lock(&self.force_sessions)
                .get_or_insert_with(|| lock(&self.sessions).keys().cloned().collect());
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
        // Host keeps evidence only for receipted, unresolved turns (at most
        // the unresolved cap), which include every force-stopped turn.
        let turns = self.unresolved.turns();
        let report = self.adapter.shutdown(Deadline::at(host_by), &turns).await;
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
                // C1 §3.14: close only once every other turn of the session
                // has a durable disposition. Design §3.2: once
                // `failure_pending` is observed no new close-bearing commit
                // starts; Store refuses `session.closed` in the same
                // transaction while any other turn is queued or running, which
                // covers a turn an uncertain receipt committed unregistered.
                let admission = self.admission.lock().await;
                let close = !self.store_failed()
                    && !self
                        .unresolved
                        .others(&turn.started.session, turn.started.turn);
                self.finish(&turn.started, record, terminal, close, Some(&admission))
                    .await
            };
            if !matches!(
                tokio::time::timeout_at(deadline.instant(), commit).await,
                Ok(Ok(()))
            ) {
                uncommitted_turns += 1;
            }
        }
        let unclosed_sessions =
            tokio::time::timeout_at(deadline.instant(), self.close_forced_sessions())
                .await
                .unwrap_or_else(|_| lock(&self.force_sessions).as_ref().map_or(1, Vec::len));
        let unresolved_turns = self.unresolved_turns(deadline).await;
        let unstarted_dispatchers = lock(&self.sessions)
            .values()
            .filter(|slot| slot.starting())
            .count();
        self.finalized.store(true, Ordering::Release);
        EngineShutdown {
            anchors: report.anchors,
            uncertain_owners: report.uncertain_anchors,
            pending_tasks: report.pending_tasks,
            failed_tasks: report.failed_tasks,
            failure: report.failure,
            uncommitted_turns,
            unresolved_turns,
            store_failed: self.store_failed(),
            unstarted_dispatchers,
            unclosed_sessions,
        }
    }

    /// Final shutdown's force closure pass, after the dispatchers joined: under
    /// `admission`, each session with dispatch state at force acceptance that
    /// is still open in Store is closed with `session.closed`
    /// (`daemon_stop_force`) once every turn has a durable disposition.
    /// Returns how many could not be closed. Skipped after a Store failure,
    /// whose exit is already incomplete; a session already closed in-path is
    /// read as closed and never closed twice.
    async fn close_forced_sessions(&self) -> usize {
        let sessions = lock(&self.force_sessions).clone().unwrap_or_default();
        let mut unclosed = 0;
        for session in sessions {
            let admission = self.admission.lock().await;
            if self.store_failed() {
                return 0;
            }
            if !self.close_forced(&session, &admission).await {
                unclosed += 1;
            }
        }
        lock(&self.force_sessions).take();
        unclosed
    }

    /// Closes one session for the closure pass; true when it is durably closed.
    async fn close_forced(&self, session: &SessionId, admission: &Admission<'_>) -> bool {
        let Ok(Some(snapshot)) = self.store.session_snapshot(session).await else {
            return false;
        };
        if snapshot.closed {
            return true;
        }
        let Ok(next) = TurnNumber::try_from(snapshot.turns + 1) else {
            return false;
        };
        if !matches!(self.store.predecessors(session, next).await, Ok(p) if !p.unresolved) {
            return false;
        }
        let head = self
            .slot(session)
            .map_or_else(|| Head::new(None), |slot| Arc::clone(&slot.head));
        let Ok(guard) = head.lock(&self.store, session).await else {
            return false;
        };
        let Ok(closed) = (Event {
            seq: guard.next(),
            session_id: session,
            turn: None,
            late: false,
            at: &rfc3339(SystemTime::now()),
            raw_ref: None,
            body: EventBody::SessionClosed {
                reason: FORCE_CLOSE_REASON,
            },
        })
        .to_value() else {
            return false;
        };
        match self.store.commit_session_closed(session, closed).await {
            Ok(true) => {
                guard.committed(1);
                true
            }
            // Store found the session closed or a turn unfinished: nothing written.
            Ok(false) => false,
            Err(error) => {
                if journal::may_have_committed(&error) {
                    guard.lost();
                }
                self.latch_held(admission);
                false
            }
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
