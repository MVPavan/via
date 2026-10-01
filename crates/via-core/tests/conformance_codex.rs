//! Codex adapter conformance (`via-5lr.3.1`): one test per sanitized replay
//! fixture in `crates/via-adapters/tests/fixtures/codex/`.
//!
//! Each case pairs `<case>.replay.json` (the recorded `codex app-server`
//! exchange, replayed by the fake agent) with `<case>.expect.json` (what the
//! C2 driver must produce). The case tests are red by construction until the
//! adapter slice `via-5lr.3.2` replaces [`drive`] and removes the `ignore`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The bead whose adapter makes these cases pass.
const ADAPTER_BEAD: &str = "via-5lr.3.2";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../via-adapters/tests/fixtures/codex")
}

/// One fixture and its expectation.
struct Case {
    name: String,
    /// The fixture the fake agent replays, installed beside it as `<case>`.
    #[expect(dead_code, reason = "read by the adapter driver in via-5lr.3.2")]
    replay: PathBuf,
    expect: Value,
}

/// What the C2 driver produced, one entry per expected turn, in the
/// expectation schema's terms plus `observations`: the kinds it emitted.
struct Outcome {
    turns: Vec<Value>,
}

fn load(name: &str) -> Result<Case, String> {
    let dir = fixtures();
    let text = fs::read_to_string(dir.join(format!("{name}.expect.json")))
        .map_err(|error| format!("{name}.expect.json: {error}"))?;
    Ok(Case {
        name: name.to_owned(),
        replay: dir.join(format!("{name}.replay.json")),
        expect: serde_json::from_str(&text)
            .map_err(|error| format!("{name}.expect.json: {error}"))?,
    })
}

/// The string members of an optional JSON array.
fn strings(value: &Value) -> BTreeSet<&str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Runs the case's sessions and turns through the Codex C2 driver against
/// the replayed server. Replaced by `via-5lr.3.2`.
fn drive(case: &Case) -> Result<Outcome, String> {
    Err(format!(
        "adapter not implemented: {ADAPTER_BEAD} (case {})",
        case.name
    ))
}

/// Fields of a turn's `expect` compared by value; the rest are checked below
/// or are prose.
const COMPARED: &[&str] = &[
    "plan_refusal",
    "rejected",
    "accepted",
    "terminal",
    "error",
    "cleanup",
    "steer",
    "final_text",
    "usage",
    "instance",
    "stop_facts",
    "structured_output",
];

fn check(name: &str) -> Result<(), String> {
    let case = load(name)?;
    let outcome = drive(&case).map_err(|error| format!("{name}: {error}"))?;
    let turns = case.expect["turns"]
        .as_array()
        .ok_or_else(|| format!("{name}: no turns"))?;
    assert_eq!(outcome.turns.len(), turns.len(), "{name}: turn count");
    for (index, (turn, actual)) in turns.iter().zip(&outcome.turns).enumerate() {
        let expect = &turn["expect"];
        let open = strings(&expect["unasserted"]);
        for field in COMPARED {
            if open.contains(field) || expect.get(*field).is_none() {
                continue;
            }
            assert_eq!(
                actual.get(*field),
                expect.get(*field),
                "{name} turn {index}: {field}"
            );
        }
        let seen = strings(&actual["observations"]);
        for kind in strings(&expect["observations_include"]) {
            assert!(seen.contains(kind), "{name} turn {index}: missing {kind}");
        }
        for kind in strings(&expect["observations_exclude"]) {
            assert!(
                !seen.contains(kind),
                "{name} turn {index}: unexpected {kind}"
            );
        }
    }
    Ok(())
}

macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        /// Every case with a test below.
        const CASES: &[&str] = &[$(stringify!($name)),*];

        mod conformance_codex_cases {
            $(
                #[test]
                #[ignore = "red until via-5lr.3.2"]
                fn $name() {
                    super::check(stringify!($name)).unwrap();
                }
            )*
        }
    };
}

cases! {
    c0_server_lost,
    c10_read_only_refused,
    c11_failed_command,
    c1_commentary_usage,
    c2_steer,
    c3_interrupt_uncertain,
    c3_wall_interrupt,
    c4_two_sessions,
    c4b_workspace_write_refused,
    c5_resume,
    c5_resume_missing,
    c6_cold_initialize,
    c7_bad_model,
    c7_effort_catalog,
    c8_auth,
    c9_output_schema,
}

/// Green now: every expectation file has a case test and a replay fixture,
/// and names this harness.
#[test]
fn conformance_codex_cases_match_fixture_files() {
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(fixtures()).expect("fixture directory") {
        let path = entry.expect("fixture entry").path();
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if let Some(name) = file.strip_suffix(".expect.json") {
            names.insert(name.to_owned());
        }
    }
    let listed: BTreeSet<String> = CASES.iter().map(|&n| n.to_owned()).collect();
    assert_eq!(names, listed, "expect files and case tests differ");
    for name in &names {
        let case = load(name).unwrap();
        assert_eq!(case.expect["harness"], "codex", "{name}: harness");
        assert!(
            case.expect["turns"]
                .as_array()
                .is_some_and(|t| !t.is_empty()),
            "{name}: no turns"
        );
        assert!(
            fixtures().join(format!("{name}.replay.json")).is_file(),
            "{name}: replay fixture missing"
        );
    }
}
