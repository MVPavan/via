//! Task 4 design §2.3 "No envelope overrun" and §6.4 through the real `via`
//! binary and daemon: a final text longer than 256 KiB encoded is written to
//! the turn's `final_text.txt`, which the envelope names only once it is
//! durable; the envelope is at most 1 MiB by construction, every member
//! bounded, the receipt refusing the members a caller sizes. Written before
//! the final-text pieces, the file and the bounded members.
#![cfg(feature = "test-failpoints")]

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/failpoints.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod failpoints;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use daemon::{Daemon, Raw, Sandbox, TestResult, cli, failure, infra, request};
use failpoints::Failpoints;
use scenario::{ScenarioError, collect_available, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
/// `final_text` inline up to this many bytes encoded (design §6.4).
const INLINE: usize = 256 * 1024;
/// The lowered file cap (`VIA_TEST_FINAL_TEXT_FILE_MAX`): odd, so an
/// all-`é` text is cut inside a character.
const FILE_MAX: usize = 300_001;

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

/// A one-turn script for `prompt`: accepted, then a terminal carrying
/// `final_text` and `stop_reason`.
fn script(prompt: &str, final_text: &str, stop_reason: &str) -> Value {
    json!({"expected_request":{"type":"start","id":1,"turn":1,"prompt":prompt},"steps":[
        {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
        {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1",
            "status":"completed","final_text":final_text,"stop_reason":stop_reason}},
    ]})
}

/// One scenario's deployment with its failpoint directory.
struct Setup {
    sandbox: Sandbox,
    failpoints: Failpoints,
}

impl Setup {
    fn new(fixture: &Value) -> TestResult<Self> {
        let sandbox = Sandbox::new(fixture)?;
        let root = sandbox
            .state
            .parent()
            .ok_or("sandbox state has no parent")?
            .to_owned();
        let failpoints = Failpoints::new(&root)?;
        Ok(Self {
            sandbox,
            failpoints,
        })
    }

    fn start(&self, evidence: &Evidence) -> Result<Daemon<'_>, ScenarioError> {
        Daemon::start_with(&self.sandbox, evidence, |command| {
            self.failpoints.activate(command);
            command.env("VIA_TEST_FINAL_TEXT_FILE_MAX", FILE_MAX.to_string());
        })
    }

    /// Spawns `prompt`'s session in the background; its session ID.
    fn spawn(&self, evidence: &Evidence, prompt: &str) -> Result<String, ScenarioError> {
        let receipt = cli(
            &self.sandbox,
            evidence,
            &format!("spawn_{prompt}"),
            &[
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
            ],
        )?;
        receipt["session_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| failure(format!("receipt has no session: {receipt}")))
    }

    fn wait(&self, evidence: &Evidence, session: &str) -> Result<Value, ScenarioError> {
        let envelope = cli(
            &self.sandbox,
            evidence,
            &format!("wait_{session}"),
            &[
                "wait",
                &format!("{session}/1"),
                "--timeout-ms",
                "30000",
                "--json",
            ],
        )?;
        evidence
            .write(
                &format!("envelope_{session}.json"),
                envelope.to_string().as_bytes(),
            )
            .map_err(infra)?;
        Ok(envelope)
    }

    fn folder(&self, session: &str) -> PathBuf {
        self.sandbox.state.join("evidence").join(session).join("1")
    }
}

/// The envelope's named file: its path is `final_text.txt` in the turn's
/// folder, a 0600 regular file holding exactly `bytes` bytes, and those
/// bytes are `text`'s first, ending at a character boundary.
fn named_file(
    setup: &Setup,
    session: &str,
    envelope: &Value,
    text: &str,
) -> Result<usize, ScenarioError> {
    let named = &envelope["final_text_file"];
    let path = named["path"].as_str().unwrap_or_default();
    let expected = setup.folder(session).join("final_text.txt");
    check(
        Path::new(path).is_absolute()
            && fs::canonicalize(path).ok() == fs::canonicalize(&expected).ok()
            && fs::canonicalize(&expected).is_ok(),
        || format!("final_text_file path {named} is not {}", expected.display()),
    )?;
    let bytes = named["bytes"]
        .as_u64()
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| failure(format!("final_text_file has no bytes: {named}")))?;
    let written = fs::read(&expected).map_err(infra)?;
    let mode = fs::symlink_metadata(&expected)
        .map_err(infra)?
        .permissions()
        .mode()
        & 0o777;
    check(mode == 0o600, || format!("final_text.txt mode {mode:o}"))?;
    check(
        written.len() == bytes
            && text.is_char_boundary(bytes)
            && written == text.as_bytes()[..bytes],
        || {
            format!(
                "final_text.txt holds {} bytes, named {bytes}, of a {}-byte text",
                written.len(),
                text.len()
            )
        },
    )?;
    Ok(bytes)
}

/// Design §2.3, §6.4, §13.2 [t4r16.7.8, t4r17.3]: a text of exactly
/// 256 KiB encoded is inline; one more byte puts the exact text in
/// `final_text.txt`, with `final_text: null` and `final_text_file {path,
/// bytes, truncated: false}`; past a lowered file cap the file ends at a
/// character boundary with `truncated: true` and the turn completes; a
/// write cut inside a character (`final_text.write.short`) fails the turn
/// `store` with the file cut to its last complete character, named with its
/// `bytes` and `truncated: true`; a write failing before any byte
/// (`final_text.write.fail`) fails it `store` with the empty file named,
/// `bytes: 0` and `truncated: true`; a failed sync (`final_text.sync.fail`)
/// fails it `store` with `final_text_file: null`; killed right after the
/// terminal commit, the restarted daemon finds the named file.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario keeps each file case and the restart on one sandbox"
)]
fn s1_bounds_final_text_spills_to_a_file() -> TestResult {
    // The JSON string `"a…a"` of INLINE bytes, and one byte more.
    let inline = "a".repeat(INLINE - 2);
    let spilled = format!("{inline}b");
    // 400,000 bytes of two-byte characters: past the lowered cap.
    let capped = "é".repeat(200_000);
    let short = "é".repeat(140_000);
    let unwritten = format!("{inline}fail");
    let synced = format!("{inline}sync");
    let crashed = format!("{inline}crash");
    let setup = Setup::new(&json!({"scripts":[
        script("inline", &inline, "end_turn"),
        script("spilled", &spilled, "end_turn"),
        script("capped", &capped, "end_turn"),
        script("short", &short, "end_turn"),
        script("unwritten", &unwritten, "end_turn"),
        script("synced", &synced, "end_turn"),
        script("crashed", &crashed, "end_turn"),
    ]}))?;
    let evidence = Evidence::new(
        "s1_bounds_final_text_spills",
        &setup.sandbox.fake,
        &setup.sandbox.fixture,
    )?;
    let report = run_scenario(
        evidence,
        |evidence| {
            {
                let _daemon = setup.start(evidence)?;
                let session = setup.spawn(evidence, "inline")?;
                let envelope = setup.wait(evidence, &session)?;
                check(
                    envelope["state"] == "completed"
                        && envelope["final_text"] == inline.as_str()
                        && envelope["final_text_file"].is_null(),
                    || {
                        format!(
                            "the inline text is not inline: {}",
                            envelope["final_text_file"]
                        )
                    },
                )?;
                check(
                    !setup.folder(&session).join("final_text.txt").exists(),
                    || "an inline text left a file".to_owned(),
                )?;

                let session = setup.spawn(evidence, "spilled")?;
                let envelope = setup.wait(evidence, &session)?;
                check(
                    envelope["state"] == "completed"
                        && envelope["final_text"].is_null()
                        && envelope["final_text_file"]["truncated"] == false,
                    || format!("spilled envelope: {}", envelope["final_text_file"]),
                )?;
                let bytes = named_file(&setup, &session, &envelope, &spilled)?;
                check(bytes == spilled.len(), || {
                    format!("{bytes} of {} bytes written", spilled.len())
                })?;

                let session = setup.spawn(evidence, "capped")?;
                let envelope = setup.wait(evidence, &session)?;
                check(
                    envelope["state"] == "completed"
                        && envelope["final_text"].is_null()
                        && envelope["final_text_file"]["truncated"] == true,
                    || format!("capped envelope: {}", envelope["final_text_file"]),
                )?;
                let bytes = named_file(&setup, &session, &envelope, &capped)?;
                check(bytes == FILE_MAX - 1, || format!("capped at {bytes} bytes"))?;
            }

            // A write cut inside a character, then an error.
            setup
                .failpoints
                .arm("final_text.write.short", 1, "fail_io")
                .map_err(infra)?;
            {
                let _daemon = setup.start(evidence)?;
                let session = setup.spawn(evidence, "short")?;
                let envelope = setup.wait(evidence, &session)?;
                check(
                    envelope["state"] == "failed"
                        && envelope["failure"]["class"] == "store"
                        && envelope["final_text"].is_null()
                        && envelope["final_text_file"]["truncated"] == true,
                    || format!("short-write envelope: {envelope}"),
                )?;
                let bytes = named_file(&setup, &session, &envelope, &short)?;
                check(bytes > 0 && bytes < short.len(), || {
                    format!("the cut file holds {bytes} bytes")
                })?;
            }
            setup
                .failpoints
                .disarm("final_text.write.short")
                .map_err(infra)?;

            // The first write fails before any byte: the empty file is named.
            setup
                .failpoints
                .arm("final_text.write.fail", 1, "fail_io")
                .map_err(infra)?;
            {
                let _daemon = setup.start(evidence)?;
                let session = setup.spawn(evidence, "unwritten")?;
                let envelope = setup.wait(evidence, &session)?;
                check(
                    envelope["state"] == "failed"
                        && envelope["failure"]["class"] == "store"
                        && envelope["final_text"].is_null()
                        && envelope["final_text_file"]["truncated"] == true,
                    || format!("failed-write envelope: {envelope}"),
                )?;
                let bytes = named_file(&setup, &session, &envelope, &unwritten)?;
                check(bytes == 0, || {
                    format!("the unwritten file holds {bytes} bytes")
                })?;
            }
            setup
                .failpoints
                .disarm("final_text.write.fail")
                .map_err(infra)?;

            // The file's sync fails: the envelope names no file.
            setup
                .failpoints
                .arm("final_text.sync.fail", 1, "fail_io")
                .map_err(infra)?;
            {
                let _daemon = setup.start(evidence)?;
                let session = setup.spawn(evidence, "synced")?;
                let envelope = setup.wait(evidence, &session)?;
                check(
                    envelope["state"] == "failed"
                        && envelope["failure"]["class"] == "store"
                        && envelope["final_text"].is_null()
                        && envelope["final_text_file"].is_null(),
                    || format!("sync-failure envelope: {envelope}"),
                )?;
            }
            setup
                .failpoints
                .disarm("final_text.sync.fail")
                .map_err(infra)?;

            // Killed between the terminal commit and anything after it.
            setup
                .failpoints
                .arm("core.finish_running.pause", 1, "pause")
                .map_err(infra)?;
            let session = {
                let daemon = setup.start(evidence)?;
                // The child's own pid, which readiness confirmed serves the socket.
                let pid = daemon.pid();
                let session = setup.spawn(evidence, "crashed")?;
                setup
                    .failpoints
                    .wait_ack(
                        "core.finish_running.pause",
                        1,
                        "pause",
                        pid,
                        Duration::from_secs(20),
                    )
                    .map_err(failure)?;
                let pid = rustix::process::Pid::from_raw(i32::try_from(pid).map_err(infra)?)
                    .ok_or_else(|| infra("daemon pid 0"))?;
                rustix::process::kill_process(pid, rustix::process::Signal::KILL).map_err(infra)?;
                session
            };
            setup
                .failpoints
                .disarm("core.finish_running.pause")
                .map_err(infra)?;
            let _daemon = setup.start(evidence)?;
            let envelope = cli(
                &setup.sandbox,
                evidence,
                "result_crashed",
                &["result", &format!("{session}/1"), "--json"],
            )?;
            check(
                envelope["state"] == "completed"
                    && envelope["final_text_file"]["truncated"] == false,
                || {
                    format!(
                        "envelope after the restart: {}",
                        envelope["final_text_file"]
                    )
                },
            )?;
            let bytes = named_file(&setup, &session, &envelope, &crashed)?;
            check(bytes == crashed.len(), || {
                format!("{bytes} bytes after the restart")
            })
        },
        |evidence| collect(evidence, &setup.sandbox),
    );
    report.require_pass()
}

/// A raw `spawn` with `extra` members; its error object, or `None` for a
/// receipt.
fn spawn_error(raw: &mut Raw, id: u64, extra: &Value) -> Result<Option<Value>, ScenarioError> {
    let mut params = json!({"harness":"fake","model":"fake","prompt":"refused","handle":HANDLE});
    for (member, value) in extra.as_object().into_iter().flatten() {
        params[member] = value.clone();
    }
    let reply = raw.exchange(&request(id, "spawn", &params))?;
    Ok(reply.get("error").cloned())
}

/// A JSON value whose encoding is exactly `bytes` long: `{"pad":"…"}`.
fn object_of(bytes: usize) -> Value {
    json!({"pad":"p".repeat(bytes - r#"{"pad":""}"#.len())})
}

/// Design §6.4, §13.2 [t4r16.7.8]: every envelope member at its maximum,
/// with 1,500 denials and 1,500 declines whose targets are 64 KiB, encodes
/// within 1 MiB; each list keeps 1,000 entries of at most 256 bytes citing
/// their events, and each total is 1,500. At receipt a `bound` of
/// 32 KiB + 1, a `vendor` of 16 KiB + 1, and a `model` or `effort` of
/// 1 KiB + 1 encoded are `invalid_params` naming the member, while one
/// byte less passes the size check to the route's own rule; a vendor short
/// field of 1 KiB + 1 fails the turn `protocol`.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario keeps the maximal envelope and each receipt maximum"
)]
fn s1_bounds_envelope_at_every_member_maximum_fits_1_mib() -> TestResult {
    let maximal =
        via_core::envelope_at_maximum(1500, 1500, 64 * 1024).map_err(|error| error.message)?;
    assert!(
        maximal.len() <= 1024 * 1024,
        "maximal envelope is {} bytes",
        maximal.len()
    );
    let envelope: Value = serde_json::from_str(&maximal)?;
    for (list, total) in [
        ("denied_actions", "denied_actions_total"),
        ("auto_declined_requests", "auto_declined_requests_total"),
    ] {
        let entries = envelope[list].as_array().ok_or("no list")?;
        assert_eq!(entries.len(), 1000, "{list}");
        assert_eq!(envelope[total], 1500, "{total}");
        for (index, entry) in entries.iter().enumerate() {
            let encoded = serde_json::to_vec(entry)?.len();
            assert!(encoded <= 256, "{list}[{index}] is {encoded} bytes");
            assert_eq!(
                entry["event_seq"],
                index + 1,
                "{list}[{index}] cites its event"
            );
        }
    }
    let long_stop = "s".repeat(1025);
    let sandbox = Sandbox::new(&json!({"scripts":[
        script("short-field", "", &long_stop),
    ]}))?;
    let evidence = Evidence::new(
        "s1_bounds_envelope_maximum",
        &sandbox.fake,
        &sandbox.fixture,
    )?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let mut raw = Raw::open(&sandbox)?;
            let named = |error: &Option<Value>, field: &str| {
                error.as_ref().is_some_and(|error| {
                    error["data"]["kind"] == "invalid_params"
                        && error["data"]["field"] == field
                        && error["data"].get("route").is_none()
                })
            };
            for (id, (member, over, fits)) in [
                ("bound", object_of(32 * 1024 + 1), object_of(32 * 1024)),
                ("vendor", object_of(16 * 1024 + 1), object_of(16 * 1024)),
                (
                    "effort",
                    Value::String("e".repeat(1024 - 1)),
                    Value::String("e".repeat(1024 - 2)),
                ),
                (
                    "model",
                    Value::String("m".repeat(1024 - 1)),
                    Value::String("m".repeat(1024 - 2)),
                ),
            ]
            .into_iter()
            .enumerate()
            {
                let id = u64::try_from(id).map_err(infra)? * 2 + 1;
                let refused = spawn_error(&mut raw, id, &json!({member: over}))?;
                check(named(&refused, member), || {
                    format!("{member} of 1 byte over its maximum: {refused:?}")
                })?;
                let passed = spawn_error(&mut raw, id + 1, &json!({member: fits}))?;
                check(passed.is_some() && !named(&passed, member), || {
                    format!("{member} at its maximum was refused for its size: {passed:?}")
                })?;
            }
            let receipt = cli(
                &sandbox,
                evidence,
                "spawn_short_field",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "short-field",
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let session = receipt["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let envelope = cli(
                &sandbox,
                evidence,
                "wait_short_field",
                &[
                    "wait",
                    &format!("{session}/1"),
                    "--timeout-ms",
                    "30000",
                    "--json",
                ],
            )?;
            check(
                envelope["state"] == "failed" && envelope["failure"]["class"] == "protocol",
                || format!("a 1 KiB + 1 stop reason: {envelope}"),
            )
        },
        |evidence| collect(evidence, &sandbox),
    );
    report.require_pass()
}

/// Scenario cleanup: records every stored envelope and event as evidence,
/// then collects the Store and the evidence folders.
fn collect(evidence: &Evidence, sandbox: &Sandbox) -> Result<(), ScenarioError> {
    let path = sandbox.state.join("store.sqlite3");
    let (mut envelopes, mut events) = (String::new(), String::new());
    if path.is_file() {
        let store = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(infra)?;
        for (sql, out) in [
            (
                "SELECT envelope FROM turns WHERE envelope IS NOT NULL ORDER BY session_id,number",
                &mut envelopes,
            ),
            (
                "SELECT event FROM events ORDER BY session_id,seq",
                &mut events,
            ),
        ] {
            let mut statement = store.prepare(sql).map_err(infra)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(infra)?;
            for row in rows {
                out.push_str(&row.map_err(infra)?);
                out.push('\n');
            }
        }
    }
    evidence
        .write("envelopes.ndjson", envelopes.as_bytes())
        .map_err(infra)?;
    evidence
        .write("events.ndjson", events.as_bytes())
        .map_err(infra)?;
    collect_available(evidence, &sandbox.state)
}
