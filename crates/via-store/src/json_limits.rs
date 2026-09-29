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

#[cfg(test)]
mod tests {
    use super::{LimitError, Scanned, scan};

    #[test]
    fn strings_escapes_and_scalars_count_once() {
        assert_eq!(
            scan(br#"{"a\"[":[1,-2.5e+3,true,null,"x\\"]}"#),
            Ok(Scanned { depth: 2, nodes: 8 })
        );
        assert_eq!(scan(b"").map(|scanned| scanned.nodes), Ok(0));
        assert_eq!(scan("[".repeat(65).as_bytes()), Err(LimitError::Depth));
    }
}
