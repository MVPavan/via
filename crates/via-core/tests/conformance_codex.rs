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
/// collects the outcome (see the checker's module docs for its obligations:
/// `launches` from `<case>.launches`, the replay's exit status, and the
/// server's stdin close at an `await_eof` step).
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

/// A named change that makes an expectation malformed, and a fragment of
/// the refusal that names the rule it breaks.
type Defect = (&'static str, &'static str, fn(&mut Value));

/// Green: `validate` refuses each kind of malformed expectation, one
/// mutation of a well-formed case per rule, for the reason that rule gives.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one table row per validation rule keeps every witness beside its rule"
)]
fn conformance_codex_validate_refuses_each_defect() {
    let base = conformance_expect::load(&fixtures(), "c2_steer").unwrap();
    conformance_expect::validate(&base).unwrap();
    let defects: [Defect; 37] = [
        (
            "accepted turn counts turn.accepted twice",
            "an accepted turn states",
            |e| {
                e["turns"][0]["expect"]["observation_counts"] = json!({"turn.accepted": 2});
            },
        ),
        (
            "accepted turn omits the turn.accepted count",
            "an accepted turn states",
            |e| {
                e["turns"][0]["expect"]["observation_counts"] = json!({});
            },
        ),
        (
            "identity confirmation after acceptance",
            "must precede",
            |e| {
                e["turns"][0]["expect"]["observations_order"] =
                    json!(["turn.accepted", "session.vendor_identity_confirmed"]);
            },
        ),
        ("cleanup", "expect.cleanup:", |e| {
            e["turns"][0]["expect"]["cleanup"] = json!("pending");
        }),
        ("cleanup_settles", "expect.cleanup_settles:", |e| {
            e["turns"][0]["expect"]["cleanup_settles"] = json!("later");
        }),
        ("error", "expect.error:", |e| {
            e["turns"][0]["expect"]["error"] = json!("timeout");
        }),
        ("rejected", "expect.rejected:", |e| {
            e["turns"][0]["expect"]["rejected"] = json!("invalid_param");
        }),
        ("plan_refusal", "expect.plan_refusal:", |e| {
            e["turns"][0]["expect"]["plan_refusal"] = json!("invalid_params:effort");
        }),
        ("terminal.status", "terminal.status:", |e| {
            e["turns"][0]["expect"]["terminal"]["status"] = json!("done");
        }),
        ("terminal.stop_reason", "terminal.stop_reason:", |e| {
            e["turns"][0]["expect"]["terminal"]["stop_reason"] = json!("stop");
        }),
        ("terminal.class_hint", "terminal.class_hint:", |e| {
            e["turns"][0]["expect"]["terminal"]["class_hint"] = json!("unauthorized");
        }),
        (
            "observations_include is a string",
            "observations_include: not an array of strings",
            |e| {
                e["turns"][0]["expect"]["observations_include"] = json!("turn.accepted");
            },
        ),
        (
            "observations_include holds a number",
            "observations_include: not an array of strings",
            |e| {
                e["turns"][0]["expect"]["observations_include"] = json!(["turn.accepted", 1]);
            },
        ),
        (
            "observations_include is null",
            "observations_include: not an array of strings",
            |e| {
                e["turns"][0]["expect"]["observations_include"] = Value::Null;
            },
        ),
        (
            "observations_exclude is a string",
            "observations_exclude: not an array of strings",
            |e| {
                e["turns"][0]["expect"]["observations_exclude"] = json!("action.denied");
            },
        ),
        (
            "observations_order is a string",
            "observations_order: not an array of strings",
            |e| {
                e["turns"][0]["expect"]["observations_order"] = json!("turn.accepted");
            },
        ),
        (
            "observation_counts is an array",
            "observation_counts: not an object",
            |e| {
                e["turns"][0]["expect"]["observation_counts"] = json!([["turn.accepted", 1]]);
            },
        ),
        (
            "observation_counts holds a negative count",
            "observation_counts.final_text: not a non-negative integer",
            |e| {
                e["turns"][0]["expect"]["observation_counts"]["final_text"] = json!(-1);
            },
        ),
        (
            "observation_counts holds a fraction",
            "observation_counts.final_text: not a non-negative integer",
            |e| {
                e["turns"][0]["expect"]["observation_counts"]["final_text"] = json!(1.5);
            },
        ),
        (
            "observation_counts holds a string",
            "observation_counts.final_text: not a non-negative integer",
            |e| {
                e["turns"][0]["expect"]["observation_counts"]["final_text"] = json!("1");
            },
        ),
        (
            "usage under the retired terminal.usage",
            "terminal.usage: usage lives under expect.usage",
            |e| {
                e["turns"][0]["expect"]["terminal"]["usage"] = json!({"from": "samples"});
            },
        ),
        (
            "unknown top-level field",
            "case: unknown field usage",
            |e| {
                e["usage"] = json!({});
            },
        ),
        (
            "unknown plan_checks field",
            "plan_checks[0]: unknown field verb",
            |e| {
                e["plan_checks"] = json!([{"require": "steer", "refusal": null, "verb": "steer"}]);
            },
        ),
        (
            "unknown session field",
            "sessions.main: unknown field bound",
            |e| {
                e["sessions"]["main"]["bound"] = json!("full");
            },
        ),
        (
            "unknown close field",
            "sessions.main.close: unknown field leftovers",
            |e| {
                e["sessions"]["main"]["close"]["leftovers"] = Value::Null;
            },
        ),
        (
            "unknown turn field",
            "turns[0]: unknown field wall_ms",
            |e| {
                e["turns"][0]["wall_ms"] = json!(1000);
            },
        ),
        (
            "unknown start_after field",
            "turns[1].start_after: unknown field delay_ms",
            |e| {
                let mut next = e["turns"][0].clone();
                next["start_after"] = json!({"turn": 0, "event": "accepted", "delay_ms": 5});
                e["turns"].as_array_mut().unwrap().push(next);
            },
        ),
        (
            "unknown params field",
            "turns[0].params: unknown field wall",
            |e| {
                e["turns"][0]["params"]["wall"] = json!(1000);
            },
        ),
        (
            "unknown stop field",
            "turns[0].stop: unknown field delay_ms",
            |e| {
                e["turns"][0]["stop"] =
                    json!({"kind": "interrupt", "after": "accepted", "delay_ms": 5});
            },
        ),
        (
            "unknown steer field",
            "turns[0].steer[1]: unknown field expected_turn",
            |e| {
                e["turns"][0]["steer"][1]["expected_turn"] = json!("t");
            },
        ),
        (
            "unknown expect field",
            "turns[0].expect: unknown field stdin_sequence",
            |e| {
                e["turns"][0]["expect"]["stdin_sequence"] = json!([]);
            },
        ),
        (
            "unknown unasserted field",
            "turns[0].expect.unasserted[0]: unknown field until",
            |e| {
                e["turns"][0]["expect"]["unasserted"] =
                    json!([{"field": "leftovers", "why": "later", "until": "S-LEFTOVER"}]);
            },
        ),
        (
            "unknown terminal field",
            "turns[0].expect.terminal: unknown field state",
            |e| {
                e["turns"][0]["expect"]["terminal"]["state"] = json!("completed");
            },
        ),
        (
            "unknown terminal.cost field",
            "turns[0].expect.terminal.cost: unknown field total",
            |e| {
                e["turns"][0]["expect"]["terminal"]["cost"] = json!({"scope": "turn", "total": 1});
            },
        ),
        (
            "unknown usage field",
            "turns[0].expect.usage: unknown field input",
            |e| {
                e["turns"][0]["expect"]["usage"]["input"] = json!(1);
            },
        ),
        (
            "unknown stop_facts field",
            "turns[0].expect.stop_facts: unknown field late",
            |e| {
                e["turns"][0]["expect"]["stop_facts"] =
                    json!({"acknowledged": true, "forced": false, "shared": true, "late": true});
            },
        ),
        (
            "unknown instance field",
            "turns[0].expect.instance: unknown field build",
            |e| {
                e["turns"][0]["expect"]["instance"]["build"] = json!("x");
            },
        ),
    ];
    let wrong: Vec<String> = defects
        .into_iter()
        .filter_map(|(what, rule, defect)| {
            let mut expect = base.clone();
            defect(&mut expect);
            match conformance_expect::validate(&expect) {
                Ok(()) => Some(format!("{what}: accepted")),
                Err(error) if !error.contains(rule) => {
                    Some(format!("{what}: refused for another reason: {error}"))
                }
                Err(_) => None,
            }
        })
        .collect();
    assert!(wrong.is_empty(), "validate:\n{}", wrong.join("\n"));
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
