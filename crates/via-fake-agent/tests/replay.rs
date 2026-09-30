//! Replay mode (adapters design §8 item 3): the fake started as `<dir>/<name>`
//! beside `<dir>/<name>.replay.json` replays that recorded vendor fixture.

use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};

use serde_json::{Value, json};

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

fn spawn(binary: &Path, args: &[&str]) -> TestResult<SupervisedChild> {
    Ok(SupervisedChild(
        Command::new(binary)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    ))
}

fn read_line(stdout: &mut BufReader<ChildStdout>) -> TestResult<String> {
    let mut line = String::new();
    stdout.read_line(&mut line)?;
    Ok(line)
}

/// Closes stdin and collects the remaining output once the fake exits.
fn finish(mut child: SupervisedChild) -> TestResult<Output> {
    drop(child.0.stdin.take());
    let status = child.0.wait()?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut out) = child.0.stdout.take() {
        out.read_to_end(&mut stdout)?;
    }
    if let Some(mut err) = child.0.stderr.take() {
        err.read_to_end(&mut stderr)?;
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn stdin(child: &mut SupervisedChild) -> TestResult<&mut std::process::ChildStdin> {
    Ok(child
        .0
        .stdin
        .as_mut()
        .ok_or("fake stdin unexpectedly closed")?)
}

#[test]
fn replay_expected_line_mismatch_fails_naming_the_step() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &json!({
            "argv": ["-p"],
            "deadline_ms": 10_000,
            "steps": [
                {"emit": {"line": "{\"type\":\"system\"}"}},
                {"expect": {"line": {"type": "user", "message": {"content": "hello"}}}}
            ]
        }),
    )?;
    let mut child = spawn(&binary, &["-p"])?;
    writeln!(
        stdin(&mut child)?,
        "{}",
        json!({"type": "user", "message": {"content": "goodbye"}})
    )?;
    let output = finish(child)?;
    assert!(!output.status.success(), "a mismatch must fail the run");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("step 2"), "stderr names the step: {stderr}");
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
                {"expect": {
                    "line": {"method": "initialize"},
                    "capture": {"init": "/id"}
                }},
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
    let mut child = spawn(&binary, &["app-server", "--listen", "stdio://"])?;
    let mut stdout = BufReader::new(child.0.stdout.take().ok_or("no stdout")?);
    writeln!(
        stdin(&mut child)?,
        "{}",
        json!({"id": 0, "method": "initialize", "params": {}})
    )?;
    assert_eq!(read_line(&mut stdout)?, "{\"id\":0,\"result\":{}}\n");
    writeln!(
        stdin(&mut child)?,
        "{}",
        json!({"id": "req-7", "method": "thread/start", "params": {"cwd": "/work", "x": 1}})
    )?;
    assert_eq!(
        read_line(&mut stdout)?,
        "{\"id\":\"req-7\",\"result\":{\"thread\":\"t-1\"}}\n"
    );
    let output = finish(child)?;
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn replay_checks_argv_exactly_and_answers_version() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &json!({"argv": ["-p", "--verbose"], "version": "9.9.9 (Vendor)", "deadline_ms": 10_000, "steps": []}),
    )?;
    let version = finish(spawn(&binary, &["--version"])?)?;
    assert!(version.status.success());
    assert_eq!(String::from_utf8(version.stdout)?, "9.9.9 (Vendor)\n");

    let wrong = finish(spawn(&binary, &["-p"])?)?;
    assert!(!wrong.status.success(), "argv must match exactly");
    assert!(String::from_utf8(wrong.stderr)?.contains("argv"));

    let right = finish(spawn(&binary, &["-p", "--verbose"])?)?;
    assert!(right.status.success());
    Ok(())
}

#[test]
fn replay_signal_gated_step_waits_for_the_signal() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &json!({
            "argv": [],
            "deadline_ms": 10_000,
            "steps": [
                {"emit": {"line": "ready"}},
                {"await_signal": {"signal": "SIGINT"}},
                {"emit": {"line": "interrupted"}}
            ]
        }),
    )?;
    let mut child = spawn(&binary, &[])?;
    let mut stdout = BufReader::new(child.0.stdout.take().ok_or("no stdout")?);
    assert_eq!(read_line(&mut stdout)?, "ready\n");
    let kill = Command::new("kill")
        .args(["-INT", &child.0.id().to_string()])
        .status()?;
    assert!(kill.success());
    assert_eq!(read_line(&mut stdout)?, "interrupted\n");
    let output = finish(child)?;
    assert!(output.status.success());
    Ok(())
}

#[test]
fn replay_whole_run_deadline_fails_naming_the_step() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &json!({
            "argv": [],
            "deadline_ms": 100,
            "steps": [{"expect": {"line": {"type": "user"}}}]
        }),
    )?;
    let mut child = spawn(&binary, &[])?;
    // Stdin stays open (`wait` would close it), so only the deadline can end the run.
    let _stdin = child.0.stdin.take();
    let status = child.0.wait()?;
    assert!(!status.success());
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .ok_or("no stderr")?
        .read_to_string(&mut stderr)?;
    assert!(
        stderr.contains("deadline") && stderr.contains("step 1"),
        "{stderr}"
    );
    Ok(())
}

#[test]
fn replay_rejects_input_lines_over_one_mebibyte() -> TestResult {
    let root = tempfile::tempdir()?;
    let binary = install(
        root.path(),
        &json!({
            "argv": [],
            "deadline_ms": 10_000,
            "steps": [{"expect": {"line": {"type": "user"}}}]
        }),
    )?;
    let mut child = spawn(&binary, &[])?;
    let line = format!(
        "{{\"type\":\"user\",\"pad\":\"{}\"}}\n",
        "a".repeat(1024 * 1024)
    );
    // The fake may exit before reading all of it; a broken pipe is expected.
    let _ = stdin(&mut child)?.write_all(line.as_bytes());
    let output = finish(child)?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("step 1"));
    Ok(())
}
