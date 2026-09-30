//! Checks the evidence collector independently of unfinished S1 runtime behavior.

#[path = "support/evidenced.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod evidenced;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
#[expect(dead_code, reason = "shared support; this file uses its typed errors")]
mod scenario;
mod support;

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::Value;

use scenario::ScenarioError;
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

/// Runtime §11.2 (S1 critic r2 finding 3): missing required evidence keeps
/// the scenario's own outcome, a passing body's `pass` here, and is recorded
/// beside it in the summary and the report; finalization still fails.
#[test]
fn missing_required_evidence_keeps_the_outcome_and_fails() -> Result<(), Box<dyn Error>> {
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
    assert_eq!(summary["outcome"], "pass", "{summary}");
    assert_eq!(summary["evidence_complete"], false, "{summary}");
    assert!(
        summary["evidence_failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("store.sqlite3")),
        "{summary}"
    );
    let report = fs::read_to_string(artifact.join("REPORT.md"))?;
    assert!(report.contains("Outcome: `pass`"), "{report}");
    assert!(report.contains("Evidence complete: no"), "{report}");
    assert!(report.contains("store.sqlite3"), "{report}");
    Ok(())
}

/// Runtime §11.2 (S1 critic r2 finding 3): a body's typed timeout that
/// ends before its Store exists stays `timeout` through `evidenced`; the
/// missing Store evidence and the unproven cleanup are recorded beside it,
/// and the test still fails.
#[test]
fn a_timeout_with_missing_store_evidence_stays_a_timeout() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let root = sandbox.path().join("root");
    let state = root.join("state");
    fs::create_dir_all(&state)?;
    let fixture = root.join("fixture.json");
    fs::write(&fixture, b"{}")?;
    let mut artifact = None;
    let result = evidenced::evidenced(|| -> Result<(), Box<dyn Error>> {
        let evidence = evidenced::open(Path::new(env!("CARGO_BIN_EXE_via")), &fixture)?;
        artifact = Some(evidence.dir.clone());
        let expected = evidenced::Expected {
            store: true,
            folders: true,
        };
        evidenced::park(evidence, root.clone(), &state, expected, proved());
        Err(ScenarioError::Timeout("the turn never finished".to_owned()).into())
    });
    let error = result.expect_err("a timed-out scenario passed").to_string();
    assert!(error.contains("the turn never finished"), "{error}");
    let artifact = artifact.ok_or("no artifact")?;
    let summary: Value = serde_json::from_slice(&fs::read(artifact.join("summary.json"))?)?;
    assert_eq!(summary["outcome"], "timeout", "{summary}");
    assert_eq!(summary["evidence_complete"], false, "{summary}");
    assert!(
        summary["missing_evidence"]
            .as_array()
            .is_some_and(|missing| missing.contains(&Value::from("store.sqlite3"))),
        "{summary}"
    );
    assert!(
        summary["evidence_failure"].is_string() && summary["cleanup_failure"].is_string(),
        "{summary}"
    );
    Ok(())
}

/// A teardown whose daemons were proved gone, with its whole budget left.
fn proved() -> evidenced::Exited {
    evidenced::Exited {
        proof: Ok(()),
        deadline: std::time::Instant::now() + outer_cleanup::TEARDOWN,
    }
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
    let unproven =
        evidenced::stop_within(&runtime, &state, Duration::from_millis(300), |_| {}).proof;
    process.kill()?;
    process.wait()?;
    let error = unproven.expect_err("a live sandbox process was accepted as exited");
    assert!(error.contains(&pid.to_string()), "{error}");
    evidenced::stop_within(&runtime, &state, Duration::from_secs(5), |_| {}).proof?;
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
    })
    .proof;
    let _ = process.kill();
    let _ = process.wait();
    assert!(given.is_some_and(|left| left <= budget), "{given:?}");
    let error = late.expect_err("a proof completed after its budget was accepted");
    assert!(error.contains("budget"), "{error}");
    Ok(())
}

/// A scenario whose folders are required still has every launched turn's
/// folder checked: one of two anchored turns without its folder fails the
/// test (S1-contract r2 finding 3), and the passing body's outcome stays
/// `pass` beside the recorded failure (S1 critic r2 finding 3).
#[test]
fn collector_launched_turn_without_its_folder_fails_the_evidence() -> Result<(), Box<dyn Error>> {
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
        evidenced::park(evidence, root.clone(), &state, expected, proved());
        Ok(())
    });
    let error = result
        .expect_err("a missing launched turn folder passed")
        .to_string();
    assert!(error.contains("s_a/2 has no evidence folder"), "{error}");
    let artifact = artifact.ok_or("no artifact")?;
    let summary: Value = serde_json::from_slice(&fs::read(artifact.join("summary.json"))?)?;
    assert_eq!(summary["outcome"], "pass", "{summary}");
    assert_eq!(summary["evidence_complete"], false, "{summary}");
    assert!(
        summary["cleanup_failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("s_a/2 has no evidence folder")),
        "{summary}"
    );
    Ok(())
}

/// A process whose environment cannot be read is absent only when it
/// vanished; otherwise it is unrelated only when its command line shows
/// another program than `via` (S1-contract r3 finding 1).
#[test]
fn collector_exit_proof_is_indeterminate_for_an_unreadable_via_process()
-> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let (runtime, state) = (sandbox.path().join("runtime"), sandbox.path().join("state"));
    let mut process = sandbox_process(&runtime)?;
    let proc_dir = Path::new("/proc").join(process.id().to_string());
    let denied = || std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    let via_cmdline = [env!("CARGO_BIN_EXE_via").as_bytes(), b"\0daemon\0"].concat();
    let unreadable_via = evidenced::scan_processes(&runtime, &state, |path| {
        if path == proc_dir.join("environ") {
            Err(denied())
        } else if path == proc_dir.join("cmdline") {
            Ok(via_cmdline.clone())
        } else {
            fs::read(path)
        }
    });
    let unreadable_cmdline = evidenced::scan_processes(&runtime, &state, |path| {
        if path.starts_with(&proc_dir) {
            Err(denied())
        } else {
            fs::read(path)
        }
    });
    let unreadable_other = evidenced::scan_processes(&runtime, &state, |path| {
        if path == proc_dir.join("environ") {
            Err(denied())
        } else {
            fs::read(path)
        }
    });
    let vanished = evidenced::scan_processes(&runtime, &state, |path| {
        if path.starts_with(&proc_dir) {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        } else {
            fs::read(path)
        }
    });
    let readable = evidenced::scan_processes(&runtime, &state, |path| fs::read(path));
    process.kill()?;
    process.wait()?;
    let error = unreadable_via.expect_err("an unreadable via process was accepted as absent");
    assert!(error.contains("indeterminate"), "{error}");
    unreadable_cmdline.expect_err("an unreadable command line was accepted as unrelated");
    assert_eq!(
        unreadable_other?,
        Vec::<u32>::new(),
        "a `sleep` is not a daemon"
    );
    assert_eq!(vanished?, Vec::<u32>::new());
    assert_eq!(readable?, vec![process.id()]);
    Ok(())
}

/// Runtime §11.2, S1 critic r2 finding 4: a sandbox teardown has one
/// deadline. The exit proof's stop takes most of it; the anchor cleanup
/// that follows gets only what is left, so a group that is gone only after
/// the deadline is incomplete cleanup, not a proved absence.
#[test]
fn collector_anchor_cleanup_gets_only_the_teardown_time_left() -> Result<(), Box<dyn Error>> {
    use std::os::unix::process::CommandExt as _;

    let sandbox = tempfile::tempdir()?;
    let root = sandbox.path().join("root");
    let (runtime, state) = (root.join("runtime"), root.join("state"));
    fs::create_dir_all(&state)?;
    let fixture = root.join("fixture.json");
    fs::write(&fixture, b"{}")?;
    // The anchor's group: gone by itself 2 s from now, after the deadline.
    let mut group = Command::new("sleep").arg("2").process_group(0).spawn()?;
    let pid = group.id();
    let reaper = std::thread::spawn(move || group.wait());
    anchor_store(&state, &root, pid)?;
    let mut daemon = sandbox_process(&runtime)?;
    let mut artifact = None;
    let result = evidenced::evidenced(|| {
        let evidence = evidenced::open(Path::new(env!("CARGO_BIN_EXE_via")), &fixture)?;
        artifact = Some(evidence.dir.clone());
        // The stop phase ends the process but takes all but 300 ms of the
        // budget; the exit proof completes within it.
        let exited = evidenced::stop_within(&runtime, &state, Duration::from_secs(1), |left| {
            let _ = daemon.kill();
            let _ = daemon.wait();
            std::thread::sleep(left.saturating_sub(Duration::from_millis(300)));
        });
        let expected = evidenced::Expected {
            store: true,
            folders: true,
        };
        evidenced::park(evidence, root.clone(), &state, expected, exited);
        Ok(())
    });
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = reaper.join();
    let error = result
        .expect_err("absence after the teardown deadline passed")
        .to_string();
    assert!(error.contains("outer cleanup is unverified"), "{error}");
    let artifact = artifact.ok_or("no artifact")?;
    let summary: Value = serde_json::from_slice(&fs::read(artifact.join("summary.json"))?)?;
    assert_eq!(summary["outcome"], "pass", "{summary}");
    assert_eq!(summary["evidence_complete"], false, "{summary}");
    let cleanup: Value = serde_json::from_slice(&fs::read(artifact.join("cleanup.json"))?)?;
    let record = &cleanup["anchors"]["records"][0];
    assert_eq!(cleanup["anchors"]["absence_proven"], false, "{cleanup}");
    assert!(
        record["absence_probe"] == "present" || record["absence_probe"] == "esrch_after_deadline",
        "{cleanup}"
    );
    Ok(())
}

/// A State whose Store commits one anchor of turn `s_a/1` for the process
/// group led by `pid`, with this boot, PID namespace and user, and no
/// control socket; the turn's evidence folder exists.
fn anchor_store(state: &Path, root: &Path, pid: u32) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(state.join("evidence/s_a/1"))?;
    fs::write(state.join("evidence/s_a/1/stderr.log"), b"stderr\n")?;
    let (_, start_ticks) = outer_cleanup::process_stat(pid).ok_or("no stat for the group")?;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let namespace = fs::read_link("/proc/self/ns/pid")?;
    let store = Connection::open(state.join("store.sqlite3"))?;
    store.execute_batch(
        "CREATE TABLE turns(session_id TEXT, number INTEGER, envelope TEXT);
         CREATE TABLE events(session_id TEXT, seq INTEGER, event TEXT);
         CREATE TABLE anchors(anchor_id TEXT, generation TEXT, marker TEXT, socket_path TEXT,
             phase TEXT, pid INTEGER, pgid INTEGER, uid INTEGER, boot_id TEXT,
             pid_namespace TEXT, start_ticks INTEGER, absence_time TEXT,
             owner_session TEXT, owner_turn INTEGER);",
    )?;
    store.execute(
        "INSERT INTO anchors VALUES ('a_1','g1','marker',?1,'armed',?2,?2,?3,?4,?5,?6,NULL,'s_a',1)",
        rusqlite::params![
            root.join("no-anchor.sock").to_string_lossy(),
            pid,
            rustix::process::getuid().as_raw(),
            boot.trim(),
            namespace.to_string_lossy(),
            i64::try_from(start_ticks)?,
        ],
    )?;
    Ok(())
}

/// Runtime §11.2, S1 critic r2 finding 4: `ESRCH` observed only after the
/// outer deadline proves nothing; the anchor's cleanup is incomplete.
#[test]
fn outer_cleanup_absence_after_the_deadline_is_incomplete() -> Result<(), Box<dyn Error>> {
    use std::os::unix::process::CommandExt as _;

    let sandbox = tempfile::tempdir()?;
    let (root, state) = (sandbox.path().to_owned(), sandbox.path().join("state"));
    let mut group = Command::new("sleep").arg("0.2").process_group(0).spawn()?;
    anchor_store(&state, &root, group.id())?;
    group.wait()?;
    let rows = outer_cleanup::snapshot(&state.join("store.sqlite3"))?;
    let expired = outer_cleanup::verify(&rows, std::time::Instant::now());
    assert_eq!(expired["status"], "unverified", "{expired}");
    assert_eq!(
        expired["records"][0]["absence_probe"], "esrch_after_deadline",
        "{expired}"
    );
    let timely = outer_cleanup::verify(&rows, std::time::Instant::now() + outer_cleanup::TEARDOWN);
    assert_eq!(timely["status"], "quiescent", "{timely}");
    Ok(())
}
