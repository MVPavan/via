//! `cancel` (C1 §3.5, design §3): a queued turn is dropped by its one
//! cancellation owner; a claimed or running turn gets a stop order, which its
//! run loop acknowledges; a terminal turn replies with its recorded `cancel`.

use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use via_store::CancelCause;

use super::drive::Cancelled;
use super::queue::{Ack, CancelStep, QueuedOutcome, StopSpec};
use super::stop::StopMode;
use super::{Engine, journal, lock};
use crate::api::{DEFAULT_FORCE_AFTER_MS, rfc3339};
use crate::{ApiError, CancelParams, SessionId, TurnNumber, hash_handle};

impl Engine {
    /// C1 §3.5 `cancel`, in design §3's order of checks: authenticate; after
    /// the latch `store_error`; resolve the turn; a terminal turn replies
    /// `already_terminal: true`; once force is accepted `daemon_stopping`.
    /// Allowed during a drain and while the session is closing.
    pub async fn cancel(&self, params: CancelParams) -> Result<Value, ApiError> {
        let hash = hash_handle(&params.handle)?;
        if params.turn == Some(0) {
            return Err(ApiError::INVALID_PARAMS);
        }
        let session = params.session;
        let snapshot = self
            .store
            .session_snapshot(&session)
            .await
            .map_err(|_| ApiError::STORE)?
            .ok_or(ApiError::SESSION_NOT_FOUND)?;
        if !self
            .store
            .authenticate(&session, &hash)
            .await
            .map_err(|_| ApiError::STORE)?
        {
            return Err(ApiError::INVALID_HANDLE);
        }
        // O1.D13: after the latch its force stop performs the cleanup.
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
        let turn = self.cancel_target(&session, params.turn, snapshot.turns)?;
        let address = format!("{}/{}", session.as_str(), turn.get());
        if let Some(envelope) =
            journal::read_result(&self.store, &self.unresolved, &session, turn).await?
        {
            return Ok(reply(&address, &envelope, true));
        }
        if *lock(&self.stop) == Some(StopMode::Force) {
            return Err(ApiError::DAEMON_STOPPING);
        }
        let force_after = params.force_after_ms.unwrap_or(DEFAULT_FORCE_AFTER_MS);
        let spec = StopSpec::Cancel {
            force_after: Duration::from_millis(force_after),
        };
        let mut acknowledged = false;
        let mut rechecked = false;
        loop {
            let step = match self.slot(&session) {
                Some(slot) => slot.cancel_step(turn, spec, tokio::time::Instant::now()),
                None => CancelStep::Absent,
            };
            match step {
                CancelStep::Queued => return self.cancel_waiting(&session, turn, &address).await,
                CancelStep::Ordered(mut ack) => {
                    // Waits holding no lock for the acknowledgement or the drop.
                    let observed = ack
                        .wait_for(Option::is_some)
                        .await
                        .ok()
                        .and_then(|ack| ack.clone());
                    match observed {
                        Some(Ack::Requested(requested_at)) => {
                            acknowledged = true;
                            if !params.wait {
                                return Ok(json!({
                                    "turn": address,
                                    "state": "running",
                                    "already_terminal": false,
                                    "cancel": {
                                        "outcome": "requested",
                                        "cleanup": "pending",
                                        "requested_at": requested_at,
                                        "settled_at": null,
                                    },
                                }));
                            }
                            while ack.changed().await.is_ok() {}
                        }
                        Some(Ack::Failed) => return Err(ApiError::STORE),
                        // Dropped unobserved: the claim rolled back or the
                        // turn ended; the next step reads which.
                        None => {}
                    }
                }
                CancelStep::Settling(mut ack) => {
                    // Design §3.3 [r1.4]: no order; wait for the drop. The
                    // seam witnesses this step (acknowledgement only).
                    #[cfg(feature = "test-failpoints")]
                    let _ = via_store::failpoint::hit_async("core.cancel.settling").await;
                    while ack.changed().await.is_ok() {}
                }
                CancelStep::Joined(mut outcome) => {
                    // One owner per cancellation [r1.3]: reply from its outcome.
                    let outcome = outcome
                        .wait_for(Option::is_some)
                        .await
                        .ok()
                        .and_then(|outcome| *outcome);
                    queued_failure(outcome)?;
                    let envelope = self.await_terminal(&session, turn).await?;
                    return Ok(reply(&address, &envelope, false));
                }
                CancelStep::Absent if !rechecked => {
                    // A receipt registers its turn under `admission` after its
                    // commit: read the slot once more after it.
                    rechecked = true;
                    drop(self.admission.lock().await);
                }
                CancelStep::Absent => {
                    // Design §3.3 [r3.4]: the drop is not a terminal.
                    let envelope = self.await_terminal(&session, turn).await?;
                    return Ok(reply(&address, &envelope, !acknowledged));
                }
            }
        }
    }

    /// Design §3 step 4: the given turn, else the session's running turn,
    /// otherwise its latest turn.
    fn cancel_target(
        &self,
        session: &SessionId,
        turn: Option<u32>,
        turns: u32,
    ) -> Result<TurnNumber, ApiError> {
        match turn {
            Some(turn) if turn > turns => Err(ApiError::TURN_NOT_FOUND),
            Some(turn) => TurnNumber::try_from(turn).map_err(|_| ApiError::INVALID_PARAMS),
            None => match self.slot(session).and_then(|slot| slot.running_turn()) {
                Some(turn) => Ok(turn),
                None => TurnNumber::try_from(turns).map_err(|_| ApiError::TURN_NOT_FOUND),
            },
        }
    }

    /// Design §3.2: the caller owns the `Waiting` turn's cancellation and
    /// commits it `queued → cancelled` with cause `cancel`.
    async fn cancel_waiting(
        &self,
        session: &SessionId,
        turn: TurnNumber,
        address: &str,
    ) -> Result<Value, ApiError> {
        let slot = self.slot(session).ok_or(ApiError::STORE)?;
        let requested_at = rfc3339(SystemTime::now());
        let cancelled = self
            .cancel_queued(
                &slot,
                session,
                turn,
                false,
                Some((CancelCause::Cancel, requested_at)),
            )
            .await;
        let published = cancelled.published();
        match cancelled {
            Cancelled::Committed(cancel) => Ok(json!({
                "turn": address,
                "state": "cancelled",
                "already_terminal": false,
                "cancel": serde_json::to_value(cancel).map_err(|_| ApiError::STORE)?,
            })),
            Cancelled::Unread | Cancelled::Expired | Cancelled::Failed(_) | Cancelled::Latched => {
                slot.cancel_failed(turn, published);
                queued_failure(Some(published)).map(|()| Value::Null)
            }
        }
    }
}

/// A queued cancellation's failure as C1 `store_error` (design §3.2): a
/// failed read is plain, with no turn fields [r1.13]; a commit reports its
/// outcome.
fn queued_failure(outcome: Option<QueuedOutcome>) -> Result<(), ApiError> {
    match outcome {
        Some(QueuedOutcome::Committed) => Ok(()),
        Some(QueuedOutcome::ReadFailed) | None => Err(ApiError::STORE),
        Some(QueuedOutcome::NotCommitted) => Err(ApiError::RECEIPT_NOT_COMMITTED),
        Some(QueuedOutcome::Uncertain) => Err(ApiError::RECEIPT_UNKNOWN),
    }
}

/// The C1 §3.5 result from a committed envelope.
fn reply(address: &str, envelope: &Value, already_terminal: bool) -> Value {
    json!({
        "turn": address,
        "state": envelope["state"],
        "already_terminal": already_terminal,
        "cancel": envelope["cancel"],
    })
}
