//! Post-terminal stream drain through the real `via` daemon and fake vendor (T1-I1).
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test fixtures and assertions fail loudly"
)]

use std::fs;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const ACCEPTED: &str = r#"{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}"#;
const TERMINAL: &str = r#"{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"done","stop_reason":"end_turn"}"#;

/// One isolated daemon, State and fake fixture; the daemon is stopped on drop.
struct Sandbox {
    _root: tempfile::TempDir,
    via: PathBuf,
    fake: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    sync: PathBuf,
    fixture: PathBuf,
}

impl Sandbox {
    fn new(steps: &[Value]) -> Self {
        let via = PathBuf::from(env!("CARGO_BIN_EXE_via"));
        let fake = via.parent().unwrap().join("via-fake-agent");
        assert!(fake.is_file(), "build -p via-fake-agent first");
        let root = tempfile::tempdir().unwrap();
        let [state, runtime, sync] = ["state", "runtime", "sync"].map(|name| {
            let path = root.path().join(name);
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            path
        });
        let fixture = root.path().join("fixture.json");
        let script = json!({
            "expected_request": {"type":"start","id":1,"turn":1,"prompt":"hello"},
            "steps": steps,
        });
        fs::write(&fixture, serde_json::to_vec(&script).unwrap()).unwrap();
        Self {
            _root: root,
            via,
            fake,
            state,
            runtime,
            sync,
            fixture,
        }
    }

    fn run(&self, args: &[&str], timeout: Duration) -> Output {
        let mut child = Command::new(&self.via)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("VIA_STATE_DIR", &self.state)
            .env("VIA_RUNTIME_DIR", &self.runtime)
            .env("VIA_FAKE_AGENT_BINARY", &self.fake)
            .env("VIA_FAKE_SCENARIO", &self.fixture)
            .env("VIA_FAKE_SYNC_DIR", &self.sync)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + timeout;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("via {args:?} exceeded {timeout:?}");
            }
            thread::sleep(Duration::from_millis(5));
        }
        child.wait_with_output().unwrap()
    }

    /// Runs one foreground fake turn and returns its result envelope.
    fn spawn_turn(&self) -> Value {
        let output = self.run(
            &[
                "spawn",
                "--harness",
                "fake",
                "--model",
                "fake",
                "--prompt",
                "hello",
                "--json",
            ],
            Duration::from_secs(40),
        );
        let lines: Vec<Value> = output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "receipt and envelope: {lines:?}");
        lines[1].clone()
    }

    /// Returns every durable raw unit for the sole connection as (stream code, bytes).
    fn raw_units(&self) -> Vec<(u8, Vec<u8>)> {
        let dir = self.state.join("raw");
        let idx = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "idx"))
            .expect("raw index");
        let mut payload = Vec::new();
        fs::File::open(idx.with_extension("raw"))
            .unwrap()
            .read_to_end(&mut payload)
            .unwrap();
        let index = fs::read(idx).unwrap();
        assert_eq!(&index[..8], b"VIARAW01");
        index[8..]
            .chunks(45)
            .map(|entry| {
                let offset =
                    usize::try_from(u64::from_le_bytes(entry[1..9].try_into().unwrap())).unwrap();
                let len =
                    usize::try_from(u32::from_le_bytes(entry[9..13].try_into().unwrap())).unwrap();
                (entry[0], payload[offset..offset + len].to_vec())
            })
            .collect()
    }

    fn stream(&self, code: u8) -> Vec<u8> {
        self.raw_units()
            .into_iter()
            .filter(|(stream, _)| *stream == code)
            .flat_map(|(_, bytes)| bytes)
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.run(
            &["daemon", "stop", "--force", "--json"],
            Duration::from_secs(5),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.runtime.join("via.sock").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

const STDOUT: u8 = 1;
const STDERR: u8 = 2;

fn emit(message: &str) -> Value {
    json!({"action":"emit","message":serde_json::from_str::<Value>(message).unwrap()})
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Counts stdout lines equal to `message` as JSON; the fake re-serializes its fixtures.
fn json_lines(stdout: &[u8], message: &str) -> usize {
    let expected: Value = serde_json::from_str(message).unwrap();
    stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| serde_json::from_slice::<Value>(line).is_ok_and(|value| value == expected))
        .count()
}

fn failure_text(envelope: &Value) -> String {
    envelope["failure"].to_string()
}

#[test]
fn route_drain_records_late_tool_end_and_stderr_tail_after_terminal() {
    let tool_started = r#"{"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t1","name":"shell","input_summary":"ls"}"#;
    let tool_ended = r#"{"type":"tool_ended","vendor_turn_id":"fake-turn-1","tool_id":"t1","status":"completed","output_summary":"ok","exit_code":0}"#;
    let sandbox = Sandbox::new(&[
        emit(ACCEPTED),
        emit(tool_started),
        emit(TERMINAL),
        json!({"action":"emit_raw","text":"stderr-tail-after-terminal\n","stream":"stderr"}),
        emit(tool_ended),
    ]);
    let envelope = sandbox.spawn_turn();
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert!(contains(
        &sandbox.stream(STDERR),
        b"stderr-tail-after-terminal\n"
    ));
    assert_eq!(json_lines(&sandbox.stream(STDOUT), tool_ended), 1);
}

#[test]
fn route_drain_rejects_duplicate_terminal_after_late_observation() {
    let late = r#"{"type":"later_note","n":1}"#;
    let sandbox = Sandbox::new(&[
        emit(ACCEPTED),
        emit(TERMINAL),
        emit(late),
        // One atomic pipe write: the tail is buffered behind the duplicate terminal.
        json!({"action":"emit_raw","text":format!("{TERMINAL}\nstdout-tail-after-duplicate\n")}),
    ]);
    let envelope = sandbox.spawn_turn();
    assert_eq!(envelope["state"], "failed", "{envelope}");
    assert!(
        failure_text(&envelope).contains("duplicate fake terminal"),
        "{envelope}"
    );
    let stdout = sandbox.stream(STDOUT);
    assert_eq!(
        json_lines(&stdout, TERMINAL),
        2,
        "both terminals stay in the raw log"
    );
    assert!(contains(&stdout, b"stdout-tail-after-duplicate\n"));
    assert_eq!(json_lines(&stdout, late), 1);
}

#[test]
fn route_drain_survives_stderr_flood_after_terminal() {
    const LINE: usize = 1024;
    const COUNT: usize = 4096;
    let line = format!("{}\n", "e".repeat(LINE - 1));
    let sandbox = Sandbox::new(&[
        emit(ACCEPTED),
        emit(TERMINAL),
        json!({"action":"flood","text":line,"count":COUNT,"stream":"stderr"}),
        json!({"action":"emit_raw","text":"flood-end\n","stream":"stderr"}),
    ]);
    let started = Instant::now();
    let envelope = sandbox.spawn_turn();
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert!(started.elapsed() < Duration::from_secs(30));
    let stderr = sandbox.stream(STDERR);
    assert_eq!(stderr.len(), LINE * COUNT + b"flood-end\n".len());
    assert!(stderr.ends_with(b"flood-end\n"));
}
