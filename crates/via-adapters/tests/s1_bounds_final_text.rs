//! Task 4 design §2.3 `final_text` (C2 A1 after R2): the Adapter sends a
//! completed final text as pieces cut at the last character whose escaped
//! encoding keeps the whole observation within 256 KiB. Written before the
//! splitter.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use serde_json::json;
use via_adapters::{
    MAX_OBSERVATION_BYTES, encoded_text_len, final_text_pieces, final_text_pieces_owned,
};

/// The observation as encoded on the wire of C2 (`{"type":"final_text",…}`).
fn encoded_piece(piece: &str) -> usize {
    serde_json::to_vec(&json!({"type":"final_text","text":piece}))
        .unwrap()
        .len()
}

/// Design §13.2: a text of six-byte escapes (U+0001 encodes as `\u0001`)
/// mixed with multi-byte characters is cut into pieces, each at most
/// 256 KiB encoded, that concatenate to the text; each piece but the last
/// is full, so no cut is early. The escaped length Core counts is
/// `serde_json`'s.
#[test]
fn s1_bounds_final_text_piece_fits_256_kib() {
    let mut text = String::new();
    while text.len() < 1024 * 1024 - 64 {
        text.push('\u{1}');
        text.push('é');
        text.push('"');
        text.push('😀');
        text.push('a');
    }
    let pieces: Vec<&str> = final_text_pieces(&text).collect();
    assert!(pieces.len() > 4, "{} pieces", pieces.len());
    assert_eq!(pieces.concat(), text);
    for (index, piece) in pieces.iter().enumerate() {
        let encoded = encoded_piece(piece);
        assert!(encoded <= MAX_OBSERVATION_BYTES, "piece {index}: {encoded}");
        if index + 1 < pieces.len() {
            // The next character, a six-byte escape at most, would not fit.
            assert!(
                encoded + 6 > MAX_OBSERVATION_BYTES,
                "piece {index} cut early: {encoded}"
            );
        }
        assert!(!piece.is_empty());
    }
    // One piece of exactly the bound: `{"type":"final_text","text":""}` is
    // 31 bytes of the 256 KiB.
    let exact = "a".repeat(MAX_OBSERVATION_BYTES - 31);
    assert_eq!(
        final_text_pieces(&exact).collect::<Vec<_>>(),
        [exact.as_str()]
    );
    let over = "a".repeat(MAX_OBSERVATION_BYTES - 30);
    assert_eq!(final_text_pieces(&over).count(), 2);
    assert_eq!(final_text_pieces("").count(), 0);
    // Core's inline measure is serde_json's escaped length.
    for sample in [text.as_str(), "", "plain", "\u{7f}\u{1f}\t\n\\/\u{2028}"] {
        assert_eq!(
            encoded_text_len(sample) + 2,
            serde_json::to_string(sample).unwrap().len()
        );
    }
    for code in 0_u32..0x80 {
        let one = char::from_u32(code).unwrap().to_string();
        assert_eq!(
            encoded_text_len(&one) + 2,
            serde_json::to_string(&one).unwrap().len(),
            "{code:#x}"
        );
    }
}

/// Review cfix-1 #2: the owned cut moves the text into the same pieces the
/// borrowed cut makes; one piece is the text itself, an empty text none.
#[test]
fn s1_bounds_final_text_owned_pieces_match() {
    let mut text = String::new();
    while text.len() < 1024 * 1024 + 17 {
        text.push_str("\u{1}é\"😀a");
    }
    let borrowed: Vec<String> = final_text_pieces(&text).map(str::to_owned).collect();
    assert!(borrowed.len() > 4);
    assert_eq!(final_text_pieces_owned(text), borrowed);
    let exact = "a".repeat(MAX_OBSERVATION_BYTES - 31);
    let address = exact.as_ptr();
    let pieces = final_text_pieces_owned(exact);
    assert_eq!(pieces.len(), 1);
    assert_eq!(pieces[0].as_ptr(), address, "moved, not copied");
    assert!(final_text_pieces_owned(String::new()).is_empty());
}
