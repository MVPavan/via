//! SQLite and raw evidence resources owned exclusively by Store.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    thread::{self, JoinHandle},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::{CommitOutcome, ConnectionId, RawRef, SessionId, StoreFailureKind, TurnNumber};

const SCHEMA_VERSION: i64 = 1;
const RAW_MAGIC: &[u8; 8] = b"VIARAW01";
const INDEX_ENTRY_LEN: usize = 45;
const RAW_UNIT_LIMIT: usize = 1_048_576;

/// Storage or evidence failure, with no caller handle or vendor payload.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The State directory or database cannot be opened safely.
    #[error("Store open failed: {0}")]
    Open(String),
    /// A mutation failed before a positive commit receipt.
    #[error("Store write failed: {0}")]
    Write(String),
    /// A raw payload or index append/sync failed.
    #[error("raw Store write failed: {0}")]
    Raw(String),
    /// SQLite reported failure while committing; do not resend the mutation.
    #[error("Store commit outcome uncertain: {0}")]
    Uncertain(String),
    /// A request violates a storage constraint.
    #[error("Store constraint: {0}")]
    Constraint(&'static str),
    /// A referenced raw span is absent or corrupt.
    #[error("raw evidence is missing or corrupt")]
    CorruptEvidence,
    /// The bounded Store request queue is full or closed.
    #[error("Store writer unavailable")]
    Unavailable,
}

impl StoreError {
    fn kind(&self) -> StoreFailureKind {
        match self {
            Self::Open(_) => StoreFailureKind::Open,
            Self::Write(_) | Self::Constraint(_) => StoreFailureKind::Write,
            Self::Raw(_) => StoreFailureKind::Raw,
            Self::Uncertain(_) | Self::Unavailable => StoreFailureKind::UncertainCommit,
            Self::CorruptEvidence => StoreFailureKind::CorruptEvidence,
        }
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
    /// Core's initial canonical queued event, with sequence one.
    pub initial_event: Value,
}

/// A committed receipt returned only after SQLite reports success.
pub struct ReceiptRecord {
    /// Original C1 receipt document.
    pub receipt: Value,
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
    /// Synced raw span of the accepting frame.
    pub raw_ref: RawRef,
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
    /// Already-synced source span, or `None` for a synthesized event.
    pub raw_ref: Option<RawRef>,
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
    /// Optional already-synced evidence reference.
    pub raw_ref: Option<RawRef>,
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
}

/// One durable event returned in sequence order.
pub struct StoredEvent {
    /// Dense per-session sequence.
    pub seq: u64,
    /// Canonical event document.
    pub event: Value,
    /// Optional raw evidence reference.
    pub raw_ref: Option<RawRef>,
}

/// Source stream of a raw payload unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawStream {
    /// Agent standard output.
    Stdout,
    /// Agent standard error.
    Stderr,
    /// Successfully written prefix of agent standard input.
    Stdin,
}

impl RawStream {
    fn code(self) -> u8 {
        match self {
            Self::Stdout => 1,
            Self::Stderr => 2,
            Self::Stdin => 3,
        }
    }
}

/// A raw span whose payload and index were both synced before release.
pub struct DurableRaw(RawRef);

impl DurableRaw {
    /// Returns the checked, persisted span.
    pub fn raw_ref(&self) -> &RawRef {
        &self.0
    }
}

/// Cloneable bounded raw append capability; it owns no file descriptor.
#[derive(Clone)]
pub struct RawWriter {
    connection_id: ConnectionId,
    sender: SyncSender<RawCommand>,
}

/// Opaque capability that opens per-connection raw writers without exposing SQLite.
#[derive(Clone)]
pub struct RawFactory {
    sender: SyncSender<RawCommand>,
}

impl RawFactory {
    /// Opens an append capability for a validated connection id.
    pub fn open(&self, connection_id: ConnectionId) -> RawWriter {
        RawWriter {
            connection_id,
            sender: self.sender.clone(),
        }
    }
}

impl RawWriter {
    /// Appends and syncs a bounded unit on Store's raw worker thread.
    pub async fn append(
        &self,
        stream: RawStream,
        bytes: Vec<u8>,
    ) -> Result<DurableRaw, StoreError> {
        if bytes.is_empty() || bytes.len() > RAW_UNIT_LIMIT {
            return Err(StoreError::Constraint(
                "raw unit must contain 1 to 1048576 bytes",
            ));
        }
        let (reply, receive) = oneshot::channel();
        self.sender
            .try_send(RawCommand::Append {
                connection_id: self.connection_id.clone(),
                stream,
                bytes,
                reply,
            })
            .map_err(|_| StoreError::Unavailable)?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }
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

/// Owns the single SQLite writer and the independent raw worker.
pub struct Store {
    client: StoreClient,
    raw_sender: SyncSender<RawCommand>,
    writer_join: Option<JoinHandle<()>>,
    raw_join: Option<JoinHandle<()>>,
}

/// Releases a stalled raw worker when dropped.
#[cfg(feature = "test-failpoints")]
pub struct RawStall {
    _release: mpsc::Sender<()>,
}

/// One unopened pair of Store capabilities passed intact to Wire bootstrap.
pub struct RuntimeResources {
    raw: RawFactory,
    journal: ProcessJournal,
}

impl RuntimeResources {
    /// Consumes the bundle at the architectural Wire bootstrap boundary.
    pub fn into_wire_parts(self) -> (RawFactory, ProcessJournal) {
        (self.raw, self.journal)
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
}

enum Command {
    Spawn(
        SpawnRecord,
        oneshot::Sender<Result<ReceiptRecord, StoreError>>,
    ),
    Submission(SubmissionRecord, oneshot::Sender<Result<(), StoreError>>),
    Acceptance(AcceptanceRecord, oneshot::Sender<Result<(), StoreError>>),
    Event(EventRecord, oneshot::Sender<Result<(), StoreError>>),
    Terminal(TerminalRecord, oneshot::Sender<Result<(), StoreError>>),
    ClosingTerminal(
        TerminalRecord,
        Value,
        oneshot::Sender<Result<(), StoreError>>,
    ),
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
    Logs(SessionId, oneshot::Sender<Result<Value, StoreError>>),
    Unfinished(oneshot::Sender<Result<Vec<UnfinishedTurn>, StoreError>>),
    AnchorOwners(oneshot::Sender<Result<Vec<AnchorOwner>, StoreError>>),
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
    AnchorRecords(oneshot::Sender<Result<Vec<AnchorRecord>, StoreFailureKind>>),
    Shutdown,
}

enum RawCommand {
    Append {
        connection_id: ConnectionId,
        stream: RawStream,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<DurableRaw, StoreError>>,
    },
    /// Test-only fault: holds the raw worker until the paired sender drops.
    #[cfg(feature = "test-failpoints")]
    Stall(Receiver<()>),
    Shutdown,
}

impl Store {
    /// Opens `<state>/store.sqlite3`, refusing an unsafe or newer Store before mutation.
    pub fn open(state: &Path) -> Result<Self, StoreError> {
        validate_state(state)?;
        let db = state.join("store.sqlite3");
        if db.exists() {
            validate_regular(&db)?;
            let readonly = Connection::open_with_flags(
                &db,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .map_err(|error| StoreError::Open(error.to_string()))?;
            let version: i64 = readonly
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(|error| StoreError::Open(error.to_string()))?;
            if version > SCHEMA_VERSION {
                return Err(StoreError::Open("newer Store schema".to_owned()));
            }
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
        let raw_dir = state.join("raw");
        if raw_dir.exists() {
            validate_state(&raw_dir)?;
        } else {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&raw_dir)
                .map_err(|error| StoreError::Open(error.to_string()))?;
            File::open(state)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| StoreError::Open(error.to_string()))?;
        }
        let mut conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| StoreError::Open(error.to_string()))?;
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600))
            .map_err(|error| StoreError::Open(error.to_string()))?;
        configure(&mut conn)?;
        let (sender, receiver) = mpsc::sync_channel(128);
        let (raw_sender, raw_receiver) = mpsc::sync_channel(64);
        let writer_root = state.to_path_buf();
        let writer_join = thread::Builder::new()
            .name("via-store-sqlite".to_owned())
            .spawn(move || writer_loop(conn, &writer_root, &receiver))
            .map_err(|error| StoreError::Open(error.to_string()))?;
        let raw_join = thread::Builder::new()
            .name("via-store-raw".to_owned())
            .spawn(move || raw_loop(&raw_dir, &raw_receiver))
            .map_err(|error| StoreError::Open(error.to_string()))?;
        Ok(Self {
            client: StoreClient { sender },
            raw_sender,
            writer_join: Some(writer_join),
            raw_join: Some(raw_join),
        })
    }

    /// Returns a bounded, cloneable client for Core lifecycle operations.
    pub fn client(&self) -> StoreClient {
        self.client.clone()
    }

    /// Test-only fault: every raw append queued after this call waits until the
    /// returned guard drops. Drop the guard before the Store owner, whose drop
    /// joins the raw worker.
    #[cfg(feature = "test-failpoints")]
    pub fn stall_raw_worker(&self) -> RawStall {
        let (release, held) = mpsc::channel();
        // The queue is bounded; a full queue only delays the stall behind real appends.
        let _ = self.raw_sender.send(RawCommand::Stall(held));
        RawStall { _release: release }
    }

    /// Returns the unopened lower-layer bundle for Adapter and Route pass-through.
    pub fn runtime_resources(&self) -> RuntimeResources {
        RuntimeResources {
            raw: RawFactory {
                sender: self.raw_sender.clone(),
            },
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
        let _ = self.raw_sender.send(RawCommand::Shutdown);
        if let Some(join) = self.writer_join.take() {
            let _ = join.join();
        }
        if let Some(join) = self.raw_join.take() {
            let _ = join.join();
        }
    }
}

impl StoreClient {
    /// Atomically persists the receipt, handle hash, session and queued first turn.
    pub async fn commit_spawn(&self, record: SpawnRecord) -> Result<ReceiptRecord, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Spawn(record, reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Commits submission intent before any agent I/O is authorized.
    pub async fn commit_submission(&self, record: SubmissionRecord) -> Result<(), StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Submission(record, reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Commits acceptance with a synced raw span and one vendor correlation.
    pub async fn commit_acceptance(&self, record: AcceptanceRecord) -> Result<(), StoreError> {
        if record.correlation.is_empty() || record.correlation.len() > 128 {
            return Err(StoreError::Constraint(
                "acceptance correlation must contain 1 to 128 bytes",
            ));
        }
        let (reply, receive) = oneshot::channel();
        self.send(Command::Acceptance(record, reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Commits one event at the next sequence of a turn that is still running.
    pub async fn commit_event(&self, record: EventRecord) -> Result<(), StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Event(record, reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Atomically commits a terminal envelope and final event.
    pub async fn commit_terminal(&self, record: TerminalRecord) -> Result<(), StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Terminal(record, reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Atomically commits a terminal envelope and final event, then the
    /// session-level `closed` event after it, and marks the session closed.
    pub async fn commit_closing_terminal(
        &self,
        record: TerminalRecord,
        closed: Value,
    ) -> Result<(), StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::ClosingTerminal(record, closed, reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Reads a durable terminal envelope, if one has committed.
    pub async fn result(
        &self,
        session_id: &SessionId,
        turn: TurnNumber,
    ) -> Result<Option<Value>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Result(session_id.clone(), turn, reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
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
        receive.await.map_err(|_| StoreError::Unavailable)?
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
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Returns at most 1000 turns that have submission intent but no terminal.
    pub async fn unfinished_turns(&self) -> Result<Vec<UnfinishedTurn>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Unfinished(reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Returns every committed anchor id with its owning turn (at most
    /// 10000; more is an explicit error), for recovery's coverage check.
    pub async fn anchor_owners(&self) -> Result<Vec<AnchorOwner>, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::AnchorOwners(reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Reads committed raw excerpts for one session in event order.
    pub async fn logs(&self, session_id: &SessionId) -> Result<Value, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Logs(session_id.clone(), reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Compares a Core-computed SHA-256 hash without receiving the plaintext handle.
    pub async fn authenticate(
        &self,
        session_id: &SessionId,
        hash: &[u8; 32],
    ) -> Result<bool, StoreError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Authenticate(session_id.clone(), *hash, reply))?;
        receive.await.map_err(|_| StoreError::Unavailable)?
    }

    fn send(&self, command: Command) -> Result<(), StoreError> {
        self.sender.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) | TrySendError::Disconnected(_) => StoreError::Unavailable,
        })
    }
}

impl ProcessJournal {
    /// Commits an immutable anchor intent before Host creates its process.
    pub async fn commit_anchor_intent(
        &self,
        intent: AnchorIntent,
    ) -> CommitOutcome<AnchorIntentReceipt> {
        let (reply, receive) = oneshot::channel();
        match self.send(Command::AnchorIntent(intent, reply)) {
            Ok(()) => receive
                .await
                .unwrap_or(CommitOutcome::Uncertain(StoreFailureKind::UncertainCommit)),
            Err(error) => CommitOutcome::NotCommitted(error.kind()),
        }
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
        match self.send(Command::AnchorIdentified(
            anchor_id.to_owned(),
            generation.to_owned(),
            expected_version,
            identity,
            reply,
        )) {
            Ok(()) => receive
                .await
                .unwrap_or(CommitOutcome::Uncertain(StoreFailureKind::UncertainCommit)),
            Err(error) => CommitOutcome::NotCommitted(error.kind()),
        }
    }

    /// Atomically records the irreversible ARM authorization before Host sends ARM once.
    pub async fn commit_arm_intent(
        &self,
        anchor_id: &str,
        generation: &str,
        expected_version: u64,
    ) -> CommitOutcome<u64> {
        let (reply, receive) = oneshot::channel();
        match self.send(Command::ArmIntent(
            anchor_id.to_owned(),
            generation.to_owned(),
            expected_version,
            reply,
        )) {
            Ok(()) => receive
                .await
                .unwrap_or(CommitOutcome::Uncertain(StoreFailureKind::UncertainCommit)),
            Err(error) => CommitOutcome::NotCommitted(error.kind()),
        }
    }

    /// Records vendor child facts as evidence, never as signalling authority.
    pub async fn commit_vendor_facts(
        &self,
        anchor_id: &str,
        generation: &str,
        pid: u32,
    ) -> CommitOutcome<()> {
        let (reply, receive) = oneshot::channel();
        match self.send(Command::VendorFacts(
            anchor_id.to_owned(),
            generation.to_owned(),
            pid,
            reply,
        )) {
            Ok(()) => receive
                .await
                .unwrap_or(CommitOutcome::Uncertain(StoreFailureKind::UncertainCommit)),
            Err(error) => CommitOutcome::NotCommitted(error.kind()),
        }
    }

    /// Stores a positively verified, non-signalling group absence proof.
    pub async fn commit_group_absence(&self, proof: GroupAbsenceRecord) -> CommitOutcome<()> {
        let (reply, receive) = oneshot::channel();
        match self.send(Command::GroupAbsence(proof, reply)) {
            Ok(()) => receive
                .await
                .unwrap_or(CommitOutcome::Uncertain(StoreFailureKind::UncertainCommit)),
            Err(error) => CommitOutcome::NotCommitted(error.kind()),
        }
    }

    /// Returns a consistent bounded snapshot of retained anchor records.
    pub async fn list_anchor_records(&self) -> Result<Vec<AnchorRecord>, StoreFailureKind> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::AnchorRecords(reply))
            .map_err(|error| error.kind())?;
        receive
            .await
            .unwrap_or(Err(StoreFailureKind::UncertainCommit))
    }

    fn send(&self, command: Command) -> Result<(), StoreError> {
        self.sender.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) | TrySendError::Disconnected(_) => StoreError::Unavailable,
        })
    }
}

mod anchor;
mod raw;
mod sql;

use anchor::{
    commit_anchor_identified, commit_anchor_intent, commit_arm_intent, commit_group_absence,
    commit_vendor_facts, read_anchor_owners, read_anchor_records,
};
use raw::{raw_loop, read_raw_ref, validate_raw_ref};
use sql::{configure, validate_regular, validate_state, writer_loop};
