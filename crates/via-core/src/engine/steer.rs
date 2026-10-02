//! Keyed `steer` (C1 §3, §3.4; via-jm4.36): the key's answer is its
//! durable intent's stored outcome. The intent row is committed before the
//! input goes to the driver; the outcome is recorded once, with the
//! `steer.delivered` event that reports the delivery, or alone for a
//! refusal, and every repeat replays it. The steer has one owner at a time
//! (K2 r3): the request until it hands the steer to the session's lane,
//! then the lane, whose ticket book keeps the key with the input's token
//! until no delivery can resolve it: the lane consumed the steer's report,
//! the request took it back on the driver's refusal, its turn settled with
//! the request gone (K2 r4), or the lane ended. A repeat while an owner
//! remains waits for it. An intent whose owner ended without recording an
//! outcome gets the daemon's uncertain outcome, from the next repeat or
//! restart recovery; the input is never sent again.

use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
};

use serde_json::{Value, json};
use tokio::sync::watch;
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

/// The keyed steers that have an owner, by session and key: a repeat waits
/// on the owner's watch, which closes when the owner ends ([`Owner`]).
#[derive(Default)]
pub(super) struct KeyedSteers(Arc<StdMutex<OwnerMap>>);

type OwnerMap = HashMap<(SessionId, String), watch::Receiver<()>>;

impl KeyedSteers {
    /// The owner of the steer under `key`, if any.
    pub(super) fn in_flight(&self, key: &(SessionId, String)) -> Option<watch::Receiver<()>> {
        lock(&self.0).get(key).cloned()
    }

    /// Registers the first attempt under `key`; its owner ends when dropped.
    pub(super) fn begin(&self, key: (SessionId, String)) -> Owner {
        let (done, waiting) = watch::channel(());
        lock(&self.0).insert(key.clone(), waiting);
        Owner {
            steers: Arc::clone(&self.0),
            key,
            _done: done,
        }
    }
}

/// The ownership of a keyed steer whose outcome is not recorded yet: the
/// request holds it until it hands the steer to its lane, whose ticket
/// book then holds it until the lane resolves the steer ([`super::lane`]).
/// Dropped, by any path, it leaves the map, then closes its watch, so a
/// waiting repeat looks the key up again and finds the recorded outcome or
/// an intent with no owner.
pub(super) struct Owner {
    steers: Arc<StdMutex<OwnerMap>>,
    key: (SessionId, String),
    _done: watch::Sender<()>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        lock(&self.steers).remove(&self.key);
    }
}

impl Engine {
    /// C1 §3.4 `steer` under `op_key`, after authentication, the latch and
    /// the params' validation, in the request's own future. Under
    /// `admission`, before the route's support or any current-turn check
    /// (K2 r1 #3): a row under the key of another verb or identity is
    /// `idempotency_conflict`; a recorded outcome is replayed; a steer that
    /// still has an owner is waited for, then the key is looked up again;
    /// an intent whose owner ended without an outcome gets the uncertain
    /// one. Otherwise this is the first attempt: its intent row commits
    /// and the request owns the steer, which then runs as an unkeyed one
    /// would, the route's support first. Handed to the lane with its input
    /// ([`Self::deliver_steer`]), the steer is the lane's, which records
    /// its outcome with its `steer.delivered` whatever becomes of the
    /// request; a refusal is recorded alone, by the request, which owns
    /// the steer again then. A `store_error` reply records no outcome: a
    /// refused or rolled-back intent leaves no key, and an intent whose
    /// owner ended is left for the uncertain outcome.
    pub(super) async fn keyed_steer(
        &self,
        params: SteerParams,
        frozen: &Frozen,
        (op_key, identity): (String, via_store::Identity),
    ) -> Result<Value, ApiError> {
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
                        // The owner only ever ends, closing the watch;
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
                    let mut owner = Some(self.keyed_steers.begin(key.clone()));
                    let intent = SteerIntent {
                        session_id: key.0.clone(),
                        op_key: key.1.clone(),
                        identity,
                    };
                    if let Err(error) = self.store.commit_steer_intent(intent).await {
                        return Err(self.steer_write_failed(&error, Some(&admission)).await);
                    }
                    drop(admission);
                    let handed = Some((key.1.as_str(), &mut owner));
                    let reply = match self.deliver_steer(params, frozen, handed).await {
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
                    drop(owner);
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
