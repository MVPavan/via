//! Task 4 design §6.5 through the real `via` binary and daemon: an inline
//! `prompt` over `INLINE_MAX` (256 KiB) is written to a blob before
//! admission, adopted as `turns.prompt_blob`, and loaded at dispatch with
//! its SHA-256 and UTF-8 checks; a prompt of exactly 256 KiB stays inline.
//! A keyed retry of the blob spawn replays its receipt and leaves no second
//! blob. Written before the blob path.

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use daemon::{Daemon, Raw, Sandbox, TestResult, cli, events, failure, infra, request};
use scenario::{ScenarioError, collect_available, run_scenario};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const INLINE_MAX: usize = 256 * 1024;

/// A completing fake script selected by its start request's exact prompt:
/// the fake refuses any other bytes, so completion proves the prompt it got.
fn completes(prompt: &str) -> Value {
    json!({"expected_request":{"type":"start","id":1,"turn":1,"prompt":prompt},"steps":[
        {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
        {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"done","stop_reason":"end_turn"}},
    ]})
}

/// `len` bytes of prompt text with multi-byte characters, starting with
/// `tag` so each prompt selects its own script.
fn prompt(tag: char, len: usize) -> String {
    let mut text = String::with_capacity(len);
    text.push(tag);
    while text.len() + 3 <= len {
        text.push('é');
        text.push('x');
    }
    while text.len() < len {
        text.push('y');
    }
    text
}

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

/// The session's turn 1 `prompt` and `prompt_blob` columns.
fn stored(
    sandbox: &Sandbox,
    session: &str,
) -> Result<(Option<String>, Option<String>), ScenarioError> {
    let store = rusqlite::Connection::open_with_flags(
        sandbox.state.join("store.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(infra)?;
    store
        .query_row(
            "SELECT prompt,prompt_blob FROM turns WHERE session_id=?1 AND number=1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(infra)
}

/// The regular files in `<state>/blobs`.
fn blobs(sandbox: &Sandbox) -> Result<Vec<fs::DirEntry>, ScenarioError> {
    fs::read_dir(sandbox.state.join("blobs"))
        .map_err(infra)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(infra)
}

/// The large prompt is one 0600 blob holding its exact bytes, named with its
/// length and SHA-256 by `prompt_blob`; the 256 KiB prompt stayed inline.
fn stored_as_expected(
    sandbox: &Sandbox,
    (session, large): (&str, &str),
    (small_session, inline): (&str, &str),
) -> Result<(), ScenarioError> {
    let (column, blob) = stored(sandbox, session)?;
    check(column.is_none(), || {
        "the large prompt is stored inline".to_owned()
    })?;
    let blob = blob.ok_or_else(|| failure("the large prompt has no blob"))?;
    let files = blobs(sandbox)?;
    check(files.len() == 1, || {
        format!("{} blob files, expected exactly one", files.len())
    })?;
    let file = &files[0];
    let name = file.file_name().to_string_lossy().into_owned();
    check(blob.starts_with(name.trim_end_matches(".blob")), || {
        format!("prompt_blob {blob} does not name {name}")
    })?;
    let metadata = fs::symlink_metadata(file.path()).map_err(infra)?;
    check(
        metadata.is_file() && metadata.permissions().mode() & 0o777 == 0o600,
        || format!("blob mode {:o}", metadata.permissions().mode()),
    )?;
    let bytes = fs::read(file.path()).map_err(infra)?;
    check(bytes == large.as_bytes(), || {
        format!("blob holds {} bytes, not the prompt", bytes.len())
    })?;
    let digest = Sha256::digest(&bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    check(
        blob.ends_with(&format!(":{}:{digest}", large.len())),
        || format!("prompt_blob {blob} does not carry the length and SHA-256"),
    )?;
    let (column, blob) = stored(sandbox, small_session)?;
    check(column.as_deref() == Some(inline) && blob.is_none(), || {
        "a 256 KiB prompt did not stay inline".to_owned()
    })
}

#[test]
fn s1_blob_prompt_over_inline_max_is_a_verified_blob() -> TestResult {
    let large = prompt('L', INLINE_MAX + 1);
    let inline = prompt('I', INLINE_MAX);
    let sandbox = Sandbox::new(&json!({"scripts":[completes(&large), completes(&inline)]}))?;
    let evidence = Evidence::new("s1_blob_prompt", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let mut raw = Raw::open(&sandbox)?;
            let spawn_large = request(
                1,
                "spawn",
                &json!({"harness":"fake","model":"fake","prompt":large,"handle":HANDLE,"idempotency_key":"large"}),
            );
            let receipt = raw.exchange(&spawn_large)?;
            let session = receipt["result"]["session_id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| failure(format!("large spawn refused: {receipt}")))?;
            // A keyed retry replays the receipt; its own blob is discarded.
            let replay = raw.exchange(&spawn_large)?;
            check(replay["result"] == receipt["result"], || {
                format!("retry {replay} differs from {receipt}")
            })?;
            let small = raw.exchange(&request(
                2,
                "spawn",
                &json!({"harness":"fake","model":"fake","prompt":inline,"handle":HANDLE}),
            ))?;
            let small_session = small["result"]["session_id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| failure(format!("inline spawn refused: {small}")))?;
            let mut envelopes = String::new();
            let mut all = Vec::new();
            for (name, session) in [("large", &session), ("inline", &small_session)] {
                let envelope = cli(
                    &sandbox,
                    evidence,
                    &format!("wait_{name}"),
                    &[
                        "wait",
                        &format!("{session}/1"),
                        "--timeout-ms",
                        "30000",
                        "--json",
                    ],
                )?;
                envelopes.push_str(&envelope.to_string());
                envelopes.push('\n');
                all.extend(events(
                    &sandbox,
                    evidence,
                    &format!("events_{name}"),
                    session,
                )?);
                check(envelope["state"] == "completed", || {
                    format!("{name} turn: {envelope}")
                })?;
            }
            evidence
                .write("envelopes.ndjson", envelopes.as_bytes())
                .map_err(infra)?;
            evidence
                .write(
                    "events.ndjson",
                    serde_json::to_vec(&all).map_err(infra)?.as_slice(),
                )
                .map_err(infra)?;
            stored_as_expected(&sandbox, (&session, &large), (&small_session, &inline))
        },
        |evidence| collect_available(evidence, &sandbox.state),
    );
    report.require_pass()
}
