//! Reads: address resolution, `result`, `wait`, `events`, `logs` and
//! `status`.

use std::{sync::Arc, time::Duration};

use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::sync::watch;
use via_store::{EventsPage, EventsQuery, EventsRead, PAGE_MAX, StoreClient, TerminalFacts};

use super::{Engine, journal};
use crate::api::{DEFAULT_WAIT_MS, PlanFields, Warning, rfc3339};
use crate::intake::{self, Frozen};
use crate::{
    ApiError, EventsParams, ListParams, LogsParams, SessionId, StatusParams, TurnNumber,
    WaitParams, parse_address,
};

/// `status` reply bound (Task 4 design §4.2): met by construction, checked
/// by a debug assertion.
const STATUS_MAX: usize = 1024 * 1024;

/// The safety recheck (owner, 2026-10-04): a pending `wait`, `events`
/// long-poll or terminal await with no wake re-reads after this long, until
/// its deadline, so a missed wake only delays it and never hangs it.
const RECHECK: Duration = Duration::from_secs(5);

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
    /// Design §4.1: it checks the turn's terminal facts on the Public lane
    /// at once and again after each change that may settle it ([`Wakes`]),
    /// or after [`RECHECK`] without one, and reads the envelope with
    /// `result_text` only once the turn is terminal. A turn's end is seen as
    /// soon as it commits. Outstanding reads are bounded: at most 32 sockets
    /// make at most 32 waiters, each with at most one read in flight, since
    /// the signal coalesces the changes a waiter has not yet consumed. How
    /// often a waiter reads follows the commit rate (it re-reads after each
    /// change it consumes), and every read queues on the one writer behind
    /// the commits. Once final shutdown committed its last record,
    /// a result still missing can never commit in this daemon: the wait
    /// ends `daemon_stopping`. The deadline bounds its Store reads too
    /// ([`by_deadline`]): a turn already terminal when the first check
    /// completes within it is returned.
    pub async fn wait(&self, params: WaitParams) -> Result<Box<RawValue>, ApiError> {
        let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(DEFAULT_WAIT_MS));
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or(ApiError::INVALID_PARAMS)?;
        let (session, turn) = by_deadline(deadline, self.address(&params.address)).await?;
        let public = self.store.public();
        // Subscribed before the first read: a change after any read wakes
        // the next await.
        let mut wakes = self.wakes();
        let mut checked = false;
        let mut registered = false;
        loop {
            // Read before the Store: a result committed before finalization is seen.
            let finalized = *self.finalized.borrow();
            let facts = self.read_facts_on(&public, &session, turn);
            if by_deadline(deadline, facts).await?.is_some()
                && let Some(result) =
                    by_deadline(deadline, self.read_result(&session, turn)).await?
            {
                return Ok(result);
            }
            if !checked {
                by_deadline(deadline, self.exists(&session, turn)).await?;
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
            // At the deadline the loop's next read or check ends the wait
            // `wait_timeout`.
            wakes.next(Some(deadline)).await;
        }
    }

    /// Waits, unbounded, for the turn's durable terminal facts (design
    /// §3.3 [r3.4], §6.7): a turn's own deadlines bound it; no envelope is
    /// read. It re-reads on each [`Wakes`] change, or after [`RECHECK`]
    /// without one. Once final shutdown finalized, a turn recorded
    /// unpersisted is `store_error` and any other is `daemon_stopping`.
    pub(super) async fn await_terminal(
        &self,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<TerminalFacts, ApiError> {
        let mut wakes = self.wakes();
        loop {
            let finalized = *self.finalized.borrow();
            if let Some(facts) = self.read_facts(session, turn).await? {
                return Ok(facts);
            }
            if finalized {
                return Err(ApiError::DAEMON_STOPPING);
            }
            wakes.next(None).await;
        }
    }

    /// What may settle a pending read, subscribed now: subscribe before
    /// the read.
    fn wakes(&self) -> Wakes {
        Wakes {
            commits: Some(self.store.subscribe_commits()),
            failures: self.unresolved.subscribe(),
            finalized: self.finalized.subscribe(),
        }
    }

    /// C1 §3.11 `events` (Task 4 design §4.3): one Public Store read of the
    /// window after `after`, filtered by `turn` and `types`; the events
    /// array Store wrote is placed in the reply as it is, never parsed.
    /// `earliest_seq` is 1: nothing is pruned.
    ///
    /// With `wait_ms` (design §4.3), a page with no events is read again
    /// from its `next_after`, at once while `more`, else after the next
    /// [`Wakes`] change or [`RECHECK`], until a page has events or the bound
    /// passes; then the last empty page is the reply. The bound cuts a read
    /// still pending too; if none completed, the reply is empty at `after`
    /// with `more: true` (read again). Final shutdown ends a long-poll that
    /// found nothing `daemon_stopping`.
    pub async fn events(&self, params: EventsParams) -> Result<Box<RawValue>, ApiError> {
        let wait = params.wait()?;
        let mut query = params.query()?;
        let Some(wait) = wait else {
            return page_reply(&self.events_read(&query).await?);
        };
        let deadline = tokio::time::Instant::now()
            .checked_add(wait)
            .ok_or(ApiError::INVALID_PARAMS)?;
        let mut wakes = self.wakes();
        let mut last = EventsPage {
            events: "[]".to_owned(),
            next_after: query.after,
            more: true,
        };
        let mut registered = false;
        loop {
            let finalized = *self.finalized.borrow();
            // Dropping a read at the bound only drops its reply receiver;
            // Store keeps ownership of a read it admitted.
            let Ok(read) = tokio::time::timeout_at(deadline, self.events_read(&query)).await else {
                return page_reply(&last);
            };
            let page = read?;
            if page.events != "[]" || tokio::time::Instant::now() >= deadline {
                return page_reply(&page);
            }
            query.after = page.next_after;
            let more = page.more;
            last = page;
            if more {
                continue;
            }
            if finalized {
                return Err(ApiError::DAEMON_STOPPING);
            }
            if !registered {
                registered = true;
                // The first read found nothing; the long-poll is registered.
                #[cfg(feature = "test-failpoints")]
                let _ = via_store::failpoint::hit_async("core.events.registered").await;
            }
            wakes.next(Some(deadline)).await;
            if tokio::time::Instant::now() >= deadline {
                return page_reply(&last);
            }
        }
    }

    /// One `events` page read on the Public lane.
    async fn events_read(&self, query: &EventsQuery) -> Result<EventsPage, ApiError> {
        let read = self
            .store
            .public()
            .events_page(query.clone())
            .await
            .map_err(|error| ApiError::read(&error))?;
        match read {
            EventsRead::Page(page) => Ok(page),
            EventsRead::SessionNotFound => Err(ApiError::SESSION_NOT_FOUND),
            EventsRead::TurnNotFound => Err(ApiError::TURN_NOT_FOUND),
        }
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
    /// submitted one's. Each fixed file name, and the structured-output
    /// file its committed envelope names (fix r4 #2, Sol r5 #1), is
    /// `stat`ed once, in one owned blob step answered within 2 s
    /// (coding-style §5), without following a symlink; no file is opened
    /// and the folder is never listed.
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
        let folder = refs.evidence_dir.map(|dir| self.store.evidence_path(&dir));
        let named = refs
            .structured_output_file
            .as_deref()
            .and_then(structured_output_file);
        // A failed `lstat` is a failed evidence read: `store_error`, as for
        // the Store read above; so is a diagnostic permit not available.
        let files = match folder.clone() {
            Some(folder) => {
                let permit = Arc::clone(&self.diagnostics)
                    .try_acquire_owned()
                    .map_err(|_| ApiError::STORE)?;
                self.store
                    .blocking_step(move || {
                        // Released when the checks end, even after 2 s.
                        let _permit = permit;
                        evidence_files(&folder, named.as_deref())
                    })
                    .await
                    .map_err(|_| ApiError::STORE)?
            }
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
    /// and `process.alive`. The session's frozen plan is read with it
    /// (decision F12): its route's token scope labels `progress.tokens`,
    /// and its recorded adapter version, inheritance states and warnings
    /// are reported (C1 §3.7), with the described turn's vendor version
    /// from its envelope once it is terminal.
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
        let frozen = Frozen::of(&status.frozen);
        let progress = status
            .selected
            .as_ref()
            .filter(|(_, state)| !terminal(state))
            .and_then(|(turn, _)| self.slot(&params.session)?.progress(*turn))
            .map(|progress| progress.to_value(frozen.token_scope()));
        // x.3.2 X4 D7: the running turn's acknowledgement, kept in memory.
        let acknowledged = status.active.as_ref().is_some_and(|active| {
            TurnNumber::try_from(active.turn).is_ok_and(|turn| {
                self.slot(&params.session).is_some_and(|slot| {
                    matches!(
                        slot.cancel_shown(turn),
                        Some(super::queue::Ack::Acknowledged(_))
                    )
                })
            })
        });
        let alive = self.adapter.live_armed(&status.unproven_anchors);
        // Decision H3: verified once this daemon committed the open of the
        // lane's current connection generation.
        let verified = self
            .kept_lane(&params.session)
            .is_some_and(|lane| lane.verified());
        let value = status_value(
            &params.session,
            status,
            after_step,
            (progress.as_ref(), &frozen),
            (alive, verified),
            acknowledged,
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
    (progress, frozen): (Option<&Value>, &Frozen),
    (alive, verified): (bool, bool),
    acknowledged: bool,
) -> Value {
    let tested = status.version_status.as_deref() == Some("tested");
    let version = PlanFields {
        route: frozen.route.clone(),
        adapter_version: frozen.adapter_version.clone(),
        vendor_version: status.vendor_version.clone(),
        version_status: if tested {
            via_adapters::VersionStatus::Tested
        } else {
            via_adapters::VersionStatus::Untested
        },
    };
    let mut warnings: Vec<Warning> = version.warning().into_iter().collect();
    warnings.extend(frozen.config_warning());
    let active_turn = status.active.map(|active| {
        json!({
            "n": active.turn,
            "state": "running",
            "phase": if active.accepted { "accepted" } else { "submitting" },
            "started_at": active.submitted_at,
            "last_event_seq": active.last_event_seq,
            // C1 §3.5's object (x.3.2 X4 D7): cleanup pending while the
            // turn runs; `acknowledged` once the vendor acknowledged.
            "cancel": active.cancel_requested_at.map(|at| json!({
                "outcome": if acknowledged { "acknowledged" } else { "requested" },
                "cleanup": "pending",
                "requested_at": at,
                "settled_at": null,
            })),
        })
    });
    let queue: Vec<Value> = status
        .queue
        .into_iter()
        .map(|queued| {
            json!({"n": queued.turn, "op_key": queued.op_key, "queued_at": queued.queued_at,
                "effective": intake::c1_effective(&queued.effective)})
        })
        .collect();
    let turns: Vec<Value> = status
        .turns
        .iter()
        .map(|turn| {
            // C1 §3.7, §7.6: a terminal turn reports its envelope's revision.
            if terminal(&turn.state) {
                json!({"n": turn.number, "state": turn.state, "revision": turn.revision})
            } else {
                json!({"n": turn.number, "state": turn.state})
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
        "adapter_version": version.adapter_version,
        "vendor_version": version.vendor_version,
        "version_status": version.version_status,
        "inherit": frozen.inherit.map(|inherit| inherit.effective),
        "warnings": warnings,
        "vendor_session_id": status.vendor_session_id,
        "vendor_identity_verified": verified,
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

/// The basename of the structured-output file a committed envelope names
/// (C1 §5): `structured_output.json`, or a revision's own
/// `structured_output.r…json`; else `None`.
fn structured_output_file(path: &str) -> Option<String> {
    let name = std::path::Path::new(path).file_name()?.to_str()?;
    let named = name == "structured_output.json"
        || name
            .strip_prefix("structured_output.r")
            .and_then(|rest| rest.strip_suffix(".json"))
            .is_some();
    named.then(|| name.to_owned())
}

/// The fixed evidence files present in `folder`, and the structured-output
/// file `named` by the committed envelope, as `{name, bytes}`: one `lstat`
/// each, and only regular files count (design §4.4, §7.1). A missing name
/// is absent; any other `lstat` error fails the read, so an existing file
/// never drops out of the list (C1 §3.12). No other file is looked for:
/// a structured-output file no envelope names is never listed.
fn evidence_files(folder: &std::path::Path, named: Option<&str>) -> std::io::Result<Vec<Value>> {
    let mut files = Vec::new();
    for name in via_store::EVIDENCE_FILES.into_iter().chain(named) {
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

/// The `events` reply for `page`, built around the events array Store wrote.
fn page_reply(page: &EventsPage) -> Result<Box<RawValue>, ApiError> {
    let reply = format!(
        r#"{{"events":{},"next_after":{},"more":{},"earliest_seq":1}}"#,
        page.events, page.next_after, page.more
    );
    debug_assert!(reply.len() <= PAGE_MAX, "an events page exceeds PAGE_MAX");
    RawValue::from_string(reply).map_err(|_| ApiError::STORE)
}

/// What can settle a pending `wait`, `events` long-poll or terminal await
/// (design §4.1): a Store commit, a turn Core records unpersisted (no
/// Store commit), or final shutdown. A change carries no fact; the waiter
/// re-reads.
struct Wakes {
    /// `None` once the Store's writer ended: the read that its close woke
    /// surfaced the Store's error, and a closed channel would wake at once
    /// forever.
    commits: Option<watch::Receiver<u64>>,
    failures: watch::Receiver<u64>,
    finalized: watch::Receiver<bool>,
}

impl Wakes {
    /// Returns at the next change, after [`RECHECK`] without one, or at
    /// `deadline`, whichever is first: the caller re-reads in each case.
    async fn next(&mut self, deadline: Option<tokio::time::Instant>) {
        let recheck = tokio::time::Instant::now() + RECHECK;
        let until = deadline.map_or(recheck, |deadline| deadline.min(recheck));
        // Safe to ignore: expiry is a recheck or the caller's deadline, and
        // the caller re-reads or ends either way.
        let _ = tokio::time::timeout_at(until, self.changed()).await;
    }

    /// Returns at the next change of any source since the last return or
    /// the subscription. Cancel-safe: dropping it loses no change.
    async fn changed(&mut self) {
        let commits = async {
            match self.commits.as_mut() {
                Some(commits) => commits.changed().await.is_err(),
                None => std::future::pending().await,
            }
        };
        // Core owns the other two senders and outlives every waiter, so
        // their `changed()` never ends in `Err`.
        let writer_ended = tokio::select! {
            ended = commits => ended,
            _ = self.failures.changed() => false,
            _ = self.finalized.changed() => false,
        };
        if writer_ended {
            self.commits = None;
        }
    }
}

/// Bounds one of `wait`'s Store reads by the wait's absolute deadline
/// (C1 §3.8): a read still pending then is `wait_timeout`. Dropping it only
/// drops its reply receiver; Store keeps ownership of a read it admitted.
async fn by_deadline<T>(
    deadline: tokio::time::Instant,
    read: impl Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    tokio::time::timeout_at(deadline, read)
        .await
        .unwrap_or(Err(ApiError::WAIT_TIMEOUT))
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
        let listed = evidence_files(&folder, None)?;
        assert_eq!(listed, [serde_json::json!({"name":"stderr.log","bytes":3})]);
        // No search permission: each `lstat` fails with EACCES.
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o600))?;
        let bypassed = std::fs::symlink_metadata(folder.join("stderr.log")).is_ok();
        let unstated = evidence_files(&folder, None);
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))?;
        if bypassed {
            eprintln!("skipped: this process bypasses file permissions (root)");
        } else {
            assert!(unstated.is_err(), "an unstated file was listed as absent");
        }
        Ok(())
    }
}
