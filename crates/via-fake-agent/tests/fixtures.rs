//! Fidelity and hygiene of the recorded-vendor fixtures under
//! `crates/via-adapters/tests/fixtures/<harness>/` (adapters design §8 item
//! 3; x.3.1 slices).
//!
//! - Every `*.replay.json` loads in replay mode and names its source run.
//! - A generic driver that answers each expect step with that step's own
//!   line (a subset matches itself), reads back each emitted line, and
//!   delivers each `await_signal`, completes the fixture with exit 0.
//! - No fixture file contains a home path, an email address, a token-like
//!   value or a credential field with a real value.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Outer bound for one fixture run; each fixture's own deadline is shorter.
const OUTER: Duration = Duration::from_secs(30);
/// The value the driver passes for every argv capture.
const CAPTURED_ARG: &str = "0f1de11e-0000-4000-8000-000000000001";
/// Credential fields that may hold only a placeholder.
const SECRET_KEYS: [&str; 3] = ["access_token", "refresh_token", "api_key"];
/// The only values a credential field may hold.
const PLACEHOLDERS: [&str; 4] = ["", "<redacted>", "REDACTED", "PLACEHOLDER"];

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../via-adapters/tests/fixtures")
}

/// Every file under `fixtures/<harness>/`, sorted.
fn fixture_files() -> TestResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    for harness in fs::read_dir(fixtures_root())? {
        let harness = harness?.path();
        if !harness.is_dir() {
            continue;
        }
        for file in fs::read_dir(&harness)? {
            let file = file?.path();
            if file.is_file() {
                files.push(file);
            }
        }
    }
    files.sort();
    Ok(files)
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

/// Kills and reaps the fake if the drive fails part way.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            // Cleanup is best-effort; the test has already failed.
            let _ = self.0.kill();
            let _ = self.0.wait();
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

/// Runs one fixture with the generic driver.
fn drive(fixture_path: &Path) -> TestResult {
    let fixture: Value = serde_json::from_slice(&fs::read(fixture_path)?)?;
    let source = fixture.get("source").and_then(Value::as_str).unwrap_or("");
    if source.trim().is_empty() {
        return Err("the fixture names no source run".into());
    }
    let root = tempfile::tempdir()?;
    let binary = install(root.path(), fixture_path)?;

    if let Some(version) = fixture.get("version").and_then(Value::as_str) {
        let output = Command::new(&binary).arg("--version").output()?;
        if output.status.code() != Some(0) || output.stdout != format!("{version}\n").as_bytes() {
            return Err(format!("--version gave {output:?}").into());
        }
    }

    let mut captures = BTreeMap::new();
    let args = args_of(&fixture, &mut captures)?;

    let mut child = Command::new(&binary)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = lines_of(child.stdout.take().ok_or("no stdout")?);
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let stderr = thread::spawn(move || {
        let mut text = String::new();
        // Only for the failure message; a read error leaves it short.
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let mut guard = Guard(child);
    let deadline = Instant::now() + OUTER;

    for (index, step) in fixture["steps"]
        .as_array()
        .ok_or("steps is not an array")?
        .iter()
        .enumerate()
    {
        let number = index + 1;
        let fail = |message: String| format!("step {number}: {message}");
        if let Some(emit) = step.get("emit") {
            let expected =
                substitute(emit["line"].as_str().ok_or("emit line")?, &captures).map_err(fail)?;
            let left = deadline.saturating_duration_since(Instant::now());
            let line = stdout
                .recv_timeout(left)
                .map_err(|error| fail(format!("no emitted line: {error}")))?
                .map_err(fail)?;
            if line != expected {
                return Err(fail(format!("emitted {line}, fixture says {expected}")).into());
            }
        } else if let Some(expect) = step.get("expect") {
            let line = answer(expect, &mut captures).map_err(fail)?;
            writeln!(stdin, "{line}")?;
            stdin.flush()?;
        } else if let Some(wait) = step.get("await_signal") {
            let signal = match wait["signal"].as_str() {
                Some("SIGINT") => "-INT",
                Some("SIGTERM") => "-TERM",
                other => return Err(fail(format!("unknown signal {other:?}")).into()),
            };
            // Handlers are installed before the first step, and any earlier
            // emit was already read, so the fake is past its setup.
            let status = Command::new("kill")
                .args([signal, &guard.0.id().to_string()])
                .status()?;
            if !status.success() {
                return Err(fail("kill failed".to_owned()).into());
            }
        } else if step.get("delay").is_none() {
            return Err(fail(format!("unknown step {step}")).into());
        }
    }
    drop(stdin);

    let status = loop {
        if let Some(status) = guard.0.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err("the fake outlived the outer bound".into());
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stderr = stderr.join().map_err(|_| "stderr reader panicked")?;
    if status.code() != Some(0) {
        return Err(format!("exit {status}: {stderr}").into());
    }
    if let Ok(Ok(extra)) = stdout.recv_timeout(Duration::from_millis(100)) {
        return Err(format!("unexpected extra output {extra}").into());
    }
    Ok(())
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
            let run = thread::spawn(move || drive(&path).map_err(|error| error.to_string()));
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
/// not itself parsed: backslash-escaped slashes, doubled backslashes and
/// `\u` escapes of `/`, `\` and `@`.
fn unescape(text: &str) -> String {
    let mut text = text.to_owned();
    for (escaped, plain) in [
        ("\\u002f", "/"),
        ("\\u002F", "/"),
        ("\\u005c", "\\"),
        ("\\u005C", "\\"),
        ("\\u0040", "@"),
    ] {
        text = text.replace(escaped, plain);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
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

/// What a hygiene finding is in one (decoded) string, or `None`.
fn hygiene_finding(text: &str) -> Option<String> {
    let text = unescape(text);
    let lower = text.to_ascii_lowercase();
    for needle in ["/home/", "/users/", "c:\\users", "bearer "] {
        if lower.contains(needle) {
            return Some(format!("contains {needle:?}"));
        }
    }
    for needle in ["eyJ", "ghp_", "gho_"] {
        if text.contains(needle) {
            return Some(format!("contains {needle:?}"));
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
/// is scanned, credential fields must hold an exact placeholder, and a
/// string that is itself JSON (an emit line, templated or not) is decoded
/// and scanned recursively.
fn value_finding(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => map.iter().find_map(|(key, item)| {
            if SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str()) && !placeholder(item) {
                return Some(format!("{key} has a non-placeholder value"));
            }
            hygiene_finding(key).or_else(|| value_finding(item))
        }),
        Value::Array(items) => items.iter().find_map(value_finding),
        Value::String(text) => hygiene_finding(text).or_else(|| {
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

/// A credential field name in text that is not JSON.
fn secret_mention(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    SECRET_KEYS
        .iter()
        .find(|key| lower.contains(**key))
        .map(|key| format!("non-JSON text mentions {key}"))
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
