//! JSON structure limits (Task 4 design §10.2, amendment A10), checked on a
//! peer's bytes before any serde pass builds a value from them: at most
//! [`MAX_DEPTH`] levels of nesting and [`MAX_NODES`] nodes, where every
//! value and every object key is a node. It lives in the lowest crate so
//! that Wire (for Routes) and Core (for the C1 reader) re-export it.

/// Deepest accepted nesting of arrays and objects; one more fails.
pub const MAX_DEPTH: usize = 64;

/// Most accepted nodes (values and keys) in one document; one more fails.
pub const MAX_NODES: usize = 65_536;

/// Which limit a document passed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitError {
    /// Nesting reached depth `MAX_DEPTH + 1`.
    Depth,
    /// The document holds more than `MAX_NODES` nodes.
    Nodes,
}

/// What a scan of a document within the limits found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scanned {
    /// Deepest nesting; a scalar document has depth 0.
    pub depth: usize,
    /// Values and keys.
    pub nodes: usize,
}

/// Counts one more node; past `MAX_NODES` the scan fails.
fn count(nodes: &mut usize) -> Result<(), LimitError> {
    *nodes += 1;
    if *nodes > MAX_NODES {
        Err(LimitError::Nodes)
    } else {
        Ok(())
    }
}

/// Scans `bytes` once, tracking string boundaries and escapes exactly, and
/// fails as soon as nesting reaches depth 65 or the 65,537th node starts.
/// It checks limits only: malformed JSON is left to the decoder that
/// follows, and on any valid document, or prefix of one, it counts the
/// tokens `serde_json` reads. Linear in the input; no allocation.
pub fn scan(bytes: &[u8]) -> Result<Scanned, LimitError> {
    let mut depth = 0_usize;
    let mut deepest = 0_usize;
    let mut nodes = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    // Inside a number or literal: its later bytes start no node.
    let mut in_scalar = false;
    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => {
                in_scalar = false;
                in_string = true;
                count(&mut nodes)?;
            }
            b'[' | b'{' => {
                in_scalar = false;
                count(&mut nodes)?;
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(LimitError::Depth);
                }
                deepest = deepest.max(depth);
            }
            b']' | b'}' => {
                in_scalar = false;
                depth = depth.saturating_sub(1);
            }
            b',' | b':' | b' ' | b'\t' | b'\n' | b'\r' => in_scalar = false,
            // Any other byte is part of a number or literal (or invalid,
            // which the decoder refuses); a run of them is one node.
            _ => {
                if !in_scalar {
                    in_scalar = true;
                    count(&mut nodes)?;
                }
            }
        }
    }
    Ok(Scanned {
        depth: deepest,
        nodes,
    })
}

/// The kind of one JSON value, read from its text (design §10.2): a
/// free-form C1 member is kept as its raw text and refused by its shape,
/// never built into a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shape {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool,
    /// A number.
    Number,
    /// A string.
    String,
    /// An array; `empty` when it has no element.
    Array {
        /// No element.
        empty: bool,
    },
    /// An object; `empty` when it has no member.
    Object {
        /// No member.
        empty: bool,
    },
}

/// The shape of `text`, one valid JSON value as `serde_json`'s `RawValue`
/// holds it: its first byte names the kind, and for a container the next
/// non-whitespace byte tells whether it is empty.
pub fn shape(text: &str) -> Shape {
    let is_space = |byte: &u8| matches!(byte, b' ' | b'\t' | b'\n' | b'\r');
    let mut bytes = text.as_bytes().iter().filter(|byte| !is_space(byte));
    match bytes.next() {
        Some(b'n') => Shape::Null,
        Some(b't' | b'f') => Shape::Bool,
        Some(b'"') => Shape::String,
        Some(b'[') => Shape::Array {
            empty: bytes.next() == Some(&b']'),
        },
        Some(b'{') => Shape::Object {
            empty: bytes.next() == Some(&b'}'),
        },
        // A number (a valid value has no other first byte).
        _ => Shape::Number,
    }
}

/// `text` as a list of strings, or `None` when it is anything else
/// (design §10.2, §11.1 `require`). Bounded by the request line.
pub fn string_list(text: &str) -> Option<Vec<String>> {
    serde_json::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::{LimitError, Scanned, Shape, scan, shape, string_list};

    #[test]
    fn strings_escapes_and_scalars_count_once() {
        assert_eq!(
            scan(br#"{"a\"[":[1,-2.5e+3,true,null,"x\\"]}"#),
            Ok(Scanned { depth: 2, nodes: 8 })
        );
        assert_eq!(scan(b"").map(|scanned| scanned.nodes), Ok(0));
        assert_eq!(scan("[".repeat(65).as_bytes()), Err(LimitError::Depth));
    }

    /// Design §10.2: a free-form member's kind is read from its text
    /// without building a value.
    #[test]
    fn shape_names_the_kind_of_a_value() {
        assert_eq!(shape("null"), Shape::Null);
        assert_eq!(shape(" true"), Shape::Bool);
        assert_eq!(shape("-1.5e3"), Shape::Number);
        assert_eq!(shape(r#""s""#), Shape::String);
        assert_eq!(shape("[ ]"), Shape::Array { empty: true });
        assert_eq!(shape("[0]"), Shape::Array { empty: false });
        assert_eq!(shape("{ }"), Shape::Object { empty: true });
        assert_eq!(shape(r#"{"k":1}"#), Shape::Object { empty: false });
    }

    #[test]
    fn string_list_expands_a_list_of_strings_only() {
        assert_eq!(
            string_list(r#"["spawn", "steer:partial"]"#),
            Some(vec!["spawn".to_owned(), "steer:partial".to_owned()])
        );
        assert_eq!(string_list("[]"), Some(Vec::new()));
        for other in [r#""spawn""#, "[1]", "null", r#"{"a":"b"}"#, r#"["a",null]"#] {
            assert_eq!(string_list(other), None, "{other}");
        }
    }
}
