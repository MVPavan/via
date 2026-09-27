//! C1 §7.5 crash recovery (runtime-contracts §7): before admission, Host
//! reconciles every committed anchor, then each turn a previous daemon
//! submitted but never ended becomes `unknown` with Host's verified or
//! uncertain cleanup. Nothing is resent: the prompt may have reached the
//! vendor, and process exit proves neither non-submission nor inaction.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use via_adapters::FakeRecovery;
use via_store::{ANCHOR_PAGE_LIMIT, AnchorOwner, TerminalRecord, UnfinishedTurn};

use super::stop::stop_outcome;
use super::terminal::terminal_envelope;
use super::{Accepted, Engine, Terminal, TurnRecord, failure, journal};
use crate::api::{Cancel, Event, EventBody, FailureClass, RawSpan, Timestamps, rfc3339};
use crate::{ApiError, Cleanup, Deadline, RawRef, SessionId, TurnNumber};

/// Event page size, Store's bound.
const PAGE: u32 = 1000;
/// Startup budget for Host's anchor reconciliation (its native stop is 3 s).
const HOST_RECOVERY: Duration = Duration::from_secs(5);

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

    /// Pages through every committed anchor with Host's reports for the same
    /// id range, in bounded memory. An anchor Host did not report in time
    /// stays uncertain; only a Store read or write failure is fatal
    /// (runtime-contracts §7).
    async fn reconcile(&self, deadline: Deadline) -> Result<Reconciled, String> {
        let mut reconciled = Reconciled::default();
        let mut after = None;
        loop {
            let owners = self
                .store
                .anchor_owners_page(after.clone(), ANCHOR_PAGE_LIMIT)
                .await
                .map_err(|error| format!("store_error: {error}"))?;
            let Some(last) = owners.last() else {
                return Ok(reconciled);
            };
            let next = last.anchor_id.clone();
            let reports = if tokio::time::Instant::now() < deadline.instant() {
                match self
                    .adapter
                    .recover_page(after, ANCHOR_PAGE_LIMIT, deadline)
                    .await
                {
                    Ok(reports) => reports,
                    Err(error) if error.is_store_failure() => {
                        return Err(format!("host_reconciliation_failed: {error}"));
                    }
                    // Deadline or unproven evidence: this page stays unreported.
                    Err(_) => Vec::new(),
                }
            } else {
                Vec::new()
            };
            reconciled.add(&owners, &reports);
            if owners.len() < ANCHOR_PAGE_LIMIT as usize {
                return Ok(reconciled);
            }
            after = Some(next);
        }
    }

    /// Commits `cancel.requested`, `cancel.settled` with Host's cleanup and
    /// `turn.ended` (`unknown`) after every event the crashed daemon
    /// committed, citing the raw spans those events reference.
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
            started,
            spans,
        } = self.history(&session, turn).await?;
        // Acceptance is reported only when both its evidence and its event committed.
        let accepted = match (correlation, started) {
            (Some(vendor_turn_id), Some((at, raw_ref))) => Some(Accepted {
                at,
                raw_ref,
                vendor_turn_id,
            }),
            _ => None,
        };
        let mut record = TurnRecord {
            session: session.clone(),
            turn,
            seq: last_seq,
            accepted,
            spans,
            store_failed: false,
            uncertain: None,
        };
        let cancel = self.settle_recovered(&mut record, reconciled).await?;
        let seq = record.seq + 1;
        let ended_at = rfc3339(SystemTime::now());
        let terminal = Terminal {
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
            raw_incomplete: false,
            warnings: Vec::new(),
            cancel: Some(cancel.clone()),
        };
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
            seq,
        );
        let envelope = serde_json::to_value(&envelope).map_err(|_| ApiError::STORE)?;
        journal::commit_terminal(
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
        .await
    }

    /// Records the recovery stop of the turn's orphaned execution (C1 §7.5):
    /// `forced` only when Host's stop found its vendor live; cleanup
    /// `quiescent` only when Host proved every owned group absent, or when no
    /// anchor intent committed, so no process could exist.
    async fn settle_recovered(
        &self,
        record: &mut TurnRecord,
        reconciled: &Reconciled,
    ) -> Result<Cancel, ApiError> {
        let requested_at = rfc3339(SystemTime::now());
        let (quiescent, forced) = reconciled.cleanup(&record.session, record.turn);
        let (outcome, cleanup) = stop_outcome(quiescent, forced);
        self.commit_event(record, EventBody::CancelRequested {}, None)
            .await;
        let cancel = self.settle(record, requested_at, outcome, cleanup).await;
        if record.store_failed {
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
        let mut started = None;
        let mut spans = Vec::new();
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
                    Some("turn.queued") => queued_at = at.map(str::to_owned),
                    Some("turn.started") => {
                        started = at.map(str::to_owned).zip(stored.raw_ref.clone());
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
            queued_at: queued_at.ok_or(ApiError::STORE)?,
            started,
            spans,
        })
    }
}

/// What a recovered turn's envelope cites from the committed history.
struct History {
    /// The session's last committed sequence.
    last_seq: u64,
    queued_at: String,
    /// `turn.started` time and raw span, when acceptance's event committed.
    started: Option<(String, RawRef)>,
    spans: Vec<RawSpan>,
}

/// Host's cleanup evidence for running turns, checked against every
/// committed anchor; anchors of already-ended turns are only counted.
#[derive(Default)]
struct Reconciled {
    /// `(quiescent, forced)` per running owning turn of a committed anchor.
    turns: HashMap<(SessionId, TurnNumber), (bool, bool)>,
    /// Committed anchors Host returned no report for; each stays uncertain.
    missing: usize,
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

    /// `(quiescent, forced)` for a turn; with no committed anchor intent no
    /// process could exist, so nothing needs cleaning.
    fn cleanup(&self, session: &SessionId, turn: TurnNumber) -> (bool, bool) {
        self.turns
            .get(&(session.clone(), turn))
            .copied()
            .unwrap_or((true, false))
    }
}

#[cfg(test)]
mod tests {
    use super::{AnchorOwner, Cleanup, FakeRecovery, Reconciled, SessionId, TurnNumber};

    fn owner(anchor_id: &str, session: &SessionId, turn_running: bool) -> AnchorOwner {
        AnchorOwner {
            anchor_id: anchor_id.to_owned(),
            session_id: session.clone(),
            turn: TurnNumber::try_from(1).expect("turn"),
            turn_running,
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
}
