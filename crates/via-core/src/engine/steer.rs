//! Keyed `steer` (C1 §3, §3.4; via-jm4.36): the key's answer is its
//! durable intent's stored outcome. The intent row is committed before the
//! input goes to the driver; the outcome is recorded once, with the
//! `steer.delivered` event that reports the delivery, or alone for a
//! refusal, and every repeat replays it. The attempt runs on its own task,
//! never the caller's, so a caller that goes away ends none of it; a
//! repeat while it runs waits for it. Only an intent left unresolved after
//! its attempt ended, by the lane resolving it without a delivery commit
//! or by a daemon restart, gets the uncertain outcome; the input is never
//! sent again.

#[cfg(test)]
use std::sync::atomic::Ordering;
use std::{borrow::Cow, collections::HashMap, sync::Mutex as StdMutex};

use serde_json::{Value, json};
use tokio::{sync::watch, task::JoinSet};
use via_adapters::{SteerDelivery, Verb};
use via_store::{KeyedOperation, OperationVerb, SteerIntent, SteerOutcome, StoreError};

use super::drive::steer_delivery;
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use super::{Admission, Engine, lock};
use crate::intake::{self, Frozen};
use crate::{ApiError, SessionId, SteerParams};

/// The outcome recorded for a keyed steer whose durable intent stayed
/// unresolved after its attempt ended (C1 §3.4): VIA cannot tell whether
/// its input was applied. `recorded: false` tells it from a driver's
/// `NotDelivered` refusal, whose message speaks of the vendor (K2 r1 #5).
pub(super) fn uncertain() -> Value {
    json!({"refused":"steer_failed","reason":"not_delivered","delivery":"uncertain",
           "recorded":false})
}

/// A delivered steer's reply, and its keyed outcome (C1 §3.4).
pub(super) fn delivered(turn: &str, delivery: &SteerDelivery) -> Value {
    json!({"turn": turn, "delivery": steer_delivery(delivery)})
}

/// A refusal's stored form, by its C1 names; `None` for an answer that is
/// not a steer's outcome, such as `store_error`.
fn stored_refusal(error: &ApiError) -> Option<Value> {
    match (error.kind, error.reason) {
        ("no_active_turn" | "turn_mismatch" | "unsupported_verb", _) => {
            Some(json!({"refused": error.kind}))
        }
        ("admission_refused", Some(reason @ "control_lane_full")) => {
            Some(json!({"refused": error.kind, "reason": reason}))
        }
        ("steer_failed", Some(reason)) => {
            let delivery = error
                .named
                .as_ref()
                .and_then(|named| named.delivery.as_deref())?;
            Some(json!({"refused": error.kind, "reason": reason, "delivery": delivery}))
        }
        _ => None,
    }
}

/// The answer a stored outcome replays; one that does not decode is
/// `store_error`.
fn replay(stored: Value, frozen: &Frozen) -> Result<Value, ApiError> {
    let Some(refused) = stored.get("refused") else {
        return Ok(stored);
    };
    if stored.get("recorded") == Some(&Value::Bool(false)) {
        return Err(ApiError::steer_unrecorded());
    }
    let reason = stored.get("reason").and_then(Value::as_str);
    Err(match (refused.as_str(), reason) {
        (Some("no_active_turn"), _) => ApiError::NO_ACTIVE_TURN,
        (Some("turn_mismatch"), _) => ApiError::TURN_MISMATCH,
        (Some("unsupported_verb"), _) => intake::unsupported_on(Verb::Steer, frozen),
        (Some("admission_refused"), Some("control_lane_full")) => ApiError::CONTROL_LANE_FULL,
        (Some("steer_failed"), Some(reason)) => {
            let reason = match reason {
                "not_steerable" => "not_steerable",
                "not_delivered" => "not_delivered",
                "not_recorded" => "not_recorded",
                _ => return Err(ApiError::STORE),
            };
            let delivery = stored
                .get("delivery")
                .and_then(Value::as_str)
                .ok_or(ApiError::STORE)?;
            ApiError::steer_failed(reason, Cow::Owned(delivery.to_owned()))
        }
        _ => ApiError::STORE,
    })
}

/// The keyed steer attempts, which the Engine owns, never their callers
/// (runtime §2, K2 r2 #1). Each runs as a task of the set until its result
/// is collected: a finished one at the next spawn, the rest at final
/// shutdown ([`Self::join`]). A failed attempt, one that panicked, is
/// counted, and final shutdown reports the count.
#[derive(Default)]
pub(super) struct KeyedAttempts(StdMutex<AttemptSet>);

#[derive(Default)]
struct AttemptSet {
    tasks: JoinSet<()>,
    /// Collected attempts that failed.
    failed: usize,
}

impl AttemptSet {
    /// Collects every finished attempt without waiting.
    fn collect_finished(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            if result.is_err() {
                self.failed += 1;
            }
        }
    }
}

impl KeyedAttempts {
    /// Runs `attempt` as the set's task, after collecting the finished
    /// ones, so the set holds only attempts still running and the
    /// unreported ones.
    fn spawn(&self, attempt: impl Future<Output = ()> + Send + 'static) {
        let mut set = lock(&self.0);
        set.collect_finished();
        set.tasks.spawn(attempt);
    }

    /// Final shutdown (runtime §6.2): collects the attempts until `by`, and
    /// returns how many had not ended then and how many failed, ever. One
    /// still running at `by` is counted, then aborted: it holds neither the
    /// Engine, nor so its Store, nor the report past the deadline, and its
    /// intent, if any, stays open for restart recovery.
    pub(super) async fn join(&self, by: tokio::time::Instant) -> (usize, usize) {
        let mut tasks = std::mem::take(&mut lock(&self.0).tasks);
        let mut failed = 0;
        let _ = tokio::time::timeout_at(by, async {
            while let Some(result) = tasks.join_next().await {
                if result.is_err() {
                    failed += 1;
                }
            }
        })
        .await;
        while let Some(result) = tasks.try_join_next() {
            if result.is_err() {
                failed += 1;
            }
        }
        let mut set = lock(&self.0);
        set.failed += failed;
        // Attempts spawned while the join ran are past the deadline too.
        set.collect_finished();
        let pending = tasks.len() + set.tasks.len();
        let late = std::mem::take(&mut set.tasks);
        let failed = set.failed;
        drop(set);
        // Dropping a set aborts its tasks.
        drop((tasks, late));
        (pending, failed)
    }
}

/// The keyed steers whose first attempt is in flight, by session and key:
/// a repeat waits on the attempt's watch, which closes when it ends.
#[derive(Default)]
pub(super) struct KeyedSteers(StdMutex<HashMap<(SessionId, String), watch::Receiver<()>>>);

impl KeyedSteers {
    /// The attempt in flight under `key`, if any.
    fn in_flight(&self, key: &(SessionId, String)) -> Option<watch::Receiver<()>> {
        lock(&self.0).get(key).cloned()
    }

    /// Registers the first attempt under `key`; it ends when dropped.
    fn begin(&self, key: (SessionId, String)) -> Attempt<'_> {
        let (done, waiting) = watch::channel(());
        lock(&self.0).insert(key.clone(), waiting);
        Attempt {
            steers: self,
            key,
            _done: done,
        }
    }
}

/// A keyed steer's first attempt in flight. Dropped, by any path, it leaves
/// the map, then closes its watch, so a waiting repeat looks the key up
/// again and finds the recorded outcome or none.
struct Attempt<'a> {
    steers: &'a KeyedSteers,
    key: (SessionId, String),
    _done: watch::Sender<()>,
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        lock(&self.steers.0).remove(&self.key);
    }
}

/// Test builds: a keyed steer attempt that stalls at its start, forever.
#[cfg(test)]
pub(super) const ATTEMPT_STALLS: u8 = 1;

/// Test builds: a keyed steer attempt that waits at its start for
/// `steer_attempt_release`, then panics.
#[cfg(test)]
pub(super) const ATTEMPT_PANICS: u8 = 2;

impl Engine {
    /// Test builds: the fault a keyed steer attempt meets at its start.
    #[cfg(test)]
    async fn attempt_fault(&self) {
        let fault = self.faults.steer_attempt.load(Ordering::Acquire);
        if fault == 0 {
            return;
        }
        self.faults.steer_attempt_entered.notify_one();
        if fault == ATTEMPT_STALLS {
            std::future::pending::<()>().await;
        }
        self.faults.steer_attempt_release.notified().await;
        panic!("injected keyed steer attempt failure");
    }

    /// C1 §3.4 `steer` under `op_key`, after authentication and the latch:
    /// the attempt ([`Self::keyed_attempt`]) runs as the Engine's own task
    /// ([`KeyedAttempts`]), so the caller going away, its future dropped,
    /// ends none of it (K2 r1 #1): its intent, its delivery and the lane's
    /// commit of its `steer.delivered` with its outcome, or its refusal's
    /// record, all happen whatever the caller does. An attempt that failed,
    /// or that final shutdown ended, answers `store_error`.
    pub(super) async fn keyed_steer(
        &self,
        params: SteerParams,
        frozen: Frozen,
        key: (String, via_store::Identity),
    ) -> Result<Value, ApiError> {
        // Every Engine is made in its own `Arc` ([`Engine::open`]).
        let Some(engine) = self.me.upgrade() else {
            return Err(ApiError::STORE);
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.keyed_attempts.spawn(async move {
            // The caller may be gone: the attempt's work is done either way.
            let _ = reply.send(engine.keyed_attempt(params, &frozen, key).await);
        });
        answer.await.unwrap_or(Err(ApiError::STORE))
    }

    /// A keyed steer's attempt. Under `admission`, before any route or
    /// current-state check (K2 r1 #3): a row under the key of another verb
    /// or identity is `idempotency_conflict`; a recorded outcome is
    /// replayed; an attempt still running is waited for, then the key is
    /// looked up again; an intent whose attempt ended without an outcome
    /// gets the uncertain one. Otherwise this is the first attempt: its
    /// intent row commits, then the steer runs as an unkeyed one would,
    /// the route's support first, and its outcome is recorded with its
    /// `steer.delivered` or, for a refusal, alone. A `store_error` reply
    /// records no outcome: a refused or rolled-back intent leaves no key,
    /// and an intent the lane resolved without a delivery commit is left
    /// open, so the next look-up, its attempt ended, records the uncertain
    /// outcome.
    async fn keyed_attempt(
        &self,
        params: SteerParams,
        frozen: &Frozen,
        (op_key, identity): (String, via_store::Identity),
    ) -> Result<Value, ApiError> {
        #[cfg(test)]
        self.attempt_fault().await;
        let key = (params.session.clone(), op_key);
        loop {
            let admission = self.admission.lock().await;
            // Runtime §7: no new mutation, not even a keyed replay.
            if self.store_failed() {
                return Err(ApiError::STORE);
            }
            let stored = self
                .store
                .keyed_operation(&key.0, &key.1)
                .await
                .map_err(|_| ApiError::STORE)?;
            match stored {
                Some(stored)
                    if stored.verb != OperationVerb::Steer || stored.identity != identity =>
                {
                    return Err(ApiError::IDEMPOTENCY_CONFLICT);
                }
                Some(KeyedOperation {
                    result: Some(result),
                    ..
                }) => return replay(result, frozen),
                Some(_) => {
                    if let Some(mut first) = self.keyed_steers.in_flight(&key) {
                        drop(admission);
                        #[cfg(test)]
                        self.faults.steer_key_waiting.notify_one();
                        // The attempt only ever ends, closing the watch;
                        // either way the key is looked up again.
                        let _ = first.changed().await;
                        continue;
                    }
                    let stored = self
                        .record_outcome(&key, uncertain(), Some(&admission))
                        .await?;
                    return replay(stored, frozen);
                }
                None => {
                    let attempt = self.keyed_steers.begin(key.clone());
                    let intent = SteerIntent {
                        session_id: key.0.clone(),
                        op_key: key.1.clone(),
                        identity,
                    };
                    if let Err(error) = self.store.commit_steer_intent(intent).await {
                        return Err(self.steer_write_failed(&error, Some(&admission)).await);
                    }
                    drop(admission);
                    let reply = match self.deliver_steer(params, frozen, Some(&key.1)).await {
                        // Recorded with its `steer.delivered`.
                        Ok(reply) => Ok(reply),
                        Err(error) => match stored_refusal(&error) {
                            Some(refusal) => self
                                .record_outcome(&key, refusal, None)
                                .await
                                .and_then(|stored| replay(stored, frozen)),
                            None => Err(error),
                        },
                    };
                    drop(attempt);
                    return reply;
                }
            }
        }
    }

    /// Records `outcome` under `key`, on its open intent, and returns it.
    async fn record_outcome(
        &self,
        (session, op_key): &(SessionId, String),
        outcome: Value,
        admission: Option<&Admission<'_>>,
    ) -> Result<Value, ApiError> {
        let record = SteerOutcome {
            op_key: op_key.clone(),
            result: outcome.clone(),
        };
        match self.store.commit_steer_outcome(session, record).await {
            Ok(()) => Ok(outcome),
            Err(error) => Err(self.steer_write_failed(&error, admission).await),
        }
    }

    /// A keyed steer's intent or outcome write that failed: the request's
    /// Store failure (design §7.2 row 1), `store_error`. Refused before
    /// `BEGIN` at `wal.max`, nothing was written and it is no failure.
    async fn steer_write_failed(
        &self,
        error: &StoreError,
        admission: Option<&Admission<'_>>,
    ) -> ApiError {
        if matches!(error, StoreError::WalFull) {
            return ApiError::WAL_FULL;
        }
        self.store_failure(
            FailureSite::Receipt,
            WriteOutcome::of(error),
            FailureScope::Request,
        )
        .finish_with(admission)
        .await;
        ApiError::STORE
    }
}

#[cfg(test)]
mod unit {
    use super::{replay, stored_refusal, uncertain};
    use crate::ApiError;

    /// Every refusal a steer stores replays as itself.
    #[test]
    fn stored_refusals_replay_as_themselves() {
        let frozen = crate::intake::Frozen::of(&via_store::SessionRoute::default());
        for error in [
            ApiError::NO_ACTIVE_TURN,
            ApiError::TURN_MISMATCH,
            ApiError::CONTROL_LANE_FULL,
            ApiError::steer_failed("not_steerable", "none".into()),
            ApiError::steer_failed("not_delivered", "uncertain".into()),
            ApiError::steer_failed("not_recorded", "merged".into()),
        ] {
            let stored = stored_refusal(&error).expect("a steer refusal is stored");
            let replayed = replay(stored, &frozen).expect_err("a refusal replays as one");
            assert_eq!(
                (
                    replayed.code,
                    replayed.kind,
                    replayed.reason,
                    replayed.data()
                ),
                (error.code, error.kind, error.reason, error.data())
            );
        }
        assert!(stored_refusal(&ApiError::STORE).is_none());
        let uncertain = replay(uncertain(), &frozen).expect_err("uncertain is a refusal");
        assert_eq!(uncertain.data()["delivery"], "uncertain");
        assert_eq!(uncertain.data()["reason"], "not_delivered");
        assert_eq!(uncertain.message, ApiError::steer_unrecorded().message);
    }
}
