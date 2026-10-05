//! The stdout message splitter (design §8.2): splits bytes on LF into
//! complete messages of at most its cap each ([`MAX_STDOUT_MESSAGE_BYTES`]
//! unless the connection's bounds say otherwise), LF included. It keeps bytes exact and never interprets them: split UTF-8,
//! invalid UTF-8 and JSON are Route's to decode.

use super::MAX_STDOUT_MESSAGE_BYTES;
use crate::connection::UNDECODED_BYTES;

/// Assembles LF-terminated messages from chunks read in any split.
pub struct LineSplitter {
    /// The unfinished message: at most `cap - 1` bytes.
    assembly: Vec<u8>,
    /// The largest complete message, LF included.
    cap: usize,
}

/// What one pushed chunk ended with.
#[derive(Debug, Eq, PartialEq)]
pub enum Pushed {
    /// Every complete message in the chunk was handed on; any rest is kept.
    Consumed,
    /// The consumer refused a message: the rest of the chunk was not split.
    Refused,
    /// A message crossed the cap: its first [`UNDECODED_BYTES`] bytes. The
    /// splitter keeps nothing more of it.
    TooLarge(Vec<u8>),
}

impl Default for LineSplitter {
    fn default() -> Self {
        Self::new()
    }
}

impl LineSplitter {
    /// An empty splitter with the default cap.
    pub fn new() -> Self {
        Self::within(MAX_STDOUT_MESSAGE_BYTES)
    }

    /// An empty splitter whose messages are at most `cap` bytes, LF
    /// included.
    pub fn within(cap: usize) -> Self {
        Self {
            assembly: Vec::new(),
            cap,
        }
    }

    /// Splits `chunk`, handing each complete message (LF included) to
    /// `accept` in order; `accept` returns `false` to refuse it and stop.
    pub fn push(&mut self, mut chunk: &[u8], mut accept: impl FnMut(Vec<u8>) -> bool) -> Pushed {
        while let Some(index) = chunk.iter().position(|byte| *byte == b'\n') {
            let (line, rest) = chunk.split_at(index + 1);
            if self.assembly.len() + line.len() > self.cap {
                return Pushed::TooLarge(self.prefix(line));
            }
            let message = if self.assembly.is_empty() {
                line.to_vec()
            } else {
                let mut message = std::mem::take(&mut self.assembly);
                message.extend_from_slice(line);
                message
            };
            if !accept(message) {
                return Pushed::Refused;
            }
            chunk = rest;
        }
        // An unfinished message that already fills the cap can only end
        // past it.
        if self.assembly.len() + chunk.len() >= self.cap {
            return Pushed::TooLarge(self.prefix(chunk));
        }
        self.assembly.extend_from_slice(chunk);
        Pushed::Consumed
    }

    /// The unfinished tail at EOF, if any: the in-band end `Unterminated`.
    pub fn finish(self) -> Option<Vec<u8>> {
        (!self.assembly.is_empty()).then_some(self.assembly)
    }

    /// The first bytes of the over-cap message whose rest starts `more`;
    /// the assembly is dropped.
    fn prefix(&mut self, more: &[u8]) -> Vec<u8> {
        let mut prefix = std::mem::take(&mut self.assembly);
        prefix.truncate(UNDECODED_BYTES);
        let room = UNDECODED_BYTES - prefix.len();
        prefix.extend_from_slice(&more[..more.len().min(room)]);
        prefix
    }
}
