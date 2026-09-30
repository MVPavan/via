//! Checks the evidence collector independently of unfinished S1 runtime behavior.

#[path = "support/evidenced.rs"]
mod evidenced;
#[path = "support/outer_cleanup.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod outer_cleanup;
#[path = "support/process.rs"]
mod process;
#[path = "support/scenario.rs"]
#[expect(dead_code, reason = "shared support; this file uses its typed errors")]
mod scenario;
mod support;

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};

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
    exited(Ok(()), Instant::now() + outer_cleanup::TEARDOWN)
}

/// A teardown with exit proof `proof` and deadline `deadline`, and no guard.
fn exited(proof: Result<(), String>, deadline: Instant) -> evidenced::Exited {
    evidenced::Exited {
        proof,
        deadline,
        teardown: json!({"complete":false,"generations":[],"failures":[]}),
        failures: Vec::new(),
    }
}

/// Scheduling tolerance for a supervised operation's return after its
/// deadline: the 5-20 ms polls plus thread wake-up latency on a loaded
/// parallel test run (the gate runs every test binary at once). A return
/// later than its deadline plus this is an unbounded operation, the
/// regression these tests catch (S1-evidence2 fix round 2, finding 15).
const TOLERANCE: Duration = Duration::from_millis(500);

/// Asserts that an operation begun at `started` with deadline `deadline`
/// returned by the deadline plus [`TOLERANCE`].
fn returned_in_time(what: &str, deadline: Instant) {
    let late = Instant::now().saturating_duration_since(deadline);
    assert!(
        late <= TOLERANCE,
        "{what} returned {late:?} after its deadline (tolerance {TOLERANCE:?})"
    );
}

/// Kills and reaps a fixture child within 5 s, never a blocking wait
/// (S1-evidence2 fix round 2, finding 10).
fn reap(child: &mut Child) -> Result<(), Box<dyn Error>> {
    if outer_cleanup::kill_and_reap(child, Instant::now() + Duration::from_secs(5)) {
        Ok(())
    } else {
        Err(format!("fixture child {} was not reaped in 5 s", child.id()).into())
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
    let unproven = evidenced::stop_daemons(
        &runtime,
        &state,
        &outer_cleanup::Teardown::with_budget(Duration::from_millis(300)),
        |_| (json!({}), None),
    )
    .proof;
    reap(&mut process)?;
    let error = unproven.expect_err("a live sandbox process was accepted as exited");
    assert!(error.contains(&pid.to_string()), "{error}");
    evidenced::stop_daemons(
        &runtime,
        &state,
        &outer_cleanup::Teardown::with_budget(Duration::from_secs(5)),
        |_| (json!({}), None),
    )
    .proof?;
    Ok(())
}

#[test]
fn collector_exit_proof_budget_includes_the_stop() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let (runtime, state) = (sandbox.path().join("runtime"), sandbox.path().join("state"));
    let mut process = sandbox_process(&runtime)?;
    let budget = Duration::from_millis(300);
    let mut given = None;
    let teardown = outer_cleanup::Teardown::with_budget(budget);
    let deadline = teardown.begin();
    // The stop proves the exit, but only after the budget elapsed.
    let late = evidenced::stop_daemons(&runtime, &state, &teardown, |by| {
        given = Some(by);
        let _ = reap(&mut process);
        std::thread::sleep(outer_cleanup::left(by) + Duration::from_millis(100));
        (json!({}), None)
    })
    .proof;
    reap(&mut process)?;
    assert!(given.is_some_and(|by| by <= deadline), "{given:?}");
    let error = late.expect_err("a proof completed after its budget was accepted");
    assert!(error.contains("deadline"), "{error}");
    Ok(())
}

/// Runtime §11.2, S1-evidence2 fix round 2 finding 9: the sandbox's
/// ordinary stop gets at most 2 s of the teardown, never all of it, and
/// its record and cleanup failure reach the teardown.
#[test]
fn collector_ordinary_stop_is_capped_at_two_seconds() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let (runtime, state) = (sandbox.path().join("runtime"), sandbox.path().join("state"));
    let mut process = sandbox_process(&runtime)?;
    let teardown = outer_cleanup::Teardown::new();
    let begun = Instant::now();
    let mut given = None;
    let exited = evidenced::stop_daemons(&runtime, &state, &teardown, |by| {
        given = Some(by);
        let _ = reap(&mut process);
        (
            json!({"status":"stalled"}),
            Some("stop child 1 was not reaped".to_owned()),
        )
    });
    reap(&mut process)?;
    let given = given.ok_or("the stop never ran")?;
    assert!(
        given <= begun + outer_cleanup::ORDINARY_STOP + TOLERANCE,
        "the ordinary stop got {:?} of the teardown",
        given.saturating_duration_since(begun)
    );
    exited.proof?;
    assert!(
        exited
            .failures
            .iter()
            .any(|failure| failure.contains("not reaped")),
        "{:?}",
        exited.failures
    );
    assert_eq!(
        exited.teardown["generations"][0]["stop"]["status"], "stalled",
        "{}",
        exited.teardown
    );
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
         CREATE TABLE anchors(anchor_id TEXT, generation TEXT, marker TEXT, socket_path TEXT,
             phase TEXT, pid INTEGER, pgid INTEGER, uid INTEGER, boot_id TEXT,
             pid_namespace TEXT, start_ticks INTEGER, absence_time TEXT,
             owner_session TEXT, owner_turn INTEGER);
         INSERT INTO anchors VALUES
             ('a_1', 'g', 'm', 'none.sock', 'created', NULL, NULL, 0, 'b', 'n', NULL, NULL,
              's_a', 1),
             ('a_2', 'g', 'm', 'none.sock', 'created', NULL, NULL, 0, 'b', 'n', NULL, NULL,
              's_a', 2);",
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
    // Both failures accumulate (S1-evidence2 fix round 2, finding 6): the
    // collection error no longer erases the unverified anchor cleanup of
    // these identity-less rows.
    assert!(
        summary["evidence_failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("s_a/2 has no evidence folder")),
        "{summary}"
    );
    assert!(
        summary["cleanup_failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("outer cleanup is unverified")),
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
    reap(&mut process)?;
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
    // The anchor's group: present past the deadline (the fixture reaps it
    // only at the end).
    let mut group = Command::new("sleep").arg("30").process_group(0).spawn()?;
    let pid = group.id();
    anchor_store(&state, &root, pid)?;
    let mut daemon = sandbox_process(&runtime)?;
    let mut artifact = None;
    let mut teardown_deadline = None;
    let result = evidenced::evidenced(|| {
        let evidence = evidenced::open(Path::new(env!("CARGO_BIN_EXE_via")), &fixture)?;
        artifact = Some(evidence.dir.clone());
        // The stop phase ends the process but takes all but 300 ms of the
        // budget; the exit proof completes within it.
        let teardown = outer_cleanup::Teardown::with_budget(Duration::from_secs(1));
        teardown_deadline = Some(teardown.begin());
        let exited = evidenced::stop_daemons(&runtime, &state, &teardown, |by| {
            let _ = reap(&mut daemon);
            std::thread::sleep(outer_cleanup::left(by).saturating_sub(Duration::from_millis(300)));
            (json!({}), None)
        });
        let expected = evidenced::Expected {
            store: true,
            folders: true,
        };
        evidenced::park(evidence, root.clone(), &state, expected, exited);
        // The supervisor itself returned within the teardown's deadline
        // (S1-evidence2 fix round 2, finding 15).
        returned_in_time("the sandbox teardown", teardown.begin());
        Ok(())
    });
    reap(&mut daemon)?;
    reap(&mut group)?;
    assert!(teardown_deadline.is_some(), "the teardown never began");
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
        "INSERT INTO anchors VALUES ('a_1','g1','marker',?1,'arm_intent',?2,?2,?3,?4,?5,?6,NULL,'s_a',1)",
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
    if !outer_cleanup::reap_by(&mut group, Instant::now() + Duration::from_secs(5)) {
        return Err("the fixture group did not exit in 5 s".into());
    }
    let rows = outer_cleanup::snapshot(
        &state.join("store.sqlite3"),
        Instant::now() + outer_cleanup::TEARDOWN,
    )?;
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

/// Runtime §11.2, S1-evidence2 fix round 1: a scenario's final teardown has
/// one deadline. A guard's phase begins it and uses most of it; the
/// sandbox's exit proof and anchor cleanup that follow get only what is
/// left, so a group gone only after the deadline is incomplete cleanup.
/// A guard's recorded cleanup failure fails the exit proof.
#[test]
fn collector_sandbox_teardown_shares_the_guards_deadline() -> Result<(), Box<dyn Error>> {
    use std::os::unix::process::CommandExt as _;

    let sandbox = tempfile::tempdir()?;
    let root = sandbox.path().join("root");
    let (runtime, state) = (root.join("runtime"), root.join("state"));
    fs::create_dir_all(&state)?;
    let fixture = root.join("fixture.json");
    fs::write(&fixture, b"{}")?;
    // The anchor's group: present past the 1 s deadline, as a group gone
    // only before a fresh budget begun after the guard's phase would be.
    let mut group = Command::new("sleep").arg("30").process_group(0).spawn()?;
    let pid = group.id();
    anchor_store(&state, &root, pid)?;
    let teardown = outer_cleanup::Teardown::with_budget(Duration::from_secs(1));
    // The guard's phase: begins the teardown, uses 700 ms of it, and
    // records its direct child's reap (S1-evidence2 fix round 2, finding 16).
    teardown.begin();
    std::thread::sleep(Duration::from_millis(700));
    teardown.record(
        json!({"generation":"guard-1","direct_child":{"pid":1,"reaped":true}}),
        None,
    );
    let mut artifact = None;
    let result = evidenced::evidenced(|| {
        let evidence = evidenced::open(Path::new(env!("CARGO_BIN_EXE_via")), &fixture)?;
        artifact = Some(evidence.dir.clone());
        let exited = evidenced::stop_daemons(&runtime, &state, &teardown, |_| (json!({}), None));
        let expected = evidenced::Expected {
            store: true,
            folders: true,
        };
        evidenced::park(evidence, root.clone(), &state, expected, exited);
        returned_in_time("the sandbox teardown", teardown.begin());
        Ok(())
    });
    reap(&mut group)?;
    let error = result
        .expect_err("the sandbox teardown restarted the budget")
        .to_string();
    assert!(error.contains("outer cleanup is unverified"), "{error}");
    let artifact = artifact.ok_or("no artifact")?;
    let cleanup: Value = serde_json::from_slice(&fs::read(artifact.join("cleanup.json"))?)?;
    assert_eq!(cleanup["anchors"]["absence_proven"], false, "{cleanup}");
    assert_eq!(
        cleanup["teardown"]["generations"][0]["direct_child"]["reaped"], true,
        "{cleanup}"
    );

    // A guard's recorded cleanup failure reaches the sandbox's cleanup.
    let failed = outer_cleanup::Teardown::new();
    failed.record(
        json!({"generation":"guard-1","direct_child":{"pid":1,"reaped":false}}),
        Some("daemon child 1 was not reaped by the teardown deadline".to_owned()),
    );
    let exited = evidenced::stop_daemons(&runtime, &state, &failed, |_| (json!({}), None));
    assert!(
        exited
            .failures
            .iter()
            .any(|failure| failure.contains("not reaped")),
        "an unreaped guard child was dropped: {:?}",
        exited.failures
    );
    Ok(())
}

/// S1 critic r2 finding 4, S1-evidence2 fix round 1: the anchor connect is
/// bounded by the deadline. A listener whose backlog is full would hold a
/// blocking connect indefinitely; `connect_by` gives up at the deadline.
#[test]
fn outer_cleanup_connect_is_bounded_by_the_deadline() -> Result<(), Box<dyn Error>> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

    let sandbox = tempfile::tempdir()?;
    let path = sandbox.path().join("anchor.sock");
    let listener = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let address = SocketAddrUnix::new(&path)?;
    rustix::net::bind(&listener, &address)?;
    rustix::net::listen(&listener, 0)?;
    // Fill the backlog: connections that are never accepted.
    let mut pending = Vec::new();
    for _ in 0..64 {
        let socket = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )?;
        match rustix::net::connect(&socket, &address) {
            Ok(()) => pending.push(socket),
            Err(rustix::io::Errno::AGAIN) => break,
            Err(error) => return Err(error.into()),
        }
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let target = path.clone();
    let deadline = Instant::now() + Duration::from_millis(300);
    std::thread::spawn(move || {
        let _ = sender.send(outer_cleanup::connect_by(&target, deadline).map(|_| ()));
    });
    // Measured by the waiter: the connect's own return, by its deadline
    // plus the tolerance (S1-evidence2 fix round 2, finding 15).
    let result = receiver
        .recv_timeout(outer_cleanup::left(deadline) + TOLERANCE)
        .map_err(|_| "the anchor connect blocked past its deadline")?;
    returned_in_time("the anchor connect", deadline);
    let error = result.expect_err("a full backlog accepted the connect");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
    drop(pending);
    Ok(())
}

/// S1 critic r2 finding 4, S1-evidence2 fix round 1: a whole exchange is
/// bounded, not each read. A peer that trickles its reply one byte every
/// 40 ms keeps every single read under a 300 ms timeout, but the reply
/// completes after the deadline: the exchange has no reply.
#[test]
fn outer_cleanup_exchange_is_bounded_as_a_whole() -> Result<(), Box<dyn Error>> {
    use std::io::{BufRead as _, Write as _};

    let sandbox = tempfile::tempdir()?;
    let path = sandbox.path().join("anchor.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    let peer = std::thread::spawn(move || -> std::io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        let mut request = String::new();
        std::io::BufReader::new(stream.try_clone()?).read_line(&mut request)?;
        for byte in b"{\"kind\":\"stopping\",\"padding\":\"xx\"}\n" {
            stream.write_all(&[*byte])?;
            std::thread::sleep(Duration::from_millis(40));
        }
        Ok(())
    });
    let stream = std::os::unix::net::UnixStream::connect(&path)?;
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    let reply = outer_cleanup::exchange(&stream, b"{\"kind\":\"stop\"}\n", deadline, 1024);
    // The exchange's own return, not only its lack of a reply
    // (S1-evidence2 fix round 2, finding 15).
    returned_in_time("the exchange", deadline);
    drop(stream);
    let _ = peer.join();
    assert_eq!(
        reply.map(|reply| String::from_utf8_lossy(&reply).into_owned()),
        None,
        "a reply completed after the deadline was accepted"
    );
    Ok(())
}

/// Runtime §11.2, S1-evidence2 fix round 1: finalization that fails partway,
/// here hashing a missing fake binary, keeps the scenario's outcome in the
/// fallback artifact and records the evidence failure beside it.
#[test]
fn a_finalization_failure_keeps_the_outcome() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let fixture = sandbox.path().join("fixture.json");
    fs::write(&fixture, b"{}")?;
    let evidence = Evidence::new(
        "collector_finalization_failure",
        &sandbox.path().join("no-fake-binary"),
        &fixture,
    )?;
    let artifact = evidence.dir.clone();
    assert!(
        evidence
            .finish("timeout", "the turn never finished")
            .is_err()
    );
    let summary: Value = serde_json::from_slice(&fs::read(artifact.join("summary.json"))?)?;
    assert_eq!(summary["outcome"], "timeout", "{summary}");
    assert_eq!(summary["detail"], "the turn never finished", "{summary}");
    assert_eq!(summary["evidence_complete"], false, "{summary}");
    assert!(summary["evidence_failure"].is_string(), "{summary}");
    Ok(())
}

/// The anchors table of the runtime Store, with the columns the harness
/// reads and the owner columns the collector checks.
const ANCHORS: &str = "CREATE TABLE anchors(anchor_id TEXT, generation TEXT, marker TEXT,
    socket_path TEXT, phase TEXT, pid INTEGER, pgid INTEGER, uid INTEGER, boot_id TEXT,
    pid_namespace TEXT, start_ticks INTEGER, absence_time TEXT, owner_session TEXT,
    owner_turn INTEGER);";

/// This boot's id and this process's PID namespace, as the Store records them.
fn boot_and_namespace() -> Result<(String, String), Box<dyn Error>> {
    Ok((
        fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned(),
        fs::read_link("/proc/self/ns/pid")?
            .to_string_lossy()
            .into_owned(),
    ))
}

/// S1-evidence2 fix round 2, finding 2: the absence predicate validates the
/// full identity as production does before any probe. The reviewer's row
/// (pid 0, start ticks 0, an empty marker, a group that answers `ESRCH`)
/// was accepted as `quiescent`; each invalid field now keeps the anchor
/// uncertain, never probed.
#[test]
fn outer_cleanup_invalid_identity_stays_uncertain() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let store = sandbox.path().join("store.sqlite3");
    let connection = Connection::open(&store)?;
    connection.execute_batch(ANCHORS)?;
    let (boot, namespace) = boot_and_namespace()?;
    let uid = rustix::process::getuid().as_raw();
    // A group above any pid_max: `ESRCH`, the probe's absence answer.
    let absent = 2_147_483_647_i64;
    for (id, marker, pid, pgid, ticks, phase) in [
        ("pid_zero", "m", 0, absent, 1, "arm_intent"),
        ("ticks_zero", "m", absent, absent, 0, "arm_intent"),
        ("marker_empty", "", absent, absent, 1, "arm_intent"),
        ("pid_not_group", "m", absent - 1, absent, 1, "arm_intent"),
        ("phase_unknown", "m", absent, absent, 1, "armed"),
    ] {
        connection.execute(
            "INSERT INTO anchors VALUES (?1,'g',?2,'none.sock',?3,?4,?5,?6,?7,?8,?9,NULL,'s',1)",
            rusqlite::params![id, marker, phase, pid, pgid, uid, boot, namespace, ticks],
        )?;
    }
    let rows = outer_cleanup::snapshot(&store, Instant::now() + outer_cleanup::TEARDOWN)?;
    let cleanup = outer_cleanup::verify(&rows, Instant::now() + outer_cleanup::TEARDOWN);
    assert_eq!(cleanup["status"], "unverified", "{cleanup}");
    assert_eq!(cleanup["absence_proven"], false, "{cleanup}");
    for record in cleanup["records"].as_array().ok_or("no records")? {
        assert_eq!(record["verification"], "invalid_identity", "{record}");
        assert_eq!(record["absence_probe"], "not_probed", "{record}");
        assert_eq!(record["cleanup"], "uncertain", "{record}");
    }
    Ok(())
}

/// S1-evidence2 fix round 2, finding 18: runtime §5.1 lets recovery
/// connect only to an `arm_intent` anchor. A valid `identified` anchor with
/// a listening control socket is never connected to; its absence is
/// observed independently.
#[test]
fn outer_cleanup_never_contacts_a_pre_arm_anchor() -> Result<(), Box<dyn Error>> {
    use std::os::unix::process::CommandExt as _;

    let sandbox = tempfile::tempdir()?;
    let store = sandbox.path().join("store.sqlite3");
    let socket = sandbox.path().join("anchor.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let mut group = Command::new("sleep").arg("0.2").process_group(0).spawn()?;
    let pid = group.id();
    let (_, start_ticks) = outer_cleanup::process_stat(pid).ok_or("no stat for the group")?;
    if !outer_cleanup::reap_by(&mut group, Instant::now() + Duration::from_secs(5)) {
        return Err("the fixture group did not exit in 5 s".into());
    }
    let connection = Connection::open(&store)?;
    connection.execute_batch(ANCHORS)?;
    let (boot, namespace) = boot_and_namespace()?;
    // Both pre-ARM phases: an `intent` row may carry a full identity too,
    // and is probed, never contacted, as production does.
    for (id, phase) in [("a_1", "intent"), ("a_2", "identified")] {
        connection.execute(
            "INSERT INTO anchors VALUES (?1,'g','m',?2,?3,?4,?4,?5,?6,?7,?8,NULL,'s',1)",
            rusqlite::params![
                id,
                socket.to_string_lossy(),
                phase,
                pid,
                rustix::process::getuid().as_raw(),
                boot,
                namespace,
                i64::try_from(start_ticks)?
            ],
        )?;
    }
    let rows = outer_cleanup::snapshot(&store, Instant::now() + outer_cleanup::TEARDOWN)?;
    let cleanup = outer_cleanup::verify(&rows, Instant::now() + outer_cleanup::TEARDOWN);
    let accepted = listener.accept();
    assert!(
        matches!(&accepted, Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "a pre-ARM anchor was connected to: {accepted:?}"
    );
    for record in cleanup["records"].as_array().ok_or("no records")? {
        assert_eq!(record["verification"], "pre_arm_not_contacted", "{cleanup}");
        assert_eq!(record["requested_cleanup"], "none", "{cleanup}");
        assert_eq!(record["absence_probe"], "esrch", "{cleanup}");
    }
    assert_eq!(cleanup["count"], 2, "{cleanup}");
    assert_eq!(cleanup["status"], "quiescent", "{cleanup}");
    Ok(())
}

/// A guard's anchor cleanup of a Store file that does not exist: no anchor
/// was ever committed there, an empty committed inventory. A Store path
/// that exists but cannot be opened stays unverified.
#[test]
fn outer_cleanup_absent_store_has_no_anchors() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let deadline = Instant::now() + outer_cleanup::TEARDOWN;
    let absent = outer_cleanup::anchors_by(&sandbox.path().join("store.sqlite3"), None, deadline);
    assert!(outer_cleanup::anchors_proven(&absent), "{absent}");
    assert_eq!(absent["status"], "no_anchors", "{absent}");
    fs::create_dir(sandbox.path().join("dir.sqlite3"))?;
    let unopenable = outer_cleanup::anchors_by(&sandbox.path().join("dir.sqlite3"), None, deadline);
    assert!(!outer_cleanup::anchors_proven(&unopenable), "{unopenable}");
    assert_eq!(unopenable["status"], "unverified", "{unopenable}");
    Ok(())
}

/// S1-evidence2 fix round 2, finding 12: the snapshot's SQLite busy wait
/// is capped by the time left. The reviewer's exclusive-lock probe waited
/// 1,001 ms with any time left; with 100 ms left it now returns by then.
#[test]
fn outer_cleanup_snapshot_is_bounded_by_the_deadline() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let store = sandbox.path().join("store.sqlite3");
    let connection = Connection::open(&store)?;
    connection.execute_batch("CREATE TABLE anchors(dummy); BEGIN EXCLUSIVE;")?;
    let deadline = Instant::now() + Duration::from_millis(100);
    let locked = outer_cleanup::snapshot(&store, deadline);
    returned_in_time("the locked snapshot", deadline);
    assert!(locked.is_err(), "a locked Store was read");
    let expired = outer_cleanup::snapshot(&store, Instant::now());
    assert!(expired.is_err(), "a snapshot ran with no time left");
    Ok(())
}

/// Parks a sandbox at `root` with `exited`, under `evidenced` with a passing
/// body, and returns its summary and `cleanup.json`, if written.
fn park_passing(
    root: &Path,
    state: &Path,
    expected: evidenced::Expected,
    exited: evidenced::Exited,
) -> Result<(Value, Option<Value>, bool), Box<dyn Error>> {
    let fixture = root.join("fixture.json");
    fs::write(&fixture, b"{}")?;
    let mut artifact = None;
    let result = evidenced::evidenced(|| {
        let evidence = evidenced::open(Path::new(env!("CARGO_BIN_EXE_via")), &fixture)?;
        artifact = Some(evidence.dir.clone());
        evidenced::park(evidence, root.to_owned(), state, expected, exited);
        Ok(())
    });
    let artifact = artifact.ok_or("no artifact")?;
    let summary = serde_json::from_slice(&fs::read(artifact.join("summary.json"))?)?;
    let cleanup = fs::read(artifact.join("cleanup.json"))
        .ok()
        .map(|bytes| serde_json::from_slice(&bytes))
        .transpose()?;
    Ok((summary, cleanup, result.is_ok()))
}

/// S1-evidence2 fix round 2, finding 3: the no-Store waiver never waives the
/// cleanup proof or its deadline. A Store absent only when observed after
/// the teardown deadline is not a proof; a Store present but unreadable
/// as evidence is still snapshotted and verified, and its unverified
/// anchor fails the cleanup, not only the waived evidence.
#[test]
fn collector_evidence_waivers_never_waive_the_cleanup_proof() -> Result<(), Box<dyn Error>> {
    let waived = evidenced::Expected {
        store: false,
        folders: false,
    };
    // The reviewer's `expired_no_store`.
    let sandbox = tempfile::tempdir()?;
    let root = sandbox.path().join("root");
    let state = root.join("state");
    fs::create_dir_all(&state)?;
    let expired = exited(
        Ok(()),
        Instant::now()
            .checked_sub(Duration::from_millis(10))
            .ok_or("clock")?,
    );
    let (summary, cleanup, passed) = park_passing(&root, &state, waived, expired)?;
    assert!(!passed, "an expired no-Store teardown passed: {summary}");
    assert_eq!(summary["outcome"], "pass", "{summary}");
    let cleanup = cleanup.ok_or("no cleanup.json")?;
    assert_eq!(cleanup["anchors"]["status"], "unverified", "{cleanup}");
    assert!(summary["cleanup_failure"].is_string(), "{summary}");

    // The reviewer's `waived_store_error`: a Store without the evidence
    // tables and with an identity-less anchor.
    let sandbox = tempfile::tempdir()?;
    let root = sandbox.path().join("root");
    let state = root.join("state");
    fs::create_dir_all(&state)?;
    Connection::open(state.join("store.sqlite3"))?.execute_batch(&format!(
        "{ANCHORS} INSERT INTO anchors VALUES
             ('a','g','m','none','identified',NULL,NULL,0,'b','n',NULL,NULL,'s',1);"
    ))?;
    let (summary, cleanup, passed) = park_passing(&root, &state, waived, proved())?;
    assert!(
        !passed,
        "a waived Store's unverified anchor passed: {summary}"
    );
    let cleanup = cleanup.ok_or("no cleanup.json")?;
    assert_eq!(cleanup["anchors"]["status"], "unverified", "{cleanup}");
    assert!(
        summary["cleanup_failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("outer cleanup is unverified")),
        "{summary}"
    );
    Ok(())
}

/// S1-evidence2 fix round 2, findings 6 and 16: a failed exit proof still
/// writes `cleanup.json` first, with the exit proof, the guards' records
/// and an explicit anchor record; the proof's failure is recorded beside
/// the outcome.
#[test]
fn collector_failed_exit_still_records_cleanup() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let root = sandbox.path().join("root");
    let state = root.join("state");
    fs::create_dir_all(&state)?;
    let expected = evidenced::Expected {
        store: false,
        folders: false,
    };
    let failed = exited(
        Err("child not reaped".to_owned()),
        Instant::now() + outer_cleanup::TEARDOWN,
    );
    let (summary, cleanup, passed) = park_passing(&root, &state, expected, failed)?;
    assert!(!passed, "an unproven exit passed: {summary}");
    assert_eq!(summary["outcome"], "pass", "{summary}");
    let cleanup = cleanup.ok_or("a failed exit proof wrote no cleanup.json")?;
    assert_eq!(cleanup["exit_proof"]["status"], "unproven", "{cleanup}");
    assert!(cleanup["teardown"].is_object(), "{cleanup}");
    assert!(cleanup["anchors"].is_object(), "{cleanup}");
    assert!(
        summary["cleanup_failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("child not reaped")),
        "{summary}"
    );
    Ok(())
}

/// S1-evidence2 fix round 2, finding 7: a stop command killed at its
/// deadline is reaped within the same deadline, never left a zombie, and
/// the reviewer's `sleep` child is reported reaped.
#[test]
fn outer_cleanup_run_within_reaps_its_killed_child() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let pidfile = sandbox.path().join("pid");
    let mut command = Command::new("sh");
    command
        .args(["-c", "echo $$ > \"$1\"; exec sleep 30", "sh"])
        .arg(&pidfile);
    let deadline = Instant::now() + Duration::from_millis(400);
    let (record, failure) = outer_cleanup::run_within(&mut command, deadline);
    returned_in_time("run_within", deadline);
    assert_eq!(record["status"], "timed_out", "{record}");
    assert_eq!(record["reaped"], true, "{record}");
    assert_eq!(failure, None);
    let pid: u32 = fs::read_to_string(&pidfile)?.trim().parse()?;
    // Reaped: its /proc entry is gone, not a zombie.
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "the killed stop child {pid} was left unreaped"
    );
    Ok(())
}

/// S1-evidence2 fix round 2, finding 14: a reap observed only after its
/// deadline is not a success.
#[test]
fn outer_cleanup_reap_after_the_deadline_fails() -> Result<(), Box<dyn Error>> {
    let mut child = Command::new("true").spawn()?;
    std::thread::sleep(Duration::from_millis(100));
    let expired = Instant::now()
        .checked_sub(Duration::from_millis(10))
        .ok_or("clock")?;
    assert!(
        !outer_cleanup::reap_by(&mut child, expired),
        "a reap after the deadline was accepted"
    );
    reap(&mut child)
}

/// S1-evidence2 fix round 2, finding 4: a helper that cannot return an
/// error raises a typed timeout with `panic_any`; `evidenced` records it
/// as `timeout`, not `fail`.
#[test]
fn a_raised_typed_timeout_stays_a_timeout() -> Result<(), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let root = sandbox.path().join("root");
    let state = root.join("state");
    fs::create_dir_all(&state)?;
    let fixture = root.join("fixture.json");
    fs::write(&fixture, b"{}")?;
    let mut artifact = None;
    let raised = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        evidenced::evidenced(|| -> Result<(), Box<dyn Error>> {
            let evidence = evidenced::open(Path::new(env!("CARGO_BIN_EXE_via")), &fixture)?;
            artifact = Some(evidence.dir.clone());
            let expected = evidenced::Expected {
                store: false,
                folders: false,
            };
            evidenced::park(evidence, root.clone(), &state, expected, proved());
            std::panic::panic_any(ScenarioError::Timeout("no result within 1s".to_owned()))
        })
    }));
    assert!(raised.is_err(), "the raised timeout was swallowed");
    let artifact = artifact.ok_or("no artifact")?;
    let summary: Value = serde_json::from_slice(&fs::read(artifact.join("summary.json"))?)?;
    assert_eq!(summary["outcome"], "timeout", "{summary}");
    assert_eq!(summary["detail"], "no result within 1s", "{summary}");
    Ok(())
}

/// S1-evidence2 fix round 2, findings 5 and 11: a command that outlives its
/// bound returns by the bound, run and reap included, with its timeout
/// kept.
#[test]
fn run_command_bound_covers_the_reap() -> Result<(), Box<dyn Error>> {
    let mut command = Command::new("sleep");
    command.arg("30");
    let bound = Duration::from_millis(400);
    let deadline = Instant::now() + bound;
    let capture = scenario::run_command(&mut command, bound)?;
    returned_in_time("run_command", deadline);
    assert!(capture.timed_out, "the hung command was not timed out");
    assert!(!capture.status.success());
    assert!(capture.attached.is_empty(), "{:?}", capture.attached);
    Ok(())
}
