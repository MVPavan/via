//! Store identity and raw-range validation at the serialization boundary.

use via_store::{ConnectionId, RawRef, SessionId, TurnNumber};

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
fn turn_number_and_raw_range_are_checked() {
    assert!(serde_json::from_str::<TurnNumber>("0").is_err());
    assert!(RawRef::new(ConnectionId::try_from("c_01").unwrap(), u64::MAX, 2).is_err());
    assert!(RawRef::new(ConnectionId::try_from("c_01").unwrap(), 4, 0).is_err());
    let raw: RawRef =
        serde_json::from_str(r#"{"connection_id":"c_01","offset":4,"len":2}"#).unwrap();
    assert_eq!(raw.end_offset(), 6);
}
