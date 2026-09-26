//! Private fake-route wire validation and attribution.

use via_routes::{FakeMessage, FakeStart, RouteError, TerminalStatus, TurnNumber};

#[test]
fn start_wire_schema_is_exact_and_typed() {
    let start = FakeStart::new(
        "fake-session-1".to_owned(),
        TurnNumber::try_from(1).unwrap(),
        "hello".to_owned(),
    )
    .unwrap();
    let json = serde_json::to_value(&start).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"type":"start","id":1,"session_id":"fake-session-1","turn":1,"prompt":"hello"})
    );
    assert!(serde_json::from_value::<FakeStart>(serde_json::json!({"type":"start","id":1,"session_id":"fake-session-1","turn":0,"prompt":"hello"})).is_err());
    assert!(serde_json::from_value::<FakeStart>(serde_json::json!({"type":"start","id":2,"session_id":"fake-session-1","turn":1,"prompt":"hello"})).is_err());
}

#[test]
fn known_malformed_messages_are_protocol_errors() {
    let turn = TurnNumber::try_from(1).unwrap();
    assert!(matches!(FakeMessage::decode(br#"{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","stop_reason":"end_turn"}"#, turn), Err(RouteError::Protocol { .. })));
    assert!(matches!(
        FakeMessage::decode(
            br#"{"type":"accepted","id":99,"vendor_turn_id":"fake-turn-1"}"#,
            turn
        ),
        Err(RouteError::Protocol { .. })
    ));
    assert!(matches!(
        FakeMessage::decode(
            br#"{"type":"text","vendor_turn_id":"fake-turn-2","text":"crossed"}"#,
            turn
        ),
        Err(RouteError::Protocol { .. })
    ));
}

#[test]
fn terminal_and_unknown_notification_keep_their_distinct_evidence() {
    let turn = TurnNumber::try_from(1).unwrap();
    let terminal = FakeMessage::decode(br#"{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"interrupted","final_text":"","stop_reason":"cancelled"}"#, turn).unwrap();
    assert!(matches!(
        terminal,
        FakeMessage::Terminal {
            status: TerminalStatus::Interrupted,
            ..
        }
    ));
    let other = FakeMessage::decode(br#"{"type":"later","data":123}"#, turn).unwrap();
    assert!(matches!(other, FakeMessage::UnknownNotification { .. }));
    assert!(matches!(
        FakeMessage::decode(br#"{"type":"later","id":4}"#, turn),
        Err(RouteError::Protocol { .. })
    ));
}

#[test]
fn unknown_notification_payload_is_bounded_and_marks_loss() {
    let turn = TurnNumber::try_from(1).unwrap();
    let input = format!(r#"{{"type":"later","text":"{}"}}"#, "é".repeat(12_000));
    let message = FakeMessage::decode(input.as_bytes(), turn).unwrap();
    let FakeMessage::UnknownNotification {
        raw_payload,
        truncated,
        ..
    } = message
    else {
        panic!("expected unknown notification");
    };
    assert!(truncated);
    assert!(raw_payload.len() <= via_routes::UNKNOWN_NOTIFICATION_BYTES);
    assert!(raw_payload.is_char_boundary(raw_payload.len()));
}
