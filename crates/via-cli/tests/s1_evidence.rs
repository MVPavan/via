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

use daemon::{Daemon, Sandbox, TestResult, cli, collect_available, events, failure, infra};
use scenario::{ScenarioError, run_scenario};
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

/// Milliseconds since the Unix epoch of a VIA timestamp
/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC).
fn unix_ms(at: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| at.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second, milli) = (
        number(11..13)?,
        number(14..16)?,
        number(17..19)?,
        number(20..23)?,
    );
    // Days from the civil date (Howard Hinnant's algorithm).
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let of_era = shifted - era * 400;
    let of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let of_cycle = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    let days = era * 146_097 + of_cycle - 719_468;
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + milli)
}

/// Design §7.1, §7.5, §13.2: the fake writes 1 MiB to its stderr. The
/// operating system writes it to `stderr.log`, which holds exactly those
/// bytes; the bytes are not progress, so the idle deadline still strikes one
/// budget after acceptance. Proven by the daemon's and the file's own
/// timestamps (A50): the idle order comes less than one budget after the
/// bytes landed, which a reset by them would forbid. `logs` lists the file with its size, `folder`
/// absolute, `transcript` and `vendor_session_id` `null`, and the envelope's
/// `evidence` equals it.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario proves the stderr file, the idle order and the logs listing"
)]
fn s1_evidence_stderr_is_written_by_the_os_and_listed() -> TestResult {
    const STDERR: usize = 1024 * 1024;
    const IDLE_MS: i64 = 1500;
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
            // The realtime clock against the monotonic one, across the
            // window the daemon's and the file's timestamps fall in.
            let clocks = (std::time::SystemTime::now(), Instant::now());
            let session = spawn(&sandbox, evidence, "stderr", &["--idle-ms", "1500"])?;
            sandbox.await_gate("quiet")?;
            let accepted_at = Instant::now();
            // Elapsed time only: the stderr bytes land inside the idle window.
            thread::sleep(Duration::from_millis(800));
            sandbox.release_gate("quiet")?;
            await_event(&sandbox, evidence, &session, "cancel.requested")?;
            let ordered_after = accepted_at.elapsed();
            let wall = std::time::SystemTime::now()
                .duration_since(clocks.0)
                .map_or(i128::MIN, |elapsed| elapsed.as_millis().cast_signed());
            let stepped = (wall - clocks.1.elapsed().as_millis().cast_signed()).abs() > 100;
            let envelope = wait(&sandbox, evidence, "wait", &format!("{session}/1"))?;
            let listed = logs(&sandbox, evidence, "logs", &session)?;
            record(&sandbox, evidence, &[&envelope], &[&session])?;
            check(
                envelope["state"] == "failed" && envelope["failure"]["class"] == "deadline_idle",
                || format!("idle envelope: {envelope}"),
            )?;
            let expected = folder(&sandbox, &session, 1);
            let landed = fs::metadata(expected.join("stderr.log"))
                .and_then(|metadata| metadata.modified())
                .map_err(infra)?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(infra)?;
            let landed = i64::try_from(landed.as_millis()).map_err(infra)?;
            let lifecycle = events(&sandbox, evidence, "events_idle", &session)?;
            let at = |kind: &str| {
                lifecycle
                    .iter()
                    .find(|event| event["type"] == kind)
                    .and_then(|event| event["at"].as_str())
                    .and_then(unix_ms)
                    .ok_or_else(|| failure(format!("no {kind} time in {lifecycle:?}")))
            };
            let (started, ordered) = (at("turn.started")?, at("cancel.requested")?);
            evidence
                .write(
                    "idle_timing.json",
                    json!({"idle_ms":IDLE_MS,"started_to_order_ms":ordered - started,
                        "started_to_stderr_ms":landed - started,
                        "stderr_to_order_ms":ordered - landed,
                        "test_clock_ordered_after_ms":ordered_after.as_millis(),
                        "realtime_clock_stepped":stepped})
                    .to_string()
                    .as_bytes(),
                )
                .map_err(infra)?;
            if stepped {
                // The realtime clock stepped inside the window (seen on
                // WSL), so its timestamps cannot be compared: the order is
                // judged on the monotonic clock instead. Stderr as progress
                // would have put it past 800 ms + one budget.
                check(ordered_after < Duration::from_millis(2300), || {
                    format!("the idle order came {ordered_after:?} after acceptance")
                })?;
            } else {
                // The bytes landed inside the idle window, before the order.
                check(started < landed && landed < ordered, || {
                    format!("stderr landed at {landed}, outside {started}..{ordered}")
                })?;
                // Stderr as progress would have put the order a whole budget
                // after the bytes.
                check(ordered - landed < IDLE_MS, || {
                    format!(
                        "the idle order came {} ms after the stderr bytes: they reset idle",
                        ordered - landed
                    )
                })?;
            }
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
        |evidence| collect_available(evidence, &sandbox.state, &sandbox.teardown),
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
    // Task 4 design §2.2: `text` is skipped unread, so the malformed field
    // is the turn ID.
    let head = r#"{"type":"text","vendor_turn_id":1,"pad":""#;
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
        |evidence| collect_available(evidence, &sandbox.state, &sandbox.teardown),
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
        |evidence| collect_available(evidence, &sandbox.state, &sandbox.teardown),
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
        |evidence| collect_available(evidence, &sandbox.state, &sandbox.teardown),
    );
    report.require_pass()
}

/// T4-flake: the harness readiness probe never starts a daemon. Here the
/// harness's daemon serves another deployment, so the sandbox's socket never
/// appears; an auto-starting probe would start a second daemon in the
/// sandbox, without the child's failpoints, and report it ready. Under
/// parallel runs that second daemon won `daemon.lock` from the child.
/// Positive evidence for this case: the child served its own socket, then
/// left through its idle exit (status 0, a clean `idle` shutdown summary);
/// readiness reported exactly that exit; the sandbox never got a socket or
/// a Store, which any daemon started there would create. By design no
/// Store opens in the sandbox and no turn runs, so the evidence declares
/// neither (T4-fix, Close finding 1): it holds the probe results, the
/// readiness message and the child's shutdown summary.
#[cfg(feature = "test-failpoints")]
#[test]
fn s1_evidence_harness_readiness_never_starts_a_daemon() -> TestResult {
    let sandbox = Sandbox::new(&script(1, "unused", &[]))?;
    let mut evidence = Evidence::new("s1_evidence_readiness", &sandbox.fake, &sandbox.fixture)?;
    evidence.store_expected = false;
    let elsewhere = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let (state, runtime) = (
        elsewhere.path().join("state"),
        elsewhere.path().join("runtime"),
    );
    for dir in [&state, &runtime] {
        daemon::private_dir(dir)?;
    }
    let report = run_scenario(
        evidence,
        |evidence| {
            // Observes the child serving its own socket, over a direct
            // connection.
            let served = {
                let runtime = runtime.clone();
                thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    loop {
                        if let Some(pid) = daemon::serving_pid(&runtime) {
                            return Some(pid);
                        }
                        if Instant::now() >= deadline {
                            return None;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                })
            };
            let started = Daemon::start_with(&sandbox, evidence, |command| {
                command.env("VIA_STATE_DIR", &state);
                command.env("VIA_RUNTIME_DIR", &runtime);
                command.env("VIA_TEST_IDLE_EXIT_MS", "3000");
            });
            let socket = sandbox.runtime.join("via.sock").exists();
            let store = sandbox.state.join("store.sqlite3").exists();
            // Stops a daemon the probe may have started; refused when there
            // is none.
            let stop = sandbox
                .run(
                    &["daemon", "stop", "--force", "--json"],
                    Duration::from_secs(20),
                )
                .map_err(infra)?;
            let reported = match &started {
                Err(error) => error.to_string(),
                Ok(_) => "ready".to_owned(),
            };
            drop(started);
            let served = served
                .join()
                .map_err(|_| infra("the serving probe panicked"))?;
            // Task 4 design §7.6: the shutdown summary is the child's
            // `via.log` line.
            let log = fs::read_to_string(state.join("via.log")).map_err(infra)?;
            let summary = log
                .lines()
                .rev()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find_map(|line| line.get("daemon_shutdown").cloned())
                .unwrap_or(Value::Null);
            evidence
                .write(
                    "readiness.json",
                    json!({"reported":reported,"child_served_pid":served,
                        "sandbox_socket":socket,"sandbox_store":store,
                        "stop_exit":stop.status.code(),
                        "stop_stdout":String::from_utf8_lossy(&stop.stdout)})
                    .to_string()
                    .as_bytes(),
                )
                .map_err(infra)?;
            evidence
                .write("daemon_shutdown.json", summary.to_string().as_bytes())
                .map_err(infra)?;
            let idle_exit = summary["mode"] == "idle" && summary["disposition"] == "clean";
            check(
                reported == "fail: daemon exited before readiness: exit status: 0"
                    && served.is_some()
                    && idle_exit
                    && !socket
                    && !store,
                || {
                    format!(
                        "readiness reported {reported:?}; child served {served:?}; \
                         shutdown {summary}; sandbox socket {socket}, Store {store}"
                    )
                },
            )
        },
        // The teardown's report: the refused generation's reap and its
        // (absent) Store's empty inventory.
        |evidence| collect_available(evidence, &sandbox.state, &sandbox.teardown),
    );
    report.require_pass()
}

/// S1-evidence2 fix round 2, finding 1: every daemon generation keeps its
/// own cleanup report and trace, and collection validates all of them. The
/// first generation's clean report used to stand for the whole scenario:
/// a later generation's report was never written (`create_new` on the
/// existing `cleanup.json`), so its failure was lost. Here the second
/// generation's report cannot be written, and collection fails.
#[test]
fn s1_evidence_every_daemon_generation_is_validated() -> TestResult {
    let sandbox = Sandbox::new(&json!({}))?;
    let evidence = Evidence::new("s1_evidence_generations", &sandbox.fake, &sandbox.fixture)?;
    let first = Daemon::start(&sandbox, &evidence)?;
    let first_pid = first.pid();
    first.shutdown()?;
    let second = Daemon::start(&sandbox, &evidence)?;
    let second_pid = second.pid();
    // A directory holds the second generation's report path.
    fs::DirBuilder::new().create(evidence.dir.join("cleanup-2.json"))?;
    drop(second);
    let collected = collect_available(&evidence, &sandbox.state, &sandbox.teardown);
    let cleanup: Value = serde_json::from_slice(&fs::read(evidence.dir.join("cleanup.json"))?)?;
    let pids: Vec<Value> = cleanup["generations"]
        .as_array()
        .ok_or("cleanup.json has no generations")?
        .iter()
        .map(|generation| generation["direct_child"]["pid"].clone())
        .collect();
    assert_eq!(pids, [json!(first_pid), json!(second_pid)], "{cleanup}");
    assert_eq!(cleanup["complete"], false, "{cleanup}");
    let error = collected.expect_err("a generation's lost cleanup report passed");
    assert!(
        error.detail().contains("cleanup report not written"),
        "{error}"
    );
    assert!(evidence.dir.join("cleanup-1.json").is_file());
    let trace = fs::read_to_string(evidence.dir.join("daemon.trace"))?;
    for generation in 1..=2 {
        assert!(
            trace.contains(&format!("=== daemon generation {generation} ===")),
            "{trace}"
        );
    }
    // Finalized so the self-test leaves no half-written artifact; its
    // required Store evidence is absent by design.
    let _ = evidence.finish("pass", "daemon generation self-test");
    Ok(())
}
