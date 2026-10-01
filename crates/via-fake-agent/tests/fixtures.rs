//! Fidelity and hygiene of the recorded-vendor fixtures under
//! `crates/via-adapters/tests/fixtures/<harness>/` (adapters design §8 item
//! 3; x.3.1 slices).
//!
//! - Every `*.replay.json` loads in replay mode and names its source run.
//! - A generic driver that answers each expect step with that step's own
//!   line (a subset matches itself), reads back each emitted line, delivers
//!   each `await_signal` once the fake catches that signal and closes stdin
//!   at each `await_eof`, completes the fixture with the exit code and
//!   stderr of its `exit` step (0 and none without one), with one launch
//!   logged per start. Ordered EOF and strict
//!   trailing input hold: a line resent before an `await_eof`, or an EOF
//!   right after the first line, fails replay.
//! - No fixture file contains a home path (`/home/`, `/Users/`, `/root/`,
//!   `X:\Users\` on any drive), an email address, a token-like value
//!   (`sk-`, `ghp_`/`gho_`/`ghs_`/`ghu_`/`ghr_`, `github_pat_`, `xox?-`,
//!   `AKIA…`, `glpat-`, `AIza…`, `Bearer `, JWT `eyJ…`), a credential field
//!   with a real value, or an identity-bearing field (`user`, `username`,
//!   `login`, `email`, `account`) whose string value is not a placeholder.
//!   Credential names are normalized for case and `_`/`-` (`password`,
//!   `passwd`, `secret`, `client_secret`, `token`, `auth_token`, `api_key`,
//!   `ANTHROPIC_AUTH_TOKEN`, …: any name ending in one of
//!   [`CREDENTIAL_SUFFIXES`]). JSON `\u` escapes are decoded before matching,
//!   including inside prefixed stderr. Every key and string is scanned, the
//!   step fields (`exit.stderr`, `absent` pointers) included; a credential
//!   assigned in free text (`api_key=…`) is a finding.
//! - Every file under the fixtures root is scanned, nested directories
//!   included. Test sources are outside the scan: they are covered by normal
//!   review.
//! - Every fixture that does not model a crash seals its input with a final
//!   `await_eof` (alone, or right before its `exit`); the exceptions are
//!   listed with their reasons.
//! - The outer supervisor bounds the whole run of each fake, the
//!   `--version` probe and the driver's stdin writes included.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Outer bound for one fixture run; each fixture's own deadline is shorter.
const OUTER: Duration = Duration::from_secs(30);
/// The value the driver passes for every argv capture.
const CAPTURED_ARG: &str = "0f1de11e-0000-4000-8000-000000000001";
/// A field or word whose normalized name (lowercase, no `_` or `-`) ends
/// with one of these is a credential: it may hold only a placeholder.
const CREDENTIAL_SUFFIXES: [&str; 10] = [
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "privatekey",
    "accesskey",
    "credential",
    "credentials",
];
/// Identity-bearing fields, normalized: a string value is a placeholder.
const IDENTITY_FIELDS: [&str; 5] = ["user", "username", "login", "email", "account"];
/// The only values a credential or identity field may hold.
const PLACEHOLDERS: [&str; 4] = ["", "<redacted>", "REDACTED", "PLACEHOLDER"];
/// Fixtures whose input is not sealed by a final `await_eof` (alone, or
/// right before the closing `exit`), each with its reason.
const ENDS_WITHOUT_EOF: [(&str, &str); 4] = [
    (
        "claude/c10_early_eof.replay.json",
        "a vendor record of stdin EOF right after the prompt: its await_eof is step 2 and the \
         vendor output follows it",
    ),
    (
        "codex/c0_server_lost.replay.json",
        "models a crash: the server exits mid-turn, so no input is sealed; the exit step's \
         trailing-input check is best effort",
    ),
    (
        "codex/c10_read_only_refused.replay.json",
        "no launch: the bound is refused before any vendor I/O, so the fixture has no steps",
    ),
    (
        "codex/c4b_workspace_write_refused.replay.json",
        "no launch: the bound is refused before any vendor I/O, so the fixture has no steps",
    ),
];

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../via-adapters/tests/fixtures")
}

/// Every file under `root`, nested directories included, sorted.
fn files_under(root: &Path) -> TestResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Every file under the fixtures root.
fn fixture_files() -> TestResult<Vec<PathBuf>> {
    files_under(&fixtures_root())
}

fn replay_fixtures() -> TestResult<Vec<PathBuf>> {
    Ok(fixture_files()?
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".replay.json"))
        })
        .collect())
}

/// Replaces `${name}` with its capture and `$${` with `${`, as replay does.
fn substitute(text: &str, captures: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(after) = rest.strip_prefix("$${") {
            out.push_str("${");
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let end = after.find('}').ok_or("unterminated ${")?;
            let name = &after[..end];
            out.push_str(
                captures
                    .get(name)
                    .ok_or_else(|| format!("uses uncaptured ${{{name}}}"))?,
            );
            rest = &after[end + 1..];
        } else {
            out.push('$');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn substitute_value(value: &Value, captures: &BTreeMap<String, String>) -> Result<Value, String> {
    Ok(match value {
        Value::String(text) => Value::String(substitute(text, captures)?),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute_value(item, captures))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| Ok((key.clone(), substitute_value(item, captures)?)))
                .collect::<Result<_, String>>()?,
        ),
        other @ (Value::Null | Value::Bool(_) | Value::Number(_)) => other.clone(),
    })
}

/// Sets the object member at a JSON pointer, creating objects on the way.
fn insert_at(target: &mut Value, pointer: &str, value: Value) -> Result<(), String> {
    let keys: Vec<String> = pointer
        .strip_prefix('/')
        .ok_or_else(|| format!("bad pointer {pointer}"))?
        .split('/')
        .map(|key| key.replace("~1", "/").replace("~0", "~"))
        .collect();
    let (last, parents) = keys.split_last().ok_or("empty pointer")?;
    let mut node = target;
    for key in parents {
        node = node
            .as_object_mut()
            .ok_or_else(|| format!("{pointer} crosses a non-object"))?
            .entry(key.clone())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
    node.as_object_mut()
        .ok_or_else(|| format!("{pointer} ends in a non-object"))?
        .insert(last.clone(), value);
    Ok(())
}

/// Installs the fake as `<root>/vendor` beside a copy of the fixture.
fn install(root: &Path, fixture: &Path) -> TestResult<PathBuf> {
    let binary = root.join("vendor");
    symlink(env!("CARGO_BIN_EXE_via-fake-agent"), &binary)?;
    fs::copy(fixture, root.join("vendor.replay.json"))?;
    Ok(binary)
}

/// A child shared with its supervisor, which kills it at the deadline.
type Shared = Arc<Mutex<Child>>;

fn lock(child: &Shared) -> std::sync::MutexGuard<'_, Child> {
    // Each holder only kills or polls; a panic cannot leave it inconsistent.
    child.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Kills its child at the deadline unless dropped first, so no step of a
/// run (a `--version` probe, a blocked stdin write, a read) outlives the
/// outer bound. Dropping it stops the watch and, if the child still runs,
/// kills and reaps it.
struct Supervisor {
    child: Shared,
    stop: Option<SyncSender<()>>,
    watch: Option<JoinHandle<()>>,
}

impl Supervisor {
    fn start(child: Child, deadline: Instant) -> Self {
        let child = Arc::new(Mutex::new(child));
        let watched = Arc::clone(&child);
        let (stop, stopped) = mpsc::sync_channel(1);
        let watch = thread::spawn(move || {
            let left = deadline.saturating_duration_since(Instant::now());
            if matches!(stopped.recv_timeout(left), Err(RecvTimeoutError::Timeout)) {
                // Best effort: the run then fails on its closed pipes.
                let _ = lock(&watched).kill();
            }
        });
        Self {
            child,
            stop: Some(stop),
            watch: Some(watch),
        }
    }

    /// Waits for the child's exit, polling until the supervisor kills it.
    fn wait(&self) -> TestResult<ExitStatus> {
        loop {
            if let Some(status) = lock(&self.child).try_wait()? {
                return Ok(status);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(watch) = self.watch.take() {
            // A panicked watch has nothing left to clean up.
            let _ = watch.join();
        }
        let mut child = lock(&self.child);
        if !matches!(child.try_wait(), Ok(Some(_))) {
            // Cleanup is best-effort; the test has already failed.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Stdout lines, read on their own thread so the fake never blocks.
fn lines_of(stdout: impl Read + Send + 'static) -> Receiver<Result<String, String>> {
    let (sender, receiver) = mpsc::sync_channel(64);
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender
                .send(line.map_err(|error| error.to_string()))
                .is_err()
            {
                return;
            }
        }
    });
    receiver
}

/// The argv for a fixture: exact entries as is, each capture as
/// [`CAPTURED_ARG`], recorded in `captures`.
fn args_of(fixture: &Value, captures: &mut BTreeMap<String, String>) -> TestResult<Vec<String>> {
    let mut args = Vec::new();
    for arg in fixture["argv"].as_array().ok_or("argv is not an array")? {
        if let Some(text) = arg.as_str() {
            args.push(text.to_owned());
        } else {
            let name = arg["capture"].as_str().ok_or("bad argv entry")?;
            captures.insert(name.to_owned(), CAPTURED_ARG.to_owned());
            args.push(CAPTURED_ARG.to_owned());
        }
    }
    Ok(args)
}

/// The line answering an expect step: its own subset, with captures
/// substituted. A captured value the subset leaves out is one VIA generates
/// (a request ID), so the driver supplies its own.
fn answer(expect: &Value, captures: &mut BTreeMap<String, String>) -> Result<Value, String> {
    let mut line = substitute_value(&expect["line"], captures)?;
    let capture = expect.get("capture").and_then(Value::as_object);
    for (name, pointer) in capture.into_iter().flatten() {
        let pointer = pointer.as_str().ok_or("capture pointer")?;
        if line.pointer(pointer).is_none() {
            insert_at(&mut line, pointer, Value::String(format!("driver-{name}")))?;
        }
        let value = line.pointer(pointer).ok_or("capture insert failed")?;
        captures.insert(name.clone(), value.to_string());
    }
    Ok(line)
}

/// How the generic driver departs from the fixture, to show that replay
/// catches what a fixture pins.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Deviation {
    /// Answers every step as the fixture says.
    None,
    /// Resends the last answered line just before closing stdin.
    ExtraInput,
    /// Closes stdin right after the first answered line.
    EarlyEof,
}

/// What the fake must end with: its exit code and stderr.
type End = (i64, String);

/// The fixtures one file holds, in launch order: the file itself, or each
/// of its `lifetimes`.
fn lifetimes_of(file: &Value) -> Vec<&Value> {
    match file.get("lifetimes").and_then(Value::as_array) {
        Some(lifetimes) => lifetimes.iter().collect(),
        None => vec![file],
    }
}

/// Every pipelined expect step names its reason in the fixture's notes.
fn pipelined_have_reasons(fixture: &Value) -> Result<(), String> {
    let notes = fixture
        .get("notes")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    for (index, step) in fixture["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let number = index + 1;
        if step["expect"]["pipelined"] == Value::Bool(true)
            && !notes.contains(&format!("step {number}"))
        {
            return Err(format!(
                "step {number} is pipelined with no reason in notes"
            ));
        }
    }
    Ok(())
}

/// Runs one fixture file with the generic driver: each lifetime as its own
/// launch, in order. A single fixture's `--version` is probed first; a
/// lifetime's is not, since a probe would take a lifetime's launch.
fn drive(fixture_path: &Path, deviation: Deviation) -> TestResult {
    let file: Value = serde_json::from_slice(&fs::read(fixture_path)?)?;
    let source = file.get("source").and_then(Value::as_str).unwrap_or("");
    if source.trim().is_empty() {
        return Err("the fixture names no source run".into());
    }
    let root = tempfile::tempdir()?;
    let binary = install(root.path(), fixture_path)?;
    let lifetimes = lifetimes_of(&file);
    let single = file.get("lifetimes").is_none();
    let mut starts = 0;
    if single && let Some(version) = file.get("version").and_then(Value::as_str) {
        probe_version(&binary, version)?;
        starts += 1;
    }
    for (index, fixture) in lifetimes.into_iter().enumerate() {
        pipelined_have_reasons(fixture)?;
        run_lifetime(&binary, fixture, deviation)
            .map_err(|error| format!("lifetime {}: {error}", index + 1))?;
        starts += 1;
    }
    let launches = fs::read_to_string(root.path().join("vendor.launches"))?;
    if launches.lines().count() != starts {
        return Err(format!("launch log has {launches:?}, expected {starts} starts").into());
    }
    Ok(())
}

/// Runs `--version` under the outer bound and checks its line.
fn probe_version(binary: &Path, version: &str) -> TestResult {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let supervisor = Supervisor::start(child, Instant::now() + OUTER);
    let mut out = Vec::new();
    // Ends at the fake's exit, or when the supervisor kills it.
    stdout.read_to_end(&mut out)?;
    let status = supervisor.wait()?;
    if status.code() != Some(0) || out != format!("{version}\n").as_bytes() {
        return Err(format!("--version gave {status} with {out:?}").into());
    }
    Ok(())
}

/// Runs one launch of the fake against `fixture`'s steps.
fn run_lifetime(binary: &Path, fixture: &Value, deviation: Deviation) -> TestResult {
    let mut captures = BTreeMap::new();
    let args = args_of(fixture, &mut captures)?;
    let mut child = Command::new(binary)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = Some(child.stdin.take().ok_or("no stdin")?);
    let stdout = lines_of(child.stdout.take().ok_or("no stdout")?);
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let stderr = thread::spawn(move || {
        let mut text = String::new();
        // A read error leaves it short, which the comparison reports.
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let pid = child.id();
    let deadline = Instant::now() + OUTER;
    // From here every blocking step is bounded: at the deadline the
    // supervisor kills the fake, which ends writes, reads and the wait.
    let supervisor = Supervisor::start(child, deadline);

    let steps = run_steps(
        fixture,
        &mut Run {
            stdin: &mut stdin,
            stdout: &stdout,
            pid,
            deadline,
            captures,
            deviation,
        },
    );
    drop(stdin);
    let status = supervisor.wait()?;
    if Instant::now() >= deadline {
        return Err("the fake outlived the outer bound".into());
    }
    let stderr = stderr.join().map_err(|_| "stderr reader panicked")?;
    let (code, text) =
        steps.map_err(|error| format!("{error}; the fake ended {status}: {stderr}"))?;
    if status.code().map(i64::from) != Some(code) || stderr != text {
        return Err(
            format!("{status} with stderr {stderr:?}; fixture says {code} {text:?}").into(),
        );
    }
    if let Ok(Ok(extra)) = stdout.recv_timeout(Duration::from_millis(100)) {
        return Err(format!("unexpected extra output {extra}").into());
    }
    Ok(())
}

/// The driver's side of one run.
struct Run<'a> {
    stdin: &'a mut Option<ChildStdin>,
    stdout: &'a Receiver<Result<String, String>>,
    pid: u32,
    deadline: Instant,
    captures: BTreeMap<String, String>,
    deviation: Deviation,
}

/// Answers the fixture's steps in order and returns how the fake must end:
/// exit 0 with no stderr, unless an exit step says otherwise.
fn run_steps(fixture: &Value, run: &mut Run<'_>) -> TestResult<End> {
    let mut end = (0, String::new());
    let mut last_answer = None;
    for (index, step) in fixture["steps"]
        .as_array()
        .ok_or("steps is not an array")?
        .iter()
        .enumerate()
    {
        let number = index + 1;
        let fail = |message: String| format!("step {number}: {message}");
        if let Some(emit) = step.get("emit") {
            let expected = substitute(emit["line"].as_str().ok_or("emit line")?, &run.captures)
                .map_err(fail)?;
            let left = run.deadline.saturating_duration_since(Instant::now());
            let line = run
                .stdout
                .recv_timeout(left)
                .map_err(|error| fail(format!("no emitted line: {error}")))?
                .map_err(fail)?;
            if line != expected {
                return Err(fail(format!("emitted {line}, fixture says {expected}")).into());
            }
        } else if let Some(expect) = step.get("expect") {
            let line = answer(expect, &mut run.captures).map_err(fail)?;
            // The answer is the fixture's own subset, so an absent pointer
            // it resolves is a fixture that no adapter could pass.
            for pointer in expect
                .get("absent")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let pointer = pointer
                    .as_str()
                    .ok_or_else(|| fail("absent pointer".to_owned()))?;
                if line.pointer(pointer).is_some() {
                    return Err(fail(format!("the subset sets absent {pointer}")).into());
                }
            }
            let input = run
                .stdin
                .as_mut()
                .ok_or_else(|| fail("stdin already closed".to_owned()))?;
            // Answered at once, so any `within_ms` is met.
            writeln!(input, "{line}")?;
            input.flush()?;
            last_answer = Some(line);
            if run.deviation == Deviation::EarlyEof {
                drop(run.stdin.take());
            }
        } else if let Some(wait) = step.get("await_signal") {
            let (signal, number) = match wait["signal"].as_str() {
                Some("SIGINT") => ("-INT", 2),
                Some("SIGTERM") => ("-TERM", 15),
                Some("SIGUSR1") => ("-USR1", 10),
                other => return Err(fail(format!("unknown signal {other:?}")).into()),
            };
            // A gate before any emit can come before the fake's setup, so
            // wait until the fake catches the signal instead of dying of it.
            wait_until_caught(run.pid, number, run.deadline).map_err(fail)?;
            let status = Command::new("kill")
                .args([signal, &run.pid.to_string()])
                .status()?;
            if !status.success() {
                return Err(fail("kill failed".to_owned()).into());
            }
        } else if step.get("await_eof").is_some() {
            if run.deviation == Deviation::ExtraInput
                && let (Some(input), Some(line)) = (run.stdin.as_mut(), &last_answer)
            {
                writeln!(input, "{line}")?;
                input.flush()?;
            }
            drop(run.stdin.take());
        } else if let Some(exit) = step.get("exit") {
            let code = exit["code"]
                .as_i64()
                .ok_or_else(|| fail("exit code".to_owned()))?;
            let text = exit["stderr"]
                .as_str()
                .ok_or_else(|| fail("exit stderr".to_owned()))?;
            end = (code, text.to_owned());
        } else if step.get("delay").is_none() && step.get("spawn_survivor").is_none() {
            return Err(fail(format!("unknown step {step}")).into());
        }
    }
    Ok(end)
}

/// Waits until process `pid` catches signal `number` (Linux: its bit in
/// `SigCgt` of `/proc/<pid>/status`), polling until `deadline`.
fn wait_until_caught(pid: u32, number: u32, deadline: Instant) -> Result<(), String> {
    let bit = 1_u64 << (number - 1);
    loop {
        let status = fs::read_to_string(format!("/proc/{pid}/status"))
            .map_err(|error| format!("cannot read the fake's status: {error}"))?;
        let caught = status
            .lines()
            .find_map(|line| line.strip_prefix("SigCgt:"))
            .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
            .ok_or("no SigCgt in the fake's status")?;
        if caught & bit != 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("the fake never caught signal {number}"));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn fixtures_replay_with_a_generic_driver_to_exit_zero() -> TestResult {
    let fixtures = replay_fixtures()?;
    assert!(!fixtures.is_empty(), "no replay fixtures found");
    // Fixtures replay their recorded delays, so they run concurrently.
    let runs: Vec<_> = fixtures
        .into_iter()
        .map(|path| {
            let name = path.display().to_string();
            let run = thread::spawn(move || {
                drive(&path, Deviation::None).map_err(|error| error.to_string())
            });
            (name, run)
        })
        .collect();
    let mut failures = Vec::new();
    for (name, run) in runs {
        match run.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(format!("{name}: {error}")),
            Err(_) => failures.push(format!("{name}: driver panicked")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

/// Undoes the escapes a sensitive value could hide behind in text that is
/// not itself parsed: every JSON `\u` escape (surrogate pairs included),
/// then backslash-escaped slashes and doubled backslashes.
fn unescape(text: &str) -> String {
    let unit = |hex: &str| {
        hex.get(..4)
            .filter(|digits| digits.chars().all(|c| c.is_ascii_hexdigit()))
            .and_then(|digits| u32::from_str_radix(digits, 16).ok())
    };
    // The character an escape body (after `\u`) stands for, and the bytes
    // of the body it used.
    let escape = |body: &str| -> Option<(char, usize)> {
        let high = unit(body)?;
        if !(0xD800..=0xDBFF).contains(&high) {
            return char::from_u32(high).map(|c| (c, 4));
        }
        let low = unit(body.get(4..)?.strip_prefix("\\u")?)?;
        if !(0xDC00..=0xDFFF).contains(&low) {
            return None;
        }
        char::from_u32(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)).map(|c| (c, 10))
    };
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("\\u") {
        decoded.push_str(&rest[..at]);
        let body = &rest[at + 2..];
        if let Some((c, used)) = escape(body) {
            decoded.push(c);
            rest = &body[used..];
        } else {
            decoded.push_str("\\u");
            rest = body;
        }
    }
    decoded.push_str(rest);
    let mut out = String::with_capacity(decoded.len());
    let mut chars = decoded.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Drop a backslash that escapes a slash or another backslash.
            if matches!(chars.peek(), Some('/' | '\\')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// A name lowercased with `_` and `-` removed: `Client-Secret`,
/// `client_secret` and `clientSecret` are one name.
fn normalized(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// The credential suffix a name ends with, normalized, if any.
fn credential(name: &str) -> Option<&'static str> {
    let name = normalized(name);
    CREDENTIAL_SUFFIXES
        .into_iter()
        .find(|suffix| name.ends_with(suffix))
}

fn identity(name: &str) -> bool {
    IDENTITY_FIELDS.contains(&normalized(name).as_str())
}

/// Whether `prefix` occurs at `at` followed by `len` characters that
/// satisfy `body`.
fn followed_by(text: &str, at: usize, prefix: &str, len: usize, body: fn(char) -> bool) -> bool {
    let tail = &text[at + prefix.len()..];
    tail.chars().take(len).filter(|c| body(*c)).count() == len
}

/// What a hygiene finding is in one (decoded) string, or `None`.
fn hygiene_finding(text: &str) -> Option<String> {
    let text = unescape(text);
    let lower = text.to_ascii_lowercase();
    for needle in ["/home/", "/users/", "/root/", "bearer "] {
        if lower.contains(needle) {
            return Some(format!("contains {needle:?}"));
        }
    }
    for (at, _) in lower.match_indices(":\\users") {
        if lower[..at]
            .chars()
            .next_back()
            .is_some_and(|drive| drive.is_ascii_alphabetic())
        {
            return Some("contains a drive's Users folder".to_owned());
        }
    }
    for needle in [
        "eyJ",
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "ghr_",
        "github_pat_",
        "glpat-",
    ] {
        if text.contains(needle) {
            return Some(format!("contains {needle:?}"));
        }
    }
    for (at, _) in text.match_indices("xox") {
        let kind = text[at + 3..].chars().next();
        if kind.is_some_and(|kind| "abprs".contains(kind)) && text[at + 4..].starts_with('-') {
            return Some("contains a Slack-like xox?- token".to_owned());
        }
    }
    let upper_alnum = |c: char| c.is_ascii_uppercase() || c.is_ascii_digit();
    let key_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    for (at, _) in text.match_indices("AKIA") {
        if followed_by(&text, at, "AKIA", 16, upper_alnum) {
            return Some("contains an AKIA access key".to_owned());
        }
    }
    for (at, _) in text.match_indices("AIza") {
        if followed_by(&text, at, "AIza", 35, key_char) {
            return Some("contains an AIza API key".to_owned());
        }
    }
    for (at, _) in text.match_indices("sk-") {
        let starts_word = text[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_ascii_alphanumeric());
        if starts_word {
            return Some("contains a token-like sk- value".to_owned());
        }
    }
    email_in(&text).map(|email| format!("contains an email address {email:?}"))
}

/// The first `local@domain.tld` in `text`.
fn email_in(text: &str) -> Option<&str> {
    let local = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    let domain = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '.';
    for (at, _) in text.match_indices('@') {
        let start = text[..at]
            .char_indices()
            .rev()
            .take_while(|(_, c)| local(*c))
            .last()
            .map_or(at, |(index, _)| index);
        let end = text[at + 1..]
            .char_indices()
            .take_while(|(_, c)| domain(*c))
            .last()
            .map_or(at + 1, |(index, c)| at + 1 + index + c.len_utf8());
        let host = text[at + 1..end].trim_end_matches('.');
        let tld_ok = host.rsplit_once('.').is_some_and(|(name, tld)| {
            !name.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
        });
        if start < at && tld_ok {
            return Some(&text[start..at + 1 + host.len()]);
        }
    }
    None
}

/// Whether a credential field's value is exactly a placeholder.
fn placeholder(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => PLACEHOLDERS.contains(&text.as_str()),
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => false,
    }
}

/// Text as JSON would see it once replay substitutes captures: each
/// `${name}` becomes the neutral `0` (valid both bare and inside a string)
/// and `$${` the literal `${`.
fn neutralize_captures(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(after) = rest.strip_prefix("$${") {
            out.push_str("${");
            rest = after;
        } else if let Some((_, after)) = rest
            .strip_prefix("${")
            .and_then(|after| after.split_once('}'))
        {
            out.push('0');
            rest = after;
        } else {
            out.push('$');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// The first finding anywhere in `value`: every key and decoded string leaf
/// is scanned, credential fields must hold an exact placeholder, identity
/// fields a placeholder when they hold a string, and a string that is
/// itself JSON (an emit line, templated or not) is decoded and scanned
/// recursively.
fn value_finding(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => map.iter().find_map(|(key, item)| {
            let key_text = unescape(key);
            if credential(&key_text).is_some() && !placeholder(item) {
                return Some(format!("{key} has a non-placeholder value"));
            }
            if identity(&key_text) && item.is_string() && !placeholder(item) {
                return Some(format!("identity field {key} has a non-placeholder value"));
            }
            hygiene_finding(key).or_else(|| value_finding(item))
        }),
        Value::Array(items) => items.iter().find_map(value_finding),
        Value::String(text) => hygiene_finding(text)
            .or_else(|| secret_assignment(text))
            .or_else(|| {
                let trimmed = text.trim_start();
                if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
                    return None;
                }
                match serde_json::from_str::<Value>(&neutralize_captures(text)) {
                    Ok(inner) => value_finding(&inner),
                    // An object-shaped emit that cannot be decoded cannot be
                    // shown clean; bracketed prose is covered by the text scan
                    // plus a credential-name check.
                    Err(error) if trimmed.starts_with('{') => {
                        Some(format!("embedded JSON does not parse: {error}"))
                    }
                    Err(_) => secret_mention(text),
                }
            }),
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

fn file_finding(text: &str) -> Option<String> {
    if let Some(finding) = hygiene_finding(text) {
        return Some(finding);
    }
    match serde_json::from_str::<Value>(text) {
        Ok(value) => value_finding(&value),
        Err(_) => secret_mention(text),
    }
}

/// The words of `text`: maximal runs of ASCII letters, digits, `_` and
/// `-`, each with its byte offset.
fn words(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut start = None;
    text.char_indices()
        .chain(std::iter::once((text.len(), ' ')))
        .filter_map(move |(at, c)| match (start, word(c)) {
            (None, true) => {
                start = Some(at);
                None
            }
            (Some(from), false) => {
                start = None;
                Some((from, &text[from..at]))
            }
            (None, false) | (Some(_), true) => None,
        })
}

/// A credential assigned a value in free text, such as stderr: a credential
/// name (normalized), an optional quote, `=` or `:`, then a value that is
/// not exactly a placeholder (or a bare `null`). A quoted value is taken
/// whole, through its closing quote, so whitespace inside it hides nothing.
/// `\u` escapes are decoded first.
fn secret_assignment(text: &str) -> Option<String> {
    let text = unescape(text);
    let quote = |c: char| c == '"' || c == '\'';
    for (at, name) in words(&text) {
        if credential(name).is_none() {
            continue;
        }
        let rest = text[at + name.len()..]
            .trim_start_matches(quote)
            .trim_start();
        let Some(rest) = rest.strip_prefix(['=', ':']) else {
            continue;
        };
        let rest = rest.trim_start();
        // A quoted value runs through its closing quote (or to the end),
        // whitespace included; a bare one stops at a delimiter.
        let (value, quoted) = match rest.chars().next() {
            Some(open) if quote(open) => {
                let inner = &rest[1..];
                (&inner[..inner.find(open).unwrap_or(inner.len())], true)
            }
            _ => {
                let end = rest
                    .find(|c: char| c.is_whitespace() || quote(c) || ",;&}".contains(c))
                    .unwrap_or(rest.len());
                (&rest[..end], false)
            }
        };
        // `PLACEHOLDERS` includes the empty value; a bare `null` is JSON's.
        let placeholder =
            PLACEHOLDERS.contains(&value) || (!quoted && (value.is_empty() || value == "null"));
        if !placeholder {
            return Some(format!("{name} is assigned a non-placeholder value"));
        }
    }
    None
}

/// A compound credential name (`access_token`, `client-secret`, not a bare
/// `token`) in text that is not JSON.
fn secret_mention(text: &str) -> Option<String> {
    let text = unescape(text);
    words(&text)
        .find(|(_, name)| credential(name).is_some_and(|suffix| normalized(name) != suffix))
        .map(|(_, name)| format!("non-JSON text mentions {name}"))
}

#[test]
fn fixtures_hygiene_scan_finds_nothing() -> TestResult {
    let files = fixture_files()?;
    assert!(!files.is_empty(), "no fixture files found");
    let mut findings = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file)?;
        if let Some(finding) = file_finding(&text) {
            findings.push(format!("{}: {finding}", file.display()));
        }
    }
    assert!(findings.is_empty(), "{}", findings.join("\n"));
    Ok(())
}

#[test]
fn fixtures_hygiene_scan_detects_each_pattern() {
    let jwt = format!("{}{}", "ey", "J0eXAiOiJKV1QifQ");
    let bad = [
        // Plain.
        r#"{"p":"/home/someone/project"}"#.to_owned(),
        r#"{"p":"/Users/someone"}"#.to_owned(),
        r#"{"p":"C:\\Users\\someone"}"#.to_owned(),
        r#"{"t":"contact a.person@example.org today"}"#.to_owned(),
        r#"{"t":"key sk-abc123"}"#.to_owned(),
        r#"{"t":"ghp_abc"}"#.to_owned(),
        r#"{"t":"gho_abc"}"#.to_owned(),
        r#"{"t":"Authorization: Bearer abc"}"#.to_owned(),
        format!(r#"{{"t":"{jwt}"}}"#),
        r#"{"access_token":"abc"}"#.to_owned(),
        r#"{"nested":{"API_KEY":"abc"}}"#.to_owned(),
        // Escaped representations.
        r#"{"p":"\u002fhome\u002fsomeone"}"#.to_owned(),
        r#"{"p":"\/home\/someone"}"#.to_owned(),
        r#"{"line":"{\"p\":\"\\u002fhome\\u002fsomeone\"}"}"#.to_owned(),
        r#"{"line":"{\"p\":\"C:\\\\Users\\\\someone\"}"}"#.to_owned(),
        r#"{"t":"C:\\\\Users\\\\someone"}"#.to_owned(),
        r#"{"t":"a.person\u0040example.org"}"#.to_owned(),
        r#"{"t":"\u0073k-abc123"}"#.to_owned(),
        r#"{"t":"authorization: bearer abc"}"#.to_owned(),
        r#"{"line":"{\"refresh_token\":\"abc\"}"}"#.to_owned(),
        r#"{"line":"{\"\\u0061ccess_token\":\"abc\"}"}"#.to_owned(),
        // Templated emits are decoded, not skipped.
        r#"{"line":"{\"id\":${request},\"api_key\":\"abc\"}"}"#.to_owned(),
        r#"{"line":"{\"session_id\":\"${sid}\",\"cwd\":\"/home/someone\"}"}"#.to_owned(),
        // Placeholders match exactly.
        r#"{"access_token":"<redacted>abc"}"#.to_owned(),
        r#"{"api_key":"REDACTED-abc"}"#.to_owned(),
        r#"{"api_key":"abcPLACEHOLDER"}"#.to_owned(),
        // An emit that cannot be decoded is a finding.
        r#"{"line":"{\"broken\":"}"#.to_owned(),
        r#"{"t":"[not json] access_token abc"}"#.to_owned(),
        // Non-JSON text.
        "access_token=abc".to_owned(),
    ];
    for text in bad {
        assert!(file_finding(&text).is_some(), "missed {text}");
    }
    let clean = [
        r#"{"access_token":"<redacted>","api_key":"","refresh_token":"REDACTED"}"#,
        r#"{"source":"cc-plugin-agents-md@builtin","cwd":"/work/project"}"#,
        r#"{"subtype":"task_started","text":"ask-me desk-top"}"#,
        r#"{"line":"{\"request_id\":${rid},\"session_id\":\"${sid}\",\"cmd\":\"echo $${HOME}\"}"}"#,
        r#"{"capabilities":["msg_lifecycle_v1"]}"#,
        r#"{"text":"[Request interrupted by user for tool use]"}"#,
    ];
    for text in clean {
        assert_eq!(file_finding(text), None, "flagged {text}");
    }
}

/// The critical review's probes (F21): credential names in any case or
/// `_`/`-` spelling, more token families, more home roots and drives,
/// identity-bearing fields, and a `\u`-escaped credential in prefixed
/// stderr. Each family has a negative witness.
#[test]
fn fixtures_hygiene_scan_detects_the_review_probes() {
    let bad = [
        // Credential names, normalized for case and `_`/`-`.
        r#"{"password":"hunter2"}"#,
        r#"{"passwd":"hunter2"}"#,
        r#"{"secret":"abc"}"#,
        r#"{"client_secret":"abc"}"#,
        r#"{"clientSecret":"abc"}"#,
        r#"{"token":"abc"}"#,
        r#"{"auth_token":"abc"}"#,
        r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"abc"}}"#,
        r#"{"Api-Key":"abc"}"#,
        r#"{"apiKey":"abc"}"#,
        r#"{"t":"export ANTHROPIC_AUTH_TOKEN=abc"}"#,
        r#"{"t":"client-secret: abc"}"#,
        r#"{"t":"Password = hunter2"}"#,
        // Token families.
        r#"{"t":"github_pat_11ABCDEFG0123456789"}"#,
        r#"{"t":"ghs_abc123"}"#,
        r#"{"t":"ghu_abc123"}"#,
        r#"{"t":"sk-ant-api03-abc"}"#,
        r#"{"t":"xoxb-1234-abcd"}"#,
        r#"{"t":"xoxp-1234-abcd"}"#,
        r#"{"t":"AKIAABCDEFGHIJKLMNOP"}"#,
        r#"{"t":"glpat-abc123def456"}"#,
        r#"{"t":"AIzaSyA0123456789abcdefghijklmnopqrstuv"}"#,
        // Home roots, every drive letter, either slash.
        r#"{"p":"/root/.config/vendor"}"#,
        r#"{"p":"D:\\Users\\someone"}"#,
        r#"{"p":"e:\\users\\someone"}"#,
        r#"{"p":"Z:/Users/someone"}"#,
        // Identity-bearing fields hold placeholders only.
        r#"{"user":"jdoe"}"#,
        r#"{"username":"jdoe"}"#,
        r#"{"login":"jdoe"}"#,
        r#"{"email":"jdoe"}"#,
        r#"{"account":"acme-corp"}"#,
        r#"{"nested":{"User_Name":"jdoe"}}"#,
        // A `\u`-escaped credential in prefixed stderr.
        r#"{"steps":[{"exit":{"code":1,"stderr":"[auth] {\"\\u0070assword\":\"abc\"}\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"[warn] \\u0061pi_key=abc\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"[cfg] ANTHROPIC\\u005fAUTH\\u005fTOKEN=abc\n"}}]}"#,
    ];
    let missed: Vec<&str> = bad
        .into_iter()
        .filter(|text| file_finding(text).is_none())
        .collect();
    assert!(missed.is_empty(), "missed:\n{}", missed.join("\n"));
    let clean = [
        r#"{"input_tokens":5,"cached_input_tokens":0,"thinkingTokens":3,"estimated_tokens":50,"maxOutputTokens":9}"#,
        r#"{"apiKeySource":"none","authMode":"chatgpt","tokenUsage":{"total":{"totalTokens":1}}}"#,
        r#"{"type":"user","message":{"role":"user"},"killed":{"parent":0,"user":0,"system":0}}"#,
        r#"{"text":"Remember token NONCE0001 and say the secret word."}"#,
        r#"{"user":"<redacted>","account":null,"password":"REDACTED"}"#,
        r#"{"cwd":"/work/project","home":"/state/codex-home","p":"/rootless/x"}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"[auth] token expired; run login again\n"}}]}"#,
    ];
    for text in clean {
        assert_eq!(file_finding(text), None, "flagged {text}");
    }
}

#[test]
fn fixtures_hygiene_scan_covers_the_new_step_fields() {
    let bad = [
        // An exit step's stderr is text, scanned like every other string.
        r#"{"steps":[{"exit":{"code":1,"stderr":"cannot open /home/someone/.config\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"cannot open \/home\/someone\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"Error: api_key=abc123\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"refresh_token: \"abc\"\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"[auth] {\"access_token\":\"abc\"}\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"Authorization: Bearer abc\n"}}]}"#,
        // A quoted value is checked whole: a space inside the quotes, or
        // leading whitespace, does not hide the rest of it.
        r#"{"steps":[{"exit":{"code":1,"stderr":"Error: api_key=\"<redacted> abc123\"\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"Error: api_key=' abc123'\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"Error: access_token: \" abc123\"\n"}}]}"#,
        // An absent pointer is a string too.
        r#"{"steps":[{"expect":{"line":{},"absent":["/home/someone"]}}]}"#,
    ];
    for text in bad {
        assert!(file_finding(text).is_some(), "missed {text}");
    }
    let clean = [
        r#"{"steps":[{"exit":{"code":1,"stderr":"[claude-code:unrecognized_model] {\"model\":\"m\",\"query_source\":\"sdk\"}\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"api_key=<redacted>\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"api_key=\"<redacted>\", api_key=''\n"}}]}"#,
        r#"{"steps":[{"exit":{"code":1,"stderr":"set the API key first\n"}}]}"#,
        r#"{"steps":[{"expect":{"line":{},"absent":["/response/response"],"within_ms":5000}},{"await_eof":{}}]}"#,
    ];
    for text in clean {
        assert_eq!(file_finding(text), None, "flagged {text}");
    }
}

/// Runs every fixture concurrently with `deviation` and returns, per
/// fixture, the driver's result.
fn drive_all(deviation: Deviation) -> TestResult<Vec<(PathBuf, Result<(), String>)>> {
    let fixtures = replay_fixtures()?;
    assert!(!fixtures.is_empty(), "no replay fixtures found");
    let runs: Vec<_> = fixtures
        .into_iter()
        .map(|path| {
            let worker = path.clone();
            let run =
                thread::spawn(move || drive(&worker, deviation).map_err(|error| error.to_string()));
            (path, run)
        })
        .collect();
    runs.into_iter()
        .map(|(path, run)| {
            let result = run.join().map_err(|_| "driver panicked")?;
            Ok((path, result))
        })
        .collect()
}

/// The fixture's steps.
fn steps_of(path: &Path) -> TestResult<Vec<Value>> {
    let fixture: Value = serde_json::from_slice(&fs::read(path)?)?;
    Ok(fixture["steps"].as_array().ok_or("steps")?.clone())
}

/// Whether the fixture has a step of this kind.
fn has_step(steps: &[Value], kind: &str) -> bool {
    steps.iter().any(|step| step.get(kind).is_some())
}

/// Whether the fixture's first `await_eof` directly follows its first
/// expect: there an EOF right after the first line is the recorded order.
fn eof_follows_first_expect(steps: &[Value]) -> bool {
    let first = steps.iter().position(|step| step.get("expect").is_some());
    first.is_some_and(|at| {
        steps
            .get(at + 1)
            .is_some_and(|step| step.get("await_eof").is_some())
    })
}

/// A resent line before an `await_eof` is strict trailing input, and an EOF
/// right after the first line is an early EOF; both fail replay. Fixtures
/// without an `await_eof` (the resend is never written) or without an
/// expect (nothing to close after) are not exercised by that deviation.
#[test]
fn fixtures_fail_replay_on_trailing_input_or_early_eof() -> TestResult {
    let mut misses = Vec::new();
    let mut exercised = 0;
    for (path, result) in drive_all(Deviation::ExtraInput)? {
        if !has_step(&steps_of(&path)?, "await_eof") {
            continue;
        }
        exercised += 1;
        match result {
            Err(error) if error.contains("fake replay: ") && error.contains("unexpected input") => {
            }
            other => misses.push(format!("{}: extra input gave {other:?}", path.display())),
        }
    }
    for (path, result) in drive_all(Deviation::EarlyEof)? {
        let steps = steps_of(&path)?;
        if !has_step(&steps, "expect") || eof_follows_first_expect(&steps) {
            continue;
        }
        exercised += 1;
        match result {
            Err(error) if error.contains("fake replay: ") && error.contains("exit status: 3") => {}
            other => misses.push(format!("{}: early EOF gave {other:?}", path.display())),
        }
    }
    assert!(exercised > 0, "no fixture exercised a deviation");
    assert!(misses.is_empty(), "{}", misses.join("\n"));
    Ok(())
}

/// Every fixture that does not model a crash seals its input with a final
/// `await_eof`, alone or right before the closing `exit`; the rest are
/// listed in [`ENDS_WITHOUT_EOF`] with their reasons (review F24).
#[test]
fn fixtures_end_with_await_eof_unless_declared() -> TestResult {
    let root = fixtures_root();
    let mut undeclared = Vec::new();
    let mut seen = Vec::new();
    for path in replay_fixtures()? {
        let file: Value = serde_json::from_slice(&fs::read(&path)?)?;
        let name = path
            .strip_prefix(&root)?
            .to_str()
            .ok_or("path is not UTF-8")?
            .to_owned();
        for fixture in lifetimes_of(&file) {
            let steps: &[Value] = fixture["steps"].as_array().map_or(&[], Vec::as_slice);
            let eof =
                |step: Option<&Value>| step.is_some_and(|step| step.get("await_eof").is_some());
            let sealed = match steps.split_last() {
                Some((last, before)) if last.get("exit").is_some() => eof(before.last()),
                Some((last, _)) => eof(Some(last)),
                None => false,
            };
            if !sealed {
                if ENDS_WITHOUT_EOF.iter().any(|(listed, _)| *listed == name) {
                    seen.push(name.clone());
                } else {
                    undeclared.push(name.clone());
                }
            }
        }
    }
    for (listed, why) in ENDS_WITHOUT_EOF {
        assert!(!why.is_empty(), "{listed}: no reason");
        assert!(
            seen.iter().any(|name| name == listed),
            "{listed}: not an exception"
        );
    }
    assert!(undeclared.is_empty(), "{}", undeclared.join("\n"));
    Ok(())
}

#[test]
fn fixtures_scan_reaches_nested_directories() -> TestResult {
    let root = tempfile::tempdir()?;
    let nested = root.path().join("claude").join("chain");
    fs::create_dir_all(&nested)?;
    fs::write(root.path().join("top.json"), "{}")?;
    fs::write(nested.join("deep.replay.json"), "{}")?;
    let files = files_under(root.path())?;
    assert_eq!(
        files,
        vec![
            nested.join("deep.replay.json"),
            root.path().join("top.json")
        ]
    );
    Ok(())
}
