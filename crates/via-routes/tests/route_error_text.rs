//! via-f1q: a route failure's text reaches C1 `failure.message`, so it
//! names the turn as C1 does (its number), never Rust's `Debug` form
//! (`TurnNumber(2)`).

use via_routes::{RouteError, StoreFailure, TurnNumber};

#[test]
fn route_errors_name_the_turn_by_its_number() {
    let turn = TurnNumber::try_from(2).unwrap();
    let errors = [
        RouteError::Protocol {
            turn,
            detail: "the shared connection failed to decode a message",
        },
        RouteError::TransportLost { turn },
        RouteError::ProcessExited { turn },
        RouteError::Overflow { turn },
        RouteError::Store {
            turn,
            kind: StoreFailure::Evidence,
        },
        RouteError::Stopped { turn },
        RouteError::Deadline { turn },
        RouteError::ForceStopped { turn },
        RouteError::ServerLost { turn },
        RouteError::HandshakeRefused { turn, detail: None },
        RouteError::InvalidParam {
            turn,
            field: "model",
        },
    ];
    for error in errors {
        let text = error.to_string();
        assert!(text.contains("in turn 2"), "{text}");
        assert!(!text.contains("TurnNumber"), "{text}");
    }
    assert_eq!(
        RouteError::Protocol {
            turn,
            detail: "the shared connection failed to decode a message",
        }
        .to_string(),
        "protocol error in turn 2: the shared connection failed to decode a message"
    );
    // Pi's profile refusal (packet §4.3) carries VIA's own detail.
    assert_eq!(
        RouteError::HandshakeRefused {
            turn,
            detail: Some("settings key retry is not allowed".to_owned()),
        }
        .to_string(),
        "handshake refused in turn 2: settings key retry is not allowed"
    );
    assert_eq!(
        RouteError::HandshakeRefused { turn, detail: None }.to_string(),
        "handshake refused in turn 2"
    );
}
