//! Codex adapter conformance (`via-5lr.3.1`): one test per sanitized replay
//! fixture in `crates/via-adapters/tests/fixtures/codex/`.
//!
//! Each case pairs `<case>.replay.json` (the recorded `codex app-server`
//! exchange, replayed by the fake agent) with `<case>.expect.json` (what the
//! C2 driver must produce, in the unified expectation schema). The case
//! tests are red by construction until the adapter slice `via-5lr.3.2`
//! replaces [`drive`] and removes the `ignore`.

#[path = "support/conformance_expect.rs"]
mod conformance_expect;

use std::path::{Path, PathBuf};

use conformance_expect::Outcome;
use serde_json::{Value, json};

/// The bead whose adapter makes these cases pass.
const ADAPTER_BEAD: &str = "via-5lr.3.2";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../via-adapters/tests/fixtures/codex")
}

/// Runs the case's sessions and turns through the Codex C2 driver against
/// the fake agent replaying `<case>.replay.json`, with controlled time, and
/// collects the outcome (see the checker's module docs for its obligations).
/// Replaced by `via-5lr.3.2`.
fn drive(name: &str, _expect: &Value, _replay: &Path) -> Result<Outcome, String> {
    Err(format!(
        "adapter not implemented: {ADAPTER_BEAD} (case {name})"
    ))
}

fn check(name: &str) -> Result<(), String> {
    let dir = fixtures();
    let expect = conformance_expect::load(&dir, name)?;
    let outcome = drive(name, &expect, &dir.join(format!("{name}.replay.json")))?;
    conformance_expect::check(&expect, &outcome).map_err(|wrong| format!("{name}:\n{wrong}"))
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
/// names this harness and validates against the unified schema.
#[test]
fn conformance_codex_cases_match_fixture_files() {
    let dir = fixtures();
    let mut listed: Vec<&str> = CASES.to_vec();
    listed.sort_unstable();
    assert_eq!(conformance_expect::case_names(&dir).unwrap(), listed);
    for name in CASES {
        let expect = conformance_expect::load(&dir, name).unwrap();
        assert_eq!(expect["harness"], "codex", "{name}: harness");
        conformance_expect::validate(&expect).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            dir.join(format!("{name}.replay.json")).is_file(),
            "{name}: replay fixture missing"
        );
    }
}

/// A named change to an ideal outcome.
type Mutation = (&'static str, fn(&mut Outcome));

/// Green now: the checker accepts the ideal outcome of every case and
/// reports a change in any compared part of it.
#[test]
fn conformance_codex_checker_detects_each_difference() {
    let dir = fixtures();
    for name in CASES {
        let expect = conformance_expect::load(&dir, name).unwrap();
        conformance_expect::check(&expect, &conformance_expect::ideal(&expect))
            .unwrap_or_else(|e| panic!("{name}: ideal outcome refused:\n{e}"));
        let mutations: [Mutation; 12] = [
            ("usage", |o| {
                o.turns[0].usage = Some(json!({"input_tokens": 1}));
            }),
            ("final_text", |o| {
                o.turns[0].final_text = Some(vec!["other".to_owned()]);
            }),
            ("cleanup_settles", |o| {
                o.turns[0].cleanup_settles = Some("never".to_owned());
            }),
            ("steer", |o| o.turns[0].steer.push("injected".to_owned())),
            ("launches", |o| o.launches += 1),
            ("close", |o| {
                for close in o.closes.values_mut() {
                    *close = match close.take() {
                        Some(_) => None,
                        None => Some(json!({"vendor_closed": true})),
                    };
                }
            }),
            ("accepted", |o| o.turns[0].accepted = !o.turns[0].accepted),
            ("terminal", |o| {
                o.turns[0].terminal = Some(json!({"status": "other"}));
            }),
            ("cleanup", |o| {
                o.turns[0].cleanup = Some("pending".to_owned());
            }),
            ("observations", |o| {
                o.turns[0].observations.push("turn.accepted".to_owned());
                o.turns[0].observations.push("action.denied".to_owned());
            }),
            ("instance", |o| {
                o.turns[0].instance = Some(json!({"vendor_version": "0.0.0"}));
            }),
            ("turns", |o| {
                o.turns.pop();
            }),
        ];
        for (what, mutate) in mutations {
            let mut outcome = conformance_expect::ideal(&expect);
            mutate(&mut outcome);
            let first = &expect["turns"][0]["expect"];
            let stated = match what {
                "launches" => expect.get("launches").is_some(),
                "close" => expect["sessions"]
                    .as_object()
                    .is_some_and(|s| s.values().any(|s| s.get("close").is_some())),
                "observations" => {
                    first.get("observation_counts").is_some()
                        || first.get("observations_exclude").is_some()
                }
                "turns" | "steer" => true,
                field => first.get(field).is_some(),
            };
            if stated {
                assert!(
                    conformance_expect::check(&expect, &outcome).is_err(),
                    "{name}: a changed {what} passed"
                );
            }
        }
    }
}
