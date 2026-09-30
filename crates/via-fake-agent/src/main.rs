//! A scripted, test-only private vendor process; it owns no VIA runtime state.
//!
//! The executable validates requests and writes fixture messages and raw bytes.
//! Tests control progress through files in an isolated synchronization directory.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

const SCRIPT_ENV: &str = "VIA_FAKE_SCENARIO";
const SYNC_ENV: &str = "VIA_FAKE_SYNC_DIR";
const MAX_INPUT_MESSAGE: u64 = 32 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Script {
    expected_request: Value,
    steps: Vec<Step>,
}

/// One script, or several for a multi-turn deployment: each launch runs the
/// first script whose `expected_request` its start request contains.
#[derive(Deserialize)]
#[serde(untagged)]
enum Fixture {
    Many { scripts: Vec<Script> },
    One(Script),
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Emit {
        message: Value,
        #[serde(default)]
        stream: Stream,
    },
    EmitRaw {
        text: String,
        #[serde(default)]
        stream: Stream,
    },
    EmitBytes {
        bytes: Vec<u8>,
        #[serde(default)]
        stream: Stream,
    },
    Gate {
        name: String,
    },
    ExpectRequest {
        expected: Value,
    },
    Flood {
        text: String,
        count: u64,
        #[serde(default)]
        stream: Stream,
    },
    /// Writes `bytes` pattern bytes (`b'a' + i % 26`) to stderr (Task 4
    /// design §13.1): the evidence folder's `stderr.log` must hold them.
    Stderr {
        bytes: u64,
    },
    /// Holds stdin unread until gate `name` is released (Task 4 design
    /// §13.1): as a script's first step it runs before the start request is
    /// read, so a start larger than the pipe buffer blocks VIA's writer.
    /// Elsewhere it is a plain gate.
    HoldStdin {
        name: String,
    },
    ReportPids,
    /// Writes the process's working directory to `cwd-<session>-<turn>` in
    /// the sync directory (Task 4 design §13.1).
    ReportCwd,
    /// Writes the start request's prompt bytes to `prompt-<session>-<turn>`
    /// in the sync directory, for the test to digest (§13.1).
    EchoPromptDigest,
    SpawnGrandchild {
        name: String,
    },
    DumpEnvironment,
    Hang,
    IgnoreTerm,
    Exit {
        code: i32,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    #[serde(rename = "type")]
    kind: String,
    id: u64,
    session_id: String,
    turn: u64,
    prompt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InterruptRequest {
    #[serde(rename = "type")]
    kind: String,
    id: u64,
    vendor_turn_id: String,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stream {
    #[default]
    Stdout,
    Stderr,
}

enum InputEvent {
    Message(Value),
    Eof,
    Error(String),
}

fn main() {
    if env::args().nth(1).as_deref() == Some("--grandchild") {
        if let Err(error) = grandchild_main() {
            let _ = writeln!(io::stderr().lock(), "fake grandchild: {error}");
            process::exit(2);
        }
        return;
    }
    if let Err(error) = agent_main() {
        let _ = writeln!(io::stderr().lock(), "fake agent: {error}");
        process::exit(2);
    }
}

fn agent_main() -> Result<(), Box<dyn std::error::Error>> {
    let script_path = PathBuf::from(env::var(SCRIPT_ENV)?);
    let sync_dir = PathBuf::from(env::var(SYNC_ENV)?);
    let scripts = match serde_json::from_slice(&fs::read(script_path)?)? {
        Fixture::Many { scripts } => scripts,
        Fixture::One(script) => vec![script],
    };
    let mut input = BufReader::new(io::stdin());
    let mut grandchildren = Vec::new();
    let held = scripts
        .iter()
        .find_map(|script| match script.steps.first() {
            Some(Step::HoldStdin { name }) => Some(name.clone()),
            _ => None,
        });
    if let Some(name) = &held {
        gate(&sync_dir, name)?;
    }
    let (start, script) = read_start(&mut input, scripts)?;
    let (input_tx, input_rx) = mpsc::sync_channel(8);
    thread::spawn(move || read_remaining(input, &input_tx));
    let mut terminal_emitted = false;
    let mut interrupt_seen = false;
    for step in script.steps {
        match step {
            Step::Emit { message, stream } => {
                if matches!(stream, Stream::Stdout) && message["type"] == "terminal" {
                    terminal_emitted = true;
                }
                write_bytes(stream, serde_json::to_string(&message)?.as_bytes())?;
                write_bytes(stream, b"\n")?;
            }
            Step::EmitRaw { text, stream } => write_bytes(stream, text.as_bytes())?,
            Step::EmitBytes { bytes, stream } => write_bytes(stream, &bytes)?,
            // A hold that ran before the start request was read is done.
            Step::Gate { name } | Step::HoldStdin { name }
                if held.as_deref() != Some(name.as_str()) =>
            {
                gate(&sync_dir, &name)?;
            }
            Step::Gate { .. } | Step::HoldStdin { .. } => {}
            Step::ExpectRequest { expected } => {
                let message = next_input(&input_rx)?;
                read_interrupt(message, &expected, start.turn, &mut interrupt_seen)?;
            }
            Step::Flood {
                text,
                count,
                stream,
            } => {
                for _ in 0..count {
                    write_bytes(stream, text.as_bytes())?;
                }
            }
            Step::Stderr { bytes } => write_pattern(bytes)?,
            Step::ReportPids => {
                fs::write(sync_dir.join("agent.pid"), process::id().to_string())?;
            }
            Step::ReportCwd => {
                let cwd = env::current_dir()?;
                let name = format!("cwd-{}-{}", start.session_id, start.turn);
                fs::write(sync_dir.join(name), cwd.as_os_str().as_encoded_bytes())?;
            }
            Step::EchoPromptDigest => {
                let name = format!("prompt-{}-{}", start.session_id, start.turn);
                fs::write(sync_dir.join(name), start.prompt.as_bytes())?;
            }
            Step::SpawnGrandchild { name } => {
                grandchildren.push(spawn_grandchild(&sync_dir, &name)?);
            }
            Step::DumpEnvironment => dump_environment(&sync_dir)?,
            Step::Hang => loop {
                thread::park();
            },
            Step::IgnoreTerm => ignore_term(&sync_dir)?,
            Step::Exit { code } => process::exit(code),
        }
    }
    if terminal_emitted {
        finalize_input(&input_rx, start.turn, &mut interrupt_seen)?;
    }
    for mut grandchild in grandchildren {
        grandchild.wait()?;
    }
    Ok(())
}

fn read_message<R: BufRead>(input: &mut R) -> Result<Option<Value>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    let read = Read::take(input, MAX_INPUT_MESSAGE + 1).read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }
    if read as u64 > MAX_INPUT_MESSAGE || bytes.last() != Some(&b'\n') {
        return Err("fake input message exceeds bound or has no newline".into());
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// Reads the start request and selects the first script that expects it.
fn read_start<R: BufRead>(
    input: &mut R,
    scripts: Vec<Script>,
) -> Result<(StartRequest, Script), Box<dyn std::error::Error>> {
    let actual = read_message(input)?.ok_or("request ended before expected message")?;
    let script = scripts
        .into_iter()
        .find(|script| contains_expected(&actual, &script.expected_request))
        .ok_or_else(|| format!("request mismatch: no script expects {actual}"))?;
    let request: StartRequest = serde_json::from_value(actual)?;
    if request.kind != "start"
        || request.id != 1
        || request.session_id.is_empty()
        || request.turn == 0
        || request.prompt.len() > 16 * 1024 * 1024
    {
        return Err("invalid typed start request".into());
    }
    Ok((request, script))
}

fn read_interrupt(
    actual: Value,
    expected: &Value,
    turn: u64,
    interrupt_seen: &mut bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !contains_expected(&actual, expected) {
        return Err("interrupt request does not match fixture".into());
    }
    let request: InterruptRequest = serde_json::from_value(actual)?;
    if request.kind != "interrupt"
        || request.id != 2
        || request.vendor_turn_id != format!("fake-turn-{turn}")
        || *interrupt_seen
    {
        return Err("invalid typed interrupt request".into());
    }
    *interrupt_seen = true;
    Ok(())
}

fn read_remaining<R: BufRead>(mut input: R, sender: &SyncSender<InputEvent>) {
    loop {
        let event = match read_message(&mut input) {
            Ok(Some(message)) if message["type"] == "start" => {
                InputEvent::Error("duplicate start request".to_owned())
            }
            Ok(Some(message)) => InputEvent::Message(message),
            Ok(None) => InputEvent::Eof,
            Err(error) => InputEvent::Error(error.to_string()),
        };
        let terminal = !matches!(event, InputEvent::Message(_));
        if sender.send(event).is_err() || terminal {
            break;
        }
    }
}

fn next_input(receiver: &Receiver<InputEvent>) -> Result<Value, Box<dyn std::error::Error>> {
    match receiver.recv()? {
        InputEvent::Message(message) => Ok(message),
        InputEvent::Eof => Err("request stream ended before scripted control".into()),
        InputEvent::Error(error) => Err(error.into()),
    }
}

fn finalize_input(
    receiver: &Receiver<InputEvent>,
    turn: u64,
    interrupt_seen: &mut bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("fake input finalization timed out".into());
        }
        match receiver.recv_timeout(remaining)? {
            InputEvent::Eof => return Ok(()),
            InputEvent::Error(error) => return Err(error.into()),
            InputEvent::Message(message) => {
                read_interrupt(
                    message,
                    &serde_json::json!({"type":"interrupt"}),
                    turn,
                    interrupt_seen,
                )?;
            }
        }
    }
}

fn contains_expected(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|a| contains_expected(a, value))),
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(a, e)| contains_expected(a, e))
        }
        _ => actual == expected,
    }
}

fn write_bytes(stream: Stream, bytes: &[u8]) -> io::Result<()> {
    match stream {
        Stream::Stdout => {
            let mut out = io::stdout().lock();
            out.write_all(bytes)?;
            out.flush()
        }
        Stream::Stderr => {
            let mut out = io::stderr().lock();
            out.write_all(bytes)?;
            out.flush()
        }
    }
}

/// Writes `count` bytes of the `b'a' + i % 26` pattern to stderr.
fn write_pattern(count: u64) -> io::Result<()> {
    let mut out = io::stderr().lock();
    let mut chunk = Vec::with_capacity(64 * 1024);
    for index in 0..count {
        chunk.push(b'a' + u8::try_from(index % 26).unwrap_or(0));
        if chunk.len() == chunk.capacity() {
            out.write_all(&chunk)?;
            chunk.clear();
        }
    }
    out.write_all(&chunk)?;
    out.flush()
}

fn gate(sync_dir: &Path, name: &str) -> io::Result<()> {
    validate_name(name)?;
    let marker = sync_dir.join(format!("{name}.entered"));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)?;
    let release = sync_dir.join(format!("{name}.release"));
    while !release.exists() {
        thread::sleep(Duration::from_millis(5));
    }
    fs::write(sync_dir.join(format!("{name}.released")), b"")
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid gate name",
        ));
    }
    Ok(())
}

fn spawn_grandchild(sync_dir: &Path, name: &str) -> io::Result<process::Child> {
    validate_name(name)?;
    let child = Command::new(env::current_exe()?)
        .arg("--grandchild")
        .arg(name)
        .env(SYNC_ENV, sync_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    fs::write(sync_dir.join(format!("{name}.pid")), child.id().to_string())?;
    Ok(child)
}

fn grandchild_main() -> io::Result<()> {
    let name = env::args()
        .nth(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing grandchild name"))?;
    let sync_dir = PathBuf::from(env::var(SYNC_ENV).map_err(io::Error::other)?);
    gate(&sync_dir, &name)
}

fn dump_environment(sync_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let environment: std::collections::BTreeMap<String, String> = env::vars().collect();
    fs::write(
        sync_dir.join("environment.json"),
        serde_json::to_vec(&environment)?,
    )?;
    Ok(())
}

fn ignore_term(sync_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let _term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        fs::write(sync_dir.join("ignore_term.entered"), b"")?;
        std::future::pending::<Result<(), io::Error>>().await
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_INPUT_MESSAGE, contains_expected, read_message};
    use serde_json::json;
    use std::io::{BufReader, Cursor};

    #[test]
    fn selected_fields_must_match_even_in_nested_objects() {
        let actual = json!({"method":"start","params":{"prompt":"expected","turn":1}});
        assert!(contains_expected(
            &actual,
            &json!({"params":{"prompt":"expected"}})
        ));
        assert!(!contains_expected(
            &actual,
            &json!({"params":{"prompt":"wrong"}})
        ));
        assert!(!contains_expected(
            &actual,
            &json!({"params":{"missing":true}})
        ));
    }

    #[test]
    fn input_message_rejects_bytes_past_its_bound() -> Result<(), Box<dyn std::error::Error>> {
        let mut bytes = vec![b'a'; usize::try_from(MAX_INPUT_MESSAGE)?];
        bytes.push(b'\n');
        let mut input = BufReader::new(Cursor::new(bytes));
        assert!(read_message(&mut input).is_err());
        Ok(())
    }
}
