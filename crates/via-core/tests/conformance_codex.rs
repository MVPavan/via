//! Codex adapter conformance (`via-5lr.3.1`): one test per sanitized replay
//! fixture in `crates/via-adapters/tests/fixtures/codex/`.
//!
//! Each case pairs `<case>.replay.json` (the recorded `codex app-server`
//! exchange, replayed by the fake agent) with `<case>.expect.json` (what the
//! C2 driver must produce, in the unified expectation schema). The case
//! tests are red by construction until the adapter slice `via-5lr.3.2`
//! replaces [`drive`] and removes the `ignore`.
//!
//! A resumed thread is pinned literally: each `thread/resume` expects the
//! case's `sessions.<label>.resume` as its `threadId`.
//!
//! # Versions
//!
//! Every launched case pins `instance` `{vendor_version: "0.159.2",
//! version_status: "tested"}`, parsed from `initialize`'s `userAgent`:
//! `via-5lr.3.2`'s initial `checked` set holds the fixture version (the live
//! re-probes of 2026-09-30). The synthetic untested case is `via-5lr.3.2`'s
//! `codex_pin_handshake`.

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
/// names this harness, validates against the unified schema and gates only
/// `await_signal` steps of its replay.
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
        let replay = std::fs::read(dir.join(format!("{name}.replay.json")))
            .unwrap_or_else(|e| panic!("{name}: replay fixture: {e}"));
        let replay: Value = serde_json::from_slice(&replay).unwrap();
        conformance_expect::gates_resolve(&expect, &replay)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
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
    let defects: [Defect; 72] = [
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
            "observations_include: not an array of kinds or observation objects",
            |e| {
                e["turns"][0]["expect"]["observations_include"] = json!("turn.accepted");
            },
        ),
        (
            "observations_include holds a number",
            "observations_include: not an array of kinds or observation objects",
            |e| {
                e["turns"][0]["expect"]["observations_include"] = json!(["turn.accepted", 1]);
            },
        ),
        (
            "observations_include is null",
            "observations_include: not an array of kinds or observation objects",
            |e| {
                e["turns"][0]["expect"]["observations_include"] = Value::Null;
            },
        ),
        (
            "observations_exclude is a string",
            "observations_exclude: not an array of kinds or observation objects",
            |e| {
                e["turns"][0]["expect"]["observations_exclude"] = json!("action.denied");
            },
        ),
        (
            "observations_order is a string",
            "observations_order: not an array of kinds or observation objects",
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
                e["launch_checkpoints"]["after_turn"] = json!([1, 1]);
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
            "unasserted names a stated field",
            "unasserted: accepted is also stated",
            |e| {
                e["turns"][0]["expect"]["unasserted"] =
                    json!([{"field": "accepted", "why": "skip"}]);
            },
        ),
        (
            "a baseline field neither stated nor unasserted",
            "cleanup is stated (null allowed) or unasserted",
            |e| {
                e["turns"][0]["expect"]
                    .as_object_mut()
                    .unwrap()
                    .remove("cleanup");
            },
        ),
        ("launches missing", "case: launches is required", |e| {
            e.as_object_mut().unwrap().remove("launches");
        }),
        (
            "start_after names a later turn",
            "start_after.turn: names no earlier turn",
            |e| {
                e["turns"][0]["start_after"] = json!({"turn": 0, "event": "accepted"});
            },
        ),
        ("start_after event unknown", "start_after.event:", |e| {
            let mut next = e["turns"][0].clone();
            next["start_after"] = json!({"turn": 0, "event": "finished"});
            e["turns"].as_array_mut().unwrap().push(next);
            e["launch_checkpoints"]["after_turn"] = json!([1, 1]);
        }),
        (
            "turn names an unknown session",
            "turns[0]: unknown session other",
            |e| {
                e["turns"][0]["session"] = json!("other");
            },
        ),
        (
            "bound as a bare string",
            "params.bound: not an object",
            |e| {
                e["turns"][0]["params"]["bound"] = json!("full");
            },
        ),
        ("bound mode unknown", "params.bound.mode:", |e| {
            e["turns"][0]["params"]["bound"]["mode"] = json!("everything");
        }),
        (
            "bound without network",
            "params.bound: network is required",
            |e| {
                e["turns"][0]["params"]["bound"]
                    .as_object_mut()
                    .unwrap()
                    .remove("network");
            },
        ),
        ("prompt not a string", "params.prompt:", |e| {
            e["turns"][0]["params"]["prompt"] = json!(1);
        }),
        ("max_steps zero", "params.max_steps:", |e| {
            e["turns"][0]["params"]["max_steps"] = json!(0);
        }),
        (
            "unknown deadlines field",
            "turns[0].deadlines: unknown field total_ms",
            |e| {
                e["turns"][0]["deadlines"] = json!({"total_ms": 1});
            },
        ),
        ("stop kind unknown", "stop.kind:", |e| {
            e["turns"][0]["stop"] = json!({"kind": "pause", "after": "accepted"});
        }),
        ("steer without text", "steer[0]: text is required", |e| {
            e["turns"][0]["steer"][0]
                .as_object_mut()
                .unwrap()
                .remove("text");
        }),
        ("close mode unknown", "close.mode:", |e| {
            e["sessions"]["main"]["close"]["mode"] = json!("kill");
        }),
        (
            "vendor_options not per harness",
            "vendor_options: one object of keys per harness",
            |e| {
                e["sessions"]["main"]["vendor_options"] = json!({"codex": "x"});
            },
        ),
        (
            "health failed without a cause",
            "a first_cause exactly when failed",
            |e| {
                e["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": null});
            },
        ),
        ("health cause unknown", "health.first_cause:", |e| {
            e["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": "crash"});
        }),
        (
            "checkpoints miss a session",
            "after_open: one count per session",
            |e| {
                e["launch_checkpoints"]["after_open"] = json!({});
            },
        ),
        (
            "checkpoints part missing",
            "launch_checkpoints: after_pure is stated or unasserted",
            |e| {
                e["launch_checkpoints"]
                    .as_object_mut()
                    .unwrap()
                    .remove("after_pure");
            },
        ),
        (
            "checkpoints miss a turn",
            "after_turn: one count per turn",
            |e| {
                e["launch_checkpoints"]["after_turn"] = json!([]);
            },
        ),
        ("pure_writes not strings", "case.pure_writes:", |e| {
            e["pure_writes"] = json!([1]);
        }),
        (
            "describe starts something",
            "describe: launches is 0",
            |e| {
                e["describe"] = json!({"params": {"harness": "codex"}, "launches": 1});
            },
        ),
        ("describe status unknown", "describe.version_status:", |e| {
            e["describe"] =
                json!({"params": {"harness": "codex"}, "version_status": "ok", "launches": 0});
        }),
        (
            "observation kind unknown",
            "observations_include: turn.started is not a C2 observation",
            |e| {
                e["turns"][0]["expect"]["observations_include"] = json!(["turn.started"]);
            },
        ),
        (
            "observation field unknown",
            "turn.accepted has no field turn_id",
            |e| {
                e["turns"][0]["expect"]["observations_include"] =
                    json!([{"kind": "turn.accepted", "turn_id": "t"}]);
            },
        ),
        (
            "counted kind unknown",
            "observation_counts: turn.started is not a C2 observation",
            |e| {
                e["turns"][0]["expect"]["observation_counts"]["turn.started"] = json!(0);
            },
        ),
        (
            "warning code unknown",
            "warnings: slow is not a C1 warning code",
            |e| {
                e["turns"][0]["expect"]["warnings"] = json!(["slow"]);
            },
        ),
        (
            "warning code twice",
            "warnings: a code is listed twice",
            |e| {
                e["turns"][0]["expect"]["warnings"] = json!(["deprecated", "deprecated"]);
            },
        ),
        ("instance status refused", "instance.version_status:", |e| {
            e["turns"][0]["expect"]["instance"]["version_status"] = json!("refused");
        }),
        (
            "unknown exit field",
            "expect.exit: unknown field status",
            |e| {
                e["turns"][0]["expect"]["exit"] = json!({"status": 0});
            },
        ),
        (
            "journal_uncertain not a boolean",
            "expect.journal_uncertain:",
            |e| {
                e["turns"][0]["expect"]["journal_uncertain"] = json!("no");
            },
        ),
        (
            "cost provenance unknown",
            "terminal.cost.provenance:",
            |e| {
                e["turns"][0]["expect"]["terminal"]["cost"]["provenance"] = json!("guessed");
            },
        ),
        ("gate without a step", "gates[0]: step is required", |e| {
            e["turns"][0]["gates"] = json!([{"expect": {}}]);
        }),
        (
            "gate expect field unknown",
            "gates[0].expect: unknown field unasserted",
            |e| {
                e["turns"][0]["gates"] = json!([{"step": 1, "expect": {"unasserted": []}}]);
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

/// Green: `unasserted` is documentary. A listed field the turn also states
/// is a validation error, and a stated field is compared whatever the list
/// says (review F1).
#[test]
fn conformance_codex_unasserted_is_documentary() {
    let mut expect = conformance_expect::load(&fixtures(), "c2_steer").unwrap();
    expect["turns"][0]["expect"]["unasserted"] = json!([{"field": "accepted", "why": "skip"}]);
    let refused = conformance_expect::validate(&expect);
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.contains("unasserted") && e.contains("accepted")),
        "{refused:?}"
    );
    let mut outcome = conformance_expect::ideal(&expect);
    outcome.turns[0].accepted = false;
    assert!(conformance_expect::check(&expect, &outcome).is_err());
}

/// Green: `validate` refuses a vacuous case and malformed inputs (review F2).
#[test]
fn conformance_codex_validate_refuses_vacuous_and_malformed_inputs() {
    let vacuous = json!({"sessions": {"main": {}}, "turns": [{"expect": {}}]});
    assert!(
        conformance_expect::validate(&vacuous).is_err(),
        "the vacuous case validated"
    );
    let base = conformance_expect::load(&fixtures(), "c4_two_sessions").unwrap();
    conformance_expect::validate(&base).unwrap();
    for turn in [json!("0"), json!(5), json!(1), json!(-1), json!(0.5)] {
        let mut expect = base.clone();
        expect["turns"][1]["start_after"]["turn"] = turn.clone();
        assert!(
            conformance_expect::validate(&expect).is_err(),
            "start_after.turn {turn} validated"
        );
    }
}

/// Green: opaque vendor JSON (`terminal.structured_output`) compares by
/// exact equality, so an extra property fails (review F3).
#[test]
fn conformance_codex_opaque_json_compares_exactly() {
    let expect = conformance_expect::load(&fixtures(), "c9_output_schema").unwrap();
    let mut outcome = conformance_expect::ideal(&expect);
    let terminal = outcome.turns[0].terminal.as_mut().unwrap();
    terminal["structured_output"]["invented"] = json!(true);
    assert!(
        conformance_expect::check(&expect, &outcome).is_err(),
        "an extra structured_output property passed"
    );
}

/// Green: a gate names an `await_signal` step of the case's replay, in its
/// lifetime; any other step, or a lifetime the replay lacks, is refused.
#[test]
fn conformance_codex_gates_name_await_signal_steps() {
    let dir = fixtures();
    let expect = conformance_expect::load(&dir, "c6_cold_initialize").unwrap();
    let replay: Value =
        serde_json::from_slice(&std::fs::read(dir.join("c6_cold_initialize.replay.json")).unwrap())
            .unwrap();
    conformance_expect::gates_resolve(&expect, &replay).unwrap();
    let lifetimes = json!({"source": "s", "lifetimes": [replay.clone(), replay.clone()]});
    let mut second = expect.clone();
    second["turns"][0]["gates"][0]["lifetime"] = json!(2);
    conformance_expect::gates_resolve(&second, &lifetimes).unwrap();
    for (what, gate) in [
        ("an expect step", json!({"step": 1, "expect": {}})),
        ("a step past the end", json!({"step": 999, "expect": {}})),
        (
            "a missing lifetime",
            json!({"step": 2, "lifetime": 2, "expect": {}}),
        ),
    ] {
        let mut wrong = expect.clone();
        wrong["turns"][0]["gates"] = json!([gate]);
        assert!(
            conformance_expect::gates_resolve(&wrong, &replay).is_err(),
            "a gate on {what} resolved"
        );
    }
}

/// Green: each gate's snapshot is compared with the gate's `expect`, so an
/// adapter that answered before the gate is caught.
#[test]
fn conformance_codex_gate_snapshots_are_compared() {
    let expect = conformance_expect::load(&fixtures(), "c6_cold_initialize").unwrap();
    let mut outcome = conformance_expect::ideal(&expect);
    conformance_expect::check(&expect, &outcome).unwrap();
    outcome.turns[0].gates[0]
        .observations
        .push(json!({"kind": "turn.accepted"}));
    assert!(conformance_expect::check(&expect, &outcome).is_err());
    outcome.turns[0].gates.clear();
    assert!(conformance_expect::check(&expect, &outcome).is_err());
}

/// Green: `warnings` compare as an exact set of C1 codes, in any order.
#[test]
fn conformance_codex_warnings_compare_as_an_exact_set() {
    let mut expect = conformance_expect::load(&fixtures(), "c2_steer").unwrap();
    expect["turns"][0]["expect"]["warnings"] = json!(["deprecated", "instructions_partial"]);
    let mut outcome = conformance_expect::ideal(&expect);
    outcome.turns[0].warnings = vec!["instructions_partial".to_owned(), "deprecated".to_owned()];
    conformance_expect::check(&expect, &outcome).unwrap();
    outcome.turns[0].warnings.pop();
    assert!(
        conformance_expect::check(&expect, &outcome).is_err(),
        "a missing code passed"
    );
    outcome.turns[0].warnings = vec![
        "deprecated".to_owned(),
        "instructions_partial".to_owned(),
        "config_switch_unverified".to_owned(),
    ];
    assert!(
        conformance_expect::check(&expect, &outcome).is_err(),
        "an extra code passed"
    );
}

/// Green: observation entries that are objects subset-match one observation
/// by its correlation fields.
#[test]
fn conformance_codex_observation_objects_match_fields() {
    let expect = conformance_expect::load(&fixtures(), "c2_steer").unwrap();
    let mut outcome = conformance_expect::ideal(&expect);
    conformance_expect::check(&expect, &outcome).unwrap();
    let tool = outcome.turns[0]
        .observations
        .iter_mut()
        .find(|o| o.get("tools_started").is_some())
        .unwrap();
    tool["tools_started"][0][0] = json!("exec-other");
    assert!(
        conformance_expect::check(&expect, &outcome).is_err(),
        "another tool ID passed"
    );
}

/// Green: a `describe` assertion compares its capabilities (subset), version
/// fields and launch count, never its `params`.
#[test]
fn conformance_codex_describe_compares_results() {
    let mut expect = conformance_expect::load(&fixtures(), "c2_steer").unwrap();
    expect["describe"] = json!({
        "params": {"harness": "codex", "model": "gpt-6-sol"},
        "capabilities": {"verbs": {"steer": "native"}},
        "vendor_version": null,
        "version_status": "untested",
        "launches": 0
    });
    let mut outcome = conformance_expect::ideal(&expect);
    conformance_expect::check(&expect, &outcome).unwrap();
    outcome.describe = Some(json!({
        "capabilities": {"verbs": {"steer": "native", "spawn": "native"}},
        "vendor_version": null,
        "version_status": "untested",
        "launches": 0
    }));
    conformance_expect::check(&expect, &outcome).unwrap();
    outcome.describe.as_mut().unwrap()["launches"] = json!(1);
    assert!(
        conformance_expect::check(&expect, &outcome).is_err(),
        "a describe launch passed"
    );
    outcome.describe = None;
    assert!(
        conformance_expect::check(&expect, &outcome).is_err(),
        "a missing describe passed"
    );
}

/// A named change to an ideal outcome.
type Mutation = (&'static str, fn(&mut Outcome));

/// Green now: the checker accepts the ideal outcome of every case and
/// reports a change in any compared part of it.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one table row per compared part keeps every mutation beside its check"
)]
fn conformance_codex_checker_detects_each_difference() {
    let dir = fixtures();
    for name in CASES {
        let expect = conformance_expect::load(&dir, name).unwrap();
        conformance_expect::check(&expect, &conformance_expect::ideal(&expect))
            .unwrap_or_else(|e| panic!("{name}: ideal outcome refused:\n{e}"));
        let mutations: [Mutation; 20] = [
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
                o.turns[0]
                    .observations
                    .push(json!({"kind": "turn.accepted"}));
                o.turns[0]
                    .observations
                    .push(json!({"kind": "action.denied"}));
            }),
            ("observation_payload", |o| {
                for observation in &mut o.turns[0].observations {
                    for (key, value) in observation.as_object_mut().into_iter().flatten() {
                        if key != "kind" {
                            *value = json!("changed");
                        }
                    }
                }
            }),
            ("exit", |o| {
                o.turns[0].exit = Some(json!({"code": 99, "signal": null}));
            }),
            ("journal_uncertain", |o| {
                o.turns[0].journal_uncertain = !o.turns[0].journal_uncertain;
            }),
            ("group_absent", |o| {
                o.turns[0].group_absent = !o.turns[0].group_absent;
            }),
            ("launch_checkpoints", |o| o.checkpoints.after_pure += 1),
            ("pure_writes", |o| o.pure_writes.push("changed".to_owned())),
            ("health", |o| {
                for health in o.health.values_mut() {
                    *health = json!({"state": "open", "first_cause": null});
                }
            }),
            ("gates", |o| {
                for gate in &mut o.turns[0].gates {
                    gate.accepted = !gate.accepted;
                }
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
                "launch_checkpoints" | "pure_writes" => expect.get(what).is_some(),
                "health" => expect["sessions"]
                    .as_object()
                    .is_some_and(|s| s.values().any(|s| s.get("health").is_some())),
                "gates" => expect["turns"][0]["gates"].as_array().is_some_and(|gates| {
                    gates.iter().any(|g| g["expect"].get("accepted").is_some())
                }),
                "observation_payload" => {
                    first["observations_include"]
                        .as_array()
                        .is_some_and(|entries| {
                            entries
                                .iter()
                                .any(|e| e.as_object().is_some_and(|e| e.len() > 1))
                        })
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
