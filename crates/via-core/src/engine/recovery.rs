//! C1 §7.5 crash recovery (runtime-contracts §7): before admission, every turn
//! a previous daemon submitted but never ended becomes `unknown`. Nothing is
//! resent: the prompt may have reached the vendor, and process exit proves
//! neither non-submission nor inaction.

use std::time::SystemTime;

use serde_json::Value;
use via_store::{TerminalRecord, UnfinishedTurn};

use super::terminal::terminal_envelope;
use super::{Accepted, Engine, Terminal, journal};
use crate::api::{Event, EventBody, RawSpan, Timestamps, rfc3339};
use crate::{ApiError, RawRef, SessionId, TurnNumber};

/// Event page size, Store's bound.
const PAGE: u32 = 1000;

impl Engine {
    /// Resolves every durable nonterminal submitted turn as `unknown` and
    /// returns how many; the daemon admits requests only after this succeeds.
    /// A queued turn without submission intent stays queued.
    pub async fn recover(&self) -> Result<usize, ApiError> {
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
                self.recover_turn(turn).await?;
                recovered += 1;
            }
        }
    }

    /// Commits `turn.ended` (`unknown`) and its envelope after every event the
    /// crashed daemon committed, citing the raw spans those events reference.
    async fn recover_turn(&self, unfinished: UnfinishedTurn) -> Result<(), ApiError> {
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
        let seq = last_seq + 1;
        let ended_at = rfc3339(SystemTime::now());
        let terminal = Terminal {
            state: "unknown",
            failure: None,
            stop_reason: "error",
            vendor_stop_reason: None,
            final_text: String::new(),
            exit: None,
            raw_ref: None,
            raw_incomplete: false,
            warnings: Vec::new(),
            cancel: None,
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
                cancel: None,
            },
        }
        .to_value()?;
        let timestamps = Timestamps {
            queued_at,
            submitted_at: Some(submitted_at),
            accepted_at: accepted.as_ref().map(|accepted| accepted.at.clone()),
            ended_at,
        };
        // The crashed daemon's clock is gone: no duration is claimed.
        let envelope = terminal_envelope(
            &session, turn, terminal, accepted, spans, timestamps, None, seq,
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
