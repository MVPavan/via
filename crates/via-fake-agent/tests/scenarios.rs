//! Exercise the test vendor itself before using it as runtime evidence.

use std::error::Error;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct SupervisedChild(Child);

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            // A test failure still must not leave the fake running. Cleanup is best-effort.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn fake(script: &serde_json::Value, root: &Path) -> TestResult<SupervisedChild> {
    let script_path = root.join("scenario.json");
    fs::write(&script_path, serde_json::to_vec(script)?)?;
    Ok(SupervisedChild(
        Command::new(env!("CARGO_BIN_EXE_via-fake-agent"))
            .env("VIA_FAKE_SCENARIO", script_path)
            .env("VIA_FAKE_SYNC_DIR", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    ))
}

fn send_start(child: &mut SupervisedChild, prompt: &str) -> TestResult {
    let stdin = child
        .0
        .stdin
        .as_mut()
        .ok_or("fake stdin unexpectedly closed")?;
    writeln!(
        stdin,
        "{}",
        json!({"type":"start","id":1,"session_id":"fake-session-test","turn":1,"prompt":prompt})
    )?;
    Ok(())
}

fn wait_for_path(path: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for {}", path.display()),
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn wait_for_exit(child: &mut SupervisedChild) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "fake agent failed to exit",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn stdout(child: &mut SupervisedChild) -> TestResult<String> {
    let mut stdout = String::new();
    child
        .0
        .stdout
        .take()
        .ok_or("fake stdout unexpectedly closed")?
        .read_to_string(&mut stdout)?;
    Ok(stdout)
}

fn stdout_bytes(child: &mut SupervisedChild) -> TestResult<Vec<u8>> {
    let mut stdout = Vec::new();
    child
        .0
        .stdout
        .take()
        .ok_or("fake stdout unexpectedly closed")?
        .read_to_end(&mut stdout)?;
    Ok(stdout)
}

fn read_three_lines(child: &mut SupervisedChild) -> TestResult<String> {
    let stdout = child
        .0
        .stdout
        .take()
        .ok_or("fake stdout unexpectedly closed")?;
    let mut reader = BufReader::new(stdout);
    let mut transcript = String::new();
    for _ in 0..3 {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err("fake ended before terminal frame".into());
        }
        transcript.push_str(&line);
    }
    Ok(transcript)
}

#[test]
fn scripted_reply_waits_for_named_release_and_emits_exact_lines() -> TestResult {
    let dir = tempfile::tempdir()?;
    let script = json!({
        "expected_request":{"type":"start","prompt":"hello","turn":1},
        "steps":[
            {"action":"gate","name":"before_accept"},
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"text","vendor_turn_id":"fake-turn-1","text":"reply"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"reply","stop_reason":"end_turn"}}
        ]
    });
    let mut child = fake(&script, dir.path())?;
    send_start(&mut child, "hello")?;
    wait_for_path(&dir.path().join("before_accept.entered"))?;
    fs::write(dir.path().join("before_accept.release"), b"")?;
    wait_for_path(&dir.path().join("before_accept.released"))?;
    let transcript = read_three_lines(&mut child)?;
    child.0.stdin.take();
    assert!(wait_for_exit(&mut child)?.success());
    assert_eq!(
        transcript,
        "{\"id\":1,\"type\":\"accepted\",\"vendor_turn_id\":\"fake-turn-1\"}\n{\"text\":\"reply\",\"type\":\"text\",\"vendor_turn_id\":\"fake-turn-1\"}\n{\"final_text\":\"reply\",\"status\":\"completed\",\"stop_reason\":\"end_turn\",\"type\":\"terminal\",\"vendor_turn_id\":\"fake-turn-1\"}\n"
    );
    Ok(())
}

#[test]
fn wrong_prompt_is_rejected_before_any_fixture_output() -> TestResult {
    let dir = tempfile::tempdir()?;
    let script = json!({
        "expected_request":{"type":"start","prompt":"expected"},
        "steps":[{"action":"emit","message":{"type":"terminal"}}]
    });
    let mut child = fake(&script, dir.path())?;
    send_start(&mut child, "wrong")?;
    assert!(!wait_for_exit(&mut child)?.success());
    assert!(stdout(&mut child)?.is_empty());
    Ok(())
}

#[test]
fn partial_line_then_crash_does_not_add_newline() -> TestResult {
    let dir = tempfile::tempdir()?;
    let script = json!({
        "expected_request":{"type":"start"},
        "steps":[
            {"action":"emit_raw","text":"{\"type\":\"partial\""},
            {"action":"exit","code":17}
        ]
    });
    let mut child = fake(&script, dir.path())?;
    send_start(&mut child, "hello")?;
    assert_eq!(wait_for_exit(&mut child)?.code(), Some(17));
    assert_eq!(stdout(&mut child)?, "{\"type\":\"partial\"");
    Ok(())
}

#[test]
fn malformed_typed_start_is_rejected_even_if_subset_matches() -> TestResult {
    let dir = tempfile::tempdir()?;
    let script = json!({
        "expected_request":{"type":"start","prompt":"hello"},
        "steps":[{"action":"emit","message":{"type":"accepted","id":1}}]
    });
    let mut child = fake(&script, dir.path())?;
    writeln!(
        child
            .0
            .stdin
            .as_mut()
            .ok_or("fake stdin unexpectedly closed")?,
        "{}",
        json!({"type":"start","id":1,"session_id":"fake-session-test","turn":1,"prompt":"hello","unexpected":true})
    )?;
    assert!(!wait_for_exit(&mut child)?.success());
    assert!(stdout(&mut child)?.is_empty());
    Ok(())
}

#[test]
fn interrupt_must_match_derived_turn_and_request_id() -> TestResult {
    let dir = tempfile::tempdir()?;
    let script = json!({
        "expected_request":{"type":"start","prompt":"hello"},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"expect_request","expected":{"type":"interrupt","id":2}},
            {"action":"emit","message":{"type":"interrupt_ack","id":2,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"interrupted","final_text":"","stop_reason":"cancelled"}}
        ]
    });
    let mut child = fake(&script, dir.path())?;
    send_start(&mut child, "hello")?;
    writeln!(
        child
            .0
            .stdin
            .as_mut()
            .ok_or("fake stdin unexpectedly closed")?,
        "{}",
        json!({"type":"interrupt","id":2,"vendor_turn_id":"fake-turn-1"})
    )?;
    let transcript = read_three_lines(&mut child)?;
    child.0.stdin.take();
    assert!(wait_for_exit(&mut child)?.success());
    assert!(transcript.contains("\"type\":\"interrupt_ack\""));
    Ok(())
}

#[test]
fn invalid_utf8_is_emitted_as_exact_bytes() -> TestResult {
    let dir = tempfile::tempdir()?;
    let script = json!({
        "expected_request":{"type":"start"},
        "steps":[{"action":"emit_bytes","bytes":[255,10,0,128]}]
    });
    let mut child = fake(&script, dir.path())?;
    send_start(&mut child, "hello")?;
    assert!(wait_for_exit(&mut child)?.success());
    assert_eq!(stdout_bytes(&mut child)?, [255, 10, 0, 128]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn ignore_term_waits_for_supervisor_kill() -> TestResult {
    let dir = tempfile::tempdir()?;
    let script = json!({
        "expected_request":{"type":"start"},
        "steps":[{"action":"ignore_term"}]
    });
    let mut child = fake(&script, dir.path())?;
    send_start(&mut child, "hello")?;
    wait_for_path(&dir.path().join("ignore_term.entered"))?;
    assert!(
        Command::new("kill")
            .arg("-TERM")
            .arg(child.0.id().to_string())
            .status()?
            .success()
    );
    assert!(child.0.try_wait()?.is_none(), "TERM ended the fake");
    child.0.kill()?;
    assert!(!wait_for_exit(&mut child)?.success());
    Ok(())
}

#[test]
fn normal_reply_fixture_rejects_pipelined_second_start() -> TestResult {
    let dir = tempfile::tempdir()?;
    let script = json!({
        "expected_request":{"type":"start","prompt":"hello"},
        "steps":[
            {"action":"gate","name":"before_accept"},
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"text","vendor_turn_id":"fake-turn-1","text":"reply"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"reply","stop_reason":"end_turn"}}
        ]
    });
    let mut child = fake(&script, dir.path())?;
    send_start(&mut child, "hello")?;
    wait_for_path(&dir.path().join("before_accept.entered"))?;
    send_start(&mut child, "hello")?;
    fs::write(dir.path().join("before_accept.release"), b"")?;
    read_three_lines(&mut child)?;
    child.0.stdin.take();
    assert!(!wait_for_exit(&mut child)?.success());
    Ok(())
}

#[test]
fn normal_reply_fixture_rejects_partial_second_frame() -> TestResult {
    let dir = tempfile::tempdir()?;
    let script = json!({
        "expected_request":{"type":"start"},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"text","vendor_turn_id":"fake-turn-1","text":"reply"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"reply","stop_reason":"end_turn"}}
        ]
    });
    let mut child = fake(&script, dir.path())?;
    send_start(&mut child, "hello")?;
    child
        .0
        .stdin
        .as_mut()
        .ok_or("fake stdin unexpectedly closed")?
        .write_all(b"{\"type\":")?;
    read_three_lines(&mut child)?;
    child.0.stdin.take();
    assert!(!wait_for_exit(&mut child)?.success());
    Ok(())
}

#[test]
fn normal_reply_fixture_fails_when_input_never_closes() -> TestResult {
    let dir = tempfile::tempdir()?;
    let script = json!({
        "expected_request":{"type":"start"},
        "steps":[
            {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
            {"action":"emit","message":{"type":"text","vendor_turn_id":"fake-turn-1","text":"reply"}},
            {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"reply","stop_reason":"end_turn"}}
        ]
    });
    let mut child = fake(&script, dir.path())?;
    send_start(&mut child, "hello")?;
    read_three_lines(&mut child)?;
    assert_eq!(wait_for_exit(&mut child)?.code(), Some(2));
    Ok(())
}

#[test]
fn a_multi_script_fixture_runs_the_script_its_start_request_selects() -> TestResult {
    let root = tempfile::tempdir()?;
    let script = |prompt: &str| {
        json!({"expected_request":{"type":"start","prompt":prompt},
            "steps":[{"action":"emit","message":{"type":"text","text":format!("{prompt} ran")}}]})
    };
    let fixture = json!({"scripts":[script("first"), script("second")]});
    for prompt in ["second", "first"] {
        let mut child = fake(&fixture, root.path())?;
        send_start(&mut child, prompt)?;
        drop(child.0.stdin.take());
        let mut stdout = String::new();
        child
            .0
            .stdout
            .take()
            .ok_or("fake stdout unexpectedly closed")?
            .read_to_string(&mut stdout)?;
        assert!(wait_for_exit(&mut child)?.success());
        let line: serde_json::Value = serde_json::from_str(stdout.trim())?;
        assert_eq!(line["text"], format!("{prompt} ran"));
    }
    let mut child = fake(&fixture, root.path())?;
    send_start(&mut child, "unscripted")?;
    drop(child.0.stdin.take());
    assert!(!wait_for_exit(&mut child)?.success());
    Ok(())
}
