//! Checks the evidence collector independently of unfinished S1 runtime behavior.

mod support;

use std::error::Error;
use std::fs;
use std::process::Command;

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
    assert!(artifact.join("summary.json").is_file());
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
