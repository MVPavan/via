//! C1 §7.5 crash recovery (runtime-contracts §7): before admission, Host
//! reconciles every committed anchor, then each turn a previous daemon
//! submitted but never ended becomes `unknown` with Host's verified or
//! uncertain cleanup. Nothing is resent: the prompt may have reached the
//! vendor, and process exit proves neither non-submission nor inaction.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use via_adapters::FakeRecovery;
use via_store::{
    ANCHOR_PAGE_LIMIT, AnchorOwner, AnchorPhase, CancelCause, StoreError, SubmitFailedRecord,
    TerminalRecord, UnfinishedTurn,
};

use std::sync::atomic::Ordering;

use super::drive::Cancelled;
use super::journal::Head;
use super::queue::{CLOSE_ALLOWANCE, CONNECTION_SLOTS};
use super::stop::stop_outcome;
use super::terminal::terminal_envelope;
use super::{Accepted, Engine, Terminal, TurnRecord, failure, journal};
use crate::api::{
    Cancel, Effective, Event, EventBody, FailureClass, RawSpan, Timestamps, Warning, rfc3339,
};
use crate::{ApiError, Cleanup, ConnectionId, Deadline, RawRef, SessionId, TurnNumber};

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
/// The failure message of a turn whose frozen row cannot be parsed.
const CORRUPT_ROW: &str = "a frozen value of the queued turn could not be read";

impl Engine {
    /// Reconciles committed anchors through Host, then resolves every durable
    /// nonterminal submitted turn as `unknown` and returns how many; the
    /// daemon admits requests only after this succeeds. A queued turn without
    /// submission intent stays queued.
    pub async fn recover(&self) -> Result<usize, String> {
        let deadline = Deadline::at(tokio::time::Instant::now() + HOST_RECOVERY);
        let reconciled = self.reconcile(deadline).await?;
        let mut recovered = 0;
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
    /// Design §7.3 (O1.D8): a turn at its session's head whose frozen row is
    /// present but unparseable fails `failed(store)` without agent I/O
    /// through `commit_submit_failed`, and its successors are handed off as
    /// usual. That write failing fails startup (§7.2 row 13). A corrupt turn
    /// behind an unresolved predecessor is enqueued: the dispatcher's live
    /// rule meets it at the head, so turns still dispatch in order.
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
                let cancel = !predecessors.unresolved
                    && predecessors.last_submitted.is_some_and(|envelope| {
                        envelope["state"] == "unknown" && envelope["cancel"]["cleanup"] != "pending"
                    });
                let close = closing.contains(&session);
                if !predecessors.unresolved
                    && self
                        .frozen_row_corrupt(&session, turn, !(cancel || close))
                        .await?
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
                    let cause = close.then(|| (CancelCause::Close, rfc3339(SystemTime::now())));
                    if !matches!(
                        self.cancel_queued(&slot, &session, turn, false, cause)
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

    /// Fails a queued turn whose frozen row is corrupt (design §7.2 row 2,
    /// §7.3): `turn.submitted` and `turn.ended` `failed(store)`, with
    /// `cancel: null`, in one `commit_submit_failed` transaction and without
    /// agent I/O. Any failure of the write fails startup (§7.2 row 13).
    async fn fail_corrupt_turn(&self, session: &SessionId, turn: TurnNumber) -> Result<(), String> {
        let store_error = |error: ApiError| format!("store_error: {}", error.kind);
        // The row itself may be unreadable: its queueing comes from the
        // committed history instead.
        let History {
            queued_at,
            queued_seq,
            ..
        } = self.history(session, turn).await.map_err(store_error)?;
        let slot = self.slot_for(session);
        let head = slot
            .head
            .lock(&self.store, session)
            .await
            .map_err(|error| format!("store_error: {error}"))?;
        let submitted_seq = head.next();
        let ended_seq = submitted_seq
            .checked_add(1)
            .ok_or("store_error: the session's sequence is exhausted")?;
        let at = rfc3339(SystemTime::now());
        let terminal = Terminal {
            state: "failed",
            failure: Some(failure(FailureClass::Store, CORRUPT_ROW.to_owned(), None)),
            stop_reason: "error",
            vendor_stop_reason: None,
            final_text: String::new(),
            exit: None,
            raw_ref: None,
            raw_incomplete: false,
            warnings: Vec::new(),
            cancel: None,
        };
        let submitted = Event {
            seq: submitted_seq,
            session_id: session,
            turn: Some(turn.get()),
            late: false,
            at: &at,
            raw_ref: None,
            body: EventBody::TurnSubmitted { attempt: 1 },
        }
        .to_value()
        .map_err(store_error)?;
        let ended = Event {
            seq: ended_seq,
            session_id: session,
            turn: Some(turn.get()),
            late: false,
            at: &at,
            raw_ref: None,
            body: EventBody::TurnEnded {
                state: terminal.state,
                failure: terminal.failure.clone(),
                stop_reason: terminal.stop_reason,
                cancel: None,
            },
        }
        .to_value()
        .map_err(store_error)?;
        let timestamps = Timestamps {
            queued_at,
            submitted_at: Some(at.clone()),
            accepted_at: None,
            ended_at: at.clone(),
        };
        // No vendor I/O happened, so no duration is claimed.
        let envelope = terminal_envelope(
            session,
            turn,
            terminal,
            None,
            Vec::new(),
            timestamps,
            None,
            (queued_seq, ended_seq),
        );
        let envelope = serde_json::to_value(&envelope).map_err(|_| store_error(ApiError::STORE))?;
        let committed = self
            .store
            .commit_submit_failed(SubmitFailedRecord {
                session_id: session.clone(),
                turn,
                submitted,
                ended,
                envelope,
            })
            .await;
        match committed {
            Ok(()) => {
                head.committed(2);
                Ok(())
            }
            Err(error) => {
                head.lost();
                Err(format!("store_error: {error}"))
            }
        }
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

    /// Design §11: a group an earlier daemon left, whose absence recovery did
    /// not prove, holds a connection slot until a later Host absence proof
    /// drops its token. Past the pool the groups share the permits held, so
    /// no new child starts until cleanup proves room.
    fn hold_unproven(&self, owners: &[AnchorOwner], reports: &[FakeRecovery]) {
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
    /// committed, citing the raw spans those events reference.
    ///
    /// Design §9: a turn with a committed anchor at `arm_intent` had a
    /// connection the crashed daemon never sealed, so `raw_log.incomplete`
    /// commits first and the envelope carries `raw_log_incomplete`. A
    /// durable `cancel.requested` (the crashed daemon's order, or an
    /// earlier recovery attempt's) is kept, with its `at` as
    /// `requested_at`; so is an earlier attempt's `raw_log.incomplete`.
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
            spans,
            requested_at,
            settled,
            raw_logged,
        } = self.history(&session, turn).await?;
        let accepted = recovered_acceptance(correlation, started);
        // Recovery runs before admission: this turn's writes are the session's only ones.
        let head = Head::new(Some(last_seq + 1));
        let mut record = TurnRecord {
            session: session.clone(),
            turn,
            head: std::sync::Arc::clone(&head),
            accepted,
            spans,
            first_failure: None,
            uncertain: None,
        };
        let raw_incomplete = self
            .record_raw_incomplete(&mut record, reconciled, raw_logged)
            .await?;
        let cancel = self
            .settle_recovered(&mut record, reconciled, requested_at, settled)
            .await?;
        let head = head
            .lock(&self.store, &session)
            .await
            .map_err(|_| ApiError::STORE)?;
        let seq = head.next();
        let ended_at = rfc3339(SystemTime::now());
        let terminal = recovered_terminal(cancel.clone(), raw_incomplete);
        let event = Event {
            seq,
            session_id: &session,
            turn: Some(turn.get()),
            late: false,
            at: &ended_at,
            raw_ref: None,
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
            record.spans,
            timestamps,
            None,
            (queued_seq, seq),
        );
        let envelope = serde_json::to_value(&envelope).map_err(|_| ApiError::STORE)?;
        let committed = journal::commit_terminal(
            &self.store,
            TerminalRecord {
                session_id: session,
                turn,
                envelope,
                event,
                raw_ref: None,
            },
            None,
        )
        .await;
        // Recovery must be certain before admission: an uncertain commit, even
        // one read back as durable, fails startup instead (runtime §7).
        if committed.is_ok_and(|durable| !durable.uncertain) {
            head.committed(1);
            Ok(())
        } else {
            head.lost();
            Err(ApiError::STORE)
        }
    }

    /// Design §9: commits `raw_log.incomplete` for a turn whose raw log may be
    /// incomplete, unless an earlier recovery attempt did; returns whether it
    /// may be incomplete. A failed commit is the record's first failure,
    /// which fails startup once the settlement is written.
    async fn record_raw_incomplete(
        &self,
        record: &mut TurnRecord,
        reconciled: &Reconciled,
        raw_logged: bool,
    ) -> Result<bool, ApiError> {
        let raw_incomplete = reconciled.raw_incomplete(&record.session, record.turn);
        if raw_incomplete && !raw_logged {
            let connection_id =
                recovered_connection(&record.session, record.turn).map_err(|_| ApiError::STORE)?;
            self.commit_event(record, EventBody::RawLogIncomplete { connection_id }, None)
                .await;
        }
        Ok(raw_incomplete)
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
        let (outcome, cleanup) = stop_outcome(quiescent, forced);
        let requested_at = if let Some(at) = requested {
            at
        } else {
            let at = rfc3339(SystemTime::now());
            self.commit_event(record, EventBody::CancelRequested {}, None)
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
    /// recovered envelope of `turn` cites.
    async fn history(&self, session: &SessionId, turn: TurnNumber) -> Result<History, ApiError> {
        let mut last_seq = 0;
        let mut queued_at = None;
        let mut queued_seq = None;
        let mut started = None;
        let mut spans = Vec::new();
        let mut requested_at = None;
        let mut settled = None;
        let mut raw_logged = false;
        loop {
            let page = self
                .store
                .events(session, last_seq + 1, PAGE)
                .await
                .map_err(|_| ApiError::STORE)?;
            let full = page.len() == PAGE as usize;
            for stored in page {
                last_seq = stored.seq;
                if stored.event.get("turn").and_then(Value::as_u64) != Some(u64::from(turn.get())) {
                    continue;
                }
                if let Some(reference) = &stored.raw_ref {
                    RawSpan::include(&mut spans, reference);
                }
                let at = stored.event.get("at").and_then(Value::as_str);
                match stored.event.get("type").and_then(Value::as_str) {
                    Some("turn.queued") => {
                        queued_at = at.map(str::to_owned);
                        queued_seq = Some(stored.seq);
                    }
                    Some("turn.started") => {
                        started = at.map(str::to_owned).zip(stored.raw_ref.clone());
                    }
                    Some("cancel.requested") if requested_at.is_none() => {
                        requested_at = at.map(str::to_owned);
                    }
                    Some("cancel.settled") if settled.is_none() => {
                        settled = Some(DurableSettlement::read(&stored.event)?);
                    }
                    Some("raw_log.incomplete") => raw_logged = true,
                    _ => {}
                }
            }
            if !full {
                break;
            }
        }
        Ok(History {
            last_seq,
            queued_at: queued_at.ok_or(ApiError::STORE)?,
            queued_seq: queued_seq.ok_or(ApiError::STORE)?,
            started,
            spans,
            requested_at,
            settled,
            raw_logged,
        })
    }
}

/// The turn's connection, named as `drive.rs`'s `connection_id` names it
/// when it launches the turn: one private connection per turn, and turn 1
/// keeps the session's own name. The copy exists because that helper is
/// private to `drive.rs` (S4 owns only this file); `s1_f09_` checks the two
/// agree.
fn recovered_connection(
    session: &SessionId,
    turn: TurnNumber,
) -> Result<ConnectionId, <ConnectionId as TryFrom<&str>>::Error> {
    let suffix = session.as_str().trim_start_matches("s_");
    let connection = match turn.get() {
        1 => format!("c_{suffix}"),
        n => format!("c_{suffix}t{n}"),
    };
    ConnectionId::try_from(connection.as_str())
}

/// A recovered turn's terminal: `unknown` with Core's restart class, the
/// recovery settlement, and the `raw_log_incomplete` warning when its raw
/// log may be incomplete (design §9).
fn recovered_terminal(cancel: Cancel, raw_incomplete: bool) -> Terminal {
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
        final_text: String::new(),
        exit: None,
        raw_ref: None,
        raw_incomplete,
        warnings: if raw_incomplete {
            vec![Warning::RAW_LOG_INCOMPLETE]
        } else {
            Vec::new()
        },
        cancel: Some(cancel),
    }
}

/// Acceptance is reported only when both its evidence and its event committed.
fn recovered_acceptance(
    correlation: Option<String>,
    started: Option<(String, RawRef)>,
) -> Option<Accepted> {
    match (correlation, started) {
        (Some(vendor_turn_id), Some((at, raw_ref))) => Some(Accepted {
            at,
            raw_ref,
            vendor_turn_id,
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
    /// `turn.started` time and raw span, when acceptance's event committed.
    started: Option<(String, RawRef)>,
    spans: Vec<RawSpan>,
    /// `at` of the turn's first durable `cancel.requested`.
    requested_at: Option<String>,
    /// The turn's first durable `cancel.settled`.
    settled: Option<DurableSettlement>,
    /// The turn already has a durable `raw_log.incomplete`.
    raw_logged: bool,
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
    /// Running turns with a committed anchor at `arm_intent`, or whose
    /// anchor phase is unreadable (design §9).
    armed: HashSet<(SessionId, TurnNumber)>,
    /// Committed anchors Host returned no report for; each stays uncertain.
    missing: usize,
    /// The deadline stopped paging before the whole inventory was read.
    incomplete: bool,
}

impl Reconciled {
    /// Folds one inventory page and Host's reports for the same id range.
    fn add(&mut self, owners: &[AnchorOwner], reports: &[FakeRecovery]) {
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
            // An unreadable phase may have been `arm_intent`: a raw log is
            // never claimed complete on missing evidence.
            if owner
                .phase
                .is_none_or(|phase| phase == AnchorPhase::ArmIntent)
            {
                self.armed.insert((owner.session_id.clone(), owner.turn));
            }
            let entry = self
                .turns
                .entry((owner.session_id.clone(), owner.turn))
                .or_insert((true, false));
            if let Some(report) = report {
                entry.0 &= report.cleanup == Cleanup::Quiescent;
                entry.1 |= report.forced;
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

    /// Whether the turn's raw log may be incomplete (design §9): it has a
    /// committed anchor at `arm_intent`, so the vendor may have written to a
    /// connection the crashed daemon never sealed. An incomplete inventory
    /// cannot show that an unread anchor was not armed.
    fn raw_incomplete(&self, session: &SessionId, turn: TurnNumber) -> bool {
        self.incomplete || self.armed.contains(&(session.clone(), turn))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AnchorOwner, AnchorPhase, Cleanup, DurableSettlement, FakeRecovery, Reconciled, SessionId,
        TurnNumber, recovered_connection,
    };

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

    fn report(anchor_id: &str, session: &SessionId, cleanup: Cleanup) -> FakeRecovery {
        FakeRecovery {
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

    /// Design §9: only a committed anchor at `arm_intent` (or one whose phase
    /// is unreadable) marks a recovered turn's raw log incomplete; a pre-ARM
    /// anchor, no anchor, or an armed anchor of an ended turn does not.
    #[test]
    fn raw_incompleteness_follows_the_armed_anchor_phase() {
        let session =
            |n: u8| SessionId::try_from(format!("s_00000000000{n}").as_str()).expect("session");
        let turn = TurnNumber::try_from(1).expect("turn");
        let mut reconciled = Reconciled::default();
        let owners = [
            at_phase("a", &session(1), true, Some(AnchorPhase::ArmIntent)),
            at_phase("b", &session(2), true, Some(AnchorPhase::Identified)),
            at_phase("c", &session(3), true, Some(AnchorPhase::Intent)),
            at_phase("d", &session(4), true, None),
            at_phase("e", &session(5), false, Some(AnchorPhase::ArmIntent)),
            at_phase("f", &session(6), true, Some(AnchorPhase::Intent)),
            at_phase("g", &session(6), true, Some(AnchorPhase::ArmIntent)),
        ];
        reconciled.add(&owners, &[]);
        let marked: Vec<u8> = (1..=7)
            .filter(|n| reconciled.raw_incomplete(&session(*n), turn))
            .collect();
        assert_eq!(marked, [1, 4, 6]);
    }

    /// An inventory the deadline cut short cannot show that an unread anchor
    /// was never armed.
    #[test]
    fn an_incomplete_inventory_never_claims_a_complete_raw_log() {
        let session = SessionId::try_from("s_000000000000").expect("session");
        let turn = TurnNumber::try_from(1).expect("turn");
        let mut reconciled = Reconciled::default();
        reconciled.add(
            &[at_phase("a1", &session, true, Some(AnchorPhase::Intent))],
            &[],
        );
        assert!(!reconciled.raw_incomplete(&session, turn));
        reconciled.incomplete = true;
        assert!(reconciled.raw_incomplete(&session, turn));
    }

    /// The recovered connection is the one the turn launched on: turn 1 keeps
    /// the session's name, later turns add their number.
    #[test]
    fn the_recovered_connection_is_the_turns_own() {
        let session = SessionId::try_from("s_0123456789ab").expect("session");
        let name = |n: u32| {
            recovered_connection(&session, TurnNumber::try_from(n).expect("turn"))
                .expect("connection")
                .as_str()
                .to_owned()
        };
        assert_eq!(name(1), "c_0123456789ab");
        assert_eq!(name(12), "c_0123456789abt12");
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
