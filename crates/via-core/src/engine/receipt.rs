//! Receipts: `spawn`, `resume`, the queued-turn commit and `steer`.

use std::{
    sync::{Arc, atomic::Ordering},
    time::SystemTime,
};

use serde_json::{Value, json};
use via_store::{
    OperationRecord, ResumeRecord, SessionSnapshot, SpawnKey, SpawnRecord, StoreError,
};

use super::journal::{self, Head};
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use super::queue::{DAEMON_QUEUE_LIMIT, SESSION_QUEUE_LIMIT, Slot};
use super::{Admission, Engine, Receipted, lock};
use crate::api::{
    Capabilities, Effective, Event, EventBody, Receipt, RoutePlan, TurnReceipt, retry_key, rfc3339,
};
use crate::{
    ApiError, ResumeParams, SessionId, SpawnParams, SteerParams, TurnNumber, hash_handle,
    retry_identity,
};

impl Engine {
    /// A receipt commit that reported failure (C1 §8.1, runtime §7): latches
    /// Store failure and is `store_error` with `commit_outcome`, `unknown`
    /// with `retry: same_key_only` when it may have committed. Restart
    /// recovery settles an unknown one.
    fn receipt_failed(&self, error: &StoreError, admission: &Admission<'_>) -> ApiError {
        let outcome = WriteOutcome::of(error);
        self.store_failure(FailureSite::Receipt, outcome, FailureScope::Request)
            .finish_held(admission);
        match outcome {
            WriteOutcome::Uncertain => ApiError::RECEIPT_UNKNOWN,
            WriteOutcome::NotCommitted => ApiError::RECEIPT_NOT_COMMITTED,
        }
    }

    /// A receipt commit's reply, lost by the test fault backend when armed.
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "the fault backend exists only in tests")
    )]
    fn receipt_reply<T>(&self, reply: Result<T, StoreError>) -> Result<T, StoreError> {
        #[cfg(test)]
        if reply.is_ok() && self.faults.receipt_reply_lost.swap(false, Ordering::AcqRel) {
            return Err(StoreError::Uncertain("injected reply loss".to_owned()));
        }
        reply
    }

    /// Commits a receipt before authorizing any process launch.
    ///
    /// A keyed retry is looked up before any admission check (runtime §6): the
    /// same key, handle and byte-identical `raw_params` replay the stored
    /// receipt, anything else under the key is `idempotency_conflict`.
    pub async fn spawn(
        &self,
        params: SpawnParams,
        raw_params: &str,
    ) -> Result<Receipted, ApiError> {
        let admission = self.admission.lock().await;
        // Runtime §7: no new mutation, not even a keyed replay, after a failed write.
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
        let hash = hash_handle(&params.handle)?;
        let key = match retry_key(params.idempotency_key.as_deref())? {
            Some(key) => {
                let identity = retry_identity(raw_params, &hash)?;
                if let Some(stored) = self
                    .store
                    .spawn_key(key)
                    .await
                    .map_err(|_| ApiError::STORE)?
                {
                    return if stored.identity == identity {
                        Ok(Receipted {
                            receipt: stored.receipt,
                            enqueued: None,
                        })
                    } else {
                        Err(ApiError::IDEMPOTENCY_CONFLICT)
                    };
                }
                Some(SpawnKey {
                    key: key.to_owned(),
                    identity,
                })
            }
            None => None,
        };
        if lock(&self.stop).is_some() {
            return Err(ApiError::DAEMON_STOPPING);
        }
        // Bounds the turns retained for their `store_error` reads.
        journal::admission(&self.store, &self.unresolved).await?;
        if params.harness != "fake" || !self.adapter.fake_available() {
            return Err(ApiError::HARNESS_UNAVAILABLE);
        }
        if params.model != "fake" || params.prompt.is_empty() {
            return Err(ApiError::INVALID_PARAMS);
        }
        let effective = Effective::fake(&params.model, &params.per_turn().fake_overrides()?);
        if self.queued.load(Ordering::Acquire) >= DAEMON_QUEUE_LIMIT {
            return Err(ApiError::QUEUED_AT_CAPACITY);
        }
        let turn = TurnNumber::try_from(1).map_err(|_| ApiError::STORE)?;
        let session = crate::api::new_session_id()?;
        let plan = RoutePlan::fake();
        let receipt = Receipt {
            session_id: session.clone(),
            turn: format!("{}/{}", session.as_str(), turn.get()),
            state: "queued",
            warnings: plan.warnings(),
            plan,
            capabilities: Capabilities::fake(),
            effective,
        };
        let receipt = serde_json::to_value(&receipt).map_err(|_| ApiError::STORE)?;
        let at = rfc3339(SystemTime::now());
        let initial_event = Event {
            seq: 1,
            session_id: &session,
            turn: Some(turn.get()),
            late: false,
            at: &at,
            raw_ref: None,
            body: EventBody::TurnQueued { queue_position: 0 },
        }
        .to_value()?;
        #[cfg(test)]
        self.hold(&self.faults.hold_receipt).await;
        let stored = self
            .store
            .commit_keyed_spawn(
                SpawnRecord {
                    session_id: session.clone(),
                    handle_hash: hash,
                    receipt: receipt.clone(),
                    params: json!({"harness":"fake","model":"fake"}),
                    effective: receipt["effective"].clone(),
                    prompt: params.prompt,
                    initial_event,
                },
                key,
            )
            .await;
        if let Err(error) = self.receipt_reply(stored) {
            return Err(self.receipt_failed(&error, &admission));
        }
        let slot = Slot::new(Head::new(Some(2)));
        lock(&self.sessions).insert(session.clone(), Arc::clone(&slot));
        self.receipted(&session, turn, &slot);
        Ok(Receipted {
            receipt,
            enqueued: Some((session, turn)),
        })
    }

    /// C1 §3.3: authenticates, replays a keyed retry, then commits the next
    /// turn `queued` with its `turn.queued` event before the receipt.
    pub async fn resume(
        &self,
        params: ResumeParams,
        raw_params: &str,
    ) -> Result<Receipted, ApiError> {
        let admission = self.admission.lock().await;
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
        let hash = hash_handle(&params.handle)?;
        let key = retry_key(params.op_key.as_deref())?;
        if params.prompt.is_empty() {
            return Err(ApiError::INVALID_PARAMS);
        }
        params.refuse_session_scope()?;
        let overrides = params.per_turn().fake_overrides()?;
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
        let operation = match key {
            Some(key) => {
                let identity = retry_identity(raw_params, &hash)?;
                let stored = self
                    .store
                    .operation(&session, key)
                    .await
                    .map_err(|_| ApiError::STORE)?;
                if let Some(stored) = stored {
                    return if stored.identity == identity {
                        Ok(Receipted {
                            enqueued: None,
                            receipt: stored.result,
                        })
                    } else {
                        Err(ApiError::IDEMPOTENCY_CONFLICT)
                    };
                }
                Some((key.to_owned(), identity))
            }
            None => None,
        };
        // Design §4: a closing session refuses `resume`, from Store's gate
        // or, after an uncertain `Closing`, from memory.
        if snapshot.closed || snapshot.closing || lock(&self.closing).contains(&session) {
            return Err(ApiError::SESSION_CLOSED);
        }
        if lock(&self.stop).is_some() {
            return Err(ApiError::DAEMON_STOPPING);
        }
        journal::admission(&self.store, &self.unresolved).await?;
        if snapshot.queued >= SESSION_QUEUE_LIMIT {
            return Err(ApiError::QUEUE_FULL);
        }
        if self.queued.load(Ordering::Acquire) >= DAEMON_QUEUE_LIMIT {
            return Err(ApiError::QUEUED_AT_CAPACITY);
        }
        // C1 P5: omitted values inherit from the latest accepted turn,
        // resolved under `admission`, which every receipt commit holds;
        // Store's next-turn check in the same transaction confirms it.
        let latest: Effective = snapshot
            .latest_effective
            .clone()
            .and_then(|latest| serde_json::from_value(latest).ok())
            .ok_or(ApiError::STORE)?;
        let effective = latest.inherit(&overrides);
        self.queue_turn(
            session,
            &snapshot,
            (params.prompt, effective),
            operation,
            &admission,
        )
        .await
    }

    /// Commits the session's next turn `queued` with its frozen effective
    /// values, its `turn.queued` event at the shared head and any `op_key`
    /// result, then returns the turn receipt.
    async fn queue_turn(
        &self,
        session: SessionId,
        snapshot: &SessionSnapshot,
        (prompt, effective): (String, Effective),
        operation: Option<(String, Vec<u8>)>,
        admission: &Admission<'_>,
    ) -> Result<Receipted, ApiError> {
        let turn = TurnNumber::try_from(snapshot.turns + 1).map_err(|_| ApiError::STORE)?;
        let slot = self.slot_for(&session);
        let receipt = TurnReceipt {
            turn: format!("{}/{}", session.as_str(), turn.get()),
            state: "queued",
            queue_position: snapshot.queued,
            effective,
            warnings: RoutePlan::fake().warnings(),
        };
        let receipt = serde_json::to_value(&receipt).map_err(|_| ApiError::STORE)?;
        let head = slot
            .head
            .lock(&self.store, &session)
            .await
            .map_err(|_| ApiError::STORE)?;
        let at = rfc3339(SystemTime::now());
        let event = Event {
            seq: head.next(),
            session_id: &session,
            turn: Some(turn.get()),
            late: false,
            at: &at,
            raw_ref: None,
            body: EventBody::TurnQueued {
                queue_position: snapshot.queued,
            },
        }
        .to_value()?;
        let committed = self
            .store
            .commit_resume(ResumeRecord {
                session_id: session.clone(),
                turn,
                prompt,
                effective: receipt["effective"].clone(),
                event,
                operation: operation.map(|(op_key, identity)| OperationRecord {
                    op_key,
                    identity,
                    result: receipt.clone(),
                }),
            })
            .await;
        match self.receipt_reply(committed) {
            Ok(()) => head.committed(1),
            // Store's same-transaction closing check: a refusal, not a
            // Store failure; nothing was written (design §4).
            Err(StoreError::Refused(_)) => {
                drop(head);
                drop(slot);
                self.retire(&session);
                return Err(ApiError::SESSION_CLOSED);
            }
            Err(error) => {
                if journal::may_have_committed(&error) {
                    head.lost();
                } else {
                    drop(head);
                }
                // A slot this request created holds nothing: retire it.
                drop(slot);
                self.retire(&session);
                return Err(self.receipt_failed(&error, admission));
            }
        }
        self.receipted(&session, turn, &slot);
        Ok(Receipted {
            receipt,
            enqueued: Some((session, turn)),
        })
    }

    /// Authenticates before reporting fake's unsupported mutation capability.
    pub async fn steer(&self, params: SteerParams) -> Result<Value, ApiError> {
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
        let hash = hash_handle(&params.handle)?;
        if !self
            .store
            .authenticate(&params.session, &hash)
            .await
            .map_err(|_| ApiError::STORE)?
        {
            return Err(ApiError::INVALID_HANDLE);
        }
        Err(ApiError::UNSUPPORTED_VERB)
    }
}
