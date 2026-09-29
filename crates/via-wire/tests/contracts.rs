//! Wire's message boundary tests exercise failure modes before a vendor parser runs.

use via_wire::{
    BoundedBytes, ConnectionId, MAX_STDOUT_MESSAGE_BYTES, RawRef, VendorMessage, WireFailure,
};

#[test]
fn complete_message_includes_lf_in_cap() {
    let mut maximum = vec![b'x'; MAX_STDOUT_MESSAGE_BYTES];
    maximum[MAX_STDOUT_MESSAGE_BYTES - 1] = b'\n';
    assert!(BoundedBytes::try_from_message(maximum).is_ok());
    let mut oversized = vec![b'x'; MAX_STDOUT_MESSAGE_BYTES];
    oversized.push(b'\n');
    assert!(matches!(
        BoundedBytes::try_from_message(oversized),
        Err(WireFailure::MessageTooLarge)
    ));
    assert!(matches!(
        BoundedBytes::try_from_message(b"partial".to_vec()),
        Err(WireFailure::UnterminatedMessage)
    ));
}

#[test]
fn a_message_cannot_point_at_a_different_raw_span() {
    let bytes = BoundedBytes::try_from_message(b"ok\n".to_vec()).unwrap();
    let connection = ConnectionId::try_from("c_01").unwrap();
    let short_ref = RawRef::new(connection.clone(), 20, 2).unwrap();
    assert!(matches!(
        VendorMessage::new(bytes, short_ref),
        Err(WireFailure::RawRangeMismatch)
    ));
    let bytes = BoundedBytes::try_from_message(b"ok\n".to_vec()).unwrap();
    let exact_ref = RawRef::new(connection, 20, 3).unwrap();
    assert!(VendorMessage::new(bytes, exact_ref).is_ok());
}
