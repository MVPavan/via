//! Stdin for replay: one reader thread owns it and publishes each line, EOF
//! or input error to a queue of at most [`QUEUE`] events, stamped with its
//! arrival. Arrival means the instant the reader publishes the event: the
//! stamp and the push are one step under the queue's lock, and the main
//! thread decides timeouts under the same lock, so no stamped event is ever
//! lost to a timeout. The gap between the kernel delivering the bytes and
//! the reader taking the lock is inherent to any reader.
//!
//! When it publishes a line or EOF the reader also acknowledges it in the
//! progress log (`read <k>` for the *k*th line, `eof`), still under the
//! lock, so the ack is written before the main thread can take the event
//! and a driver can order its next action after the arrival.
//!
//! A line is on time if and only if its arrival is at or before its limit:
//! `within_ms` after the previous step's completion, capped by the run
//! deadline. An expect step completes at its line's arrival.

use std::collections::VecDeque;
use std::io::{self, BufRead, Read};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{MAX_LINE, Progress};

/// Most events queued; the reader waits for room before it reads more.
const QUEUE: usize = 2;
/// Most bytes one read takes: a longest line and its newline.
const MAX_READ: u64 = 1024 * 1024 + 1;

/// What the stdin reader saw.
enum Event {
    /// A complete line, newline included.
    Line(Vec<u8>),
    Eof,
    /// An over-long line, a partial line at EOF, or a read error.
    Error(String),
}

/// A published event and its arrival.
type Stamped = (Instant, Event);

/// The queue between the reader and the main thread. One condvar serves
/// both: the reader waits for room, the main thread for an event.
#[derive(Default)]
struct Shared {
    queue: Mutex<VecDeque<Stamped>>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, VecDeque<Stamped>> {
        // A panicking holder cannot leave the queue inconsistent: each
        // critical section is a single push or pop.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Stdin, as stamped events from the reader thread.
pub(super) struct Input {
    shared: Arc<Shared>,
    /// When EOF arrived, once it has been taken.
    eof: Option<Instant>,
    /// The run deadline, which caps every wait.
    deadline: Instant,
}

/// What the main thread does with the queue at `now`.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Take the front event, whatever its stamp; the caller checks it.
    Take,
    /// Nothing came by the limit.
    TimedOut,
    /// Wait this long for an event.
    Wait(Duration),
}

/// Decides, under the queue's lock, between taking a published event,
/// timing out and waiting. A published event is always taken first.
fn decide(queue: &VecDeque<Stamped>, limit: Instant, now: Instant) -> Decision {
    if !queue.is_empty() {
        Decision::Take
    } else if now >= limit {
        Decision::TimedOut
    } else {
        Decision::Wait(limit - now)
    }
}

/// The limit for an expected line, and its name for the failure message:
/// `within_ms` after `previous`, the previous step's completion, unless the
/// run deadline comes first.
fn limit(previous: Instant, within_ms: Option<u64>, deadline: Instant) -> (Instant, &'static str) {
    match within_ms
        .and_then(|ms| previous.checked_add(Duration::from_millis(ms)))
        .filter(|limit| *limit < deadline)
    {
        Some(limit) => (limit, "within_ms"),
        None => (deadline, "deadline"),
    }
}

impl Input {
    /// Starts the reader thread.
    pub(super) fn start(deadline: Instant, progress: Progress) -> Result<Self, String> {
        let shared = Arc::new(Shared::default());
        let reader = Arc::clone(&shared);
        // Detached on purpose: it may block reading stdin until the process exits.
        thread::Builder::new()
            .spawn(move || read_stdin(&reader, &progress))
            .map_err(|error| format!("cannot start the stdin reader: {error}"))?;
        Ok(Self::new(shared, deadline))
    }

    fn new(shared: Arc<Shared>, deadline: Instant) -> Self {
        Self {
            shared,
            eof: None,
            deadline,
        }
    }

    /// The next event, waiting until `limit` or the run deadline, whichever
    /// is first; `None` if nothing was published by then.
    fn next(&mut self, limit: Instant) -> Option<Stamped> {
        if let Some(at) = self.eof {
            return Some((at, Event::Eof));
        }
        let limit = limit.min(self.deadline);
        let mut queue = self.shared.lock();
        let (at, event) = loop {
            match decide(&queue, limit, Instant::now()) {
                Decision::Take => break queue.pop_front()?,
                Decision::TimedOut => return None,
                Decision::Wait(left) => {
                    queue = self
                        .shared
                        .changed
                        .wait_timeout(queue, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        };
        drop(queue);
        // The reader may be waiting for room.
        self.shared.changed.notify_all();
        if matches!(event, Event::Eof) {
            self.eof = Some(at);
        }
        Some((at, event))
    }

    /// Takes the expected line and its arrival, which is when the expect
    /// step completes. `previous` is the previous step's completion, from
    /// which `within_ms` counts.
    pub(super) fn expect_line(
        &mut self,
        previous: Instant,
        within_ms: Option<u64>,
    ) -> Result<(Instant, Value), String> {
        let (limit, name) = limit(previous, within_ms, self.deadline);
        match self.next(limit) {
            Some((at, Event::Line(bytes))) if at <= limit => serde_json::from_slice(&bytes)
                .map(|line| (at, line))
                .map_err(|error| format!("input line is not JSON: {error}")),
            Some((at, Event::Eof)) if at <= limit => {
                Err("stdin ended before the expected line".to_owned())
            }
            Some((at, Event::Error(error))) if at <= limit => Err(error),
            Some(_) | None => Err(format!("{name} passed before the line arrived")),
        }
    }

    /// Waits for EOF, which must not arrive before `previous`, the
    /// completion of step `number - 1`, and returns its arrival: the
    /// step's completion. A cached EOF is returned again, so consecutive
    /// `await_eof` steps all pass.
    pub(super) fn await_eof(
        &mut self,
        previous: Instant,
        number: usize,
    ) -> Result<Instant, String> {
        match self.next(self.deadline) {
            Some((at, Event::Eof)) if at > self.deadline => {
                Err("deadline passed while awaiting EOF".to_owned())
            }
            Some((at, Event::Eof)) if at < previous => {
                Err(format!("stdin closed before step {} completed", number - 1))
            }
            Some((at, Event::Eof)) => Ok(at),
            Some((_, Event::Line(_))) => Err("unexpected input while awaiting EOF".to_owned()),
            Some((_, Event::Error(error))) => Err(error),
            None => Err("deadline passed while awaiting EOF".to_owned()),
        }
    }

    /// Fails if a line or an input error has already been published. Best
    /// effort: input that comes later, even after the process ends, is not
    /// seen.
    pub(super) fn check_trailing(&mut self) -> Result<(), String> {
        if self.eof.is_some() {
            return Ok(());
        }
        let front = self.shared.lock().pop_front();
        self.shared.changed.notify_all();
        match front {
            Some((at, Event::Eof)) => {
                self.eof = Some(at);
                Ok(())
            }
            Some((_, Event::Line(_) | Event::Error(_))) => {
                Err("unexpected input after the last expect".to_owned())
            }
            None => Ok(()),
        }
    }
}

/// Reads stdin line by line until EOF or an error. It waits for room before
/// each read, reads without the lock, then stamps, publishes and
/// acknowledges a line or EOF in the progress log under it. A failed
/// acknowledgement is published as an input error.
fn read_stdin(shared: &Shared, progress: &Progress) {
    let mut input = io::stdin().lock();
    let mut lines = 0_usize;
    loop {
        let mut queue = shared.lock();
        while queue.len() >= QUEUE {
            queue = shared
                .changed
                .wait(queue)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(queue);
        let mut bytes = Vec::new();
        let read = Read::take(&mut input, MAX_READ).read_until(b'\n', &mut bytes);
        let event = match read {
            Err(error) => Event::Error(format!("cannot read stdin: {error}")),
            Ok(0) => Event::Eof,
            Ok(_) if bytes.last() == Some(&b'\n') => Event::Line(bytes),
            Ok(_) if bytes.len() > MAX_LINE => {
                Event::Error(format!("input line exceeds {MAX_LINE} bytes"))
            }
            Ok(_) => Event::Error("stdin ended within a partial line".to_owned()),
        };
        let ack = match event {
            Event::Line(_) => {
                lines += 1;
                Some(format!("read {lines}"))
            }
            Event::Eof => Some("eof".to_owned()),
            Event::Error(_) => None,
        };
        let last = !matches!(event, Event::Line(_));
        let mut queue = shared.lock();
        queue.push_back((Instant::now(), event));
        // Acknowledged under the lock, so the main thread cannot take the
        // event (and the process cannot end on it) before the ack is written.
        let failed = ack.and_then(|ack| progress.log(&ack).err());
        let stop = last || failed.is_some();
        if let Some(error) = failed {
            queue.push_back((Instant::now(), Event::Error(error)));
        }
        drop(queue);
        shared.changed.notify_all();
        if stop {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{Decision, Event, Input, Shared, Stamped, decide, limit};

    const MS: Duration = Duration::from_millis(1);

    /// An instant safely in the past, so stamps and limits before now are
    /// representable.
    fn past(by: Duration) -> Instant {
        Instant::now()
            .checked_sub(by)
            .expect("the clock is far enough from its origin")
    }

    fn line() -> Event {
        Event::Line(b"{}\n".to_vec())
    }

    /// An input with these events already published, and no reader.
    fn queued(events: Vec<Stamped>, deadline: Instant) -> Input {
        let shared = Shared::default();
        *shared.lock() = VecDeque::from(events);
        Input::new(Arc::new(shared), deadline)
    }

    #[test]
    fn limit_is_within_ms_after_the_previous_completion_unless_the_run_deadline_is_first() {
        let previous = past(10_000 * MS);
        let deadline = previous + 5_000 * MS;
        assert_eq!(
            limit(previous, Some(300), deadline),
            (previous + 300 * MS, "within_ms")
        );
        assert_eq!(
            limit(previous, Some(9_000), deadline),
            (deadline, "deadline")
        );
        assert_eq!(limit(previous, None, deadline), (deadline, "deadline"));
    }

    #[test]
    fn a_published_event_is_taken_even_past_the_limit() {
        let base = past(2_000 * MS);
        let limit = base + 1_000 * MS;
        let queue = VecDeque::from([(base + 999 * MS, line())]);
        assert_eq!(decide(&queue, limit, Instant::now()), Decision::Take);
        assert_eq!(
            decide(&VecDeque::new(), limit, Instant::now()),
            Decision::TimedOut
        );
        assert_eq!(
            decide(&VecDeque::new(), limit, base + 995 * MS),
            Decision::Wait(5 * MS)
        );
    }

    #[test]
    fn step_capped_limit_accepts_early_and_equal_arrivals_and_rejects_late_ones() {
        // The previous step completed 2 s ago, so now is far past the
        // 300 ms limit; only the stamps decide.
        let previous = past(2_000 * MS);
        let deadline = previous + 60_000 * MS;
        for (stamp, on_time) in [
            (previous + 100 * MS, true),
            (previous + 300 * MS, true),
            (previous + 301 * MS, false),
        ] {
            let mut input = queued(vec![(stamp, line())], deadline);
            let result = input.expect_line(previous, Some(300));
            assert_eq!(result.is_ok(), on_time, "{result:?}");
            if !on_time {
                assert_eq!(
                    result,
                    Err("within_ms passed before the line arrived".to_owned())
                );
            }
        }
    }

    #[test]
    fn run_capped_limit_accepts_early_and_equal_arrivals_and_rejects_late_ones() {
        let previous = past(2_000 * MS);
        let deadline = previous + 200 * MS;
        for within_ms in [Some(300), None] {
            for (stamp, on_time) in [
                (previous + 100 * MS, true),
                (deadline, true),
                (deadline + MS, false),
            ] {
                let mut input = queued(vec![(stamp, line())], deadline);
                let result = input.expect_line(previous, within_ms);
                assert_eq!(result.is_ok(), on_time, "{within_ms:?} {result:?}");
                if !on_time {
                    assert_eq!(
                        result,
                        Err("deadline passed before the line arrived".to_owned())
                    );
                }
            }
        }
    }

    #[test]
    fn within_ms_counts_from_the_previous_completion_not_from_the_step() {
        // The step runs now, 2 s after the previous completion; a line
        // 400 ms after that completion is late for a 300 ms limit however
        // late the step itself started.
        let previous = past(2_000 * MS);
        let mut input = queued(vec![(previous + 400 * MS, line())], previous + 60_000 * MS);
        assert!(input.expect_line(previous, Some(300)).is_err());
    }

    #[test]
    fn nothing_published_by_the_limit_times_out_on_its_name() {
        let previous = past(2_000 * MS);
        let mut input = queued(Vec::new(), previous + 60_000 * MS);
        assert_eq!(
            input.expect_line(previous, Some(300)),
            Err("within_ms passed before the line arrived".to_owned())
        );
    }

    #[test]
    fn await_eof_rejects_an_eof_before_the_previous_completion() {
        let base = past(1_000 * MS);
        let previous = base + MS;
        let deadline = previous + 60_000 * MS;
        let mut early = queued(vec![(base, Event::Eof)], deadline);
        assert_eq!(
            early.await_eof(previous, 4),
            Err("stdin closed before step 3 completed".to_owned())
        );
        let mut equal = queued(vec![(previous, Event::Eof)], deadline);
        assert_eq!(equal.await_eof(previous, 4), Ok(previous));
        // The cached EOF completes a second wait at the same instant.
        assert_eq!(equal.await_eof(previous, 5), Ok(previous));
    }

    #[test]
    fn an_expect_completes_at_its_line_arrival() {
        // The line and the EOF were both published before the main thread
        // took the line: the EOF follows the expect's completion.
        let previous = past(2_000 * MS);
        let arrival = previous + 100 * MS;
        let mut input = queued(
            vec![(arrival, line()), (arrival + MS, Event::Eof)],
            previous + 60_000 * MS,
        );
        let (completed, _) = input.expect_line(previous, None).expect("the line");
        assert_eq!(completed, arrival);
        assert_eq!(input.await_eof(completed, 2), Ok(arrival + MS));
    }
}
