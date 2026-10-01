//! C1 §7.5 crash recovery (runtime-contracts §7): before admission, Host
//! reconciles every committed anchor, then each turn a previous daemon
//! submitted but never ended becomes `unknown` with Host's verified or
//! uncertain cleanup. Nothing is resent: the prompt may have reached the
//! vendor, and process exit proves neither non-submission nor inaction.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use via_adapters::{AnchorRecovery, Recovery, SessionCx, observation_channel};
use via_store::{
    ANCHOR_PAGE_LIMIT, AnchorOwner, CancelCause, StoreError, TerminalRecord, UnfinishedTurn,
};

use std::sync::atomic::Ordering;

use super::drive::{Cancelled, Commit, queued_cancellation};
use super::journal::Head;
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use super::queue::{CLOSE_ALLOWANCE, CONNECTION_SLOTS, Owner};
use super::resolve::{self, CORRUPT_ROW, Queueing};
use super::stop::stop_outcome;
use super::terminal::terminal_envelope;
use super::{Accepted, Engine, Terminal, TurnRecord, failure, journal};
use crate::api::{Cancel, Effective, Event, EventBody, FailureClass, Timestamps, Usage, rfc3339};
use crate::{ApiError, Cleanup, Deadline, SessionId, TurnNumber};

/// Event page size, Store's bound.
const PAGE: u32 = 1000;
/// Queued turns read per page by the restart handoff (Store's page bound).
const HANDOFF_PAGE: u32 = 256;

/// What the restart handoff did with the queued turns it found.
#[derive(Debug, Default)]
pub struct Handoff {
    /// Enqueued with their session's dispatcher.
    pub enqueued: usize,
    /// Committed `queued → cancelled` behind an `unknown` predecessor, or
    /// for a durably `closing` session.
    pub cancelled: usize,
    /// Durably `closing` sessions the restart closed (design §4).
    pub closed: usize,
    /// Failed `failed(store)` without agent I/O because a frozen value in
    /// the queued row is unparseable (design §7.3, O1.D8).
    pub failed: usize,
}
/// Startup budget for Host's anchor reconciliation (its native stop is 3 s).
const HOST_RECOVERY: Duration = Duration::from_secs(5);

impl Engine {
    /// Reconciles committed anchors through Host, then resolves every durable
    /// nonterminal submitted turn as `unknown` and returns how many; the
    /// daemon admits requests only after this succeeds. A queued turn without
    /// submission intent stays queued.
    pub async fn recover(&self) -> Result<usize, String> {
        self.recover_logged(|_, _| {}).await
    }

    /// [`Self::recover`], calling `on_turn` for each turn it resolves as
    /// `unknown` before resolving it: the daemon logs one warning per turn
    /// with its session and turn (Task 4 design §7.6).
    pub async fn recover_logged(
        &self,
        mut on_turn: impl FnMut(&SessionId, TurnNumber) + Send,
    ) -> Result<usize, String> {
        // Design §6.5: every referenced blob is checked, then unreferenced
        // ones (a lost discard, a crash before adoption) are unlinked.
        let store_error = |error| format!("store_error: {error}");
        self.store.verify_blobs().await.map_err(store_error)?;
        self.store.sweep_blobs().await.map_err(store_error)?;
        let deadline = Deadline::at(tokio::time::Instant::now() + HOST_RECOVERY);
        let reconciled = self.reconcile(deadline).await?;
        let mut recovered = 0;
        let mut asked = std::collections::HashSet::new();
        loop {
            let turns = self
                .store
                .unfinished_turns()
                .await
                .map_err(|error| format!("store_error: {error}"))?;
            if turns.is_empty() {
                return Ok(recovered);
            }
            // Each resolution commits or fails recovery, so the next read
            // never returns the same turn again.
            for turn in turns {
                if asked.insert(turn.session_id.clone()) {
                    self.recover_session(&turn.session_id, &reconciled)
                        .await
                        .map_err(|error| format!("store_error: {}", error.kind))?;
                }
                on_turn(&turn.session_id, turn.turn);
                self.recover_turn(turn, &reconciled)
                    .await
                    .map_err(|error| format!("store_error: {}", error.kind))?;
                recovered += 1;
            }
        }
    }

    /// The restart handoff (design §10), after `recover` and before
    /// admission. Every durable `queued` turn is read in bounded pages, in
    /// `(session, turn)` order. Behind a durably `unknown` latest submitted
    /// predecessor whose cleanup is settled, with nothing unresolved in
    /// between, the turn is committed `queued → cancelled`
    /// (C1 §7.2, P6). Every other turn is registered like a receipt (counted
    /// queued, active and unresolved, even past the daemon-wide bound) and
    /// enqueued, with its dispatcher start requested. Nothing is resent. Any
    /// Store failure fails startup: the daemon never admits on a partial
    /// handoff.
    ///
    /// A durably `closing` session is finished before admission (design §4
    /// "Restart"): its queued turns are cancelled with cause `close`, then
    /// `Closed` commits after one bounded absence check.
    ///
    /// Design §7.3 (O1.D8): a turn the handoff would enqueue, at its
    /// session's head, whose frozen row is present but unparseable fails
    /// `failed(store)` without agent I/O through `commit_submit_failed`, and
    /// its successors are handed off as usual. That write failing fails
    /// startup (§7.2 row 13). A corrupt turn behind an unresolved
    /// predecessor, or behind an `unknown` one whose cleanup is pending, is
    /// enqueued: the dispatcher's live rule meets it at the head, so turns
    /// still dispatch in order. A turn the handoff cancels is cancelled even
    /// when Store cannot read its row, so it never counts as submitted and
    /// the `unknown` barrier (C1 P6) holds for every turn behind it, across
    /// restarts too.
    pub async fn hand_off_queued(&self) -> Result<Handoff, String> {
        let mut handoff = Handoff::default();
        let closing = self.closing_on_disk().await?;
        let mut after = None;
        loop {
            let page = self
                .store
                .queued_turns_page(after.clone(), HANDOFF_PAGE)
                .await
                .map_err(|error| format!("store_error: {error}"))?;
            let full = page.len() == HANDOFF_PAGE as usize;
            after = page.last().cloned();
            for (session, turn) in page {
                let predecessors = self
                    .store
                    .predecessors(&session, turn)
                    .await
                    .map_err(|error| format!("store_error: {error}"))?;
                // The dispatcher's rule (§2.2): only behind an `unknown`
                // predecessor with settled cleanup; pending cleanup waits.
                // Design §6.7: the terminal's facts, never its envelope.
                let unknown = predecessors
                    .last_submitted
                    .as_ref()
                    .filter(|facts| !predecessors.unresolved && facts.state == "unknown");
                let cancel = unknown.is_some_and(|facts| {
                    facts
                        .cancel
                        .as_ref()
                        .is_none_or(|cancel| cancel.cleanup != "pending")
                });
                let close = closing.contains(&session);
                let cause = close.then(|| (CancelCause::Close, rfc3339(SystemTime::now())));
                if cancel || close {
                    if self.frozen_row_corrupt(&session, turn, false).await? {
                        self.cancel_unreadable_turn(&session, turn, cause).await?;
                        handoff.cancelled += 1;
                        continue;
                    }
                } else if !predecessors.unresolved
                    && unknown.is_none()
                    && self.frozen_row_corrupt(&session, turn, true).await?
                {
                    self.fail_corrupt_turn(&session, turn).await?;
                    handoff.failed += 1;
                    continue;
                }
                let slot = self.slot_for(&session);
                self.unresolved.receipt(&session, turn);
                self.active.fetch_add(1, Ordering::AcqRel);
                self.queued.fetch_add(1, Ordering::AcqRel);
                if cancel || close {
                    if !matches!(
                        self.cancel_queued(
                            &slot,
                            &session,
                            (turn, Owner::Dispatcher),
                            false,
                            cause
                        )
                        .await,
                        Cancelled::Committed(_)
                    ) {
                        return Err(format!(
                            "store_error: queued turn {session}/{} could not be cancelled",
                            turn.get()
                        ));
                    }
                    handoff.cancelled += 1;
                } else {
                    if slot.enqueue(turn) {
                        self.request_start(session.clone());
                    }
                    handoff.enqueued += 1;
                }
            }
            if !full {
                break;
            }
        }
        let bound = tokio::time::Instant::now() + CLOSE_ALLOWANCE;
        for session in closing {
            self.finish_restart_close(&session, bound).await?;
            handoff.closed += 1;
        }
        Ok(handoff)
    }

    /// Whether `turn`'s queued row holds a frozen value that is present but
    /// unparseable (design §7.3): Store cannot read the row, or, for a turn
    /// the handoff would enqueue (`parse`), Core cannot read its `effective`.
    /// Any other read failure fails startup.
    async fn frozen_row_corrupt(
        &self,
        session: &SessionId,
        turn: TurnNumber,
        parse: bool,
    ) -> Result<bool, String> {
        match self.store.queued_turn(session, turn).await {
            Ok(Some(queued)) => {
                Ok(parse && serde_json::from_value::<Effective>(queued.effective).is_err())
            }
            // Store's own parse of the row's values failed.
            Err(StoreError::CorruptEvidence) => Ok(true),
            Ok(None) => Err(format!(
                "store_error: queued turn {session}/{} is gone",
                turn.get()
            )),
            Err(error) => Err(format!("store_error: {error}")),
        }
    }

    /// Cancels a queued turn the handoff cancels (C1 P6, or its session's
    /// close) although Store cannot read its row: the cancellation needs only
    /// the turn's queueing, which comes from the committed history, and is
    /// otherwise the one `drive.rs`'s `queued_cancellation` builds. The turn
    /// is never submitted, so later turns still see the `unknown`
    /// predecessor. A cancellation that is not certainly durable fails
    /// startup.
    async fn cancel_unreadable_turn(
        &self,
        session: &SessionId,
        turn: TurnNumber,
        cause: Option<(CancelCause, String)>,
    ) -> Result<(), String> {
        let queueing = self
            .queueing(session, turn)
            .await
            .map_err(|_| format!("store_error: {}", ApiError::STORE.kind))?;
        let slot = self.slot_for(session);
        let (started, record, terminal, extras) =
            queued_cancellation(&slot, session, turn, queueing, cause);
        match Self::commit_turn_ended_with(
            &self.store,
            &started,
            record,
            terminal,
            false,
            extras,
            Commit::default(),
        )
        .await
        {
            Ok(durable) if durable.uncertain.is_none() => Ok(()),
            Ok(_) | Err(_) => Err(format!(
                "store_error: queued turn {session}/{} could not be cancelled",
                turn.get()
            )),
        }
    }

    /// Fails a queued turn whose frozen row is corrupt (design §7.2 row 2,
    /// §7.3): `turn.submitted` and `turn.ended` `failed(store)`, with
    /// `cancel: null`, in one `commit_submit_failed` transaction and without
    /// agent I/O. Any failure of the write fails startup (§7.2 row 13).
    /// The corrupt row is recorded first, as the live rule records it, so
    /// `store_failure` reports it once admission opens (§7.5).
    async fn fail_corrupt_turn(&self, session: &SessionId, turn: TurnNumber) -> Result<(), String> {
        let scope = FailureScope::Turn(session, turn);
        self.store_failure(FailureSite::CorruptRow, WriteOutcome::NotCommitted, scope)
            .finish()
            .await;
        // The row itself may be unreadable: its queueing comes from the
        // committed history instead.
        let queueing = self
            .queueing(session, turn)
            .await
            .map_err(|_| format!("store_error: {}", ApiError::STORE.kind))?;
        let slot = self.slot_for(session);
        resolve::commit_submit_failed(
            &self.store,
            &slot.head,
            (session, turn),
            queueing,
            CORRUPT_ROW,
        )
        .await
        .map_err(|error| format!("store_error: {error}"))
    }

    /// The committed queueing of `turn` (its `turn.queued` time and
    /// sequence), read from the session's history, for a row Store cannot
    /// read (design §7.3): shared by the restart handoff and the live
    /// dispatcher. When it cannot be read, the error is the outcome of the
    /// write that needed it: nothing was written, and a corrupt read is
    /// [`WriteOutcome::ReadCorrupt`], which Store's read reply already
    /// recorded (T3-S5 round 3, decision 13).
    pub(super) async fn queueing(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Queueing, WriteOutcome> {
        let History {
            queued_at,
            queued_seq,
            ..
        } = self.history(session, turn).await?;
        let cwd = self
            .frozen_cwd(session)
            .await
            .map_err(|error| WriteOutcome::of_read(&error))?;
        Ok(Queueing {
            queued_at,
            queued_seq,
            cwd,
        })
    }

    /// The turn's evidence folder, as its envelope names it (C1 §5).
    pub(super) fn evidence_folder(&self, session: &SessionId, turn: TurnNumber) -> String {
        self.store
            .evidence_path(&via_store::EvidenceRoot::relative(session, turn))
            .display()
            .to_string()
    }

    /// The session's frozen `cwd` (design §11.1), which a rebuilt envelope
    /// reports as the live drive would.
    async fn frozen_cwd(&self, session: &SessionId) -> Result<Option<String>, StoreError> {
        Ok(self
            .store
            .session_snapshot(session)
            .await?
            .and_then(|snapshot| snapshot.cwd))
    }

    /// Pages through every committed anchor with Host's reports for the same
    /// id range, in bounded memory. An anchor Host did not report in time
    /// stays uncertain; only a Store read or write failure is fatal
    /// (runtime-contracts §7).
    async fn reconcile(&self, deadline: Deadline) -> Result<Reconciled, String> {
        let mut reconciled = Reconciled::default();
        let mut after = None;
        loop {
            // At expiry paging stops: the unread rest of the inventory leaves
            // every recovered turn uncertain, and startup continues.
            if tokio::time::Instant::now() >= deadline.instant() {
                reconciled.incomplete = true;
                // Design §11: the unread anchors without absence proof still
                // count against the pool, as unidentified groups. One indexed
                // query saturating at the pool (never released during
                // admission, so no more are needed); its failure is a Store
                // failure and fails startup.
                let pool = u32::try_from(CONNECTION_SLOTS).unwrap_or(u32::MAX);
                self.recovered.save_cursor(after.clone());
                let unread = self
                    .store
                    .unproven_anchors_up_to(after, pool)
                    .await
                    .map_err(|error| format!("store_error: {error}"))?;
                self.recovered.hold_unidentified(&self.slots, unread);
                return Ok(reconciled);
            }
            let owners = self
                .store
                .anchor_owners_page(after.clone(), ANCHOR_PAGE_LIMIT)
                .await
                .map_err(|error| format!("store_error: {error}"))?;
            let Some(last) = owners.last() else {
                return Ok(reconciled);
            };
            let next = last.anchor_id.clone();
            let reports = match self
                .adapter
                .recover_page(after, ANCHOR_PAGE_LIMIT, deadline)
                .await
            {
                Ok(reports) => reports,
                Err(error) if error.is_store_failure() => {
                    return Err(format!("store_error: host reconciliation: {error}"));
                }
                // Unproven evidence: this page stays unreported.
                Err(_) => Vec::new(),
            };
            reconciled.add(&owners, &reports);
            self.hold_unproven(&owners, &reports);
            if owners.len() < ANCHOR_PAGE_LIMIT as usize {
                return Ok(reconciled);
            }
            after = Some(next);
            #[cfg(feature = "test-failpoints")]
            via_store::failpoint::hit_async("core.recovery.page_boundary")
                .await
                .map_err(|error| format!("store_error: {error}"))?;
        }
    }

    /// C2 §2 Recover (AD9, Sol r1 F12): asks the adapter set about
    /// `session` from its stored route identity, with Host's reconciled
    /// facts for it; nothing is submitted. A resumed driver becomes the
    /// session's lane. The session's unfinished turns still end `unknown`
    /// (C1 §7.5), with the cleanup Host's facts prove.
    async fn recover_session(
        &self,
        session: &SessionId,
        reconciled: &Reconciled,
    ) -> Result<(), ApiError> {
        let snapshot = self
            .store
            .session_snapshot(session)
            .await
            .map_err(|_| ApiError::STORE)?
            .ok_or(ApiError::STORE)?;
        let facts = reconciled.facts.get(session).map_or(&[][..], Vec::as_slice);
        let (sink, receiver) = observation_channel();
        let cx = SessionCx {
            observations: sink,
            tracker: self.tracker.clone(),
            cancel: self.cancel.child_token(),
        };
        let reference = super::lane::session_ref(&snapshot.route);
        let recovery = self.adapter.recover(&reference, facts, cx).await;
        #[cfg(test)]
        {
            let answer = match &recovery {
                Recovery::Resumed(_) => "resumed",
                Recovery::Unknown { .. } => "unknown",
                Recovery::Dead { .. } => "dead",
            };
            super::lock(&self.faults.recoveries).push((session.clone(), facts.len(), answer));
        }
        if let Recovery::Resumed(driver) = recovery {
            self.adopt_lane(session, (*driver, receiver), &reference);
        }
        Ok(())
    }

    /// Design §11: a group an earlier daemon left, whose absence recovery did
    /// not prove, holds a connection slot until a later Host absence proof
    /// drops its token. Past the pool the groups share the permits held, so
    /// no new child starts until cleanup proves room.
    fn hold_unproven(&self, owners: &[AnchorOwner], reports: &[AnchorRecovery]) {
        for owner in owners {
            let proved = reports.iter().any(|report| {
                report.anchor_id == owner.anchor_id && report.cleanup == Cleanup::Quiescent
            });
            if !proved {
                let token = self.recovered.hold(&self.slots);
                self.adapter.hold_capacity(
                    owner.anchor_id.clone(),
                    owner.session_id.clone(),
                    Box::new(token),
                );
            }
        }
    }

    /// Commits `cancel.requested`, `cancel.settled` with Host's cleanup and
    /// `turn.ended` (`unknown`) after every event the crashed daemon
    /// committed. The envelope names the turn's evidence folder, which is
    /// complete as written (Task 4 design §7.5).
    ///
    /// A durable `cancel.requested` (the crashed daemon's order, or an
    /// earlier recovery attempt's) is kept, with its `at` as
    /// `requested_at`.
    async fn recover_turn(
        &self,
        unfinished: UnfinishedTurn,
        reconciled: &Reconciled,
    ) -> Result<(), ApiError> {
        let UnfinishedTurn {
            session_id: session,
            turn,
            submitted_at,
            correlation,
        } = unfinished;
        let History {
            last_seq,
            queued_at,
            queued_seq,
            started,
            requested_at,
            settled,
        } = self
            .history(&session, turn)
            .await
            .map_err(|_| ApiError::STORE)?;
        let cwd = self
            .frozen_cwd(&session)
            .await
            .map_err(|_| ApiError::STORE)?;
        let accepted = recovered_acceptance(correlation, started);
        // Recovery runs before admission: this turn's writes are the session's only ones.
        let head = Head::new(Some(last_seq + 1));
        let mut record = TurnRecord {
            session: session.clone(),
            turn,
            head: std::sync::Arc::clone(&head),
            accepted,
            first_failure: None,
            uncertain: None,
            steps: super::progress::StepTracker::default(),
            vendor: super::lane::VendorRecord::default(),
        };
        let cancel = self
            .settle_recovered(&mut record, reconciled, requested_at, settled)
            .await?;
        let head = head
            .lock(&self.store, &session)
            .await
            .map_err(|_| ApiError::STORE)?;
        let seq = head.next();
        let ended_at = rfc3339(SystemTime::now());
        let terminal = recovered_terminal(cancel.clone());
        let event = Event {
            seq,
            session_id: &session,
            turn: Some(turn.get()),
            late: false,
            at: &ended_at,
            body: EventBody::TurnEnded {
                state: terminal.state,
                failure: terminal.failure.clone(),
                stop_reason: terminal.stop_reason,
                cancel: Some(cancel),
            },
        }
        .to_value()?;
        let timestamps = Timestamps {
            queued_at,
            submitted_at: Some(submitted_at),
            accepted_at: record.accepted.as_ref().map(|accepted| accepted.at.clone()),
            ended_at,
        };
        // The crashed daemon's clock is gone: no duration is claimed.
        let envelope = terminal_envelope(
            &session,
            turn,
            terminal,
            record.accepted,
            // A recovered turn was submitted: its folder was named then.
            (cwd, Some(self.evidence_folder(&session, turn))),
            timestamps,
            None,
            (queued_seq, seq),
            // The crashed daemon's samples are gone with it.
            Usage::UNAVAILABLE,
        );
        let envelope = serde_json::to_value(&envelope).map_err(|_| ApiError::STORE)?;
        let committed = journal::commit_terminal(
            &self.store,
            TerminalRecord {
                session_id: session,
                turn,
                envelope,
                event,
                steps: Vec::new(),
            },
            None,
        )
        .await;
        // Recovery must be certain before admission: an uncertain commit, even
        // one read back as durable, fails startup instead (runtime §7).
        if committed.is_ok_and(|durable| durable.uncertain.is_none()) {
            head.committed(1);
            Ok(())
        } else {
            head.lost();
            Err(ApiError::STORE)
        }
    }

    /// Records the recovery stop of the turn's orphaned execution (C1 §7.5):
    /// `forced` only when Host's stop found its vendor live; cleanup
    /// `quiescent` only when Host proved every owned group absent, or when no
    /// anchor intent committed, so no process could exist. A durable
    /// `cancel.requested` (`requested`, its `at`) is the turn's one request
    /// (design §9); otherwise recovery commits it. A durable `cancel.settled`
    /// (`settled`, from a live settlement or an earlier recovery whose
    /// terminal did not commit) is likewise the turn's one settlement: the
    /// terminal cites it and nothing new is committed.
    async fn settle_recovered(
        &self,
        record: &mut TurnRecord,
        reconciled: &Reconciled,
        requested: Option<String>,
        settled: Option<DurableSettlement>,
    ) -> Result<Cancel, ApiError> {
        if let Some(settled) = settled {
            if record.first_failure.is_some() {
                return Err(ApiError::STORE);
            }
            // A settlement is only ever committed after its request.
            let requested_at = requested.ok_or(ApiError::STORE)?;
            return Ok(Cancel {
                outcome: settled.outcome,
                cleanup: settled.cleanup,
                requested_at,
                settled_at: settled.at,
            });
        }
        let (quiescent, forced) = reconciled.cleanup(&record.session, record.turn);
        let (outcome, cleanup) = stop_outcome(quiescent, forced, false);
        let requested_at = if let Some(at) = requested {
            at
        } else {
            let at = rfc3339(SystemTime::now());
            self.commit_event(record, EventBody::CancelRequested {})
                .await;
            at
        };
        let cancel = self.settle(record, requested_at, outcome, cleanup).await;
        if record.first_failure.is_some() {
            // Recovery must be durable before admission; startup fails instead.
            return Err(ApiError::STORE);
        }
        Ok(cancel)
    }

    /// Reads the session's committed events in pages and keeps what the
    /// recovered envelope of `turn` cites. An unreadable or incomplete
    /// history fails with the outcome of the write that needed it, as
    /// [`Engine::queueing`] reports it.
    async fn history(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<History, WriteOutcome> {
        let mut last_seq = 0;
        let mut queued_at = None;
        let mut queued_seq = None;
        let mut started = None;
        let mut requested_at = None;
        let mut settled = None;
        loop {
            let page = self
                .store
                .events(session, last_seq + 1, PAGE)
                .await
                .map_err(|error| WriteOutcome::of_read(&error))?;
            let full = page.len() == PAGE as usize;
            for stored in page {
                last_seq = stored.seq;
                if stored.event.get("turn").and_then(Value::as_u64) != Some(u64::from(turn.get())) {
                    continue;
                }
                let at = stored.event.get("at").and_then(Value::as_str);
                match stored.event.get("type").and_then(Value::as_str) {
                    Some("turn.queued") => {
                        queued_at = at.map(str::to_owned);
                        queued_seq = Some(stored.seq);
                    }
                    Some("turn.started") => {
                        started = at.map(str::to_owned);
                    }
                    Some("cancel.requested") if requested_at.is_none() => {
                        requested_at = at.map(str::to_owned);
                    }
                    Some("cancel.settled") if settled.is_none() => {
                        settled = Some(
                            DurableSettlement::read(&stored.event)
                                .map_err(|_| WriteOutcome::NotCommitted)?,
                        );
                    }
                    _ => {}
                }
            }
            if !full {
                break;
            }
        }
        Ok(History {
            last_seq,
            queued_at: queued_at.ok_or(WriteOutcome::NotCommitted)?,
            queued_seq: queued_seq.ok_or(WriteOutcome::NotCommitted)?,
            started,
            requested_at,
            settled,
        })
    }
}

/// A recovered turn's terminal: `unknown` with Core's restart class and the
/// recovery settlement.
fn recovered_terminal(cancel: Cancel) -> Terminal {
    Terminal {
        state: "unknown",
        // C1 §8.2: Core's restart class; the state stays `unknown` (§7.5).
        failure: Some(failure(
            FailureClass::DaemonRestart,
            "the daemon restarted before the turn ended".to_owned(),
            None,
        )),
        // C1 §7.6: an unconfirmed outcome, like transport loss, is an error.
        stop_reason: "error",
        vendor_stop_reason: None,
        final_text: Some(String::new()),
        final_text_file: None,
        exit: None,
        warnings: Vec::new(),
        cancel: Some(cancel),
    }
}

/// Acceptance is reported only when both its evidence and its event committed.
fn recovered_acceptance(correlation: Option<String>, started: Option<String>) -> Option<Accepted> {
    match (correlation, started) {
        (Some(correlation), Some(at)) => Some(Accepted {
            at,
            vendor_turn_id: (!correlation.starts_with(super::drive::TOKEN_CORRELATION))
                .then_some(correlation),
        }),
        _ => None,
    }
}

/// What a recovered turn's envelope cites from the committed history.
struct History {
    /// The session's last committed sequence.
    last_seq: u64,
    queued_at: String,
    /// Sequence of the turn's `turn.queued`: the envelope's `first_seq`.
    queued_seq: u64,
    /// `turn.started` time, when acceptance's event committed.
    started: Option<String>,
    /// `at` of the turn's first durable `cancel.requested`.
    requested_at: Option<String>,
    /// The turn's first durable `cancel.settled`.
    settled: Option<DurableSettlement>,
}

/// A durable `cancel.settled`: what the recovered terminal's `cancel` cites.
#[derive(Debug, PartialEq, Eq)]
struct DurableSettlement {
    at: String,
    outcome: &'static str,
    cleanup: &'static str,
}

impl DurableSettlement {
    /// Reads a committed `cancel.settled`; a value outside C1 §3.5's sets
    /// is corrupt evidence.
    fn read(event: &Value) -> Result<Self, ApiError> {
        let field = |name: &str| event.get(name).and_then(Value::as_str);
        let outcome = ["requested", "acknowledged", "forced", "unknown"]
            .into_iter()
            .find(|known| field("outcome") == Some(*known));
        let cleanup = ["quiescent", "uncertain", "pending"]
            .into_iter()
            .find(|known| field("cleanup") == Some(*known));
        match (field("at"), outcome, cleanup) {
            (Some(at), Some(outcome), Some(cleanup)) => Ok(Self {
                at: at.to_owned(),
                outcome,
                cleanup,
            }),
            _ => Err(ApiError::STORE),
        }
    }
}

/// Host's cleanup evidence for running turns, checked against every
/// committed anchor; anchors of already-ended turns are only counted.
#[derive(Default)]
struct Reconciled {
    /// `(quiescent, forced)` per running owning turn of a committed anchor.
    turns: HashMap<(SessionId, TurnNumber), (bool, bool)>,
    /// Host's reports for the anchors of each session's running turns: the
    /// facts its adapter recovery is given (C2 §2 Recover).
    facts: HashMap<SessionId, Vec<AnchorRecovery>>,
    /// Committed anchors Host returned no report for; each stays uncertain.
    missing: usize,
    /// The deadline stopped paging before the whole inventory was read.
    incomplete: bool,
}

impl Reconciled {
    /// Folds one inventory page and Host's reports for the same id range.
    fn add(&mut self, owners: &[AnchorOwner], reports: &[AnchorRecovery]) {
        for owner in owners {
            let report = reports.iter().find(|report| {
                report.anchor_id == owner.anchor_id
                    && report.session_id == owner.session_id
                    && report.turn == owner.turn
            });
            if report.is_none() {
                self.missing += 1;
            }
            if !owner.turn_running {
                continue;
            }
            let entry = self
                .turns
                .entry((owner.session_id.clone(), owner.turn))
                .or_insert((true, false));
            if let Some(report) = report {
                entry.0 &= report.cleanup == Cleanup::Quiescent;
                entry.1 |= report.forced;
                self.facts
                    .entry(owner.session_id.clone())
                    .or_default()
                    .push(AnchorRecovery {
                        session_id: report.session_id.clone(),
                        anchor_id: report.anchor_id.clone(),
                        generation: report.generation.clone(),
                        turn: report.turn,
                        cleanup: report.cleanup,
                        forced: report.forced,
                    });
            } else {
                // An unreported anchor is never proved absent.
                entry.0 = false;
            }
        }
    }

    /// `(quiescent, forced)` for a turn. With a complete inventory and no
    /// committed anchor intent no process could exist, so nothing needs
    /// cleaning; an incomplete inventory proves nothing for any turn.
    fn cleanup(&self, session: &SessionId, turn: TurnNumber) -> (bool, bool) {
        let (quiescent, forced) = self
            .turns
            .get(&(session.clone(), turn))
            .copied()
            .unwrap_or((true, false));
        (quiescent && !self.incomplete, forced)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AnchorOwner, AnchorRecovery, Cleanup, DurableSettlement, Reconciled, SessionId, TurnNumber,
    };
    use via_store::AnchorPhase;

    fn owner(anchor_id: &str, session: &SessionId, turn_running: bool) -> AnchorOwner {
        at_phase(
            anchor_id,
            session,
            turn_running,
            Some(AnchorPhase::ArmIntent),
        )
    }

    fn at_phase(
        anchor_id: &str,
        session: &SessionId,
        turn_running: bool,
        phase: Option<AnchorPhase>,
    ) -> AnchorOwner {
        AnchorOwner {
            anchor_id: anchor_id.to_owned(),
            session_id: session.clone(),
            turn: TurnNumber::try_from(1).expect("turn"),
            turn_running,
            phase,
        }
    }

    fn report(anchor_id: &str, session: &SessionId, cleanup: Cleanup) -> AnchorRecovery {
        AnchorRecovery {
            session_id: session.clone(),
            anchor_id: anchor_id.to_owned(),
            generation: "g".to_owned(),
            turn: TurnNumber::try_from(1).expect("turn"),
            cleanup,
            forced: false,
        }
    }

    #[test]
    fn an_anchor_without_a_report_is_never_quiescent_and_is_counted_missing() {
        let session = SessionId::try_from("s_000000000000").expect("session");
        let turn = TurnNumber::try_from(1).expect("turn");
        let mut reconciled = Reconciled::default();
        reconciled.add(
            &[owner("a1", &session, true), owner("a2", &session, true)],
            &[report("a1", &session, Cleanup::Quiescent)],
        );
        assert_eq!(reconciled.missing, 1);
        assert_eq!(reconciled.cleanup(&session, turn), (false, false));
    }

    #[test]
    fn a_page_without_reports_after_the_deadline_stays_uncertain() {
        let session = SessionId::try_from("s_000000000000").expect("session");
        let turn = TurnNumber::try_from(1).expect("turn");
        let mut reconciled = Reconciled::default();
        reconciled.add(
            &[owner("a1", &session, true)],
            &[report("a1", &session, Cleanup::Quiescent)],
        );
        reconciled.add(&[owner("a2", &session, true)], &[]);
        assert_eq!(reconciled.missing, 1);
        assert_eq!(reconciled.cleanup(&session, turn), (false, false));
    }

    #[test]
    fn an_incomplete_inventory_leaves_every_turn_uncertain() {
        let session = SessionId::try_from("s_000000000000").expect("session");
        let unseen = SessionId::try_from("s_000000000001").expect("session");
        let turn = TurnNumber::try_from(1).expect("turn");
        let mut reconciled = Reconciled::default();
        reconciled.add(
            &[owner("a1", &session, true)],
            &[report("a1", &session, Cleanup::Quiescent)],
        );
        reconciled.incomplete = true;
        assert_eq!(reconciled.cleanup(&session, turn), (false, false));
        assert_eq!(reconciled.cleanup(&unseen, turn), (false, false));
    }

    #[test]
    fn verified_uncertainty_stays_uncertain_and_ended_turns_are_not_retained() {
        let session = SessionId::try_from("s_000000000000").expect("session");
        let other = SessionId::try_from("s_000000000001").expect("session");
        let ended = SessionId::try_from("s_000000000002").expect("session");
        let turn = TurnNumber::try_from(1).expect("turn");
        let mut reconciled = Reconciled::default();
        reconciled.add(
            &[
                owner("a1", &session, true),
                owner("b1", &other, true),
                owner("c1", &ended, false),
            ],
            &[
                report("a1", &session, Cleanup::Uncertain),
                report("b1", &other, Cleanup::Quiescent),
                report("c1", &ended, Cleanup::Quiescent),
            ],
        );
        assert_eq!(reconciled.missing, 0);
        assert_eq!(reconciled.cleanup(&session, turn), (false, false));
        assert_eq!(reconciled.cleanup(&other, turn), (true, false));
        assert_eq!(reconciled.turns.len(), 2);
    }

    /// A durable `cancel.settled` is read back as committed; a value outside
    /// C1 §3.5's sets, or a missing `at`, is corrupt evidence.
    #[test]
    fn a_durable_settlement_reads_only_c1_values() {
        let event = |outcome: &str, cleanup: &str| serde_json::json!({"type":"cancel.settled","at":"t","outcome":outcome,"cleanup":cleanup});
        assert_eq!(
            DurableSettlement::read(&event("acknowledged", "pending")).ok(),
            Some(DurableSettlement {
                at: "t".to_owned(),
                outcome: "acknowledged",
                cleanup: "pending",
            })
        );
        assert!(DurableSettlement::read(&event("stopped", "quiescent")).is_err());
        assert!(DurableSettlement::read(&event("forced", "gone")).is_err());
        assert!(
            DurableSettlement::read(&serde_json::json!({"outcome":"forced","cleanup":"quiescent"}))
                .is_err()
        );
    }
}
