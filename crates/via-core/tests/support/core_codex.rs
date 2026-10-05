//! x.3.2 X4 D4 "Codex under Engine": a `conformance_core` case runs a
//! recorded Codex replay through Core's public Engine. The case directory
//! holds the replay copy and the fake agent linked as `<case>`; Engine's
//! adapter config pins the `codex` binary to that link.
//!
//! The fixtures pin the recorded session cwd, which Engine's `spawn`
//! refuses unless it exists, so the copy is rewritten to a case-owned
//! directory `D`: every JSON string leaf equal to the recorded cwd, or
//! starting with it and `/`, becomes `D` (plus the same suffix), in the
//! `expect` line subsets and in the `emit` line templates. A template's
//! placeholders (`${name}`, and the literal `$${`) are protected by unique
//! numeric tokens while the line is parsed and re-serialized, then restored
//! byte for byte, so captures keep their names and pairing. Setup asserts
//! the round trip and that a fixture change fails loudly.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde_json::{Value, json};
use via_core::{AdapterConfig, BootstrapEnv};

/// The session cwd every Codex fixture recorded.
const RECORDED: &str = "/work/project";
/// The first placeholder token: 19 decimal digits, within `i64`.
const TOKEN_BASE: u64 = 7_391_000_000_000_000_000;
/// The bound on a wait for the fake's progress log.
const PROGRESS_WAIT: Duration = Duration::from_secs(60);

/// The Codex replay fixtures' directory.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../via-adapters/tests/fixtures/codex")
}

/// Fixture `name`'s replay, parsed.
pub(crate) fn replay(name: &str) -> Value {
    let path = fixtures().join(format!("{name}.replay.json"));
    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap()
}

/// Fixture `name`'s expectations, parsed.
#[cfg(feature = "test-failpoints")]
pub(crate) fn expected(name: &str) -> Value {
    let path = fixtures().join(format!("{name}.expect.json"));
    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap()
}

/// One Codex case: the rewritten replay and the fake's link in the case
/// directory, and the case-owned cwd `D`.
pub(crate) struct CodexCase {
    dir: PathBuf,
    name: String,
    link: PathBuf,
    cwd: tempfile::TempDir,
}

impl CodexCase {
    /// Sets case `name` up under `root`: `replay` rewritten to a fresh
    /// cwd, with `fake` linked as `<name>`. Creates the Store's vendor
    /// state directory, as bootstrap does.
    pub(crate) fn new(root: &Path, name: &str, mut replay: Value, fake: &Path) -> Self {
        let cwd = tempfile::Builder::new()
            .prefix("via-x4-cwd-")
            .tempdir_in("/tmp")
            .unwrap();
        let text = cwd.path().to_str().unwrap().to_owned();
        assert!(
            !text.is_empty()
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte)),
            "the case cwd {text:?} needs no JSON escaping"
        );
        rewrite(&mut replay, &text);
        let dir = root.join("case");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.replay.json")), replay.to_string()).unwrap();
        let link = dir.join(name);
        std::os::unix::fs::symlink(fake, &link).unwrap();
        let vendor = root.join("state").join("vendor");
        if !vendor.exists() {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&vendor).unwrap();
        }
        Self {
            dir,
            name: name.to_owned(),
            link,
            cwd,
        }
    }

    /// The case-owned cwd `D`.
    pub(crate) fn cwd(&self) -> &str {
        self.cwd.path().to_str().unwrap()
    }

    /// Engine's adapter config, with `codex` pinned to the fake's link.
    pub(crate) fn config(&self) -> AdapterConfig {
        let raw = serde_json::value::RawValue::from_string(
            json!({"codex": {"binary": self.link}}).to_string(),
        )
        .unwrap();
        AdapterConfig::load(BootstrapEnv::capture(), Some(&raw)).unwrap()
    }

    /// Waits until a launch of the fake logged `at <step>` (its
    /// `await_signal` started waiting); returns that launch's ordinal.
    pub(crate) async fn at(&self, step: usize) -> u64 {
        let path = self.dir.join(format!("{}.progress", self.name));
        let prefix = format!("at {step} launch ");
        let by = tokio::time::Instant::now() + PROGRESS_WAIT;
        loop {
            let text = fs::read_to_string(&path).unwrap_or_default();
            if let Some(launch) = text.lines().find_map(|line| line.strip_prefix(&prefix)) {
                return launch.parse().unwrap();
            }
            assert!(
                tokio::time::Instant::now() < by,
                "the fake never logged {prefix:?}: {text}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// How many times the fake launched (its launch log's lines).
    pub(crate) fn launches(&self) -> usize {
        fs::read_to_string(self.dir.join(format!("{}.launches", self.name)))
            .map_or(0, |log| log.lines().count())
    }

    /// Waits until launch `launch` of the fake logged `at <step>`.
    pub(crate) async fn at_launch(&self, step: usize, launch: u64) {
        let path = self.dir.join(format!("{}.progress", self.name));
        let marker = format!("at {step} launch {launch}");
        let by = tokio::time::Instant::now() + PROGRESS_WAIT;
        while !fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .any(|line| line == marker)
        {
            assert!(
                tokio::time::Instant::now() < by,
                "the fake never logged {marker:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Sends launch `launch`'s fake its gate signal.
    pub(crate) fn signal(&self, launch: u64) {
        let log = fs::read_to_string(self.dir.join(format!("{}.launches", self.name))).unwrap();
        let pid = log
            .lines()
            .nth(usize::try_from(launch).unwrap() - 1)
            .unwrap()
            .trim()
            .to_owned();
        let status = Command::new("kill").args(["-USR1", &pid]).status().unwrap();
        assert!(status.success(), "kill -USR1 {pid}");
    }
}

/// Rewrites the recorded cwd to `cwd` throughout `replay`, checking that
/// each of the `thread/start` and `turn/start` expectations and the thread
/// reply's echo had one, and that none is left.
fn rewrite(replay: &mut Value, cwd: &str) {
    let (mut thread_start, mut turn_start, mut echo) = (0, 0, 0);
    for step in replay["steps"].as_array_mut().unwrap() {
        if let Some(expect) = step.get_mut("expect") {
            let method = expect["line"]["method"].as_str().map(str::to_owned);
            let count = leaves(&mut expect["line"], cwd);
            match method.as_deref() {
                Some("thread/start") => thread_start += count,
                Some("turn/start") => turn_start += count,
                _ => {}
            }
        } else if let Some(emit) = step.get_mut("emit") {
            let (line, count, echoed) = rewrite_line(emit["line"].as_str().unwrap(), cwd);
            if echoed {
                echo += count;
            }
            emit["line"] = Value::String(line);
        }
    }
    assert!(
        thread_start > 0 && turn_start > 0 && echo > 0,
        "the fixture changed: rewrites thread/start {thread_start}, \
         turn/start {turn_start}, thread echo {echo}"
    );
    assert!(
        !replay.to_string().contains(RECORDED),
        "the recorded cwd is left in the copy"
    );
}

/// Rewrites the recorded cwd at `value`'s string leaves; returns how many.
fn leaves(value: &mut Value, cwd: &str) -> usize {
    match value {
        Value::String(text) => match moved(text, cwd) {
            Some(new) => {
                *text = new;
                1
            }
            None => 0,
        },
        Value::Array(items) => items.iter_mut().map(|item| leaves(item, cwd)).sum(),
        Value::Object(members) => members.values_mut().map(|item| leaves(item, cwd)).sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

/// `text` with the recorded cwd prefix replaced by `cwd`, if it has one.
fn moved(text: &str, cwd: &str) -> Option<String> {
    if text == RECORDED {
        return Some(cwd.to_owned());
    }
    text.strip_prefix(RECORDED)
        .filter(|rest| rest.starts_with('/'))
        .map(|rest| format!("{cwd}{rest}"))
}

/// Placeholder `k`'s token.
fn token(k: usize) -> String {
    (TOKEN_BASE + u64::try_from(k).unwrap()).to_string()
}

/// `line` with each placeholder replaced by its token, scanned with the
/// replay's own rule (`$${` is a literal; `${name}` runs to the next `}`),
/// and the placeholders' texts in order.
fn protect(line: &str) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut placeholders = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let length = if tail.starts_with("$${") {
            3
        } else if tail.starts_with("${") {
            tail.find('}').unwrap() + 1
        } else {
            out.push('$');
            rest = &tail[1..];
            continue;
        };
        let token = token(placeholders.len());
        assert!(!line.contains(&token), "token {token} is in {line}");
        out.push_str(&token);
        placeholders.push(tail[..length].to_owned());
        rest = &tail[length..];
    }
    out.push_str(rest);
    (out, placeholders)
}

/// `value` with each token (a number, or text inside a string) replaced by
/// its placeholder's text, so two protections compare.
fn canonical(value: &Value, placeholders: &[String]) -> Value {
    match value {
        Value::Number(number) => placeholders
            .iter()
            .enumerate()
            .find(|(k, _)| number.as_u64() == Some(TOKEN_BASE + u64::try_from(*k).unwrap()))
            .map_or_else(
                || value.clone(),
                |(_, text)| Value::String(format!("\u{1}{text}")),
            ),
        Value::String(text) => {
            let mut text = text.clone();
            for (k, placeholder) in placeholders.iter().enumerate() {
                text = text.replace(&token(k), placeholder);
            }
            Value::String(text)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| canonical(item, placeholders))
                .collect(),
        ),
        Value::Object(members) => Value::Object(
            members
                .iter()
                .map(|(key, item)| (key.clone(), canonical(item, placeholders)))
                .collect(),
        ),
        Value::Null | Value::Bool(_) => value.clone(),
    }
}

/// Asserts `rewritten` differs from `original` only at string leaves that
/// held the recorded cwd, now moved to `cwd`.
fn only_cwd_leaves(original: &Value, rewritten: &Value, cwd: &str) {
    match (original, rewritten) {
        (Value::String(before), Value::String(after)) if before != after => {
            assert_eq!(
                moved(before, cwd).as_ref(),
                Some(after),
                "{before} -> {after}"
            );
        }
        (Value::Array(before), Value::Array(after)) => {
            assert_eq!(before.len(), after.len());
            for (before, after) in before.iter().zip(after) {
                only_cwd_leaves(before, after, cwd);
            }
        }
        (Value::Object(before), Value::Object(after)) => {
            assert!(before.keys().eq(after.keys()), "{before:?} -> {after:?}");
            for (key, before) in before {
                only_cwd_leaves(before, &after[key], cwd);
            }
        }
        _ => assert_eq!(original, rewritten),
    }
}

/// Emit template `line` with the recorded cwd moved to `cwd`: the line,
/// how many leaves moved, and whether it is the thread reply's echo. A
/// line with none is kept byte for byte.
fn rewrite_line(line: &str, cwd: &str) -> (String, usize, bool) {
    let (protected, placeholders) = protect(line);
    let original: Value = serde_json::from_str(&protected)
        .map_err(|error| format!("{error}: {protected}"))
        .unwrap();
    let mut rewritten = original.clone();
    let count = leaves(&mut rewritten, cwd);
    if count == 0 {
        return (line.to_owned(), 0, false);
    }
    let echo = rewritten.pointer("/result/thread").is_some();
    let mut text = rewritten.to_string();
    for (k, placeholder) in placeholders.iter().enumerate() {
        let token = token(k);
        assert_eq!(text.matches(&token).count(), 1, "token {token} in {text}");
        text = text.replacen(&token, placeholder, 1);
    }
    // The lossless round trip.
    let (again, restored) = protect(&text);
    let mut sorted = (placeholders.clone(), restored.clone());
    sorted.0.sort();
    sorted.1.sort();
    assert_eq!(sorted.0, sorted.1, "the placeholders changed: {text}");
    let reparsed: Value = serde_json::from_str(&again).unwrap();
    let rewritten = canonical(&rewritten, &placeholders);
    assert_eq!(canonical(&reparsed, &restored), rewritten, "{text}");
    only_cwd_leaves(&canonical(&original, &placeholders), &rewritten, cwd);
    (text, count, echo)
}

#[test]
fn protection_restores_placeholders_byte_for_byte() {
    let line = r#"{"id":${thread},"result":{"thread":{"cwd":"/work/project/a","x":"$${lit}","n":"${n}"}}}"#;
    let (text, count, echo) = rewrite_line(line, "/tmp/d");
    assert_eq!((count, echo), (1, true));
    assert!(text.contains("${thread}") && text.contains("$${lit}") && text.contains("\"${n}\""));
    assert!(text.contains("\"/tmp/d/a\""), "{text}");
    assert_eq!(rewrite_line("{\"cwd\":\"/work/projects\"}", "/tmp/d").1, 0);
}
