//! Routes own protocol messages and request pairing; they never choose vendor
//! policy or supervise processes.

use serde::{Deserialize, Deserializer, de::IgnoredAny};
use thiserror::Error;

pub use via_wire::{
    AnchorCohort, CloseRequest, Deadline, ExitReport, OutboundMessage, SendOutcome, TurnNumber,
};

/// Task 4 design §2.2 rule 1: an ID, tool name, type tag, `stop_reason` or
/// `vendor_code` longer than this is `protocol`.
pub const SHORT_FIELD_MAX: usize = 1024;

/// Task 4 design §2.2: an unknown message's type tag is kept up to this.
pub const UNKNOWN_TAG_MAX: usize = 256;

/// C2 A1 bound on one encoded observation payload (Task 4 design §2.2
/// rule 2).
pub const MAX_OBSERVATION_BYTES: usize = 256 * 1024;

/// The one prompt submission of a private fake connection. Wire streams it
/// without a second whole copy of the prompt (Task 4 design §8.3).
pub struct TurnStart {
    session_id: String,
    turn: TurnNumber,
    prompt: String,
}

impl TurnStart {
    /// Creates the only allowed start request for a connection.
    pub fn new(session_id: String, turn: TurnNumber, prompt: String) -> Result<Self, &'static str> {
        if session_id.is_empty() {
            return Err("fake vendor session id cannot be empty");
        }
        Ok(Self {
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

    /// The start as Wire writes it, one JSON line:
    /// `{"type":"start","id":1,"session_id":…,"turn":…,"prompt":"…"}`, the
    /// prompt escaped slice by slice.
    pub fn into_message(self) -> Result<OutboundMessage, RouteError> {
        let session_id =
            serde_json::to_string(&self.session_id).map_err(|_| RouteError::Protocol {
                turn: self.turn,
                detail: "cannot encode fake start",
            })?;
        Ok(OutboundMessage::Start {
            prefix: format!(
                r#"{{"type":"start","id":1,"session_id":{session_id},"turn":{},"prompt":""#,
                self.turn.get()
            )
            .into_bytes(),
            prompt: self.prompt,
            suffix: b"\"}\n".to_vec(),
            escape: escape_json,
        })
    }
}

/// Appends `slice` as the contents of a JSON string, escaped as `serde_json`
/// writes it.
fn escape_json(slice: &str, piece: &mut Vec<u8>) {
    let start = piece.len();
    if serde_json::to_writer(&mut *piece, slice).is_ok() {
        // Drop the quotes around the string.
        piece.remove(start);
        piece.pop();
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

/// A decoded fake message (Task 4 design §2.2): only what progress, the
/// acceptance and the terminal need; everything else is skipped unread.
#[derive(Eq, PartialEq)]
pub enum FakeMessage {
    /// Response to the only start request, paired by ID and vendor turn.
    Accepted {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
    },
    /// Model output: a `model` mark. Its text is not kept; final text comes
    /// from the terminal message.
    Text {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
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
    },
    /// A tool ended inside the turn.
    ToolEnded {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
        /// Vendor tool identifier.
        tool_id: String,
    },
    /// A keyless usage sample: the tokens of one model call.
    Usage {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
        /// The call's total tokens.
        total_tokens: u64,
    },
    /// Acknowledgement of receiving interrupt, not cancellation settlement.
    InterruptAck {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
    },
    /// An unknown id-less message: activity only.
    Unknown {
        /// Its type tag, at most [`UNKNOWN_TAG_MAX`] bytes.
        vendor_type: String,
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
    /// A storage step the turn depends on failed: the evidence folder or
    /// `stderr.log` (Task 4 design §7.2) or a Host journal write (rows 3 and
    /// 4), with its classified outcome [r5.5].
    #[error("fake store failed in turn {turn:?}: {kind:?}")]
    Store {
        /// Affected turn.
        turn: TurnNumber,
        /// The classified failure; [`StoreFailure::latches`] tells Core to latch.
        kind: StoreFailure,
    },
    /// The turn's stop order was honoured (design §2): before launch nothing
    /// started; after it, the group was force-closed at `force_at` under
    /// `close_by` and stdout was drained.
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
    /// force-closed and stdout drained when launched.
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
    /// The turn's evidence folder or `stderr.log` could not be created
    /// before launch: nothing was committed (Task 4 design §7.2).
    Evidence,
    /// A Host journal write was not committed (rows 3 and 4).
    NotCommitted,
    /// The SQLite writer's queue was full: never enqueued, not committed.
    NotEnqueued,
    /// The SQLite writer is gone: uncertain.
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
#[error("{cause}{}", undecoded_note(.undecoded.as_deref()))]
pub struct RouteFailure {
    /// First cause; later cleanup failures never replace it.
    pub cause: RouteError,
    /// Where the message VIA could not decode was kept, or why not (Task 4
    /// design §7.3); part of the failure's message.
    pub undecoded: Option<String>,
    /// Host-confirmed vendor exit when one was observed.
    pub exit: Option<ExitReport>,
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

/// `; <note>` when an undecoded message was kept, else nothing.
fn undecoded_note(note: Option<&str>) -> String {
    note.map(|note| format!("; {note}")).unwrap_or_default()
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
struct TurnFields {
    vendor_turn_id: String,
}

#[derive(Deserialize)]
struct TerminalFields {
    vendor_turn_id: String,
    status: TerminalStatus,
    final_text: String,
    stop_reason: String,
    vendor_code: Option<String>,
}

#[derive(Deserialize)]
struct ToolStartedFields {
    vendor_turn_id: String,
    tool_id: String,
    name: String,
}

#[derive(Deserialize)]
struct ToolEndedFields {
    vendor_turn_id: String,
    tool_id: String,
}

#[derive(Deserialize)]
struct UsageFields {
    vendor_turn_id: String,
    total_tokens: u64,
}

fn known<T: for<'de> Deserialize<'de>>(input: &[u8], turn: TurnNumber) -> Result<T, RouteError> {
    serde_json::from_slice(input).map_err(|_| RouteError::Protocol {
        turn,
        detail: "malformed known fake message",
    })
}

/// Design §2.2 rule 1: every kept short field is at most 1 KiB.
fn short(fields: &[&str], turn: TurnNumber) -> Result<(), RouteError> {
    if fields.iter().all(|field| field.len() <= SHORT_FIELD_MAX) {
        Ok(())
    } else {
        Err(RouteError::Protocol {
            turn,
            detail: "fake short field exceeds 1 KiB",
        })
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
    /// Decodes one bounded fake message and validates its connection-local
    /// ID (Task 4 design §2.2): UTF-8 and the structure limits first, then a
    /// typed decode that skips every field it does not keep, then the short
    /// field bound. Caller still owns duplicate and order checks.
    pub fn decode(input: &[u8], turn: TurnNumber) -> Result<Self, RouteError> {
        if input.len() > via_wire::MAX_STDOUT_MESSAGE_BYTES {
            return Err(RouteError::Protocol {
                turn,
                detail: "fake message exceeds wire cap",
            });
        }
        if std::str::from_utf8(input).is_err() {
            return Err(RouteError::Protocol {
                turn,
                detail: "fake message is not UTF-8",
            });
        }
        via_wire::json_limits::scan(input).map_err(|_| RouteError::Protocol {
            turn,
            detail: "fake message exceeds JSON structure limits",
        })?;
        let tag: Tag = known(input, turn)?;
        short(&[&tag.kind], turn)?;
        let message = match tag.kind.as_str() {
            "accepted" => {
                let fields: AcceptedFields = known(input, turn)?;
                short(&[&fields.vendor_turn_id], turn)?;
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
                let fields: TurnFields = known(input, turn)?;
                short(&[&fields.vendor_turn_id], turn)?;
                require_vendor_turn(&fields.vendor_turn_id, turn, "text belongs to another turn")?;
                Self::Text {
                    vendor_turn_id: fields.vendor_turn_id,
                }
            }
            "terminal" => Self::decode_terminal(input, turn)?,
            "tool_started" => Self::decode_tool_started(input, turn)?,
            "tool_ended" => Self::decode_tool_ended(input, turn)?,
            "usage" => Self::decode_usage(input, turn)?,
            "interrupt_ack" => {
                let fields: AcceptedFields = known(input, turn)?;
                short(&[&fields.vendor_turn_id], turn)?;
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
            _ => Self::decode_unknown(tag, turn)?,
        };
        Ok(message)
    }

    fn decode_tool_started(input: &[u8], turn: TurnNumber) -> Result<Self, RouteError> {
        let fields: ToolStartedFields = known(input, turn)?;
        short(
            &[&fields.vendor_turn_id, &fields.tool_id, &fields.name],
            turn,
        )?;
        require_vendor_turn(
            &fields.vendor_turn_id,
            turn,
            "tool start belongs to another turn",
        )?;
        Ok(Self::ToolStarted {
            vendor_turn_id: fields.vendor_turn_id,
            tool_id: fields.tool_id,
            name: fields.name,
        })
    }

    fn decode_tool_ended(input: &[u8], turn: TurnNumber) -> Result<Self, RouteError> {
        let fields: ToolEndedFields = known(input, turn)?;
        short(&[&fields.vendor_turn_id, &fields.tool_id], turn)?;
        require_vendor_turn(
            &fields.vendor_turn_id,
            turn,
            "tool end belongs to another turn",
        )?;
        Ok(Self::ToolEnded {
            vendor_turn_id: fields.vendor_turn_id,
            tool_id: fields.tool_id,
        })
    }

    fn decode_usage(input: &[u8], turn: TurnNumber) -> Result<Self, RouteError> {
        let fields: UsageFields = known(input, turn)?;
        short(&[&fields.vendor_turn_id], turn)?;
        require_vendor_turn(
            &fields.vendor_turn_id,
            turn,
            "usage belongs to another turn",
        )?;
        Ok(Self::Usage {
            vendor_turn_id: fields.vendor_turn_id,
            total_tokens: fields.total_tokens,
        })
    }

    /// An unknown message keeps only its type tag, at most 256 bytes; one
    /// with an `id` would be an unanswerable request.
    fn decode_unknown(tag: Tag, turn: TurnNumber) -> Result<Self, RouteError> {
        if tag.has_id {
            return Err(RouteError::Protocol {
                turn,
                detail: "unknown id-bearing fake message",
            });
        }
        if tag.kind.len() > UNKNOWN_TAG_MAX {
            return Err(RouteError::Protocol {
                turn,
                detail: "fake type tag exceeds 256 bytes",
            });
        }
        Ok(Self::Unknown {
            vendor_type: tag.kind,
        })
    }

    fn decode_terminal(input: &[u8], turn: TurnNumber) -> Result<Self, RouteError> {
        let fields: TerminalFields = known(input, turn)?;
        short(
            &[
                &fields.vendor_turn_id,
                &fields.stop_reason,
                fields.vendor_code.as_deref().unwrap_or_default(),
            ],
            turn,
        )?;
        require_vendor_turn(
            &fields.vendor_turn_id,
            turn,
            "terminal belongs to another turn",
        )?;
        Ok(Self::Terminal {
            vendor_turn_id: fields.vendor_turn_id,
            status: fields.status,
            final_text: fields.final_text,
            stop_reason: fields.stop_reason,
            vendor_code: fields.vendor_code,
        })
    }
}

/// One decoded vendor message.
#[derive(Eq, PartialEq)]
pub struct RouteMessage {
    /// Typed fake payload.
    pub payload: FakeMessage,
}

mod runtime;

pub use runtime::{FakeRoute, FakeRouteResult};
pub use via_wire::StoreError;
pub use via_wire::{
    CapacityToken, EnvAllowList, PrivateProcessSpec, ProcessOwner, ReprobeReport, RuntimeConfig,
    RuntimeResources, SessionId, WireCleanup, WireError, WireRecovery, WireShutdown,
    WireTurnRecovery,
};

/// Internal hidden-anchor entrypoint forwarded through this architecture layer.
pub fn run_anchor_from_args(args: &[std::ffi::OsString]) -> i32 {
    via_wire::run_anchor_from_args(args)
}
