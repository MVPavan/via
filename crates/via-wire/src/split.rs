//! The stdout message splitter (design §8.2): splits bytes on LF into
//! complete messages of at most its cap each ([`MAX_STDOUT_MESSAGE_BYTES`]
//! unless the connection's bounds say otherwise), LF included. It keeps bytes exact and never interprets them: split UTF-8,
//! invalid UTF-8 and JSON are Route's to decode.

use super::MAX_STDOUT_MESSAGE_BYTES;
use crate::connection::UNDECODED_BYTES;

/// The trailing bytes of a skipped line kept in its record (review
/// cfix-1 C). No production route reads them: Codex fails the shared
/// connection on any over-cap line and keeps only the head as evidence
/// (owner 2026-10-05); a post-release streaming tracker that attributes
/// the line to its turn would replace this tail.
pub const SKIPPED_TAIL_BYTES: usize = 4096;

/// Assembles LF-terminated messages from chunks read in any split.
pub struct LineSplitter {
    /// The unfinished message: at most `cap - 1` bytes.
    assembly: Vec<u8>,
    /// The largest complete message, LF included.
    cap: usize,
    /// The over-cap line being skipped, in [`Self::push_skipping`].
    skipping: Option<Skipped>,
}

/// One line over the cap, skipped to its LF (owner 2026-10-05): its
/// length, its first [`UNDECODED_BYTES`] and its last
/// [`SKIPPED_TAIL_BYTES`] bytes, LF included; nothing else of it is kept.
#[derive(Debug, Eq, PartialEq)]
pub struct Skipped {
    /// The whole line's bytes, LF included.
    pub length: u64,
    /// Its first bytes.
    pub head: Vec<u8>,
    /// Its last bytes.
    pub tail: Vec<u8>,
}

impl Skipped {
    fn new() -> Self {
        Self {
            length: 0,
            head: Vec::new(),
            tail: Vec::new(),
        }
    }

    /// Takes `bytes` of the line: counted, the head filled, the tail kept
    /// at its last [`SKIPPED_TAIL_BYTES`] (amortized: trimmed past twice
    /// that).
    fn absorb(&mut self, bytes: &[u8]) {
        self.length = self
            .length
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let room = UNDECODED_BYTES.saturating_sub(self.head.len());
        self.head.extend_from_slice(&bytes[..bytes.len().min(room)]);
        let keep = &bytes[bytes.len().saturating_sub(SKIPPED_TAIL_BYTES)..];
        self.tail.extend_from_slice(keep);
        if self.tail.len() > 2 * SKIPPED_TAIL_BYTES {
            self.tail.drain(..self.tail.len() - SKIPPED_TAIL_BYTES);
        }
    }

    /// The finished record, its tail trimmed to its bound.
    fn finished(mut self) -> Self {
        if self.tail.len() > SKIPPED_TAIL_BYTES {
            self.tail.drain(..self.tail.len() - SKIPPED_TAIL_BYTES);
        }
        self
    }
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
            skipping: None,
        }
    }

    /// Splits `chunk`, handing each complete message (LF included) to
    /// `accept` in order; `accept` returns `false` to refuse it and stop.
    pub fn push(&mut self, chunk: &[u8], accept: impl FnMut(Vec<u8>) -> bool) -> Pushed {
        self.split(chunk, accept, None::<fn(Skipped) -> bool>)
    }

    /// [`Self::push`], except that a line over the cap is skipped to its
    /// LF, keeping the stream in step, and handed to `skipped` as its
    /// [`Skipped`] record once its LF arrives (owner 2026-10-05): never
    /// [`Pushed::TooLarge`]. `skipped` returns `false` to refuse it and
    /// stop.
    pub fn push_skipping(
        &mut self,
        chunk: &[u8],
        accept: impl FnMut(Vec<u8>) -> bool,
        skipped: impl FnMut(Skipped) -> bool,
    ) -> Pushed {
        self.split(chunk, accept, Some(skipped))
    }

    fn split<S: FnMut(Skipped) -> bool>(
        &mut self,
        mut chunk: &[u8],
        mut accept: impl FnMut(Vec<u8>) -> bool,
        mut skipped: Option<S>,
    ) -> Pushed {
        loop {
            if let Some(skip) = self.skipping.as_mut() {
                let Some(index) = chunk.iter().position(|byte| *byte == b'\n') else {
                    skip.absorb(chunk);
                    return Pushed::Consumed;
                };
                let (line, rest) = chunk.split_at(index + 1);
                skip.absorb(line);
                chunk = rest;
                if let (Some(done), Some(handler)) = (self.skipping.take(), skipped.as_mut())
                    && !handler(done.finished())
                {
                    return Pushed::Refused;
                }
                continue;
            }
            let Some(index) = chunk.iter().position(|byte| *byte == b'\n') else {
                break;
            };
            let (line, rest) = chunk.split_at(index + 1);
            if self.assembly.len() + line.len() > self.cap {
                let Some(handler) = skipped.as_mut() else {
                    return Pushed::TooLarge(self.prefix(line));
                };
                // The whole line is here, its LF included: skipped at once.
                self.begin_skip(line);
                if let Some(done) = self.skipping.take()
                    && !handler(done.finished())
                {
                    return Pushed::Refused;
                }
                chunk = rest;
                continue;
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
            if skipped.is_none() {
                return Pushed::TooLarge(self.prefix(chunk));
            }
            self.begin_skip(chunk);
            return Pushed::Consumed;
        }
        self.assembly.extend_from_slice(chunk);
        Pushed::Consumed
    }

    /// Starts skipping the over-cap line whose assembled start is kept and
    /// whose bytes go on with `more`; the assembly is dropped.
    fn begin_skip(&mut self, more: &[u8]) {
        let mut skip = Skipped::new();
        skip.absorb(&std::mem::take(&mut self.assembly));
        skip.absorb(more);
        self.skipping = Some(skip);
    }

    /// The unfinished line's whole length so far, in bytes: a skipped
    /// line's count, not just the head [`Self::finish`] keeps of it
    /// (review cfix-2).
    pub fn unfinished_length(&self) -> u64 {
        self.skipping.as_ref().map_or_else(
            || u64::try_from(self.assembly.len()).unwrap_or(u64::MAX),
            |skipping| skipping.length,
        )
    }

    /// The unfinished tail at EOF, if any: the in-band end `Unterminated`;
    /// a line being skipped gives its head.
    pub fn finish(self) -> Option<Vec<u8>> {
        if let Some(skipping) = self.skipping {
            return Some(skipping.head);
        }
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
