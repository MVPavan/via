//! C2 §4 observation and turn-end types (adapter design §3.2, AD4, AD6,
//! AD7, AD20) and the per-session observation channel the driver lane
//! sends them on.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::value::RawValue;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc};
use tokio::time::{Instant, timeout_at};

use crate::plan::{VersionStatus, Warning};
use crate::runtime::{OBSERVATION_BYTES, OBSERVATION_ITEMS};
use crate::{
    AcceptanceToken, Cleanup, RouteFailure, StartRejected, VendorTerminalStatus, VendorTurnId,
};

/// A vendor message's progress marks (C2 §4 `progress`); the arrival time
/// is the item's `at`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProgressMarks {
    /// Model output: text, reasoning or a tool request.
    pub model: bool,
    /// Tools started, as `(id, name)`.
    pub tools_started: Vec<(String, String)>,
    /// Ids of tools ended.
    pub tools_ended: Vec<String>,
    /// A per-model-call usage sample, never a cumulative total (AD6).
    pub usage: Option<UsageSample>,
}

/// One observation, stamped when the driver decoded it (C2 §4).
#[derive(Debug)]
pub struct ObservationItem {
    /// When the driver decoded it; Core records wall time.
    pub at: Instant,
    /// The vendor turn it belongs to, which Core maps to a turn number.
    pub vendor_turn: Option<VendorTurnId>,
    /// The payload.
    pub observation: Observation,
}

/// A C2 §4 observation.
#[derive(Debug)]
pub enum Observation {
    /// `turn.accepted`.
    Accepted(Acceptance),
    /// `session.vendor_identity_confirmed`.
    IdentityConfirmed(Identity),
    /// A vendor message's progress marks.
    Progress(ProgressMarks),
    /// One completed `final_text` piece.
    FinalText(String),
    /// `action.denied`.
    ActionDenied(Denial),
    /// `vendor.request_declined`.
    RequestDeclined(Decline),
    /// `steer.delivered`, carrying the token of the steer it answers
    /// (C2 `SteerInput.token`).
    SteerDelivered {
        /// How the input reached the vendor.
        delivery: SteerDelivery,
        /// The token Core gave the same input.
        token: SteerToken,
    },
    /// `warning`.
    Warning(Warning),
    /// `session.vendor_closed`, with its reason.
    VendorClosed(String),
    /// `resume.mismatch`.
    ResumeMismatch {
        /// The vendor session ID VIA asked to continue.
        requested: String,
        /// The one the vendor returned.
        returned: String,
    },
    /// `turn.late_terminal`: only for a turn whose end carried no terminal.
    LateTerminal(VendorTerminal),
}

impl Observation {
    /// The token a `steer.delivered` carries; `None` for any other.
    #[must_use]
    pub fn steer_token(&self) -> Option<SteerToken> {
        if let Self::SteerDelivered { token, .. } = self {
            Some(*token)
        } else {
            None
        }
    }
}

/// Acceptance of one submission, on vendor evidence.
#[derive(Debug)]
pub struct Acceptance {
    /// The token shared with the start result.
    pub correlation: AcceptanceToken,
    /// The vendor's turn ID, when the route has one.
    pub vendor_turn_id: Option<VendorTurnId>,
    /// The handshake of the instance running the turn (AD7), read before
    /// its acceptance; `None` when the route read none.
    pub instance: Option<InstanceReport>,
}

/// A confirmed vendor identity for the current connection generation.
#[derive(Debug)]
pub struct Identity {
    /// The confirmed vendor session ID.
    pub vendor_session_id: String,
    /// The connection generation that confirmed it.
    pub connection_id: String,
    /// The vendor transcript, when known.
    pub transcript: Option<PathBuf>,
    /// The vendor version the confirming handshake carried, if it carried
    /// one (C2 §4): `session.opened`/`session.reopened`'s `vendor_version`.
    pub vendor_version: Option<String>,
}

/// An action class the vendor's own bound denied (C1 §5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DenialKind {
    /// A file write.
    FileWrite,
    /// A command.
    Command,
    /// Network access.
    Network,
    /// Anything else.
    Other,
}

/// One denied action.
#[derive(Debug)]
pub struct Denial {
    /// The action class.
    pub kind: DenialKind,
    /// What was denied, bounded.
    pub target: String,
    /// Why, bounded.
    pub reason: String,
}

/// One vendor request VIA declined.
#[derive(Debug)]
pub struct Decline {
    /// The vendor method.
    pub vendor_method: String,
    /// A bounded summary.
    pub summary: String,
    /// Whether the vendor was blocked on it.
    pub blocking: bool,
}

/// A steer's correlation (C2 `SteerInput.token`, critical r2 #2): Core
/// mints it, unique within the session, and passes it in `SteerInput`;
/// the `steer.delivered` observation the driver emits for the same input
/// carries it. Opaque to the driver.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SteerToken(u64);

impl SteerToken {
    /// The token numbered `value`, as Core mints it.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Its number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// How steer input reached the vendor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SteerDelivery {
    /// Injected into the active turn.
    Injected,
    /// Delivered with the profile's declared partial semantics.
    Partial(Cow<'static, str>),
}

/// A normalized stop reason (AD5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    /// The model ended its turn.
    EndTurn,
    /// The step limit was reached.
    MaxSteps,
    /// A budget was exhausted.
    Budget,
    /// The model refused.
    Refusal,
    /// The turn was interrupted.
    Interrupted,
    /// The vendor reported an error.
    Error,
    /// Any other vendor reason, kept in `vendor_stop_reason`.
    Other,
}

/// A vendor's suggestion for the failure class; Core decides.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClassHint {
    /// Authentication failed.
    Auth,
    /// Rate limited.
    RateLimit,
    /// The context window was exceeded.
    ContextExceeded,
    /// A budget was exceeded.
    BudgetExceeded,
    /// Another vendor error.
    VendorError,
    /// A protocol contradiction.
    Protocol,
    /// The vendor returned a different session.
    ResumeMismatch,
}

/// One usage sample (AD6): keyed samples supersede, keyless ones add.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageSample {
    /// The vendor's sample key, if any.
    pub key: Option<String>,
    /// Input tokens.
    pub input: Option<u64>,
    /// Cached input tokens.
    pub cached_input: Option<u64>,
    /// Output tokens.
    pub output: Option<u64>,
    /// Reasoning output tokens.
    pub reasoning_output: Option<u64>,
    /// Total tokens.
    pub total: Option<u64>,
}

/// A vendor-reported cost.
#[derive(Clone, Debug, PartialEq)]
pub struct CostReport {
    /// US dollars.
    pub usd: f64,
    /// The declared cost scope.
    pub scope: String,
}

/// The one retained vendor terminal of a turn (AD4).
#[derive(Debug)]
pub struct VendorTerminal {
    /// When it was decoded.
    pub at: Instant,
    /// The vendor's terminal status.
    pub status: VendorTerminalStatus,
    /// The normalized stop reason.
    pub stop_reason: StopReason,
    /// The vendor's own stop reason.
    pub vendor_stop_reason: String,
    /// The vendor's failure code.
    pub vendor_code: Option<String>,
    /// The suggested failure class.
    pub class_hint: Option<ClassHint>,
    /// A bounded failure detail.
    pub detail: Option<String>,
    /// Structured output, validated by Core.
    pub structured_output: Option<Box<RawValue>>,
    /// Structured output the route assembled from text that is no value
    /// Core can validate (C2 §2 `NotJson`, `OverLimit`): present and
    /// invalid, with `structured_output` `None`.
    pub structured_output_unparsed: Option<UnparsedOutput>,
    /// Steps the vendor counted.
    pub steps: Option<u64>,
    /// The turn aggregate, superseding call samples.
    pub usage: Option<UsageSample>,
    /// The vendor's cost.
    pub cost: Option<CostReport>,
    /// Bounded vendor data (16 KiB) for the envelope's `vendor` member.
    pub vendor: Option<Box<RawValue>>,
}

/// Why a route's structured output, assembled from text, is no value
/// (C2 §2 `VendorTerminal`): Core treats it as present and invalid (C1 §5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnparsedOutput {
    /// The text does not parse as JSON: `reason: invalid`.
    NotJson,
    /// The text passed the route's retention bound or the JSON structure
    /// limits: `reason: validation_limit`.
    OverLimit,
}

impl UnparsedOutput {
    /// C1 §5's `data.reason` of the invalid output.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotJson => "invalid",
            Self::OverLimit => "validation_limit",
        }
    }
}

/// The version an instance reported at its own handshake (AD7);
/// `Tested` or `Untested` only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceReport {
    /// The handshake version.
    pub vendor_version: Option<String>,
    /// Whether it is in the adapter's `checked` set.
    pub version_status: VersionStatus,
}

/// Where a leftover report was taken (AD20).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeftoverScope {
    /// After a per-turn process.
    Turn,
    /// After a server stopped.
    Server,
}

/// One process left over by the coding agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeftoverProcess {
    /// Its pid.
    pub pid: u32,
    /// The kernel's name, lossy UTF-8.
    pub comm: String,
    /// RFC 3339 UTC, second precision.
    pub started_at: String,
}

/// Processes the agent left behind (AD20).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeftoverReport {
    /// Where it was taken.
    pub scope: LeftoverScope,
    /// At most 16, oldest first.
    pub processes: Vec<LeftoverProcess>,
    /// Matches found; a lower bound when `incomplete`.
    pub total: u32,
    /// Detection did not finish.
    pub incomplete: bool,
}

/// Process and cleanup facts of a turn (C2 §4.1), on every outcome: the
/// cleanup gate always has facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnEvidence {
    /// The confirmed process exit, when there is one.
    pub exit: Option<via_routes::ExitReport>,
    /// Cleanup certainty.
    pub cleanup: Cleanup,
    /// A Host journal write had an uncertain outcome.
    pub journal_uncertain: bool,
}

impl TurnEvidence {
    /// A failure before any vendor launch (C2 §2, AD9 no-launch row): no
    /// exit, and `Quiescent` only when Host's journal is complete.
    pub fn no_launch(journal_uncertain: bool) -> Self {
        Self {
            exit: None,
            cleanup: if journal_uncertain {
                Cleanup::Uncertain
            } else {
                Cleanup::Quiescent
            },
            journal_uncertain,
        }
    }

    /// Route's evidence of a failed turn: a launched one's exit and
    /// cleanup, unproven when Route established none; otherwise the
    /// no-launch evidence.
    pub fn of_failure(failure: &RouteFailure) -> Self {
        if !failure.launched {
            return Self::no_launch(failure.journal_uncertain);
        }
        Self {
            exit: failure.exit,
            cleanup: failure
                .cleanup
                .map_or(Cleanup::Uncertain, crate::runtime::cleanup),
            journal_uncertain: failure.journal_uncertain,
        }
    }
}

/// The one result of `run_turn` (C2 §4.1).
#[derive(Debug)]
pub struct TurnEnd {
    /// The decoded vendor terminal, retained even when earlier observations
    /// could not be delivered.
    pub terminal: Option<VendorTerminal>,
    /// Set once the handshake was read, on every outcome (AD7).
    pub instance: Option<InstanceReport>,
    /// Per-turn routes on every outcome, and server loss (AD20).
    pub leftovers: Option<LeftoverReport>,
    /// Process and cleanup facts, or a typed failure.
    pub outcome: Result<TurnEvidence, AdapterError>,
}

/// A failed turn or adapter construction (C2 §2 `AdapterError`). Every turn
/// failure carries its evidence ([`Self::evidence`]).
#[derive(Debug, Error)]
pub enum AdapterError {
    /// A route cause with Route's evidence: S1's causes, `ServerLost` and
    /// transport loss on the persistent profile, and `HandshakeRefused`.
    #[error("route failed: {0}")]
    Route(RouteFailure),
    /// A definite rejection before acceptance; nothing was resent.
    #[error("the turn was rejected before submission: {reason:?}")]
    Rejected {
        /// Why.
        reason: StartRejected,
        /// The turn's process facts, or the no-launch evidence.
        evidence: TurnEvidence,
    },
    /// The vendor returned another session than the one VIA continues,
    /// before the turn's terminal (C2 §2 Reopen); never `Rejected`.
    #[error("the vendor returned another session")]
    ResumeMismatch {
        /// The turn's process facts.
        evidence: TurnEvidence,
    },
    /// No adapter serves the session's harness in this daemon; nothing
    /// launched.
    #[error("the harness is not available in this daemon")]
    Unavailable,
    /// The driver's task for the turn ended without its result.
    #[error("the driver's turn task failed")]
    TaskFailed,
    /// The lower runtime could not initialize, or a Host-fact operation
    /// failed.
    #[error("adapter runtime failed: {0}")]
    Open(#[from] via_routes::WireError),
}

impl AdapterError {
    /// The failure's process and cleanup facts (C2 §4.1): Route's for a
    /// route cause, the no-launch evidence where nothing launched, and an
    /// unproven cleanup where the driver lost the turn's task.
    pub fn evidence(&self) -> TurnEvidence {
        match self {
            Self::Route(failure) => TurnEvidence::of_failure(failure),
            Self::Rejected { evidence, .. } | Self::ResumeMismatch { evidence } => evidence.clone(),
            Self::Unavailable | Self::Open(_) => TurnEvidence::no_launch(false),
            Self::TaskFailed => TurnEvidence {
                exit: None,
                cleanup: Cleanup::Uncertain,
                journal_uncertain: false,
            },
        }
    }

    /// Durable Store state could not be read or written; other failures leave
    /// evidence unproven without making Store unusable.
    pub fn is_store_failure(&self) -> bool {
        matches!(self, Self::Open(error) if error.is_store_failure())
    }

    /// A Host journal write had an uncertain outcome: the daemon latches
    /// (design §7.2 row 12).
    pub fn journal_uncertain(&self) -> bool {
        match self {
            Self::Open(error) => error.journal_uncertain(),
            Self::Route(failure) => failure.journal_uncertain,
            Self::Rejected { evidence, .. } | Self::ResumeMismatch { evidence } => {
                evidence.journal_uncertain
            }
            Self::Unavailable | Self::TaskFailed => false,
        }
    }
}

/// One observation in the session channel with its share of the session's
/// 4 MiB budget: Core holds `permit` until it has handled the item.
pub struct Admitted {
    /// The observation.
    pub item: ObservationItem,
    /// The item's bytes of the budget.
    pub permit: OwnedSemaphorePermit,
}

/// The sending side of one session's observation channel (C2 A1: 1,024
/// items and 4 MiB per session); Core keeps the receiver.
#[derive(Clone)]
pub struct ObservationSink {
    sender: mpsc::Sender<Admitted>,
    budget: Arc<Semaphore>,
}

/// One session's observation channel (C2 §2 `SessionCx`).
pub fn observation_channel() -> (ObservationSink, mpsc::Receiver<Admitted>) {
    observation_channel_in(&ObservationBudget::new())
}

/// One session's 4 MiB observation byte budget (C2 A1), which a session
/// keeps across its channels: a replaced driver's channel and its
/// successor's share it, so the admitted payloads of both stay within it.
#[derive(Clone, Debug)]
pub struct ObservationBudget(Arc<Semaphore>);

impl ObservationBudget {
    /// A full budget.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Semaphore::new(OBSERVATION_BYTES)))
    }

    /// The bytes not held by an admitted item.
    #[must_use]
    pub fn available(&self) -> usize {
        self.0.available_permits()
    }

    /// Whether `other` is a handle on this same budget.
    #[must_use]
    pub fn shares(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Test builds only: `bytes` of the budget, as an admitted item holds
    /// them; `None` when they are not free.
    #[cfg(feature = "test-failpoints")]
    #[must_use]
    pub fn charge(&self, bytes: u32) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.0).try_acquire_many_owned(bytes).ok()
    }
}

impl Default for ObservationBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// A session channel on the session's `budget` (C2 §2 `SessionCx`).
pub fn observation_channel_in(
    budget: &ObservationBudget,
) -> (ObservationSink, mpsc::Receiver<Admitted>) {
    // Full: the driver waits, up to the stall bound, then fails the turn
    // `overflow` (C2 §7 item 12).
    let (sender, receiver) = mpsc::channel(OBSERVATION_ITEMS);
    let budget = Arc::clone(&budget.0);
    (ObservationSink { sender, budget }, receiver)
}

/// An item the channel did not take.
#[derive(Debug)]
pub(crate) enum Undelivered {
    /// Not within the stall bound, or past what the budget can ever admit.
    Stalled,
    /// The receiver is gone.
    Closed,
}

impl ObservationSink {
    /// Test builds only: sends `item` as a driver does ([`Self::send`]);
    /// whether the channel took it.
    #[cfg(feature = "test-failpoints")]
    pub async fn deliver(&self, item: ObservationItem, stall: Duration) -> bool {
        self.send(item, stall).await.is_ok()
    }

    /// Sends `item`: acquires its byte cost, then a slot. The item owns one
    /// stall deadline, set at its first block; at it the send gives up.
    /// Test builds: `adapter.observation.admitted` acknowledges each item
    /// the channel took, `adapter.observation.stalled` a send that gave
    /// up at its stall deadline, and `adapter.observation.blocked` each
    /// block, before its wait.
    pub(crate) async fn send(
        &self,
        item: ObservationItem,
        stall: Duration,
    ) -> Result<(), Undelivered> {
        let sent = self.admit(item, stall).await;
        #[cfg(feature = "test-failpoints")]
        {
            let point = match &sent {
                Ok(()) => Some("adapter.observation.admitted"),
                Err(Undelivered::Stalled) => Some("adapter.observation.stalled"),
                Err(Undelivered::Closed) => None,
            };
            if let Some(point) = point {
                let _ = via_routes::failpoint::hit_async(point).await;
            }
        }
        sent
    }

    async fn admit(&self, item: ObservationItem, stall: Duration) -> Result<(), Undelivered> {
        let mut stall_at = None;
        let wanted = u32::try_from(item_cost(&item)).map_err(|_| Undelivered::Stalled)?;
        let permit = match Arc::clone(&self.budget).try_acquire_many_owned(wanted) {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                #[cfg(feature = "test-failpoints")]
                blocked().await;
                let at = *stall_at.get_or_insert_with(|| Instant::now() + stall);
                timeout_at(at, Arc::clone(&self.budget).acquire_many_owned(wanted))
                    .await
                    .map_err(|_| Undelivered::Stalled)?
                    .map_err(|_| Undelivered::Closed)?
            }
            Err(TryAcquireError::Closed) => return Err(Undelivered::Closed),
        };
        let admitted = Admitted { item, permit };
        match self.sender.try_send(admitted) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(admitted)) => {
                #[cfg(feature = "test-failpoints")]
                blocked().await;
                let at = *stall_at.get_or_insert_with(|| Instant::now() + stall);
                timeout_at(at, self.sender.send(admitted))
                    .await
                    .map_err(|_| Undelivered::Stalled)?
                    .map_err(|_| Undelivered::Closed)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Undelivered::Closed),
        }
    }

    /// Reserves what [`Self::send`] would take for `item`: its byte cost,
    /// then a channel slot, under one stall deadline set at the first
    /// block (x.3.2 X0 item 13.2). The send itself is then synchronous
    /// ([`Reserved::send`]), so a caller can make it under its own lock;
    /// a dropped reservation releases both. Test builds hit the same
    /// `adapter.observation.blocked` and `.stalled` points as a send.
    pub(crate) async fn reserve(
        &self,
        item: &ObservationItem,
        stall: Duration,
    ) -> Result<Reserved<'_>, Undelivered> {
        let reserved = self.reserve_in(item, stall).await;
        #[cfg(feature = "test-failpoints")]
        if matches!(reserved, Err(Undelivered::Stalled)) {
            let _ = via_routes::failpoint::hit_async("adapter.observation.stalled").await;
        }
        reserved
    }

    async fn reserve_in(
        &self,
        item: &ObservationItem,
        stall: Duration,
    ) -> Result<Reserved<'_>, Undelivered> {
        let mut stall_at = None;
        let wanted = u32::try_from(item_cost(item)).map_err(|_| Undelivered::Stalled)?;
        let permit = match Arc::clone(&self.budget).try_acquire_many_owned(wanted) {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                #[cfg(feature = "test-failpoints")]
                blocked().await;
                let at = *stall_at.get_or_insert_with(|| Instant::now() + stall);
                timeout_at(at, Arc::clone(&self.budget).acquire_many_owned(wanted))
                    .await
                    .map_err(|_| Undelivered::Stalled)?
                    .map_err(|_| Undelivered::Closed)?
            }
            Err(TryAcquireError::Closed) => return Err(Undelivered::Closed),
        };
        let slot = match self.sender.try_reserve() {
            Ok(slot) => slot,
            Err(mpsc::error::TrySendError::Full(())) => {
                #[cfg(feature = "test-failpoints")]
                blocked().await;
                let at = *stall_at.get_or_insert_with(|| Instant::now() + stall);
                timeout_at(at, self.sender.reserve())
                    .await
                    .map_err(|_| Undelivered::Stalled)?
                    .map_err(|_| Undelivered::Closed)?
            }
            Err(mpsc::error::TrySendError::Closed(())) => return Err(Undelivered::Closed),
        };
        Ok(Reserved { permit, slot })
    }
}

/// A send [`ObservationSink::reserve`] made room for.
pub(crate) struct Reserved<'a> {
    permit: OwnedSemaphorePermit,
    slot: mpsc::Permit<'a, Admitted>,
}

impl Reserved<'_> {
    /// Sends `item`, synchronously, into the reserved slot with the
    /// reserved bytes. Test builds acknowledge it at
    /// `adapter.observation.admitted` through [`admitted`], after the
    /// caller's own lock.
    pub(crate) fn send(self, item: ObservationItem) {
        self.slot.send(Admitted {
            item,
            permit: self.permit,
        });
    }
}

/// Test builds: acknowledges a reserved send, as [`ObservationSink::send`]
/// acknowledges each item the channel took.
pub(crate) async fn admitted() {
    #[cfg(feature = "test-failpoints")]
    {
        let _ = via_routes::failpoint::hit_async("adapter.observation.admitted").await;
    }
}

/// A send blocked on the budget or the channel, before its wait.
#[cfg(feature = "test-failpoints")]
async fn blocked() {
    let _ = via_routes::failpoint::hit_async("adapter.observation.blocked").await;
}

/// An item's cost against the byte budget: `512 + Σ(64 + len)` over every
/// variable-size field it keeps (Task 4 design §2.3), saturating.
fn item_cost(item: &ObservationItem) -> usize {
    let mut lengths: Vec<usize> = Vec::new();
    if let Some(vendor_turn) = &item.vendor_turn {
        lengths.push(vendor_turn.as_str().len());
    }
    match &item.observation {
        // The acceptance keeps its own copy of the vendor turn ID.
        Observation::Accepted(acceptance) => {
            if let Some(vendor_turn_id) = &acceptance.vendor_turn_id {
                lengths.push(vendor_turn_id.as_str().len());
            }
            if let Some(version) = acceptance
                .instance
                .as_ref()
                .and_then(|instance| instance.vendor_version.as_ref())
            {
                lengths.push(version.len());
            }
        }
        Observation::SteerDelivered { delivery, .. } => match delivery {
            SteerDelivery::Injected => {}
            SteerDelivery::Partial(semantics) => lengths.push(semantics.len()),
        },
        Observation::IdentityConfirmed(identity) => {
            lengths.push(identity.vendor_session_id.len());
            lengths.push(identity.connection_id.len());
            if let Some(transcript) = &identity.transcript {
                lengths.push(transcript.as_os_str().len());
            }
            if let Some(version) = &identity.vendor_version {
                lengths.push(version.len());
            }
        }
        Observation::Progress(marks) => {
            for (id, name) in &marks.tools_started {
                lengths.push(id.len());
                lengths.push(name.len());
            }
            lengths.extend(marks.tools_ended.iter().map(String::len));
            if let Some(key) = marks.usage.as_ref().and_then(|usage| usage.key.as_deref()) {
                lengths.push(key.len());
            }
        }
        Observation::FinalText(string) | Observation::VendorClosed(string) => {
            lengths.push(string.len());
        }
        Observation::ActionDenied(denial) => {
            lengths.push(denial.target.len());
            lengths.push(denial.reason.len());
        }
        Observation::RequestDeclined(decline) => {
            lengths.push(decline.vendor_method.len());
            lengths.push(decline.summary.len());
        }
        Observation::Warning(warning) => {
            lengths.push(warning.code.len());
            lengths.push(warning.message.len());
            if let Some(data) = &warning.data {
                lengths.push(encoded_len(data));
            }
        }
        Observation::ResumeMismatch {
            requested,
            returned,
        } => {
            lengths.push(requested.len());
            lengths.push(returned.len());
        }
        Observation::LateTerminal(terminal) => terminal_lengths(terminal, &mut lengths),
    }
    lengths.iter().fold(512_usize, |cost, length| {
        cost.saturating_add(length.saturating_add(64))
    })
}

/// A late terminal's variable-size fields.
fn terminal_lengths(terminal: &VendorTerminal, lengths: &mut Vec<usize>) {
    let strings = [
        Some(terminal.vendor_stop_reason.as_str()),
        terminal.vendor_code.as_deref(),
        terminal.detail.as_deref(),
        terminal.structured_output.as_deref().map(RawValue::get),
        terminal
            .usage
            .as_ref()
            .and_then(|usage| usage.key.as_deref()),
        terminal.cost.as_ref().map(|cost| cost.scope.as_str()),
        terminal.vendor.as_deref().map(RawValue::get),
    ];
    lengths.extend(strings.into_iter().flatten().map(str::len));
}

/// `value`'s encoded bytes, counted without keeping them.
fn encoded_len(value: &serde_json::Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    // A `Value` always encodes; were it not to, what was counted stands.
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

#[cfg(test)]
mod tests {
    use super::{
        Identity, Instant, Observation, ObservationItem, ProgressMarks, StopReason, VendorTerminal,
        item_cost, observation_channel,
    };
    use crate::VendorTerminalStatus;
    use crate::plan::Warning;
    use crate::runtime::{OBSERVATION_BYTES, OBSERVATION_ITEMS};
    use std::time::Duration;

    fn tool(name: &str) -> ObservationItem {
        ObservationItem {
            at: Instant::now(),
            vendor_turn: None,
            observation: Observation::Progress(ProgressMarks {
                tools_started: vec![("t".to_owned(), name.to_owned())],
                ..ProgressMarks::default()
            }),
        }
    }

    /// Design §2.3 Bounds: an item costs `512 + Σ(64 + len)` of the
    /// session's 4 MiB; with the receiver never drained, large items fill
    /// the budget well before 1,024 items and the next send stays blocked
    /// until the stall. The fake route's short fields (at most 1 KiB)
    /// cannot reach 4 MiB within 1,024 items, so the byte bound is checked
    /// here.
    #[test]
    fn the_byte_budget_admits_items_to_4_mib_then_the_next_stalls() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let name = "n".repeat(100 * 1024);
        let cost = 512 + (64 + 1) + (64 + name.len());
        let fits = OBSERVATION_BYTES / cost;
        assert!(fits < OBSERVATION_ITEMS);
        let (sink, receiver) = observation_channel();
        runtime.block_on(async {
            let stall = Duration::from_millis(50);
            for _ in 0..fits {
                assert!(sink.send(tool(&name), stall).await.is_ok());
            }
            assert_eq!(receiver.len(), fits);
            assert!(sink.send(tool(&name), stall).await.is_err());
            assert_eq!(receiver.len(), fits);
            // A small item still fits what is left.
            assert!(sink.send(tool("n"), stall).await.is_ok());
            assert_eq!(receiver.len(), fits + 1);
        });
    }

    /// Bead via-mnx: distinct clones, each on its own thread, send
    /// together while a reader drains the channel. Each producer's items
    /// arrive in its own order with their own `at`, none lost or repeated;
    /// concurrent producers' items may interleave out of `at` order (C2 §4).
    #[test]
    fn clones_on_threads_keep_each_producers_order() {
        const SENDERS: usize = 4;
        const ITEMS: usize = 20_000;
        let (sink, mut receiver) = observation_channel();
        let start = std::sync::Arc::new(std::sync::Barrier::new(SENDERS));
        let senders: Vec<_> = (0..SENDERS)
            .map(|producer| {
                let (sink, start) = (sink.clone(), std::sync::Arc::clone(&start));
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                        .expect("runtime");
                    start.wait();
                    runtime.block_on(async {
                        for sequence in 0..ITEMS {
                            let item = tool(&format!("{producer}:{sequence}"));
                            assert!(sink.send(item, Duration::from_secs(10)).await.is_ok());
                        }
                    });
                })
            })
            .collect();
        drop(sink);
        // Per producer: the next sequence number and the last `at`.
        let mut next = [(0_usize, None); SENDERS];
        while let Some(admitted) = receiver.blocking_recv() {
            let Observation::Progress(marks) = &admitted.item.observation else {
                panic!("not a sent item");
            };
            let (producer, sequence) = marks.tools_started[0].1.split_once(':').unwrap();
            let (producer, sequence): (usize, usize) =
                (producer.parse().unwrap(), sequence.parse().unwrap());
            let (expected, last) = &mut next[producer];
            assert_eq!(sequence, *expected, "lost, repeated or reordered");
            assert!(last.is_none_or(|last| admitted.item.at >= last));
            (*expected, *last) = (sequence + 1, Some(admitted.item.at));
        }
        for sender in senders {
            sender.join().expect("a sender");
        }
        assert!(next.iter().all(|(sent, _)| *sent == ITEMS));
    }

    fn cost(observation: Observation) -> usize {
        item_cost(&ObservationItem {
            at: Instant::now(),
            vendor_turn: None,
            observation,
        })
    }

    /// The byte budget counts every retained variable-size field: the
    /// identity's transcript path, warning data and the late terminal's
    /// contents.
    #[test]
    fn item_cost_counts_every_retained_field() {
        let identity = |transcript: Option<&str>| {
            Observation::IdentityConfirmed(Identity {
                vendor_session_id: "v".to_owned(),
                connection_id: "c".to_owned(),
                transcript: transcript.map(Into::into),
                vendor_version: None,
            })
        };
        let path = "p".repeat(4096);
        assert!(cost(identity(Some(&path))) >= cost(identity(None)) + 4096);

        let warning = |data: Option<serde_json::Value>| {
            Observation::Warning(Warning {
                code: "w",
                message: String::new(),
                data,
            })
        };
        let data = serde_json::json!({"categories": ["x".repeat(4096)]});
        assert!(cost(warning(Some(data))) >= cost(warning(None)) + 4096);

        let late = |big: &str| {
            Observation::LateTerminal(VendorTerminal {
                at: Instant::now(),
                status: VendorTerminalStatus::Failed,
                stop_reason: StopReason::Error,
                vendor_stop_reason: "error".to_owned(),
                vendor_code: Some(big.to_owned()),
                class_hint: None,
                detail: Some(big.to_owned()),
                structured_output: Some(
                    serde_json::value::to_raw_value(&serde_json::json!({ "x": big })).unwrap(),
                ),
                structured_output_unparsed: None,
                steps: None,
                usage: None,
                cost: None,
                vendor: Some(serde_json::value::to_raw_value(&big).unwrap()),
            })
        };
        assert!(cost(late(&"b".repeat(1024))) >= cost(late("")) + 4 * 1024);

        // Critical r1 #13: the acceptance keeps its own copy of the ID.
        let accepted = |id: Option<&str>| {
            Observation::Accepted(super::Acceptance {
                correlation: crate::AcceptanceToken::FIRST,
                vendor_turn_id: id.map(|id| crate::VendorTurnId::try_from(id.to_owned()).unwrap()),
                instance: None,
            })
        };
        let id = "i".repeat(4096);
        assert!(cost(accepted(Some(&id))) >= cost(accepted(None)) + 4096);
    }
}
