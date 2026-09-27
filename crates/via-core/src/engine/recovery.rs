//! C1 §7.5 crash recovery (runtime-contracts §7): before admission, Host
//! reconciles every committed anchor, then each turn a previous daemon
//! submitted but never ended becomes `unknown` with Host's verified or
//! uncertain cleanup. Nothing is resent: the prompt may have reached the
//! vendor, and process exit proves neither non-submission nor inaction.

use std::time::{Duration, SystemTime};

use serde_json::Value;
use via_adapters::FakeRecovery;
use via_store::{TerminalRecord, UnfinishedTurn};

use super::stop::stop_outcome;
use super::terminal::terminal_envelope;
use super::{Accepted, Engine, Terminal, TurnRecord, journal};
use crate::api::{Cancel, Event, EventBody, RawSpan, Timestamps, rfc3339};
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
    pub async fn recover(&self) -> Result<usize, ApiError> {
        let deadline = Deadline::at(tokio::time::Instant::now() + HOST_RECOVERY);
        // A failed reconciliation proves nothing: every cleanup stays uncertain.
        let host = self.adapter.recover(deadline).await.ok();
        let mut recovered = 0;
        loop {
            let turns = self
                .store
                .unfinished_turns()
                .await
                .map_err(|_| ApiError::STORE)?;
            if turns.is_empty() {
                return Ok(recovered);
            }
            // Each resolution commits or fails recovery, so the next read
            // never returns the same turn again.
            for turn in turns {
                self.recover_turn(turn, host.as_deref()).await?;
                recovered += 1;
            }
        }
    }

    /// Commits `cancel.requested`, `cancel.settled` with Host's cleanup and
    /// `turn.ended` (`unknown`) after every event the crashed daemon
    /// committed, citing the raw spans those events reference.
    async fn recover_turn(
        &self,
        unfinished: UnfinishedTurn,
        host: Option<&[FakeRecovery]>,
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
        let cancel = self.settle_recovered(&mut record, host).await?;
        let seq = record.seq + 1;
        let ended_at = rfc3339(SystemTime::now());
        let terminal = Terminal {
            state: "unknown",
            failure: None,
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
                failure: None,
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
    /// anchor intent committed, so no process could exist. A failed Host
    /// reconciliation leaves cleanup `uncertain`.
    async fn settle_recovered(
        &self,
        record: &mut TurnRecord,
        host: Option<&[FakeRecovery]>,
    ) -> Result<Cancel, ApiError> {
        let requested_at = rfc3339(SystemTime::now());
        let owned = |report: &&FakeRecovery| {
            report.session_id == record.session && report.turn == record.turn
        };
        let (quiescent, forced) = match host {
            None => (false, false),
            Some(reports) => (
                reports
                    .iter()
                    .filter(owned)
                    .all(|report| report.cleanup == Cleanup::Quiescent),
                reports.iter().filter(owned).any(|report| report.forced),
            ),
        };
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
