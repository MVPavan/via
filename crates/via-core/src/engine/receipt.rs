//! Receipts: `spawn`, `resume`, the queued-turn commit and `steer`.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::SystemTime,
};

use serde_json::{Value, json};
use via_store::{
    BLOB_CHUNK, BlobRef, INLINE_MAX, OperationRecord, Prompt, PromptFileError, ResumeRecord,
    SessionSnapshot, SpawnKey, SpawnRecord, StoreError,
};

use super::journal::{self, Head};
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use super::queue::{DAEMON_QUEUE_LIMIT, SESSION_QUEUE_LIMIT, Slot};
use super::{Admission, Engine, Receipted, lock};
use crate::api::{
    Capabilities, Effective, Event, EventBody, Named, PATH_MAX, PromptSource, Receipt, RoutePlan,
    TurnReceipt, retry_key, rfc3339,
};
use crate::{
    ApiError, ResumeParams, SessionId, SpawnParams, SteerParams, TurnNumber, hash_handle,
    retry_identity,
};

impl Engine {
    /// A receipt commit that reported failure (C1 §8.1, design §7.2 row 1):
    /// `store_error` with `commit_outcome`. Not committed, it is scoped to
    /// the request; one that may have committed, or hit corruption, latches
    /// and is `unknown` with `retry: same_key_only`, which restart recovery
    /// settles.
    fn receipt_failed(&self, error: &StoreError, admission: &Admission<'_>) -> ApiError {
        let outcome = WriteOutcome::of(error);
        self.store_failure(FailureSite::Receipt, outcome, FailureScope::Request)
            .finish_held(admission);
        outcome.api_error()
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

    /// Design §6.5, §10.4: a turn's prompt, staged with no lock held. An
    /// inline prompt over `INLINE_MAX` is written to a finished blob in
    /// 64 KiB chunks; a prompt file is copied into one in a single pass,
    /// which also gives its content token `sha256:<64 hex>:<len>` for the
    /// retry identity. A blob write that fails is `not_committed` for the
    /// request, and its handle unlinks the unfinished file.
    async fn stage_prompt(&self, source: PromptSource) -> Result<Staged, ApiError> {
        let not_committed = |_| WriteOutcome::NotCommitted.api_error();
        let text = match source {
            PromptSource::Inline(text) => text,
            PromptSource::File(path) => {
                if path.len() > PATH_MAX {
                    return Err(ApiError::prompt_file("unreadable"));
                }
                if !Path::new(&path).is_absolute() {
                    return Err(ApiError::prompt_file("not_absolute"));
                }
                let deadline = tokio::time::Instant::now() + PROMPT_FILE_PASS;
                let blob = self
                    .store
                    .copy_prompt_file(PathBuf::from(path), deadline)
                    .await
                    .map_err(|error| match error {
                        PromptFileError::Refused(reason) => ApiError::prompt_file(reason),
                        PromptFileError::Store(_) => WriteOutcome::NotCommitted.api_error(),
                    })?;
                let content = content_token(&blob);
                return Ok(Staged {
                    prompt: Prompt::Blob(blob.clone()),
                    pending: Some(blob),
                    content: Some(content),
                });
            }
        };
        if text.len() <= INLINE_MAX {
            return Ok(Staged {
                prompt: Prompt::Inline(text),
                pending: None,
                content: None,
            });
        }
        let mut writer = self.store.blob_writer().await.map_err(not_committed)?;
        for chunk in text.as_bytes().chunks(BLOB_CHUNK) {
            if let Err(error) = writer.write(chunk).await {
                writer.discard().await;
                return Err(not_committed(error));
            }
        }
        drop(text);
        let blob = writer.finish().await.map_err(not_committed)?;
        Ok(Staged {
            prompt: Prompt::Blob(blob.clone()),
            pending: Some(blob),
            content: None,
        })
    }

    /// Design §6.5: a staged blob that no commit adopted (a replay, a
    /// conflict, a refusal, a commit known not to have happened) is
    /// discarded after `admission` is released.
    async fn discard_unadopted(&self, pending: Option<BlobRef>) {
        if let Some(blob) = pending {
            self.store.discard_blob(blob).await;
        }
    }

    /// Design §11.1: the session's `cwd`, checked by an owned blocking step
    /// with no lock held: at most 4096 bytes, absolute and an existing
    /// directory. An omitted `cwd` is the daemon's working directory at
    /// startup (§5.1 #22).
    async fn session_cwd(&self, cwd: Option<String>) -> Result<String, ApiError> {
        let invalid = |message| {
            ApiError::naming(
                ApiError::INVALID_PARAMS,
                &const { Named::field("cwd") },
                message,
            )
        };
        let Some(cwd) = cwd else {
            return self
                .cwd
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("the daemon's working directory is not UTF-8; give cwd"));
        };
        if cwd.len() > PATH_MAX || !Path::new(&cwd).is_absolute() {
            return Err(invalid(
                "cwd must be an absolute path of at most 4096 bytes",
            ));
        }
        let path = PathBuf::from(&cwd);
        // An owned, bounded blocking step: a stalled filesystem holds a
        // Store blob-step slot, never an unowned thread.
        let directory = self
            .store
            .blocking_step(move || {
                Ok(std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()))
            })
            .await
            .map_err(|_| WriteOutcome::NotCommitted.api_error())?;
        if directory {
            Ok(cwd)
        } else {
            Err(invalid("cwd is not an existing directory"))
        }
    }

    /// Commits a receipt before authorizing any process launch.
    ///
    /// Design §10.3: the prompt is staged, the `cwd` checked and the retry
    /// identity streamed with no lock held; under `admission` a keyed retry
    /// is looked up before any admission check, the `cwd` check's result
    /// included (runtime §6): the same key,
    /// handle and byte-identical `raw_params` (a prompt file by its
    /// content) replay the stored receipt, anything else under the key is
    /// `idempotency_conflict`.
    pub async fn spawn(
        &self,
        mut params: SpawnParams,
        raw_params: &str,
    ) -> Result<Receipted, ApiError> {
        let source = params.take_prompt()?;
        params.check_session_members()?;
        let hash = hash_handle(&params.handle)?;
        let key = retry_key(params.idempotency_key.as_deref())?.map(str::to_owned);
        // Checked with no lock held, applied only to new work: a keyed
        // replay comes before any current-state check (runtime §6).
        let cwd = self.session_cwd(params.cwd.take()).await;
        let Staged {
            prompt,
            mut pending,
            content,
        } = self.stage_prompt(source).await?;
        // Task 4 design §5.3: read before `admission`, applied only to new work.
        let free = self.free_space().await;
        let receipted = match key
            .map(|key| {
                retry_identity(raw_params, &hash, content.as_deref())
                    .map(|identity| SpawnKey { key, identity })
            })
            .transpose()
        {
            Ok(key) => {
                self.spawn_admitted(params, (prompt, cwd), (hash, key, free), &mut pending)
                    .await
            }
            Err(error) => Err(error),
        };
        self.discard_unadopted(pending).await;
        receipted
    }

    /// A keyed spawn's stored receipt, under `admission` before any
    /// admission check (runtime §6); another identity under the key is
    /// `idempotency_conflict`. `None` when there is no key or none stored.
    async fn spawn_replay(&self, key: Option<&SpawnKey>) -> Result<Option<Receipted>, ApiError> {
        let Some(key) = key else {
            return Ok(None);
        };
        let Some(stored) = self
            .store
            .spawn_key(&key.key)
            .await
            .map_err(|_| ApiError::STORE)?
        else {
            return Ok(None);
        };
        if stored.identity == key.identity {
            Ok(Some(Receipted {
                receipt: stored.receipt,
                enqueued: None,
            }))
        } else {
            Err(ApiError::IDEMPOTENCY_CONFLICT)
        }
    }

    /// `spawn` under `admission`: `pending` is taken by a commit that may
    /// have happened, and left for discard otherwise.
    async fn spawn_admitted(
        &self,
        params: SpawnParams,
        (prompt, cwd): (Prompt, Result<String, ApiError>),
        (hash, key, free): ([u8; 32], Option<SpawnKey>, Option<FreeSpace>),
        pending: &mut Option<BlobRef>,
    ) -> Result<Receipted, ApiError> {
        let admission = self.admission.lock().await;
        // Runtime §7: no new mutation, not even a keyed replay, after a failed write.
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
        if let Some(replayed) = self.spawn_replay(key.as_ref()).await? {
            return Ok(replayed);
        }
        // No key was found: the `cwd` check and the floor apply to this new
        // work (§5.3).
        let cwd = cwd?;
        self.floor_admits(free.as_ref())?;
        if lock(&self.signal.stop).is_some() {
            return Err(ApiError::DAEMON_STOPPING);
        }
        // Bounds the turns retained for their `store_error` reads.
        journal::admission(&self.store, &self.unresolved).await?;
        if params.harness != "fake" || !self.harness_available(&params.harness) {
            return Err(ApiError::HARNESS_UNAVAILABLE);
        }
        // Design §5.2: with its harness named, a model the catalog lacks
        // passes through for the vendor to judge.
        if is_empty(&prompt) {
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
            body: EventBody::TurnQueued { queue_position: 0 },
        }
        .to_value()?;
        #[cfg(test)]
        self.hold(&self.faults.hold_receipt).await;
        let adopting = pending.take();
        let stored = self
            .store
            .commit_keyed_spawn(
                SpawnRecord {
                    session_id: session.clone(),
                    handle_hash: hash,
                    receipt: receipt.clone(),
                    // Design §11.1 (A14): the frozen session parameters.
                    params: json!({"harness":"fake","model":&params.model,"cwd":cwd,
                        "allow_untested":params.allow_untested}),
                    label: params.label,
                    effective: receipt["effective"].clone(),
                    prompt,
                    initial_event,
                },
                key,
            )
            .await;
        if let Err(error) = self.receipt_reply(stored) {
            if WriteOutcome::of(&error) == WriteOutcome::NotCommitted {
                *pending = adopting;
            }
            // Task 4 design §5.4: refused before `BEGIN`; not a Store failure.
            if matches!(error, StoreError::WalFull) {
                return Err(ApiError::WAL_FULL);
            }
            return Err(self.receipt_failed(&error, &admission));
        }
        self.session_opened();
        let slot = Slot::new(Head::new(Some(2)), std::sync::Weak::clone(&self.me));
        lock(&self.sessions).insert(session.clone(), Arc::clone(&slot));
        self.receipted(&session, turn, &slot);
        Ok(Receipted {
            receipt,
            enqueued: Some((session, turn)),
        })
    }

    /// C1 §3.3: authenticates, replays a keyed retry, then commits the next
    /// turn `queued` with its `turn.queued` event before the receipt. The
    /// prompt is staged and the identity streamed with no lock held, after
    /// the session and its handle ([`Self::authenticate_existing`]): a wrong
    /// or missing handle does no prompt-file I/O and writes no blob.
    pub async fn resume(
        &self,
        mut params: ResumeParams,
        raw_params: &str,
    ) -> Result<Receipted, ApiError> {
        let (hash, _) = self
            .authenticate_existing(&params.session, params.handle.as_deref())
            .await?;
        let source = params.take_prompt()?;
        let key = retry_key(params.op_key.as_deref())?.map(str::to_owned);
        let Staged {
            prompt,
            mut pending,
            content,
        } = self.stage_prompt(source).await?;
        // Task 4 design §5.3: read before `admission`, applied only to new work.
        let free = self.free_space().await;
        let receipted = match key
            .map(|key| {
                retry_identity(raw_params, &hash, content.as_deref())
                    .map(|identity| (key, identity))
            })
            .transpose()
        {
            Ok(operation) => {
                self.resume_admitted(params, prompt, (operation, free), &mut pending)
                    .await
            }
            Err(error) => Err(error),
        };
        self.discard_unadopted(pending).await;
        receipted
    }

    /// `resume` under `admission`; `pending` as for `spawn_admitted`.
    async fn resume_admitted(
        &self,
        params: ResumeParams,
        prompt: Prompt,
        (operation, free): (Option<(String, via_store::Identity)>, Option<FreeSpace>),
        pending: &mut Option<BlobRef>,
    ) -> Result<Receipted, ApiError> {
        let admission = self.admission.lock().await;
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
        if is_empty(&prompt) {
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
        if let Some((key, identity)) = &operation
            && let Some(stored) = self
                .store
                .operation(&session, key)
                .await
                .map_err(|_| ApiError::STORE)?
        {
            return if stored.identity == *identity {
                Ok(Receipted {
                    enqueued: None,
                    receipt: stored.result,
                })
            } else {
                Err(ApiError::IDEMPOTENCY_CONFLICT)
            };
        }
        // No key was found: the floor applies to this new work (§5.3).
        self.floor_admits(free.as_ref())?;
        // Design §4: a closing session refuses `resume`, from Store's gate
        // or, after an uncertain `Closing`, from memory.
        if snapshot.closed || snapshot.closing || lock(&self.closing).contains(&session) {
            return Err(ApiError::SESSION_CLOSED);
        }
        if lock(&self.signal.stop).is_some() {
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
            (prompt, effective),
            operation,
            (&admission, pending),
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
        (prompt, effective): (Prompt, Effective),
        operation: Option<(String, via_store::Identity)>,
        (admission, pending): (&Admission<'_>, &mut Option<BlobRef>),
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
        // Nothing was written; Store's read reply already reported SQLite
        // corruption (design §7.1, T3-S5 round 2, decision 11).
        let Ok(head) = slot.head.lock(&self.store, &session).await else {
            return Err(ApiError::STORE);
        };
        let at = rfc3339(SystemTime::now());
        let event = Event {
            seq: head.next(),
            session_id: &session,
            turn: Some(turn.get()),
            late: false,
            at: &at,
            body: EventBody::TurnQueued {
                queue_position: snapshot.queued,
            },
        }
        .to_value()?;
        let adopting = pending.take();
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
                *pending = adopting;
                drop(head);
                drop(slot);
                self.retire(&session);
                return Err(ApiError::SESSION_CLOSED);
            }
            // Task 4 design §5.4: refused before `BEGIN` at `wal.max`;
            // nothing was written, and it is not a Store failure.
            Err(StoreError::WalFull) => {
                *pending = adopting;
                drop(head);
                drop(slot);
                self.retire(&session);
                return Err(ApiError::WAL_FULL);
            }
            Err(error) => {
                if WriteOutcome::of(&error) == WriteOutcome::NotCommitted {
                    *pending = adopting;
                }
                if WriteOutcome::of(&error).head_unknown() {
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

    /// The session and its handle ([`Self::authenticate_existing`]), then
    /// the latch, then fake's unsupported mutation capability.
    pub async fn steer(&self, params: SteerParams) -> Result<Value, ApiError> {
        self.authenticate_existing(&params.session, params.handle.as_deref())
            .await?;
        if self.store_failed() {
            return Err(ApiError::STORE);
        }
        Err(ApiError::UNSUPPORTED_VERB)
    }
}

/// An empty prompt: an empty inline text, or an empty prompt file's blob.
fn is_empty(prompt: &Prompt) -> bool {
    match prompt {
        Prompt::Inline(text) => text.is_empty(),
        Prompt::Blob(blob) => blob.is_empty(),
    }
}

/// A receipt's free-space read (Task 4 design §5.3); `None` with the
/// floor off.
type FreeSpace = Result<u64, StoreError>;

/// Design §10.4: the whole prompt-file pass ends within this bound.
const PROMPT_FILE_PASS: std::time::Duration = std::time::Duration::from_secs(10);

/// A staged turn prompt: what the commit stores, the blob it adopts, and a
/// prompt file's content token for the retry identity.
struct Staged {
    prompt: Prompt,
    pending: Option<BlobRef>,
    content: Option<String>,
}

/// A prompt file's content token (design §10.3): `sha256:<64 hex>:<len>`.
fn content_token(blob: &BlobRef) -> String {
    use std::fmt::Write as _;
    let mut token = String::from("sha256:");
    for byte in blob.sha256() {
        let _ = write!(token, "{byte:02x}");
    }
    let _ = write!(token, ":{}", blob.len());
    token
}
