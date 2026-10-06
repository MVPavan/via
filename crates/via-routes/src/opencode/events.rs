//! Typed `OpenCode` SSE boundary; only short routing and usage fields survive decode.

use serde_json::Value;

use super::bounds::ID_BYTES;

/// The retained, sanitized part of a vendor error.
#[derive(Clone, Debug, PartialEq)]
pub struct VendorError {
    /// Vendor's error type; its message is never retained.
    pub code: String,
    /// An HTTP-like provider status, when reported.
    pub status: Option<u16>,
}

/// Reported model-call token counters; absent counters remain absent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tokens {
    /// Uncached input.
    pub input: Option<u64>,
    /// Output.
    pub output: Option<u64>,
    /// Reasoning.
    pub reasoning: Option<u64>,
    /// Cached input read.
    pub cache_read: Option<u64>,
    /// Cache writes (kept as vendor data).
    pub cache_write: Option<u64>,
}

/// Inbox lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboxKind {
    /// Accepted.
    Enqueued,
    /// Input joined the running execution.
    Delivered,
    /// Input removed before delivery.
    Cancelled,
    /// Other inbox activity.
    Activity,
}
/// Execution lifecycle; terminal membership is explicit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionKind {
    /// Execution began, without implying ownership.
    Started,
    /// Natural success.
    Succeeded,
    /// Natural failure.
    Failed,
    /// Native interrupt.
    Interrupted,
}
/// Assistant step lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepKind {
    /// Learns assistant correlation.
    Started,
    /// Successful call sample.
    Ended,
    /// Failed call sample.
    Failed,
    /// Other step activity.
    Activity,
}
/// Text lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextKind {
    /// Text began.
    Started,
    /// Model progress.
    Delta,
    /// Final candidate.
    Ended,
    /// Reasoning progress.
    Reasoning,
}
/// Tool lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    /// Learns call correlation.
    InputStarted,
    /// Tool begins.
    Called,
    /// Tool finishes successfully.
    Success,
    /// Tool finishes unsuccessfully.
    Failed,
    /// Other tool activity.
    Activity,
}
/// Interactive vendor request kind (`opencode.md` §11).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InteractiveKind {
    /// Permission request.
    Permission,
    /// Form request.
    Form,
}

/// Decoded vendor payload, excluding echoed prompts and error messages.
#[derive(Clone, Debug, PartialEq)]
pub enum EventData {
    /// Inbox event.
    Inbox {
        /// Lifecycle.
        kind: InboxKind,
        /// Caller input key.
        id: String,
    },
    /// Execution event.
    Execution {
        /// Lifecycle.
        kind: ExecutionKind,
        /// Sanitized failure.
        error: Option<VendorError>,
        /// Interrupt reason.
        reason: Option<String>,
    },
    /// Assistant step.
    Step {
        /// Lifecycle.
        kind: StepKind,
        /// Assistant key.
        assistant_message_id: String,
        /// Finish reason.
        finish: Option<String>,
        /// Call sample.
        tokens: Option<Tokens>,
        /// Reported cost.
        cost: Option<f64>,
    },
    /// Text or reasoning.
    Text {
        /// Lifecycle.
        kind: TextKind,
        /// Assistant key.
        assistant_message_id: String,
        /// Final piece order (§7.3); unused progress ordinals default to zero.
        ordinal: u64,
        /// Text only, never prompt echo.
        text: String,
    },
    /// Tool event.
    Tool {
        /// Lifecycle.
        kind: ToolKind,
        /// Assistant key, if present.
        assistant_message_id: Option<String>,
        /// Tool key.
        call_id: String,
        /// Tool name.
        tool: Option<String>,
        /// Sanitized failure.
        error: Option<VendorError>,
    },
    /// Compaction call sample.
    Compaction {
        /// Input key, else the event ID.
        key: String,
        /// Reported tokens.
        tokens: Option<Tokens>,
        /// Reported cost.
        cost: Option<f64>,
    },
    /// Interactive request; child requests may name a parent's turn.
    Interactive {
        /// Request category.
        kind: InteractiveKind,
        /// Request key.
        id: String,
        /// Sanitized permission action; no resources or submitted values (§11).
        action: Option<String>,
        /// Source assistant key.
        message_id: Option<String>,
        /// Source tool key.
        call_id: Option<String>,
    },
    /// Stream evidence settling an interactive request (`opencode.md` §11).
    InteractiveSettled {
        /// Request category.
        kind: InteractiveKind,
        /// Vendor request key.
        id: String,
    },
    /// Child session birth.
    Created {
        /// Parent session, if any.
        parent_id: Option<String>,
    },
    /// No observation, but session activity.
    Activity {
        /// Assistant correlation, if supplied.
        message_id: Option<String>,
        /// Tool correlation, if supplied.
        call_id: Option<String>,
    },
}

/// A typed SSE event.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Vendor event ID, used only as compaction fallback.
    pub id: Option<String>,
    /// Routing session, including nested form routing.
    pub session_id: Option<String>,
    /// Durable per-session sequence; gaps are diagnostic only.
    pub seq: Option<u64>,
    /// Typed data.
    pub data: EventData,
}

/// Sanitized protocol failure location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// Envelope or JSON cannot identify a safe session.
    Generation,
    /// A known payload or sequence was malformed for this session.
    Session(String),
}

/// Decode one `data:` payload; unknown fields/types survive as activity only.
pub fn decode(bytes: &[u8]) -> Result<Event, DecodeError> {
    // §9: reject structure beyond depth 64 / 65,536 nodes before serde allocates it.
    via_wire::json_limits::scan(bytes).map_err(|_| DecodeError::Generation)?;
    let envelope: Value = serde_json::from_slice(bytes).map_err(|_| DecodeError::Generation)?;
    let kind = envelope
        .get("type")
        .and_then(Value::as_str)
        .ok_or(DecodeError::Generation)?;
    let data = envelope
        .get("data")
        .filter(|value| value.is_object())
        .ok_or(DecodeError::Generation)?;
    let routing = if kind == "form.created" {
        data.get("form").and_then(|form| form.get("sessionID"))
    } else {
        data.get("sessionID")
    };
    let session_id = routing
        .map(|value| {
            value
                .as_str()
                .filter(|id| id.len() <= ID_BYTES)
                .map(str::to_owned)
                .ok_or(DecodeError::Generation)
        })
        .transpose()?;
    let failure = session_id.as_ref().map_or(DecodeError::Generation, |id| {
        DecodeError::Session(id.clone())
    });
    let id = optional_string(&envelope, "id").map_err(|()| failure.clone())?;
    let seq = envelope
        .get("durable")
        .and_then(|value| value.get("seq"))
        .map(|value| value.as_u64().ok_or_else(|| failure.clone()))
        .transpose()?;
    let decoded = payload(kind, data, id.as_deref()).map_err(|()| failure.clone())?;
    if session_id.is_none() && !matches!(decoded, EventData::Activity { .. }) {
        return Err(DecodeError::Generation);
    }
    Ok(Event {
        id,
        session_id,
        seq,
        data: decoded,
    })
}

fn string(data: &Value, key: &str) -> Result<String, ()> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|value| value.len() <= ID_BYTES)
        .map(str::to_owned)
        .ok_or(())
}
fn optional_string(data: &Value, key: &str) -> Result<Option<String>, ()> {
    data.get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .filter(|value| value.len() <= ID_BYTES)
                .map(str::to_owned)
                .ok_or(())
        })
        .transpose()
}
fn counter(data: &Value, key: &str) -> Result<Option<u64>, ()> {
    data.get(key)
        .filter(|value| !value.is_null())
        .map(|value| value.as_u64().ok_or(()))
        .transpose()
}
fn tokens(data: &Value) -> Result<Option<Tokens>, ()> {
    let Some(tokens) = data.get("tokens").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    if !tokens.is_object() {
        return Err(());
    }
    let cache = tokens.get("cache").filter(|value| !value.is_null());
    if cache.is_some_and(|value| !value.is_object()) {
        return Err(());
    }
    Ok(Some(Tokens {
        input: counter(tokens, "input")?,
        output: counter(tokens, "output")?,
        reasoning: counter(tokens, "reasoning")?,
        cache_read: cache
            .map(|cache| counter(cache, "read"))
            .transpose()?
            .flatten(),
        cache_write: cache
            .map(|cache| counter(cache, "write"))
            .transpose()?
            .flatten(),
    }))
}
fn cost(data: &Value) -> Result<Option<f64>, ()> {
    data.get("cost")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .ok_or(())
        })
        .transpose()
}
fn error(data: &Value) -> Result<VendorError, ()> {
    let error = data.get("error").ok_or(())?;
    let status = counter(error, "status")?
        .map(u16::try_from)
        .transpose()
        .map_err(|_| ())?;
    Ok(VendorError {
        code: string(error, "type")?,
        status,
    })
}
fn activity(data: &Value) -> Result<EventData, ()> {
    // Unknown schemas ignore non-string fields, but retained identities stay bounded (§9).
    let key = |name| {
        data.get(name)
            .and_then(Value::as_str)
            .map(|id| (id.len() <= ID_BYTES).then(|| id.to_owned()).ok_or(()))
            .transpose()
    };
    Ok(EventData::Activity {
        message_id: key("assistantMessageID")?,
        call_id: key("id")?,
    })
}
fn inbox_payload(kind: InboxKind, data: &Value) -> Result<EventData, ()> {
    Ok(EventData::Inbox {
        kind,
        id: string(data, "inboxID")?,
    })
}
fn execution_payload(kind: ExecutionKind, data: &Value) -> Result<EventData, ()> {
    let error = if kind == ExecutionKind::Failed {
        Some(error(data)?)
    } else {
        None
    };
    let reason = if kind == ExecutionKind::Interrupted {
        Some(string(data, "reason")?)
    } else {
        None
    };
    Ok(EventData::Execution {
        kind,
        error,
        reason,
    })
}
fn step_payload(kind: StepKind, data: &Value) -> Result<EventData, ()> {
    if kind == StepKind::Failed {
        error(data)?;
    }
    let finish = if kind == StepKind::Ended {
        Some(string(data, "finish")?)
    } else {
        None
    };
    Ok(EventData::Step {
        kind,
        assistant_message_id: string(data, "assistantMessageID")?,
        finish,
        tokens: tokens(data)?,
        cost: cost(data)?,
    })
}
fn text_payload(kind: TextKind, field: Option<&str>, data: &Value) -> Result<EventData, ()> {
    let text = field
        .map(|field| {
            data.get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(())
        })
        .transpose()?
        .unwrap_or_default();
    // §7.3 requires final text ordering; §7.1 progress needs no ordinal.
    let ordinal = counter(data, "ordinal")?;
    let ordinal = if kind == TextKind::Ended {
        ordinal.ok_or(())?
    } else {
        ordinal.unwrap_or_default()
    };
    Ok(EventData::Text {
        kind,
        assistant_message_id: string(data, "assistantMessageID")?,
        ordinal,
        text,
    })
}
fn tool_payload(kind: ToolKind, data: &Value) -> Result<EventData, ()> {
    let error = if kind == ToolKind::Failed {
        Some(error(data)?)
    } else {
        None
    };
    Ok(EventData::Tool {
        kind,
        assistant_message_id: optional_string(data, "assistantMessageID")?,
        call_id: string(data, "id")?,
        tool: optional_string(data, "name")?,
        error,
    })
}
fn permission_payload(data: &Value) -> Result<EventData, ()> {
    let source = data.get("source").filter(|value| !value.is_null());
    if source.is_some_and(|source| !source.is_object()) {
        return Err(());
    }
    Ok(EventData::Interactive {
        kind: InteractiveKind::Permission,
        id: string(data, "id")?,
        action: optional_string(data, "action")?,
        message_id: source
            .map(|source| optional_string(source, "messageID"))
            .transpose()?
            .flatten(),
        call_id: source
            .map(|source| optional_string(source, "id"))
            .transpose()?
            .flatten(),
    })
}
fn payload(kind: &str, data: &Value, event_id: Option<&str>) -> Result<EventData, ()> {
    match kind {
        "session.inbox.enqueued" => inbox_payload(InboxKind::Enqueued, data),
        "session.inbox.delivered" => inbox_payload(InboxKind::Delivered, data),
        "session.inbox.cancelled" => inbox_payload(InboxKind::Cancelled, data),
        "session.inbox.delivery.changed" => inbox_payload(InboxKind::Activity, data),
        "session.execution.started" => execution_payload(ExecutionKind::Started, data),
        "session.execution.succeeded" => execution_payload(ExecutionKind::Succeeded, data),
        "session.execution.failed" => execution_payload(ExecutionKind::Failed, data),
        "session.execution.interrupted" => execution_payload(ExecutionKind::Interrupted, data),
        "session.step.started" => step_payload(StepKind::Started, data),
        "session.step.ended" => step_payload(StepKind::Ended, data),
        "session.step.failed" => step_payload(StepKind::Failed, data),
        "session.step.streamed" => step_payload(StepKind::Activity, data),
        "session.text.started" => text_payload(TextKind::Started, None, data),
        "session.text.delta" => text_payload(TextKind::Delta, Some("delta"), data),
        "session.text.ended" => text_payload(TextKind::Ended, Some("text"), data),
        "session.reasoning.started" => text_payload(TextKind::Reasoning, None, data),
        "session.reasoning.delta" => text_payload(TextKind::Reasoning, Some("delta"), data),
        "session.reasoning.ended" => text_payload(TextKind::Reasoning, Some("text"), data),
        "session.tool.input.started" => tool_payload(ToolKind::InputStarted, data),
        "session.tool.called" => tool_payload(ToolKind::Called, data),
        "session.tool.success" => tool_payload(ToolKind::Success, data),
        "session.tool.failed" => tool_payload(ToolKind::Failed, data),
        "session.tool.input.ended" | "session.tool.progress" => {
            tool_payload(ToolKind::Activity, data)
        }
        "session.compaction.ended" | "session.compaction.failed" => {
            if kind == "session.compaction.failed" {
                error(data)?;
            }
            let key = optional_string(data, "inputID")?
                .or_else(|| event_id.map(str::to_owned))
                .ok_or(())?;
            Ok(EventData::Compaction {
                key,
                tokens: tokens(data)?,
                cost: cost(data)?,
            })
        }
        "permission.asked" => permission_payload(data),
        "permission.replied" => Ok(EventData::InteractiveSettled {
            kind: InteractiveKind::Permission,
            id: string(data, "requestID")?,
        }),
        "form.cancelled" | "form.replied" => Ok(EventData::InteractiveSettled {
            kind: InteractiveKind::Form,
            id: string(data, "id")?,
        }),
        "form.created" => {
            let form = data.get("form").ok_or(())?;
            Ok(EventData::Interactive {
                kind: InteractiveKind::Form,
                id: string(form, "id")?,
                action: None,
                message_id: None,
                call_id: None,
            })
        }
        "session.created" => Ok(EventData::Created {
            parent_id: optional_string(data, "parentID")?,
        }),
        _ => activity(data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(kind: &str, data: Value) -> Vec<u8> {
        let mut envelope = json!({"id":"evt_test", "type":kind, "durable":{"seq":7}});
        envelope["data"] = data;
        serde_json::to_vec(&envelope).unwrap()
    }

    #[test]
    fn oc06_boundary_routes_nested_form_and_ignores_unknown_fields() {
        let decoded = decode(&event(
            "form.created",
            json!({"form":{"id":"frm_one","sessionID":"ses_one","vendorNew":"ignored"}}),
        ))
        .unwrap();
        assert_eq!(decoded.session_id.as_deref(), Some("ses_one"));
        assert_eq!(decoded.seq, Some(7));
        assert!(matches!(
            decoded.data,
            EventData::Interactive {
                kind: InteractiveKind::Form,
                ..
            }
        ));
    }

    #[test]
    fn oc09_event_json_structure_is_bounded_before_decoding() {
        let mut nested = json!(null);
        for _ in 0..65 {
            nested = json!([nested]);
        }
        assert_eq!(
            decode(&event(
                "vendor.future",
                json!({"sessionID":"ses_one","extra":nested})
            )),
            Err(DecodeError::Generation)
        );
        let wide = vec![json!(0); 65_537];
        assert_eq!(
            decode(&event(
                "vendor.future",
                json!({"sessionID":"ses_one","extra":wide})
            )),
            Err(DecodeError::Generation)
        );
    }

    #[test]
    fn oc07_permission_reply_retains_only_its_settlement_key() {
        let decoded = decode(&event(
            "permission.replied",
            json!({"sessionID":"ses_one","requestID":"req_one","reply":"reject"}),
        ))
        .unwrap();
        assert_eq!(
            decoded.data,
            EventData::InteractiveSettled {
                kind: InteractiveKind::Permission,
                id: "req_one".into(),
            }
        );
    }

    #[test]
    fn oc07_form_settlement_retains_only_its_request_key() {
        for kind in ["form.cancelled", "form.replied"] {
            let decoded = decode(&event(
                kind,
                json!({"sessionID":"ses_one","id":"frm_one","answer":{"private":"ignored"}}),
            ))
            .unwrap();
            assert_eq!(
                decoded.data,
                EventData::InteractiveSettled {
                    kind: InteractiveKind::Form,
                    id: "frm_one".into(),
                }
            );
        }
    }

    #[test]
    fn oc09_oversized_short_fields_fail_only_the_decodable_session() {
        let too_long = "x".repeat(1025);
        assert_eq!(
            decode(&event(
                "session.step.started",
                json!({"sessionID":"ses_one","assistantMessageID":too_long}),
            )),
            Err(DecodeError::Session("ses_one".into()))
        );
        assert!(
            decode(&event(
                "session.text.delta",
                json!({"sessionID":"ses_one","assistantMessageID":"msg_one","delta":too_long}),
            ))
            .is_ok(),
            "text is governed by the event bound, not the short-field bound"
        );
    }

    #[test]
    fn oc09_oversized_session_identity_cannot_be_attributed() {
        assert_eq!(
            decode(&event(
                "vendor.future",
                json!({"sessionID":"x".repeat(1025)})
            )),
            Err(DecodeError::Generation)
        );
    }

    #[test]
    fn oc06_boundary_unknown_terminal_suffix_is_only_activity() {
        let decoded = decode(&event(
            "session.execution.future",
            json!({"sessionID":"ses_one","anything":42}),
        ))
        .unwrap();
        assert!(matches!(decoded.data, EventData::Activity { .. }));
    }

    #[test]
    fn oc06_boundary_known_malformed_is_session_protocol_and_nonjson_is_global() {
        assert_eq!(
            decode(&event("session.step.ended", json!({"sessionID":"ses_one"}))),
            Err(DecodeError::Session("ses_one".into()))
        );
        assert_eq!(decode(b"<not-json>"), Err(DecodeError::Generation));
    }

    #[test]
    fn oc06_boundary_unused_ordinals_are_optional_but_final_text_order_is_required() {
        for kind in [
            "session.text.started",
            "session.text.delta",
            "session.reasoning.started",
            "session.reasoning.delta",
            "session.reasoning.ended",
        ] {
            let decoded = decode(&event(
                kind,
                json!({"sessionID":"ses_one","assistantMessageID":"msg_one",
                    "delta":"thinking","text":"thinking"}),
            ));
            assert!(decoded.is_ok(), "{kind}: {decoded:?}");
        }
        assert_eq!(
            decode(&event(
                "session.text.ended",
                json!({"sessionID":"ses_one","assistantMessageID":"msg_one","text":"answer"}),
            )),
            Err(DecodeError::Session("ses_one".into()))
        );
        assert_eq!(
            decode(&event(
                "session.reasoning.delta",
                json!({"sessionID":"ses_one","assistantMessageID":"msg_one",
                    "delta":"thinking","ordinal":"wrong type"}),
            )),
            Err(DecodeError::Session("ses_one".into()))
        );
    }

    #[test]
    fn oc11_boundary_keeps_call_counters_and_sanitizes_error_text() {
        let decoded = decode(&event("session.step.failed", json!({"sessionID":"ses_one","assistantMessageID":"msg_one","error":{"type":"provider.auth","status":403,"message":"vendor-secret"},"tokens":{"input":3,"cache":{"read":4,"write":5}}}))).unwrap();
        assert!(matches!(
            decoded.data,
            EventData::Step {
                tokens: Some(Tokens {
                    input: Some(3),
                    cache_read: Some(4),
                    cache_write: Some(5),
                    ..
                }),
                ..
            }
        ));
        assert!(!format!("{decoded:?}").contains("vendor-secret"));
    }
}
