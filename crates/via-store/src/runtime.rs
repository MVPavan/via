//! SQLite and the evidence root, resources owned exclusively by Store.

use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::{
    CommitOutcome, EvidenceRoot, Identity, SessionId, StoreFailureKind, TurnNumber,
    evidence::sync_dir,
};

const SCHEMA_VERSION: i64 = 6;

/// Most queued turns one session holds, enforced inside the receipt
/// transaction (C1 P6, runtime §6); Core also checks it to answer `queue_full`.
pub const SESSION_QUEUE_LIMIT: u32 = 8;

/// Refuses a Store whose schema this build neither creates nor reads. Older
/// versions are unreleased dev formats with no migration (runtime §6).
/// Version 0 is a new Store only in a file VIA itself `created`.
fn check_schema_version(version: i64, created: bool) -> Result<(), StoreError> {
    if version > SCHEMA_VERSION {
        return Err(StoreError::Open("newer Store schema".to_owned()));
    }
    if version < SCHEMA_VERSION && !(version == 0 && created) {
        return Err(StoreError::Open(format!(
            "Store schema v{version} is an unreleased development format with no migration; \
             stop the daemon and recreate the Store by removing store.sqlite3 from the State directory"
        )));
    }
    Ok(())
}

/// Storage failure, with no caller handle or vendor payload.
///
/// Design §7.1: `Write`, `Constraint`, `NotEnqueued` and `Refused` were
/// not committed; `Uncertain` and `WriterLost` may have committed; `Corrupt`
/// is SQLite-level corruption, which always latches.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The State directory or database cannot be opened safely.
    #[error("Store open failed: {0}")]
    Open(String),

    /// A mutation failed before a positive commit receipt.
    #[error("Store write failed: {0}")]
    Write(String),
    /// SQLite reported failure while committing; do not resend the mutation.
    #[error("Store commit outcome uncertain: {0}")]
    Uncertain(String),
    /// A request violates a storage constraint.
    #[error("Store constraint: {0}")]
    Constraint(&'static str),
    /// A stored row cannot be read back as written.
    #[error("stored evidence is missing or corrupt")]
    CorruptEvidence,
    /// The request never reached the SQLite writer: its bounded queue was
    /// full. Nothing was written.
    #[error("Store request not enqueued: the writer queue is full")]
    NotEnqueued,
    /// The SQLite writer is gone: its queue is disconnected or
    /// it dropped the reply. The request may have committed.
    #[error("Store writer lost")]
    WriterLost,
    /// SQLite reported a corrupt database (`SQLITE_CORRUPT`, `SQLITE_NOTADB`).
    #[error("Store is corrupt: {0}")]
    Corrupt(String),
    /// A Store-defined refusal, such as `resume` of a closing session:
    /// nothing was written, and it is not a Store failure.
    #[error("Store refused: {0}")]
    Refused(&'static str),
}

impl StoreError {
    fn kind(&self) -> StoreFailureKind {
        match self {
            Self::Open(_) => StoreFailureKind::Open,
            Self::Write(_) | Self::Constraint(_) | Self::Refused(_) => StoreFailureKind::Write,
            Self::Uncertain(_) | Self::WriterLost => StoreFailureKind::UncertainCommit,
            Self::CorruptEvidence => StoreFailureKind::CorruptEvidence,
            Self::NotEnqueued => StoreFailureKind::Quota,
            Self::Corrupt(_) => StoreFailureKind::Corrupt,
        }
    }

    /// A journal write's outcome: a write that may have committed, or hit
    /// SQLite corruption, is uncertain, so it latches (design §7.1).
    fn journal_outcome<T>(self) -> CommitOutcome<T> {
        match self {
            Self::Uncertain(_) | Self::WriterLost | Self::Corrupt(_) => {
                CommitOutcome::Uncertain(self.kind())
            }
            Self::Open(_)
            | Self::Write(_)
            | Self::Constraint(_)
            | Self::CorruptEvidence
            | Self::NotEnqueued
            | Self::Refused(_) => CommitOutcome::NotCommitted(self.kind()),
        }
    }
}

/// Maps a failed `try_send` to the SQLite writer: a full queue never
/// enqueued the request, a disconnected one lost its writer (design §7.1).
fn enqueue_error<T>(error: &TrySendError<T>) -> StoreError {
    match error {
        TrySendError::Full(_) => StoreError::NotEnqueued,
        TrySendError::Disconnected(_) => StoreError::WriterLost,
    }
}

/// Atomic session and first-turn creation proposed by Core.
pub struct SpawnRecord {
    /// Canonical Core-minted session id.
    pub session_id: SessionId,
    /// SHA-256 of the caller handle; plaintext never enters Store.
    pub handle_hash: [u8; 32],
    /// Exact C1 receipt to replay after a committed spawn.
    pub receipt: Value,
    /// Frozen validated parameters for this turn.
    pub params: Value,
    /// Frozen first-turn prompt.
    pub prompt: String,
    /// Turn 1's frozen effective per-turn values (C1 §3.2 `effective`).
    pub effective: Value,
    /// Core's initial canonical queued event, with sequence one.
    pub initial_event: Value,
}

/// A committed receipt returned only after SQLite reports success.
pub struct ReceiptRecord {
    /// Original C1 receipt document.
    pub receipt: Value,
}

/// A spawn's C1 `idempotency_key` and exact retry identity, kept for the
/// session's lifetime.
pub struct SpawnKey {
    /// Caller key, unique per Store.
    pub key: String,
    /// Retry identity of the params with the handle replaced by its hash.
    pub identity: Identity,
}

/// A committed spawn key with the receipt it replays.
pub struct StoredSpawnKey {
    /// Session the key created.
    pub session_id: SessionId,
    /// Retry identity recorded with the key.
    pub identity: Identity,
    /// Original C1 receipt.
    pub receipt: Value,
}

/// A keyed mutation's exact result, replayed for the same `op_key` (C1 §3).
pub struct OperationRecord {
    /// Caller key, unique per session.
    pub op_key: String,
    /// Retry identity of the params with the handle replaced by its hash.
    pub identity: Identity,
    /// Original C1 result.
    pub result: Value,
}

/// A queued turn Core proposes for an existing session, with its receipt.
pub struct ResumeRecord {
    /// Owning session.
    pub session_id: SessionId,
    /// The session's next turn number.
    pub turn: TurnNumber,
    /// Frozen prompt.
    pub prompt: String,
    /// Frozen effective per-turn values, resolved by Core under admission
    /// against [`SessionSnapshot::latest_effective`] (C1 P5).
    pub effective: Value,
    /// Core's canonical `turn.queued` event at the session's next sequence.
    pub event: Value,
    /// The `op_key` result committed with the turn, when the caller gave a key.
    pub operation: Option<OperationRecord>,
}

/// Session facts Core decides a new turn from.
pub struct SessionSnapshot {
    /// The session no longer admits turns.
    pub closed: bool,
    /// The session is durably `closing` (design §4): `resume` is refused.
    pub closing: bool,
    /// Highest turn number.
    pub turns: u32,
    /// Queued turns of the session, without submission intent.
    pub queued: u32,
    /// Frozen effective values of the latest accepted turn, whatever its
    /// state: what an omitted per-turn parameter inherits (C1 P5).
    pub latest_effective: Option<Value>,
}

/// Durable state of a turn's predecessors, from which Core decides dispatch.
pub struct Predecessors {
    /// An earlier turn is still queued or running: no terminal is durable.
    pub unresolved: bool,
    /// Terminal envelope of the latest earlier turn that was submitted.
    pub last_submitted: Option<Value>,
}

/// Durable facts of a queued turn that its submission needs.
pub struct QueuedTurn {
    /// Frozen prompt.
    pub prompt: String,
    /// Frozen effective per-turn values the turn is driven from.
    pub effective: Value,
    /// Time of `turn.queued`.
    pub queued_at: String,
    /// Sequence of `turn.queued`.
    pub queued_seq: u64,
}

/// Core's submission intent and its canonical event, committed before agent I/O.
pub struct SubmissionRecord {
    /// Owning session.
    pub session_id: SessionId,
    /// One-based turn number.
    pub turn: TurnNumber,
    /// Canonical event with the session's next sequence; its `at` becomes `submitted_at`.
    pub event: Value,
}

/// Vendor acceptance evidence and Core's canonical event, committed atomically.
pub struct AcceptanceRecord {
    /// Owning session.
    pub session_id: SessionId,
    /// One-based turn number.
    pub turn: TurnNumber,
    /// Vendor correlation retained as durable C2 acceptance evidence.
    pub correlation: String,
    /// Canonical event with the session's next sequence; its `at` becomes `accepted_at`.
    pub event: Value,
}

/// One canonical event inside a running turn, such as an adapter observation.
pub struct EventRecord {
    /// Owning session.
    pub session_id: SessionId,
    /// One-based running turn.
    pub turn: TurnNumber,
    /// Canonical event with the session's next sequence.
    pub event: Value,
}

/// Core's terminal state and final event, committed atomically.
pub struct TerminalRecord {
    /// Owning session.
    pub session_id: SessionId,
    /// One-based turn number.
    pub turn: TurnNumber,
    /// Canonical C1 envelope.
    pub envelope: Value,
    /// Canonical final event.
    pub event: Value,
}

/// Who cancelled a turn: a caller `cancel` or a `close` (design §4, §10).
/// Recorded in the cancelling transaction; `close` rows derive a close's
/// `cancelled_turns`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelCause {
    /// A caller `cancel`.
    Cancel,
    /// A session `close`.
    Close,
}

impl CancelCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Close => "close",
        }
    }
}

/// Facts a terminal commits in its own transaction (design §7.2 row 6, §10).
#[derive(Default)]
pub struct TerminalExtras {
    /// The turn's `cancel_cause`, when a caller cancel or a close ended it.
    pub cancel_cause: Option<CancelCause>,
}

/// A keyed close's `op_key` and exact retry identity (C1 §3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloseIntent {
    /// Caller key, unique per session.
    pub op_key: String,
    /// Retry identity of the params with the handle replaced by its hash.
    pub identity: Identity,
}

/// The `Closing` commit: the session's `closing` gate and, when keyed, the
/// close's intent row, in one transaction (design §4 step 6).
pub struct ClosingRecord {
    /// Session to close.
    pub session_id: SessionId,
    /// Intent row of a keyed close.
    pub operation: Option<CloseIntent>,
}

/// The `Closed` commit (design §4 dispatcher step 5).
#[derive(Clone)]
pub struct ClosedRecord {
    /// Durably `closing` session.
    pub session_id: SessionId,
    /// Core's `session.closed {reason: "close"}` event at the next sequence.
    pub event: Value,
    /// The keyed close whose result this commit records.
    pub operation: Option<CloseIntent>,
}

/// Whether `Closed` committed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClosedOutcome {
    /// Committed, with the close result derived in the transaction.
    Closed(Value),
    /// Refused, not failed: a turn of the session is queued or running.
    Unfinished,
}

/// Which mutation a keyed operation row belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationVerb {
    /// A keyed `resume`.
    Resume,
    /// A keyed `close`.
    Close,
}

/// A keyed operation row: a close's may have no result yet (design §4).
pub struct KeyedOperation {
    /// The keyed mutation.
    pub verb: OperationVerb,
    /// Retry identity recorded with the key.
    pub identity: Identity,
    /// Committed result; `None` while a close is in progress.
    pub result: Option<Value>,
}

/// A queued turn failed without agent I/O (design §7.2 row 2, §7.3):
/// `turn.submitted` and `turn.ended` in one transaction.
pub struct SubmitFailedRecord {
    /// Owning session.
    pub session_id: SessionId,
    /// The queued turn.
    pub turn: TurnNumber,
    /// Canonical `turn.submitted` at the session's next sequence; its `at`
    /// becomes `submitted_at`.
    pub submitted: Value,
    /// Canonical `turn.ended` right after it.
    pub ended: Value,
    /// The `failed` terminal envelope.
    pub envelope: Value,
}

/// Most queued cancellations one latch batch carries (design §7.4).
pub const FAILURE_BATCH_CANCELLATIONS: usize = 8;

/// The latch's failure-resolution batch (design §7.4): one running turn's
/// terminal and its session's queued cancellations, in one transaction.
pub struct FailureResolutionRecord {
    /// The affected running turn's terminal.
    pub terminal: TerminalRecord,
    /// At most [`FAILURE_BATCH_CANCELLATIONS`] `queued → cancelled` records
    /// of the same session, sequenced after the terminal.
    pub cancellations: Vec<TerminalRecord>,
}

/// A turn with durable submission intent and no terminal, as a crashed daemon
/// left it; recovery resolves it before admission.
pub struct UnfinishedTurn {
    /// Owning session.
    pub session_id: SessionId,
    /// One-based turn number.
    pub turn: TurnNumber,
    /// Durable submission time.
    pub submitted_at: String,
    /// Recorded vendor acceptance correlation, if acceptance committed.
    pub correlation: Option<String>,
}

/// A committed anchor and its owning turn; no marker, identity or control path.
pub struct AnchorOwner {
    /// Opaque committed anchor identifier.
    pub anchor_id: String,
    /// Owning session.
    pub session_id: SessionId,
    /// Owning turn.
    pub turn: TurnNumber,
    /// The owning turn is still `running`: recovery resolves it.
    pub turn_running: bool,
    /// The anchor's last committed launch phase (design §9); `None` when the
    /// stored phase is unreadable, which Host reconciliation then reports.
    pub phase: Option<AnchorPhase>,
}

/// The anchors committed when [`Store::anchor_cohort`] read it, as a bound
/// for later anchor reads (design §8): a read bounded by it never returns
/// an anchor committed afterwards. Anchors are rows of a rowid table that
/// VIA never deletes, replaces or vacuums, so SQLite gives every later
/// insert a rowid above the largest one here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchorCohort(i64);

/// Largest anchor page one read returns; callers page with a cursor.
pub const ANCHOR_PAGE_LIMIT: u32 = 256;

/// One durable event returned in sequence order.
pub struct StoredEvent {
    /// Dense per-session sequence.
    pub seq: u64,
    /// Canonical event document.
    pub event: Value,
}

/// Where a turn's evidence is (design §4.4, §6.7): the selected turn, its
/// folder relative to the State directory (`None` for a turn never
/// submitted) and the session's vendor identity and transcript hint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceRefs {
    /// The selected turn; `None` when the session has no such turn.
    pub turn: Option<TurnNumber>,
    /// `turns.evidence_dir`, relative to the State directory.
    pub evidence_dir: Option<String>,
    /// The session's confirmed vendor session ID.
    pub vendor_session_id: Option<String>,
    /// The route's path hint for the vendor transcript; never opened.
    pub transcript_hint: Option<String>,
}

/// Full Host-created anchor intent, before process creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnchorIntent {
    /// Internal anchor identifier.
    pub anchor_id: String,
    /// Fresh immutable launch generation.
    pub generation: String,
    /// Private marker, never inherited by the vendor.
    pub marker: String,
    /// Exact private control socket path validated by Host.
    pub socket_path: PathBuf,
    /// Owning session.
    pub owner_session: SessionId,
    /// Owning turn.
    pub owner_turn: TurnNumber,
    /// User ID at intent creation.
    pub uid: u32,
    /// Boot identifier at intent creation.
    pub boot_id: String,
    /// PID namespace identifier at intent creation.
    pub pid_namespace: String,
}

/// Native identity verified after Host starts its anchor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnchorIdentity {
    /// Anchor process ID.
    pub pid: u32,
    /// Host-created process group ID.
    pub pgid: u32,
    /// Anchor user ID.
    pub uid: u32,
    /// Boot identifier.
    pub boot_id: String,
    /// PID namespace identifier.
    pub pid_namespace: String,
    /// Kernel process start ticks.
    pub start_ticks: u64,
    /// Private marker from the anchor, never a vendor marker.
    pub marker: String,
}

/// Anchor's irreversible launch phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnchorPhase {
    /// Intent committed, no verified process identity yet.
    Intent,
    /// Full identity committed, ARM not yet authorized.
    Identified,
    /// ARM intent committed; ARM may have been sent once.
    ArmIntent,
}

impl AnchorPhase {
    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "intent" => Ok(Self::Intent),
            "identified" => Ok(Self::Identified),
            "arm_intent" => Ok(Self::ArmIntent),
            _ => Err(StoreError::CorruptEvidence),
        }
    }
}

/// Positive commit receipt for a newly created anchor intent.
pub struct AnchorIntentReceipt {
    /// Version to compare on the next identity mutation.
    pub record_version: u64,
}

/// Evidence from a non-signalling, same-boot group absence query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupAbsenceRecord {
    /// Persisted anchor identifier.
    pub anchor_id: String,
    /// Persisted immutable generation.
    pub generation: String,
    /// Current boot identifier verified against the intent and identity.
    pub boot_id: String,
    /// Current PID namespace verified against the intent and identity.
    pub pid_namespace: String,
    /// Group probed with a non-signalling ESRCH query.
    pub pgid: u32,
    /// Observation timestamp for evidence.
    pub observed_at: String,
    /// The full identity Host verified and probed with. An anchor still at
    /// `intent` phase records it with the proof (design §7.2 row 4); for a
    /// later phase it must equal the committed identity.
    pub identity: Option<AnchorIdentity>,
}

/// Durable anchor snapshot used by Host recovery.
pub struct AnchorRecord {
    /// Original intent.
    pub intent: AnchorIntent,
    /// Full identity when verified and committed.
    pub identity: Option<AnchorIdentity>,
    /// Last committed launch phase.
    pub phase: AnchorPhase,
    /// Current compare-and-set version.
    pub record_version: u64,
    /// Vendor child PID as evidence, never signalling authority.
    pub vendor_pid: Option<u32>,
    /// Positive group absence evidence when committed.
    pub absence: Option<GroupAbsenceRecord>,
}

/// The observer a Store calls on SQLite corruption on a read (design
/// §7.1), registered once by its owner. Store names no caller type: the
/// observer is a plain callback.
type ReadObserver = Box<dyn Fn() + Send + Sync>;

/// Where the SQLite worker finds the read-corruption observer; unset until
/// the owner registers one, then fixed.
#[derive(Clone, Default)]
pub(crate) struct ReadCorruption(Arc<OnceLock<ReadObserver>>);

impl ReadCorruption {
    /// Runs the registered observer, if any, on the worker's thread. The
    /// worker calls it before it sends a read reply carrying
    /// [`StoreError::Corrupt`], so the caller receives the reply only after
    /// the observer returned.
    pub(crate) fn report(&self) {
        if let Some(observer) = self.0.get() {
            observer();
        }
    }
}

/// Owns the single SQLite writer and the evidence root.
pub struct Store {
    client: StoreClient,
    writer_join: Option<JoinHandle<()>>,
    /// Read by the SQLite worker; set once by [`Store::on_read_corruption`].
    read_corruption: ReadCorruption,
    /// Released after `Drop` joined the workers: the last field.
    _lock: StoreLock,
}

/// One unopened pair of Store capabilities passed intact to Wire bootstrap.
pub struct RuntimeResources {
    evidence: EvidenceRoot,
    journal: ProcessJournal,
}

impl RuntimeResources {
    /// Consumes the bundle at the architectural Wire bootstrap boundary.
    pub fn into_wire_parts(self) -> (EvidenceRoot, ProcessJournal) {
        (self.evidence, self.journal)
    }
}

/// Host-only process journal port over Store's existing bounded writer.
#[derive(Clone)]
pub struct ProcessJournal {
    sender: SyncSender<Command>,
}

/// Bounded asynchronous Core access to the sole SQLite connection.
#[derive(Clone)]
pub struct StoreClient {
    sender: SyncSender<Command>,
    evidence: EvidenceRoot,
}

enum Command {
    Spawn(
        SpawnRecord,
        Option<SpawnKey>,
        oneshot::Sender<Result<ReceiptRecord, StoreError>>,
    ),
    SpawnKey(
        String,
        oneshot::Sender<Result<Option<StoredSpawnKey>, StoreError>>,
    ),
    Resume(ResumeRecord, oneshot::Sender<Result<(), StoreError>>),
    Operation(
        SessionId,
        String,
        oneshot::Sender<Result<Option<OperationRecord>, StoreError>>,
    ),
    Snapshot(
        SessionId,
        oneshot::Sender<Result<Option<SessionSnapshot>, StoreError>>,
    ),
    QueuedTurn(
        SessionId,
        TurnNumber,
        oneshot::Sender<Result<Option<QueuedTurn>, StoreError>>,
    ),
    NextSeq(SessionId, oneshot::Sender<Result<Option<u64>, StoreError>>),
    Predecessors(
        SessionId,
        TurnNumber,
        oneshot::Sender<Result<Predecessors, StoreError>>,
    ),
    Submission(SubmissionRecord, oneshot::Sender<Result<(), StoreError>>),
    Acceptance(AcceptanceRecord, oneshot::Sender<Result<(), StoreError>>),
    Event(EventRecord, oneshot::Sender<Result<(), StoreError>>),
    Terminal(
        TerminalRecord,
        TerminalExtras,
        oneshot::Sender<Result<(), StoreError>>,
    ),
    Closing(ClosingRecord, oneshot::Sender<Result<(), StoreError>>),
    Closed(
        ClosedRecord,
        oneshot::Sender<Result<ClosedOutcome, StoreError>>,
    ),
    CloseResult(
        SessionId,
        oneshot::Sender<Result<Option<Value>, StoreError>>,
    ),
    ClosingSessions(
        Option<SessionId>,
        u32,
        oneshot::Sender<Result<Vec<SessionId>, StoreError>>,
    ),
    KeyedOperation(
        SessionId,
        String,
        oneshot::Sender<Result<Option<KeyedOperation>, StoreError>>,
    ),
    SubmitFailed(SubmitFailedRecord, oneshot::Sender<Result<(), StoreError>>),
    FailureResolution(
        FailureResolutionRecord,
        oneshot::Sender<Result<(), StoreError>>,
    ),
    ClosingTerminal(
        TerminalRecord,
        Value,
        oneshot::Sender<Result<bool, StoreError>>,
    ),
    SessionClosed(SessionId, Value, oneshot::Sender<Result<bool, StoreError>>),
    Result(
        SessionId,
        TurnNumber,
        oneshot::Sender<Result<Option<Value>, StoreError>>,
    ),
    Terminated(
        Vec<(SessionId, TurnNumber)>,
        oneshot::Sender<Result<Vec<(SessionId, TurnNumber)>, StoreError>>,
    ),
    Events(
        SessionId,
        u64,
        u32,
        oneshot::Sender<Result<Vec<StoredEvent>, StoreError>>,
    ),
    EvidenceRefs(
        SessionId,
        Option<TurnNumber>,
        oneshot::Sender<Result<Option<EvidenceRefs>, StoreError>>,
    ),
    Unfinished(oneshot::Sender<Result<Vec<UnfinishedTurn>, StoreError>>),
    AnchorOwners(
        Option<String>,
        u32,
        Option<AnchorCohort>,
        oneshot::Sender<Result<Vec<AnchorOwner>, StoreError>>,
    ),
    UnprovenAnchors(
        Option<String>,
        u32,
        Option<AnchorCohort>,
        oneshot::Sender<Result<u64, StoreError>>,
    ),
    AnchorCohort(oneshot::Sender<Result<AnchorCohort, StoreError>>),
    QueuedTurns(
        Option<(SessionId, TurnNumber)>,
        u32,
        oneshot::Sender<Result<Vec<(SessionId, TurnNumber)>, StoreError>>,
    ),
    Authenticate(
        SessionId,
        [u8; 32],
        oneshot::Sender<Result<bool, StoreError>>,
    ),
    AnchorIntent(
        AnchorIntent,
        oneshot::Sender<CommitOutcome<AnchorIntentReceipt>>,
    ),
    AnchorIdentified(
        String,
        String,
        u64,
        AnchorIdentity,
        oneshot::Sender<CommitOutcome<u64>>,
    ),
    ArmIntent(String, String, u64, oneshot::Sender<CommitOutcome<u64>>),
    VendorFacts(String, String, u32, oneshot::Sender<CommitOutcome<()>>),
    GroupAbsence(GroupAbsenceRecord, oneshot::Sender<CommitOutcome<()>>),
    AnchorRecords(
        AnchorQuery,
        oneshot::Sender<Result<Vec<AnchorRecord>, StoreFailureKind>>,
    ),
    Shutdown,
}

/// One page of anchor records: after `after`, at most `limit`, optionally
/// only unproven ones, only one owner session's and only one cohort's.
struct AnchorQuery {
    after: Option<String>,
    limit: u32,
    unproven: bool,
    owner: Option<SessionId>,
    cohort: Option<AnchorCohort>,
}

/// `<state>/store.lock`, held for the life of the [`Store`] opened under
/// it: VIA's writer exclusion for the State directory (runtime §6.1). One
/// daemon holds it; a writer that ignores it is unsupported. It is bound to
/// that directory's identity (device and inode), so it opens no other.
pub struct StoreLock {
    _file: File,
    state: (u64, u64),
}

/// The filesystem identity of `state`: its device and inode.
fn directory_identity(state: &Path) -> Result<(u64, u64), StoreError> {
    let metadata = fs::metadata(state)
        .map_err(|error| StoreError::Open(format!("{}: {error}", state.display())))?;
    Ok((metadata.dev(), metadata.ino()))
}

impl StoreLock {
    /// Takes `<state>/store.lock` without waiting; [`StoreError::Open`]
    /// when another holder has it (the State directory is in use by another
    /// VIA daemon) or the lock file is unsafe.
    pub fn acquire(state: &Path) -> Result<Self, StoreError> {
        let path = state.join("store.lock");
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (!metadata.file_type().is_file()
                || metadata.uid() != current_uid()?
                || metadata.mode() & 0o777 != 0o600)
        {
            return Err(StoreError::Open(format!(
                "unsafe VIA lock file: {}",
                path.display()
            )));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(&path)
            .map_err(|error| StoreError::Open(format!("store.lock: {error}")))?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                _file: file,
                state: directory_identity(state)?,
            }),
            Err(fs::TryLockError::WouldBlock) => Err(StoreError::Open(
                "store.lock is held: the State directory is in use by another VIA daemon"
                    .to_owned(),
            )),
            Err(fs::TryLockError::Error(error)) => {
                Err(StoreError::Open(format!("store.lock: {error}")))
            }
        }
    }
}

/// The read-only connection that checks an existing Store before any
/// mutation (runtime §6, F11). It runs only under `lock`, so no VIA writer
/// can change the file meanwhile. With no `-wal` file every committed page
/// is in the main file, so it is read as immutable: a refused Store gets no
/// `-wal` or `-shm` sidecar. A `-wal` left by a crash is read through an
/// ordinary read-only connection, since `immutable` ignores the WAL
/// (sqlite.org/uri.html) and no read-only open reads a WAL without its
/// wal-index (sqlite.org/wal.html, "Read-Only Databases"). Limit: when the
/// `-shm` is missing, SQLite creates it in the State directory, even for a
/// Store it then refuses.
fn probe(db: &Path, _lock: &StoreLock) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    if Path::new(&wal).exists() {
        return Connection::open_with_flags(db, flags);
    }
    let mut uri = String::from("file:");
    for character in db.to_string_lossy().chars() {
        match character {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            other => uri.push(other),
        }
    }
    uri.push_str("?immutable=1");
    Connection::open_with_flags(uri, flags | OpenFlags::SQLITE_OPEN_URI)
}

impl Store {
    /// Opens `<state>/store.sqlite3` under a [`StoreLock`] it takes first,
    /// refusing an unsafe, older or newer Store before mutation. Only a file
    /// this call creates is initialized.
    pub fn open(state: &Path) -> Result<Self, StoreError> {
        validate_state(state)?;
        Self::open_locked(state, StoreLock::acquire(state)?)
    }

    /// [`Store::open`] under `lock`, which the Store holds until it drops.
    /// A lock taken for another State directory is refused before the Store
    /// is read: it excludes no writer here.
    pub fn open_locked(state: &Path, lock: StoreLock) -> Result<Self, StoreError> {
        validate_state(state)?;
        if directory_identity(state)? != lock.state {
            return Err(StoreError::Open(format!(
                "store.lock was taken for another State directory than {}",
                state.display()
            )));
        }
        let db = state.join("store.sqlite3");
        let created = !db.exists();
        if !created {
            validate_regular(&db)?;
            let readonly =
                probe(&db, &lock).map_err(|error| StoreError::Open(error.to_string()))?;
            let version: i64 = readonly
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(|error| StoreError::Open(error.to_string()))?;
            check_schema_version(version, false)?;
            readonly
                .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                .map_err(|error| StoreError::Open(error.to_string()))
                .and_then(|check| {
                    if check == "ok" {
                        Ok(())
                    } else {
                        Err(StoreError::Open("corrupt Store".to_owned()))
                    }
                })?;
        }
        // Design §7.2: the evidence root, validated as the State directory
        // is; a new one is made durable with one sync of the State directory.
        let evidence_dir = state.join("evidence");
        if fs::symlink_metadata(&evidence_dir).is_ok() {
            validate_state(&evidence_dir)?;
        } else {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&evidence_dir)
                .map_err(|error| StoreError::Open(error.to_string()))?;
            sync_dir(state).map_err(|error| StoreError::Open(error.to_string()))?;
        }
        if created {
            // Exclusive (and never through a symlink): a file that appeared
            // since the check is not one VIA created.
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&db)
                .map_err(|error| StoreError::Open(error.to_string()))?;
        }
        let mut conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| StoreError::Open(error.to_string()))?;
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600))
            .map_err(|error| StoreError::Open(error.to_string()))?;
        configure(&mut conn, created)?;
        let (sender, receiver) = mpsc::sync_channel(128);
        let read_corruption = ReadCorruption::default();
        let observer = read_corruption.clone();
        let writer_join = thread::Builder::new()
            .name("via-store-sqlite".to_owned())
            .spawn(move || writer_loop(conn, &receiver, &observer))
            .map_err(|error| StoreError::Open(error.to_string()))?;
        Ok(Self {
            client: StoreClient {
                sender,
                evidence: EvidenceRoot::new(state),
            },
            writer_join: Some(writer_join),
            read_corruption,
            _lock: lock,
        })
    }

    /// Registers the observer of SQLite corruption on any read (design
    /// §7.1): the SQLite worker calls it on its own thread before it sends a
    /// read reply carrying [`StoreError::Corrupt`], whichever client made the
    /// read. It must not block on this Store. Returns false, keeping the
    /// first, when one is already registered.
    pub fn on_read_corruption(&self, observer: impl Fn() + Send + Sync + 'static) -> bool {
        self.read_corruption.0.set(Box::new(observer)).is_ok()
    }

    /// Returns a bounded, cloneable client for Core lifecycle operations.
    pub fn client(&self) -> StoreClient {
        self.client.clone()
    }

    /// Returns the unopened lower-layer bundle for Adapter and Route pass-through.
    pub fn runtime_resources(&self) -> RuntimeResources {
        RuntimeResources {
            evidence: self.client.evidence.clone(),
            journal: ProcessJournal {
                sender: self.client.sender.clone(),
            },
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // A queued mutation is handled before Shutdown; disconnect never aborts it.
        let _ = self.client.sender.send(Command::Shutdown);
        if let Some(join) = self.writer_join.take() {
            let _ = join.join();
        }
    }
}

impl StoreClient {
    /// Atomically persists the receipt, handle hash, session and queued first turn.
    pub async fn commit_spawn(&self, record: SpawnRecord) -> Result<ReceiptRecord, StoreError> {
        self.commit_keyed_spawn(record, None).await
    }

    /// `commit_spawn` that also records the spawn's idempotency key, atomically.
    pub async fn commit_keyed_spawn(
        &self,
        record: SpawnRecord,
        key: Option<SpawnKey>,
    ) -> Result<ReceiptRecord, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Spawn(record, key, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads a committed spawn key.
    pub async fn spawn_key(&self, key: &str) -> Result<Option<StoredSpawnKey>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::SpawnKey(key.to_owned(), reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Atomically commits a queued turn, its `turn.queued` event and any
    /// `op_key` result. Refuses a closed session or a turn that is not the next.
    pub async fn commit_resume(&self, record: ResumeRecord) -> Result<(), StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Resume(record, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads a session's committed `op_key` result.
    pub async fn operation(
        &self,
        session_id: &SessionId,
        op_key: &str,
    ) -> Result<Option<OperationRecord>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Operation(
            session_id.clone(),
            op_key.to_owned(),
            reply,
        ))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads the facts Core decides a new turn from; `None` for no such session.
    pub async fn session_snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSnapshot>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Snapshot(session_id.clone(), reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads a turn's prompt and `turn.queued` facts while it is still queued.
    pub async fn queued_turn(
        &self,
        session_id: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<QueuedTurn>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::QueuedTurn(session_id.clone(), turn, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads the durable state of the turns before `turn`.
    pub async fn predecessors(
        &self,
        session_id: &SessionId,
        turn: TurnNumber,
    ) -> Result<Predecessors, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Predecessors(session_id.clone(), turn, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads the session's durable next event sequence.
    pub async fn next_seq(&self, session_id: &SessionId) -> Result<Option<u64>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::NextSeq(session_id.clone(), reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Commits submission intent before any agent I/O is authorized.
    pub async fn commit_submission(&self, record: SubmissionRecord) -> Result<(), StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Submission(record, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Commits acceptance with one vendor correlation.
    pub async fn commit_acceptance(&self, record: AcceptanceRecord) -> Result<(), StoreError> {
        if record.correlation.is_empty() || record.correlation.len() > 128 {
            return Err(StoreError::Constraint(
                "acceptance correlation must contain 1 to 128 bytes",
            ));
        }
        let (reply, receive) = oneshot::channel();
        self.send(Command::Acceptance(record, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Commits one event at the next sequence of a turn that is still running.
    pub async fn commit_event(&self, record: EventRecord) -> Result<(), StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Event(record, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Atomically commits a terminal envelope and final event.
    pub async fn commit_terminal(&self, record: TerminalRecord) -> Result<(), StoreError> {
        self.commit_terminal_with(record, TerminalExtras::default())
            .await
    }

    /// [`Self::commit_terminal`] that also records the turn's `cancel_cause`
    /// in the same transaction (design §10).
    pub async fn commit_terminal_with(
        &self,
        record: TerminalRecord,
        extras: TerminalExtras,
    ) -> Result<(), StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Terminal(record, extras, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Commits `Closing`: the session's `closing` gate and a keyed close's
    /// intent row, in one transaction. A closed session is refused
    /// ([`StoreError::Refused`]); a session already closing only gains the
    /// intent row.
    pub async fn commit_closing(&self, record: ClosingRecord) -> Result<(), StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Closing(record, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Commits `Closed` for a durably `closing` session: `session.closed`,
    /// the closed state and the close result, derived in the transaction
    /// from the turns with `cancel_cause = 'close'` and the session's
    /// unproven groups, plus a keyed close's result. Refused, not failed,
    /// while a turn of the session is queued or running.
    pub async fn commit_closed(&self, record: ClosedRecord) -> Result<ClosedOutcome, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Closed(record, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads a closed session's close result: the stored one, or for a
    /// session closed another way the result derived the same way. `None`
    /// while the session is not closed.
    pub async fn session_close_result(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Value>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::CloseResult(session_id.clone(), reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads one page of up to `limit` (1 to 256) durably `closing`, not yet
    /// closed sessions in id order after `after`, for restart (design §4).
    pub async fn closing_sessions_page(
        &self,
        after: Option<SessionId>,
        limit: u32,
    ) -> Result<Vec<SessionId>, StoreError> {
        if limit == 0 || limit > ANCHOR_PAGE_LIMIT {
            return Err(StoreError::Constraint(
                "closing page limit must be 1 to 256",
            ));
        }
        let (reply, receive) = oneshot::channel();
        self.send(Command::ClosingSessions(after, limit, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads a keyed operation row of either verb, including a close still
    /// in progress. [`Self::operation`] reports such a row's result as `null`.
    pub async fn keyed_operation(
        &self,
        session_id: &SessionId,
        op_key: &str,
    ) -> Result<Option<KeyedOperation>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::KeyedOperation(
            session_id.clone(),
            op_key.to_owned(),
            reply,
        ))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Fails a queued turn without agent I/O: `turn.submitted` and a `failed`
    /// `turn.ended` in one transaction (design §7.2 row 2, §7.3).
    pub async fn commit_submit_failed(&self, record: SubmitFailedRecord) -> Result<(), StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::SubmitFailed(record, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Commits the latch's failure-resolution batch in one transaction
    /// (design §7.4).
    pub async fn commit_failure_resolution(
        &self,
        record: FailureResolutionRecord,
    ) -> Result<(), StoreError> {
        if record.cancellations.len() > FAILURE_BATCH_CANCELLATIONS {
            return Err(StoreError::Constraint(
                "a failure batch carries at most 8 cancellations",
            ));
        }
        let (reply, receive) = oneshot::channel();
        self.send(Command::FailureResolution(record, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Atomically commits a terminal envelope and final event, then the
    /// session-level `closed` event after it, and marks the session closed.
    /// While another turn of the session is queued or running the terminal
    /// commits alone: `Ok(false)` reports that the close was not written.
    pub async fn commit_closing_terminal(
        &self,
        record: TerminalRecord,
        closed: Value,
    ) -> Result<bool, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::ClosingTerminal(record, closed, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Commits a session's `session.closed` event alone and marks it closed;
    /// `Ok(false)`, writing nothing, while the session is closed or holds
    /// queued or running work.
    pub async fn commit_session_closed(
        &self,
        session_id: &SessionId,
        closed: Value,
    ) -> Result<bool, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::SessionClosed(session_id.clone(), closed, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads a durable terminal envelope, if one has committed.
    pub async fn result(
        &self,
        session_id: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<Value>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Result(session_id.clone(), turn, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Returns those of at most 1000 `turns` whose terminal envelope has
    /// committed, in one Store operation.
    pub async fn terminated(
        &self,
        turns: Vec<(SessionId, TurnNumber)>,
    ) -> Result<Vec<(SessionId, TurnNumber)>, StoreError> {
        if turns.len() > 1000 {
            return Err(StoreError::Constraint("at most 1000 turns per query"));
        }
        let (reply, receive) = oneshot::channel();
        self.send(Command::Terminated(turns, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads at most one bounded page of durable events.
    pub async fn events(
        &self,
        session_id: &SessionId,
        from_seq: u64,
        limit: u32,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        if limit == 0 || limit > 1000 {
            return Err(StoreError::Constraint("event page limit must be 1 to 1000"));
        }
        let (reply, receive) = oneshot::channel();
        self.send(Command::Events(session_id.clone(), from_seq, limit, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Returns at most 1000 turns that have submission intent but no terminal.
    pub async fn unfinished_turns(&self) -> Result<Vec<UnfinishedTurn>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Unfinished(reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Returns up to `limit` (at most `ANCHOR_PAGE_LIMIT`) committed anchors
    /// after the `after` anchor id with their owning turns, for recovery's
    /// coverage check; page with the last id until a short page.
    pub async fn anchor_owners_page(
        &self,
        after: Option<String>,
        limit: u32,
    ) -> Result<Vec<AnchorOwner>, StoreError> {
        self.anchor_owners(after, limit, None).await
    }

    /// [`Self::anchor_owners_page`] of the anchors in `cohort` only.
    pub async fn cohort_owners_page(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: AnchorCohort,
    ) -> Result<Vec<AnchorOwner>, StoreError> {
        self.anchor_owners(after, limit, Some(cohort)).await
    }

    async fn anchor_owners(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: Option<AnchorCohort>,
    ) -> Result<Vec<AnchorOwner>, StoreError> {
        if limit == 0 || limit > ANCHOR_PAGE_LIMIT {
            return Err(StoreError::Constraint("anchor page limit must be 1 to 256"));
        }
        let (reply, receive) = oneshot::channel();
        self.send(Command::AnchorOwners(after, limit, cohort, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads the cohort of every anchor committed so far (read-only).
    pub async fn anchor_cohort(&self) -> Result<AnchorCohort, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::AnchorCohort(reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Counts committed anchors after the `after` anchor id with no recorded
    /// absence proof, saturating at `limit`: the groups a recovery deadline
    /// left unread (design §11). One indexed query that reads at most
    /// `limit` entries; Core passes the slot pool, and since those holdings
    /// are never released during admission, a count saturated there holds
    /// the same permits as an exact one.
    pub async fn unproven_anchors_up_to(
        &self,
        after: Option<String>,
        limit: u32,
    ) -> Result<u64, StoreError> {
        self.unproven_anchors(after, limit, None).await
    }

    /// [`Self::unproven_anchors_up_to`] of the anchors in `cohort` only.
    pub async fn unproven_cohort_anchors_up_to(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: AnchorCohort,
    ) -> Result<u64, StoreError> {
        self.unproven_anchors(after, limit, Some(cohort)).await
    }

    async fn unproven_anchors(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: Option<AnchorCohort>,
    ) -> Result<u64, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::UnprovenAnchors(after, limit, cohort, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads one page of up to `limit` (1 to 256) durable `queued` turns in
    /// `(session, turn)` order after `after`, for the restart handoff.
    pub async fn queued_turns_page(
        &self,
        after: Option<(SessionId, TurnNumber)>,
        limit: u32,
    ) -> Result<Vec<(SessionId, TurnNumber)>, StoreError> {
        if limit == 0 || limit > ANCHOR_PAGE_LIMIT {
            return Err(StoreError::Constraint("queued page limit must be 1 to 256"));
        }
        let (reply, receive) = oneshot::channel();
        self.send(Command::QueuedTurns(after, limit, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// Reads where a turn's evidence is (design §4.4, §6.7): `turn`, or
    /// with none the session's running turn, else its latest submitted
    /// turn, else its latest turn. `None` when the session does not exist.
    /// Reads no file.
    pub async fn evidence_refs(
        &self,
        session_id: &SessionId,
        turn: Option<TurnNumber>,
    ) -> Result<Option<EvidenceRefs>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::EvidenceRefs(session_id.clone(), turn, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    /// The evidence root, for making a stored folder absolute; no I/O.
    pub fn evidence(&self) -> &EvidenceRoot {
        &self.evidence
    }

    /// Compares a Core-computed SHA-256 hash without receiving the plaintext handle.
    pub async fn authenticate(
        &self,
        session_id: &SessionId,
        hash: &[u8; 32],
    ) -> Result<bool, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Authenticate(session_id.clone(), *hash, reply))?;
        receive.await.map_err(|_| StoreError::WriterLost)?
    }

    fn send(&self, command: Command) -> Result<(), StoreError> {
        send_command(&self.sender, command)
    }
}

/// Enqueues one request for the SQLite writer. The test-only
/// `store.request.not_enqueued` point reports a full queue.
fn send_command(sender: &SyncSender<Command>, command: Command) -> Result<(), StoreError> {
    #[cfg(feature = "test-failpoints")]
    if crate::failpoint::hit("store.request.not_enqueued").is_err() {
        return Err(StoreError::NotEnqueued);
    }
    sender
        .try_send(command)
        .map_err(|error| enqueue_error(&error))
}

impl ProcessJournal {
    /// Commits an immutable anchor intent before Host creates its process.
    pub async fn commit_anchor_intent(
        &self,
        intent: AnchorIntent,
    ) -> CommitOutcome<AnchorIntentReceipt> {
        let (reply, receive) = oneshot::channel();
        self.commit(Command::AnchorIntent(intent, reply), receive)
            .await
    }

    /// Commits full anchor identity with generation and version checks.
    pub async fn commit_anchor_identified(
        &self,
        anchor_id: &str,
        generation: &str,
        expected_version: u64,
        identity: AnchorIdentity,
    ) -> CommitOutcome<u64> {
        let (reply, receive) = oneshot::channel();
        let command = Command::AnchorIdentified(
            anchor_id.to_owned(),
            generation.to_owned(),
            expected_version,
            identity,
            reply,
        );
        self.commit(command, receive).await
    }

    /// Atomically records the irreversible ARM authorization before Host sends ARM once.
    pub async fn commit_arm_intent(
        &self,
        anchor_id: &str,
        generation: &str,
        expected_version: u64,
    ) -> CommitOutcome<u64> {
        let (reply, receive) = oneshot::channel();
        let command = Command::ArmIntent(
            anchor_id.to_owned(),
            generation.to_owned(),
            expected_version,
            reply,
        );
        self.commit(command, receive).await
    }

    /// Records vendor child facts as evidence, never as signalling authority.
    pub async fn commit_vendor_facts(
        &self,
        anchor_id: &str,
        generation: &str,
        pid: u32,
    ) -> CommitOutcome<()> {
        let (reply, receive) = oneshot::channel();
        let command = Command::VendorFacts(anchor_id.to_owned(), generation.to_owned(), pid, reply);
        self.commit(command, receive).await
    }

    /// Stores a positively verified, non-signalling group absence proof. An
    /// anchor still at `intent` phase needs the proof's full identity, which
    /// commits with it (design §7.2 row 4).
    pub async fn commit_group_absence(&self, proof: GroupAbsenceRecord) -> CommitOutcome<()> {
        let (reply, receive) = oneshot::channel();
        self.commit(Command::GroupAbsence(proof, reply), receive)
            .await
    }

    /// Returns up to `limit` (at most `ANCHOR_PAGE_LIMIT`) anchor records
    /// after the `after` anchor id, in id order, each read consistently.
    pub async fn list_anchor_records_page(
        &self,
        after: Option<String>,
        limit: u32,
    ) -> Result<Vec<AnchorRecord>, StoreFailureKind> {
        self.records_page(after, limit, false, None, None).await
    }

    /// [`Self::list_anchor_records_page`] of the anchors in `cohort` only.
    pub async fn list_cohort_records_page(
        &self,
        after: Option<String>,
        limit: u32,
        cohort: AnchorCohort,
    ) -> Result<Vec<AnchorRecord>, StoreFailureKind> {
        self.records_page(after, limit, false, None, Some(cohort))
            .await
    }

    /// [`Self::list_anchor_records_page`] of the anchors with no absence
    /// proof, optionally only those owned by `owner` (design §8, §10).
    pub async fn unproven_anchor_records_page(
        &self,
        after: Option<String>,
        limit: u32,
        owner: Option<SessionId>,
    ) -> Result<Vec<AnchorRecord>, StoreFailureKind> {
        self.records_page(after, limit, true, owner, None).await
    }

    async fn records_page(
        &self,
        after: Option<String>,
        limit: u32,
        unproven: bool,
        owner: Option<SessionId>,
        cohort: Option<AnchorCohort>,
    ) -> Result<Vec<AnchorRecord>, StoreFailureKind> {
        if limit == 0 || limit > ANCHOR_PAGE_LIMIT {
            return Err(StoreFailureKind::Write);
        }
        let (reply, receive) = oneshot::channel();
        let query = AnchorQuery {
            after,
            limit,
            unproven,
            owner,
            cohort,
        };
        self.send(Command::AnchorRecords(query, reply))
            .map_err(|error| error.kind())?;
        receive
            .await
            .unwrap_or(Err(StoreFailureKind::UncertainCommit))
    }

    fn send(&self, command: Command) -> Result<(), StoreError> {
        send_command(&self.sender, command)
    }

    /// Enqueues a journal mutation and awaits its outcome. A request never
    /// enqueued did not commit; a lost writer or reply is uncertain.
    async fn commit<T>(
        &self,
        command: Command,
        receive: oneshot::Receiver<CommitOutcome<T>>,
    ) -> CommitOutcome<T> {
        match self.send(command) {
            Ok(()) => receive
                .await
                .unwrap_or_else(|_| StoreError::WriterLost.journal_outcome()),
            Err(error) => error.journal_outcome(),
        }
    }
}

mod anchor;
mod sql;

use anchor::{
    commit_anchor_identified, commit_anchor_intent, commit_arm_intent, commit_group_absence,
    commit_vendor_facts, count_unproven_anchors, read_anchor_cohort, read_anchor_owners,
    read_anchor_records,
};
use sql::{configure, current_uid, validate_regular, validate_state, writer_loop};

#[cfg(test)]
mod tests {
    use super::{
        CommitOutcome, EvidenceRoot, ProcessJournal, SessionId, StoreClient, StoreError,
        StoreFailureKind, mpsc,
        sql::{commit, commit_error, sql_error},
    };

    fn session() -> SessionId {
        SessionId::try_from("s_7f3k9q2mzr4c").expect("session")
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
    }

    /// Design §7.1 [r3.9, r4.5]: a full writer queue never enqueued the
    /// request; a disconnected queue or a dropped reply lost the writer.
    #[test]
    fn writer_queue_failures_split_into_not_enqueued_and_writer_lost() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let client = StoreClient {
            sender,
            evidence: EvidenceRoot::new(std::path::Path::new("/nonexistent")),
        };
        let journal = ProcessJournal {
            sender: client.sender.clone(),
        };
        runtime().block_on(async {
            // One request fills the queue; the reply of the queued one is kept.
            let queued = tokio::spawn({
                let client = client.clone();
                async move { client.next_seq(&session()).await }
            });
            tokio::task::yield_now().await;
            assert!(matches!(
                client.next_seq(&session()).await,
                Err(StoreError::NotEnqueued)
            ));
            assert!(matches!(
                journal.commit_vendor_facts("a", "g", 2).await,
                CommitOutcome::NotCommitted(StoreFailureKind::Quota)
            ));
            // The worker takes the request and drops its reply unserved.
            drop(receiver.recv().expect("queued request"));
            assert!(matches!(
                queued.await.expect("join"),
                Err(StoreError::WriterLost)
            ));
            let journal_reply = tokio::spawn({
                let journal = journal.clone();
                async move { journal.commit_vendor_facts("a", "g", 2).await }
            });
            tokio::task::yield_now().await;
            drop(receiver.recv().expect("journal request"));
            assert!(matches!(
                journal_reply.await.expect("join"),
                CommitOutcome::Uncertain(StoreFailureKind::UncertainCommit)
            ));
            drop(receiver);
            assert!(matches!(
                client.next_seq(&session()).await,
                Err(StoreError::WriterLost)
            ));
            assert!(matches!(
                journal.commit_vendor_facts("a", "g", 2).await,
                CommitOutcome::Uncertain(StoreFailureKind::UncertainCommit)
            ));
        });
    }

    /// Design §7.1 [O1.D8]: `SQLITE_CORRUPT` and `SQLITE_NOTADB` are
    /// `Corrupt`; any other SQLite failure before `COMMIT` is `Write`, and a
    /// journal write that met corruption is uncertain, so it latches.
    #[test]
    fn sqlite_corruption_is_classified_corrupt() {
        let failure = |code| rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
        for code in [rusqlite::ffi::SQLITE_CORRUPT, rusqlite::ffi::SQLITE_NOTADB] {
            let error = sql_error(failure(code));
            assert!(matches!(error, StoreError::Corrupt(_)), "{error:?}");
            assert!(matches!(
                error.journal_outcome::<()>(),
                CommitOutcome::Uncertain(StoreFailureKind::Corrupt)
            ));
        }
        let full = sql_error(failure(rusqlite::ffi::SQLITE_FULL));
        assert!(matches!(full, StoreError::Write(_)), "{full:?}");
        assert!(matches!(
            full.journal_outcome::<()>(),
            CommitOutcome::NotCommitted(StoreFailureKind::Write)
        ));
    }

    /// Design §7.1 [O1.D8], S1 round-1 decision 1: a `COMMIT` step that
    /// reports `SQLITE_CORRUPT` or `SQLITE_NOTADB` is `Corrupt`; any other
    /// commit failure stays `Uncertain`. Either latches. Every commit site
    /// goes through `commit`, which a real failing `COMMIT` (a deferred
    /// foreign key) exercises here.
    #[test]
    fn commit_failures_are_uncertain_unless_sqlite_reports_corruption() {
        let failure = |code| rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
        for code in [rusqlite::ffi::SQLITE_CORRUPT, rusqlite::ffi::SQLITE_NOTADB] {
            let error = commit_error(failure(code));
            assert!(matches!(error, StoreError::Corrupt(_)), "{error:?}");
            assert!(matches!(
                error.journal_outcome::<()>(),
                CommitOutcome::Uncertain(StoreFailureKind::Corrupt)
            ));
        }
        for code in [
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_IOERR,
            rusqlite::ffi::SQLITE_BUSY,
        ] {
            let error = commit_error(failure(code));
            assert!(matches!(error, StoreError::Uncertain(_)), "{error:?}");
        }
        let mut conn = rusqlite::Connection::open_in_memory().expect("memory database");
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE parent(id INTEGER PRIMARY KEY);
             CREATE TABLE child(parent INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);",
        )
        .expect("schema");
        let tx = conn.transaction().expect("transaction");
        tx.execute("INSERT INTO child VALUES (1)", [])
            .expect("deferred until COMMIT");
        let error = commit(tx).expect_err("the deferred foreign key fails COMMIT");
        assert!(matches!(error, StoreError::Uncertain(_)), "{error:?}");
    }
}
