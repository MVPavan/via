//! Turn data path through the real `via` daemon and fake vendor: post-terminal
//! drain (T1-I1), observation events (T1-I5), evidence files and failure classes.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test fixtures and assertions fail loudly"
)]

#[path = "support/evidenced.rs"]
mod evidenced;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
mod support;

use std::error::Error;
use std::fs;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use evidenced::evidenced;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const ACCEPTED: &str = r#"{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}"#;
const TERMINAL: &str = r#"{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"done","stop_reason":"end_turn"}"#;

/// One isolated daemon, State and fake fixture; the daemon is stopped on drop.
struct Sandbox {
    root: tempfile::TempDir,
    /// The scenario's evidence, collected when the sandbox is dropped.
    evidence: Option<support::evidence::Evidence>,
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
        let evidence = evidenced::open(&fake, &fixture).unwrap();
        Self {
            root,
            evidence: Some(evidence),
            via,
            fake,
            state,
            runtime,
            sync,
            fixture,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.via);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("VIA_STATE_DIR", &self.state)
            .env("VIA_RUNTIME_DIR", &self.runtime)
            .env("VIA_FAKE_AGENT_BINARY", &self.fake)
            .env("VIA_FAKE_SCENARIO", &self.fixture)
            .env("VIA_FAKE_SYNC_DIR", &self.sync);
        command
    }

    fn run(&self, args: &[&str], timeout: Duration) -> Output {
        let mut child = self
            .command()
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

    /// A file in the turn's evidence folder (Task 4 design §7.1).
    fn evidence_file(&self, session: &str, name: &str) -> Vec<u8> {
        fs::read(
            self.state
                .join("evidence")
                .join(session)
                .join("1")
                .join(name),
        )
        .unwrap()
    }

    /// Starts one background fake turn with `extra` flags and returns its
    /// session id.
    fn spawn_background(&self, extra: &[&str]) -> String {
        let mut args = vec![
            "spawn",
            "--harness",
            "fake",
            "--model",
            "fake",
            "--prompt",
            "hello",
            "--background",
            "--json",
        ];
        args.extend_from_slice(extra);
        let output = self.run(&args, Duration::from_secs(10));
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
}

impl Drop for Sandbox {
    /// Stops the auto-started daemon and proves it exited
    /// ([`evidenced::stop_daemons`]) before the evidence is collected.
    fn drop(&mut self) {
        let exited = evidenced::stop_daemons(&self.runtime, &self.state, |budget| {
            evidenced::run_within(
                self.command().args(["daemon", "stop", "--force", "--json"]),
                budget,
            );
        });
        if let Some(evidence) = self.evidence.take() {
            self.root.disable_cleanup(true);
            evidenced::park(
                evidence,
                self.root.path().to_owned(),
                &self.state,
                evidenced::Expected {
                    store: true,
                    folders: true,
                },
                exited,
            );
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

fn emit(message: &str) -> Value {
    json!({"action":"emit","message":serde_json::from_str::<Value>(message).unwrap()})
}

fn failure_text(envelope: &Value) -> String {
    envelope["failure"].to_string()
}

/// Task 4 design §7.1: VIA keeps no copy of vendor stdout; the vendor's
/// stderr after the terminal is in `stderr.log`, written by the OS, and a
/// late observation does not change the completed turn.
#[test]
fn route_drain_completes_with_late_tool_end_and_stderr_tail_in_stderr_log() -> TestResult {
    evidenced(|| {
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
        let session = envelope["session_id"].as_str().unwrap();
        assert_eq!(
            sandbox.evidence_file(session, "stderr.log"),
            b"stderr-tail-after-terminal\n"
        );
        Ok(())
    })
}

#[test]
fn route_drain_rejects_duplicate_terminal_after_late_observation() -> TestResult {
    evidenced(|| {
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
        Ok(())
    })
}

/// Task 4 design §7.1: a stderr flood after the terminal goes to
/// `stderr.log` whole and never stalls the turn.
#[test]
fn route_drain_survives_stderr_flood_after_terminal() -> TestResult {
    evidenced(|| {
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
        let stderr = sandbox.evidence_file(envelope["session_id"].as_str().unwrap(), "stderr.log");
        assert_eq!(stderr.len(), LINE * COUNT + b"flood-end\n".len());
        assert!(stderr.ends_with(b"flood-end\n"));
        Ok(())
    })
}

/// Checks the common C1 §6.1 fields, dense sequence and the envelope's event range.
fn assert_dense(events: &[Value], session: &str, envelope: &Value) {
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["seq"], json!(index + 1), "{event}");
        assert_eq!(event["session_id"], session, "{event}");
        assert_eq!(event["turn"], 1, "{event}");
        assert_eq!(event["late"], false, "{event}");
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

/// Task 4 design §7.3: the turn's `undecoded.bin` holds `message` and the
/// failure message names the file.
fn assert_undecoded(sandbox: &Sandbox, envelope: &Value, message: &Value) {
    let session = envelope["session_id"].as_str().unwrap();
    let bytes = sandbox.evidence_file(session, "undecoded.bin");
    assert_eq!(bytes.last(), Some(&b'\n'), "{envelope}");
    assert_eq!(&serde_json::from_slice::<Value>(&bytes).unwrap(), message);
    assert!(
        failure_text(envelope).contains(&format!("{session}/1/undecoded.bin")),
        "{envelope}"
    );
}

/// Task 4 design §2.1, §3.2: text, tool and unknown messages are not
/// events; the durable events stay dense, and the turn's one step has its
/// row, committed with `turn.ended`.
#[test]
fn observations_become_ordered_events() -> TestResult {
    evidenced(|| {
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
                "turn.ended",
            ]
        );
        assert_dense(&events, session, &envelope);
        let store = rusqlite::Connection::open_with_flags(
            sandbox.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let rows: Vec<u32> = store
            .prepare("SELECT step FROM steps WHERE session_id=?1 AND turn=1 ORDER BY step")
            .unwrap()
            .query_map([session], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // No model output followed the tool result: one step.
        assert_eq!(rows, [1]);
        Ok(())
    })
}

/// C1 §8.2, Task 4 design §7.3: a stdout line over the 1 MiB cap fails the
/// turn `overflow`; `undecoded.bin` keeps its first 64 KiB.
#[test]
fn oversized_stdout_line_fails_overflow_and_keeps_its_head() -> TestResult {
    evidenced(|| {
        // Exactly 1 MiB before the LF: the line is complete only once it exceeds the cap.
        let sandbox = Sandbox::new(&[
            emit(ACCEPTED),
            json!({"action":"flood","text":"x".repeat(1024),"count":1024}),
            json!({"action":"emit_raw","text":"\nafter-oversized\n"}),
            emit(TERMINAL),
        ]);
        let envelope = sandbox.spawn_turn();
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(envelope["failure"]["class"], "overflow", "{envelope}");
        let session = envelope["session_id"].as_str().unwrap();
        assert_eq!(
            sandbox.evidence_file(session, "undecoded.bin"),
            vec![b'x'; 64 * 1024]
        );
        assert!(
            failure_text(&envelope).contains(&format!("{session}/1/undecoded.bin")),
            "{envelope}"
        );
        Ok(())
    })
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
fn failure_class_protocol_cites_the_malformed_message() -> TestResult {
    evidenced(|| {
        // Task 4 design §2.2: a known message without its turn is malformed.
        let malformed = json!({"type":"text","text":"hi"});
        let (sandbox, envelope, _) = failed_turn(
            &[
                emit(ACCEPTED),
                json!({"action":"emit","message":malformed}),
                emit(TERMINAL),
            ],
            "protocol",
        );
        assert_eq!(envelope["stop_reason"], "error");
        assert_undecoded(&sandbox, &envelope, &malformed);
        Ok(())
    })
}

/// Task 4 design §2.2 rule 1: a tool name over 1 KiB is `protocol`; the
/// input summary is skipped unread, so only the name can be oversized.
#[test]
fn failure_class_protocol_for_oversized_tool_name() -> TestResult {
    evidenced(|| {
        let tool = json!({"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t1","name":"y".repeat(300 * 1024),"input_summary":"ls"});
        let (sandbox, envelope, events) = failed_turn(
            &[
                emit(ACCEPTED),
                json!({"action":"emit","message":tool}),
                emit(TERMINAL),
            ],
            "protocol",
        );
        assert!(events.iter().all(|event| event["type"] != "tool.started"));
        // Over 64 KiB: `undecoded.bin` keeps the message's head.
        let session = envelope["session_id"].as_str().unwrap();
        let head = sandbox.evidence_file(session, "undecoded.bin");
        assert_eq!(head.len(), 64 * 1024);
        assert!(head.starts_with(b"{\""), "{envelope}");
        assert!(
            failure_text(&envelope).contains("undecodable vendor message"),
            "{envelope}"
        );
        Ok(())
    })
}

#[test]
fn failure_class_process_exited_before_acceptance() -> TestResult {
    evidenced(|| {
        let (_, envelope, events) =
            failed_turn(&[json!({"action":"exit","code":3})], "process_exited");
        assert_eq!(envelope["exit"]["code"], 3, "{envelope}");
        assert!(envelope["timestamps"]["accepted_at"].is_null());
        assert!(events.iter().all(|event| event["type"] != "turn.started"));
        Ok(())
    })
}

#[test]
fn failure_class_process_exited_after_acceptance() -> TestResult {
    evidenced(|| {
        let (_, envelope, events) = failed_turn(
            &[emit(ACCEPTED), json!({"action":"exit","code":4})],
            "process_exited",
        );
        assert_eq!(envelope["exit"]["code"], 4, "{envelope}");
        assert_eq!(events[2]["type"], "turn.started");
        Ok(())
    })
}

/// S1-runtime2 fix round 2 (C1 §7.6 row 3, §7.5): a decoded `completed`
/// terminal is `completed` even when the vendor then exits 5; the exit
/// stays independent evidence (runtime §11.2 checks the fake's exit
/// separately).
#[test]
fn a_completed_terminal_then_a_failed_exit_is_completed() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&[
            emit(ACCEPTED),
            emit(TERMINAL),
            json!({"action":"exit","code":5}),
        ]);
        let envelope = sandbox.spawn_turn();
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["final_text"], "done", "{envelope}");
        assert!(envelope["failure"].is_null(), "{envelope}");
        assert_eq!(envelope["exit"]["code"], 5, "{envelope}");
        Ok(())
    })
}

/// S1-runtime2 fix round 2 (C1 §7.6 row 3, design §2 rule 3 [r1.23]): a
/// vendor that reports `completed` with `"done"` and then lives past the
/// wall deadline is force-closed by Route's late path; the turn stays
/// `completed` with its final text, and the forced exit (a signal) stays
/// evidence, never `failed(process_exited)`.
#[test]
fn a_completed_terminal_whose_vendor_outlives_the_wall_is_completed() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&[emit(ACCEPTED), emit(TERMINAL), json!({"action":"hang"})]);
        let session = sandbox.spawn_background(&["--wall-ms", "2000"]);
        let envelope = sandbox.await_result(&session, Duration::from_secs(30));
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["final_text"], "done", "{envelope}");
        assert!(envelope["failure"].is_null(), "{envelope}");
        assert!(envelope["exit"]["signal"].is_u64(), "{envelope}");
        Ok(())
    })
}

#[test]
fn failure_class_vendor_error_keeps_vendor_code() -> TestResult {
    evidenced(|| {
        let terminal = json!({"type":"terminal","vendor_turn_id":"fake-turn-1","status":"failed","final_text":"","stop_reason":"error","vendor_code":"E42"});
        let (_, envelope, _) = failed_turn(
            &[emit(ACCEPTED), json!({"action":"emit","message":terminal})],
            "vendor_error",
        );
        assert_eq!(envelope["failure"]["vendor_code"], "E42", "{envelope}");
        Ok(())
    })
}

#[test]
fn failure_class_deadline_wall_after_hang() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new(&[emit(ACCEPTED), json!({"action":"hang"})]);
        // The fake's default wall is C1's hour (A3): the test sets its own.
        let session = sandbox.spawn_background(&["--wall-ms", "30000"]);
        let envelope = sandbox.await_result(&session, Duration::from_secs(45));
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(envelope["failure"]["class"], "deadline_wall", "{envelope}");
        assert_eq!(envelope["stop_reason"], "deadline", "{envelope}");
        // C1 §7.6: Core's deadline cancels the turn, so `cancel` is filled with the
        // evidenced outcome: Route force-closed the live group and Host proved it
        // absent (W2-E Sol finding 2).
        assert_eq!(envelope["cancel"]["outcome"], "forced", "{envelope}");
        assert_eq!(envelope["cancel"]["cleanup"], "quiescent", "{envelope}");
        let events = sandbox.events(&session);
        assert_dense(&events, &session, &envelope);
        let tail: Vec<&Value> = events.iter().rev().take(3).rev().collect();
        assert_eq!(tail[0]["type"], "cancel.requested", "{}", tail[0]);
        assert_eq!(tail[1]["type"], "cancel.settled", "{}", tail[1]);
        assert_eq!(tail[1]["outcome"], envelope["cancel"]["outcome"]);
        assert_eq!(tail[1]["cleanup"], envelope["cancel"]["cleanup"]);
        assert_eq!(tail[2]["cancel"], envelope["cancel"], "{}", tail[2]);
        Ok(())
    })
}

/// F27: a vendor message that is not valid UTF-8 fails the turn with the
/// route's decode failure, `protocol`, and `undecoded.bin` holds the
/// message's bytes exactly, as the agent wrote them.
#[test]
fn failure_class_protocol_for_a_message_that_is_not_utf8() -> TestResult {
    evidenced(|| {
        let mut line = br#"{"type":"text","vendor_turn_id":"fake-turn-1","text":""#.to_vec();
        line.extend_from_slice(&[0xff, 0xfe, b'"', b'}', b'\n']);
        let (sandbox, envelope, _) = failed_turn(
            &[
                emit(ACCEPTED),
                json!({"action":"emit_bytes","bytes":line}),
                emit(TERMINAL),
            ],
            "protocol",
        );
        assert_eq!(envelope["stop_reason"], "error", "{envelope}");
        assert!(failure_text(&envelope).contains("not UTF-8"), "{envelope}");
        let session = envelope["session_id"].as_str().unwrap();
        assert_eq!(sandbox.evidence_file(session, "undecoded.bin"), line);
        assert!(
            failure_text(&envelope).contains(&format!("{session}/1/undecoded.bin")),
            "{envelope}"
        );
        Ok(())
    })
}
