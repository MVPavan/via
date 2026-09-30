//! Durable identity and storage boundary types. Store never decides lifecycle
//! outcomes; Core supplies proposed transitions and Store checks their versions.

use std::{fmt, num::NonZeroU32};

use serde::{Deserialize, Deserializer, Serialize};

/// A canonical VIA session identifier (`s_` and 12 lowercase Crockford digits).
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Returns the canonical wire representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for SessionId {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        const CROCKFORD: &str = "0123456789abcdefghjkmnpqrstvwxyz";
        let Some(suffix) = value.strip_prefix("s_") else {
            return Err("session id must start with s_");
        };
        if suffix.len() != 12
            || !suffix
                .bytes()
                .all(|byte| CROCKFORD.as_bytes().contains(&byte))
        {
            return Err("session id must contain 12 lowercase Crockford digits");
        }
        Ok(Self(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value.as_str()).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// One-based turn number within a session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TurnNumber(NonZeroU32);

impl TurnNumber {
    /// Returns the one-based number.
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl TryFrom<u32> for TurnNumber {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        NonZeroU32::new(value)
            .map(Self)
            .ok_or("turn number must be at least one")
    }
}

impl<'de> Deserialize<'de> for TurnNumber {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(u32::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A retry identity as Store keeps it: the identity bytes' length and
/// SHA-256, never the bytes (Task 4 design §6.4, §6.5). A retry matches when
/// both are equal, which keeps C1's byte-identical rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identity {
    /// Length of the identity bytes.
    pub len: u64,
    /// SHA-256 of the identity bytes.
    pub sha256: [u8; 32],
}

impl Identity {
    /// The identity of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            len: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
        }
    }
}

/// An absolute monotonic deadline; never serialized or persisted.
#[derive(Clone, Copy, Debug)]
pub struct Deadline(tokio::time::Instant);

impl Deadline {
    /// Wraps an already calculated absolute instant.
    pub fn at(instant: tokio::time::Instant) -> Self {
        Self(instant)
    }

    /// Returns the absolute instant for nested operations.
    pub fn instant(self) -> tokio::time::Instant {
        self.0
    }
}

/// The first recorded storage failure class, without request payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreFailureKind {
    /// Storage could not be opened or validated.
    Open,
    /// A transaction failed before commit.
    Write,
    /// The writer cannot establish whether a mutation committed.
    UncertainCommit,
    /// A referenced durable span was missing or corrupt.
    CorruptEvidence,
    /// The bounded request quota was exhausted.
    Quota,
    /// SQLite reported a corrupt database.
    Corrupt,
}

/// Whether a requested mutation has a positive commit receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitOutcome<T> {
    /// The mutation committed and may be acknowledged to its caller.
    Committed(T),
    /// The mutation definitely did not commit.
    NotCommitted(StoreFailureKind),
    /// The outcome is unknown; the mutation must never be resent speculatively.
    Uncertain(StoreFailureKind),
}

mod blob;
mod evidence;
#[cfg(feature = "test-failpoints")]
pub mod failpoint;
mod final_text;
pub mod json_limits;
mod lanes;
mod runtime;

pub use blob::{
    BLOB_CHUNK, BlobReader, BlobRef, BlobTasks, BlobWriter, INLINE_MAX, PROMPT_MAX, PromptFileError,
};
pub use evidence::{EVIDENCE_FILES, EvidenceRoot};
pub use final_text::{FINAL_TEXT_FILE_MAX, FinalTextFile, FinalTextRef};
pub use lanes::{Lane, Lanes};

pub use runtime::{
    ANCHOR_PAGE_LIMIT, AcceptanceRecord, ActiveTurn, AnchorCohort, AnchorIdentity, AnchorIntent,
    AnchorIntentReceipt, AnchorOwner, AnchorPhase, AnchorRecord, CancelCause, CloseIntent,
    ClosedOutcome, ClosedRecord, ClosingRecord, ENVELOPE_MAX, EventRecord, EventsPage, EventsQuery,
    EventsRead, EvidenceRefs, FAILURE_BATCH_CANCELLATIONS, FailureResolutionRecord,
    GroupAbsenceRecord, KeyedOperation, ListPage, ListQuery, OperationRecord, OperationVerb,
    PAGE_BYTES, PAGE_MAX, Predecessors, ProcessJournal, Prompt, QueuedSummary, QueuedTurn,
    ReceiptRecord, ResumeRecord, RuntimeResources, SESSION_QUEUE_LIMIT, STATUS_ANCHORS,
    STATUS_QUEUE, STATUS_STEPS, STATUS_TURNS, SessionSnapshot, SessionStatus, SessionSummary,
    SpawnKey, SpawnRecord, StepRow, StepsRecord, Store, StoreClient, StoreError, StoreLock,
    StoredEvent, StoredSpawnKey, SubmissionRecord, SubmitFailedRecord, TerminalCancel,
    TerminalExtras, TerminalFacts, TerminalRecord, UnfinishedTurn, WalLimits, at_ms,
};
