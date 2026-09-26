//! C1 strict request shape and additive response-state decoding.

use via_core::{C1TurnState, HelloParams};

#[test]
fn c1_request_rejects_unknown_fields() {
    let known = r#"{"api_version":1,"client_version":"0.1.0","client":"via-cli"}"#;
    assert!(
        serde_json::from_str::<HelloParams>(known)
            .unwrap()
            .validate()
            .is_ok()
    );
    let unknown = r#"{"api_version":1,"client_version":"0.1.0","client":"via-cli","surprise":1}"#;
    assert!(serde_json::from_str::<HelloParams>(unknown).is_err());
    let version = r#"{"api_version":2,"client_version":"0.1.0","client":"via-cli"}"#;
    assert!(
        serde_json::from_str::<HelloParams>(version)
            .unwrap()
            .validate()
            .is_err()
    );
}

#[test]
fn c1_wire_enum_preserves_future_state() {
    let state: C1TurnState = serde_json::from_str("\"paused\"").unwrap();
    assert_eq!(state, C1TurnState::Other("paused".to_owned()));
    assert_eq!(serde_json::to_string(&state).unwrap(), "\"paused\"");
}
