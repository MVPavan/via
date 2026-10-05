//! The vendor's capped `stderr.log` (runtime §4, bead via-c2r). The anchor
//! gives the vendor a pipe as stderr. One thread drains it into memory: the
//! first `head` bytes queue for the file, the last `tail` bytes wait in a
//! ring, and everything between is counted and discarded. A second thread
//! writes the queue to the file. Draining never waits on the file, so a
//! vendor never blocks on its stderr. Finishing never waits on it either,
//! beyond a caller's deadline. Once the vendor group ends, the ring is
//! queued, after one marker line when bytes were dropped.

use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Write},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::Instant,
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

/// One vendor's capped stderr, in memory: the bytes queued for the file
/// and the tail's ring. At most the head and the tail are held, the head
/// only while the file's writes are behind.
struct Capped {
    cap: StderrCap,
    /// Head bytes queued so far.
    headed: u64,
    /// Bytes waiting for the file's writer.
    queued: Vec<u8>,
    /// The latest bytes past the head, at most `cap.tail`; allocated at
    /// that bound once the head is full.
    ring: VecDeque<u8>,
    dropped: u64,
    finished: bool,
}

impl Capped {
    fn new(cap: StderrCap) -> Self {
        Self {
            cap,
            headed: 0,
            queued: Vec::new(),
            ring: VecDeque::new(),
            dropped: 0,
            finished: false,
        }
    }

    /// Takes the next drained bytes; after [`Self::finish`] they are
    /// discarded.
    fn write(&mut self, mut bytes: &[u8]) {
        if self.finished {
            return;
        }
        let room = usize::try_from(self.cap.head.saturating_sub(self.headed)).unwrap_or(usize::MAX);
        if room > 0 {
            let (head, rest) = bytes.split_at(room.min(bytes.len()));
            self.queued.extend_from_slice(head);
            self.headed += head.len() as u64;
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

    /// Queues the marker, when bytes were dropped, and the tail, once; the
    /// ring's memory is released.
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.dropped > 0 {
            self.queued
                .extend_from_slice(marker(self.dropped).as_bytes());
        }
        let ring = std::mem::take(&mut self.ring);
        let (front, back) = ring.as_slices();
        self.queued.extend_from_slice(front);
        self.queued.extend_from_slice(back);
    }
}

/// The log's shared state: the capped bytes and whether the writer wrote
/// everything after the finish.
struct State {
    capped: Capped,
    written: bool,
}

/// One vendor's stderr log, shared by its drain, its writer and the anchor.
/// The lock is never held across file I/O.
pub(crate) struct StderrLog {
    state: Mutex<State>,
    wake: Condvar,
}

impl StderrLog {
    fn new(cap: StderrCap) -> Self {
        Self {
            state: Mutex::new(State {
                capped: Capped::new(cap),
                written: false,
            }),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends the log: the marker and the tail are queued for the writer, and
    /// later bytes are discarded. Idempotent; never waits on the file.
    pub(crate) fn finish(&self) {
        self.lock().capped.finish();
        self.wake.notify_all();
    }

    /// [`Self::finish`], then waits until the writer wrote the rest or
    /// `deadline` passed, whichever is first.
    pub(crate) fn finish_by(&self, deadline: Instant) {
        self.finish();
        let mut state = self.lock();
        while !state.written {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            state = self
                .wake
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// The vendor's log over `file`, fed from `pipe` under `cap`: starts its
/// drain and its writer threads.
pub(crate) fn start(
    pipe: io::PipeReader,
    file: File,
    cap: StderrCap,
) -> io::Result<Arc<StderrLog>> {
    let log = Arc::new(StderrLog::new(cap));
    let writer = log.clone();
    std::thread::Builder::new()
        .name("stderr-write".into())
        .spawn(move || write_out(file, &writer))?;
    let drained = log.clone();
    std::thread::Builder::new()
        .name("stderr-drain".into())
        .spawn(move || drain(pipe, &drained))?;
    Ok(log)
}

/// Drains `pipe` into `log` until every writer closed it (the vendor group
/// ended), then finishes `log`. A read error other than an interrupt ends
/// the drain the same way.
fn drain(mut pipe: impl Read, log: &StderrLog) {
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                log.lock().capped.write(&buffer[..read]);
                log.wake.notify_all();
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    log.finish();
}

/// Writes `log`'s queued bytes to `out`, outside the lock, until the log
/// finished and everything queued was written. A failed write is abandoned,
/// never retried: later bytes are taken and discarded, so the queue stays
/// bounded.
fn write_out(mut out: impl Write, log: &StderrLog) {
    let mut failed = false;
    loop {
        let (bytes, last) = {
            let mut state = log.lock();
            while state.capped.queued.is_empty() && !state.capped.finished {
                state = log.wake.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
            (
                std::mem::take(&mut state.capped.queued),
                state.capped.finished,
            )
        };
        if !failed && out.write_all(&bytes).is_err() {
            failed = true;
        }
        if last {
            if !failed {
                let _ = out.flush();
            }
            log.lock().written = true;
            log.wake.notify_all();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, mpsc},
        time::Duration,
    };

    fn log(head: u64, tail: u64) -> Capped {
        Capped::new(StderrCap { head, tail })
    }

    fn feed(capped: &mut Capped, bytes: &[u8], chunk: usize) {
        for part in bytes.chunks(chunk) {
            capped.write(part);
        }
        capped.finish();
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
            assert_eq!(capped.queued, expected, "chunk {chunk}");
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
            assert_eq!(capped.queued, bytes, "length {length}");
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
        let finished = capped.queued.clone();
        capped.write(b"late");
        capped.finish();
        assert_eq!(capped.queued, finished);
    }

    /// A failed file write never stops the writer: it takes and discards
    /// the rest, and reports the log written.
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
        let log = Arc::new(StderrLog::new(StderrCap { head: 4, tail: 4 }));
        let writer = log.clone();
        let thread = std::thread::spawn(move || write_out(Full, &writer));
        for _ in 0..10 {
            log.lock().capped.write(b"abcdef");
            log.wake.notify_all();
        }
        log.finish_by(Instant::now() + Duration::from_secs(5));
        thread.join().unwrap();
        let state = log.lock();
        assert!(state.written && state.capped.queued.is_empty());
    }

    /// The whole path, drain to file: what the vendor wrote, capped, is in
    /// the file once the pipe closes and the finish returns.
    #[test]
    fn a_drained_log_reaches_its_file() {
        let (mut file_reader, file_writer) = io::pipe().unwrap();
        let file = File::from(std::os::fd::OwnedFd::from(file_writer));
        let (vendor_reader, mut vendor_writer) = io::pipe().unwrap();
        let log = start(vendor_reader, file, StderrCap { head: 10, tail: 20 }).unwrap();
        let bytes: Vec<u8> = (0..100_u8).collect();
        vendor_writer.write_all(&bytes).unwrap();
        drop(vendor_writer);
        let mut got = Vec::new();
        file_reader.read_to_end(&mut got).unwrap();
        log.finish_by(Instant::now() + Duration::from_secs(5));
        assert!(log.lock().written);
        let mut expected = bytes[..10].to_vec();
        expected.extend_from_slice(marker(70).as_bytes());
        expected.extend_from_slice(&bytes[80..]);
        assert_eq!(got, expected);
    }

    /// A stalled sink (bead via-c2r fix round 1): the drain keeps reading
    /// the vendor's pipe, and finishing the log returns by its deadline,
    /// while a file write is blocked. The sink is a pipe nobody reads until
    /// the end.
    #[test]
    fn a_stalled_sink_neither_stops_the_drain_nor_blocks_finish() {
        let (mut sink_reader, sink_writer) = io::pipe().unwrap();
        let sink = File::from(std::os::fd::OwnedFd::from(sink_writer));
        let (vendor_reader, mut vendor_writer) = io::pipe().unwrap();
        let log = start(vendor_reader, sink, StderrCap::TURN).unwrap();
        let (wrote, wrote_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let written = vendor_writer.write_all(&vec![b'x'; 1024 * 1024]);
            let _ = wrote.send(written.is_ok());
        });
        let vendor_done = wrote_rx.recv_timeout(Duration::from_secs(2));
        let (finished, finished_rx) = mpsc::channel();
        let stalled = log.clone();
        std::thread::spawn(move || {
            stalled.finish_by(Instant::now() + Duration::from_millis(20));
            let _ = finished.send(());
        });
        let finish_done = finished_rx.recv_timeout(Duration::from_secs(1));
        // Unblocks the threads before asserting.
        std::thread::spawn(move || io::copy(&mut sink_reader, &mut io::sink()));
        assert_eq!(vendor_done, Ok(true), "the vendor's 1 MiB write blocked");
        assert!(finish_done.is_ok(), "finishing waited on the stalled write");
    }
}
