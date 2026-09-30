//! Task 4 design §4.3 and §4.5 (A25, A38) through the real `via` binary and
//! daemon: `events` pages a session or one turn with no follow stream, and
//! `list` pages sessions in creation order with `last_active_at`. Written
//! before the `events` page and `list`.

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::collections::HashSet;

use daemon::{Daemon, Raw, Sandbox, TestResult, cli, failure, infra, refused, request};
use scenario::{ScenarioError, collect_available, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

/// A script for `prompt` as turn `turn`: accepted, then completed.
fn completes(prompt: &str, turn: u32) -> Value {
    let vendor = format!("fake-turn-{turn}");
    json!({"expected_request":{"type":"start","id":1,"turn":turn,"prompt":prompt},"steps":[
        {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":vendor}},
        {"action":"emit","message":{"type":"terminal","vendor_turn_id":vendor,
            "status":"completed","final_text":"done","stop_reason":"end_turn"}},
    ]})
}

fn spawn(
    sandbox: &Sandbox,
    evidence: &Evidence,
    prompt: &str,
    label: Option<&str>,
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
    if let Some(label) = label {
        args.extend(["--label", label]);
    }
    let receipt = cli(sandbox, evidence, &format!("spawn_{prompt}"), &args)?;
    receipt["session_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| failure(format!("receipt has no session: {receipt}")))
}

fn wait(sandbox: &Sandbox, evidence: &Evidence, address: &str) -> Result<Value, ScenarioError> {
    cli(
        sandbox,
        evidence,
        &format!("wait_{}", address.replace('/', "_")),
        &["wait", address, "--timeout-ms", "30000", "--json"],
    )
}

fn resume(
    sandbox: &Sandbox,
    evidence: &Evidence,
    session: &str,
    prompt: &str,
) -> Result<(), ScenarioError> {
    cli(
        sandbox,
        evidence,
        &format!("resume_{prompt}"),
        &[
            "resume", session, "--prompt", prompt, "--handle", HANDLE, "--json",
        ],
    )
    .map(drop)
}

fn seqs(page: &Value) -> Vec<u64> {
    page["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(|event| event["seq"].as_u64())
                .collect()
        })
        .unwrap_or_default()
}

/// Design §4.3, §13.2 (A25): `follow: true` is an unknown member and so
/// `invalid_params`, and `unsubscribe` is `method_not_found`. The page
/// addresses exactly one of a session and a turn: a turn address pages
/// that turn's events only, `types` filters them, `after` and `limit` move
/// the window, `earliest_seq` is 1 and no event carries `raw_ref`; an
/// unknown type and a `limit` out of 1 to 1000 are refused.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario keeps every refused shape and each page read on one daemon"
)]
fn s1_c1_follow_and_unsubscribe_are_refused() -> TestResult {
    let sandbox =
        Sandbox::new(&json!({"scripts":[completes("first", 1), completes("second", 2)]}))?;
    let evidence = Evidence::new("s1_c1_follow_refused", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let session = spawn(&sandbox, evidence, "first", None)?;
            wait(&sandbox, evidence, &format!("{session}/1"))?;
            resume(&sandbox, evidence, &session, "second")?;
            wait(&sandbox, evidence, &format!("{session}/2"))?;

            let mut raw = Raw::open(&sandbox)?;
            let followed = raw.exchange(&request(
                1,
                "events",
                &json!({"session":session,"follow":true}),
            ))?;
            check(
                followed["error"]["data"]["kind"] == "invalid_params",
                || format!("follow: {followed}"),
            )?;
            let unsubscribed =
                raw.exchange(&request(2, "unsubscribe", &json!({"session":session})))?;
            check(
                unsubscribed["error"]["data"]["kind"] == "method_not_found",
                || format!("unsubscribe: {unsubscribed}"),
            )?;
            for (id, params) in [
                (3, json!({"session":session,"turn":format!("{session}/1")})),
                (4, json!({})),
                (5, json!({"session":session,"types":["assistant.text"]})),
                (6, json!({"session":session,"limit":0})),
                (7, json!({"session":session,"limit":1001})),
                (8, json!({"turn":session})),
            ] {
                let reply = raw.exchange(&request(id, "events", &params))?;
                check(reply["error"]["data"]["kind"] == "invalid_params", || {
                    format!("events {params}: {reply}")
                })?;
            }

            let all = cli(
                &sandbox,
                evidence,
                "events_all",
                &["events", &session, "--json"],
            )?;
            let events = all["events"].as_array().cloned().unwrap_or_default();
            check(
                all["earliest_seq"] == 1
                    && all["more"] == false
                    && seqs(&all)
                        == (1..=u64::try_from(events.len()).map_err(infra)?).collect::<Vec<_>>()
                    && all["next_after"] == events.len()
                    && events.iter().all(|event| event.get("raw_ref").is_none()),
                || format!("session page: {all}"),
            )?;
            let second = cli(
                &sandbox,
                evidence,
                "events_turn_2",
                &["events", &format!("{session}/2"), "--json"],
            )?;
            let of_second = second["events"].as_array().cloned().unwrap_or_default();
            check(
                !of_second.is_empty() && of_second.iter().all(|event| event["turn"] == 2),
                || format!("turn 2 page: {second}"),
            )?;
            let ended = cli(
                &sandbox,
                evidence,
                "events_ended",
                &[
                    "events",
                    &session,
                    "--types",
                    "turn.ended,turn.queued",
                    "--json",
                ],
            )?;
            let kinds: Vec<&str> = ended["events"]
                .as_array()
                .map(|events| {
                    events
                        .iter()
                        .filter_map(|event| event["type"].as_str())
                        .collect()
                })
                .unwrap_or_default();
            check(
                kinds == ["turn.queued", "turn.ended", "turn.queued", "turn.ended"],
                || format!("typed page: {ended}"),
            )?;
            let window = cli(
                &sandbox,
                evidence,
                "events_window",
                &["events", &session, "--after", "2", "--limit", "2", "--json"],
            )?;
            check(
                seqs(&window) == [3, 4] && window["next_after"] == 4 && window["more"] == true,
                || format!("window page: {window}"),
            )
        },
        |evidence| collect(evidence, &sandbox),
    );
    report.require_pass()
}

/// Every session a `via list` paging with `args` returns, in order.
fn list_all(
    sandbox: &Sandbox,
    evidence: &Evidence,
    name: &str,
    args: &[&str],
    between: &mut dyn FnMut(usize) -> Result<(), ScenarioError>,
) -> Result<Vec<Value>, ScenarioError> {
    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0.. {
        let mut call = vec!["list", "--json"];
        call.extend_from_slice(args);
        if let Some(cursor) = &cursor {
            call.extend(["--cursor", cursor.as_str()]);
        }
        let listed = cli(sandbox, evidence, &format!("{name}_{page}"), &call)?;
        sessions.extend(listed["sessions"].as_array().cloned().unwrap_or_default());
        match listed["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
        between(page)?;
    }
    Ok(sessions)
}

fn ids(sessions: &[Value]) -> Vec<String> {
    sessions
        .iter()
        .filter_map(|summary| summary["session_id"].as_str().map(str::to_owned))
        .collect()
}

/// Design §4.5, §6.8, §13.2 [t4r16.4] (A38): sessions page newest first
/// with no repeats while states change, and one created mid-scan never
/// appears; each summary has exactly `{session_id, state, admission,
/// harness, model, label, created_at, last_active_at}`, `last_active_at`
/// the time of the session's latest durable event, which `since` filters
/// on; `label` and `state` filter; a cursor of another version (`l2.`) or
/// malformed is `invalid_params`. The paging bound (1000 sessions examined
/// per page) is `s1_c1_list_page_examines_at_most_1000_sessions_each_once`
/// at the Store level.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario keeps the paged walk, the filters and the refused cursors on one daemon"
)]
fn s1_c1_list_creation_order_and_last_active() -> TestResult {
    const SESSIONS: usize = 12;
    let mut scripts: Vec<Value> = (0..SESSIONS)
        .map(|index| completes(&format!("p{index}"), 1))
        .collect();
    scripts.push(completes("again", 2));
    scripts.push(completes("late", 1));
    let sandbox = Sandbox::new(&json!({ "scripts": scripts }))?;
    let evidence = Evidence::new("s1_c1_list_order", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = Daemon::start(&sandbox, evidence)?;
            let mut created = Vec::new();
            for index in 0..SESSIONS {
                let label = (index == 3).then_some("needle");
                let session = spawn(&sandbox, evidence, &format!("p{index}"), label)?;
                wait(&sandbox, evidence, &format!("{session}/1"))?;
                created.push(session);
            }
            let newest_first: Vec<String> = created.iter().rev().cloned().collect();

            let mut changed = false;
            let mut late = None;
            let listed = list_all(&sandbox, evidence, "list", &["--limit", "5"], &mut |page| {
                if page == 0 {
                    // A state changes and a session is created mid-scan.
                    resume(&sandbox, evidence, &created[0], "again")?;
                    late = Some(spawn(&sandbox, evidence, "late", None)?);
                    changed = true;
                }
                Ok(())
            })?;
            check(changed && ids(&listed) == newest_first, || {
                format!("listed {:?}, created {newest_first:?}", ids(&listed))
            })?;
            let unique: HashSet<String> = ids(&listed).into_iter().collect();
            check(unique.len() == SESSIONS, || "a session repeated".to_owned())?;
            let late = late.ok_or_else(|| failure("no late session"))?;
            wait(&sandbox, evidence, &format!("{late}/1"))?;
            wait(&sandbox, evidence, &format!("{}/2", created[0]))?;

            // Every member, and the latest durable event's time.
            let fresh = list_all(&sandbox, evidence, "fresh", &[], &mut |_| Ok(()))?;
            check(
                ids(&fresh).first() == Some(&late) && fresh.len() == SESSIONS + 1,
                || format!("the late session is not newest: {:?}", ids(&fresh)),
            )?;
            for summary in &fresh {
                let mut keys: Vec<&str> = summary
                    .as_object()
                    .map(|object| object.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                keys.sort_unstable();
                check(
                    keys == [
                        "admission",
                        "created_at",
                        "harness",
                        "label",
                        "last_active_at",
                        "model",
                        "session_id",
                        "state",
                    ] && summary["harness"] == "fake"
                        && summary["model"] == "fake"
                        && summary["admission"] == "open"
                        && summary["state"] == "idle",
                    || format!("summary {summary}"),
                )?;
                let session = summary["session_id"].as_str().unwrap_or_default();
                let page = cli(
                    &sandbox,
                    evidence,
                    &format!("events_{session}"),
                    &["events", session, "--json"],
                )?;
                let last = page["events"]
                    .as_array()
                    .and_then(|events| events.last())
                    .cloned();
                check(
                    last.is_some_and(|event| event["at"] == summary["last_active_at"]),
                    || {
                        format!(
                            "{session}: last_active_at {} is not its latest event's",
                            summary["last_active_at"]
                        )
                    },
                )?;
            }
            let resumed = fresh
                .iter()
                .find(|summary| summary["session_id"] == created[0].as_str())
                .ok_or_else(|| failure("the resumed session is missing"))?;
            let since = resumed["last_active_at"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let recent = list_all(
                &sandbox,
                evidence,
                "since",
                &["--since", &since],
                &mut |_| Ok(()),
            )?;
            let expected: Vec<String> = fresh
                .iter()
                .filter(|summary| summary["last_active_at"].as_str() >= Some(since.as_str()))
                .filter_map(|summary| summary["session_id"].as_str().map(str::to_owned))
                .collect();
            check(
                ids(&recent) == expected && expected.contains(&created[0]),
                || format!("since {since}: {:?}, expected {expected:?}", ids(&recent)),
            )?;
            let needle = list_all(
                &sandbox,
                evidence,
                "needle",
                &["--label", "needle"],
                &mut |_| Ok(()),
            )?;
            check(
                ids(&needle) == [created[3].clone()] && needle[0]["label"] == "needle",
                || format!("label filter: {needle:?}"),
            )?;
            let active = list_all(
                &sandbox,
                evidence,
                "active",
                &["--state", "active"],
                &mut |_| Ok(()),
            )?;
            check(active.is_empty(), || format!("state filter: {active:?}"))?;
            for (name, cursor) in [
                ("l2", "l2.5"),
                ("empty", "l3."),
                ("sign", "l3.-1"),
                ("word", "l3.x"),
            ] {
                refused(
                    &sandbox,
                    evidence,
                    &format!("cursor_{name}"),
                    &["list", "--cursor", cursor, "--json"],
                    "invalid_params",
                )?;
            }
            Ok(())
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
