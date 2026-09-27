//! Turn data path through the real `via` daemon and fake vendor: post-terminal
//! drain (T1-I1), observation events (T1-I5), evidence gaps and failure classes.
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
        // Read both pipes while waiting, so output larger than a pipe buffer cannot block.
        let readers = [
            read_all(child.stdout.take().unwrap()),
            read_all(child.stderr.take().unwrap()),
        ];
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("via {args:?} exceeded {timeout:?}");
            }
            thread::sleep(Duration::from_millis(5));
        };
        let [stdout, stderr] = readers.map(|reader| reader.join().unwrap());
        Output {
            status,
            stdout,
            stderr,
        }
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

    /// Starts one background fake turn and returns its session id.
    fn spawn_background(&self) -> String {
        let output = self.run(
            &[
                "spawn",
                "--harness",
                "fake",
                "--model",
                "fake",
                "--prompt",
                "hello",
                "--background",
                "--json",
            ],
            Duration::from_secs(10),
        );
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        receipt["session_id"].as_str().unwrap().to_owned()
    }

    /// Polls `result` until the turn's envelope is durable.
    fn await_result(&self, session: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let output = self.run(&["result", session, "--json"], Duration::from_secs(5));
            if output.status.success() {
                return serde_json::from_slice(&output.stdout).unwrap();
            }
            assert!(Instant::now() < deadline, "no result within {timeout:?}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Returns the session's committed C1 events in page order.
    fn events(&self, session: &str) -> Vec<Value> {
        let output = self.run(&["events", session, "--json"], Duration::from_secs(10));
        assert!(output.status.success(), "{output:?}");
        let page: Value = serde_json::from_slice(&output.stdout).unwrap();
        page["events"].as_array().unwrap().clone()
    }

    /// Resolves an event `raw_ref` to its stream code and exact durable bytes.
    fn raw_at(&self, reference: &Value) -> (u8, Vec<u8>) {
        let offset = reference["offset"].as_u64().unwrap();
        let len = reference["len"].as_u64().unwrap();
        let mut position = 0;
        for (code, bytes) in self.raw_units() {
            if position == offset && bytes.len() as u64 == len {
                return (code, bytes);
            }
            position += bytes.len() as u64;
        }
        panic!("raw_ref {reference} is not a durable unit");
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

fn read_all(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        bytes
    })
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

/// C2 A1: at most 256 KiB per encoded observation payload.
const OBSERVATION_BYTES: usize = 256 * 1024;

/// Checks the common C1 §6.1 fields, dense sequence and the envelope's event range.
fn assert_dense(events: &[Value], session: &str, envelope: &Value) {
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["seq"], json!(index + 1), "{event}");
        assert_eq!(event["session_id"], session, "{event}");
        assert_eq!(event["turn"], 1, "{event}");
        assert_eq!(event["late"], false, "{event}");
        assert!(event.get("raw_ref").is_some(), "{event}");
    }
    let last = events.last().unwrap();
    assert_eq!(last["type"], "turn.ended", "turn.ended is the last event");
    assert_eq!(last["state"], envelope["state"]);
    assert_eq!(last["failure"], envelope["failure"]);
    assert_eq!(
        envelope["events"],
        json!({"first_seq":1,"last_seq":events.len(),"count":events.len()})
    );
}

/// Asserts that a raw reference is an exact stdout frame equal to `message`.
fn assert_frame(sandbox: &Sandbox, reference: &Value, message: &Value) {
    let (code, bytes) = sandbox.raw_at(reference);
    assert_eq!(code, STDOUT, "{reference}");
    assert_eq!(bytes.last(), Some(&b'\n'), "{reference}");
    assert_eq!(
        &serde_json::from_slice::<Value>(&bytes).unwrap(),
        message,
        "{reference}"
    );
}

#[test]
fn observations_become_ordered_events_with_exact_raw_refs() {
    let big_text = "é".repeat(140_000);
    let messages = [
        json!({"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}),
        json!({"type":"text","vendor_turn_id":"fake-turn-1","text":"hello "}),
        json!({"type":"text","vendor_turn_id":"fake-turn-1","text":big_text}),
        json!({"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t1","name":"shell","input_summary":"ls"}),
        json!({"type":"tool_ended","vendor_turn_id":"fake-turn-1","tool_id":"t1","status":"failed","output_summary":"no such file","exit_code":2}),
        json!({"type":"vendor_note","blob":"x".repeat(20_000)}),
        json!({"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"done","stop_reason":"end_turn"}),
        json!({"type":"later_note","n":1}),
    ];
    let steps: Vec<Value> = messages
        .iter()
        .map(|message| json!({"action":"emit","message":message}))
        .collect();
    let sandbox = Sandbox::new(&steps);
    let envelope = sandbox.spawn_turn();
    assert_eq!(envelope["state"], "completed", "{envelope}");
    let session = envelope["session_id"].as_str().unwrap();
    let events = sandbox.events(session);
    let types: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "turn.queued",
            "turn.submitted",
            "turn.started",
            "assistant.text",
            "assistant.text",
            "assistant.text",
            "tool.started",
            "tool.ended",
            "vendor.other",
            "vendor.other",
            "turn.ended",
        ]
    );
    assert_dense(&events, session, &envelope);
    assert_frame(&sandbox, &events[2]["raw_ref"], &messages[0]);

    // Small text keeps its payload; oversized text is split in order at UTF-8
    // boundaries, and every piece cites the one frame it came from.
    assert_eq!(events[3]["text"], "hello ");
    assert_eq!(events[3]["final"], false);
    assert_frame(&sandbox, &events[3]["raw_ref"], &messages[1]);
    let mut joined = String::new();
    for piece in &events[4..6] {
        let payload = json!({"text":piece["text"],"final":piece["final"]});
        assert!(serde_json::to_vec(&payload).unwrap().len() <= OBSERVATION_BYTES);
        assert_eq!(piece["raw_ref"], events[4]["raw_ref"]);
        joined.push_str(piece["text"].as_str().unwrap());
    }
    assert_eq!(joined, big_text);
    assert_frame(&sandbox, &events[4]["raw_ref"], &messages[2]);

    assert_eq!(events[6]["tool_id"], "t1");
    assert_eq!(events[6]["name"], "shell");
    assert_eq!(events[6]["input_summary"], "ls");
    assert_frame(&sandbox, &events[6]["raw_ref"], &messages[3]);
    assert_eq!(events[7]["tool_id"], "t1");
    assert_eq!(events[7]["status"], "failed");
    assert_eq!(events[7]["output_summary"], "no such file");
    assert_eq!(events[7]["exit_code"], 2);
    assert_frame(&sandbox, &events[7]["raw_ref"], &messages[4]);

    // Unknown payloads keep a bounded prefix with an explicit truncation marker.
    for (event, message, truncated) in [
        (&events[8], &messages[5], true),
        (&events[9], &messages[7], false),
    ] {
        assert_eq!(event["vendor_type"], message["type"]);
        assert_eq!(event["truncated"], truncated, "{event}");
        let payload = event["payload"].as_str().unwrap();
        assert!(payload.len() <= 16 * 1024);
        let (_, frame) = sandbox.raw_at(&event["raw_ref"]);
        assert!(frame.starts_with(payload.as_bytes()));
        assert_frame(&sandbox, &event["raw_ref"], message);
    }
    assert_frame(&sandbox, &events[10]["raw_ref"], &messages[6]);

    // The envelope's bounding span covers every cited reference.
    let spans = envelope["raw_spans"].as_array().unwrap();
    assert_eq!(spans.len(), 1, "{envelope}");
    let first = spans[0]["first_offset"].as_u64().unwrap();
    let last = spans[0]["last_offset"].as_u64().unwrap();
    for event in &events[2..] {
        let reference = &event["raw_ref"];
        let offset = reference["offset"].as_u64().unwrap();
        assert!(first <= offset && offset + reference["len"].as_u64().unwrap() <= last);
    }
}

#[test]
fn oversized_stdout_line_is_retained_as_raw_evidence() {
    // Exactly 1 MiB before the LF: the line is complete only once it exceeds the cap.
    let sandbox = Sandbox::new(&[
        emit(ACCEPTED),
        json!({"action":"flood","text":"x".repeat(1024),"count":1024}),
        json!({"action":"emit_raw","text":"\nafter-oversized\n"}),
        emit(TERMINAL),
    ]);
    let envelope = sandbox.spawn_turn();
    assert_eq!(envelope["state"], "failed", "{envelope}");
    assert_eq!(envelope["failure"]["class"], "protocol", "{envelope}");
    let stdout = sandbox.stream(STDOUT);
    let mut line = vec![b'x'; 1024 * 1024];
    line.extend_from_slice(b"\nafter-oversized\n");
    assert!(contains(&stdout, &line), "oversized line lost from raw log");
    // Every byte was recorded, so the raw log is complete.
    let events = sandbox.events(envelope["session_id"].as_str().unwrap());
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "raw_log.incomplete")
    );
    assert!(
        envelope["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|warning| warning["code"] != "raw_log_incomplete"),
        "{envelope}"
    );
}

/// Runs one failing fixture and checks its C1 §8.2 class and the shared event shape.
fn failed_turn(steps: &[Value], class: &str) -> (Sandbox, Value, Vec<Value>) {
    let sandbox = Sandbox::new(steps);
    let envelope = sandbox.spawn_turn();
    assert_eq!(envelope["state"], "failed", "{envelope}");
    assert_eq!(envelope["failure"]["class"], class, "{envelope}");
    let events = sandbox.events(envelope["session_id"].as_str().unwrap());
    assert_dense(&events, envelope["session_id"].as_str().unwrap(), &envelope);
    (sandbox, envelope, events)
}

#[test]
fn failure_class_protocol_cites_the_malformed_frame() {
    let malformed = json!({"type":"text","vendor_turn_id":"fake-turn-1"});
    let (sandbox, envelope, events) = failed_turn(
        &[
            emit(ACCEPTED),
            json!({"action":"emit","message":malformed}),
            emit(TERMINAL),
        ],
        "protocol",
    );
    assert_eq!(envelope["stop_reason"], "error");
    assert_frame(&sandbox, &events.last().unwrap()["raw_ref"], &malformed);
}

#[test]
fn failure_class_protocol_for_oversized_tool_payload() {
    let tool = json!({"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t1","name":"shell","input_summary":"y".repeat(300 * 1024)});
    let (sandbox, _, events) = failed_turn(
        &[
            emit(ACCEPTED),
            json!({"action":"emit","message":tool}),
            emit(TERMINAL),
        ],
        "protocol",
    );
    assert!(events.iter().all(|event| event["type"] != "tool.started"));
    assert_frame(&sandbox, &events.last().unwrap()["raw_ref"], &tool);
}

#[test]
fn failure_class_process_exited_before_acceptance() {
    let (_, envelope, events) = failed_turn(&[json!({"action":"exit","code":3})], "process_exited");
    assert_eq!(envelope["exit"]["code"], 3, "{envelope}");
    assert!(envelope["timestamps"]["accepted_at"].is_null());
    assert!(events.iter().all(|event| event["type"] != "turn.started"));
}

#[test]
fn failure_class_process_exited_after_acceptance() {
    let (_, envelope, events) = failed_turn(
        &[emit(ACCEPTED), json!({"action":"exit","code":4})],
        "process_exited",
    );
    assert_eq!(envelope["exit"]["code"], 4, "{envelope}");
    assert_eq!(events[2]["type"], "turn.started");
}

#[test]
fn failure_class_process_exited_after_completed_terminal() {
    let (_, envelope, _) = failed_turn(
        &[
            emit(ACCEPTED),
            emit(TERMINAL),
            json!({"action":"exit","code":5}),
        ],
        "process_exited",
    );
    assert_eq!(envelope["exit"]["code"], 5, "{envelope}");
}

#[test]
fn failure_class_vendor_error_keeps_vendor_code() {
    let terminal = json!({"type":"terminal","vendor_turn_id":"fake-turn-1","status":"failed","final_text":"","stop_reason":"error","vendor_code":"E42"});
    let (_, envelope, _) = failed_turn(
        &[emit(ACCEPTED), json!({"action":"emit","message":terminal})],
        "vendor_error",
    );
    assert_eq!(envelope["failure"]["vendor_code"], "E42", "{envelope}");
}

#[test]
fn failure_class_deadline_wall_after_hang() {
    let sandbox = Sandbox::new(&[emit(ACCEPTED), json!({"action":"hang"})]);
    let session = sandbox.spawn_background();
    let envelope = sandbox.await_result(&session, Duration::from_secs(45));
    assert_eq!(envelope["state"], "failed", "{envelope}");
    assert_eq!(envelope["failure"]["class"], "deadline_wall", "{envelope}");
    assert_eq!(envelope["stop_reason"], "deadline", "{envelope}");
    assert_dense(&sandbox.events(&session), &session, &envelope);
}
