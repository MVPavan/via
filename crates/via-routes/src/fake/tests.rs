use super::{FakeClassHint, FakeDenialKind, FakeMessage, Handshake, RouteError, TurnNumber};

fn decode(json: &str) -> Result<FakeMessage, RouteError> {
    FakeMessage::decode(json.as_bytes(), TurnNumber::try_from(1).unwrap())
}

fn refused(json: &str) -> Option<&'static str> {
    match decode(json) {
        Err(RouteError::Protocol { detail, .. }) => Some(detail),
        _ => None,
    }
}

/// Adapter design §3.2: the C2 terminal fields decode typed; the vendor
/// data is bounded to 16 KiB and an unknown class hint is a malformed
/// known message.
#[test]
fn c2_terminal_fields_decode_typed_and_bounded() {
    let terminal = decode(
        r#"{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"failed","final_text":"","stop_reason":"max_steps","class_hint":"budget_exceeded","detail":"d","structured_output":{"a":1},"steps":3,"usage":{"input":1,"total":2},"cost":{"usd":0.5,"scope":"turn"},"vendor":{"k":"v"}}"#,
    )
    .unwrap();
    let FakeMessage::Terminal { details, .. } = terminal else {
        panic!("expected a terminal");
    };
    assert_eq!(details.class_hint, Some(FakeClassHint::BudgetExceeded));
    assert_eq!(
        details.structured_output.as_ref().unwrap().get(),
        r#"{"a":1}"#
    );
    assert_eq!(details.steps, Some(3));
    assert_eq!(details.usage.as_ref().unwrap().total, Some(2));
    assert_eq!(details.cost.as_ref().unwrap().scope, "turn");
    assert_eq!(details.vendor.as_ref().unwrap().get(), r#"{"k":"v"}"#);

    let big = format!(
        r#"{{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"","stop_reason":"end_turn","vendor":"{}"}}"#,
        "v".repeat(16 * 1024)
    );
    assert_eq!(refused(&big), Some("fake vendor data exceeds 16 KiB"));
    assert_eq!(
        refused(
            r#"{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"","stop_reason":"end_turn","class_hint":"nope"}"#
        ),
        Some("malformed known fake message")
    );
}

/// The C2 messages beyond S1's decode typed; the steer reply pairs by ID.
#[test]
fn c2_messages_decode_typed() {
    assert!(matches!(
        decode(r#"{"type":"hello","vendor_version":"1.2","features":["x"]}"#).unwrap(),
        FakeMessage::Hello(Handshake { vendor_version: Some(ref version), ref features })
            if version == "1.2" && features == &["x"]
    ));
    assert!(matches!(
        decode(r#"{"type":"identity","vendor_session_id":"v1","transcript":"/t"}"#).unwrap(),
        FakeMessage::Identity { ref vendor_session_id, transcript: Some(_) }
            if vendor_session_id == "v1"
    ));
    assert!(matches!(
        decode(
            r#"{"type":"denial","vendor_turn_id":"fake-turn-1","kind":"network","target":"h","reason":"r"}"#
        )
        .unwrap(),
        FakeMessage::Denial {
            kind: FakeDenialKind::Network,
            ..
        }
    ));
    assert!(matches!(
        decode(
            r#"{"type":"decline","vendor_turn_id":"fake-turn-1","vendor_method":"m","summary":"s","blocking":true}"#
        )
        .unwrap(),
        FakeMessage::Decline { blocking: true, .. }
    ));
    assert!(matches!(
        decode(r#"{"type":"vendor_closed","reason":"bye"}"#).unwrap(),
        FakeMessage::VendorClosed { ref reason } if reason == "bye"
    ));
    assert!(matches!(
        decode(r#"{"type":"steer_delivered","id":3,"vendor_turn_id":"fake-turn-1"}"#).unwrap(),
        FakeMessage::SteerDelivered { .. }
    ));
    assert_eq!(
        refused(r#"{"type":"steer_delivered","id":2,"vendor_turn_id":"fake-turn-1"}"#),
        Some("steer ID does not match control")
    );
    assert!(matches!(
        decode(
            r#"{"type":"usage","vendor_turn_id":"fake-turn-1","total_tokens":5,"key":"k","cached_input":1}"#
        )
        .unwrap(),
        FakeMessage::Usage { total_tokens: 5, ref sample, .. }
            if sample.key.as_deref() == Some("k")
                && sample.cached_input == Some(1)
                && sample.total == Some(5)
    ));
}
