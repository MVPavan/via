//! Replay mode (adapters design §8 item 3): the fake started as `<dir>/<name>`
//! beside `<dir>/<name>.replay.json` replays that recorded vendor fixture.

use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Exit code of every replay failure.
const FAILED: i32 = 3;
const MIB: usize = 1024 * 1024;
/// Outer supervision bound for any one fake run.
const OUTER: Duration = Duration::from_secs(10);
/// Most stdout bytes a test keeps; the rest is drained and dropped.
const STDOUT_KEEP: usize = 4 * MIB;
/// Most stderr bytes a test keeps; the rest is drained and dropped.
const STDERR_KEEP: usize = 64 * 1024;

/// Stdout drained so far (its first [`STDOUT_KEEP`] bytes), and whether it closed.
#[derive(Default)]
struct Collected {
    bytes: Vec<u8>,
    closed: bool,
}

type Shared = Arc<(Mutex<Collected>, Condvar)>;

/// A running fake whose stdout and stderr are drained continuously, so it
/// never blocks on a full pipe.
struct Run {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Shared,
    /// Bytes of stdout already returned by [`Run::next_line`].
    cursor: usize,
    stderr: Option<JoinHandle<Vec<u8>>>,
}

impl Drop for Run {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            // A failed test still must not leave the fake running. Cleanup is best-effort.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct Finished {
    code: Option<i32>,
    /// Every stdout line from the start of the run.
    stdout: Vec<Vec<u8>>,
    stderr: String,
}

/// Installs the fake as `<root>/vendor` beside `vendor.replay.json`.
fn install(root: &Path, fixture: &Value) -> TestResult<PathBuf> {
    let binary = root.join("vendor");
    symlink(env!("CARGO_BIN_EXE_via-fake-agent"), &binary)?;
    fs::write(
        root.join("vendor.replay.json"),
        serde_json::to_vec(fixture)?,
    )?;
    Ok(binary)
}

fn spawn<S: AsRef<OsStr>>(binary: &Path, args: &[S]) -> TestResult<Run> {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let shared: Shared = Arc::default();
    let collector = Arc::clone(&shared);
    // Ends at EOF, which the fake's exit (or the kill in Drop) guarantees.
    thread::spawn(move || {
        let mut chunk = vec![0_u8; 64 * 1024];
        loop {
            // A read error ends the stream like EOF; the test then sees it closed.
            let read = stdout.read(&mut chunk).unwrap_or(0);
            let (lock, ready) = &*collector;
            let Ok(mut collected) = lock.lock() else {
                return;
            };
            if read == 0 {
                collected.closed = true;
                ready.notify_all();
                return;
            }
            let room = STDOUT_KEEP.saturating_sub(collected.bytes.len());
            collected.bytes.extend_from_slice(&chunk[..read.min(room)]);
            ready.notify_all();
        }
    });
    let stderr = thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0_u8; 8192];
        while let Ok(read @ 1..) = stderr.read(&mut chunk) {
            let room = STDERR_KEEP.saturating_sub(kept.len());
            kept.extend_from_slice(&chunk[..read.min(room)]);
        }
        kept
    });
    Ok(Run {
        child,
        stdin,
        stdout: shared,
        cursor: 0,
        stderr: Some(stderr),
    })
}

impl Run {
    fn send(&mut self, line: &Value) -> TestResult {
        let stdin = self.stdin.as_mut().ok_or("stdin closed")?;
        writeln!(stdin, "{line}")?;
        Ok(())
    }

    /// The next complete stdout line, waiting at most [`OUTER`].
    fn next_line(&mut self) -> TestResult<Vec<u8>> {
        let deadline = Instant::now() + OUTER;
        let (lock, ready) = &*self.stdout;
        let mut collected = lock.lock().map_err(|_| "stdout collector poisoned")?;
        loop {
            let pending = &collected.bytes[self.cursor..];
            if let Some(at) = pending.iter().position(|byte| *byte == b'\n') {
                let line = pending[..=at].to_vec();
                self.cursor += at + 1;
                return Ok(line);
            }
            if collected.closed {
                return Err("stdout closed before a complete line".into());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("timed out waiting for a stdout line".into());
            }
            collected = ready
                .wait_timeout(collected, left)
                .map_err(|_| "stdout collector poisoned")?
                .0;
        }
    }

    /// Waits for exit within [`OUTER`], killing and reaping on expiry.
    /// `close_stdin` false keeps stdin open so only the fake can end the run.
    fn finish(mut self, close_stdin: bool) -> TestResult<Finished> {
        if close_stdin {
            drop(self.stdin.take());
        }
        let deadline = Instant::now() + OUTER;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                self.child.wait()?;
                return Err("fake outlived the outer deadline".into());
            }
            thread::sleep(Duration::from_millis(5));
        };
        let bytes = {
            let (lock, ready) = &*self.stdout;
            let mut collected = lock.lock().map_err(|_| "stdout collector poisoned")?;
            while !collected.closed {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err("stdout never closed".into());
                }
                collected = ready
                    .wait_timeout(collected, left)
                    .map_err(|_| "stdout collector poisoned")?
                    .0;
            }
            std::mem::take(&mut collected.bytes)
        };
        let stderr = self
            .stderr
            .take()
            .ok_or("stderr taken")?
            .join()
            .map_err(|_| "stderr reader panicked")?;
        Ok(Finished {
            code: status.code(),
            stdout: bytes
                .split_inclusive(|byte| *byte == b'\n')
                .map(<[u8]>::to_vec)
                .collect(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

fn line(text: &str) -> Vec<u8> {
    format!("{text}\n").into_bytes()
}

fn fixture(argv: &Value, deadline_ms: u64, steps: &Value) -> Value {
    json!({"argv": argv, "deadline_ms": deadline_ms, "steps": steps})
}

/// Runs a fixture with no argv and no input, returning its end.
fn run_closed(steps: &Value, deadline_ms: u64) -> TestResult<Finished> {
    let root = tempfile::tempdir()?;
    let binary = install(root.path(), &fixture(&json!([]), deadline_ms, steps))?;
    spawn::<&str>(&binary, &[])?.finish(true)
}

#[test]
fn replay_expected_line_mismatch_fails_naming_the_step() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!(["-p"]),
            10_000,
            &json!([
                {"emit": {"line": "{\"type\":\"system\"}"}},
                {"expect": {"line": {"type": "user", "message": {"content": "hello"}}}}
            ]),
        ),
    )?;
    let mut run = spawn(&binary, &["-p"])?;
    run.send(&json!({"type": "user", "message": {"content": "goodbye"}}))?;
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stderr.contains("step 2"), "{}", end.stderr);
    Ok(())
}

#[test]
fn replay_full_fixture_captures_and_substitutes_ids() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &json!({
            "source": "test fixture",
            "argv": ["app-server", "--listen", "stdio://"],
            "deadline_ms": 10_000,
            "steps": [
                {"expect": {"line": {"method": "initialize"}, "capture": {"init": "/id"}}},
                {"emit": {"line": "{\"id\":${init},\"result\":{}}"}},
                {"expect": {
                    "line": {"method": "thread/start", "params": {"cwd": "/work"}},
                    "capture": {"start": "/id"}
                }},
                {"delay": {"ms": 1}},
                {"emit": {"line": "{\"id\":${start},\"result\":{\"thread\":\"t-1\"}}"}}
            ]
        }),
    )?;
    let mut run = spawn(&binary, &["app-server", "--listen", "stdio://"])?;
    run.send(&json!({"id": 0, "method": "initialize", "params": {}}))?;
    assert_eq!(run.next_line()?, line("{\"id\":0,\"result\":{}}"));
    run.send(
        &json!({"id": "req-7", "method": "thread/start", "params": {"cwd": "/work", "x": 1}}),
    )?;
    assert_eq!(
        run.next_line()?,
        line("{\"id\":\"req-7\",\"result\":{\"thread\":\"t-1\"}}")
    );
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(0), "{}", end.stderr);
    Ok(())
}

#[test]
fn replay_argv_capture_binds_a_generated_session_id() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!(["--session-id", {"capture": "sid"}, "-p"]),
            10_000,
            &json!([
                {"emit": {"line": "{\"type\":\"system\",\"session_id\":\"${sid}\"}"}},
                {"expect": {"line": {"type": "user", "session_id": "${sid}"}}},
                {"emit": {"line": "{\"type\":\"result\",\"session_id\":\"${sid}\"}"}}
            ]),
        ),
    )?;
    let sid = "0b7e2c4a-1f7d-4c52-9b1e-3d7d2f1a9c55";
    let mut run = spawn(&binary, &["--session-id", sid, "-p"])?;
    assert_eq!(
        run.next_line()?,
        line(&format!("{{\"type\":\"system\",\"session_id\":\"{sid}\"}}"))
    );
    run.send(&json!({"type": "user", "session_id": sid}))?;
    assert_eq!(
        run.next_line()?,
        line(&format!("{{\"type\":\"result\",\"session_id\":\"{sid}\"}}"))
    );
    assert_eq!(run.finish(true)?.code, Some(0));

    // The bound argument still has to match on the next expect, and the
    // other arguments are still checked exactly.
    let mut run = spawn(&binary, &["--session-id", sid, "-p"])?;
    run.next_line()?;
    run.send(&json!({"type": "user", "session_id": "other"}))?;
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stderr.contains("step 2"), "{}", end.stderr);
    let end = spawn(&binary, &["--session-id", sid, "--print"])?.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stderr.contains("argv mismatch"), "{}", end.stderr);
    Ok(())
}

#[test]
fn replay_checks_argv_exactly_and_answers_version() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &json!({"argv": ["-p", "--verbose"], "version": "9.9.9 (Vendor)", "deadline_ms": 10_000, "steps": []}),
    )?;
    let version = spawn(&binary, &["--version"])?.finish(true)?;
    assert_eq!(version.code, Some(0));
    assert_eq!(version.stdout, vec![line("9.9.9 (Vendor)")]);

    let wrong = spawn(&binary, &["-p"])?.finish(true)?;
    assert_eq!(wrong.code, Some(FAILED), "argv must match exactly");
    assert!(wrong.stderr.contains("argv mismatch"), "{}", wrong.stderr);

    let invalid = [OsStr::new("-p"), OsStr::from_bytes(b"--verb\xffose")];
    let not_utf8 = spawn(&binary, &invalid)?.finish(true)?;
    assert_eq!(not_utf8.code, Some(FAILED));
    assert!(
        not_utf8.stderr.contains("argv mismatch"),
        "{}",
        not_utf8.stderr
    );

    let right = spawn(&binary, &["-p", "--verbose"])?.finish(true)?;
    assert_eq!(right.code, Some(0));
    Ok(())
}

#[test]
fn replay_signal_gated_step_waits_for_the_signal() -> TestResult {
    let steps = json!([
        {"emit": {"line": "ready"}},
        {"await_signal": {"signal": "SIGINT"}},
        {"emit": {"line": "interrupted"}}
    ]);
    let root = tempfile::tempdir()?;
    let binary = install(root.path(), &fixture(&json!([]), 10_000, &steps))?;
    let mut run = spawn::<&str>(&binary, &[])?;
    assert_eq!(run.next_line()?, line("ready"));
    let kill = Command::new("kill")
        .args(["-INT", &run.child.id().to_string()])
        .status()?;
    assert!(kill.success());
    assert_eq!(run.next_line()?, line("interrupted"));
    assert_eq!(run.finish(true)?.code, Some(0));

    // Without the signal the step never completes; the deadline ends the run.
    let end = run_closed(&steps, 300)?;
    assert_eq!(end.code, Some(FAILED));
    assert_eq!(end.stdout, vec![line("ready")]);
    Ok(())
}

#[test]
fn replay_delay_longer_than_the_deadline_fails() -> TestResult {
    let end = run_closed(
        &json!([{"delay": {"ms": 5_000}}, {"emit": {"line": "late"}}]),
        200,
    )?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stdout.is_empty());
    Ok(())
}

#[test]
fn replay_steps_share_one_whole_run_deadline() -> TestResult {
    // Each delay fits the deadline on its own; together they exceed it.
    let end = run_closed(
        &json!([
            {"delay": {"ms": 150}},
            {"emit": {"line": "one"}},
            {"delay": {"ms": 150}},
            {"emit": {"line": "two"}},
            {"delay": {"ms": 150}},
            {"emit": {"line": "three"}}
        ]),
        300,
    )?;
    assert_eq!(end.code, Some(FAILED));
    assert!(!end.stdout.contains(&line("three")), "{:?}", end.stdout);
    Ok(())
}

#[test]
fn replay_whole_run_deadline_fails_naming_the_step() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            1_000,
            &json!([
                {"emit": {"line": "ready"}},
                {"expect": {"line": {"type": "user"}}},
                {"emit": {"line": "late"}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    assert_eq!(run.next_line()?, line("ready"));
    // Stdin stays open, so only the deadline can end the run.
    let end = run.finish(false)?;
    assert_eq!(end.code, Some(FAILED));
    assert_eq!(end.stdout, vec![line("ready")]);
    // The message is best-effort by design; check its content only if it came.
    if end.stderr.contains("deadline") {
        // The watchdog may sample the step before or after "ready" is written.
        assert!(
            ["step 0", "step 1", "step 2"]
                .iter()
                .any(|step| end.stderr.contains(step)),
            "{}",
            end.stderr
        );
    }
    Ok(())
}

#[test]
fn replay_bounds_expected_value_including_plain_strings() -> TestResult {
    // A plain 600 KiB string and a 600 KiB substitution exceed 1 MiB together.
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            10_000,
            &json!([
                {"expect": {"line": {}, "capture": {"pad": "/pad"}}},
                {"expect": {"line": {"plain": "b".repeat(600 * 1024), "sub": "${pad}"}}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    run.send(&json!({"pad": "a".repeat(600 * 1024)}))?;
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(
        end.stderr.contains("step 2") && end.stderr.contains("substitution exceeds"),
        "{}",
        end.stderr
    );
    Ok(())
}

#[test]
fn replay_rejects_input_lines_over_one_mebibyte() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            10_000,
            &json!([{"expect": {"line": {"type": "user"}}}]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    let text = format!("{{\"type\":\"user\",\"pad\":\"{}\"}}\n", "a".repeat(MIB));
    // The fake may exit before reading all of it; a broken pipe is expected.
    if let Some(stdin) = run.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stderr.contains("step 1"), "{}", end.stderr);
    Ok(())
}

#[test]
fn replay_bounds_step_count() -> TestResult {
    let delay = json!({"delay": {"ms": 0}});
    let at_bound = run_closed(&Value::Array(vec![delay.clone(); 10_000]), 10_000)?;
    assert_eq!(at_bound.code, Some(0), "{}", at_bound.stderr);
    let over = run_closed(&Value::Array(vec![delay; 10_001]), 10_000)?;
    assert_eq!(over.code, Some(FAILED));
    assert!(over.stderr.contains("steps"), "{}", over.stderr);
    Ok(())
}

#[test]
fn replay_bounds_output_lines_after_substitution() -> TestResult {
    let at_bound = run_closed(&json!([{"emit": {"line": "a".repeat(MIB)}}]), 10_000)?;
    assert_eq!(at_bound.code, Some(0), "{}", at_bound.stderr);
    assert_eq!(at_bound.stdout, vec![line(&"a".repeat(MIB))]);
    let over = run_closed(&json!([{"emit": {"line": "a".repeat(MIB + 1)}}]), 10_000)?;
    assert_eq!(over.code, Some(FAILED));
    assert!(over.stderr.contains("step 1"), "{}", over.stderr);

    // A short template whose captures expand past the bound fails too.
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            10_000,
            &json!([
                {"expect": {"line": {}, "capture": {"pad": "/pad"}}},
                {"emit": {"line": "${pad}${pad}"}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    run.send(&json!({"pad": "a".repeat(600 * 1024)}))?;
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stderr.contains("step 2"), "{}", end.stderr);
    assert!(end.stdout.is_empty());
    Ok(())
}

#[test]
fn replay_bounds_version_output() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &json!({"argv": [], "version": "v".repeat(MIB + 1), "deadline_ms": 10_000, "steps": []}),
    )?;
    let end = spawn(&binary, &["--version"])?.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stdout.is_empty());
    Ok(())
}

#[test]
fn replay_bounds_captures() -> TestResult {
    // Two names retaining the same 600 KiB value exceed the aggregate bound.
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            10_000,
            &json!([{"expect": {"line": {}, "capture": {"a": "/pad", "b": "/pad"}}}]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    run.send(&json!({"pad": "a".repeat(600 * 1024)}))?;
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stderr.contains("step 1"), "{}", end.stderr);

    let names = |count: usize| {
        (0..count)
            .map(|index| (format!("c{index}"), json!("/id")))
            .collect::<serde_json::Map<_, _>>()
    };
    let at_bound = run_closed(
        &json!([{"expect": {"line": {}, "capture": names(64)}}]),
        10_000,
    )?;
    // It passes loading and then fails only because stdin is closed.
    assert!(
        at_bound.stderr.contains("stdin ended"),
        "{}",
        at_bound.stderr
    );
    let over = run_closed(
        &json!([{"expect": {"line": {}, "capture": names(65)}}]),
        10_000,
    )?;
    assert_eq!(over.code, Some(FAILED));
    assert!(over.stderr.contains("capture names"), "{}", over.stderr);
    Ok(())
}

#[test]
fn replay_bounds_fixture_size() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = root.path().join("vendor");
    symlink(env!("CARGO_BIN_EXE_via-fake-agent"), &binary)?;
    let pad = " ".repeat(16 * MIB);
    fs::write(
        root.path().join("vendor.replay.json"),
        format!("{{\"argv\":[],\"deadline_ms\":10000,\"steps\":[]}}{pad}"),
    )?;
    let end = spawn::<&str>(&binary, &[])?.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stderr.contains("exceeds"), "{}", end.stderr);
    Ok(())
}

#[test]
fn replay_escape_writes_and_matches_a_literal_placeholder() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            10_000,
            &json!([
                {"emit": {"line": "{\"command\":\"echo $${HOME}\"}"}},
                {"expect": {"line": {"command": "echo $${HOME}"}}},
                {"emit": {"line": "matched"}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    assert_eq!(run.next_line()?, line("{\"command\":\"echo ${HOME}\"}"));
    run.send(&json!({"command": "echo ${HOME}"}))?;
    assert_eq!(run.next_line()?, line("matched"));
    assert_eq!(run.finish(true)?.code, Some(0));
    Ok(())
}

#[test]
fn replay_bounds_expected_value_substitution_in_aggregate() -> TestResult {
    // Each element fits the line bound; together they exceed it.
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            10_000,
            &json!([
                {"expect": {"line": {}, "capture": {"pad": "/pad"}}},
                {"expect": {"line": {"items": vec!["${pad}"; 10_000]}}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    run.send(&json!({"pad": "a".repeat(600 * 1024)}))?;
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(
        end.stderr.contains("step 2") && end.stderr.contains("substitution exceeds"),
        "{}",
        end.stderr
    );
    Ok(())
}

#[test]
fn replay_await_eof_passes_at_eof_and_fails_on_input() -> TestResult {
    let steps = json!([{"await_eof": {}}, {"emit": {"line": "done"}}]);
    let end = run_closed(&steps, 10_000)?;
    assert_eq!(end.code, Some(0), "{}", end.stderr);
    assert_eq!(end.stdout, vec![line("done")]);

    let root = tempfile::tempdir()?;
    let binary = install(root.path(), &fixture(&json!([]), 10_000, &steps))?;
    let mut run = spawn::<&str>(&binary, &[])?;
    run.send(&json!({"type": "user"}))?;
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stdout.is_empty());
    assert!(
        end.stderr.contains("step 1") && end.stderr.contains("unexpected input while awaiting EOF"),
        "{}",
        end.stderr
    );

    // With stdin left open, only the run deadline ends the wait.
    let root = tempfile::tempdir()?;
    let binary = install(root.path(), &fixture(&json!([]), 300, &steps))?;
    let end = spawn::<&str>(&binary, &[])?.finish(false)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stdout.is_empty());
    Ok(())
}

#[test]
fn replay_absent_pointer_fails_when_it_resolves() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            10_000,
            &json!([
                {"expect": {"line": {"type": "user"}, "absent": ["/session_id", "/message/model"]}},
                {"emit": {"line": "ok"}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    run.send(&json!({"type": "user", "message": {"content": "hi"}}))?;
    assert_eq!(run.next_line()?, line("ok"));
    assert_eq!(run.finish(true)?.code, Some(0));

    for sent in [
        json!({"type": "user", "session_id": null}),
        json!({"type": "user", "message": {"model": "m"}}),
    ] {
        let mut run = spawn::<&str>(&binary, &[])?;
        run.send(&sent)?;
        let end = run.finish(true)?;
        assert_eq!(end.code, Some(FAILED), "{sent}");
        assert!(end.stdout.is_empty());
        assert!(
            end.stderr.contains("step 1") && end.stderr.contains("must be absent"),
            "{}",
            end.stderr
        );
    }
    Ok(())
}

#[test]
fn replay_within_ms_bounds_the_wait_for_an_expected_line() -> TestResult {
    // A missed 300 ms limit fails on its own name, long before the 3 s run
    // deadline; a longer limit would fail naming the deadline instead.
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            3_000,
            &json!([
                {"emit": {"line": "ready"}},
                {"expect": {"line": {"type": "decline"}, "within_ms": 300}},
                {"emit": {"line": "late"}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    assert_eq!(run.next_line()?, line("ready"));
    let end = run.finish(false)?;
    assert_eq!(end.code, Some(FAILED));
    assert_eq!(end.stdout, vec![line("ready")]);
    assert!(
        end.stderr.contains("step 2: within_ms") && !end.stderr.contains("deadline"),
        "{}",
        end.stderr
    );

    // It counts from the previous step's completion, not from the start.
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            10_000,
            &json!([
                {"delay": {"ms": 500}},
                {"expect": {"line": {"type": "decline"}, "within_ms": 300}},
                {"emit": {"line": "ok"}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    run.send(&json!({"type": "decline"}))?;
    assert_eq!(run.next_line()?, line("ok"));
    assert_eq!(run.finish(true)?.code, Some(0));
    Ok(())
}

#[test]
fn replay_exit_step_writes_stderr_and_exits_with_its_code() -> TestResult {
    let end = run_closed(
        &json!([
            {"emit": {"line": "partial"}},
            {"exit": {"code": 7, "stderr": "Error: rate limited\n"}}
        ]),
        10_000,
    )?;
    assert_eq!(end.code, Some(7));
    assert_eq!(end.stdout, vec![line("partial")]);
    assert_eq!(end.stderr, "Error: rate limited\n");

    let at_bound = "e".repeat(1024);
    let end = run_closed(&json!([{"exit": {"code": 1, "stderr": at_bound}}]), 10_000)?;
    assert_eq!(end.code, Some(1));
    assert_eq!(end.stderr, at_bound);
    Ok(())
}

#[test]
fn replay_exit_step_is_checked_at_load() -> TestResult {
    // Each fixture is refused before its first step runs.
    for (steps, reason) in [
        (
            json!([{"emit": {"line": "x"}}, {"exit": {"code": 3, "stderr": ""}}]),
            "reserved",
        ),
        (
            json!([
                {"emit": {"line": "x"}},
                {"exit": {"code": 1, "stderr": ""}},
                {"emit": {"line": "y"}}
            ]),
            "last step",
        ),
        (
            json!([{"emit": {"line": "x"}}, {"exit": {"code": 1, "stderr": "e".repeat(1025)}}]),
            "1024 bytes",
        ),
    ] {
        let end = run_closed(&steps, 10_000)?;
        assert_eq!(end.code, Some(FAILED), "{steps}");
        assert!(end.stdout.is_empty(), "{steps}");
        assert!(end.stderr.contains(reason), "{}", end.stderr);
    }
    Ok(())
}

#[test]
fn replay_appends_each_start_to_the_launch_log() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &json!({"argv": [], "version": "1.0", "deadline_ms": 10_000, "steps": []}),
    )?;
    let mut pids = Vec::new();
    for args in [&[][..], &["--version"][..], &[][..]] {
        let run = spawn(&binary, args)?;
        pids.push(format!("{}\n", run.child.id()));
        assert_eq!(run.finish(true)?.code, Some(0));
    }
    assert_eq!(
        fs::read_to_string(root.path().join("vendor.launches"))?,
        pids.concat()
    );

    // A launch log that cannot be written fails the run.
    let root = tempfile::tempdir()?;
    let binary = install(root.path(), &fixture(&json!([]), 10_000, &json!([])))?;
    fs::create_dir(root.path().join("vendor.launches"))?;
    let end = spawn::<&str>(&binary, &[])?.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stderr.contains("launch log"), "{}", end.stderr);
    Ok(())
}

/// A fixture whose second step must see its line within 300 ms, run with a
/// 5 s deadline; the line is sent `wait` after "ready" is read.
fn within_ms_after(wait: Duration) -> TestResult<Finished> {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            5_000,
            &json!([
                {"emit": {"line": "ready"}},
                {"expect": {"line": {"type": "decline"}, "within_ms": 300}},
                {"emit": {"line": "got"}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    assert_eq!(run.next_line()?, line("ready"));
    thread::sleep(wait);
    // The fake may already have exited; then the write fails.
    let _ = run.send(&json!({"type": "decline"}));
    run.finish(false)
}

#[test]
fn replay_within_ms_late_line_fails_before_the_run_deadline() -> TestResult {
    let end = within_ms_after(Duration::from_millis(800))?;
    assert_eq!(end.code, Some(FAILED));
    assert_eq!(end.stdout, vec![line("ready")]);
    assert!(
        end.stderr.contains("step 2: within_ms") && !end.stderr.contains("deadline"),
        "{}",
        end.stderr
    );
    Ok(())
}

#[test]
fn replay_within_ms_limit_ends_with_its_step() -> TestResult {
    // The second expect has no limit of its own; its line comes after the
    // first one's 300 ms but well within the 5 s run deadline.
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            5_000,
            &json!([
                {"expect": {"line": {"type": "first"}, "within_ms": 300}},
                {"expect": {"line": {"type": "second"}}},
                {"emit": {"line": "ok"}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    run.send(&json!({"type": "first"}))?;
    thread::sleep(Duration::from_millis(800));
    run.send(&json!({"type": "second"}))?;
    assert_eq!(run.next_line()?, line("ok"));
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(0), "{}", end.stderr);
    Ok(())
}

#[test]
fn replay_await_eof_fails_when_stdin_closed_before_the_previous_step() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            10_000,
            &json!([
                {"expect": {"line": {"type": "user"}}},
                {"delay": {"ms": 1_000}},
                {"emit": {"line": "terminal"}},
                {"await_eof": {}}
            ]),
        ),
    )?;
    // Closed at once, before the terminal is emitted.
    let mut run = spawn::<&str>(&binary, &[])?;
    run.send(&json!({"type": "user"}))?;
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert_eq!(end.stdout, vec![line("terminal")]);
    assert!(
        end.stderr
            .contains("step 4: stdin closed before step 3 completed"),
        "{}",
        end.stderr
    );

    // Closed only after the terminal is read.
    let mut run = spawn::<&str>(&binary, &[])?;
    run.send(&json!({"type": "user"}))?;
    assert_eq!(run.next_line()?, line("terminal"));
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(0), "{}", end.stderr);
    Ok(())
}

#[test]
fn replay_trailing_input_after_the_last_expect_fails() -> TestResult {
    // The delay gives the reader time to see the extra line; nothing is
    // asserted about time.
    for steps in [
        json!([
            {"expect": {"line": {"type": "user"}}},
            {"delay": {"ms": 300}},
            {"exit": {"code": 1, "stderr": "vendor failed\n"}}
        ]),
        json!([
            {"expect": {"line": {"type": "user"}}},
            {"delay": {"ms": 300}},
            {"emit": {"line": "done"}}
        ]),
    ] {
        let root = tempfile::tempdir()?;
        let binary = install(root.path(), &fixture(&json!([]), 10_000, &steps))?;
        let mut run = spawn::<&str>(&binary, &[])?;
        run.send(&json!({"type": "user"}))?;
        run.send(&json!({"type": "user", "resent": true}))?;
        let end = run.finish(false)?;
        assert_eq!(end.code, Some(FAILED), "{steps}");
        assert!(
            end.stderr
                .contains("unexpected input after the last expect"),
            "{}",
            end.stderr
        );
        assert!(!end.stderr.contains("vendor failed"), "{}", end.stderr);
    }
    Ok(())
}

#[test]
fn replay_await_eof_fails_on_a_partial_line() -> TestResult {
    // Held open, the partial line is never a line, so the run deadline ends the wait.
    let steps = json!([{"await_eof": {}}, {"emit": {"line": "done"}}]);
    let root = tempfile::tempdir()?;
    let binary = install(root.path(), &fixture(&json!([]), 1_000, &steps))?;
    let mut run = spawn::<&str>(&binary, &[])?;
    run.stdin.as_mut().ok_or("stdin closed")?.write_all(b"{")?;
    let end = run.finish(false)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stdout.is_empty());

    // Closed after it, the partial line fails at EOF.
    let mut run = spawn::<&str>(&binary, &[])?;
    run.stdin.as_mut().ok_or("stdin closed")?.write_all(b"{")?;
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(FAILED));
    assert!(end.stdout.is_empty());
    Ok(())
}

#[test]
fn replay_concurrent_starts_each_log_one_launch() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(root.path(), &fixture(&json!([]), 10_000, &json!([])))?;
    let runs = (0..8)
        .map(|_| spawn::<&str>(&binary, &[]))
        .collect::<TestResult<Vec<_>>>()?;
    let mut pids: Vec<String> = runs.iter().map(|run| run.child.id().to_string()).collect();
    for run in runs {
        assert_eq!(run.finish(true)?.code, Some(0));
    }
    let log = fs::read_to_string(root.path().join("vendor.launches"))?;
    assert!(log.ends_with('\n'), "{log:?}");
    let mut logged: Vec<String> = log.lines().map(str::to_owned).collect();
    pids.sort();
    logged.sort();
    assert_eq!(logged, pids);
    Ok(())
}

#[test]
fn replay_expect_then_close_at_once_passes_await_eof() -> TestResult {
    // A correct adapter may write its last line and close stdin at once:
    // the expect completes when its line arrived, so the EOF is not early.
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &fixture(
            &json!([]),
            10_000,
            &json!([
                {"expect": {"line": {"type": "user"}}},
                {"await_eof": {}},
                {"emit": {"line": "done"}}
            ]),
        ),
    )?;
    let mut run = spawn::<&str>(&binary, &[])?;
    let mut stdin = run.stdin.take().ok_or("stdin closed")?;
    stdin.write_all(b"{\"type\":\"user\"}\n")?;
    drop(stdin);
    let end = run.finish(true)?;
    assert_eq!(end.code, Some(0), "{}", end.stderr);
    assert_eq!(end.stdout, vec![line("done")]);
    Ok(())
}
