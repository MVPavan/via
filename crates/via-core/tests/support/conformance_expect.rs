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
//! - `launches` counts vendor process starts over the whole case;
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
    /// Vendor process launches over the case.
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

/// Checks the expectation's shape: known fields only, required parts present,
/// session labels and `start_after` turns resolvable, and a reason for each
/// `unasserted` entry.
pub(crate) fn validate(expect: &Value) -> Result<(), String> {
    known(expect, TOP, "case")?;
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
        if let Some(before) = turn["start_after"]["turn"].as_u64()
            && usize::try_from(before).map_or(true, |before| before >= index)
        {
            return Err(format!("{at}: start_after names a later turn"));
        }
        let fields = turn["expect"]
            .as_object()
            .ok_or_else(|| format!("{at}: no expect"))?;
        for key in fields.keys() {
            if !VALUED.contains(&key.as_str()) && !EXPECT_OTHER.contains(&key.as_str()) {
                return Err(format!("{at}.expect: unknown field {key}"));
            }
        }
        for entry in turn["expect"]["unasserted"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if entry["field"].as_str().is_none() || entry["why"].as_str().is_none_or(str::is_empty)
            {
                return Err(format!("{at}.expect.unasserted: needs field and why"));
            }
        }
    }
    Ok(())
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
