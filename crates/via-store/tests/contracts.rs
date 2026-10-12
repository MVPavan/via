//! Store identity validation at the serialization boundary.

use via_store::{SessionId, TurnNumber};

#[test]
fn session_id_rejects_noncanonical_wire_values() {
    for invalid in [
        "s_7f3",
        "S_7f3k9q2mzr4c",
        "s_7f3k9q2mzr4i",
        "s_7f3k9q2mzr4C",
    ] {
        assert!(serde_json::from_str::<SessionId>(&format!("\"{invalid}\"")).is_err());
    }
    let id: SessionId = serde_json::from_str("\"s_7f3k9q2mzr4c\"").unwrap();
    assert_eq!(serde_json::to_string(&id).unwrap(), "\"s_7f3k9q2mzr4c\"");
}

#[test]
fn turn_number_is_checked() {
    assert!(serde_json::from_str::<TurnNumber>("0").is_err());
    let turn: TurnNumber = serde_json::from_str("2").unwrap();
    assert_eq!(turn.get(), 2);
}
