//! The fake route (Task 4 design §2): the private fake vendor's typed
//! messages and the route that drives one turn over Wire.

use serde::{Deserialize, Deserializer, de::IgnoredAny};
use serde_json::value::RawValue;

use crate::{
    MAX_OBSERVATION_BYTES, OutboundMessage, RouteError, SHORT_FIELD_MAX, TurnNumber,
    UNKNOWN_TAG_MAX,
};

mod runtime;

pub use runtime::{
    FakeLateTerminal, FakeRetired, FakeRetiredItem, FakeRoute, FakeRouteResult, FakeTerminal,
    FakeTurn, Lane,
};

/// The one prompt submission of a private fake connection. Wire streams it
/// without a second whole copy of the prompt (Task 4 design §8.3).
pub struct TurnStart {
    session_id: String,
    turn: TurnNumber,
    prompt: String,
    /// The turn's effective values, written before the prompt; empty when
    /// the turn sets none.
    values: serde_json::Map<String, serde_json::Value>,
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
            values: serde_json::Map::new(),
        })
    }

    /// The C2 lane's start (adapter design §3.2): the effective values the
    /// vendor must apply, each a top-level member written before the
    /// prompt. A reserved member name is refused.
    pub fn with_values(
        mut self,
        values: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, &'static str> {
        const RESERVED: [&str; 5] = ["type", "id", "session_id", "turn", "prompt"];
        if values.keys().any(|key| RESERVED.contains(&key.as_str())) {
            return Err("a fake start value cannot replace a start member");
        }
        self.values = values;
        Ok(self)
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
    /// prompt escaped slice by slice; the C2 lane's values come before the
    /// prompt.
    pub fn into_message(self) -> Result<OutboundMessage, RouteError> {
        let unencodable = || RouteError::Protocol {
            turn: self.turn,
            detail: "cannot encode fake start",
        };
        let session_id = serde_json::to_string(&self.session_id).map_err(|_| unencodable())?;
        let mut values = String::new();
        for (key, value) in &self.values {
            let key = serde_json::to_string(key).map_err(|_| unencodable())?;
            let value = serde_json::to_string(value).map_err(|_| unencodable())?;
            values.push_str(&key);
            values.push(':');
            values.push_str(&value);
            values.push(',');
        }
        Ok(OutboundMessage::Start {
            prefix: format!(
                r#"{{"type":"start","id":1,"session_id":{session_id},"turn":{},{values}"prompt":""#,
                self.turn.get()
            )
            .into_bytes(),
            prompt: self.prompt,
            suffix: b"\"}\n".to_vec(),
            escape: escape_json,
        })
    }
}

/// The bytes `text` encodes to inside a JSON string, quotes excluded, as
/// `serde_json` escapes it.
pub(crate) fn escaped_text_len(text: &str) -> usize {
    text.chars()
        .map(|character| match character {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            '\0'..='\u{1f}' => 6,
            _ => character.len_utf8(),
        })
        .sum()
}

/// Appends `slice` as the contents of a JSON string, escaped as `serde_json`
/// writes it.
pub(crate) fn escape_json(slice: &str, piece: &mut Vec<u8>) {
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

/// C2 §2 `ClassHint`, as the fake names it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FakeClassHint {
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

/// A denied action's class, as the fake names it (C1 §5).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FakeDenialKind {
    /// A file write.
    FileWrite,
    /// A command.
    Command,
    /// Network access.
    Network,
    /// Anything else.
    Other,
}

/// A usage sample's components (adapter design AD6). Members VIA does not
/// keep are ignored (C2 A1 tolerance).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct FakeUsage {
    /// The sample key, if any: a keyed sample supersedes an earlier one.
    #[serde(default)]
    pub key: Option<String>,
    /// Input tokens.
    #[serde(default)]
    pub input: Option<u64>,
    /// Cached input tokens.
    #[serde(default)]
    pub cached_input: Option<u64>,
    /// Output tokens.
    #[serde(default)]
    pub output: Option<u64>,
    /// Reasoning output tokens.
    #[serde(default)]
    pub reasoning_output: Option<u64>,
    /// Total tokens.
    #[serde(default)]
    pub total: Option<u64>,
}

/// A vendor-reported cost. Members VIA does not keep are ignored (C2 A1
/// tolerance).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct FakeCost {
    /// US dollars.
    pub usd: f64,
    /// The declared cost scope.
    pub scope: String,
}

/// The C2 terminal fields beyond S1's (adapter design §3.2
/// `VendorTerminal`), each optional.
#[derive(Clone, Debug, Default)]
pub struct TerminalDetails {
    /// The suggested failure class.
    pub class_hint: Option<FakeClassHint>,
    /// A bounded failure detail.
    pub detail: Option<String>,
    /// Structured output, passed through unparsed.
    pub structured_output: Option<Box<RawValue>>,
    /// Steps the vendor counted.
    pub steps: Option<u64>,
    /// The turn aggregate usage.
    pub usage: Option<FakeUsage>,
    /// The vendor's cost.
    pub cost: Option<FakeCost>,
    /// Bounded vendor data, at most [`VENDOR_DATA_MAX`] bytes.
    pub vendor: Option<Box<RawValue>>,
}

/// The version handshake a fake instance writes before it reads the start
/// (adapter design AD7), on profiles that declare one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Handshake {
    /// The instance's version.
    pub vendor_version: Option<String>,
    /// The features it reports.
    pub features: Vec<String>,
    /// The efforts its model catalog accepts, when it reports them (AD18):
    /// a turn effort outside them is `invalid_params(effort)`.
    pub efforts: Option<Vec<String>>,
}

/// C2 §2: the terminal's bounded vendor data is at most 16 KiB.
pub const VENDOR_DATA_MAX: usize = 16 * 1024;

/// A decoded fake message (Task 4 design §2.2): only what progress, the
/// acceptance and the terminal need; everything else is skipped unread.
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
        /// The C2 terminal fields.
        details: Box<TerminalDetails>,
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
    /// A usage sample: the tokens of one model call, keyless unless
    /// `sample.key` is set.
    Usage {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
        /// The call's total tokens.
        total_tokens: u64,
        /// The sample's key and components; `sample.total` is `total_tokens`.
        sample: FakeUsage,
    },
    /// The version handshake (AD7); admitted only before the start.
    Hello(Handshake),
    /// The vendor session identity of this connection (C2 §4
    /// `session.vendor_identity_confirmed`): session-level.
    Identity {
        /// The vendor's session ID.
        vendor_session_id: String,
        /// The vendor transcript path, when known.
        transcript: Option<String>,
    },
    /// An action the vendor's own bound denied.
    Denial {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
        /// The action class.
        kind: FakeDenialKind,
        /// What was denied.
        target: String,
        /// Why.
        reason: String,
    },
    /// A vendor request VIA declined.
    Decline {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
        /// The vendor method.
        vendor_method: String,
        /// A bounded summary.
        summary: String,
        /// Whether the vendor was blocked on it.
        blocking: bool,
    },
    /// The vendor injected Route's steer input into the turn: the reply to
    /// the steer request, paired by ID and vendor turn.
    SteerDelivered {
        /// Fake-scoped vendor turn identifier.
        vendor_turn_id: String,
    },
    /// The vendor closed its session (C2 §4 `session.vendor_closed`):
    /// session-level.
    VendorClosed {
        /// The vendor's reason.
        reason: String,
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
    #[serde(default)]
    class_hint: Option<FakeClassHint>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    structured_output: Option<Box<RawValue>>,
    #[serde(default)]
    steps: Option<u64>,
    #[serde(default)]
    usage: Option<FakeUsage>,
    #[serde(default)]
    cost: Option<FakeCost>,
    #[serde(default)]
    vendor: Option<Box<RawValue>>,
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
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    input: Option<u64>,
    #[serde(default)]
    cached_input: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
    #[serde(default)]
    reasoning_output: Option<u64>,
}

#[derive(Deserialize)]
struct HelloFields {
    #[serde(default)]
    vendor_version: Option<String>,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    efforts: Option<Vec<String>>,
}

/// A known message's text bodies, borrowed raw, for its retained payload
/// (C2 §2): a terminal's final text is emitted as `final_text` pieces, and a
/// `text` message's text is not retained at all.
#[derive(Deserialize)]
struct TextBodies<'a> {
    #[serde(borrow, default)]
    final_text: Option<&'a RawValue>,
    #[serde(borrow, default)]
    text: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct IdentityFields {
    vendor_session_id: String,
    #[serde(default)]
    transcript: Option<String>,
}

#[derive(Deserialize)]
struct DenialFields {
    vendor_turn_id: String,
    kind: FakeDenialKind,
    target: String,
    reason: String,
}

#[derive(Deserialize)]
struct DeclineFields {
    vendor_turn_id: String,
    vendor_method: String,
    summary: String,
    blocking: bool,
}

#[derive(Deserialize)]
struct VendorClosedFields {
    reason: String,
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

/// Whether `actual` names `turn`'s vendor turn or an earlier turn's: a
/// denial may be reported late, for a turn that already ended (AD4). Only
/// the canonical `fake-turn-{n}` counts: Core maps that exact string, so
/// an alias such as `fake-turn-01` is refused (Sol r2 #10).
fn earlier_or_own_vendor_turn(actual: &str, turn: TurnNumber) -> bool {
    actual
        .strip_prefix("fake-turn-")
        .and_then(|number| number.parse::<u32>().ok())
        .filter(|number| actual == format!("fake-turn-{number}"))
        .is_some_and(|number| number >= 1 && number <= turn.get())
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
        if input.len() > MAX_OBSERVATION_BYTES && is_known(&tag.kind) {
            retained_payload_cap(input, &tag.kind, turn)?;
        }
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
            "steer_delivered" => {
                let fields: AcceptedFields = known(input, turn)?;
                short(&[&fields.vendor_turn_id], turn)?;
                if fields.id != STEER_ID || !paired_vendor_turn(&fields.vendor_turn_id, turn) {
                    return Err(protocol(turn, "steer ID does not match control"));
                }
                Self::SteerDelivered {
                    vendor_turn_id: fields.vendor_turn_id,
                }
            }
            "hello" | "identity" | "denial" | "decline" | "vendor_closed" => {
                Self::decode_session(&tag.kind, input, turn)?
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
        short(&[fields.key.as_deref().unwrap_or_default()], turn)?;
        Ok(Self::Usage {
            vendor_turn_id: fields.vendor_turn_id,
            total_tokens: fields.total_tokens,
            sample: FakeUsage {
                key: fields.key,
                input: fields.input,
                cached_input: fields.cached_input,
                output: fields.output,
                reasoning_output: fields.reasoning_output,
                total: Some(fields.total_tokens),
            },
        })
    }

    /// The C2 messages beyond S1's: the handshake, identity, denial,
    /// decline and vendor close (adapter design §3.2, AD7, AD8).
    fn decode_session(kind: &str, input: &[u8], turn: TurnNumber) -> Result<Self, RouteError> {
        Ok(match kind {
            "hello" => {
                let fields: HelloFields = known(input, turn)?;
                let mut kept: Vec<&str> = fields.features.iter().map(String::as_str).collect();
                kept.extend(fields.efforts.iter().flatten().map(String::as_str));
                kept.push(fields.vendor_version.as_deref().unwrap_or_default());
                short(&kept, turn)?;
                Self::Hello(Handshake {
                    vendor_version: fields.vendor_version,
                    features: fields.features,
                    efforts: fields.efforts,
                })
            }
            "identity" => {
                let fields: IdentityFields = known(input, turn)?;
                short(
                    &[
                        &fields.vendor_session_id,
                        fields.transcript.as_deref().unwrap_or_default(),
                    ],
                    turn,
                )?;
                Self::Identity {
                    vendor_session_id: fields.vendor_session_id,
                    transcript: fields.transcript,
                }
            }
            "denial" => {
                let fields: DenialFields = known(input, turn)?;
                short(
                    &[&fields.vendor_turn_id, &fields.target, &fields.reason],
                    turn,
                )?;
                if !earlier_or_own_vendor_turn(&fields.vendor_turn_id, turn) {
                    return Err(protocol(turn, "denial belongs to a later turn"));
                }
                Self::Denial {
                    vendor_turn_id: fields.vendor_turn_id,
                    kind: fields.kind,
                    target: fields.target,
                    reason: fields.reason,
                }
            }
            "decline" => {
                let fields: DeclineFields = known(input, turn)?;
                short(
                    &[
                        &fields.vendor_turn_id,
                        &fields.vendor_method,
                        &fields.summary,
                    ],
                    turn,
                )?;
                require_vendor_turn(
                    &fields.vendor_turn_id,
                    turn,
                    "decline belongs to another turn",
                )?;
                Self::Decline {
                    vendor_turn_id: fields.vendor_turn_id,
                    vendor_method: fields.vendor_method,
                    summary: fields.summary,
                    blocking: fields.blocking,
                }
            }
            _ => {
                let fields: VendorClosedFields = known(input, turn)?;
                short(&[&fields.reason], turn)?;
                Self::VendorClosed {
                    reason: fields.reason,
                }
            }
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
        let usage_key = fields.usage.as_ref().and_then(|usage| usage.key.as_deref());
        short(
            &[
                fields.detail.as_deref().unwrap_or_default(),
                usage_key.unwrap_or_default(),
                fields.cost.as_ref().map_or("", |cost| cost.scope.as_str()),
            ],
            turn,
        )?;
        if fields
            .vendor
            .as_ref()
            .is_some_and(|vendor| vendor.get().len() > VENDOR_DATA_MAX)
        {
            return Err(protocol(turn, "fake vendor data exceeds 16 KiB"));
        }
        Ok(Self::Terminal {
            vendor_turn_id: fields.vendor_turn_id,
            status: fields.status,
            final_text: fields.final_text,
            stop_reason: fields.stop_reason,
            vendor_code: fields.vendor_code,
            details: Box::new(TerminalDetails {
                class_hint: fields.class_hint,
                detail: fields.detail,
                structured_output: fields.structured_output,
                steps: fields.steps,
                usage: fields.usage,
                cost: fields.cost,
                vendor: fields.vendor,
            }),
        })
    }
}

fn protocol(turn: TurnNumber, detail: &'static str) -> RouteError {
    RouteError::Protocol { turn, detail }
}

/// A type tag [`FakeMessage::decode`] decodes as a known message.
fn is_known(kind: &str) -> bool {
    matches!(
        kind,
        "accepted"
            | "text"
            | "terminal"
            | "tool_started"
            | "tool_ended"
            | "usage"
            | "interrupt_ack"
            | "steer_delivered"
            | "hello"
            | "identity"
            | "denial"
            | "decline"
            | "vendor_closed"
    )
}

/// C2 §2: the payload VIA retains or emits whole from a known message is
/// at most 256 KiB encoded, else protocol failure. That is the message
/// less its text body: a terminal's final text goes out in `final_text`
/// pieces, and a `text` message becomes a `model` mark that keeps none of
/// its text; Wire's line cap bounds both.
fn retained_payload_cap(input: &[u8], kind: &str, turn: TurnNumber) -> Result<(), RouteError> {
    let bodies: TextBodies<'_> = serde_json::from_slice(input)
        .map_err(|_| protocol(turn, "malformed known fake message"))?;
    let body = match kind {
        "terminal" => bodies.final_text,
        "text" => bodies.text,
        _ => None,
    };
    let retained = input
        .len()
        .saturating_sub(body.map_or(0, |body| body.get().len()));
    if retained > MAX_OBSERVATION_BYTES {
        return Err(protocol(turn, "fake retained payload exceeds 256 KiB"));
    }
    Ok(())
}

/// The steer request's connection-local ID; the start is 1 and the
/// interrupt 2.
pub(crate) const STEER_ID: u64 = 3;

/// One decoded vendor message.
pub struct RouteMessage {
    /// Typed fake payload.
    pub payload: FakeMessage,
    /// For a `SteerDelivered`, the token of the steer request it reports,
    /// which Route paired it with; never decoded (critical r1 #5).
    pub steer: Option<u64>,
}

#[cfg(test)]
mod tests;
