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

/// Connection-local raw log identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ConnectionId(String);

impl ConnectionId {
    /// Returns the wire representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ConnectionId {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let Some(suffix) = value.strip_prefix("c_") else {
            return Err("connection id must start with c_");
        };
        if suffix.is_empty()
            || suffix.len() > 64
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return Err("connection id must contain 1 to 64 lowercase letters or digits");
        }
        Ok(Self(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for ConnectionId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value.as_str()).map_err(serde::de::Error::custom)
    }
}

/// A nonempty, checked byte span in one connection's durable raw payload file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RawRef {
    /// The file-owning connection.
    connection_id: ConnectionId,
    /// Byte offset in that connection's payload file.
    offset: u64,
    /// Number of bytes in the span.
    len: u32,
}

impl RawRef {
    /// Constructs a span only when its end offset is representable.
    pub fn new(connection_id: ConnectionId, offset: u64, len: u32) -> Result<Self, &'static str> {
        if len == 0 || offset.checked_add(u64::from(len)).is_none() {
            return Err("raw reference must be nonempty and within u64 offsets");
        }
        Ok(Self {
            connection_id,
            offset,
            len,
        })
    }

    /// Returns the exclusive end offset, checked by construction.
    pub fn end_offset(&self) -> u64 {
        self.offset + u64::from(self.len)
    }

    /// Returns the owning connection.
    pub fn connection_id(&self) -> &ConnectionId {
        &self.connection_id
    }

    /// Returns the first byte offset.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the number of bytes in the referenced span.
    pub fn byte_len(&self) -> u32 {
        self.len
    }
}

impl<'de> Deserialize<'de> for RawRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            connection_id: ConnectionId,
            offset: u64,
            len: u32,
        }
        let fields = Fields::deserialize(deserializer)?;
        Self::new(fields.connection_id, fields.offset, fields.len).map_err(serde::de::Error::custom)
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
    /// Durable raw bytes or index failed to append or sync.
    Raw,
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

#[cfg(feature = "test-failpoints")]
pub mod failpoint;
mod runtime;

pub use runtime::{
    ANCHOR_PAGE_LIMIT, AcceptanceRecord, AnchorIdentity, AnchorIntent, AnchorIntentReceipt,
    AnchorOwner, AnchorPhase, AnchorRecord, CancelCause, CloseIntent, ClosedOutcome, ClosedRecord,
    ClosingRecord, DurableRaw, EventRecord, FAILURE_BATCH_CANCELLATIONS, FailureResolutionRecord,
    GroupAbsenceRecord, KeyedOperation, OperationRecord, OperationVerb, Predecessors,
    ProcessJournal, QueuedTurn, RawFactory, RawStream, RawWriter, ReceiptRecord, ResumeRecord,
    RuntimeResources, SESSION_QUEUE_LIMIT, SessionSnapshot, SpawnKey, SpawnRecord, Store,
    StoreClient, StoreError, StoreLock, StoredEvent, StoredSpawnKey, SubmissionRecord,
    SubmitFailedRecord, TerminalExtras, TerminalRecord, UnfinishedTurn,
};

#[cfg(feature = "test-failpoints")]
pub use runtime::RawStall;
