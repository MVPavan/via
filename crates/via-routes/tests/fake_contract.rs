//! Private fake-route wire validation and attribution.

use via_routes::{FakeMessage, OutboundMessage, RouteError, TerminalStatus, TurnNumber, TurnStart};

/// The start's bytes as Wire writes them: the prefix, the prompt escaped in
/// slices of at most 16 KiB cut at character boundaries, the suffix;
/// `None` if it is not a streamed start.
fn streamed(start: TurnStart) -> Option<Vec<u8>> {
    let OutboundMessage::Start {
        prefix,
        prompt,
        suffix,
        escape,
    } = start.into_message().ok()?
    else {
        return None;
    };
    let mut bytes = prefix;
    let mut piece = Vec::new();
    let mut at = 0;
    while at < prompt.len() {
        let mut end = (at + 16 * 1024).min(prompt.len());
        while !prompt.is_char_boundary(end) {
            end -= 1;
        }
        piece.clear();
        escape(&prompt[at..end], &mut piece);
        bytes.extend_from_slice(&piece);
        at = end;
    }
    bytes.extend_from_slice(&suffix);
    Some(bytes)
}

#[test]
fn start_wire_schema_is_exact_and_typed() {
    let turn = TurnNumber::try_from(1).unwrap();
    let prompts = [
        "hello".to_owned(),
        String::new(),
        "quote \" back \\ nl \n tab \t nul \u{0} é😀".repeat(3000),
    ];
    for prompt in prompts {
        let start = TurnStart::new("fake-session-1".to_owned(), turn, prompt.clone()).unwrap();
        let bytes = streamed(start).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(
            bytes.iter().position(|byte| *byte == b'\n'),
            Some(bytes.len() - 1)
        );
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type":"start","id":1,"session_id":"fake-session-1","turn":1,"prompt":prompt})
        );
    }
    assert!(TurnStart::new(String::new(), turn, "hello".to_owned()).is_err());
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
    assert!(matches!(other, FakeMessage::Unknown { .. }));
    assert!(matches!(
        FakeMessage::decode(br#"{"type":"later","id":4}"#, turn),
        Err(RouteError::Protocol { .. })
    ));
}

/// Task 4 design §2.2: an unknown message keeps only its type tag; no
/// part of its payload is copied.
#[test]
fn unknown_notification_keeps_only_its_type_tag() {
    let turn = TurnNumber::try_from(1).unwrap();
    let input = format!(r#"{{"type":"later","text":"{}"}}"#, "é".repeat(12_000));
    let message = FakeMessage::decode(input.as_bytes(), turn).unwrap();
    let FakeMessage::Unknown { vendor_type } = message else {
        panic!("expected unknown notification");
    };
    assert_eq!(vendor_type, "later");
}
