//! `close` and the `closing` gate (C1 §3.6, §7.1; design §4): the admission
//! step under `admission`, the dispatcher's close pass, and the restart's
//! close completion.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use serde_json::Value;
use via_store::{
    CancelCause, CloseIntent, ClosedOutcome, ClosedRecord, ClosingRecord, KeyedOperation,
    OperationVerb, StoreError,
};

use super::drive::{Cancelled, Step};
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use super::queue::{CLOSE_ALLOWANCE, CloseOrder, CloseWatch, Owner, Slot, Sweep};
use super::stop::StopMode;
use super::{Admission, Engine, lock};
use crate::api::{DEFAULT_CLOSE_DEADLINE_MS, Event, EventBody, retry_key, rfc3339};
use crate::{ApiError, CloseMode, CloseParams, Deadline, SessionId, hash_handle, retry_identity};

/// Reason recorded on a `close`'s `session.closed` (C1 §7.1).
const CLOSE_REASON: &str = "close";

/// Delay between absence-check passes while groups stay held.
const ABSENCE_POLL: Duration = Duration::from_millis(50);

/// Closing sessions read per page by the restart completion.
const CLOSING_PAGE: u32 = 256;

/// What a keyed close's replay found (design §4 step 2).
enum Replay {
    /// A committed close result.
    Result(Value),
    /// A close attempt in progress under the key.
    InProgress(CloseWatch),
    /// No result and no attempt: new close work.
    New,
}

impl Engine {
    /// C1 §3.6 `close`: design §4's admission step under `admission`, then
    /// the caller waits, holding no lock, for the close attempt's outcome.
    pub async fn close(&self, params: CloseParams, raw_params: &str) -> Result<Value, ApiError> {
        let hash = hash_handle(&params.handle)?;
        let key = retry_key(params.op_key.as_deref())?;
        let deadline_ms = params.deadline_ms.unwrap_or(DEFAULT_CLOSE_DEADLINE_MS);
        if deadline_ms == 0 {
            return Err(ApiError::INVALID_PARAMS);
        }
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_millis(deadline_ms))
            .ok_or(ApiError::INVALID_PARAMS)?;
        let session = params.session;
        let admission = self.admission.lock().await;
        // Step 1: after the latch the reply is `store_error` [O1.D13].
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
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
        // Step 2: `op_key` replay. Only a committed result and an attempt in
        // progress bypass the fence [r3.1, r4.1].
        let operation = match key {
            Some(key) => {
                let identity = retry_identity(raw_params, &hash)?;
                match self.replay(&session, key, &identity).await? {
                    Replay::Result(result) => return Ok(result),
                    Replay::InProgress(watch) => return self.await_close(watch, admission).await,
                    Replay::New => Some(CloseIntent {
                        op_key: key.to_owned(),
                        identity,
                    }),
                }
            }
            None => None,
        };
        // Step 3: a closed session's result, derived for any closure.
        if snapshot.closed {
            return self
                .store
                .session_close_result(&session)
                .await
                .map_err(|_| ApiError::STORE)?
                .ok_or(ApiError::STORE);
        }
        // Step 4: the stop fence [r1.5, r3.2]: final shutdown was entered,
        // whatever the stop mode, or an idle or force stop was accepted.
        if self.final_shutdown()
            || matches!(*lock(&self.stop), Some(StopMode::Idle | StopMode::Force))
        {
            return Err(ApiError::DAEMON_STOPPING);
        }
        let slot = self.slot_for(&session);
        // Step 5: a close in progress; a second forcing close escalates it.
        if let Some(watch) = slot.close_watch() {
            if params.mode == CloseMode::Force {
                slot.escalate_close(tokio::time::Instant::now());
            }
            return self.await_close(watch, admission).await;
        }
        let durably_closing = snapshot.closing || lock(&self.closing).contains(&session);
        if !durably_closing {
            // Step 6: `Closing` and the intent row in one transaction.
            let committed = self
                .store
                .commit_closing(ClosingRecord {
                    session_id: session.clone(),
                    operation: operation.clone(),
                })
                .await;
            if let Err(error) = committed {
                drop(slot);
                return Err(self.closing_failed(&session, &error, &admission));
            }
        }
        // Step 7: publication and dispatcher start, atomically under
        // `admission` [r4.4].
        lock(&self.closing).insert(session.clone());
        let order = CloseOrder::new(params.mode, deadline, operation);
        let (watch, start) = slot.set_close(order, tokio::time::Instant::now());
        if start {
            self.request_start(session.clone());
        }
        self.await_close(watch, admission).await
    }

    /// Design §4 step 2 for a keyed close; the caller holds `admission`.
    async fn replay(
        &self,
        session: &SessionId,
        key: &str,
        identity: &[u8],
    ) -> Result<Replay, ApiError> {
        let stored = self
            .store
            .keyed_operation(session, key)
            .await
            .map_err(|_| ApiError::STORE)?;
        Ok(match stored {
            Some(stored) if stored.verb != OperationVerb::Close || stored.identity != identity => {
                return Err(ApiError::IDEMPOTENCY_CONFLICT);
            }
            Some(KeyedOperation {
                result: Some(result),
                ..
            }) => Replay::Result(result),
            // An intent row with no close order is new close work [r4.1].
            Some(_) => self
                .slot(session)
                .and_then(|slot| slot.close_watch())
                .map_or(Replay::New, Replay::InProgress),
            None => Replay::New,
        })
    }

    /// A `Closing` commit that failed (design §4 step 6): not committed, no
    /// state changes; uncertain, memory treats the session as closing, so
    /// `resume` is refused. Either way the failure hook runs.
    fn closing_failed(
        &self,
        session: &SessionId,
        error: &StoreError,
        admission: &Admission<'_>,
    ) -> ApiError {
        if matches!(error, StoreError::Refused(_)) {
            // Closed meanwhile: not possible under `admission`, never a failure.
            self.retire(session);
            return ApiError::SESSION_CLOSED;
        }
        let outcome = WriteOutcome::of(error);
        if outcome.head_unknown() {
            lock(&self.closing).insert(session.clone());
        }
        self.retire(session);
        self.store_failure(
            FailureSite::Closing,
            outcome,
            FailureScope::Session(session),
        )
        .finish_held(admission);
        outcome.api_error()
    }

    /// Subscribes to the close attempt under `admission`, releases it, and
    /// awaits the outcome [r4.6]. The attempt's watch retains its outcome,
    /// so a publication before the subscription still counts [r5.8, r6.6].
    async fn await_close(
        &self,
        watch: CloseWatch,
        admission: Admission<'_>,
    ) -> Result<Value, ApiError> {
        #[cfg(test)]
        self.hold(&self.faults.hold_before_subscribe).await;
        // After the order check, before the subscription (design §10).
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.close.before_subscribe").await;
        let mut outcome = watch.subscribe();
        drop(watch);
        drop(admission);
        let reply = outcome
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|reply| reply.clone());
        reply.unwrap_or(Err(ApiError::DAEMON_STOPPING))
    }

    /// The dispatcher's close pass (design §4 dispatcher steps 1–7), after
    /// its force and latch checks. `Some` continues the dispatcher loop, where
    /// a force or latch exit publishes on the close watch; `None` means the
    /// dispatcher exited after `Closed`. `refused` counts Store's refusals of
    /// `Closed` in this attempt.
    pub(super) async fn close_pass(
        &self,
        slot: &Arc<Slot>,
        session: &SessionId,
        refused: &mut bool,
    ) -> Option<Step> {
        let task = slot.close_task()?;
        let cause = (CancelCause::Close, task.requested_at.clone());
        let mut force = self.force.subscribe();
        // Steps 1–2: cancel every `Waiting` turn FIFO with cause `close`;
        // wait for request-owned cancellations. Step 3: a running turn is
        // inline, so none runs here.
        loop {
            if *force.borrow() {
                return Some(Step::Next);
            }
            match slot.sweep(Some(&cause)) {
                Sweep::Done => break,
                Sweep::Wait => {
                    tokio::select! {
                        () = slot.woken() => {}
                        _ = force.wait_for(|forced| *forced) => {}
                    }
                }
                Sweep::Cancel(turn, cause) => {
                    let cancelled = self
                        .cancel_queued(slot, session, (turn, Owner::Dispatcher), false, cause)
                        .await;
                    match cancelled {
                        Cancelled::Committed(_) => {}
                        // The claim stays; the dispatcher timer retries it.
                        Cancelled::Unread => return Some(Step::Wait),
                        Cancelled::Failed(_) | Cancelled::Latched | Cancelled::Expired => {
                            slot.cancel_failed(turn, cancelled.published());
                            return Some(Step::Next);
                        }
                    }
                }
            }
        }
        // Step 4: the bounded absence check, selecting on force [r5.9].
        let bound = task
            .deadline
            .min(tokio::time::Instant::now() + CLOSE_ALLOWANCE);
        tokio::select! {
            biased;
            _ = force.wait_for(|forced| *forced) => return Some(Step::Next),
            () = self.absence_check(session, bound) => {}
        }
        #[cfg(test)]
        self.hold(&self.faults.hold_before_closed).await;
        // Step 5: under `admission`, force is re-checked [r5.9]; none starts
        // once `failure_pending` is observed [O1.D12].
        let admission = self.admission.lock().await;
        if *lock(&self.stop) == Some(StopMode::Force) || self.store_failed() {
            return Some(Step::Next);
        }
        match self.commit_closed(slot, session, task.operation).await {
            Ok(ClosedOutcome::Closed(result)) => {
                // A confirmed `Closed` leaves the durable closing set [r3.5].
                lock(&self.closing).remove(session);
                slot.finish_close(Ok(result));
                *refused = false;
                // Step 7: notify, then exit and retire the slot.
                if self.exit_held(session, slot, &admission) {
                    return None;
                }
                Some(Step::Next)
            }
            // Step 6 [r1.7]: once more, then `admission_refused`.
            Ok(ClosedOutcome::Unfinished) => {
                if *refused {
                    *refused = false;
                    slot.finish_close(Err(ApiError::CLOSE_REFUSED));
                } else {
                    *refused = true;
                }
                Some(Step::Next)
            }
            Err(outcome) => {
                // `closing` stays durable, and the session in the closing set.
                *refused = false;
                self.store_failure(FailureSite::Closed, outcome, FailureScope::Session(session))
                    .finish_held(&admission);
                slot.finish_close(Err(outcome.api_error()));
                Some(Step::Next)
            }
        }
    }

    /// Design §4's bounded absence check: re-probe passes over held groups,
    /// the session's own first, until nothing is held or `bound`. A group
    /// still unproven leaves the close's cleanup `uncertain`, which `Closed`
    /// derives from the durable proofs.
    async fn absence_check(&self, session: &SessionId, bound: tokio::time::Instant) {
        loop {
            let pass = tokio::time::timeout_at(
                bound,
                self.adapter
                    .reprobe_held(Deadline::at(bound), Some(session.clone())),
            )
            .await;
            match pass {
                Ok(Ok(report)) if report.held == report.proved => return,
                // An uncertain proof commit is design §7.2 row 12 (S5).
                Ok(Err(_)) | Err(_) => return,
                Ok(Ok(_)) => {}
            }
            let now = tokio::time::Instant::now();
            if now >= bound {
                return;
            }
            tokio::time::sleep_until(bound.min(now + ABSENCE_POLL)).await;
        }
    }

    /// Commits `Closed` for a closing session at its head's next sequence;
    /// the caller holds `admission` (lock order `admission` → head). A head
    /// that cannot be read wrote nothing.
    async fn commit_closed(
        &self,
        slot: &Slot,
        session: &SessionId,
        operation: Option<CloseIntent>,
    ) -> Result<ClosedOutcome, WriteOutcome> {
        let head = slot
            .head
            .lock(&self.store, session)
            .await
            .map_err(|_| WriteOutcome::NotCommitted)?;
        let event = Event {
            seq: head.next(),
            session_id: session,
            turn: None,
            late: false,
            at: &rfc3339(SystemTime::now()),
            raw_ref: None,
            body: EventBody::SessionClosed {
                reason: CLOSE_REASON,
            },
        }
        .to_value()
        .map_err(|_| WriteOutcome::NotCommitted)?;
        let committed = self
            .store
            .commit_closed(ClosedRecord {
                session_id: session.clone(),
                event,
                operation,
            })
            .await;
        match committed {
            Ok(ClosedOutcome::Closed(result)) => {
                head.committed(1);
                Ok(ClosedOutcome::Closed(result))
            }
            Ok(ClosedOutcome::Unfinished) => Ok(ClosedOutcome::Unfinished),
            Err(error) => {
                let outcome = WriteOutcome::of(&error);
                if outcome.head_unknown() {
                    head.lost();
                }
                Err(outcome)
            }
        }
    }

    /// Every durably `closing` session, read in bounded pages for the
    /// restart close completion (design §4 "Restart").
    pub(super) async fn closing_on_disk(&self) -> Result<Vec<SessionId>, String> {
        let mut sessions = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .store
                .closing_sessions_page(after.clone(), CLOSING_PAGE)
                .await
                .map_err(|error| format!("store_error: {error}"))?;
            let full = page.len() == CLOSING_PAGE as usize;
            after = page.last().cloned();
            sessions.extend(page);
            if !full {
                return Ok(sessions);
            }
        }
    }

    /// The restart close completion (design §4): after the queued pass
    /// cancelled the session's queued turns with cause `close`, one bounded
    /// absence check, then `Closed`, derived as for a live close. Any
    /// failure fails startup [O1.D9].
    pub(super) async fn finish_restart_close(
        &self,
        session: &SessionId,
        bound: tokio::time::Instant,
    ) -> Result<(), String> {
        self.absence_check(session, bound).await;
        let slot = self.slot_for(session);
        let closed = self.commit_closed(&slot, session, None).await;
        drop(slot);
        self.retire(session);
        match closed {
            Ok(ClosedOutcome::Closed(_)) => Ok(()),
            Ok(ClosedOutcome::Unfinished) => Err(format!(
                "store_error: closing session {session} still has unfinished turns"
            )),
            Err(outcome) => Err(format!(
                "store_error: closing session {session} could not be closed ({outcome:?})"
            )),
        }
    }
}
