//! The vendor's capped `stderr.log` (runtime §4, bead via-c2r). The anchor
//! gives the vendor a pipe as stderr and drains it here: the first `head`
//! bytes go straight to the file, the last `tail` bytes wait in a ring, and
//! everything between is counted and discarded. Draining never stops, so a
//! vendor never blocks on its stderr; the ring is flushed, after one marker
//! line when bytes were dropped, once the vendor group ends.

use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Write},
    sync::{Arc, Mutex, PoisonError},
};

use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;

/// How much of one vendor's stderr is kept: the first `head` and the last
/// `tail` bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct StderrCap {
    pub head: u64,
    pub tail: u64,
}

impl StderrCap {
    /// A per-turn process: the first 4 MiB and the last 1 MiB.
    pub(crate) const TURN: Self = Self {
        head: 4 * MIB,
        tail: MIB,
    };
    /// A shared server, which lives for hours: the first and the last 8 MiB.
    pub(crate) const SERVER: Self = Self {
        head: 8 * MIB,
        tail: 8 * MIB,
    };
}

/// The marker line written between the head and the tail when `dropped`
/// bytes were discarded.
pub(crate) fn marker(dropped: u64) -> String {
    format!("\n[via: {dropped} bytes of vendor stderr dropped]\n")
}

/// One vendor's capped log over `out`. Writes to `out` that fail are
/// abandoned, never retried: the drain goes on discarding, so the vendor
/// still never blocks.
pub(crate) struct CappedLog<W: Write> {
    out: W,
    cap: StderrCap,
    /// Head bytes written so far.
    written: u64,
    /// The latest bytes past the head, at most `cap.tail`; allocated at
    /// that bound once the head is full.
    ring: VecDeque<u8>,
    dropped: u64,
    /// A write to `out` failed: nothing more is written.
    failed: bool,
    finished: bool,
}

impl<W: Write> CappedLog<W> {
    pub(crate) fn new(out: W, cap: StderrCap) -> Self {
        Self {
            out,
            cap,
            written: 0,
            ring: VecDeque::new(),
            dropped: 0,
            failed: false,
            finished: false,
        }
    }

    /// Takes the next drained bytes; after [`Self::finish`] they are
    /// discarded.
    pub(crate) fn write(&mut self, mut bytes: &[u8]) {
        if self.finished {
            return;
        }
        let room =
            usize::try_from(self.cap.head.saturating_sub(self.written)).unwrap_or(usize::MAX);
        if room > 0 {
            let (head, rest) = bytes.split_at(room.min(bytes.len()));
            self.emit(head);
            self.written += head.len() as u64;
            bytes = rest;
        }
        if bytes.is_empty() {
            return;
        }
        let tail = usize::try_from(self.cap.tail).unwrap_or(usize::MAX);
        if self.ring.capacity() < tail {
            self.ring.reserve_exact(tail - self.ring.len());
        }
        if bytes.len() >= tail {
            self.dropped += (self.ring.len() + bytes.len() - tail) as u64;
            self.ring.clear();
            self.ring.extend(&bytes[bytes.len() - tail..]);
            return;
        }
        let overflow = (self.ring.len() + bytes.len()).saturating_sub(tail);
        self.ring.drain(..overflow);
        self.dropped += overflow as u64;
        self.ring.extend(bytes);
    }

    /// Writes the marker, when bytes were dropped, and the tail, once; the
    /// ring's memory is released.
    pub(crate) fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.dropped > 0 {
            self.emit(marker(self.dropped).as_bytes());
        }
        let ring = std::mem::take(&mut self.ring);
        let (front, back) = ring.as_slices();
        self.emit(front);
        self.emit(back);
        if !self.failed && self.out.flush().is_err() {
            self.failed = true;
        }
    }

    fn emit(&mut self, bytes: &[u8]) {
        if !self.failed && self.out.write_all(bytes).is_err() {
            self.failed = true;
        }
    }
}

/// The anchor's shared handle on its vendor's log: the drain thread writes
/// it, and the anchor finishes it before its own group KILL.
pub(crate) type SharedLog = Arc<Mutex<CappedLog<File>>>;

/// Finishes `log` (idempotent).
pub(crate) fn finish(log: &SharedLog) {
    log.lock().unwrap_or_else(PoisonError::into_inner).finish();
}

/// Drains `pipe` into `log` until every writer closed it (the vendor group
/// ended), then finishes `log`. A read error other than an interrupt ends
/// the drain the same way.
pub(crate) fn drain(mut pipe: impl Read, log: &SharedLog) {
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => log
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    finish(log);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(head: u64, tail: u64) -> CappedLog<Vec<u8>> {
        CappedLog::new(Vec::new(), StderrCap { head, tail })
    }

    fn feed(log: &mut CappedLog<Vec<u8>>, bytes: &[u8], chunk: usize) {
        for part in bytes.chunks(chunk) {
            log.write(part);
        }
        log.finish();
    }

    /// Past both bounds: exactly the head, the marker with the dropped
    /// count and the tail, whatever the read sizes.
    #[test]
    fn past_the_cap_keeps_head_marker_and_tail() {
        let bytes: Vec<u8> = (0..100_u8).collect();
        for chunk in [1, 3, 7, 16, 64, 100] {
            let mut capped = log(10, 20);
            feed(&mut capped, &bytes, chunk);
            let mut expected = bytes[..10].to_vec();
            expected.extend_from_slice(marker(70).as_bytes());
            expected.extend_from_slice(&bytes[80..]);
            assert_eq!(capped.out, expected, "chunk {chunk}");
            assert!(capped.ring.capacity() == 0, "the ring was not released");
        }
    }

    /// Within the bounds the bytes are kept whole, with no marker.
    #[test]
    fn within_the_cap_is_kept_whole() {
        for length in [0, 5, 10, 25, 30] {
            let bytes: Vec<u8> = (0..length).collect();
            let mut capped = log(10, 20);
            feed(&mut capped, &bytes, 4);
            assert_eq!(capped.out, bytes, "length {length}");
        }
    }

    /// The tail is held in memory only up to its bound, and later writes
    /// are discarded once finished.
    #[test]
    fn the_ring_is_bounded_and_finish_is_once() {
        let mut capped = log(0, 8);
        for _ in 0..100 {
            capped.write(&[1; 5]);
            assert!(capped.ring.len() <= 8);
        }
        assert!(capped.ring.capacity() < 16);
        capped.finish();
        let finished = capped.out.clone();
        capped.write(b"late");
        capped.finish();
        assert_eq!(capped.out, finished);
    }

    /// A failed file write never stops the drain.
    #[test]
    fn a_failed_write_keeps_draining() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::StorageFull))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut capped = CappedLog::new(Full, StderrCap { head: 4, tail: 4 });
        for _ in 0..10 {
            capped.write(b"abcdef");
        }
        capped.finish();
        assert!(capped.failed && capped.finished);
    }
}
