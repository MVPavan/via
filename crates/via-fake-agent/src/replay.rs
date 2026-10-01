//! Recorded-vendor replay (adapters design §8 item 3).
//!
//! Started as `<dir>/<name>` with `<dir>/<name>.replay.json` beside it, the
//! fake checks its argv, answers `--version`, then runs the fixture's steps:
//! - `expect` reads one stdin line and matches a JSON subset, capturing
//!   values by JSON pointer;
//! - `emit` writes one verbatim line;
//! - `delay` sleeps; `await_signal` waits for a signal.
//!
//! An `argv` entry is an exact string or `{"capture": "<name>"}`, which
//! captures that argument. In emit lines and in expected string values,
//! `${name}` is replaced by the capture's text: an argv capture's argument
//! verbatim, a stdin capture's JSON text.
//!
//! Lines, the fixture, the captures and the whole run are bounded. Any
//! failure exits [`FAILED`], naming the 1-based step where one was running.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use tokio::signal::unix::{Signal, SignalKind, signal};

use super::contains_expected;

/// Longest line, read or written, in bytes (newline excluded).
const MAX_LINE: usize = 1024 * 1024;
const MAX_STEPS: usize = 10_000;
/// Largest fixture file, checked before it is read.
const MAX_FIXTURE: u64 = 16 * 1024 * 1024;
/// Most distinct capture names, from argv and expect steps together.
const MAX_CAPTURES: usize = 64;
/// Most bytes all captures retain together.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
/// Longest diagnostic written to stderr; longer ones are truncated.
const MAX_DIAGNOSTIC: usize = 1024;
/// How long the deadline's message may take before the process exits anyway.
const MESSAGE_GRACE: Duration = Duration::from_millis(100);
/// Exit code for any replay failure; distinct from the start-request mode's 2.
const FAILED: i32 = 3;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    /// Provenance, for example `CC c7`; not interpreted.
    #[serde(default)]
    #[expect(dead_code, reason = "provenance is for readers of the fixture")]
    source: Option<String>,
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
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
enum SignalName {
    #[serde(rename = "SIGINT")]
    Int,
    #[serde(rename = "SIGTERM")]
    Term,
}

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

/// The sibling fixture of `argv[0]`, when the fake runs in replay mode.
pub(crate) fn fixture_path() -> Option<PathBuf> {
    let argv0 = PathBuf::from(env::args_os().next()?);
    // A bare name was found through PATH; it has no directory to look beside.
    argv0.parent().filter(|dir| !dir.as_os_str().is_empty())?;
    let mut name = OsString::from(argv0.as_os_str());
    name.push(".replay.json");
    let fixture = PathBuf::from(name);
    fixture.is_file().then_some(fixture)
}

/// Replays `fixture` and exits the process.
pub(crate) fn run(fixture: &Path) -> ! {
    let started = Instant::now();
    let step = Arc::new(AtomicUsize::new(0));
    let code = match replay(fixture, started, &step) {
        Ok(()) => 0,
        Err(error) => {
            diagnostic(&error);
            FAILED
        }
    };
    process::exit(code)
}

fn replay(fixture: &Path, started: Instant, step: &Arc<AtomicUsize>) -> Result<(), String> {
    // Loading runs before the deadline is known; the size cap bounds it.
    let fixture = load(fixture)?;
    let deadline = started
        .checked_add(Duration::from_millis(fixture.deadline_ms))
        .ok_or("deadline_ms is out of range")?;
    watchdog(deadline, Arc::clone(step));
    let mut captures = Captures::default();
    let args = check_argv(&fixture.argv, &mut captures)?;
    if args == ["--version"] {
        let version = fixture
            .version
            .ok_or("fixture has no version for --version")?;
        return write_line(&version);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    // Handlers are installed before the first step so an early signal is held
    // for its step instead of killing the process.
    let mut signals = BTreeMap::new();
    for name in fixture.steps.iter().filter_map(|step| match step {
        Step::AwaitSignal { signal } => Some(*signal),
        Step::Expect { .. } | Step::Emit { .. } | Step::Delay { .. } => None,
    }) {
        if let Entry::Vacant(entry) = signals.entry(name) {
            let kind = match name {
                SignalName::Int => SignalKind::interrupt(),
                SignalName::Term => SignalKind::terminate(),
            };
            let _guard = runtime.enter();
            entry.insert(signal(kind).map_err(|error| error.to_string())?);
        }
    }
    let mut input = io::stdin().lock();
    for (index, current) in fixture.steps.into_iter().enumerate() {
        let number = index + 1;
        step.store(number, Ordering::SeqCst);
        run_step(current, &mut input, &mut captures, &runtime, &mut signals)
            .map_err(|error| format!("step {number}: {error}"))?;
    }
    Ok(())
}

fn load(path: &Path) -> Result<Fixture, String> {
    let file = File::open(path).map_err(|error| format!("cannot open fixture: {error}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_FIXTURE + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read fixture: {error}"))?;
    if bytes.len() as u64 > MAX_FIXTURE {
        return Err(format!("fixture exceeds {MAX_FIXTURE} bytes"));
    }
    let fixture: Fixture =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid fixture: {error}"))?;
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
    for step in &fixture.steps {
        if let Step::Expect { capture, .. } = step {
            names.extend(capture.keys().map(String::as_str));
        }
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

/// Ends the whole run at `deadline`, whatever the other threads are doing.
fn watchdog(deadline: Instant, step: Arc<AtomicUsize>) {
    // Detached on purpose: it only ever ends the process.
    thread::spawn(move || {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            thread::sleep(left);
        }
        let step = step.load(Ordering::SeqCst);
        // The message is written on its own thread: stderr may be locked by a
        // blocked diagnostic or its pipe may be full, and neither may hold the
        // exit back longer than MESSAGE_GRACE. The exit status alone reports
        // the failure.
        let (done, wait) = mpsc::sync_channel(1);
        thread::spawn(move || {
            diagnostic(&format!("deadline passed at step {step}"));
            // The watchdog may already have stopped waiting; nothing to do.
            let _ = done.send(());
        });
        // A timeout or a lost sender both mean: exit now.
        let _ = wait.recv_timeout(MESSAGE_GRACE);
        process::exit(FAILED);
    });
}

fn run_step<R: BufRead>(
    step: Step,
    input: &mut R,
    captures: &mut Captures,
    runtime: &tokio::runtime::Runtime,
    signals: &mut BTreeMap<SignalName, Signal>,
) -> Result<(), String> {
    match step {
        Step::Expect { line, capture } => {
            let expected = substitute_value(line, captures)?;
            let actual = read_line(input)?;
            if !contains_expected(&actual, &expected) {
                return Err(format!("expected line {expected} does not match {actual}"));
            }
            for (name, pointer) in capture {
                let value = actual
                    .pointer(&pointer)
                    .ok_or_else(|| format!("capture {name}: {pointer} is absent in {actual}"))?;
                captures.insert(&name, value.to_string())?;
            }
            Ok(())
        }
        Step::Emit { line } => write_line(&substitute(&line, captures)?),
        Step::Delay { ms } => {
            thread::sleep(Duration::from_millis(ms));
            Ok(())
        }
        Step::AwaitSignal { signal } => {
            let stream = signals.get_mut(&signal).ok_or("signal handler missing")?;
            runtime
                .block_on(stream.recv())
                .ok_or_else(|| "signal stream closed".to_owned())
        }
    }
}

fn read_line<R: BufRead>(input: &mut R) -> Result<Value, String> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(MAX_LINE + 1).map_err(|error| error.to_string())?;
    let read = Read::take(&mut *input, limit)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| error.to_string())?;
    if read == 0 {
        return Err("stdin ended before the expected line".to_owned());
    }
    if bytes.last() != Some(&b'\n') {
        return Err(format!(
            "input line exceeds {MAX_LINE} bytes or has no newline"
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("input line is not JSON: {error}"))
}

/// Replaces each `${name}` with its captured text, within [`MAX_LINE`].
fn substitute(line: &str, captures: &Captures) -> Result<String, String> {
    let mut out = String::new();
    let mut push = |piece: &str| {
        if out.len() + piece.len() > MAX_LINE {
            return Err(format!("substituted line exceeds {MAX_LINE} bytes"));
        }
        out.push_str(piece);
        Ok(())
    };
    let mut rest = line;
    while let Some(start) = rest.find("${") {
        push(&rest[..start])?;
        let end = rest[start..].find('}').ok_or("unterminated ${ in a line")?;
        let name = &rest[start + 2..start + end];
        push(
            captures
                .texts
                .get(name)
                .ok_or_else(|| format!("uses uncaptured ${{{name}}}"))?,
        )?;
        rest = &rest[start + end + 1..];
    }
    push(rest)?;
    Ok(out)
}

/// Substitutes captures into every string value of an expected line.
fn substitute_value(value: Value, captures: &Captures) -> Result<Value, String> {
    Ok(match value {
        Value::String(text) if text.contains("${") => Value::String(substitute(&text, captures)?),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| substitute_value(item, captures))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, item)| Ok((key, substitute_value(item, captures)?)))
                .collect::<Result<_, String>>()?,
        ),
        other @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => other,
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

/// Writes a failure message truncated to [`MAX_DIAGNOSTIC`] bytes.
fn diagnostic(message: &str) {
    let mut end = message.len().min(MAX_DIAGNOSTIC);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    // Best-effort: the exit status already reports the failure.
    let _ = writeln!(io::stderr().lock(), "fake replay: {}", &message[..end]);
}
