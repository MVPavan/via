//! `daemon/stop` admission, forced-turn settlement and final shutdown.

use std::{collections::HashSet, sync::atomic::Ordering, time::SystemTime};

use via_adapters::{Cleanup, StopCause};

use std::sync::Arc;

use super::batch::{self, AffectedTurn, FailureBatches};
use super::drive::FORCE_CLOSE_REASON;
use super::journal::Head;
use super::latch::{
    ABORTED_JOIN, FINALIZE_RESERVE, FINALIZE_WRITE, FailureScope, FailureSite, WriteOutcome,
};
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
    /// Dispatchers that had not joined when final shutdown settled: their
    /// sessions' turns were left to restart recovery (design §6.8 step 3).
    pub unjoined_dispatchers: usize,
    /// Failure-resolution batches committed or skipped (design §7.4).
    pub failure_batches: FailureBatches,
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
            && self.unjoined_dispatchers == 0
    }
}

/// What final-shutdown entry found under `admission` (design §6.8 entry
/// step 2). It only feeds the pipeline's start drain; daemon main never
/// returns to serving work [r4.7].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FinalEntry {
    /// Receipted turns not yet settled.
    pub active: usize,
    /// Starts that found the start channel full.
    pub pending_starts: usize,
    /// Starts in the start channel.
    pub queued_starts: usize,
}

impl Engine {
    /// Accepts a C1 §3.14 `daemon/stop` and closes admission to new work.
    ///
    /// A plain stop is refused while turns are active or a session is in the
    /// durable closing set [r3.5]. A repeated request keeps the accepted
    /// mode, except that `force` escalates a drain. Under `admission`, the
    /// closing set and the force set are each read and released before
    /// `stop` is taken, which is always taken alone [r5.12]; the force set
    /// is inserted after `stop` is released. Wakes: the force watch.
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
        // Plain-stop check order [r5.12]: the closing set's mutex is released
        // before `stop` is taken.
        let closing = !self.closing_empty();
        // The force set is collected under `sessions` and slot state, then
        // those locks are released (design §6.3 [r5.12]).
        let force_set = (requested == StopMode::Force).then(|| self.unfinished_sessions());
        let mode = {
            let mut stop = lock(&self.signal.stop);
            // `active` counts orphans and turns whose commits failed, so an
            // idle stop cannot skip them.
            let mode = match *stop {
                Some(current) if requested != StopMode::Force => current,
                None if requested == StopMode::Idle && (self.active() > 0 || closing) => {
                    return Err(ApiError::SESSIONS_ACTIVE);
                }
                _ => requested,
            };
            *stop = Some(mode);
            mode
        };
        // `stop` is released first: dispatchers and the reconciler wake on the watch.
        if let (StopMode::Force, Some(set)) = (mode, force_set) {
            // The sessions final shutdown's closure pass closes (C1 §3.14,
            // O3): inserted alone, never nested with another `std` lock.
            lock(&self.force_sessions).get_or_insert(set);
            self.signal
                .force_requested_at
                .get_or_init(|| rfc3339(SystemTime::now()));
            self.signal.force.send_replace(true);
        }
        Ok(mode)
    }

    /// The force set (design §6.3 [O3], amendment A18): sessions whose slot
    /// has a queue entry (`Waiting`, `Claimed` or `Cancelling`) or a running
    /// or settling turn. A slot whose dispatcher is merely exiting with an
    /// empty queue, or that carries only a close order, is not in it. Taken
    /// under `admission`: `sessions`, then each slot's state, each released
    /// before the next. Each slot is read in one slot-state section
    /// ([`super::Slot::unfinished`]), which a dispatcher's queue-to-running
    /// move cannot split.
    pub(super) fn unfinished_sessions(&self) -> Vec<SessionId> {
        let slots: Vec<(SessionId, Arc<super::Slot>)> = lock(&self.sessions)
            .iter()
            .map(|(session, slot)| (session.clone(), Arc::clone(slot)))
            .collect();
        slots
            .into_iter()
            .filter(|(_, slot)| slot.unfinished())
            .map(|(session, _)| session)
            .collect()
    }

    /// Daemon main stops accepting work (design §6.8 entry [r3.2, r4.7]):
    /// at drain end, at idle expiry, at force acceptance and on the latch's
    /// force signal [r5.11]. Under `admission` it sets the `final_shutdown`
    /// fence and re-checks, at the same point, the active count and the
    /// pending and queued starts, for the pipeline's start drain. From then
    /// on new close work is refused `daemon_stopping`, while a keyed replay
    /// of a committed close still replays. A close that committed `Closing`
    /// before entry already requested its dispatcher start under
    /// `admission`, so the start drain sees it. Lock: `admission`, then the
    /// pending-start mutex alone. Wakes: the re-probe loop, on the fence's
    /// watch.
    pub async fn enter_final_shutdown(&self) -> FinalEntry {
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("daemon.shutdown.before_fence").await;
        let entry = {
            let admission = self.admission.lock().await;
            // Phase two of a latch Store's read-corruption observer began.
            self.finish_pending(&admission);
            self.final_shutdown.send_replace(true);
            FinalEntry {
                active: self.active(),
                pending_starts: lock(&self.pending_starts).len(),
                queued_starts: self
                    .starts
                    .max_capacity()
                    .saturating_sub(self.starts.capacity()),
            }
        };
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("daemon.shutdown.after_fence").await;
        entry
    }

    /// Whether final shutdown was entered: the close fence reads it under
    /// `admission` (design §4 step 4 [r3.2]).
    pub(super) fn final_shutdown(&self) -> bool {
        *self.final_shutdown.borrow()
    }

    /// Subscribes Host's early-stop task to the force signal, which a force
    /// stop raises at acceptance and the latch in phase one (design §6.8
    /// [r4.3, r5.1]). Daemon main calls it once, from within its runtime,
    /// before it serves; Host's shutdown retires the task when force never
    /// came.
    pub fn watch_force(&self) {
        self.adapter.watch_force(self.signal.force.subscribe());
    }

    /// When final shutdown's dispatcher join (pipeline step 3) gives up, so
    /// Host reconciliation and the finalization reserve keep their time
    /// (design §6.8): `deadline − FINALIZE_RESERVE`. Force-path reads stop
    /// at `deadline − 8 s` (§6.7), so a dispatcher still running here is
    /// stalled outside Store.
    pub fn dispatchers_by(deadline: tokio::time::Instant) -> tokio::time::Instant {
        Self::aborted_by(deadline)
            .checked_sub(ABORTED_JOIN)
            .unwrap_or(deadline)
    }

    /// When final shutdown stops waiting for the dispatchers it aborted at
    /// [`Engine::dispatchers_by`]: `deadline − FINALIZE_RESERVE`, where Host
    /// reconciliation begins. A dispatcher still unjoined then keeps its
    /// session: [`Engine::shutdown`] settles none of its turns.
    pub fn aborted_by(deadline: tokio::time::Instant) -> tokio::time::Instant {
        deadline.checked_sub(FINALIZE_RESERVE).unwrap_or(deadline)
    }

    /// Marks `session`'s dispatcher running until the returned guard drops.
    pub(super) fn dispatching(&self, session: &SessionId) -> Dispatching<'_> {
        lock(&self.dispatching).insert(session.clone());
        Dispatching {
            engine: self,
            session: session.clone(),
        }
    }

    /// Pipeline step 5 for one forced turn (design §6.8): its terminal
    /// with Route's close evidence and reconciliation's, bounded by
    /// `min(FINALIZE_WRITE, remaining)`. Whether it committed. An affected
    /// turn, one of whose own writes was uncertain, takes the same evidence
    /// through the failure-resolution batch instead (design §7.4), which
    /// counts in `batches`, not as an uncommitted terminal.
    async fn finalize_forced(
        &self,
        turn: super::ForcedTurn,
        report: &via_adapters::FakeShutdown,
        deadline: Deadline,
        batches: &mut FailureBatches,
    ) -> bool {
        // Test builds: the evidence is in, the terminal not yet committed.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.shutdown.before_forced_terminal").await;
        let by = deadline
            .instant()
            .min(tokio::time::Instant::now() + FINALIZE_WRITE);
        if batch::affected(&turn.record) {
            let raw_incomplete = turn.raw_owed;
            // After the first failure `cancel.settled` is not written: no I/O.
            let (started, record, terminal) = self.forced_terminal(turn, report).await;
            let affected = AffectedTurn {
                started,
                record,
                terminal,
                raw_incomplete,
            };
            self.resolve_affected(affected, deadline, batches).await;
            return true;
        }
        let commit = async {
            let (started, record, terminal) = self.forced_terminal(turn, report).await;
            // C1 §3.14: close only once every other turn of the session
            // has a durable disposition. Design §3.2: once
            // `failure_pending` is observed no new close-bearing commit
            // starts; Store refuses `session.closed` in the same
            // transaction while any other turn is queued or running, which
            // covers a turn an uncertain receipt committed unregistered.
            let admission = self.admission.lock().await;
            let close =
                !self.store_failed() && !self.unresolved.others(&started.session, started.turn);
            self.finish(&started, record, terminal, close, Some(&admission))
                .await
        };
        // Per pipeline, not per turn [r5.10]: each commit takes at most
        // `FINALIZE_WRITE`, and one cut at the deadline is uncommitted.
        matches!(tokio::time::timeout_at(by, commit).await, Ok(Ok(())))
    }

    /// A forced turn's terminal from Route's close evidence and
    /// reconciliation's (C1 §7.6 force row), after committing its
    /// `cancel.settled`.
    async fn forced_terminal(
        &self,
        turn: super::ForcedTurn,
        report: &via_adapters::FakeShutdown,
    ) -> (super::Started, TurnRecord, Terminal) {
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
        if turn.cause == Some(StopCause::IdleDeadline) {
            // Design §2: force took over an idle stop.
            terminal.fail(
                FailureClass::DeadlineIdle,
                "no progress within the idle deadline",
            );
            terminal.stop_reason = "deadline";
        }
        if record.first_failure.is_some() {
            // C1 §8.2: the durable stream already lost an event; a
            // cancellation must not present it as a complete record.
            terminal.fail(FailureClass::Store, "a turn event could not be recorded");
        }
        (turn.started, record, terminal)
    }

    /// The accepted `daemon/stop` mode, if any; daemon main reads it on notice.
    pub fn stop_mode(&self) -> Option<StopMode> {
        *lock(&self.signal.stop)
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

    /// Final shutdown's pipeline steps 4 to 6 (design §6.8 [r3.3, r4.2]),
    /// after daemon main joined the dispatchers and collected their
    /// handoffs: Host reconciliation over the collected turns closes live
    /// controls, reconciles every anchor and joins Host's tasks by
    /// `deadline − FINALIZE_RESERVE`; then exactly those forced turns commit
    /// their terminal with that evidence, each bounded by `FINALIZE_WRITE`;
    /// then the closure pass; and every receipted turn is checked for a
    /// durable terminal. Every step shares the caller's absolute deadline.
    pub async fn shutdown(&self, deadline: Deadline) -> EngineShutdown {
        // Unprovable group absence must not consume the finalization reserve.
        let host_by = deadline
            .instant()
            .checked_sub(FINALIZE_RESERVE)
            .unwrap_or_else(tokio::time::Instant::now);
        // Test builds: final shutdown enters Host reconciliation (step 4).
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.shutdown.reconcile_entry").await;
        // Host keeps evidence only for receipted, unresolved turns (at most
        // the unresolved cap), which include every force-stopped turn.
        let turns = self.unresolved.turns();
        let report = self.adapter.shutdown(Deadline::at(host_by), &turns).await;
        // A dispatcher that has not joined still owns its session: none of
        // its turns is settled here, and they stay unresolved for restart
        // recovery (design §6.8 step 3).
        let unjoined = lock(&self.dispatching).clone();
        let forced = std::mem::take(&mut *lock(&self.forced));
        let mut uncommitted_turns = 0;
        let mut batches = FailureBatches::default();
        for turn in forced {
            if unjoined.contains(&turn.started.session) {
                continue;
            }
            if !self
                .finalize_forced(turn, &report, deadline, &mut batches)
                .await
            {
                uncommitted_turns += 1;
            }
        }
        // Design §7.4: running turns whose terminal failed, after the forced
        // ones and Host reconciliation.
        for turn in self.take_affected() {
            if unjoined.contains(&turn.started.session) {
                continue;
            }
            self.resolve_affected(turn, deadline, &mut batches).await;
        }
        let unclosed_sessions =
            tokio::time::timeout_at(deadline.instant(), self.close_forced_sessions(&unjoined))
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
            unjoined_dispatchers: unjoined.len(),
            failure_batches: batches,
        }
    }

    /// Final shutdown's force closure pass, after the dispatchers joined: under
    /// `admission`, each session with dispatch state at force acceptance that
    /// is still open in Store is closed with `session.closed`
    /// (`daemon_stop_force`) once every turn has a durable disposition.
    /// Returns how many could not be closed. Skipped after a Store failure,
    /// whose exit is already incomplete, and each session it leaves open
    /// counts as unclosed; a session already closed in-path is
    /// read as closed and never closed twice. A session in `unjoined`, whose
    /// dispatcher still owns it, is not closed.
    async fn close_forced_sessions(&self, unjoined: &HashSet<SessionId>) -> usize {
        let sessions = lock(&self.force_sessions).clone().unwrap_or_default();
        let total = sessions.len();
        let mut unclosed = 0;
        for (index, session) in sessions.into_iter().enumerate() {
            if unjoined.contains(&session) {
                unclosed += 1;
                continue;
            }
            let admission = self.admission.lock().await;
            if self.store_failed() {
                // The pass cannot run: this and every later session stay
                // open (T3-S5 round 1, decision 4).
                return unclosed + (total - index);
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
        // A failed head read writes nothing; Store's read reply already
        // reported corruption (design §7.1, T3-S5 round 2, decision 11).
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
                let outcome = WriteOutcome::of(&error);
                if outcome.head_unknown() {
                    guard.lost();
                }
                self.store_failure(
                    FailureSite::SessionClosed,
                    outcome,
                    FailureScope::Session(session),
                )
                .finish_held(admission);
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

/// A running dispatcher's claim on its session (design §6.8 step 3),
/// released when the dispatcher's future ends or is dropped, as an abort
/// does once the task is next polled.
pub(super) struct Dispatching<'a> {
    engine: &'a Engine,
    session: SessionId,
}

impl Drop for Dispatching<'_> {
    fn drop(&mut self) {
        lock(&self.engine.dispatching).remove(&self.session);
    }
}
