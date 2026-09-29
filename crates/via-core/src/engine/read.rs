//! Reads: address resolution, `result`, `wait`, `events`, `logs` and
//! `status`.

use std::{sync::atomic::Ordering, time::Duration};

use serde_json::{Value, json};
use via_store::{StoreClient, TerminalFacts};

use super::{Engine, journal};
use crate::api::{DEFAULT_WAIT_MS, FAKE_TOKEN_SCOPE, rfc3339};
use crate::{ApiError, LogsParams, SessionId, StatusParams, TurnNumber, WaitParams, parse_address};

/// `status` reply bound (Task 4 design §4.2): met by construction, checked
/// by a debug assertion.
const STATUS_MAX: usize = 1024 * 1024;

/// `status` step page size without `limit` (C1 §3.7).
const STATUS_DEFAULT_LIMIT: u32 = 100;

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
    /// A C1 read: on the Public lane, whose full lane is refused.
    async fn turns(&self, session: &SessionId) -> Result<u32, ApiError> {
        let snapshot = self
            .store
            .public()
            .session_snapshot(session)
            .await
            .map_err(|error| ApiError::read(&error))?;
        Ok(snapshot.ok_or(ApiError::SESSION_NOT_FOUND)?.turns)
    }

    /// The turn's durable result as `journal::read_result` reads it, on
    /// `store`'s lane; Store's read reply latches on SQLite corruption
    /// (design §7.1). No lock is held.
    async fn read_result(
        &self,
        store: &StoreClient,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<Value>, ApiError> {
        let read = store.result(session, turn).await;
        journal::settled_result(&self.unresolved, session, turn, read)
    }

    /// The turn's committed terminal facts (design §6.7), settled as
    /// [`Self::read_result`] settles a result; no envelope is parsed.
    pub(super) async fn read_facts(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<TerminalFacts>, ApiError> {
        let read = self.store.terminal_facts(session, turn).await;
        journal::settled_result(&self.unresolved, session, turn, read)
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
        if let Some(result) = self
            .read_result(&self.store.public(), &session, turn)
            .await?
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
        let public = self.store.public();
        let mut checked = false;
        let mut registered = false;
        loop {
            // Read before the Store: a result committed before finalization is seen.
            let finalized = self.finalized.load(Ordering::Acquire);
            if let Some(result) = self.read_result(&public, &session, turn).await? {
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
            if let Some(result) = self.read_result(&self.store, session, turn).await? {
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
            .public()
            .events(&id, 1, 1000)
            .await
            .map_err(|error| ApiError::read(&error))?;
        let next_after = events.last().map_or(0, |event| event.seq);
        Ok(
            json!({"events":events.into_iter().map(|event| event.event).collect::<Vec<_>>(),"next_after":next_after,"more":false}),
        )
    }

    /// C1 §3.12 `logs` (Task 4 design §4.4): where the addressed turn's
    /// evidence is, or for a session its running turn's, else its latest
    /// submitted one's. Each fixed file name is `stat`ed once on the
    /// blocking pool, without following a symlink; no file is opened.
    pub async fn logs(&self, params: LogsParams) -> Result<Value, ApiError> {
        let (session, turn) = params.address()?;
        let refs = self
            .store
            .public()
            .evidence_refs(&session, turn)
            .await
            .map_err(|error| ApiError::read(&error))?
            .ok_or(ApiError::SESSION_NOT_FOUND)?;
        let number = refs.turn.ok_or(ApiError::TURN_NOT_FOUND)?;
        let folder = refs
            .evidence_dir
            .map(|dir| self.store.evidence().absolute(&dir));
        // A failed `lstat` is a failed evidence read: `store_error`, as for
        // the Store read above.
        let files = match folder.clone() {
            Some(folder) => tokio::task::spawn_blocking(move || evidence_files(&folder))
                .await
                .map_err(|_| ApiError::STORE)?
                .map_err(|_| ApiError::STORE)?,
            None => Vec::new(),
        };
        Ok(json!({
            "session_id": session,
            "turn": number.get(),
            "vendor_session_id": refs.vendor_session_id,
            "transcript": refs.transcript_hint,
            "folder": folder.map(|folder| folder.display().to_string()),
            "files": files,
        }))
    }
}

impl Engine {
    /// C1 §3.7 `status` (Task 4 design §4.2, §11.3): one Public Store read
    /// selects the turn and returns the durable members and a page of its
    /// step rows; then, from memory, the published progress when the
    /// selected turn is not terminal in that read and is the running one,
    /// and `process.alive`.
    pub async fn status(&self, params: StatusParams) -> Result<Value, ApiError> {
        let limit = params.limit.unwrap_or(STATUS_DEFAULT_LIMIT);
        if limit == 0 || limit > via_store::STATUS_STEPS {
            return Err(ApiError::INVALID_PARAMS);
        }
        let after_step = params.after_step.unwrap_or(0);
        let status = self
            .store
            .public()
            .session_status(&params.session, params.turn, after_step, limit)
            .await
            .map_err(|error| ApiError::read(&error))?
            .ok_or(ApiError::SESSION_NOT_FOUND)?;
        if params.turn.is_some() && status.selected.is_none() {
            return Err(ApiError::TURN_NOT_FOUND);
        }
        let progress = status
            .selected
            .as_ref()
            .filter(|(_, state)| !terminal(state))
            .and_then(|(turn, _)| self.slot(&params.session)?.progress(*turn))
            .map(|progress| progress.to_value(FAKE_TOKEN_SCOPE));
        let alive = self.adapter.live_armed(&status.unproven_anchors);
        let value = status_value(
            &params.session,
            status,
            after_step,
            progress.as_ref(),
            alive,
        );
        debug_assert!(
            serde_json::to_vec(&value).is_ok_and(|bytes| bytes.len() <= STATUS_MAX),
            "status exceeds STATUS_MAX"
        );
        Ok(value)
    }
}

/// Whether a turn state is terminal.
fn terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "cancelled" | "unknown")
}

/// Unix milliseconds as RFC 3339.
fn unix_ms(ms: i64) -> String {
    let since = std::time::Duration::from_millis(u64::try_from(ms).unwrap_or(0));
    rfc3339(std::time::UNIX_EPOCH + since)
}

/// The C1 §3.7 `status` object from one Store read and the memory part.
fn status_value(
    session: &SessionId,
    status: via_store::SessionStatus,
    after_step: u32,
    progress: Option<&Value>,
    alive: bool,
) -> Value {
    let active_turn = status.active.map(|active| {
        json!({
            "n": active.turn,
            "state": "running",
            "phase": if active.accepted { "accepted" } else { "submitting" },
            "started_at": active.submitted_at,
            "last_event_seq": active.last_event_seq,
            "cancel": active
                .cancel_requested_at
                .map(|at| json!({"requested_at": at})),
        })
    });
    let queue: Vec<Value> = status
        .queue
        .into_iter()
        .map(|queued| {
            json!({"n": queued.turn, "op_key": queued.op_key, "queued_at": queued.queued_at,
                "effective": queued.effective})
        })
        .collect();
    let turns: Vec<Value> = status
        .turns
        .iter()
        .map(|(turn, state)| {
            if terminal(state) {
                json!({"n": turn, "state": state, "revision": 0})
            } else {
                json!({"n": turn, "state": state})
            }
        })
        .collect();
    let steps = status.selected.as_ref().map(|(turn, _)| {
        let items: Vec<Value> = status
            .steps
            .iter()
            .map(|row| {
                json!({"step": row.step, "started_at": unix_ms(row.started_ms),
                    "ended_at": unix_ms(row.ended_ms), "tokens": row.tokens})
            })
            .collect();
        let next_after = status.steps.last().map_or(after_step, |row| row.step);
        json!({"turn": turn, "items": items, "next_after": next_after, "more": status.more})
    });
    json!({
        "session_id": session,
        "state": status.state,
        "admission": status.admission,
        "harness": status.harness,
        "model": status.model,
        "route": status.route,
        "vendor_session_id": status.vendor_session_id,
        "vendor_identity_verified": false,
        "cwd": status.cwd,
        "process": {
            "alive": alive,
            "cleanup": if status.cleanup_uncertain { "uncertain" } else { "quiescent" },
            "idle_since": null,
        },
        "active_turn": active_turn,
        "progress": progress,
        "steps": steps,
        "queue": queue,
        "turns": turns,
        "label": status.label,
        "created_at": unix_ms(status.created_ms),
        "updated_at": unix_ms(status.updated_ms),
    })
}

/// The fixed evidence files present in `folder`, as `{name, bytes}`: one
/// `lstat` each, and only regular files count (design §4.4, §7.1). A
/// missing name is absent; any other `lstat` error fails the read, so an
/// existing file never drops out of the list (C1 §3.12).
fn evidence_files(folder: &std::path::Path) -> std::io::Result<Vec<Value>> {
    let mut files = Vec::new();
    for name in via_store::EVIDENCE_FILES {
        let metadata = match std::fs::symlink_metadata(folder.join(name)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_file() {
            files.push(json!({"name": name, "bytes": metadata.len()}));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::evidence_files;

    /// C1 §3.12: `files` lists the files that exist. A missing file is
    /// absent; a file that cannot be stated fails the read, never drops out.
    #[test]
    #[expect(
        clippy::print_stderr,
        reason = "a skipped check under root is reported"
    )]
    fn evidence_files_skips_only_missing_files() -> std::io::Result<()> {
        let root = tempfile::TempDir::new()?;
        let folder = root.path().join("1");
        std::fs::create_dir(&folder)?;
        std::fs::write(folder.join("stderr.log"), b"abc")?;
        let listed = evidence_files(&folder)?;
        assert_eq!(listed, [serde_json::json!({"name":"stderr.log","bytes":3})]);
        // No search permission: each `lstat` fails with EACCES.
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o600))?;
        let bypassed = std::fs::symlink_metadata(folder.join("stderr.log")).is_ok();
        let unstated = evidence_files(&folder);
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))?;
        if bypassed {
            eprintln!("skipped: this process bypasses file permissions (root)");
        } else {
            assert!(unstated.is_err(), "an unstated file was listed as absent");
        }
        Ok(())
    }
}
