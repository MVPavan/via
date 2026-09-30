//! Checks the evidence collector independently of unfinished S1 runtime behavior.

#[path = "support/evidenced.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod evidenced;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
mod support;

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::Value;

use support::evidence::Evidence;

#[test]
fn evidence_collector_backs_up_live_wal_and_verifies_manifest() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let fixture = sandbox.path().join("fixture.json");
    fs::write(&fixture, b"{}")?;
    let live_store = sandbox.path().join("store.sqlite3");
    let connection = Connection::open(&live_store)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.execute_batch(
        "CREATE TABLE evidence(value TEXT); INSERT INTO evidence VALUES ('committed');",
    )?;
    let folders = sandbox.path().join("evidence");
    fs::create_dir_all(folders.join("s_01/1"))?;
    fs::write(folders.join("s_01/1/stderr.log"), b"stderr bytes\n")?;

    let evidence = Evidence::new(
        "collector_self_test",
        std::path::Path::new(env!("CARGO_BIN_EXE_via")),
        &fixture,
    )?;
    evidence.write("envelopes.ndjson", b"{}\n")?;
    evidence.write("events.ndjson", b"{}\n")?;
    evidence.write("daemon.trace", b"trace\n")?;
    evidence.write(
        "cleanup.json",
        b"{\"anchors\":{\"status\":\"self_test_no_process\"}}",
    )?;
    evidence.copy_evidence(&folders)?;
    evidence.backup_store(&live_store)?;
    let artifact = evidence.finish("pass", "collector self-test")?;

    let backup = Connection::open(artifact.join("store.sqlite3"))?;
    let value: String = backup.query_row("SELECT value FROM evidence", [], |row| row.get(0))?;
    assert_eq!(value, "committed");
    assert!(artifact.join("sha256.manifest").is_file());
    // Finding 8: the summary names the `via-cli` features this binary has.
    let summary: Value = serde_json::from_slice(&fs::read(artifact.join("summary.json"))?)?;
    let features: &[&str] = if cfg!(feature = "test-failpoints") {
        &["test-failpoints"]
    } else {
        &[]
    };
    assert_eq!(summary["features"], serde_json::json!(features));
    let stderr = artifact.join("evidence/s_01/1/stderr.log");
    let original = fs::read(&stderr)?;
    fs::write(&stderr, b"tampered")?;
    assert!(
        !Command::new("sha256sum")
            .args(["--check", "sha256.manifest"])
            .current_dir(&artifact)
            .output()?
            .status
            .success()
    );
    fs::write(&stderr, original)?;
    assert!(
        Command::new("sha256sum")
            .args(["--check", "sha256.manifest"])
            .current_dir(&artifact)
            .output()?
            .status
            .success()
    );
    Ok(())
}

#[test]
fn failure_and_timeout_remain_distinct_evidence_outcomes() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let fixture = sandbox.path().join("fixture.json");
    fs::write(&fixture, b"{}")?;
    let live_store = sandbox.path().join("store.sqlite3");
    let connection = Connection::open(&live_store)?;
    connection.execute_batch("CREATE TABLE evidence(value TEXT)")?;
    let folders = sandbox.path().join("evidence");
    fs::create_dir_all(folders.join("s_01/1"))?;
    fs::write(folders.join("s_01/1/stderr.log"), b"stderr bytes\n")?;

    for (name, outcome) in [
        ("collector_failure_test", "fail"),
        ("collector_timeout_test", "timeout"),
    ] {
        let evidence = Evidence::new(
            name,
            std::path::Path::new(env!("CARGO_BIN_EXE_via")),
            &fixture,
        )?;
        evidence.write("envelopes.ndjson", b"{}\n")?;
        evidence.write("events.ndjson", b"{}\n")?;
        evidence.write("daemon.trace", b"trace\n")?;
        evidence.write(
            "cleanup.json",
            b"{\"anchors\":{\"status\":\"self_test_no_process\"}}",
        )?;
        evidence.copy_evidence(&folders)?;
        evidence.backup_store(&live_store)?;
        let artifact = evidence.finish(outcome, "simulated collector outcome")?;
        let summary: Value = serde_json::from_slice(&fs::read(artifact.join("summary.json"))?)?;
        assert_eq!(summary["outcome"], outcome);
    }
    Ok(())
}

/// `via-jm4.7.6`: evidence can hold vendor stderr and Store backups, so the
/// artifact directory and its `evidence/` are private (0700) regardless of
/// umask.
#[test]
fn evidence_and_evidence_folder_directories_are_private() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = tempfile::tempdir()?;
    let fixture = sandbox.path().join("fixture.json");
    fs::write(&fixture, b"{}")?;
    let evidence = Evidence::new(
        "collector_private_dirs",
        std::path::Path::new(env!("CARGO_BIN_EXE_via")),
        &fixture,
    )?;
    for dir in [evidence.dir.clone(), evidence.dir.join("evidence")] {
        let mode = fs::metadata(&dir)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{} has mode {mode:o}", dir.display());
    }
    Ok(())
}

/// S1-contract r1 finding 2: missing required evidence records
/// `infrastructure_failure` in the summary and the report, whatever
/// outcome the caller passed.
#[test]
fn missing_required_evidence_is_an_infrastructure_failure() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let fixture = sandbox.path().join("fixture.json");
    fs::write(&fixture, b"{}")?;
    let evidence = Evidence::new(
        "collector_missing_evidence",
        std::path::Path::new(env!("CARGO_BIN_EXE_via")),
        &fixture,
    )?;
    let artifact = evidence.dir.clone();
    evidence.write("daemon.trace", b"trace\n")?;
    assert!(
        evidence
            .finish("pass", "passing body, missing evidence")
            .is_err()
    );
    let summary: Value = serde_json::from_slice(&fs::read(artifact.join("summary.json"))?)?;
    assert_eq!(summary["outcome"], "infrastructure_failure");
    assert_eq!(summary["evidence_complete"], false);
    let report = fs::read_to_string(artifact.join("REPORT.md"))?;
    assert!(report.contains("`infrastructure_failure`"), "{report}");
    Ok(())
}

/// A live process of the sandbox, identified by its environment, with no
/// socket and no lock: the shape of a daemon after its locks' release
/// (S1-contract r2 finding 1).
fn sandbox_process(runtime: &Path) -> Result<Child, Box<dyn Error>> {
    Ok(Command::new("sleep")
        .arg("30")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("VIA_RUNTIME_DIR", runtime)
        .spawn()?)
}

#[test]
fn collector_exit_proof_needs_the_process_gone_not_only_the_locks() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let (runtime, state) = (sandbox.path().join("runtime"), sandbox.path().join("state"));
    let mut process = sandbox_process(&runtime)?;
    let pid = process.id();
    let unproven = evidenced::stop_within(&runtime, &state, Duration::from_millis(300), |_| {});
    process.kill()?;
    process.wait()?;
    let error = unproven.expect_err("a live sandbox process was accepted as exited");
    assert!(error.contains(&pid.to_string()), "{error}");
    evidenced::stop_within(&runtime, &state, Duration::from_secs(5), |_| {})?;
    Ok(())
}

#[test]
fn collector_exit_proof_budget_includes_the_stop() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let (runtime, state) = (sandbox.path().join("runtime"), sandbox.path().join("state"));
    let mut process = sandbox_process(&runtime)?;
    let budget = Duration::from_millis(300);
    let mut given = None;
    // The stop proves the exit, but only after the budget elapsed.
    let late = evidenced::stop_within(&runtime, &state, budget, |left| {
        given = Some(left);
        let _ = process.kill();
        let _ = process.wait();
        std::thread::sleep(left + Duration::from_millis(100));
    });
    let _ = process.kill();
    let _ = process.wait();
    assert!(given.is_some_and(|left| left <= budget), "{given:?}");
    let error = late.expect_err("a proof completed after its budget was accepted");
    assert!(error.contains("budget"), "{error}");
    Ok(())
}

/// A scenario whose folders are required still has every launched turn's
/// folder checked: one of two anchored turns without its folder is an
/// infrastructure failure (S1-contract r2 finding 3).
#[test]
fn collector_launched_turn_without_its_folder_is_an_infrastructure_failure()
-> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let root = sandbox.path().join("root");
    let state = root.join("state");
    fs::create_dir_all(state.join("evidence/s_a/1"))?;
    fs::write(state.join("evidence/s_a/1/stderr.log"), b"stderr\n")?;
    let fixture = root.join("fixture.json");
    fs::write(&fixture, b"{}")?;
    Connection::open(state.join("store.sqlite3"))?.execute_batch(
        "CREATE TABLE turns(session_id TEXT, number INTEGER, envelope TEXT);
         CREATE TABLE events(session_id TEXT, seq INTEGER, event TEXT);
         CREATE TABLE anchors(owner_session TEXT, owner_turn INTEGER);
         INSERT INTO anchors VALUES ('s_a', 1), ('s_a', 2);",
    )?;
    let mut artifact = None;
    let result = evidenced::evidenced(|| {
        let evidence = evidenced::open(Path::new(env!("CARGO_BIN_EXE_via")), &fixture)?;
        artifact = Some(evidence.dir.clone());
        let expected = evidenced::Expected {
            store: true,
            folders: true,
        };
        evidenced::park(evidence, root.clone(), &state, expected, Ok(()));
        Ok(())
    });
    let error = result
        .expect_err("a missing launched turn folder passed")
        .to_string();
    assert!(error.contains("s_a/2 has no evidence folder"), "{error}");
    let artifact = artifact.ok_or("no artifact")?;
    let summary: Value = serde_json::from_slice(&fs::read(artifact.join("summary.json"))?)?;
    assert_eq!(summary["outcome"], "infrastructure_failure", "{summary}");
    Ok(())
}
