//! Harness-neutral checker for adapter conformance expectations.
//!
//! An expectation file (`<case>.expect.json` beside a replay fixture) states
//! what a C2 driver must produce, in the unified schema of the adapter
//! fixture slices: top-level `launches` and `plan_checks`, `sessions` (with
//! their close results) and `turns` (with their `expect` blocks). A harness's
//! `drive()` collects an [`Outcome`] from the real driver; [`check`] compares
//! every stated field. A field the case does not state is not compared;
//! `expect.unasserted` lists fields deliberately skipped, each with a reason.
//!
//! `drive()` obligations the checker relies on:
//! - `launches` counts vendor process starts over the whole case, read from
//!   the fixture's launch log `<name>.launches` beside the replay (one line
//!   per start of the replaying fake, `--version` probes included), never
//!   from the driver's own count; each case starts with no log;
//! - the replay's own verdict is part of the case: a fake that exits 3
//!   (unmatched or unexpected input, an absent field that was sent, a line
//!   that came late, stdin closed early) fails the case whatever the
//!   outcome. A replay that ends with `await_eof` requires the case to end
//!   the way the route ends its vendor input (Codex: idle retirement closes
//!   the server's stdin after the last session closed; a per-turn process:
//!   EOF after the result), and no line after the step before it;
//! - `cleanup_settles` is measured with controlled time: `at_terminal` when
//!   the turn settled with its terminal, `at_p7_bound` when it was still
//!   pending just before `min(ack + tool_grace, wall)` and settled at it,
//!   `when_tools_end` when the last reported tool ended first;
//! - `observations` lists the kinds the driver emitted for the turn, in
//!   order (`turn.accepted`, `final_text`, …).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};

/// What a driver produced for a whole case.
pub(crate) struct Outcome {
    /// Vendor process launches over the case: the lines of the fixture's
    /// `<name>.launches` log.
    pub(crate) launches: u64,
    /// The refusal kind of each `plan_checks` entry, in order; `None` when
    /// the check passed.
    pub(crate) plan_checks: Vec<Option<String>>,
    /// Each session's close report (`vendor_closed`, `cleanup`), or `None`
    /// when the case did not close it.
    pub(crate) closes: BTreeMap<String, Option<Value>>,
    /// One entry per expected turn, in the expectation's order.
    pub(crate) turns: Vec<TurnOutcome>,
}

/// What a driver produced for one turn, in the schema's vocabulary.
#[derive(Default)]
pub(crate) struct TurnOutcome {
    pub(crate) plan_refusal: Option<String>,
    pub(crate) rejected: Option<String>,
    pub(crate) accepted: bool,
    /// The retained vendor terminal: `status`, `stop_reason`, `class_hint`, …
    pub(crate) terminal: Option<Value>,
    /// The turn's usage with `from` (`terminal` or `samples`) and `scope`.
    pub(crate) usage: Option<Value>,
    pub(crate) final_text: Option<Vec<String>>,
    pub(crate) error: Option<String>,
    pub(crate) cleanup: Option<String>,
    pub(crate) cleanup_settles: Option<String>,
    pub(crate) stop_facts: Option<Value>,
    pub(crate) instance: Option<Value>,
    /// The result kind of each steer attempt, in the turn's `steer` order.
    pub(crate) steer: Vec<String>,
    /// Observation kinds emitted for the turn, in order.
    pub(crate) observations: Vec<String>,
}

const TOP: &[&str] = &[
    "source",
    "harness",
    "launches",
    "plan_checks",
    "sessions",
    "turns",
];
const SESSION: &[&str] = &["model", "instructions", "cwd", "resume", "close"];
const CLOSE: &[&str] = &["mode", "vendor_closed", "cleanup"];
const TURN: &[&str] = &[
    "session",
    "start_after",
    "params",
    "tool_grace_ms",
    "stop",
    "steer",
    "expect",
];
/// Turn `expect` fields compared by (subset) value.
const VALUED: &[&str] = &[
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
];
const EXPECT_OTHER: &[&str] = &[
    "observations_include",
    "observations_exclude",
    "observation_counts",
    "observations_order",
    "unasserted",
    "notes",
];
const PLAN_CHECK: &[&str] = &["require", "refusal"];
const START_AFTER: &[&str] = &["turn", "event"];
const PARAMS: &[&str] = &["prompt", "effort", "bound", "output_schema", "max_steps"];
const STOP: &[&str] = &["kind", "after"];
const STEER: &[&str] = &["after", "text", "expected_vendor_turn", "result"];
const UNASSERTED: &[&str] = &["field", "why"];
const TERMINAL: &[&str] = &[
    "status",
    "stop_reason",
    "vendor_stop_reason",
    "vendor_code",
    "class_hint",
    "detail",
    "structured_output",
    "steps",
    "cost",
];
const COST: &[&str] = &["usd", "scope", "provenance"];
/// `from`, the C1 scope and provenance, and the C1 token names.
const USAGE: &[&str] = &[
    "from",
    "scope",
    "provenance",
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "total_tokens",
];
const STOP_FACTS: &[&str] = &["acknowledged", "forced", "shared"];
const INSTANCE: &[&str] = &["vendor_version", "version_status"];
/// Observation fields that are lists of kinds.
const KIND_LISTS: &[&str] = &[
    "observations_include",
    "observations_exclude",
    "observations_order",
];

/// Reads `<dir>/<name>.expect.json`.
pub(crate) fn load(dir: &Path, name: &str) -> Result<Value, String> {
    let text = fs::read_to_string(dir.join(format!("{name}.expect.json")))
        .map_err(|error| format!("{name}.expect.json: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("{name}.expect.json: {error}"))
}

/// The case names of every `*.expect.json` in `dir`, sorted.
pub(crate) fn case_names(dir: &Path) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let path = entry.map_err(|error| error.to_string())?.path();
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if let Some(name) = file.strip_suffix(".expect.json") {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn object<'a>(value: &'a Value, at: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{at}: not an object"))
}

fn known(value: &Value, keys: &[&str], at: &str) -> Result<(), String> {
    for key in object(value, at)?.keys() {
        if !keys.contains(&key.as_str()) {
            return Err(format!("{at}: unknown field {key}"));
        }
    }
    Ok(())
}

/// [`known`] for an optional object: absent or null passes.
fn known_if(value: Option<&Value>, keys: &[&str], at: &str) -> Result<(), String> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(value) => known(value, keys, at),
    }
}

/// [`known`] for each object of an optional array: absent passes.
fn known_entries(value: Option<&Value>, keys: &[&str], at: &str) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let entries = value
        .as_array()
        .ok_or_else(|| format!("{at}: not an array"))?;
    for (index, entry) in entries.iter().enumerate() {
        known(entry, keys, &format!("{at}[{index}]"))?;
    }
    Ok(())
}

/// Refuses an observation field of the wrong type instead of reading it
/// as empty: kind lists are arrays of strings, counts an object of
/// non-negative integers. Absent fields pass.
fn observation_types(fields: &Map<String, Value>, at: &str) -> Result<(), String> {
    for key in KIND_LISTS {
        if let Some(value) = fields.get(*key)
            && !value
                .as_array()
                .is_some_and(|kinds| kinds.iter().all(Value::is_string))
        {
            return Err(format!("{at}.{key}: not an array of strings"));
        }
    }
    if let Some(counts) = fields.get("observation_counts") {
        let counts = counts
            .as_object()
            .ok_or_else(|| format!("{at}.observation_counts: not an object"))?;
        for (kind, count) in counts {
            if count.as_u64().is_none() {
                return Err(format!(
                    "{at}.observation_counts.{kind}: not a non-negative integer"
                ));
            }
        }
    }
    Ok(())
}

/// Checks the expectation's well-formedness for every harness: known fields
/// at every level (usage only under `expect.usage`, never the retired
/// `terminal.usage`), required parts present, observation fields of their
/// types, session labels and `start_after` turns resolvable, a non-empty
/// reason for each `unasserted` entry, C2 names for enumerated values,
/// `turn.accepted` counted exactly once on every accepted turn, and identity
/// confirmation ordered before acceptance.
pub(crate) fn validate(expect: &Value) -> Result<(), String> {
    known(expect, TOP, "case")?;
    known_entries(expect.get("plan_checks"), PLAN_CHECK, "plan_checks")?;
    let sessions = object(&expect["sessions"], "sessions")?;
    for (label, session) in sessions {
        known(session, SESSION, &format!("sessions.{label}"))?;
        if !session["close"].is_null() {
            known(&session["close"], CLOSE, &format!("sessions.{label}.close"))?;
        }
    }
    let turns = expect["turns"]
        .as_array()
        .filter(|turns| !turns.is_empty())
        .ok_or("turns: missing or empty")?;
    for (index, turn) in turns.iter().enumerate() {
        let at = format!("turns[{index}]");
        known(turn, TURN, &at)?;
        let label = turn["session"].as_str().unwrap_or("main");
        if !sessions.contains_key(label) {
            return Err(format!("{at}: unknown session {label}"));
        }
        known_if(
            turn.get("start_after"),
            START_AFTER,
            &format!("{at}.start_after"),
        )?;
        if let Some(before) = turn["start_after"]["turn"].as_u64()
            && usize::try_from(before).map_or(true, |before| before >= index)
        {
            return Err(format!("{at}: start_after names a later turn"));
        }
        known_if(turn.get("params"), PARAMS, &format!("{at}.params"))?;
        known_if(turn.get("stop"), STOP, &format!("{at}.stop"))?;
        known_entries(turn.get("steer"), STEER, &format!("{at}.steer"))?;
        let fields = turn["expect"]
            .as_object()
            .ok_or_else(|| format!("{at}: no expect"))?;
        for key in fields.keys() {
            if !VALUED.contains(&key.as_str()) && !EXPECT_OTHER.contains(&key.as_str()) {
                return Err(format!("{at}.expect: unknown field {key}"));
            }
        }
        let at = format!("{at}.expect");
        known_entries(
            fields.get("unasserted"),
            UNASSERTED,
            &format!("{at}.unasserted"),
        )?;
        observation_types(fields, &at)?;
        if fields
            .get("terminal")
            .and_then(Value::as_object)
            .is_some_and(|terminal| terminal.contains_key("usage"))
        {
            return Err(format!(
                "{at}.terminal.usage: usage lives under expect.usage"
            ));
        }
        known_if(fields.get("terminal"), TERMINAL, &format!("{at}.terminal"))?;
        known_if(
            fields
                .get("terminal")
                .and_then(|terminal| terminal.get("cost")),
            COST,
            &format!("{at}.terminal.cost"),
        )?;
        known_if(fields.get("usage"), USAGE, &format!("{at}.usage"))?;
        known_if(
            fields.get("stop_facts"),
            STOP_FACTS,
            &format!("{at}.stop_facts"),
        )?;
        known_if(fields.get("instance"), INSTANCE, &format!("{at}.instance"))?;
        for entry in turn["expect"]["unasserted"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if entry["field"].as_str().is_none() || entry["why"].as_str().is_none_or(str::is_empty)
            {
                return Err(format!("{at}.unasserted: needs field and why"));
            }
        }
    }
    rules(expect)
}

/// C2 terminal statuses.
const STATUS: &[&str] = &["completed", "interrupted", "failed"];
/// C2 `StopReason`.
const STOP_REASON: &[&str] = &[
    "end_turn",
    "max_steps",
    "budget",
    "refusal",
    "interrupted",
    "error",
    "other",
];
/// C2 `ClassHint`.
const CLASS_HINT: &[&str] = &[
    "auth",
    "rate_limit",
    "context_exceeded",
    "budget_exceeded",
    "vendor_error",
    "protocol",
    "resume_mismatch",
];
/// C2 `AdapterError` kinds other than `Rejected`, which is stated as `rejected`.
const ERROR: &[&str] = &[
    "deadline",
    "force_stop",
    "overflow",
    "protocol",
    "process_exit",
    "unknown_submission",
    "server_lost",
    "transport_lost",
    // C2 §2 identity: `AdapterError::ResumeMismatch { evidence }`, for a
    // mismatch before a terminal was retained; never `Rejected`.
    "resume_mismatch",
];
/// C2 `StartRejected`; a name ending in `:` takes a non-empty suffix.
const REJECTED: &[&str] = &[
    "bound_unsupported",
    "invalid_param:",
    "vendor_error",
    "session_gone",
    "protocol",
];
/// C2 `Refusal` kinds; a name ending in `:` takes a non-empty suffix.
const REFUSAL: &[&str] = &[
    "unsupported_verb",
    "bound_unsupported",
    "harness_unavailable",
    "unknown_model",
    "version_refused",
    "vendor_option_conflict",
    "invalid_param:",
    "missing_capability:",
];
const CLEANUP: &[&str] = &["quiescent", "uncertain"];
const SETTLES: &[&str] = &["at_terminal", "at_p7_bound", "when_tools_end"];
const CONFIRMED: &str = "session.vendor_identity_confirmed";
const ACCEPTED: &str = "turn.accepted";

/// Whether `value` is null or one of `names`; a name ending in `:` admits
/// that prefix followed by any non-empty suffix.
fn named(value: &Value, names: &[&str]) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => names.iter().any(|name| match name.strip_suffix(':') {
            Some(prefix) => text
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|rest| !rest.is_empty()),
            None => text == name,
        }),
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => false,
    }
}

/// The value rules of [`validate`], over every turn, reporting all breaks.
fn rules(expect: &Value) -> Result<(), String> {
    let mut wrong = Vec::new();
    for (index, turn) in expect["turns"].as_array().into_iter().flatten().enumerate() {
        let e = &turn["expect"];
        let terminal = &e["terminal"];
        let enumerated: [(&str, &Value, &[&str]); 8] = [
            ("cleanup", &e["cleanup"], CLEANUP),
            ("cleanup_settles", &e["cleanup_settles"], SETTLES),
            ("error", &e["error"], ERROR),
            ("rejected", &e["rejected"], REJECTED),
            ("plan_refusal", &e["plan_refusal"], REFUSAL),
            ("terminal.status", &terminal["status"], STATUS),
            (
                "terminal.stop_reason",
                &terminal["stop_reason"],
                STOP_REASON,
            ),
            ("terminal.class_hint", &terminal["class_hint"], CLASS_HINT),
        ];
        for (field, value, names) in enumerated {
            if !named(value, names) {
                wrong.push(format!(
                    "turns[{index}].expect.{field}: {value} is not a C2 name"
                ));
            }
        }
        if e["accepted"] == json!(true) && e["observation_counts"][ACCEPTED] != json!(1) {
            wrong.push(format!(
                "turns[{index}]: an accepted turn states observation_counts {ACCEPTED} = 1"
            ));
        }
        let order = strings(&e["observations_order"]);
        let first = |kind| order.iter().position(|&seen| seen == kind);
        if let Some(confirmed) = first(CONFIRMED)
            && first(ACCEPTED).is_none_or(|accepted| accepted < confirmed)
        {
            wrong.push(format!(
                "turns[{index}].expect.observations_order: {CONFIRMED} must precede {ACCEPTED}"
            ));
        }
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("\n"))
    }
}

/// Whether `actual` holds every stated part of `expected`: objects by their
/// stated keys (an absent actual key counts as null), arrays element by
/// element with equal length, scalars by equality.
fn holds(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => expected
            .iter()
            .all(|(key, value)| holds(value, actual.get(key).unwrap_or(&Value::Null))),
        (Value::Array(expected), Value::Array(actual)) => {
            expected.len() == actual.len() && expected.iter().zip(actual).all(|(e, a)| holds(e, a))
        }
        _ => expected == actual,
    }
}

impl TurnOutcome {
    fn valued(&self) -> Value {
        json!({
            "plan_refusal": self.plan_refusal,
            "rejected": self.rejected,
            "accepted": self.accepted,
            "terminal": self.terminal,
            "usage": self.usage,
            "final_text": self.final_text,
            "error": self.error,
            "cleanup": self.cleanup,
            "cleanup_settles": self.cleanup_settles,
            "stop_facts": self.stop_facts,
            "instance": self.instance,
        })
    }
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Compares `outcome` with every stated field of `expect`, returning all
/// mismatches at once.
pub(crate) fn check(expect: &Value, outcome: &Outcome) -> Result<(), String> {
    validate(expect)?;
    let mut wrong = Vec::new();
    if let Some(launches) = expect.get("launches")
        && *launches != json!(outcome.launches)
    {
        wrong.push(format!(
            "launches: expected {launches}, got {}",
            outcome.launches
        ));
    }
    let checks: Vec<&Value> = expect["plan_checks"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    if checks.len() != outcome.plan_checks.len() {
        wrong.push(format!(
            "plan_checks: expected {}, got {}",
            checks.len(),
            outcome.plan_checks.len()
        ));
    }
    for (index, (check, actual)) in checks.iter().zip(&outcome.plan_checks).enumerate() {
        if check["refusal"] != json!(actual) {
            wrong.push(format!(
                "plan_checks[{index}]: expected {}, got {actual:?}",
                check["refusal"]
            ));
        }
    }
    for (label, session) in expect["sessions"].as_object().into_iter().flatten() {
        let Some(close) = session.get("close") else {
            continue;
        };
        let actual = outcome.closes.get(label).cloned().flatten();
        let ok = match (close, &actual) {
            (Value::Null, None) => true,
            (Value::Null, Some(_)) | (_, None) => false,
            (stated, Some(actual)) => {
                let mut stated = stated.clone();
                // The mode is an input to the driver, not a result.
                if let Some(map) = stated.as_object_mut() {
                    map.remove("mode");
                }
                holds(&stated, actual)
            }
        };
        if !ok {
            wrong.push(format!(
                "sessions.{label}.close: expected {close}, got {actual:?}"
            ));
        }
    }
    let turns = expect["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if turns.len() != outcome.turns.len() {
        wrong.push(format!(
            "turns: expected {}, got {}",
            turns.len(),
            outcome.turns.len()
        ));
    }
    for (index, (turn, actual)) in turns.iter().zip(&outcome.turns).enumerate() {
        check_turn(index, turn, actual, &mut wrong);
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("\n"))
    }
}

fn check_turn(index: usize, turn: &Value, actual: &TurnOutcome, wrong: &mut Vec<String>) {
    let expect = &turn["expect"];
    let skipped: Vec<&str> = expect["unasserted"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["field"].as_str())
        .collect();
    let valued = actual.valued();
    for field in VALUED {
        let Some(stated) = expect.get(*field) else {
            continue;
        };
        if skipped.contains(field) {
            continue;
        }
        if !holds(stated, &valued[*field]) {
            wrong.push(format!(
                "turn {index} {field}: expected {stated}, got {}",
                valued[*field]
            ));
        }
    }
    let steer: Vec<&str> = turn["steer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|attempt| attempt["result"].as_str())
        .collect();
    if steer != actual.steer {
        wrong.push(format!(
            "turn {index} steer: expected {steer:?}, got {:?}",
            actual.steer
        ));
    }
    let seen: Vec<&str> = actual.observations.iter().map(String::as_str).collect();
    for kind in strings(&expect["observations_include"]) {
        if !seen.contains(&kind) {
            wrong.push(format!("turn {index}: missing observation {kind}"));
        }
    }
    for kind in strings(&expect["observations_exclude"]) {
        if seen.contains(&kind) {
            wrong.push(format!("turn {index}: unexpected observation {kind}"));
        }
    }
    for (kind, count) in expect["observation_counts"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let got = seen.iter().filter(|&&s| s == kind).count();
        if *count != json!(got) {
            wrong.push(format!(
                "turn {index}: {kind} count expected {count}, got {got}"
            ));
        }
    }
    let order = strings(&expect["observations_order"]);
    let mut rest = seen.iter();
    if !order.iter().all(|kind| rest.any(|s| s == kind)) {
        wrong.push(format!(
            "turn {index}: observations {seen:?} lack the order {order:?}"
        ));
    }
}

/// The outcome a conforming driver would report for `expect`: every stated
/// value, every included and ordered observation with its stated count.
/// Used to test the checker and the expectation files against each other.
pub(crate) fn ideal(expect: &Value) -> Outcome {
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let object = |value: &Value| (!value.is_null()).then(|| value.clone());
    let turns = expect["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|turn| {
            let e = &turn["expect"];
            let mut observations: Vec<String> = strings(&e["observations_order"])
                .into_iter()
                .chain(strings(&e["observations_include"]))
                .map(str::to_owned)
                .collect();
            let mut seen = Vec::new();
            observations.retain(|kind| {
                let first = !seen.contains(kind);
                seen.push(kind.clone());
                first
            });
            for (kind, count) in e["observation_counts"].as_object().into_iter().flatten() {
                // Extra copies follow the first one, so the stated order holds.
                let want = usize::try_from(count.as_u64().unwrap_or(0)).unwrap_or(usize::MAX);
                match observations.iter().position(|seen| seen == kind) {
                    Some(_) if want == 0 => observations.retain(|seen| seen != kind),
                    Some(first) => {
                        for _ in 1..want {
                            observations.insert(first + 1, kind.clone());
                        }
                    }
                    None => observations.extend((0..want).map(|_| kind.clone())),
                }
            }
            TurnOutcome {
                plan_refusal: text(&e["plan_refusal"]),
                rejected: text(&e["rejected"]),
                accepted: e["accepted"].as_bool().unwrap_or(false),
                terminal: object(&e["terminal"]),
                usage: object(&e["usage"]),
                final_text: e["final_text"]
                    .as_array()
                    .map(|pieces| pieces.iter().filter_map(text).collect()),
                error: text(&e["error"]),
                cleanup: text(&e["cleanup"]),
                cleanup_settles: text(&e["cleanup_settles"]),
                stop_facts: object(&e["stop_facts"]),
                instance: object(&e["instance"]),
                steer: strings(&turn["steer"].as_array().map_or(Value::Null, |attempts| {
                    attempts.iter().map(|a| a["result"].clone()).collect()
                }))
                .into_iter()
                .map(str::to_owned)
                .collect(),
                observations,
            }
        })
        .collect();
    Outcome {
        launches: expect["launches"].as_u64().unwrap_or(0),
        plan_checks: expect["plan_checks"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|check| text(&check["refusal"]))
            .collect(),
        closes: expect["sessions"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(label, session)| (label.clone(), object(&session["close"])))
            .collect(),
        turns,
    }
}
