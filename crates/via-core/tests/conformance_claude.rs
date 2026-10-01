//! C2 conformance cases for the Claude Code adapter, red by construction
//! (`via-p98.3.1`; adapters design §6 step 6, §7).
//!
//! Each case pairs `crates/via-adapters/tests/fixtures/claude/<case>.replay.json`
//! (the recorded vendor side, replayed by `via-fake-agent`) with
//! `<case>.expect.json` (what the C2 driver must produce). [`drive`] is the
//! one seam: today it returns `adapter not implemented`, so every case is
//! ignored; `via-p98.3.2` replaces it with a driver over the real adapter and
//! removes the `ignore`s.
//!
//! The expectations use the unified adapter-level schema shared with the
//! other harnesses (`launches`, `plan_checks`, `sessions`, `turns[] {session,
//! start_after, params, tool_grace_ms, stop, steer, expect}`). This local
//! checker follows it until the shared `support/conformance_expect.rs`
//! lands, then the cases switch to that module. A field a case does not
//! state is not compared; `expect.unasserted` names deliberately skipped
//! fields, each with its reason; `notes` is never compared.
//!
//! # Fixture argv
//!
//! The argv is the adapter's own recipe (vendor packet §4) in this order:
//! `-p --input-format stream-json --output-format stream-json --verbose
//! --model M (--session-id {capture sid} | --resume {capture resume})
//! --restricted --strict-mcp-config --permission-mode dontAsk
//! --permission-prompts none --tools T --allowedTools T`, then
//! `--append-system-prompt`, `--effort` and `--json-schema` (compact, sorted
//! keys) when set. For a resume, the driver passes `sessions.main.resume`
//! and asserts the fake's captured `--resume` value equals it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The bead whose adapter makes these cases pass.
const ADAPTER_BEAD: &str = "via-p98.3.2";

/// C2 §4 observation kinds an expectation may name.
const OBSERVATIONS: [&str; 10] = [
    "session.vendor_identity_confirmed",
    "turn.accepted",
    "turn.late_terminal",
    "session.vendor_closed",
    "resume.mismatch",
    "progress",
    "final_text",
    "action.denied",
    "vendor.request_declined",
    "steer.delivered",
];
/// The `expect` fields of the unified schema.
const EXPECT_FIELDS: [&str; 17] = [
    "plan_refusal",
    "rejected",
    "accepted",
    "terminal",
    "usage",
    "final_text",
    "error",
    "cleanup",
    "cleanup_settles",
    "stop_facts",
    "instance",
    "observations_include",
    "observations_exclude",
    "observation_counts",
    "observations_order",
    "unasserted",
    "notes",
];
/// Fields compared by their own rules rather than by value.
const SPECIAL_FIELDS: [&str; 6] = [
    "observations_include",
    "observations_exclude",
    "observation_counts",
    "observations_order",
    "unasserted",
    "notes",
];
const TERMINAL_FIELDS: [&str; 10] = [
    "status",
    "stop_reason",
    "vendor_stop_reason",
    "vendor_code",
    "class_hint",
    "detail",
    "structured_output",
    "steps",
    "cost",
    "usage",
];

/// Returns an error unless `$cond` holds.
macro_rules! ensure {
    ($cond:expr, $($message:tt)+) => {
        if !$cond {
            return Err(format!($($message)+));
        }
    };
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../via-adapters/tests/fixtures/claude")
}

/// One case: its expectation and the path of its replay fixture.
struct Case {
    expect: Value,
    fixture: PathBuf,
}

fn load(name: &str) -> Result<Case, String> {
    let dir = fixtures_dir();
    let path = dir.join(format!("{name}.expect.json"));
    let text = fs::read_to_string(&path).map_err(|error| format!("{name}: {error}"))?;
    let expect = serde_json::from_str(&text).map_err(|error| format!("{name}: {error}"))?;
    Ok(Case {
        expect,
        fixture: dir.join(format!("{name}.replay.json")),
    })
}

/// What the driver observed, in the expectation schema's shape.
struct Outcome {
    /// Vendor process launches over the whole case.
    launches: u64,
    /// One refusal kind (or none) per `plan_checks` entry.
    plan_checks: Vec<Option<String>>,
    /// Per session label, its close report (`{mode, vendor_closed,
    /// cleanup}`) or null.
    closes: serde_json::Map<String, Value>,
    /// One object per turn with the `expect` fields, plus `observations`:
    /// the observation kinds in the order the driver decoded them.
    turns: Vec<Value>,
}

/// Runs the case through the Claude adapter with the fake replaying its
/// fixture. Replaced by `via-p98.3.2`.
fn drive(case: &Case) -> Result<Outcome, String> {
    let _ = (&case.expect, &case.fixture);
    Err(format!("adapter not implemented: {ADAPTER_BEAD}"))
}

/// Whether `actual` has every member of `expected` (objects as subsets,
/// everything else equal).
fn matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|actual| matches(value, actual))),
        (expected, actual) => expected == actual,
    }
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn unasserted(expect: &Value) -> Vec<&str> {
    expect["unasserted"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["field"].as_str())
        .collect()
}

/// Whether `wanted` occurs in `seen` in order (not necessarily adjacent).
fn subsequence(wanted: &[&str], seen: &[&str]) -> bool {
    let mut seen = seen.iter();
    wanted.iter().all(|kind| seen.any(|item| item == kind))
}

fn compare_turn(expect: &Value, actual: &Value) -> Result<(), String> {
    let skipped = unasserted(expect);
    for (field, expected) in expect.as_object().ok_or("expect is not an object")? {
        if SPECIAL_FIELDS.contains(&field.as_str()) || skipped.contains(&field.as_str()) {
            continue;
        }
        let actual = actual.get(field).unwrap_or(&Value::Null);
        ensure!(
            matches(expected, actual),
            "{field}: expected {expected}, got {actual}"
        );
    }
    let seen = strings(&actual["observations"]);
    for kind in strings(&expect["observations_include"]) {
        ensure!(
            seen.contains(&kind),
            "missing observation {kind}; saw {seen:?}"
        );
    }
    for kind in strings(&expect["observations_exclude"]) {
        ensure!(!seen.contains(&kind), "unexpected observation {kind}");
    }
    for (kind, count) in expect["observation_counts"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let got = seen.iter().filter(|seen| **seen == kind).count() as u64;
        ensure!(
            Some(got) == count.as_u64(),
            "{kind}: expected {count}, got {got}"
        );
    }
    let order = strings(&expect["observations_order"]);
    ensure!(
        subsequence(&order, &seen),
        "observations {seen:?} lack the order {order:?}"
    );
    Ok(())
}

fn run_case(name: &str) -> Result<(), String> {
    let case = load(name)?;
    let doc = &case.expect;
    let outcome = drive(&case).map_err(|error| format!("{name}: {error}"))?;
    if let Some(launches) = doc["launches"].as_u64() {
        ensure!(
            launches == outcome.launches,
            "{name}: {launches} launches, got {}",
            outcome.launches
        );
    }
    let checks = doc["plan_checks"].as_array().cloned().unwrap_or_default();
    ensure!(
        checks.len() == outcome.plan_checks.len(),
        "{name}: {} plan checks, got {}",
        checks.len(),
        outcome.plan_checks.len()
    );
    for (check, got) in checks.iter().zip(&outcome.plan_checks) {
        let want = check["refusal"].as_str();
        ensure!(
            want == got.as_deref(),
            "{name}: plan check {check} gave {got:?}"
        );
    }
    for (label, session) in doc["sessions"].as_object().into_iter().flatten() {
        let close = &session["close"];
        if !close.is_null() {
            let got = outcome.closes.get(label).unwrap_or(&Value::Null);
            ensure!(matches(close, got), "{name}: close of {label}: {got}");
        }
    }
    let turns = doc["turns"].as_array().cloned().unwrap_or_default();
    ensure!(
        turns.len() == outcome.turns.len(),
        "{name}: {} turns, got {}",
        turns.len(),
        outcome.turns.len()
    );
    for (index, (turn, actual)) in turns.iter().zip(&outcome.turns).enumerate() {
        compare_turn(&turn["expect"], actual)
            .map_err(|error| format!("{name} turn {}: {error}", index + 1))?;
    }
    Ok(())
}

macro_rules! cases {
    ($($test:ident => $name:literal,)*) => {
        $(
            #[test]
            #[ignore = "red until via-p98.3.2"]
            fn $test() -> Result<(), String> {
                run_case($name)
            }
        )*
        /// Every case with a test.
        const CASES: &[&str] = &[$($name),*];
    };
}

cases! {
    conformance_claude_c0_isolated => "c0_isolated",
    conformance_claude_c0_bad_model => "c0_bad_model",
    conformance_claude_c0_bad_effort => "c0_bad_effort",
    conformance_claude_c0_invalid_resume => "c0_invalid_resume",
    conformance_claude_c1a => "c1a",
    conformance_claude_c1b => "c1b",
    conformance_claude_c1c => "c1c",
    conformance_claude_c1b_resume_mismatch => "c1b_resume_mismatch",
    conformance_claude_c3_queue => "c3_queue",
    conformance_claude_c4_never_ask => "c4_never_ask",
    conformance_claude_c5_read_only => "c5_read_only",
    conformance_claude_c7_interrupt => "c7_interrupt",
    conformance_claude_c9a => "c9a",
    conformance_claude_c9b => "c9b",
    conformance_claude_c10_early_eof => "c10_early_eof",
    conformance_claude_c11b_stdio_prompt => "c11b_stdio_prompt",
}

const REFUSALS: [&str; 8] = [
    "unsupported_verb",
    "bound_unsupported",
    "harness_unavailable",
    "unknown_model",
    "version_refused",
    "vendor_option_conflict",
    "invalid_param",
    "missing_capability",
];
const REJECTIONS: [&str; 5] = [
    "bound_unsupported",
    "invalid_param",
    "vendor_error",
    "session_gone",
    "protocol",
];
const ERRORS: [&str; 8] = [
    "deadline",
    "force_stop",
    "overflow",
    "protocol",
    "process_exit",
    "unknown_submission",
    "server_lost",
    "transport_lost",
];
const STATUSES: [&str; 3] = ["completed", "interrupted", "failed"];
const STOP_REASONS: [&str; 7] = [
    "end_turn",
    "max_steps",
    "budget",
    "refusal",
    "interrupted",
    "error",
    "other",
];
const CLASS_HINTS: [&str; 7] = [
    "auth",
    "rate_limit",
    "context_exceeded",
    "budget_exceeded",
    "vendor_error",
    "protocol",
    "resume_mismatch",
];

/// Whether `value` is null or one of `allowed`, optionally `kind:detail`.
fn kind_ok(value: &Value, allowed: &[&str]) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => allowed.iter().any(|kind| {
            text == kind
                || text
                    .strip_prefix(kind)
                    .is_some_and(|rest| rest.starts_with(':'))
        }),
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => false,
    }
}

fn check_observations(name: &str, expect: &Value) -> Result<(), String> {
    let counts = expect["observation_counts"].as_object();
    let mut kinds = strings(&expect["observations_include"]);
    kinds.extend(strings(&expect["observations_exclude"]));
    kinds.extend(strings(&expect["observations_order"]));
    kinds.extend(counts.into_iter().flatten().map(|(kind, _)| kind.as_str()));
    for kind in kinds {
        ensure!(
            OBSERVATIONS.contains(&kind),
            "{name}: unknown observation {kind}"
        );
    }
    if expect["accepted"] == true {
        let accepted = counts.and_then(|counts| counts.get("turn.accepted"));
        ensure!(
            accepted.and_then(Value::as_u64) == Some(1),
            "{name}: an accepted turn must count turn.accepted == 1"
        );
        let confirms =
            strings(&expect["observations_include"]).contains(&"session.vendor_identity_confirmed");
        let order = strings(&expect["observations_order"]);
        ensure!(
            !confirms
                || subsequence(
                    &["session.vendor_identity_confirmed", "turn.accepted"],
                    &order
                ),
            "{name}: identity confirmation must precede acceptance in observations_order"
        );
    }
    for entry in expect["unasserted"].as_array().into_iter().flatten() {
        ensure!(
            entry["field"].is_string() && entry["why"].as_str().is_some_and(|why| !why.is_empty()),
            "{name}: unasserted entry {entry} needs a field and a why"
        );
    }
    Ok(())
}

fn check_expect(name: &str, expect: &Value) -> Result<(), String> {
    for field in expect.as_object().ok_or("expect")?.keys() {
        ensure!(
            EXPECT_FIELDS.contains(&field.as_str()),
            "{name}: unknown expect field {field}"
        );
    }
    for (field, allowed) in [
        ("plan_refusal", &REFUSALS[..]),
        ("rejected", &REJECTIONS[..]),
        ("error", &ERRORS[..]),
        ("cleanup", &["quiescent", "uncertain"][..]),
        (
            "cleanup_settles",
            &["at_terminal", "at_p7_bound", "when_tools_end"][..],
        ),
    ] {
        ensure!(
            kind_ok(&expect[field], allowed),
            "{name}: {field} {}",
            expect[field]
        );
    }
    let terminal = &expect["terminal"];
    for field in terminal
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, _)| key)
    {
        ensure!(
            TERMINAL_FIELDS.contains(&field.as_str()),
            "{name}: unknown terminal field {field}"
        );
    }
    if !terminal.is_null() {
        for (field, allowed) in [
            ("status", &STATUSES[..]),
            ("stop_reason", &STOP_REASONS[..]),
            ("class_hint", &CLASS_HINTS[..]),
        ] {
            ensure!(
                kind_ok(&terminal[field], allowed),
                "{name}: terminal {field}"
            );
        }
    }
    let usage = &expect["usage"];
    if !usage.is_null() {
        ensure!(
            kind_ok(&usage["from"], &["terminal", "samples"]) && usage["from"].is_string(),
            "{name}: usage.from"
        );
    }
    ensure!(
        expect["final_text"].is_null() || expect["final_text"].is_array(),
        "{name}: final_text is ordered pieces or null"
    );
    check_observations(name, expect)?;
    ensure!(
        expect["notes"]
            .as_str()
            .is_some_and(|notes| !notes.is_empty()),
        "{name}: no notes"
    );
    Ok(())
}

fn check_turn(name: &str, sessions: &Value, turn: &Value) -> Result<(), String> {
    let label = turn["session"].as_str().ok_or("turn session")?;
    ensure!(
        !sessions[label].is_null(),
        "{name}: unknown session {label}"
    );
    ensure!(turn["params"]["prompt"].is_string(), "{name}: no prompt");
    ensure!(turn["steer"].is_array(), "{name}: steer is a list");
    let stop = &turn["stop"];
    if !stop.is_null() {
        ensure!(
            kind_ok(&stop["kind"], &["interrupt", "wall", "close"]),
            "{name}: {stop}"
        );
        ensure!(
            kind_ok(&stop["after"], &["accepted", "tool_started", "handshake"]),
            "{name}: {stop}"
        );
    }
    check_expect(name, &turn["expect"])
}

fn check_case(name: &str) -> Result<(), String> {
    let case = load(name)?;
    ensure!(case.fixture.is_file(), "{name}: no replay fixture");
    let doc = &case.expect;
    ensure!(doc["harness"] == "claude", "{name}: harness");
    ensure!(
        doc["source"]
            .as_str()
            .is_some_and(|source| !source.is_empty()),
        "{name}: no source"
    );
    ensure!(doc["launches"].is_u64(), "{name}: launches");
    for check in doc["plan_checks"].as_array().into_iter().flatten() {
        ensure!(check["require"].is_string(), "{name}: {check}");
        ensure!(kind_ok(&check["refusal"], &REFUSALS), "{name}: {check}");
    }
    let sessions = &doc["sessions"];
    for (label, session) in sessions.as_object().ok_or("sessions")? {
        ensure!(session["model"].is_string(), "{name}: {label} has no model");
    }
    let turns = doc["turns"].as_array().ok_or("turns")?;
    ensure!(!turns.is_empty(), "{name}: no turns");
    turns
        .iter()
        .try_for_each(|turn| check_turn(name, sessions, turn))
}

/// Green now: every expectation has a test and a fixture, and uses only the
/// unified schema's fields and C2's names.
#[test]
fn conformance_claude_expectations_are_well_formed() -> Result<(), String> {
    let mut files = BTreeSet::new();
    for entry in fs::read_dir(fixtures_dir()).map_err(|error| error.to_string())? {
        let name = entry.map_err(|error| error.to_string())?.file_name();
        if let Some(case) = name.to_string_lossy().strip_suffix(".expect.json") {
            files.insert(case.to_owned());
        }
    }
    let cases: BTreeSet<String> = CASES.iter().map(|case| (*case).to_owned()).collect();
    ensure!(
        files == cases,
        "expect files {files:?} differ from tests {cases:?}"
    );
    CASES.iter().try_for_each(|name| check_case(name))
}
