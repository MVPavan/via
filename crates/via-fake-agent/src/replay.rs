//! Recorded-vendor replay (adapters design §8 item 3).
//!
//! Started as `<dir>/<name>` with `<dir>/<name>.replay.json` beside it, the
//! fake checks its argv exactly, answers `--version`, then runs the fixture's
//! steps: `expect` reads one stdin line and matches a JSON subset, capturing
//! values by JSON pointer; `emit` writes a verbatim line with `${name}`
//! replaced by a captured value's JSON text; `delay` sleeps; `await_signal`
//! waits for a signal. A mismatch, an over-long line or the whole-run deadline
//! exits non-zero naming the 1-based step number.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::signal::unix::{Signal, SignalKind, signal};

use super::contains_expected;

/// Longest line, read or written, in bytes (newline excluded).
const MAX_LINE: usize = 1024 * 1024;
const MAX_STEPS: usize = 10_000;
/// Exit code for a replay failure; distinct from the start-request mode's 2.
const FAILED: i32 = 3;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    /// Provenance, for example `CC c7`; not interpreted.
    #[serde(default)]
    #[expect(dead_code, reason = "provenance is for readers of the fixture")]
    source: Option<String>,
    argv: Vec<String>,
    #[serde(default)]
    version: Option<String>,
    deadline_ms: u64,
    steps: Vec<Step>,
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
    let step = Arc::new(AtomicUsize::new(0));
    let code = match replay(fixture, &step) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "fake replay: {error}");
            FAILED
        }
    };
    let _ = io::stdout().lock().flush();
    process::exit(code)
}

fn replay(fixture: &Path, step: &Arc<AtomicUsize>) -> Result<(), String> {
    let fixture = load(fixture)?;
    let args: Vec<String> = env::args().skip(1).collect();
    if args == ["--version"] {
        let version = fixture
            .version
            .ok_or("fixture has no version for --version")?;
        return write_line(&version).map_err(|error| error.to_string());
    }
    if args != fixture.argv {
        return Err(format!(
            "argv mismatch: got {args:?}, fixture expects {:?}",
            fixture.argv
        ));
    }
    watchdog(Duration::from_millis(fixture.deadline_ms), Arc::clone(step));
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
    let mut captures = BTreeMap::new();
    for (index, current) in fixture.steps.into_iter().enumerate() {
        let number = index + 1;
        step.store(number, Ordering::SeqCst);
        run_step(current, &mut input, &mut captures, &runtime, &mut signals)
            .map_err(|error| format!("step {number}: {error}"))?;
    }
    Ok(())
}

fn load(path: &Path) -> Result<Fixture, String> {
    let bytes = fs::read(path).map_err(|error| format!("cannot read fixture: {error}"))?;
    let fixture: Fixture =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid fixture: {error}"))?;
    if fixture.steps.len() > MAX_STEPS {
        return Err(format!("fixture has more than {MAX_STEPS} steps"));
    }
    for (index, step) in fixture.steps.iter().enumerate() {
        if let Step::Emit { line } = step
            && (line.len() > MAX_LINE || line.contains('\n'))
        {
            return Err(format!(
                "step {}: emit line exceeds {MAX_LINE} bytes or holds a newline",
                index + 1
            ));
        }
    }
    Ok(fixture)
}

/// Ends the whole run at `deadline`, naming the step then in progress.
fn watchdog(deadline: Duration, step: Arc<AtomicUsize>) {
    // Detached on purpose: it only ever exits the process.
    thread::spawn(move || {
        thread::sleep(deadline);
        let _ = writeln!(
            io::stderr().lock(),
            "fake replay: deadline of {} ms passed at step {}",
            deadline.as_millis(),
            step.load(Ordering::SeqCst)
        );
        process::exit(FAILED);
    });
}

fn run_step<R: BufRead>(
    step: Step,
    input: &mut R,
    captures: &mut BTreeMap<String, String>,
    runtime: &tokio::runtime::Runtime,
    signals: &mut BTreeMap<SignalName, Signal>,
) -> Result<(), String> {
    match step {
        Step::Expect { line, capture } => {
            let actual = read_line(input)?;
            if !contains_expected(&actual, &line) {
                return Err(format!("expected line {line} does not match {actual}"));
            }
            for (name, pointer) in capture {
                let value = actual
                    .pointer(&pointer)
                    .ok_or_else(|| format!("capture {name}: {pointer} is absent in {actual}"))?;
                captures.insert(name, value.to_string());
            }
            Ok(())
        }
        Step::Emit { line } => write_line(&substitute(&line, captures)?).map_err(|e| e.to_string()),
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

/// Replaces each `${name}` with its captured JSON text.
fn substitute(line: &str, captures: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find('}')
            .ok_or("unterminated ${ in emit line")?;
        let name = &rest[start + 2..start + end];
        out.push_str(
            captures
                .get(name)
                .ok_or_else(|| format!("emit uses uncaptured ${{{name}}}"))?,
        );
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn write_line(line: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}
