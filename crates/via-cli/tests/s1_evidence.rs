//! Task 4 R8 (design §4.4, §7): the turn's evidence folder through the real
//! `via` binary and daemon. VIA keeps no copy of vendor traffic: the vendor's
//! stderr is `stderr.log`, written by the operating system; a message VIA
//! cannot decode is `undecoded.bin`, named by the turn's failure; `logs`
//! says where the evidence is without reading it. Written before the
//! evidence folder.

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use daemon::{Daemon, Sandbox, TestResult, cli, events, failure, infra};
use scenario::{ScenarioError, collect_available, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// The bytes the fake's `stderr` step writes: `a` to `z`, repeated.
fn stderr_bytes(count: usize) -> Vec<u8> {
    (0..count)
        .map(|index| b'a' + u8::try_from(index % 26).unwrap_or(0))
        .collect()
}

fn accepted(turn: u32) -> Value {
    json!({"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":format!("fake-turn-{turn}")}})
}

fn completed(turn: u32, text: &str) -> Value {
    json!({"action":"emit","message":{"type":"terminal","vendor_turn_id":format!("fake-turn-{turn}"),"status":"completed","final_text":text,"stop_reason":"end_turn"}})
}

fn script(turn: u32, prompt: &str, steps: &[Value]) -> Value {
    json!({"expected_request":{"type":"start","id":1,"turn":turn,"prompt":prompt},"steps":steps})
}

fn spawn(
    sandbox: &Sandbox,
    evidence: &Evidence,
    prompt: &str,
    extra: &[&str],
) -> Result<String, ScenarioError> {
    let mut args = vec![
        "spawn",
        "--harness",
        "fake",
        "--model",
        "fake",
        "--prompt",
        prompt,
        "--handle",
        HANDLE,
        "--background",
        "--json",
    ];
    args.extend_from_slice(extra);
    let receipt = cli(sandbox, evidence, &format!("spawn_{prompt}"), &args)?;
    receipt["session_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| failure(format!("receipt has no session: {receipt}")))
}

fn wait(
    sandbox: &Sandbox,
    evidence: &Evidence,
    name: &str,
    address: &str,
) -> Result<Value, ScenarioError> {
    cli(
        sandbox,
        evidence,
        name,
        &["wait", address, "--timeout-ms", "30000", "--json"],
    )
}

fn logs(
    sandbox: &Sandbox,
    evidence: &Evidence,
    name: &str,
    address: &str,
) -> Result<Value, ScenarioError> {
    cli(sandbox, evidence, name, &["logs", address, "--json"])
}

/// The turn's evidence folder as the daemon names it.
fn folder(sandbox: &Sandbox, session: &str, turn: u32) -> PathBuf {
    sandbox
        .state
        .join("evidence")
        .join(session)
        .join(turn.to_string())
}

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

/// Whether `named` is the absolute path of `expected`.
fn same_path(named: &Value, expected: &Path) -> bool {
    named.as_str().is_some_and(|named| {
        Path::new(named).is_absolute()
            && fs::canonicalize(named).ok() == fs::canonicalize(expected).ok()
            && fs::canonicalize(expected).is_ok()
    })
}

/// Records the session's envelopes and events as scenario evidence.
fn record(
    sandbox: &Sandbox,
    evidence: &Evidence,
    envelopes: &[&Value],
    sessions: &[&str],
) -> Result<(), ScenarioError> {
    let mut lines = String::new();
    for envelope in envelopes {
        lines.push_str(&envelope.to_string());
        lines.push('\n');
    }
    evidence
        .write("envelopes.ndjson", lines.as_bytes())
        .map_err(infra)?;
    let mut all = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
        all.extend(events(
            sandbox,
            evidence,
            &format!("events_{index}"),
            session,
        )?);
    }
    evidence
        .write(
            "events.ndjson",
            serde_json::to_vec(&all).map_err(infra)?.as_slice(),
        )
        .map_err(infra)
}

/// Polls the session's events until one of `kind` is durable.
fn await_event(
    sandbox: &Sandbox,
    evidence: &Evidence,
    session: &str,
    kind: &str,
) -> Result<(), ScenarioError> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if events(sandbox, evidence, "poll", session)?
            .iter()
            .any(|event| event["type"] == kind)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ScenarioError::Timeout(format!("no {kind} in {session}")));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Design §7.1, §7.5, §13.2: the fake writes 1 MiB to its stderr. The
/// operating system writes it to `stderr.log`, which holds exactly those
/// bytes; the bytes are not progress, so the idle deadline still strikes one
/// budget after acceptance. `logs` lists the file with its size, `folder`
/// absolute, `transcript` and `vendor_session_id` `null`, and the envelope's
/// `evidence` equals it.
#[test]
fn s1_evidence_stderr_is_written_by_the_os_and_listed() -> TestResult {
    const STDERR: usize = 1024 * 1024;
    let steps = vec![
        accepted(1),
        json!({"action":"gate","name":"quiet"}),
        json!({"action":"stderr","bytes":STDERR}),
        json!({"action":"expect_request","expected":{"type":"interrupt"}}),
        json!({"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"interrupted","final_text":"","stop_reason":"interrupted"}}),
    ];
    let sandbox = Sandbox::new(&script(1, "stderr", &steps))?;
    let evidence = Evidence::new("s1_evidence_stderr", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let session = spawn(&sandbox, evidence, "stderr", &["--idle-ms", "1500"])?;
            sandbox.await_gate("quiet")?;
            let accepted_at = Instant::now();
            // Elapsed time only: the stderr bytes land inside the idle window.
            thread::sleep(Duration::from_millis(800));
            sandbox.release_gate("quiet")?;
            await_event(&sandbox, evidence, &session, "cancel.requested")?;
            let ordered_after = accepted_at.elapsed();
            let envelope = wait(&sandbox, evidence, "wait", &format!("{session}/1"))?;
            let listed = logs(&sandbox, evidence, "logs", &session)?;
            record(&sandbox, evidence, &[&envelope], &[&session])?;
            check(
                envelope["state"] == "failed" && envelope["failure"]["class"] == "deadline_idle",
                || format!("idle envelope: {envelope}"),
            )?;
            // Stderr as progress would have moved the order past 2.3 s.
            check(ordered_after < Duration::from_millis(2200), || {
                format!("the idle order came {ordered_after:?} after acceptance")
            })?;
            let expected = folder(&sandbox, &session, 1);
            let written = fs::read(expected.join("stderr.log")).map_err(infra)?;
            check(written == stderr_bytes(STDERR), || {
                format!("stderr.log holds {} bytes, not the fake's", written.len())
            })?;
            check(
                listed["session_id"] == session.as_str()
                    && listed["turn"] == 1
                    && listed["vendor_session_id"].is_null()
                    && listed["transcript"].is_null()
                    && same_path(&listed["folder"], &expected)
                    && listed["files"] == json!([{"name":"stderr.log","bytes":STDERR}]),
                || format!("logs: {listed}"),
            )?;
            check(
                envelope["evidence"]
                    == json!({"folder":listed["folder"],"transcript":listed["transcript"]}),
                || {
                    format!(
                        "envelope evidence {} differs from logs {listed}",
                        envelope["evidence"]
                    )
                },
            )?;
            check(envelope.get("raw_spans").is_none(), || {
                format!("the envelope still has raw_spans: {envelope}")
            })
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// Design §7.3, §13.2: a malformed known message of 200 KiB fails the turn
/// `protocol` and a 2 MiB line fails it `overflow`; each turn's
/// `undecoded.bin` holds the message's first 64 KiB, and `failure.message`
/// names the file, and the length when it is known.
#[test]
fn s1_evidence_undecoded_message_is_saved_and_named() -> TestResult {
    const MALFORMED: usize = 200 * 1024;
    let head = r#"{"type":"text","vendor_turn_id":"fake-turn-1","text":1,"pad":""#;
    let tail = "\"}\n";
    let malformed = format!(
        "{head}{}{tail}",
        "m".repeat(MALFORMED - head.len() - tail.len())
    );
    let huge = format!("{}\n", "h".repeat(2 * 1024 * 1024 - 1));
    let scripts = json!({"scripts":[
        script(1, "malformed", &[
            accepted(1),
            json!({"action":"emit_raw","text":malformed}),
            json!({"action":"hang"}),
        ]),
        script(1, "huge", &[
            accepted(1),
            json!({"action":"emit_raw","text":huge}),
            json!({"action":"hang"}),
        ]),
    ]});
    let sandbox = Sandbox::new(&scripts)?;
    let evidence = Evidence::new("s1_evidence_undecoded", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let bad = spawn(&sandbox, evidence, "malformed", &[])?;
            let big = spawn(&sandbox, evidence, "huge", &[])?;
            let bad_envelope = wait(&sandbox, evidence, "wait_malformed", &format!("{bad}/1"))?;
            let big_envelope = wait(&sandbox, evidence, "wait_huge", &format!("{big}/1"))?;
            record(
                &sandbox,
                evidence,
                &[&bad_envelope, &big_envelope],
                &[&bad, &big],
            )?;
            for (session, envelope, class, message, length) in [
                (
                    &bad,
                    &bad_envelope,
                    "protocol",
                    malformed.as_bytes(),
                    Some(MALFORMED),
                ),
                (&big, &big_envelope, "overflow", huge.as_bytes(), None),
            ] {
                let file = folder(&sandbox, session, 1).join("undecoded.bin");
                check(
                    envelope["state"] == "failed" && envelope["failure"]["class"] == class,
                    || format!("{class}: envelope {envelope}"),
                )?;
                let saved = fs::read(&file).map_err(|error| {
                    failure(format!("{class}: {} unreadable: {error}", file.display()))
                })?;
                check(saved == message[..64 * 1024], || {
                    format!("{class}: undecoded.bin holds {} bytes", saved.len())
                })?;
                let text = envelope["failure"]["message"].as_str().unwrap_or_default();
                let canonical = fs::canonicalize(&file).map_err(infra)?;
                check(
                    text.contains(&file.display().to_string())
                        || text.contains(&canonical.display().to_string()),
                    || format!("{class}: failure message does not name the file: {text}"),
                )?;
                if let Some(length) = length {
                    check(text.contains(&format!("{length} bytes")), || {
                        format!("{class}: failure message lacks the length: {text}")
                    })?;
                }
                let listed = logs(&sandbox, evidence, &format!("logs_{class}"), session)?;
                check(
                    listed["files"].as_array().is_some_and(|files| {
                        files.contains(&json!({"name":"undecoded.bin","bytes":64 * 1024}))
                    }),
                    || format!("{class}: logs does not list undecoded.bin: {listed}"),
                )?;
            }
            Ok(())
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// Design §7.2, §13.2: the turn's folder is created exclusively before
/// Host's acquisition. A pre-created `<turn>` folder fails the turn `store`
/// with no anchor intent and no process.
#[test]
fn s1_evidence_folder_failure_fails_store_before_launch() -> TestResult {
    let scripts = json!({"scripts":[
        script(1, "first", &[
            accepted(1),
            json!({"action":"gate","name":"hold"}),
            completed(1, "first reply"),
        ]),
        script(2, "second", &[
            accepted(2),
            json!({"action":"report_pids"}),
            completed(2, "second reply"),
        ]),
    ]});
    let sandbox = Sandbox::new(&scripts)?;
    let evidence = Evidence::new(
        "s1_evidence_folder_failure",
        &sandbox.fake,
        &sandbox.fixture,
    )?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let session = spawn(&sandbox, evidence, "first", &[])?;
            sandbox.await_gate("hold")?;
            cli(
                &sandbox,
                evidence,
                "resume",
                &[
                    "resume", &session, "--prompt", "second", "--handle", HANDLE, "--json",
                ],
            )?;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(folder(&sandbox, &session, 2))
                .map_err(infra)?;
            sandbox.release_gate("hold")?;
            let first = wait(&sandbox, evidence, "wait_first", &format!("{session}/1"))?;
            let second = wait(&sandbox, evidence, "wait_second", &format!("{session}/2"))?;
            record(&sandbox, evidence, &[&first, &second], &[&session])?;
            check(first["state"] == "completed", || format!("turn 1: {first}"))?;
            check(
                second["state"] == "failed" && second["failure"]["class"] == "store",
                || format!("turn 2: {second}"),
            )?;
            let intents = sandbox.count(&format!(
                "SELECT count(*) FROM anchors WHERE owner_session='{session}' AND owner_turn=2"
            ))?;
            check(intents == 0, || {
                format!("turn 2 has {intents} anchor intents")
            })?;
            check(!sandbox.sync.join("agent.pid").exists(), || {
                "turn 2's vendor ran".to_owned()
            })?;
            let folder = folder(&sandbox, &session, 2);
            let entries = fs::read_dir(&folder).map_err(infra)?.count();
            check(entries == 0, || {
                format!("{} gained {entries} entries", folder.display())
            })
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}

/// Design §4.4, §13.2: a session address selects the running turn, else the
/// latest submitted one; a turn address selects that turn; a queued turn has
/// `folder: null` and no files. VIA never opens a listed file: one the daemon
/// cannot read still lists with its size.
#[test]
fn s1_c1_logs_selects_the_turn_and_never_reads_files() -> TestResult {
    const STDERR: usize = 4096;
    let scripts = json!({"scripts":[
        script(1, "first", &[
            accepted(1),
            json!({"action":"stderr","bytes":STDERR}),
            json!({"action":"gate","name":"hold"}),
            completed(1, "first reply"),
        ]),
        script(2, "second", &[accepted(2), completed(2, "second reply")]),
    ]});
    let sandbox = Sandbox::new(&scripts)?;
    let evidence = Evidence::new("s1_c1_logs_selects", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let session = spawn(&sandbox, evidence, "first", &[])?;
            sandbox.await_gate("hold")?;
            cli(
                &sandbox,
                evidence,
                "resume",
                &[
                    "resume", &session, "--prompt", "second", "--handle", HANDLE, "--json",
                ],
            )?;
            let first_folder = folder(&sandbox, &session, 1);
            let stderr = first_folder.join("stderr.log");
            fs::set_permissions(&stderr, fs::Permissions::from_mode(0o000)).map_err(infra)?;
            let stderr_file = json!([{"name":"stderr.log","bytes":STDERR}]);

            let running = logs(&sandbox, evidence, "logs_running", &session)?;
            check(
                running["turn"] == 1
                    && same_path(&running["folder"], &first_folder)
                    && running["files"] == stderr_file,
                || format!("session address while turn 1 runs: {running}"),
            )?;
            let queued = logs(&sandbox, evidence, "logs_queued", &format!("{session}/2"))?;
            check(
                queued["session_id"] == session.as_str()
                    && queued["turn"] == 2
                    && queued["folder"].is_null()
                    && queued["files"] == json!([])
                    && queued["transcript"].is_null()
                    && queued["vendor_session_id"].is_null(),
                || format!("queued turn: {queued}"),
            )?;

            fs::set_permissions(&stderr, fs::Permissions::from_mode(0o600)).map_err(infra)?;
            sandbox.release_gate("hold")?;
            let first = wait(&sandbox, evidence, "wait_first", &format!("{session}/1"))?;
            let second = wait(&sandbox, evidence, "wait_second", &format!("{session}/2"))?;
            record(&sandbox, evidence, &[&first, &second], &[&session])?;
            let latest = logs(&sandbox, evidence, "logs_latest", &session)?;
            check(
                latest["turn"] == 2
                    && same_path(&latest["folder"], &folder(&sandbox, &session, 2))
                    && latest["files"] == json!([{"name":"stderr.log","bytes":0}]),
                || format!("session address after both ended: {latest}"),
            )?;
            let addressed = logs(&sandbox, evidence, "logs_turn_1", &format!("{session}/1"))?;
            check(
                addressed["turn"] == 1
                    && same_path(&addressed["folder"], &first_folder)
                    && addressed["files"] == stderr_file,
                || format!("turn address: {addressed}"),
            )
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}
