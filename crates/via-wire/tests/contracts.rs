//! Wire's frame boundary tests exercise failure modes before a vendor parser runs.

use via_wire::{BoundedBytes, ConnectionId, Frame, MAX_STDOUT_FRAME_BYTES, RawRef, WireFailure};

#[test]
fn complete_frame_includes_lf_in_cap() {
    let mut maximum = vec![b'x'; MAX_STDOUT_FRAME_BYTES];
    maximum[MAX_STDOUT_FRAME_BYTES - 1] = b'\n';
    assert!(BoundedBytes::try_from_frame(maximum).is_ok());
    let mut oversized = vec![b'x'; MAX_STDOUT_FRAME_BYTES];
    oversized.push(b'\n');
    assert!(matches!(
        BoundedBytes::try_from_frame(oversized),
        Err(WireFailure::FrameTooLarge)
    ));
    assert!(matches!(
        BoundedBytes::try_from_frame(b"partial".to_vec()),
        Err(WireFailure::UnterminatedFrame)
    ));
}

#[test]
fn a_frame_cannot_point_at_a_different_raw_span() {
    let bytes = BoundedBytes::try_from_frame(b"ok\n".to_vec()).unwrap();
    let connection = ConnectionId::try_from("c_01").unwrap();
    let short_ref = RawRef::new(connection.clone(), 20, 2).unwrap();
    assert!(matches!(
        Frame::new(bytes, short_ref),
        Err(WireFailure::RawRangeMismatch)
    ));
    let bytes = BoundedBytes::try_from_frame(b"ok\n".to_vec()).unwrap();
    let exact_ref = RawRef::new(connection, 20, 3).unwrap();
    assert!(Frame::new(bytes, exact_ref).is_ok());
}
