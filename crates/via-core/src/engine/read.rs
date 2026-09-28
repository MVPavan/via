//! Reads: address resolution, `result`, `wait`, `events` and `logs`.

use std::{sync::atomic::Ordering, time::Duration};

use serde_json::{Value, json};

use super::{Engine, journal};
use crate::api::DEFAULT_WAIT_MS;
use crate::{ApiError, SessionId, TurnNumber, WaitParams, parse_address};

impl Engine {
    /// Resolves a C1 address; a bare session names its latest turn.
    async fn address(&self, address: &str) -> Result<(SessionId, TurnNumber), ApiError> {
        match parse_address(address)? {
            (session, Some(turn)) => Ok((session, turn)),
            (session, None) => {
                let turns = self.turns(&session).await?;
                Ok((
                    session,
                    TurnNumber::try_from(turns).map_err(|_| ApiError::TURN_NOT_FOUND)?,
                ))
            }
        }
    }

    /// The session's highest turn number; `session_not_found` without one.
    async fn turns(&self, session: &SessionId) -> Result<u32, ApiError> {
        Ok(self
            .store
            .session_snapshot(session)
            .await
            .map_err(|_| ApiError::STORE)?
            .ok_or(ApiError::SESSION_NOT_FOUND)?
            .turns)
    }

    /// Refuses a turn the session never had.
    async fn exists(&self, session: &SessionId, turn: TurnNumber) -> Result<(), ApiError> {
        if turn.get() > self.turns(session).await? {
            return Err(ApiError::TURN_NOT_FOUND);
        }
        Ok(())
    }

    /// Reads a committed terminal result without waiting.
    pub async fn result(&self, address: &str) -> Result<Value, ApiError> {
        let (session, turn) = self.address(address).await?;
        if let Some(result) =
            journal::read_result(&self.store, &self.unresolved, &session, turn).await?
        {
            return Ok(result);
        }
        self.exists(&session, turn).await?;
        Err(ApiError::TURN_NOT_FINISHED)
    }

    /// Waits for a durable terminal result independently of client lifetime,
    /// at most `timeout_ms` (C1 §3.8), then `wait_timeout`.
    ///
    /// Once final shutdown committed its last record, a result still missing
    /// can never commit in this daemon: the wait ends `daemon_stopping`.
    pub async fn wait(&self, params: WaitParams) -> Result<Value, ApiError> {
        let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(DEFAULT_WAIT_MS));
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or(ApiError::INVALID_PARAMS)?;
        let (session, turn) = self.address(&params.address).await?;
        let mut checked = false;
        let mut registered = false;
        loop {
            // Read before the Store: a result committed before finalization is seen.
            let finalized = self.finalized.load(Ordering::Acquire);
            if let Some(result) =
                journal::read_result(&self.store, &self.unresolved, &session, turn).await?
            {
                return Ok(result);
            }
            if !checked {
                self.exists(&session, turn).await?;
                checked = true;
            }
            if finalized {
                return Err(ApiError::DAEMON_STOPPING);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(ApiError::WAIT_TIMEOUT);
            }
            if !registered {
                registered = true;
                // The first read found no terminal; the waiter is registered.
                #[cfg(feature = "test-failpoints")]
                let _ = via_store::failpoint::hit_async("core.wait.registered").await;
            }
            tokio::time::sleep_until(deadline.min(now + Duration::from_millis(20))).await;
        }
    }

    /// Waits, unbounded, for the turn's durable terminal as `wait` reads it
    /// (design §3.3 [r3.4]): a turn's own deadlines bound it. Once final
    /// shutdown finalized, a turn recorded unpersisted is `store_error` and
    /// any other is `daemon_stopping`.
    pub(super) async fn await_terminal(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Value, ApiError> {
        loop {
            let finalized = self.finalized.load(Ordering::Acquire);
            if let Some(result) =
                journal::read_result(&self.store, &self.unresolved, session, turn).await?
            {
                return Ok(result);
            }
            if finalized {
                return Err(ApiError::DAEMON_STOPPING);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Reads the first bounded page of durable canonical events.
    pub async fn events(&self, session: &str) -> Result<Value, ApiError> {
        let id = SessionId::try_from(session).map_err(|_| ApiError::INVALID_PARAMS)?;
        let events = self
            .store
            .events(&id, 1, 1000)
            .await
            .map_err(|_| ApiError::STORE)?;
        let next_after = events.last().map_or(0, |event| event.seq);
        Ok(
            json!({"events":events.into_iter().map(|event| event.event).collect::<Vec<_>>(),"next_after":next_after,"more":false}),
        )
    }

    /// Reads bounded raw excerpts referenced by committed events.
    pub async fn logs(&self, session: &str) -> Result<Value, ApiError> {
        let id = SessionId::try_from(session).map_err(|_| ApiError::INVALID_PARAMS)?;
        self.store.logs(&id).await.map_err(|_| ApiError::STORE)
    }
}
