//! The fake route (Task 4 design §2): the private fake vendor's typed
//! messages and the route that drives one turn over Wire.

use serde::{Deserialize, Deserializer, de::IgnoredAny};

use crate::{OutboundMessage, RouteError, SHORT_FIELD_MAX, TurnNumber, UNKNOWN_TAG_MAX};

mod runtime;

pub use runtime::{FakeRoute, FakeRouteResult};

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
