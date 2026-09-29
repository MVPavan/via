//! Wire's message boundary tests exercise failure modes before a vendor parser runs.

use via_wire::{BoundedBytes, MAX_STDOUT_MESSAGE_BYTES, WireFailure};

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
