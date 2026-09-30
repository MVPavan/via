//! Reads: address resolution, `result`, `wait`, `events`, `logs` and
//! `status`.

use std::{sync::atomic::Ordering, time::Duration};

use serde_json::value::RawValue;
use serde_json::{Value, json};
use via_store::{EventsRead, PAGE_MAX, StoreClient, TerminalFacts};

use super::{Engine, journal};
use crate::api::{DEFAULT_WAIT_MS, FAKE_TOKEN_SCOPE, rfc3339};
use crate::{
    ApiError, EventsParams, ListParams, LogsParams, SessionId, StatusParams, TurnNumber,
    WaitParams, parse_address,
};

/// `status` reply bound (Task 4 design §4.2): met by construction, checked
/// by a debug assertion.
const STATUS_MAX: usize = 1024 * 1024;

/// How often `wait` checks a turn's terminal facts (design §4.1).
const WAIT_CHECK: Duration = Duration::from_secs(1);

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

    /// The turn's durable envelope as stored (design §6.7 `result_text`),
    /// on the Public lane; a turn whose terminal could not be made durable
    /// is `store_error` (`journal::settled_result`). Store's read reply
    /// latches on SQLite corruption (design §7.1). No lock is held, and no
    /// value is built from the envelope.
    async fn read_result(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<Box<RawValue>>, ApiError> {
        let read = self.store.public().result_text(session, turn).await;
        journal::settled_result(&self.unresolved, session, turn, read)
    }

    /// The turn's committed terminal facts (design §6.7), on `store`'s
    /// lane, settled as [`Self::read_result`] settles a result; no envelope
    /// is parsed.
    async fn read_facts_on(
        &self,
        store: &StoreClient,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<TerminalFacts>, ApiError> {
        let read = store.terminal_facts(session, turn).await;
        journal::settled_result(&self.unresolved, session, turn, read)
    }

    /// [`Self::read_facts_on`] on Core's own lane.
    pub(super) async fn read_facts(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<TerminalFacts>, ApiError> {
        self.read_facts_on(&self.store, session, turn).await
    }

    /// Refuses a turn the session never had.
    async fn exists(&self, session: &SessionId, turn: TurnNumber) -> Result<(), ApiError> {
        if turn.get() > self.turns(session).await? {
            return Err(ApiError::TURN_NOT_FOUND);
        }
        Ok(())
    }

    /// Reads a committed terminal envelope without waiting: one
    /// `result_text` read, written to the caller as stored (design §4.1).
    pub async fn result(&self, address: &str) -> Result<Box<RawValue>, ApiError> {
        let (session, turn) = self.address(address).await?;
        if let Some(result) = self.read_result(&session, turn).await? {
            return Ok(result);
        }
        self.exists(&session, turn).await?;
        Err(ApiError::TURN_NOT_FINISHED)
    }

    /// Waits for a durable terminal result independently of client lifetime,
    /// at most `timeout_ms` (C1 §3.8), then `wait_timeout`.
    ///
    /// Design §4.1 [t4r16.7.7]: it checks the turn's terminal facts on the
    /// Public lane at once and then once per second, and reads the envelope
    /// with `result_text` only once the turn is terminal: 32 waiters make
    /// 32 reads per second, and a turn's end is seen at most 1 s late.
    /// Once final shutdown committed its last record, a result still
    /// missing can never commit in this daemon: the wait ends
    /// `daemon_stopping`.
    pub async fn wait(&self, params: WaitParams) -> Result<Box<RawValue>, ApiError> {
        let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(DEFAULT_WAIT_MS));
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or(ApiError::INVALID_PARAMS)?;
        let (session, turn) = self.address(&params.address).await?;
        let public = self.store.public();
        let mut checked = false;
        let mut registered = false;
        let mut check_at = tokio::time::Instant::now();
        loop {
            // Read before the Store: a result committed before finalization is seen.
            let finalized = self.finalized.load(Ordering::Acquire);
            if self.read_facts_on(&public, &session, turn).await?.is_some()
                && let Some(result) = self.read_result(&session, turn).await?
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
            if tokio::time::Instant::now() >= deadline {
                return Err(ApiError::WAIT_TIMEOUT);
            }
            if !registered {
                registered = true;
                // The first read found no terminal; the waiter is registered.
                #[cfg(feature = "test-failpoints")]
                let _ = via_store::failpoint::hit_async("core.wait.registered").await;
            }
            check_at += WAIT_CHECK;
            tokio::time::sleep_until(deadline.min(check_at)).await;
        }
    }

    /// Waits, unbounded, for the turn's durable terminal facts (design
    /// §3.3 [r3.4], §6.7): a turn's own deadlines bound it; no envelope is
    /// read. Once final shutdown finalized, a turn recorded unpersisted is
    /// `store_error` and any other is `daemon_stopping`.
    pub(super) async fn await_terminal(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<TerminalFacts, ApiError> {
        loop {
            let finalized = self.finalized.load(Ordering::Acquire);
            if let Some(facts) = self.read_facts(session, turn).await? {
                return Ok(facts);
            }
            if finalized {
                return Err(ApiError::DAEMON_STOPPING);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// C1 §3.11 `events` (Task 4 design §4.3): one Public Store read of the
    /// window after `after`, filtered by `turn` and `types`; the events
    /// array Store wrote is placed in the reply as it is, never parsed.
    /// `earliest_seq` is 1: nothing is pruned.
    pub async fn events(&self, params: EventsParams) -> Result<Box<RawValue>, ApiError> {
        let read = self
            .store
            .public()
            .events_page(params.query()?)
            .await
            .map_err(|error| ApiError::read(&error))?;
        let page = match read {
            EventsRead::Page(page) => page,
            EventsRead::SessionNotFound => return Err(ApiError::SESSION_NOT_FOUND),
            EventsRead::TurnNotFound => return Err(ApiError::TURN_NOT_FOUND),
        };
        let reply = format!(
            r#"{{"events":{},"next_after":{},"more":{},"earliest_seq":1}}"#,
            page.events, page.next_after, page.more
        );
        debug_assert!(reply.len() <= PAGE_MAX, "an events page exceeds PAGE_MAX");
        RawValue::from_string(reply).map_err(|_| ApiError::STORE)
    }

    /// C1 §3.10 `list` (Task 4 design §4.5, §6.8): one Public Store read of
    /// at most 1000 sessions below the cursor, newest first; the page's
    /// bound holds by construction, checked by a debug assertion.
    pub async fn list(&self, params: ListParams) -> Result<Value, ApiError> {
        let page = self
            .store
            .public()
            .list_page(params.query()?)
            .await
            .map_err(|error| ApiError::read(&error))?;
        let sessions: Vec<Value> = page
            .sessions
            .into_iter()
            .map(|summary| {
                json!({
                    "session_id": summary.session_id,
                    "state": summary.state,
                    "admission": summary.admission,
                    "harness": summary.harness,
                    "model": summary.model,
                    "label": summary.label,
                    "created_at": unix_ms(summary.created_ms),
                    "last_active_at": unix_ms(summary.last_active_ms),
                })
            })
            .collect();
        let value = json!({
            "sessions": sessions,
            "next_cursor": page.next.map(|ord| format!("l3.{ord}")),
        });
        debug_assert!(
            serde_json::to_vec(&value).is_ok_and(|bytes| bytes.len() <= PAGE_MAX),
            "a list page exceeds PAGE_MAX"
        );
        Ok(value)
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
