//! Routes own protocol messages and request pairing; they never choose vendor
//! policy or supervise processes.

use serde::{Deserialize, Deserializer, Serialize, de::IgnoredAny};
use thiserror::Error;

pub use via_wire::{
    AnchorCohort, CloseRequest, Deadline, ExitReport, RawRef, SendOutcome, TurnNumber,
};

/// Maximum bytes retained for an unknown fake notification's raw payload.
pub const UNKNOWN_NOTIFICATION_BYTES: usize = 16 * 1024;

/// C2 A1 bound on one encoded observation payload. Text is split by Adapter;
/// any other known payload above it fails the turn as a protocol error.
pub const MAX_OBSERVATION_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum StartTag {
    #[serde(rename = "start")]
    Start,
}

fn start_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    let id = u8::deserialize(deserializer)?;
    if id == 1 {
        Ok(id)
    } else {
        Err(serde::de::Error::custom("start id must be 1"))
    }
}

fn vendor_session_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        Err(serde::de::Error::custom(
            "fake vendor session id cannot be empty",
        ))
    } else {
        Ok(value)
    }
}

/// One typed prompt submission on a private fake connection.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FakeStart {
    #[serde(rename = "type")]
    kind: StartTag,
    #[serde(deserialize_with = "start_id")]
    id: u8,
    #[serde(deserialize_with = "vendor_session_id")]
    session_id: String,
    turn: TurnNumber,
    prompt: String,
}

impl FakeStart {
    /// Creates the only allowed start request for a connection.
    pub fn new(session_id: String, turn: TurnNumber, prompt: String) -> Result<Self, &'static str> {
        if session_id.is_empty() {
            return Err("fake vendor session id cannot be empty");
        }
        Ok(Self {
            kind: StartTag::Start,
            id: 1,
            session_id,
            turn,
            prompt,
        })
    }

    /// Returns the vendor session identifier used across fake child processes.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the canonical turn number.
    pub fn turn(&self) -> TurnNumber {
        self.turn
    }
}

/// Fake terminal status, used as evidence rather than a Core disposition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    /// Vendor reported normal completion.
    Completed,
    /// Vendor reported an interrupted terminal.
    Interrupted,
    /// Vendor reported failure.
    Failed,
}

/// Fake tool completion status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Tool completed.
    Completed,
    /// Tool failed.
    Failed,
    /// Tool was cancelled.
    Cancelled,
}

/// A decoded fake notification or paired response.
#[derive(Eq, PartialEq)]
pub enum FakeMessage {
    /// Response to the only start request, paired by ID and vendor turn.
    Accepted {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
    },
    /// Incremental assistant text.
    Text {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
        /// One text chunk; final text comes from the terminal message.
        text: String,
    },
    /// Authoritative final fake output and vendor status.
    Terminal {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
        /// Vendor status evidence.
        status: TerminalStatus,
        /// Authoritative final output.
        final_text: String,
        /// Vendor stop reason, retained verbatim.
        stop_reason: String,
        /// Optional vendor error code.
        vendor_code: Option<String>,
    },
    /// A tool started inside the turn.
    ToolStarted {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
        /// Vendor tool identifier.
        tool_id: String,
        /// Tool name.
        name: String,
        /// Bounded input summary.
        input_summary: String,
    },
    /// A tool ended inside the turn.
    ToolEnded {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
        /// Vendor tool identifier.
        tool_id: String,
        /// Vendor completion status.
        status: ToolStatus,
        /// Bounded output summary.
        output_summary: String,
        /// Exit code when present.
        exit_code: Option<i32>,
    },
    /// Acknowledgement of receiving interrupt, not cancellation settlement.
    InterruptAck {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
    },
    /// Unknown id-less notification, retained with an explicit truncation bit.
    UnknownNotification {
        /// Original unknown type tag.
        vendor_type: String,
        /// Bounded UTF-8 prefix of the encoded message.
        raw_payload: String,
        /// Whether the payload prefix omitted bytes.
        truncated: bool,
    },
}

/// Error from the typed fake route boundary.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RouteError {
    /// A known message was malformed or a response did not match this turn.
    #[error("fake protocol error in turn {turn:?}: {detail}")]
    Protocol {
        /// Turn whose connection produced the error.
        turn: TurnNumber,
        /// Bounded diagnostic, with no raw vendor message embedded.
        detail: &'static str,
    },
    /// The transport failed while ownership remained with Wire.
    #[error("fake transport lost in turn {turn:?}")]
    TransportLost {
        /// Affected turn.
        turn: TurnNumber,
        /// Last durable raw evidence when available.
        evidence: Option<RawRef>,
    },
    /// Host confirmed the process exited before terminal evidence.
    #[error("fake process exited in turn {turn:?}")]
    ProcessExited {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// A bounded route or observation queue was exhausted.
    #[error("fake route overflow in turn {turn:?}")]
    Overflow {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// A Store write the turn depends on failed: raw evidence (design §7.2
    /// row 6) or a Host journal write (rows 3 and 4), with its classified
    /// outcome [r5.5].
    #[error("fake store failed in turn {turn:?}: {kind:?}")]
    Store {
        /// Affected turn.
        turn: TurnNumber,
        /// The classified failure; [`StoreFailure::latches`] tells Core to latch.
        kind: StoreFailure,
    },
    /// The turn's stop order was honoured (design §2): before launch nothing
    /// started; after it, the group was force-closed at `force_at` under
    /// `close_by` and both pipes were drained.
    #[error("fake turn stopped in turn {turn:?}")]
    Stopped {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// Core's absolute turn deadline elapsed before terminal evidence and exit.
    #[error("fake turn deadline elapsed in turn {turn:?}")]
    Deadline {
        /// Affected turn.
        turn: TurnNumber,
    },
    /// The caller's force stop ended the turn; its private group was
    /// force-closed and both pipes drained to the raw log when launched.
    #[error("fake turn force-stopped in turn {turn:?}")]
    ForceStopped {
        /// Affected turn.
        turn: TurnNumber,
    },
}

/// The classified Store failure behind [`RouteError::Store`] (design §7.1,
/// §7.2 rows 3, 4 and 6 [r5.5]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreFailure {
    /// A raw append or sync failed or the raw queue was full: not committed
    /// (row 6).
    Raw,
    /// A Host journal write was not committed (rows 3 and 4).
    NotCommitted,
    /// The SQLite writer's queue was full: never enqueued, not committed.
    NotEnqueued,
    /// The SQLite writer or raw thread is gone: uncertain.
    WriterLost,
    /// The write's outcome is unknown.
    Uncertain,
}

impl StoreFailure {
    /// Whether the outcome is uncertain, so the daemon latches (design §7.4).
    pub fn latches(self) -> bool {
        matches!(self, Self::WriterLost | Self::Uncertain)
    }
}

/// Why a turn is asked to stop (design §2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopCause {
    /// A caller `cancel`.
    Cancel,
    /// A session `close`.
    Close,
    /// Core's idle deadline.
    IdleDeadline,
    /// The turn's own Store write failed (design §7.2 row 5).
    Store,
}

/// A stop order for one submitted turn (design §2). Core owns the cause and
/// times; Route acts only on `force_at` and `close_by`.
#[derive(Clone, Debug)]
pub struct StopOrder {
    /// Why the turn stops.
    pub cause: StopCause,
    /// Wall time of the request.
    pub requested_at: String,
    /// When Route force-closes a turn with no terminal.
    pub force_at: Deadline,
    /// Absolute bound on the force close and drain.
    pub close_by: Deadline,
}

/// The turn's stop-order watch: `None` until Core orders a stop.
pub type StopWatch = tokio::sync::watch::Receiver<Option<StopOrder>>;

/// A failed route turn: the first typed cause plus the evidence Route still holds
/// after its forced cleanup and bounded drain.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{cause}")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is independent evidence Core weighs separately"
)]
pub struct RouteFailure {
    /// First cause; later cleanup failures never replace it.
    pub cause: RouteError,
    /// Synced vendor message that proved a protocol failure, when one exists.
    pub evidence: Option<RawRef>,
    /// Host-confirmed vendor exit when one was observed.
    pub exit: Option<ExitReport>,
    /// True when bytes read from or written to the vendor are missing from the raw log.
    pub raw_incomplete: bool,
    /// A vendor may have launched: Host sent ARM for this turn.
    pub launched: bool,
    /// Cleanup certainty of Route's forced group close, when it ran one.
    pub cleanup: Option<WireCleanup>,
    /// Host stopped the group while its vendor was live (Host force evidence).
    pub forced: bool,
    /// A Host journal write in the turn's cleanup had an uncertain outcome:
    /// the daemon must latch (design §7.2 row 12).
    pub journal_uncertain: bool,
}

#[derive(Deserialize)]
struct Tag {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "id", deserialize_with = "present")]
    has_id: bool,
}

fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

#[derive(Deserialize)]
struct AcceptedFields {
    id: u64,
    vendor_turn_id: String,
}

#[derive(Deserialize)]
struct TextFields {
    vendor_turn_id: String,
    text: String,
}

#[derive(Deserialize)]
struct TerminalFields {
    vendor_turn_id: String,
    status: TerminalStatus,
    final_text: String,
    stop_reason: String,
    vendor_code: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct ToolStartedFields {
    vendor_turn_id: String,
    tool_id: String,
    name: String,
    input_summary: String,
}

#[derive(Deserialize, Serialize)]
struct ToolEndedFields {
    vendor_turn_id: String,
    tool_id: String,
    status: ToolStatus,
    output_summary: String,
    exit_code: Option<i32>,
}

fn known<T: for<'de> Deserialize<'de>>(input: &[u8], turn: TurnNumber) -> Result<T, RouteError> {
    serde_json::from_slice(input).map_err(|_| RouteError::Protocol {
        turn,
        detail: "malformed known fake message",
    })
}

/// Refuses a known non-text payload whose encoded observation exceeds C2's bound.
fn bounded_payload<T: Serialize>(fields: &T, turn: TurnNumber) -> Result<(), RouteError> {
    match serde_json::to_vec(fields) {
        Ok(encoded) if encoded.len() <= MAX_OBSERVATION_BYTES => Ok(()),
        Ok(_) | Err(_) => Err(RouteError::Protocol {
            turn,
            detail: "fake observation payload exceeds 256 KiB",
        }),
    }
}

fn paired_vendor_turn(actual: &str, turn: TurnNumber) -> bool {
    actual == format!("fake-turn-{}", turn.get())
}

fn require_vendor_turn(
    actual: &str,
    turn: TurnNumber,
    detail: &'static str,
) -> Result<(), RouteError> {
    if paired_vendor_turn(actual, turn) {
        Ok(())
    } else {
        Err(RouteError::Protocol { turn, detail })
    }
}

impl FakeMessage {
    /// Decodes one bounded fake message and validates its connection-local ID.
    /// Caller still owns duplicate/order checks and the global JSON node budget.
    pub fn decode(input: &[u8], turn: TurnNumber) -> Result<Self, RouteError> {
        if input.len() > via_wire::MAX_STDOUT_MESSAGE_BYTES {
            return Err(RouteError::Protocol {
                turn,
                detail: "fake message exceeds wire cap",
            });
        }
        let tag: Tag = known(input, turn)?;
        if tag.kind.len() > 256 {
            return Err(RouteError::Protocol {
                turn,
                detail: "fake type tag exceeds 256 bytes",
            });
        }
        let message = match tag.kind.as_str() {
            "accepted" => {
                let fields: AcceptedFields = known(input, turn)?;
                if fields.id != 1 || !paired_vendor_turn(&fields.vendor_turn_id, turn) {
                    return Err(RouteError::Protocol {
                        turn,
                        detail: "acceptance ID does not match start",
                    });
                }
                Self::Accepted {
                    vendor_turn_id: fields.vendor_turn_id,
                }
            }
            "text" => {
                let fields: TextFields = known(input, turn)?;
                if !paired_vendor_turn(&fields.vendor_turn_id, turn) {
                    return Err(RouteError::Protocol {
                        turn,
                        detail: "text belongs to another turn",
                    });
                }
                Self::Text {
                    vendor_turn_id: fields.vendor_turn_id,
                    text: fields.text,
                }
            }
            "terminal" => {
                let fields: TerminalFields = known(input, turn)?;
                if !paired_vendor_turn(&fields.vendor_turn_id, turn) {
                    return Err(RouteError::Protocol {
                        turn,
                        detail: "terminal belongs to another turn",
                    });
                }
                Self::Terminal {
                    vendor_turn_id: fields.vendor_turn_id,
                    status: fields.status,
                    final_text: fields.final_text,
                    stop_reason: fields.stop_reason,
                    vendor_code: fields.vendor_code,
                }
            }
            "tool_started" => Self::decode_tool_started(input, turn)?,
            "tool_ended" => Self::decode_tool_ended(input, turn)?,
            "interrupt_ack" => {
                let fields: AcceptedFields = known(input, turn)?;
                if fields.id != 2 || !paired_vendor_turn(&fields.vendor_turn_id, turn) {
                    return Err(RouteError::Protocol {
                        turn,
                        detail: "interrupt ID does not match control",
                    });
                }
                Self::InterruptAck {
                    vendor_turn_id: fields.vendor_turn_id,
                }
            }
            _ => Self::decode_unknown(input, turn, tag)?,
        };
        Ok(message)
    }

    fn decode_tool_started(input: &[u8], turn: TurnNumber) -> Result<Self, RouteError> {
        let fields: ToolStartedFields = known(input, turn)?;
        require_vendor_turn(
            &fields.vendor_turn_id,
            turn,
            "tool start belongs to another turn",
        )?;
        bounded_payload(&fields, turn)?;
        Ok(Self::ToolStarted {
            vendor_turn_id: fields.vendor_turn_id,
            tool_id: fields.tool_id,
            name: fields.name,
            input_summary: fields.input_summary,
        })
    }

    fn decode_tool_ended(input: &[u8], turn: TurnNumber) -> Result<Self, RouteError> {
        let fields: ToolEndedFields = known(input, turn)?;
        require_vendor_turn(
            &fields.vendor_turn_id,
            turn,
            "tool end belongs to another turn",
        )?;
        bounded_payload(&fields, turn)?;
        Ok(Self::ToolEnded {
            vendor_turn_id: fields.vendor_turn_id,
            tool_id: fields.tool_id,
            status: fields.status,
            output_summary: fields.output_summary,
            exit_code: fields.exit_code,
        })
    }

    fn decode_unknown(input: &[u8], turn: TurnNumber, tag: Tag) -> Result<Self, RouteError> {
        if tag.has_id {
            return Err(RouteError::Protocol {
                turn,
                detail: "unknown id-bearing fake message",
            });
        }
        let raw = std::str::from_utf8(input).map_err(|_| RouteError::Protocol {
            turn,
            detail: "fake message is not UTF-8",
        })?;
        let mut end = raw.len().min(UNKNOWN_NOTIFICATION_BYTES);
        while !raw.is_char_boundary(end) {
            end -= 1;
        }
        Ok(Self::UnknownNotification {
            vendor_type: tag.kind,
            raw_payload: raw[..end].to_owned(),
            truncated: end < raw.len(),
        })
    }
}

/// A decoded message paired with its durable raw span.
#[derive(Eq, PartialEq)]
pub struct RouteMessage {
    /// Typed fake payload.
    pub payload: FakeMessage,
    /// Exact source bytes retained by Wire/Store first.
    pub raw_ref: RawRef,
}

mod runtime;

pub use runtime::{FakeRoute, FakeRouteResult};
pub use via_wire::StoreError;
pub use via_wire::{
    CapacityToken, ConnectionId, EnvAllowList, PrivateProcessSpec, ProcessOwner, ReprobeReport,
    RuntimeConfig, RuntimeResources, SessionId, WireCleanup, WireError, WireRecovery, WireShutdown,
    WireTurnRecovery,
};

/// Internal hidden-anchor entrypoint forwarded through this architecture layer.
pub fn run_anchor_from_args(args: &[std::ffi::OsString]) -> i32 {
    via_wire::run_anchor_from_args(args)
}
