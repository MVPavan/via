//! Recorded-vendor replay (adapters design §8 item 3).
//!
//! Started as `<dir>/<name>` with `<dir>/<name>.replay.json` beside it, the
//! fake checks its argv, answers `--version`, then runs the fixture's steps:
//! - `expect` takes one stdin line and matches a JSON subset, capturing
//!   values by JSON pointer; its `absent` pointers must not resolve in the
//!   line, and with `within_ms` the line must arrive within that many
//!   milliseconds of the previous step's completion. The line must also
//!   arrive after its causal predecessor was written (causal arrival): an
//!   adapter answers what it has read. The predecessor is the most recent
//!   preceding `emit`, or the earlier emit step that `"after_emit": <step>`
//!   names, where the recording shows the adapter writing the line on that
//!   reply, independently of the emits after it; the fixture's `notes` give
//!   the reason, naming the step. An expect with no earlier emit is
//!   unconstrained;
//! - `emit` writes one verbatim line;
//! - `delay` sleeps;
//! - `await_signal` waits for `SIGINT`, `SIGTERM` or `SIGUSR1` (a test
//!   driver's gate, never a vendor signal). It appends `at <step>` to the
//!   progress log when it starts waiting and `signalled <step>` once it has
//!   consumed the signal, and completes just before that second line, so a
//!   driver that waits for it orders everything it does next after the
//!   step (see [`signals`]). A signal that arrived after the run deadline
//!   fails;
//! - `await_eof` waits for stdin to end and completes at the EOF's arrival;
//!   an input line instead fails, and so does an EOF that arrived before the
//!   previous step completed. A cached EOF satisfies a later `await_eof`;
//! - `spawn_survivor` starts a child in the fake's process group that
//!   outlives the fake and exits on its own after `ms` milliseconds, capped
//!   by the run deadline: a process left in the group after the vendor's
//!   leader exited. It runs `sleep` from `PATH` with stdio detached;
//! - `exit`, only as the last step, writes its `stderr` text (at most 1 KiB)
//!   verbatim and exits with its `code`, which may not be [`FAILED`]. Its
//!   trailing-input check is best effort (below), so every fixture that does
//!   not model a crash seals its input with `await_eof`: as its last step, or
//!   right before its `exit`.
//!
//! `within_ms` is measured at the fake: from the previous step's completion
//! to the line's arrival as this process's reader publishes it, so it
//! includes pipe transfer and scheduling on both sides. A fixture that
//! stands for a C2 deadline uses the deadline plus 250 ms and says so in its
//! notes; the deadline itself is the adapter's obligation, checked by the
//! adapter's own tests with controlled time.
//!
//! Stdin is read by one thread (see [`input`]), which stamps each line, EOF
//! or input error with its arrival: the instant it publishes the event. A
//! line is on time if and only if it arrived at or before its limit:
//! `within_ms` after the previous step's completion, capped by the run
//! deadline. An expect completes at its line's arrival, since the adapter
//! may close stdin as soon as it has written the line. An emit completes
//! just before its write, since the adapter may react as soon as the line
//! is visible; the same instant is the causal bound for later expects.
//!
//! Put `await_eof` right after the step the adapter must wait for (for
//! example the terminal emit), and vendor output that follows the close
//! after it; a correct adapter then cannot fail it. An early close that the
//! reader sees late is missed, never falsely failed; so is a premature line
//! the reader publishes late. A partial line with stdin held open is never
//! a line: `await_eof` then fails at the run deadline, or at EOF as a
//! partial line.
//!
//! Before an `exit` step, and after the last step otherwise, any input line
//! or input error already read fails. This is best effort: input that comes
//! later, even after the process ends, is not detected.
//!
//! An `argv` entry is an exact string or `{"capture": "<name>"}`, which
//! captures that argument. In emit lines and in expected string values,
//! `${name}` is replaced by the capture's text: an argv capture's argument
//! verbatim, a stdin capture's JSON text.
//!
//! `$${` writes a literal `${`.
//!
//! Each start appends its pid as one line to `<dir>/<name>.launches`, so a
//! test can count launches. The progress log `<dir>/<name>.progress` gets
//! one line per `await_signal` event, each in a single append: `at <step>`
//! and `signalled <step>`, with ` lifetime <n>` added in the lifetimes
//! form below. A `<name>.replay.json` may instead be
//! `{"source", "lifetimes": [fixture, …]}`: launch *n*, the *n*th line of
//! the launch log (`--version` probes included), runs lifetime *n*, and a
//! launch past the last lifetime fails. The ordinal is the line's position,
//! read back after the single append, so concurrent launches each get their
//! own.
//!
//! Lines, the fixture, the captures and the whole run are bounded. Any
//! failure exits [`FAILED`], naming the 1-based step where one was running.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use super::contains_expected;
use input::Input;
use signals::Signals;

mod input;
mod signals;

/// Longest line, read or written, in bytes (newline excluded).
const MAX_LINE: usize = 1024 * 1024;
const MAX_STEPS: usize = 10_000;
/// Largest fixture file, checked before it is read.
const MAX_FIXTURE: u64 = 16 * 1024 * 1024;
/// Most distinct capture names, from argv and expect steps together.
const MAX_CAPTURES: usize = 64;
/// Most bytes all captures retain together.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
/// Longest diagnostic written to stderr, prefix and newline included;
/// longer messages are truncated.
const MAX_DIAGNOSTIC: usize = 1024;
/// Longest `stderr` of an `exit` step, checked at load.
const MAX_EXIT_STDERR: usize = 1024;
const DIAGNOSTIC_PREFIX: &str = "fake replay: ";
/// The watchdog's deadline from process start until the fixture is loaded.
const LOAD_LIMIT: Duration = Duration::from_secs(5);
/// How long the deadline's message may take before the process exits anyway.
const MESSAGE_GRACE: Duration = Duration::from_millis(100);
/// Exit code for any replay failure; distinct from the start-request mode's 2.
const FAILED: i32 = 3;
/// Most lifetimes in one fixture file.
const MAX_LIFETIMES: usize = 64;
/// Most `spawn_survivor` steps in one lifetime.
const MAX_SURVIVORS: usize = 8;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    /// Provenance, for example `CC c7`; not interpreted.
    #[serde(default)]
    #[expect(dead_code, reason = "provenance is for readers of the fixture")]
    source: Option<String>,
    /// Free text for readers, such as the reason for each pipelined step.
    #[serde(default)]
    #[expect(dead_code, reason = "notes are for readers of the fixture")]
    notes: Option<String>,
    argv: Vec<Arg>,
    #[serde(default)]
    version: Option<String>,
    deadline_ms: u64,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Arg {
    Exact(String),
    Capture(ArgCapture),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArgCapture {
    capture: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Expect {
        line: Value,
        /// Capture name to a JSON pointer into the received line.
        #[serde(default)]
        capture: BTreeMap<String, String>,
        /// JSON pointers that must not resolve in the received line.
        #[serde(default)]
        absent: Vec<String>,
        /// Most milliseconds the line may take, from the previous step's end.
        #[serde(default)]
        within_ms: Option<u64>,
        /// The earlier emit step this line answers, when it is not the most
        /// recent preceding emit.
        #[serde(default)]
        after_emit: Option<usize>,
    },
    Emit {
        line: String,
    },
    Delay {
        ms: u64,
    },
    AwaitSignal {
        signal: SignalName,
    },
    AwaitEof {},
    /// Leaves a child in the fake's group that exits by itself after `ms`.
    SpawnSurvivor {
        ms: u64,
    },
    /// The last step: writes `stderr` verbatim and exits with `code`.
    Exit {
        code: u8,
        stderr: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
enum SignalName {
    #[serde(rename = "SIGINT")]
    Int,
    #[serde(rename = "SIGTERM")]
    Term,
    #[serde(rename = "SIGUSR1")]
    Usr1,
}

impl SignalName {
    fn text(self) -> &'static str {
        match self {
            Self::Int => "SIGINT",
            Self::Term => "SIGTERM",
            Self::Usr1 => "SIGUSR1",
        }
    }
}

/// The instant just before each emit step's write, by step number.
type Emitted = BTreeMap<usize, Instant>;

/// Captured texts, bounded in aggregate bytes.
#[derive(Default)]
struct Captures {
    texts: BTreeMap<String, String>,
    bytes: usize,
}

impl Captures {
    fn insert(&mut self, name: &str, text: String) -> Result<(), String> {
        let old = self.texts.get(name).map_or(0, String::len);
        let bytes = self.bytes - old + text.len();
        if bytes > MAX_CAPTURE_BYTES {
            return Err(format!(
                "captures would retain more than {MAX_CAPTURE_BYTES} bytes"
            ));
        }
        self.bytes = bytes;
        self.texts.insert(name.to_owned(), text);
        Ok(())
    }
}

/// A message to the watchdog.
enum Arm {
    /// Replaces the deadline.
    Deadline(Instant),
    /// Not replay mode: the watchdog ends without acting.
    Disarm,
}

/// Runs replay mode and exits when a sibling fixture selects it; otherwise
/// returns to the start-request mode.
pub(crate) fn run_if_selected() {
    let started = Instant::now();
    let step = Arc::new(AtomicUsize::new(0));
    // Armed before the fixture lookup, so lookup, loading and their errors
    // are bounded by the load limit until the fixture names its deadline.
    let Some(load_deadline) = started.checked_add(LOAD_LIMIT) else {
        process::exit(FAILED)
    };
    let watchdog = arm(load_deadline, Arc::clone(&step));
    let Some((fixture, launches, progress)) = fixture_paths() else {
        // If the watchdog is gone it can no longer act; nothing to disarm.
        let _ = watchdog.send(Arm::Disarm);
        return;
    };
    let code = match log_launch(&launches)
        .and_then(|launch| replay(&fixture, (launch, &progress), started, &step, &watchdog))
    {
        Ok(code) => code,
        Err(error) => {
            diagnostic(&error);
            FAILED
        }
    };
    process::exit(code)
}

/// The sibling fixture, launch log and progress log of `argv[0]`, when the
/// fake runs in replay mode.
fn fixture_paths() -> Option<(PathBuf, PathBuf, PathBuf)> {
    let argv0 = PathBuf::from(env::args_os().next()?);
    // A bare name was found through PATH; it has no directory to look beside.
    argv0.parent().filter(|dir| !dir.as_os_str().is_empty())?;
    let sibling = |suffix: &str| {
        let mut name = OsString::from(argv0.as_os_str());
        name.push(suffix);
        PathBuf::from(name)
    };
    let fixture = sibling(".replay.json");
    fixture
        .is_file()
        .then(|| (fixture, sibling(".launches"), sibling(".progress")))
}

/// Appends this process's pid as one line, in a single `O_APPEND` write so
/// that concurrent starts never interleave, and returns the line's 1-based
/// position: the lines that end at or before the write's own end offset.
fn log_launch(path: &Path) -> Result<usize, String> {
    let line = format!("{}\n", process::id());
    let fail = |error: io::Error| format!("cannot write the launch log: {error}");
    let mut file = File::options()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
        .map_err(fail)?;
    let written = file.write(line.as_bytes()).map_err(fail)?;
    if written != line.len() {
        return Err("short write to the launch log".to_owned());
    }
    // An append leaves this description's offset at the end of its own
    // write, whatever other starts appended since.
    let end = file.stream_position().map_err(fail)?;
    file.seek(SeekFrom::Start(0)).map_err(fail)?;
    let mut ordinal = 0;
    let mut reader = BufReader::new(file.take(end));
    loop {
        let buffer = reader.fill_buf().map_err(fail)?;
        if buffer.is_empty() {
            break;
        }
        ordinal += buffer.split(|byte| *byte == b'\n').count() - 1;
        let consumed = buffer.len();
        reader.consume(consumed);
    }
    Ok(ordinal)
}

/// Where `await_signal` reports progress: the log and the suffix that
/// names this launch's lifetime, if any.
struct Progress<'a> {
    path: &'a Path,
    suffix: String,
}

impl Progress<'_> {
    /// Appends `<event> <step><suffix>` as one line, in a single write.
    fn log(&self, event: &str, step: usize) -> Result<(), String> {
        let line = format!("{event} {step}{}\n", self.suffix);
        let fail = |error: io::Error| format!("cannot write the progress log: {error}");
        let mut file = File::options()
            .append(true)
            .create(true)
            .open(self.path)
            .map_err(fail)?;
        let written = file.write(line.as_bytes()).map_err(fail)?;
        if written == line.len() {
            Ok(())
        } else {
            Err("short write to the progress log".to_owned())
        }
    }
}

fn replay(
    fixture: &Path,
    (launch, progress): (usize, &Path),
    started: Instant,
    step: &AtomicUsize,
    watchdog: &SyncSender<Arm>,
) -> Result<i32, String> {
    let (fixture, lifetimes) = load(fixture, launch)?;
    let progress = Progress {
        path: progress,
        suffix: if lifetimes {
            format!(" lifetime {launch}")
        } else {
            String::new()
        },
    };
    let deadline = started
        .checked_add(Duration::from_millis(fixture.deadline_ms))
        .ok_or("deadline_ms is out of range")?;
    watchdog
        .send(Arm::Deadline(deadline))
        .map_err(|_| "the watchdog is gone".to_owned())?;
    let mut captures = Captures::default();
    let args = check_argv(&fixture.argv, &mut captures)?;
    if args == ["--version"] {
        let version = fixture
            .version
            .ok_or("fixture has no version for --version")?;
        return write_line(&version).map(|()| 0);
    }
    // Handlers are installed before the first step so an early signal is held
    // for its step instead of killing the process.
    let names: BTreeSet<SignalName> = fixture
        .steps
        .iter()
        .filter_map(|step| match step {
            Step::AwaitSignal { signal } => Some(*signal),
            Step::Expect { .. }
            | Step::Emit { .. }
            | Step::Delay { .. }
            | Step::AwaitEof {}
            | Step::SpawnSurvivor { .. }
            | Step::Exit { .. } => None,
        })
        .collect();
    let signals = Signals::start(names, deadline)?;
    // Load allows an exit step only as the last step.
    let exit = match fixture.steps.last() {
        Some(Step::Exit { code, .. }) => Some(i32::from(*code)),
        _ => None,
    };
    let count = fixture.steps.len();
    // Taken before the reader starts, so no input can arrive before it.
    let mut previous = Instant::now();
    let mut emitted = Emitted::new();
    let mut input = Input::start(deadline)?;
    for (index, current) in fixture.steps.into_iter().enumerate() {
        let number = index + 1;
        step.store(number, Ordering::SeqCst);
        if matches!(current, Step::Exit { .. }) {
            input
                .check_trailing()
                .map_err(|error| format!("step {number}: {error}"))?;
        }
        let emits = matches!(current, Step::Emit { .. });
        previous = run_step(
            current,
            &Context {
                number,
                previous,
                emitted: &emitted,
                deadline,
            },
            &mut input,
            &mut captures,
            (&signals, &progress),
        )
        .map_err(|error| format!("step {number}: {error}"))?;
        if emits {
            emitted.insert(number, previous);
        }
    }
    if exit.is_none() {
        input
            .check_trailing()
            .map_err(|error| format!("after step {count}: {error}"))?;
    }
    Ok(exit.unwrap_or(0))
}

/// The other fixture file shape: one fixture per launch, in launch order.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lifetimes {
    #[serde(default)]
    #[expect(dead_code, reason = "provenance is for readers of the fixture")]
    source: Option<String>,
    lifetimes: Vec<Fixture>,
}

/// Loads the fixture that launch `launch` (1-based) runs: the file's only
/// fixture, or its lifetime `launch`, and whether the file has lifetimes.
fn load(path: &Path, launch: usize) -> Result<(Fixture, bool), String> {
    let file = File::open(path).map_err(|error| format!("cannot open fixture: {error}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_FIXTURE + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read fixture: {error}"))?;
    if bytes.len() as u64 > MAX_FIXTURE {
        return Err(format!("fixture exceeds {MAX_FIXTURE} bytes"));
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid fixture: {error}"))?;
    let invalid = |error: serde_json::Error| format!("invalid fixture: {error}");
    let lifetimes = value.get("lifetimes").is_some();
    let fixture = if lifetimes {
        let Lifetimes { lifetimes, .. } = Lifetimes::deserialize(value).map_err(invalid)?;
        if lifetimes.len() > MAX_LIFETIMES {
            return Err(format!("fixture has more than {MAX_LIFETIMES} lifetimes"));
        }
        let count = lifetimes.len();
        lifetimes
            .into_iter()
            .nth(launch.saturating_sub(1))
            .ok_or_else(|| format!("launch {launch} has no lifetime: the fixture has {count}"))?
    } else {
        Fixture::deserialize(value).map_err(invalid)?
    };
    check(fixture).map(|fixture| (fixture, lifetimes))
}

/// Checks one fixture's bounds and step placement.
fn check(fixture: Fixture) -> Result<Fixture, String> {
    if fixture.steps.len() > MAX_STEPS {
        return Err(format!("fixture has more than {MAX_STEPS} steps"));
    }
    let mut names: Vec<&str> = fixture
        .argv
        .iter()
        .filter_map(|arg| match arg {
            Arg::Capture(ArgCapture { capture }) => Some(capture.as_str()),
            Arg::Exact(_) => None,
        })
        .collect();
    for (index, step) in fixture.steps.iter().enumerate() {
        match step {
            Step::Expect {
                capture,
                after_emit,
                ..
            } => {
                if let Some(emit) = after_emit
                    && !(*emit <= index
                        && matches!(
                            fixture.steps.get(emit.wrapping_sub(1)),
                            Some(Step::Emit { .. })
                        ))
                {
                    return Err(format!(
                        "step {}: after_emit {emit} is not an earlier emit step",
                        index + 1
                    ));
                }
                names.extend(capture.keys().map(String::as_str));
            }
            Step::Exit { code, stderr } => {
                if index + 1 != fixture.steps.len() {
                    return Err(format!("step {}: exit must be the last step", index + 1));
                }
                if i32::from(*code) == FAILED {
                    return Err(format!(
                        "step {}: exit code {FAILED} is reserved for replay failure",
                        index + 1
                    ));
                }
                if stderr.len() > MAX_EXIT_STDERR {
                    return Err(format!(
                        "step {}: exit stderr exceeds {MAX_EXIT_STDERR} bytes",
                        index + 1
                    ));
                }
            }
            Step::SpawnSurvivor { ms } => {
                if *ms > fixture.deadline_ms {
                    return Err(format!(
                        "step {}: a survivor must end within deadline_ms",
                        index + 1
                    ));
                }
            }
            Step::Emit { .. }
            | Step::Delay { .. }
            | Step::AwaitSignal { .. }
            | Step::AwaitEof {} => {}
        }
    }
    let survivors = fixture
        .steps
        .iter()
        .filter(|step| matches!(step, Step::SpawnSurvivor { .. }))
        .count();
    if survivors > MAX_SURVIVORS {
        return Err(format!(
            "fixture has more than {MAX_SURVIVORS} survivor steps"
        ));
    }
    names.sort_unstable();
    names.dedup();
    if names.len() > MAX_CAPTURES {
        return Err(format!(
            "fixture has more than {MAX_CAPTURES} capture names"
        ));
    }
    Ok(fixture)
}

/// Checks argv against the fixture, capturing bound arguments, and returns it.
fn check_argv(expected: &[Arg], captures: &mut Captures) -> Result<Vec<String>, String> {
    let args = env::args_os()
        .skip(1)
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "argv mismatch: an argument is not UTF-8".to_owned())?;
    if args == ["--version"] {
        return Ok(args);
    }
    let mismatch = || format!("argv mismatch: got {args:?}");
    if args.len() != expected.len() {
        return Err(mismatch());
    }
    for (arg, spec) in args.iter().zip(expected) {
        match spec {
            Arg::Exact(text) if text == arg => {}
            Arg::Exact(_) => return Err(mismatch()),
            Arg::Capture(ArgCapture { capture }) => captures.insert(capture, arg.clone())?,
        }
    }
    Ok(args)
}

/// Starts the watchdog, which ends the process at its current deadline,
/// whatever the other threads are doing. If it cannot start, the process
/// exits at once.
fn arm(deadline: Instant, step: Arc<AtomicUsize>) -> SyncSender<Arm> {
    // Capacity 1: the main thread sends at most one message, so it never waits.
    let (sender, receiver) = mpsc::sync_channel(1);
    // Detached on purpose: it only ever ends the process, or returns when disarmed.
    let spawned = thread::Builder::new().spawn(move || {
        let mut deadline = deadline;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = if left.is_zero() {
                // A queued update may postpone the expiry: take it first and
                // re-evaluate against the newest deadline.
                match receiver.try_recv() {
                    Ok(message) => message,
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            } else {
                match receiver.recv_timeout(left) {
                    Ok(message) => message,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => {
                        thread::sleep(left);
                        continue;
                    }
                }
            };
            match message {
                Arm::Deadline(next) => deadline = next,
                Arm::Disarm => return,
            }
        }
        expire(step.load(Ordering::SeqCst));
    });
    if spawned.is_err() {
        process::exit(FAILED);
    }
    sender
}

/// Exits [`FAILED`] at the deadline. The message is written on its own
/// thread: stderr may be locked by a blocked diagnostic or its pipe may be
/// full, and neither may hold the exit back longer than [`MESSAGE_GRACE`].
/// The exit status alone reports the failure, so a helper that cannot be
/// created means exiting at once with no message.
fn expire(step: usize) -> ! {
    let (done, wait) = mpsc::sync_channel(1);
    let helper = thread::Builder::new().spawn(move || {
        diagnostic(&format!("deadline passed at step {step}"));
        // The watchdog may already have stopped waiting; nothing to do.
        let _ = done.send(());
    });
    if helper.is_ok() {
        // A timeout or a lost sender both mean: exit now.
        let _ = wait.recv_timeout(MESSAGE_GRACE);
    }
    process::exit(FAILED)
}

/// Where a step runs: its number, the previous step's completion, the
/// emits before it and the run deadline.
struct Context<'a> {
    number: usize,
    previous: Instant,
    emitted: &'a Emitted,
    deadline: Instant,
}

/// Runs one step and returns the instant it completed, from which later
/// steps measure `within_ms` and order EOF. An expect completes at its
/// line's arrival: the adapter may close stdin as soon as it has written
/// the line. An emit completes just before its write: a reader may react as
/// soon as the line is visible. An `await_signal` completes once its signal
/// is consumed, just before it reports `signalled`, and an `await_eof` at
/// the EOF's arrival, however late the step ran.
fn run_step(
    step: Step,
    at: &Context<'_>,
    input: &mut Input,
    captures: &mut Captures,
    (signals, progress): (&Signals, &Progress<'_>),
) -> Result<Instant, String> {
    match step {
        Step::Expect {
            line,
            capture,
            absent,
            within_ms,
            after_emit,
        } => {
            // One budget for the whole expected value, however many strings it has.
            let mut budget = MAX_LINE;
            let expected = substitute_value(line, captures, &mut budget)?;
            let (arrived, actual) = input.expect_line(at.previous, within_ms)?;
            if !contains_expected(&actual, &expected) {
                return Err(format!("expected line {expected} does not match {actual}"));
            }
            if let Some(pointer) = absent
                .iter()
                .find(|pointer| actual.pointer(pointer).is_some())
            {
                return Err(format!("{pointer} must be absent in {actual}"));
            }
            let floor = match after_emit {
                Some(emit) => at.emitted.get_key_value(&emit),
                None => at.emitted.last_key_value(),
            };
            if let Some((emit, written)) = floor
                && arrived < *written
            {
                return Err(format!(
                    "the line arrived before step {emit}'s emit was written"
                ));
            }
            for (name, pointer) in capture {
                let value = actual
                    .pointer(&pointer)
                    .ok_or_else(|| format!("capture {name}: {pointer} is absent in {actual}"))?;
                captures.insert(&name, value.to_string())?;
            }
            Ok(arrived)
        }
        Step::Emit { line } => {
            let mut budget = MAX_LINE;
            let line = substitute(&line, captures, &mut budget)?;
            let completed = Instant::now();
            write_line(&line)?;
            Ok(completed)
        }
        Step::Delay { ms } => {
            thread::sleep(Duration::from_millis(ms));
            Ok(Instant::now())
        }
        Step::AwaitSignal { signal } => {
            progress.log("at", at.number)?;
            signals.take(signal)?;
            let completed = Instant::now();
            progress.log("signalled", at.number)?;
            Ok(completed)
        }
        Step::AwaitEof {} => input.await_eof(at.previous, at.number),
        Step::SpawnSurvivor { ms } => {
            spawn_survivor(ms, at.deadline)?;
            Ok(Instant::now())
        }
        Step::Exit { code: _, stderr } => {
            let mut out = io::stderr().lock();
            out.write_all(stderr.as_bytes())
                .and_then(|()| out.flush())
                .map_err(|error| format!("cannot write stderr: {error}"))?;
            Ok(Instant::now())
        }
    }
}

/// Starts `sleep` in this process's group for `ms`, capped by the time left
/// before `deadline`, with its stdio detached so it holds no pipe of the
/// adapter's. It is never waited for: it outlives the fake by design and
/// exits on its own.
fn spawn_survivor(ms: u64, deadline: Instant) -> Result<(), String> {
    let left = deadline.saturating_duration_since(Instant::now());
    let life = Duration::from_millis(ms).min(left);
    let survivor = Command::new("sleep")
        .arg(format!("{}.{:03}", life.as_secs(), life.subsec_millis()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start the survivor: {error}"))?;
    // Not reaped here: the fake exits first and the survivor is reparented.
    drop(survivor);
    Ok(())
}

/// Replaces each `${name}` with its captured text and each `$${` with a
/// literal `${`. Every append is charged to `budget` before it is made.
fn substitute(text: &str, captures: &Captures, budget: &mut usize) -> Result<String, String> {
    let mut out = String::new();
    let mut push = |piece: &str| {
        *budget = budget
            .checked_sub(piece.len())
            .ok_or_else(|| format!("substitution exceeds {MAX_LINE} bytes"))?;
        out.push_str(piece);
        Ok::<(), String>(())
    };
    let mut rest = text;
    while let Some(start) = rest.find('$') {
        push(&rest[..start])?;
        rest = &rest[start..];
        if let Some(after) = rest.strip_prefix("$${") {
            push("${")?;
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let end = after.find('}').ok_or("unterminated ${ in a line")?;
            let name = &after[..end];
            push(
                captures
                    .texts
                    .get(name)
                    .ok_or_else(|| format!("uses uncaptured ${{{name}}}"))?,
            )?;
            rest = &after[end + 1..];
        } else {
            push("$")?;
            rest = &rest[1..];
        }
    }
    push(rest)?;
    Ok(out)
}

/// Substitutes captures into every string value of an expected line, all
/// charged to one `budget`.
fn substitute_value(
    value: Value,
    captures: &Captures,
    budget: &mut usize,
) -> Result<Value, String> {
    Ok(match value {
        Value::String(text) if text.contains("${") => {
            Value::String(substitute(&text, captures, budget)?)
        }
        // A plain string is kept as is, but still counts toward the bound.
        Value::String(text) => {
            *budget = budget
                .checked_sub(text.len())
                .ok_or_else(|| format!("substitution exceeds {MAX_LINE} bytes"))?;
            Value::String(text)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| substitute_value(item, captures, budget))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, item)| Ok((key, substitute_value(item, captures, budget)?)))
                .collect::<Result<_, String>>()?,
        ),
        other @ (Value::Null | Value::Bool(_) | Value::Number(_)) => other,
    })
}

/// Writes one line of at most [`MAX_LINE`] bytes; a failed write fails the run.
fn write_line(line: &str) -> Result<(), String> {
    if line.len() > MAX_LINE {
        return Err(format!("output line exceeds {MAX_LINE} bytes"));
    }
    let mut out = io::stdout().lock();
    out.write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
        .map_err(|error| format!("cannot write stdout: {error}"))
}

/// Writes a failure message as one line of at most [`MAX_DIAGNOSTIC`] bytes,
/// prefix and newline included.
fn diagnostic(message: &str) {
    // Best-effort: the exit status already reports the failure.
    let _ = io::stderr()
        .lock()
        .write_all(diagnostic_line(message).as_bytes());
}

fn diagnostic_line(message: &str) -> String {
    let mut end = message
        .len()
        .min(MAX_DIAGNOSTIC - DIAGNOSTIC_PREFIX.len() - 1);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut line = String::with_capacity(MAX_DIAGNOSTIC);
    line.push_str(DIAGNOSTIC_PREFIX);
    line.push_str(&message[..end]);
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::{MAX_DIAGNOSTIC, diagnostic_line};

    #[test]
    fn diagnostic_is_at_most_its_bound_in_total() {
        for message in [
            "short",
            &"é".repeat(MAX_DIAGNOSTIC),
            &"x".repeat(4 * MAX_DIAGNOSTIC),
        ] {
            let line = diagnostic_line(message);
            assert!(line.len() <= MAX_DIAGNOSTIC, "{} bytes", line.len());
            assert!(line.ends_with('\n'));
        }
        assert_eq!(
            diagnostic_line(&"x".repeat(4 * MAX_DIAGNOSTIC)).len(),
            MAX_DIAGNOSTIC
        );
    }
}
