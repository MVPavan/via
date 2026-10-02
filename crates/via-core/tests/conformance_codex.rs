//! Codex adapter conformance (`via-5lr.3.1`): one test per sanitized replay
//! fixture in `crates/via-adapters/tests/fixtures/codex/`.
//!
//! Each case pairs `<case>.replay.json` (the recorded `codex app-server`
//! exchange, replayed by the fake agent) with `<case>.expect.json` (what the
//! C2 driver must produce, in the unified expectation schema). [`drive`]
//! runs the pure half (describe, plan checks, each spawn's plan), then the
//! shared run half (`support/conformance_run.rs`): each planned turn through
//! the real Codex driver over Route, Wire and Host, against one replaying
//! fake per server launch. The cases X4 and X5 of `via-5lr.3.2` own stay
//! ignored until their chunks land.
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

#[path = "support/conformance_drive.rs"]
mod conformance_drive;
#[path = "support/conformance_expect.rs"]
mod conformance_expect;
#[path = "support/conformance_run.rs"]
mod conformance_run;

use std::path::{Path, PathBuf};

use conformance_expect::Outcome;
use serde_json::{Value, json};

/// The bead whose adapter makes these cases pass.
const ADAPTER_BEAD: &str = "via-5lr.3.2";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../via-adapters/tests/fixtures/codex")
}

/// Runs the case's sessions and turns through the Codex C2 driver against
/// the fake agent replaying `<case>.replay.json` (see the checker's module
/// docs for its obligations: `launches` from `<case>.launches`, the
/// replay's end judged by [`conformance_expect::replay_exit`] for every
/// launch, and the server's stdin close at an `await_eof` step).
fn drive(name: &str, expect: &Value, replay: &Path) -> Result<Outcome, String> {
    conformance_drive::Pure::run("codex", name, expect, replay)?
        .drive(expect, replay, conformance_run::Knobs::default())
        .map_err(|error| format!("{error} ({ADAPTER_BEAD} case {name})"))
}

fn check(name: &str) -> Result<(), String> {
    let dir = fixtures();
    let expect = conformance_expect::load(&dir, name)?;
    let outcome = drive(name, &expect, &dir.join(format!("{name}.replay.json")))?;
    conformance_expect::check(&expect, &outcome).map_err(|wrong| format!("{name}:\n{wrong}"))
}

macro_rules! cases {
    (green: $($green:ident),* $(,)?; red: $($red:ident = $why:literal),* $(,)?) => {
        /// Every case with a test below.
        const CASES: &[&str] = &[$(stringify!($green),)* $(stringify!($red)),*];

        mod conformance_codex_cases {
            $(
                #[test]
                fn $green() {
                    super::check(stringify!($green)).unwrap();
                }
            )*
            $(
                #[test]
                #[ignore = $why]
                fn $red() {
                    super::check(stringify!($red)).unwrap();
                }
            )*
        }
    };
}

cases! {
    green:
    c10_read_only_refused,
    c4b_workspace_write_refused,
    codex_bound_gate_refusals,
    c11_failed_command,
    c1_commentary_usage,
    c5_resume,
    c5_resume_missing,
    c6_cold_initialize,
    c7_bad_model,
    c7_effort_catalog,
    c8_auth,
    c9_output_schema;
    red:
    c0_server_lost = "red until via-5lr.3.2 X4 (server loss across sessions)",
    c2_steer = "red until via-5lr.3.2 X4 (native steer)",
    c3_interrupt_uncertain = "red until via-5lr.3.2 X4 (interrupt and P7)",
    c3_wall_interrupt = "red until via-5lr.3.2 X4 (the wall's soft stop)",
    c4_two_sessions = "red until via-5lr.3.2 X4 (leases across sessions)",
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
    let defects: [Defect; 96] = [
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
        // Review r2 #5: nested members are typed.
        ("describe model a boolean", "describe.params.model:", |e| {
            e["describe"] = json!({"params": {"model": true}, "launches": 0});
        }),
        ("describe cwd a number", "describe.params.cwd:", |e| {
            e["describe"] = json!({"params": {"harness": "codex", "cwd": 7}, "launches": 0});
        }),
        (
            "describe require a string",
            "describe.params.require:",
            |e| {
                e["describe"] =
                    json!({"params": {"harness": "codex", "require": "steer"}, "launches": 0});
            },
        ),
        (
            "describe without harness or model",
            "describe.params: harness or model is required",
            |e| {
                e["describe"] = json!({"params": {"cwd": "/w"}, "launches": 0});
            },
        ),
        ("progress model a string", ".progress.model:", |e| {
            e["turns"][0]["expect"]["observations_include"] =
                json!([{"kind": "progress", "model": "yes"}]);
        }),
        (
            "progress tools_started a number",
            ".progress.tools_started:",
            |e| {
                e["turns"][0]["expect"]["observations_include"] =
                    json!([{"kind": "progress", "tools_started": 123}]);
            },
        ),
        (
            "progress tools_started not pairs",
            ".progress.tools_started:",
            |e| {
                e["turns"][0]["expect"]["observations_include"] =
                    json!([{"kind": "progress", "tools_started": [["exec-1"]]}]);
            },
        ),
        ("denial kind unknown", ".action.denied.denial_kind:", |e| {
            e["turns"][0]["expect"]["observations_exclude"] =
                json!([{"kind": "action.denied", "denial_kind": "disk"}]);
        }),
        ("warning observation code unknown", ".warning.code:", |e| {
            e["turns"][0]["expect"]["observations_include"] =
                json!([{"kind": "warning", "code": "slow"}]);
        }),
        ("cost usd a string", "terminal.cost.usd:", |e| {
            e["turns"][0]["expect"]["terminal"]["cost"]["usd"] = json!("free");
        }),
        ("cost scope a boolean", "terminal.cost.scope:", |e| {
            e["turns"][0]["expect"]["terminal"]["cost"]["scope"] = json!(true);
        }),
        ("exit code a string", "expect.exit.code:", |e| {
            e["turns"][0]["expect"]["exit"] = json!({"code": "0", "signal": null});
        }),
        ("exit signal a boolean", "expect.exit.signal:", |e| {
            e["turns"][0]["expect"]["exit"] = json!({"code": null, "signal": true});
        }),
        ("usage from unknown", "expect.usage.from:", |e| {
            e["turns"][0]["expect"]["usage"]["from"] = json!("guess");
        }),
        ("usage scope unknown", "expect.usage.scope:", |e| {
            e["turns"][0]["expect"]["usage"]["scope"] = json!("forever");
        }),
        (
            "usage tokens a string",
            "expect.usage.output_tokens:",
            |e| {
                e["turns"][0]["expect"]["usage"]["output_tokens"] = json!("83");
            },
        ),
        ("stop fact a string", "expect.stop_facts.forced:", |e| {
            e["turns"][0]["expect"]["stop_facts"] =
                json!({"acknowledged": true, "forced": "no", "shared": false});
        }),
        // Review r3 #2 and #5: a usage sample is a C2 `UsageSample`, and
        // token provenance is never `estimated`.
        ("usage sample key a number", ".progress.usage.key:", |e| {
            e["turns"][0]["expect"]["observations_include"] =
                json!([{"kind": "progress", "usage": {"key": 123}}]);
        }),
        (
            "usage sample member unknown",
            ".progress.usage: unknown field invented_counter",
            |e| {
                e["turns"][0]["expect"]["observations_include"] =
                    json!([{"kind": "progress", "usage": {"key": 123, "invented_counter": 456}}]);
            },
        ),
        (
            "usage sample counter a string",
            ".progress.usage.output:",
            |e| {
                e["turns"][0]["expect"]["observations_include"] =
                    json!([{"kind": "progress", "usage": {"key": null, "output": "5"}}]);
            },
        ),
        (
            "usage provenance estimated",
            "expect.usage.provenance:",
            |e| {
                e["turns"][0]["expect"]["usage"]["provenance"] = json!("estimated");
            },
        ),
        // Review r2 #6: gates share the final expectations' enum rules.
        ("gate error unknown", "gates[0].expect.error:", |e| {
            e["turns"][0]["gates"] = json!([{"step": 1, "expect": {"error": "nonsense"}}]);
        }),
        (
            "gate terminal status unknown",
            "gates[0].expect.terminal.status:",
            |e| {
                e["turns"][0]["gates"] =
                    json!([{"step": 1, "expect": {"terminal": {"status": "banana"}}}]);
            },
        ),
        ("gate cleanup unknown", "gates[0].expect.cleanup:", |e| {
            e["turns"][0]["gates"] = json!([{"step": 1, "expect": {"cleanup": "done"}}]);
        }),
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

/// Green: a gate snapshot may catch cleanup still `pending`; a final
/// expectation may not (review r2 #6).
#[test]
fn conformance_codex_pending_cleanup_only_in_gates() {
    let mut expect = conformance_expect::load(&fixtures(), "c6_cold_initialize").unwrap();
    expect["turns"][0]["gates"][0]["expect"]["cleanup"] = json!("pending");
    conformance_expect::validate(&expect).unwrap();
    expect["turns"][0]["expect"]["cleanup"] = json!("pending");
    let refused = conformance_expect::validate(&expect);
    assert!(
        refused
            .as_ref()
            .is_err_and(|error| error.contains("turns[0].expect.cleanup:")),
        "{refused:?}"
    );
}

/// Green: a keyed C2 `UsageSample` validates (review r3 #2).
#[test]
fn conformance_codex_keyed_usage_samples_validate() {
    let mut expect = conformance_expect::load(&fixtures(), "c2_steer").unwrap();
    expect["turns"][0]["expect"]["observations_include"] = json!([{
        "kind": "progress",
        "usage": {"key": "call-1", "input": 10, "cached_input": null, "output": 2,
                  "reasoning_output": 0, "total": 12}
    }]);
    conformance_expect::validate(&expect).unwrap();
}

/// Green: a gate whose snapshot holds identity before any acceptance
/// validates; one that states acceptance still needs the order (review r3
/// #3).
#[test]
fn conformance_codex_identity_only_gate_prefix_validates() {
    let mut expect = conformance_expect::load(&fixtures(), "c2_steer").unwrap();
    expect["turns"][0]["gates"] = json!([{"step": 1, "expect": {
        "accepted": false,
        "observations_order": ["session.vendor_identity_confirmed"]
    }}]);
    conformance_expect::validate(&expect).unwrap();
    // Acceptance stated in the same snapshot still needs the order.
    expect["turns"][0]["gates"][0]["expect"]["observations_order"] =
        json!(["turn.accepted", "session.vendor_identity_confirmed"]);
    let refused = conformance_expect::validate(&expect);
    assert!(
        refused
            .as_ref()
            .is_err_and(|error| error.contains("must precede")),
        "{refused:?}"
    );
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

/// Green: the shared replay-exit check accepts each fixture's own end and
/// refuses a signal death, the replay's failure code and any other end
/// (review r2 #8). [`drive`] calls it for every launch.
#[test]
fn conformance_codex_replay_exit_is_judged() {
    let checked = conformance_expect::replay_exit_self_check(&fixtures()).unwrap();
    assert!(checked > 0, "no replay lifetimes checked");
}

/// Ruling 21 (x.3.2 X3 fix r1): every build judges each server launch by
/// the replay's own verdict. A replay still waiting for a line VIA never
/// sends ends failed at VIA's stdin close, and the case fails with it even
/// though every turn's outcome holds.
#[test]
fn conformance_codex_server_verdict_is_judged() {
    let name = "conformance_codex_server_verdict_is_judged";
    let (mut replay, expect) = plain(name).unwrap();
    let all = steps(&mut replay).unwrap();
    let last = all.len() - 1;
    all.insert(last, json!({"expect": {"line": {"method": "never/sent"}}}));
    let verdict = variant(name, &replay, &expect).unwrap_err();
    assert!(verdict.contains("server launch 1"), "{verdict}");
}

/// Ruling 21 (x.3.2 X3 fix r1): the harness's own close of a session whose
/// case states none is checked. A healthy session left uncertain by its
/// interrupted turn (a tool still open) fails the case unless its close is
/// stated.
#[test]
fn conformance_codex_shutdown_close_is_checked() {
    let name = "conformance_codex_shutdown_close_is_checked";
    let mut replay = replay_of("c3_interrupt_uncertain").unwrap();
    let mut expect = expect_of("c3_interrupt_uncertain").unwrap();
    replay["source"] = json!(format!("{name}: a variant of c3_interrupt_uncertain"));
    expect["source"] = replay["source"].clone();
    let turn = &mut turn_mut(&mut expect, 0)["expect"];
    let fields = turn.as_object_mut().unwrap();
    fields.remove("cleanup_settles");
    fields.remove("stop_facts");
    let why = "the interrupt's acknowledgement and the P7 window are x.3.2 X4's";
    turn["unasserted"].as_array_mut().unwrap().extend([
        json!({"field": "cleanup_settles", "why": why}),
        json!({"field": "stop_facts", "why": why}),
    ]);
    variant(name, &replay, &expect).unwrap();
    expect["sessions"]["main"]["close"] = Value::Null;
    let unchecked = variant(name, &replay, &expect).unwrap_err();
    assert!(
        unchecked.contains("shutdown close: cleanup uncertain"),
        "{unchecked}"
    );
}

// Packet §8's named tests (`test(/^codex_/)` selects them), x.3.2 X3's
// part. Each runs variants of a recorded fixture, built here and written
// only to a scratch directory, never under the fixtures.

/// The base of most variants: c6 without its gate, so the cold handshake
/// is answered at once and one turn completes.
const PLAIN: &str = "c6_cold_initialize";

/// The base's thread and turn IDs.
const THREAD: &str = "019a0000-0000-7000-8000-000000100001";
const TURN: &str = "019a0000-0000-7000-8000-000000200001";

/// Loads fixture `name`'s replay.
fn replay_of(name: &str) -> Result<Value, String> {
    let text = std::fs::read(fixtures().join(format!("{name}.replay.json")))
        .map_err(|e| format!("{name}: {e}"))?;
    serde_json::from_slice(&text).map_err(|e| format!("{name}: {e}"))
}

/// Loads fixture `name`'s expectation.
fn expect_of(name: &str) -> Result<Value, String> {
    conformance_expect::load(&fixtures(), name)
}

/// [`PLAIN`] with its gate removed: the replay without the `await_signal`
/// step (its `after_emit` renumbered) and the expectation without gates.
fn plain(variant: &str) -> Result<(Value, Value), String> {
    let mut replay = replay_of(PLAIN)?;
    let all = steps(&mut replay)?;
    if all
        .get(1)
        .and_then(|step| step.get("await_signal"))
        .is_none()
    {
        return Err(format!("{PLAIN}: step 2 is not its gate"));
    }
    all.remove(1);
    for step in all.iter_mut() {
        if let Some(after) = step["expect"]["after_emit"].as_u64() {
            step["expect"]["after_emit"] = json!(after - 1);
        }
    }
    replay["source"] = json!(format!("{variant}: a variant of {PLAIN} without its gate"));
    replay["notes"] = json!(
        "Step 10 (turn/start) names step 8, the thread/start reply, as its causal \
         predecessor (after_emit)."
    );
    replay["deadline_ms"] = json!(20000);
    let mut expect = expect_of(PLAIN)?;
    expect["source"] = replay["source"].clone();
    let turn = &mut expect["turns"][0];
    if let Some(turn) = turn.as_object_mut() {
        turn.remove("gates");
    }
    turn["expect"]["notes"] = json!(format!("{variant}; see its test"));
    Ok((replay, expect))
}

/// Turn `index` of an expectation.
fn turn_mut(expect: &mut Value, index: usize) -> &mut Value {
    &mut expect["turns"][index]
}

/// The steps of a replay.
fn steps(replay: &mut Value) -> Result<&mut Vec<Value>, String> {
    replay["steps"]
        .as_array_mut()
        .ok_or_else(|| "no steps".to_owned())
}

/// The index of the first step whose emitted line, or else whose JSON
/// text, contains `marker`.
fn step_with(replay: &Value, marker: &str) -> Result<usize, String> {
    replay["steps"]
        .as_array()
        .ok_or("no steps")?
        .iter()
        .position(|step| match step["emit"]["line"].as_str() {
            Some(line) => line.contains(marker),
            None => step.to_string().contains(marker),
        })
        .ok_or_else(|| format!("no step with {marker}"))
}

/// The line of emit step `at`.
fn line_of(replay: &Value, at: usize) -> Result<String, String> {
    replay["steps"][at]["emit"]["line"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("step {at} is no emit"))
}

/// An emit step of `line`.
fn emit(line: &Value) -> Value {
    json!({"emit": {"line": line.to_string()}})
}

/// Replaces `from` with `to` in the line of emit step `at`.
fn edit_emit(replay: &mut Value, at: usize, from: &str, to: &str) -> Result<(), String> {
    let line = line_of(replay, at)?;
    if !line.contains(from) {
        return Err(format!("step {at}: no {from}"));
    }
    replay["steps"][at]["emit"]["line"] = json!(line.replace(from, to));
    Ok(())
}

/// Cuts the replay after step `at` and ends it with `tail`.
fn cut_after(replay: &mut Value, at: usize, tail: &[Value]) -> Result<(), String> {
    let steps = steps(replay)?;
    steps.truncate(at + 1);
    steps.extend(tail.iter().cloned());
    Ok(())
}

/// The expectation of a turn that failed before acceptance: nothing
/// observed but what the caller adds.
fn unaccepted(expect: &mut Value, error: &str, instance: Value) {
    let turn = &mut expect["turns"][0]["expect"];
    turn["accepted"] = json!(false);
    turn["terminal"] = Value::Null;
    turn["usage"] = Value::Null;
    turn["final_text"] = Value::Null;
    turn["error"] = json!(error);
    turn["instance"] = instance;
    turn["observations_include"] = json!([]);
    turn["observations_exclude"] = json!(["turn.accepted", "final_text"]);
    turn["observation_counts"] = json!({"turn.accepted": 0});
    turn["observations_order"] = json!([]);
}

/// The base's instance.
fn tested() -> Value {
    json!({"vendor_version": "0.159.2", "version_status": "tested"})
}

/// Runs a variant: `replay` and `expect` as the test built them, from a
/// scratch directory.
fn check_variant(
    name: &str,
    replay: &Value,
    expect: &Value,
    knobs: conformance_run::Knobs,
) -> Result<(), String> {
    conformance_expect::validate(expect).map_err(|e| format!("{name}: {e}"))?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join(format!("{name}.replay.json"));
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(replay).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let outcome = conformance_drive::Pure::run("codex", name, expect, &path)?
        .drive(expect, &path, knobs)
        .map_err(|why| format!("{why} ({ADAPTER_BEAD} variant {name})"))?;
    conformance_expect::check(expect, &outcome).map_err(|wrong| format!("{name}:\n{wrong}"))
}

/// [`check_variant`] with the default knobs, then `then` on the adapter
/// set the case ran on, once every turn settled.
fn check_variant_then(
    name: &str,
    replay: &Value,
    expect: &Value,
    then: impl FnOnce(&conformance_drive::Pure) -> Result<(), String>,
) -> Result<(), String> {
    conformance_expect::validate(expect).map_err(|e| format!("{name}: {e}"))?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join(format!("{name}.replay.json"));
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(replay).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let outcome = conformance_drive::Pure::run("codex", name, expect, &path)?
        .drive_then(expect, &path, conformance_run::Knobs::default(), then)
        .map_err(|why| format!("{why} ({ADAPTER_BEAD} variant {name})"))?;
    conformance_expect::check(expect, &outcome).map_err(|wrong| format!("{name}:\n{wrong}"))
}

/// C2 §5 AD7 (x.3.2 X3 fix r1, finding 14): once a handshake check
/// refused the recipe, the next identical spawn's plan refuses it,
/// `harness_unavailable` with `reason: "handshake_refused"`, before any
/// receipt or launch.
fn refused_from_cache(pure: &conformance_drive::Pure) -> Result<(), String> {
    let request = via_adapters::DescribeRequest {
        harness: Some("codex".to_owned()),
        model: Some("gpt-6-sol".to_owned()),
        cwd: Some("/work/project".into()),
        ..via_adapters::DescribeRequest::default()
    };
    let plan = pure.set.plan(&request).map_err(|e| format!("{e:?}"))?;
    let plan = serde_json::to_value(&plan).map_err(|e| e.to_string())?;
    let refusal = &plan["refusals"][0];
    if plan["version_status"] != "refused"
        || refusal["kind"] != "harness_unavailable"
        || refusal["reason"] != "handshake_refused"
    {
        return Err(format!("no cached refusal: {plan}"));
    }
    Ok(())
}

/// [`check_variant`] with the default knobs.
fn variant(name: &str, replay: &Value, expect: &Value) -> Result<(), String> {
    check_variant(name, replay, expect, conformance_run::Knobs::default())
}

/// The step that seals a force-closed server's run: Host's close sends
/// the group `SIGTERM`, which the fake takes and then exits on its own.
fn sigterm() -> Value {
    json!({"await_signal": {"signal": "SIGTERM"}})
}

/// C2 §5, C1 §3.13 (x.3.2 X3 fix r1, finding 17): once the live server
/// discovered its catalog, `models --harness codex` lists it as
/// `discovered`, a model-only plan resolves Codex from it, and a plan
/// naming no model takes its default. Before discovery there is none.
/// The session stays open until shutdown, so its server is live when the
/// checks run (fix r2 #11: a retired instance's catalog is gone).
#[test]
fn codex_discovery_feeds_models() {
    let name = "codex_discovery_feeds_models";
    let (replay, mut expect) = plain(name).unwrap();
    expect["sessions"]["main"]["close"] = Value::Null;
    let pure = |set: &via_adapters::AdapterSet| -> Result<(), String> {
        let models = serde_json::to_value(set.models(Some("codex"))).map_err(|e| e.to_string())?;
        let want = json!([
            {"model": "gpt-6.1-sol", "harness": "codex", "aliases": [], "source": "discovered"},
            {"model": "gpt-6-sol", "harness": "codex", "aliases": [], "source": "discovered"},
            {"model": "gpt-6-luna", "harness": "codex", "aliases": [], "source": "discovered"},
        ]);
        if models != want {
            return Err(format!("models: {models}"));
        }
        let only_model = via_adapters::DescribeRequest {
            model: Some("gpt-6-luna".to_owned()),
            ..via_adapters::DescribeRequest::default()
        };
        let plan = set
            .plan(&only_model)
            .map_err(|e| format!("model-only: {e:?}"))?;
        if plan.harness != "codex" {
            return Err(format!("model-only resolved {}", plan.harness));
        }
        let no_model = via_adapters::DescribeRequest {
            harness: Some("codex".to_owned()),
            ..via_adapters::DescribeRequest::default()
        };
        let plan = set
            .plan(&no_model)
            .map_err(|e| format!("no model: {e:?}"))?;
        if plan.model.resolved != "gpt-6.1-sol" {
            return Err(format!("default: {}", plan.model.resolved));
        }
        Ok(())
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("{name}.replay.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&replay).unwrap()).unwrap();
    let before = conformance_drive::Pure::run("codex", name, &expect, &path).unwrap();
    assert!(
        before.set.models(Some("codex")).is_empty(),
        "none before discovery"
    );
    let outcome = before
        .drive_then(&expect, &path, conformance_run::Knobs::default(), |run| {
            pure(&run.set)
        })
        .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
}

/// F13 (packet §8 `codex_pin_handshake`): one initialize/initialized per
/// connection, with neither an experimental capability nor an opt-out
/// (every fixture's first step pins their absence); the version comes
/// from `userAgent`, and one outside `checked` proceeds as untested; a
/// malformed handshake and a policy, sandbox, model or cwd echo mismatch refuse the
/// turn (`protocol`) before any `turn/start`; a `model/list` cursor left at
/// the page bound or past the byte bound fails discovery as `protocol`.
#[test]
fn codex_pin_handshake() {
    let first = &replay_of(PLAIN).unwrap()["steps"][0]["expect"];
    assert_eq!(first["line"]["method"], "initialize");
    assert_eq!(
        first["absent"],
        json!([
            "/params/capabilities/experimentalApi",
            "/params/capabilities/optOutNotificationMethods"
        ])
    );

    // An untested version proceeds, reported as such.
    let (mut replay, mut expect) = plain("codex_pin_handshake_untested").unwrap();
    edit_emit(&mut replay, 1, "via/0.159.2", "via/0.160.0").unwrap();
    turn_mut(&mut expect, 0)["expect"]["instance"] =
        json!({"vendor_version": "0.160.0", "version_status": "untested"});
    variant("codex_pin_handshake_untested", &replay, &expect).unwrap();

    // A malformed initialize reply: no userAgent.
    let (mut replay, mut expect) = plain("codex_pin_handshake_malformed").unwrap();
    steps(&mut replay).unwrap()[1] = json!({"emit": {"line":
        "{\"id\":${init},\"result\":{\"codexHome\":\"/state/codex-home\"}}"}});
    cut_after(&mut replay, 1, &[sigterm()]).unwrap();
    unaccepted(&mut expect, "protocol", Value::Null);
    variant("codex_pin_handshake_malformed", &replay, &expect).unwrap();

    // A policy, then a sandbox, echo that is not the one requested.
    for (name, from, to) in [
        (
            "codex_pin_handshake_policy_echo",
            "\"approvalPolicy\":\"never\"",
            "\"approvalPolicy\":\"on-request\"",
        ),
        (
            "codex_pin_handshake_sandbox_echo",
            "\"sandbox\":{\"type\":\"dangerFullAccess\"}",
            "\"sandbox\":{\"type\":\"readOnly\"}",
        ),
        // The thread reply's own model and cwd, not the thread record's
        // (packet §3 readback; x.3.2 X3 fix r1, finding 13).
        (
            "codex_pin_handshake_model_echo",
            "\"model\":\"gpt-6-sol\",\"modelProvider\":\"openai\",\"serviceTier\"",
            "\"model\":\"gpt-6-luna\",\"modelProvider\":\"openai\",\"serviceTier\"",
        ),
        (
            "codex_pin_handshake_cwd_echo",
            "\"disabledPluginIds\":[],\"cwd\":\"/work/project\"",
            "\"disabledPluginIds\":[],\"cwd\":\"/work/elsewhere\"",
        ),
    ] {
        let (mut replay, mut expect) = plain(name).unwrap();
        let answer = step_with(&replay, "\"result\":{\"thread\"").unwrap();
        edit_emit(&mut replay, answer, from, to).unwrap();
        cut_after(&mut replay, answer, &[json!({"await_eof": {}})]).unwrap();
        // `HandshakeRefused`, which Core reports as `submit_failed` with
        // `failure.data.reason: "handshake_refused"` (C1 §5; Core's mapping
        // is pinned by `core_handshake_refused_is_submit_failed`), never
        // `protocol` (x.3.2 X3 fix r1, finding 22).
        unaccepted(&mut expect, "handshake_refused", tested());
        check_variant_then(name, &replay, &expect, refused_from_cache).unwrap();
    }

    // model/list: a cursor left at the page bound, then a catalog past
    // the byte bound. `initialize` was read first: its instance stays on
    // the failed outcome (C2 AD7; x.3.2 X3 fix r1, finding 18).
    let (mut replay, mut expect) = plain("codex_pin_handshake_page_bound").unwrap();
    let models = step_with(&replay, "\"result\":{\"data\"").unwrap();
    let page = replay["steps"][models]["emit"]["line"]
        .as_str()
        .unwrap()
        .replace("\"nextCursor\":null", "\"nextCursor\":\"next\"");
    assert!(page.contains("\"nextCursor\":\"next\""));
    let mut pages = Vec::new();
    for n in 0..16 {
        if n > 0 {
            pages.push(json!({"expect": {
                "line": {"method": "model/list", "params": {"cursor": "next"}},
                "capture": {"models": "/id"},
            }}));
        }
        pages.push(json!({"emit": {"line": page}}));
    }
    let tail: Vec<Value> = pages.into_iter().chain([sigterm()]).collect();
    cut_after(&mut replay, models - 1, &tail).unwrap();
    unaccepted(&mut expect, "protocol", tested());
    variant("codex_pin_handshake_page_bound", &replay, &expect).unwrap();

    let (mut replay, _) = plain("codex_pin_handshake_byte_bound").unwrap();
    let big = "x".repeat(600 * 1024);
    let mut huge: Value = serde_json::from_str(&page.replace("${models}", "0")).unwrap();
    huge["result"]["data"][0]["description"] = json!(big);
    let huge = huge.to_string().replacen("\"id\":0", "\"id\":${models}", 1);
    let tail = [
        json!({"emit": {"line": huge}}),
        json!({"expect": {
            "line": {"method": "model/list", "params": {"cursor": "next"}},
            "capture": {"models": "/id"},
        }}),
        json!({"emit": {"line": huge}}),
        sigterm(),
    ];
    cut_after(&mut replay, models - 1, &tail).unwrap();
    let mut expect = expect.clone();
    expect["source"] = replay["source"].clone();
    variant("codex_pin_handshake_byte_bound", &replay, &expect).unwrap();
}

/// The expectation of a turn accepted and then failed with `error`, before
/// any terminal.
fn failed_after_acceptance(expect: &mut Value, error: &str, cleanup: &str) {
    let turn = &mut turn_mut(expect, 0)["expect"];
    turn["terminal"] = Value::Null;
    turn["usage"] = Value::Null;
    turn["final_text"] = Value::Null;
    turn["error"] = json!(error);
    turn["cleanup"] = json!(cleanup);
    turn["observations_include"] = json!([
        {"kind": "turn.accepted", "vendor_turn_id": TURN},
    ]);
    turn["observations_exclude"] = json!(["final_text"]);
    turn["observations_order"] = json!(["session.vendor_identity_confirmed", "turn.accepted"]);
}

/// F18 (packet §8 `codex_start_order`): a notification before the paired
/// `turn/start` reply is buffered and the reply accepts once; a reply under
/// an unknown, a duplicate or a mismatched (string) ID fails the connection
/// `protocol`; a start the server never answered is no acceptance, and
/// one cancelled unanswered leaves its cleanup unproven; a
/// malformed known notification of the turn fails it `protocol`; a second,
/// contradictory `turn/completed` never replaces the retained terminal.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one variant per F18 ordering, each a few lines"
)]
fn codex_start_order() {
    let reply_marker = "\"result\":{\"turn\"";
    let started_marker = "\"method\":\"turn/started\"";
    let completed_marker = "\"method\":\"turn/completed\"";

    // turn/started (and the status change) before the paired reply.
    let (mut replay, expect) = plain("codex_start_order_early").unwrap();
    let answer = step_with(&replay, reply_marker).unwrap();
    let started = step_with(&replay, started_marker).unwrap();
    let moved: Vec<Value> = steps(&mut replay)
        .unwrap()
        .drain(answer + 1..=started)
        .collect();
    for (offset, step) in moved.into_iter().enumerate() {
        steps(&mut replay).unwrap().insert(answer + offset, step);
    }
    assert!(
        step_with(&replay, started_marker).unwrap() < step_with(&replay, reply_marker).unwrap()
    );
    variant("codex_start_order_early", &replay, &expect).unwrap();

    // Replies the connection cannot pair, mid-turn.
    for (name, line) in [
        (
            "codex_start_order_unknown_id",
            "{\"id\":9999,\"result\":{}}".to_owned(),
        ),
        ("codex_start_order_duplicate_id", {
            let base = replay_of(PLAIN).unwrap();
            let at = step_with(&base, "\"result\":{\"thread\"").unwrap();
            base["steps"][at]["emit"]["line"]
                .as_str()
                .unwrap()
                .to_owned()
        }),
    ] {
        let (mut replay, mut expect) = plain(name).unwrap();
        let started = step_with(&replay, started_marker).unwrap();
        cut_after(
            &mut replay,
            started,
            &[json!({"emit": {"line": line}}), sigterm()],
        )
        .unwrap();
        failed_after_acceptance(&mut expect, "protocol", "quiescent");
        variant(name, &replay, &expect).unwrap();
    }

    // The turn/start reply under a string ID: never paired.
    let (mut replay, mut expect) = plain("codex_start_order_mismatched_id").unwrap();
    let answer = step_with(&replay, reply_marker).unwrap();
    edit_emit(
        &mut replay,
        answer,
        "{\"id\":${turn},",
        "{\"id\":\"${turn}\",",
    )
    .unwrap();
    cut_after(&mut replay, answer, &[sigterm()]).unwrap();
    unaccepted(&mut expect, "protocol", tested());
    turn_mut(&mut expect, 0)["expect"]["observations_include"] = json!([
        {"kind": "session.vendor_identity_confirmed", "vendor_session_id": THREAD,
            "generation": 1},
    ]);
    variant("codex_start_order_mismatched_id", &replay, &expect).unwrap();

    // The start is written once and never answered: the server exits.
    let (mut replay, mut expect) = plain("codex_start_order_lost_start").unwrap();
    let start = step_with(&replay, "\"method\":\"turn/start\"").unwrap();
    cut_after(
        &mut replay,
        start,
        &[json!({"exit": {"code": 0, "stderr": ""}})],
    )
    .unwrap();
    unaccepted(&mut expect, "server_lost", tested());
    variant("codex_start_order_lost_start", &replay, &expect).unwrap();

    // A cancel while the written start is unanswered: the vendor may run
    // the turn, so its cleanup stays unproven (P7's acknowledgement is
    // X4's), whatever the server reported.
    let (mut replay, mut expect) = plain("codex_start_order_stopped_unanswered").unwrap();
    let start = step_with(&replay, "\"method\":\"turn/start\"").unwrap();
    let close = step_with(&replay, "\"method\":\"thread/unsubscribe\"").unwrap();
    let tail: Vec<Value> = replay["steps"].as_array().unwrap()[close..].to_vec();
    cut_after(&mut replay, start, &tail).unwrap();
    unaccepted(&mut expect, "", tested());
    let turn = turn_mut(&mut expect, 0);
    turn["stop"] = json!({"kind": "interrupt", "after": "handshake"});
    turn["expect"]["error"] = Value::Null;
    turn["expect"]["cleanup"] = json!("uncertain");
    turn["expect"]["stop_facts"] = json!({"acknowledged": false, "forced": false, "shared": true});
    turn["expect"]["observations_include"] = json!([
        {"kind": "session.vendor_identity_confirmed", "vendor_session_id": THREAD,
            "generation": 1},
    ]);
    // The session's close never erases that uncertainty (x.3.2 X3 fix r1,
    // finding 6): unsubscribing proves detachment, not tool cleanup.
    expect["sessions"]["main"]["close"]["cleanup"] = json!("uncertain");
    variant("codex_start_order_stopped_unanswered", &replay, &expect).unwrap();

    // The start's reply is lost while the server lives (x.3.2 X3 fix r1,
    // test strength): nothing proves the vendor's turn absent, so the
    // turn ends at its wall unaccepted with its cleanup unproven, and the
    // close keeps that uncertainty.
    let name = "codex_start_order_lost_reply_live_server";
    let (mut replay, mut expect) = plain(name).unwrap();
    let start = step_with(&replay, "\"method\":\"turn/start\"").unwrap();
    let close = step_with(&replay, "\"method\":\"thread/unsubscribe\"").unwrap();
    let tail: Vec<Value> = replay["steps"].as_array().unwrap()[close..].to_vec();
    cut_after(&mut replay, start, &tail).unwrap();
    unaccepted(&mut expect, "deadline", tested());
    let turn = turn_mut(&mut expect, 0);
    turn["deadlines"] = json!({"wall_ms": 1500, "idle_ms": 600_000});
    turn["expect"]["cleanup"] = json!("uncertain");
    turn["expect"]["observations_include"] = json!([
        {"kind": "session.vendor_identity_confirmed", "vendor_session_id": THREAD,
            "generation": 1},
    ]);
    expect["sessions"]["main"]["close"]["cleanup"] = json!("uncertain");
    variant(name, &replay, &expect).unwrap();

    // A malformed turn/completed of the turn, its correlation intact (X0
    // item 5 step 5): the turn fails `protocol`, the connection lives.
    let (mut replay, mut expect) = plain("codex_start_order_malformed").unwrap();
    let completed = step_with(&replay, completed_marker).unwrap();
    steps(&mut replay).unwrap()[completed] = emit(&json!({
        "method": "turn/completed",
        "params": {"threadId": THREAD, "turn": {"id": TURN, "status": 7}},
    }));
    cut_after(&mut replay, completed, &[json!({"await_eof": {}})]).unwrap();
    let base = turn_mut(&mut expect, 0)["expect"].clone();
    failed_after_acceptance(&mut expect, "protocol", "quiescent");
    // What the turn delivered before the malformed message stays.
    turn_mut(&mut expect, 0)["expect"]["usage"] = base["usage"].clone();
    turn_mut(&mut expect, 0)["expect"]["final_text"] = base["final_text"].clone();
    turn_mut(&mut expect, 0)["expect"]["observations_include"] = json!([
        {"kind": "turn.accepted", "vendor_turn_id": TURN}, "final_text",
    ]);
    turn_mut(&mut expect, 0)["expect"]["observations_exclude"] = json!([]);
    let malformed = expect.clone();
    variant("codex_start_order_malformed", &replay, &expect).unwrap();

    // A turn/completed whose turn names no ID: its correlation fails, so
    // the whole connection fails `protocol` and Host stops the server (X0
    // item 5 step 2; x.3.2 X3 fix r1, finding 9).
    let (mut replay, _) = plain("codex_start_order_uncorrelated").unwrap();
    let completed = step_with(&replay, completed_marker).unwrap();
    steps(&mut replay).unwrap()[completed] = emit(&json!({
        "method": "turn/completed",
        "params": {"threadId": THREAD, "turn": "not a turn"},
    }));
    cut_after(&mut replay, completed, &[sigterm()]).unwrap();
    let mut expect = malformed;
    expect["source"] = replay["source"].clone();
    expect["sessions"]["main"]["close"] = json!({
        "mode": "graceful", "vendor_closed": false, "cleanup": "quiescent",
    });
    variant("codex_start_order_uncorrelated", &replay, &expect).unwrap();

    // A second, contradictory turn/completed after the terminal.
    let (mut replay, mut expect) = plain("codex_start_order_second_terminal").unwrap();
    let completed = step_with(&replay, completed_marker).unwrap();
    let second = replay["steps"][completed]["emit"]["line"]
        .as_str()
        .unwrap()
        .replace("\"status\":\"completed\"", "\"status\":\"failed\"");
    steps(&mut replay)
        .unwrap()
        .insert(completed + 1, json!({"emit": {"line": second}}));
    turn_mut(&mut expect, 0)["expect"]["observation_counts"] =
        json!({"turn.accepted": 1, "turn.late_terminal": 0});
    variant("codex_start_order_second_terminal", &replay, &expect).unwrap();
}

/// Packet §8 `codex_resume_identity`: the persistent thread is reopened
/// under its exact ID with `excludeTurns` (`c5_resume`), a thread the vendor
/// no longer has is gone (`c5_resume_missing`), and a reply naming another
/// thread fails `resume_mismatch` before acceptance, with
/// `resume.mismatch`, no fallback `thread/start` and the session failed.
#[test]
fn codex_resume_identity() {
    let resume = &replay_of("c5_resume").unwrap()["steps"];
    let at = step_with(
        &replay_of("c5_resume").unwrap(),
        "\"method\":\"thread/resume\"",
    )
    .unwrap();
    assert_eq!(resume[at]["expect"]["line"]["params"]["threadId"], THREAD);
    assert_eq!(resume[at]["expect"]["line"]["params"]["excludeTurns"], true);
    check("c5_resume").unwrap();
    check("c5_resume_missing").unwrap();

    let name = "codex_resume_identity_mismatch";
    let other = "019a0000-0000-7000-8000-000000100009";
    let mut replay = replay_of("c5_resume").unwrap();
    replay["source"] = json!(format!("{name}: a variant of c5_resume"));
    let answer = step_with(&replay, "\"result\":{\"thread\"").unwrap();
    let line = replay["steps"][answer]["emit"]["line"]
        .as_str()
        .unwrap()
        .replace(THREAD, other);
    steps(&mut replay).unwrap()[answer] = json!({"emit": {"line": line}});
    cut_after(&mut replay, answer, &[json!({"await_eof": {}})]).unwrap();
    let mut expect = expect_of("c5_resume").unwrap();
    expect["source"] = replay["source"].clone();
    // Not closed, so its health shows the failure.
    expect["sessions"]["main"]["close"] = Value::Null;
    expect["sessions"]["main"]["health"] =
        json!({"state": "failed", "first_cause": "resume_mismatch"});
    unaccepted(&mut expect, "resume_mismatch", tested());
    let turn = &mut turn_mut(&mut expect, 0)["expect"];
    turn["observations_include"] = json!([
        {"kind": "resume.mismatch", "requested": THREAD, "returned": other},
    ]);
    turn["observations_exclude"] = json!([
        "session.vendor_identity_confirmed",
        "turn.accepted",
        "final_text",
    ]);
    turn["notes"] = json!(name);
    variant(name, &replay, &expect).unwrap();
}

/// F14 (packet §8 `codex_never_ask`): each server request is answered
/// under its exact incoming ID (integer or string) within 5 s of its
/// decode (the replay allows 5,250 ms for the pipes): the six declined
/// methods with their no-grant bodies (validated against the pinned
/// schemas by the adapter's unit test), and auth refresh, attestation,
/// the legacy approvals and an unknown method with `-32601`. Core takes no
/// observation until the fake has read the last reply, so the replies
/// wait on neither the driver's delivery nor Core. Each written reply is
/// reported `vendor.request_declined` once.
#[test]
fn codex_never_ask() {
    let declined = [
        (
            "item/commandExecution/requestApproval",
            json!({"decision": "decline"}),
        ),
        (
            "item/fileChange/requestApproval",
            json!({"decision": "decline"}),
        ),
        (
            "item/permissions/requestApproval",
            json!({"permissions": {}}),
        ),
        ("item/tool/requestUserInput", json!({"answers": {}})),
        (
            "mcpServer/elicitation/request",
            json!({"action": "decline", "content": null}),
        ),
        (
            "item/tool/call",
            json!({"contentItems": [], "success": false}),
        ),
    ];
    let refused = [
        "account/chatgptAuthTokens/refresh",
        "attestation/generate",
        "applyPatchApproval",
        "execCommandApproval",
        "item/unknown/request",
    ];
    let (mut replay, mut expect) = plain("codex_never_ask").unwrap();
    let started = step_with(&replay, "\"method\":\"turn/started\"").unwrap();
    let mut injected = Vec::new();
    for (n, (method, body)) in declined.iter().enumerate() {
        let id = json!(70 + n);
        injected.push(emit(&json!({"id": id, "method": method, "params": {
            "threadId": THREAD, "turnId": TURN, "itemId": format!("item-{n}"),
        }})));
        injected.push(json!({"expect": {
            "line": {"id": id, "result": body},
            "absent": ["/error"],
            "within_ms": 5250,
        }}));
    }
    for (n, method) in refused.iter().enumerate() {
        let id = json!(format!("s-{n}"));
        injected.push(emit(&json!({"id": id, "method": method, "params": {
            "threadId": THREAD, "turnId": TURN,
        }})));
        injected.push(json!({"expect": {
            "line": {"id": id, "error": {"code": -32601,
                "message": "Method not supported by VIA"}},
            "absent": ["/result"],
            "within_ms": 5250,
        }}));
    }
    let count = injected.len() / 2;
    for (offset, step) in injected.into_iter().enumerate() {
        steps(&mut replay)
            .unwrap()
            .insert(started + 1 + offset, step);
    }
    let turn = &mut turn_mut(&mut expect, 0)["expect"];
    turn["observation_counts"] = json!({"turn.accepted": 1, "vendor.request_declined": count});
    turn["observations_include"] = json!([
        {"kind": "vendor.request_declined",
            "vendor_method": "item/commandExecution/requestApproval", "blocking": true},
        {"kind": "vendor.request_declined", "vendor_method": "item/unknown/request"},
    ]);
    // initialize, initialized, model/list, thread/start, turn/start, then
    // the replies.
    let knobs = conformance_run::Knobs {
        hold_until_read: Some(5 + count),
        ..conformance_run::Knobs::default()
    };
    check_variant("codex_never_ask", &replay, &expect, knobs).unwrap();
}

/// c1's second vendor turn.
const TURN2: &str = "019a0000-0000-7000-8000-000000200002";

/// `c1_commentary_usage` with `inserted` emitted right after its second
/// turn started, cut after them with `tail`; turn 2 loses its gate.
fn c1_turn2_with(name: &str, inserted: &[Value], tail: &[Value]) -> Result<(Value, Value), String> {
    let mut replay = replay_of("c1_commentary_usage")?;
    let mut expect = expect_of("c1_commentary_usage")?;
    replay["source"] = json!(format!("{name}: a variant of c1_commentary_usage"));
    expect["source"] = replay["source"].clone();
    let started = step_with(
        &replay,
        &format!(
            "\"turn/started\",\"params\":{{\"threadId\":\"{THREAD}\",\"turn\":{{\"id\":\"{TURN2}\""
        ),
    )?;
    let all = steps(&mut replay)?;
    // Turn 2's gate goes with its expectation.
    let gate = all[started..]
        .iter()
        .position(|step| step.get("await_signal").is_some())
        .ok_or("turn 2 has no gate")?;
    all.remove(started + gate);
    for (offset, step) in inserted.iter().enumerate() {
        all.insert(started + 1 + offset, step.clone());
    }
    if !tail.is_empty() {
        cut_after(&mut replay, started + inserted.len(), tail)?;
    }
    turn_mut(&mut expect, 1)
        .as_object_mut()
        .ok_or("turn 2")?
        .remove("gates");
    Ok((replay, expect))
}

/// Every `undecoded.bin` under `root`, relative to it.
fn undecoded_under(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.file_name().is_some_and(|name| name == "undecoded.bin") {
                let relative = path.strip_prefix(root).unwrap_or(&path);
                found.push(relative.display().to_string());
            }
        }
    }
    found.sort();
    found
}

/// X0 item 5 steps 5 and 6 (x.3.2 X3 fix r1, finding 10): a malformed
/// message is evidence of the turn it names, else of the server, never of
/// whichever turn runs. While turn 2 runs, a malformed `turn/completed`
/// naming turn 1 is kept in turn 1's folder, and a malformed thread-level
/// notification in the server folder; either way the generation fails
/// turn 2 `protocol`.
#[test]
fn codex_malformed_evidence_owner() {
    let base = turn_mut(&mut expect_of("c1_commentary_usage").unwrap(), 1)["expect"].clone();
    for (name, malformed, owner) in [
        (
            "codex_malformed_evidence_earlier_turn",
            json!({"method": "turn/completed",
                "params": {"threadId": THREAD, "turn": {"id": TURN, "status": 7}}}),
            "s_000000000001/1/",
        ),
        (
            "codex_malformed_evidence_thread_level",
            // Past the full decode's nesting bound (64), within the peek's.
            json!({"method": "thread/status/changed",
                "params": {"threadId": THREAD, "status": {"type": "idle"},
                    "nested": serde_json::from_str::<Value>(
                        &format!("{}{}", "[".repeat(80), "]".repeat(80))).unwrap()}}),
            "evidence/servers/",
        ),
    ] {
        let (replay, mut expect) =
            c1_turn2_with(name, &[emit(&malformed)], &[json!({"await_eof": {}})]).unwrap();
        let turn = &mut turn_mut(&mut expect, 1)["expect"];
        turn["terminal"] = Value::Null;
        turn["usage"] = Value::Null;
        turn["final_text"] = Value::Null;
        turn["error"] = json!("protocol");
        turn["observations_include"] = json!([base["observations_include"][0].clone()]);
        turn["observations_exclude"] = json!(["final_text"]);
        turn["observations_order"] = json!(["turn.accepted"]);
        let mut kept = Vec::new();
        check_variant_then(name, &replay, &expect, |run| {
            kept = undecoded_under(run.state.path());
            kept.extend(undecoded_under(run.case_dir.path()));
            Ok(())
        })
        .unwrap();
        assert!(
            kept.len() == 1 && kept[0].contains(owner),
            "{name}: evidence kept at {kept:?}, not under {owner}"
        );
    }
}

/// X0 items 5 and 11 (x.3.2 X3 fix r1, finding 11; fix r2 #3): a
/// declined request belongs to the turn it names. While turn 2 runs, the
/// connection answers requests naming turn 1, an unknown turn and turn 2
/// alike. Only turn 2's is turn 2's; turn 1's is turn 1's late
/// observation (Core records it `late` on turn 1, whose envelope stays
/// unchanged), as is the vendor's denial of one of turn 1's items, but not
/// the declined status of the item VIA declined; the unknown turn's is no
/// turn's.
#[test]
fn codex_decline_owner() {
    let name = "codex_decline_owner";
    let mut inserted = Vec::new();
    for (n, turn) in [TURN, "019a0000-0000-7000-8000-000000200099", TURN2]
        .into_iter()
        .enumerate()
    {
        let id = json!(80 + n);
        inserted.push(emit(&json!({"id": id,
            "method": "item/commandExecution/requestApproval",
            "params": {"threadId": THREAD, "turnId": turn, "itemId": format!("item-{n}")}})));
        inserted.push(json!({"expect": {
            "line": {"id": id, "result": {"decision": "decline"}},
            "absent": ["/error"],
            "within_ms": 5250,
        }}));
        // Turn 1's items complete declined right after its request: one
        // VIA declined, one the vendor did. Kept apart from the burst
        // after the last decline, so the lane's bound is not the test's.
        if n == 0 {
            for item in ["item-0", "item-7"] {
                inserted.push(emit(&json!({"method": "item/completed", "params": {
                    "threadId": THREAD, "turnId": TURN,
                    "item": {"type": "commandExecution", "id": item, "command": "rm -rf build",
                        "cwd": "/work/project", "commandActions": [], "status": "declined"}}})));
            }
        }
    }
    let (replay, mut expect) = c1_turn2_with(name, &inserted, &[]).unwrap();
    turn_mut(&mut expect, 1)["expect"]["observation_counts"] =
        json!({"turn.accepted": 1, "vendor.request_declined": 1, "action.denied": 0});
    turn_mut(&mut expect, 0)["expect"]["observation_counts"] =
        json!({"turn.accepted": 1, "vendor.request_declined": 0});
    check_variant_then(name, &replay, &expect, |pure| {
        let late: Vec<(usize, Value)> = pure
            .late
            .borrow()
            .iter()
            .map(|(turn, shaped)| (*turn, shaped["kind"].clone()))
            .collect();
        if late
            == [
                (0, json!("vendor.request_declined")),
                (0, json!("action.denied")),
            ]
        {
            Ok(())
        } else {
            Err(format!("late observations: {:?}", pure.late.borrow()))
        }
    })
    .unwrap();
}

/// F8 remainder (packet §8 `codex_bound_gate`; the pure refusals are
/// `codex_bound_gate_refusals`, c10 and c4b): every admitted start carries
/// `never`, the user reviewer and the current bound, a turn naming none
/// inheriting the session's (c1's second turn); the frozen instructions go
/// byte for byte as `developerInstructions` on `thread/start` and on
/// `thread/resume`, and null instructions send none (every other fixture
/// pins its absence).
#[test]
fn codex_bound_gate() {
    let c1 = replay_of("c1_commentary_usage").unwrap();
    let starts: Vec<&Value> = c1["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["expect"]["line"]["method"] == "turn/start")
        .collect();
    assert_eq!(starts.len(), 2);
    for start in starts {
        let params = &start["expect"]["line"]["params"];
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["approvalsReviewer"], "user");
        assert_eq!(params["sandboxPolicy"], json!({"type": "dangerFullAccess"}));
    }
    let c1_expect = expect_of("c1_commentary_usage").unwrap();
    assert!(c1_expect["turns"][1]["params"].get("bound").is_none());
    check("c1_commentary_usage").unwrap();

    let instructions = "Answer tersely.\n\tKeep \"quotes\", tabs and \u{2713} as written.\n";
    // On thread/start.
    let (mut replay, mut expect) = plain("codex_bound_gate_instructions_start").unwrap();
    let open = step_with(&replay, "\"method\":\"thread/start\"").unwrap();
    let start = &mut steps(&mut replay).unwrap()[open]["expect"];
    start["line"]["params"]["developerInstructions"] = json!(instructions);
    start.as_object_mut().unwrap().remove("absent");
    expect["sessions"]["main"]["instructions"] = json!(instructions);
    variant("codex_bound_gate_instructions_start", &replay, &expect).unwrap();

    // On thread/resume.
    let name = "codex_bound_gate_instructions_resume";
    let mut replay = replay_of("c5_resume").unwrap();
    replay["source"] = json!(format!("{name}: a variant of c5_resume"));
    let open = step_with(&replay, "\"method\":\"thread/resume\"").unwrap();
    let resume = &mut steps(&mut replay).unwrap()[open]["expect"];
    resume["line"]["params"]["developerInstructions"] = json!(instructions);
    resume.as_object_mut().unwrap().remove("absent");
    let mut expect = expect_of("c5_resume").unwrap();
    expect["source"] = replay["source"].clone();
    expect["sessions"]["main"]["instructions"] = json!(instructions);
    variant(name, &replay, &expect).unwrap();
}

/// Packet §8 `codex_usage_snapshot`: keyless `last` samples sum to the
/// turn's usage with scope `turn` (c1: 20522 + 20613 total tokens in its
/// first turn), a sample naming another turn of the thread never attaches,
/// and the cost stays unavailable. (`total` and the cache-write count
/// going to `vendor` are the normalizer's unit tests.)
#[test]
fn codex_usage_snapshot() {
    let c1 = expect_of("c1_commentary_usage").unwrap();
    assert_eq!(
        c1["turns"][0]["expect"]["usage"]["total_tokens"],
        20522 + 20613
    );
    assert_eq!(c1["turns"][0]["expect"]["usage"]["scope"], "turn");
    check("c1_commentary_usage").unwrap();

    let (mut replay, expect) = plain("codex_usage_snapshot_other_turn").unwrap();
    let sample = step_with(&replay, "\"method\":\"thread/tokenUsage/updated\"").unwrap();
    let other = replay["steps"][sample]["emit"]["line"]
        .as_str()
        .unwrap()
        .replace(TURN, "019a0000-0000-7000-8000-000000200099")
        .replace("\"totalTokens\":22007", "\"totalTokens\":900000");
    steps(&mut replay)
        .unwrap()
        .insert(sample, json!({"emit": {"line": other}}));
    assert_eq!(
        expect["turns"][0]["expect"]["terminal"]["cost"],
        json!({"usd": null, "provenance": "unavailable"})
    );
    variant("codex_usage_snapshot_other_turn", &replay, &expect).unwrap();
}

/// Sol r1 #15 (C2 §2 `VendorTerminal`, C1 §5): with a schema requested, a
/// final answer that is not JSON is carried as `NotJson`, which Core treats
/// as present and invalid with `reason: invalid`; it is never the missing
/// output an absent value is. The answer's delta, completed item and
/// `turn/completed` echo of c9's first turn say `VIA PLAIN` instead.
#[test]
fn codex_structured_output_not_json() {
    const ANSWER: &str = r#"{\"answer\":\"VIA_SCHEMA_159\"}"#;
    let name = "codex_structured_output_not_json";
    let mut replay = replay_of("c9_output_schema").unwrap();
    let mut edited = 0;
    for step in steps(&mut replay).unwrap() {
        if let Some(line) = step["emit"]["line"].as_str()
            && line.contains(ANSWER)
        {
            step["emit"]["line"] = json!(line.replace(ANSWER, "VIA PLAIN"));
            edited += 1;
        }
    }
    assert_eq!(edited, 3, "the delta, the item and the turn's echo");
    let mut expect = expect_of("c9_output_schema").unwrap();
    expect["source"] = json!(format!("{name}: c9_output_schema with a plain answer"));
    let turn = &mut turn_mut(&mut expect, 0)["expect"];
    turn["terminal"]["structured_output"] = Value::Null;
    turn["terminal"]["structured_output_invalid"] = json!("invalid");
    turn["final_text"] = json!(["VIA PLAIN"]);
    variant(name, &replay, &expect).unwrap();
}

/// x.3.2 critical r2 #2, adopted by Codex (runtime §8): the connection
/// counts each message it reads for the session's thread against the
/// running turn's decode watermark, from before the turn's `turn/start` is
/// written, and the normalizer reports each one delivered once it went
/// out whole, so Core's idle deadline waits for timely progress held in
/// the lane. In c9's two turns, each ending at its `turn/completed`, every
/// message Route read for a turn was delivered by its settle. The first
/// turn counts its ten messages and, when the connection reads it after
/// the fence, `thread/started`; the second counts only its own ten.
#[test]
fn codex_turns_keep_a_decode_fence() {
    let name = "codex_turns_keep_a_decode_fence";
    let replay = replay_of("c9_output_schema").unwrap();
    let mut expect = expect_of("c9_output_schema").unwrap();
    expect["source"] = json!(format!("{name}: c9_output_schema as recorded"));
    check_variant_then(name, &replay, &expect, |pure| {
        let fences = pure.fences.borrow();
        match (fences.get(&0), fences.get(&1)) {
            (Some(&(first, through)), Some(&second))
                if (11..=12).contains(&first) && through == first && second == (11, 11) =>
            {
                Ok(())
            }
            _ => Err(format!(
                "decode fences (watermark, delivered) by turn: {fences:?}"
            )),
        }
    })
    .unwrap();
}

/// Runtime §8 (x.3.2 X3 fix r2, the stale-fence check): a message the
/// lane took under turn 1's fence, after turn 1's terminal, is counted in
/// turn 1's watermark only. Turn 2's normalizer takes it first, as an
/// earlier turn's, and reports nothing delivered for it: turn 2's
/// watermark and delivery stay its own ten messages.
#[test]
fn codex_stale_fence_counts_nothing() {
    let name = "codex_stale_fence_counts_nothing";
    let mut replay = replay_of("c9_output_schema").unwrap();
    let mut expect = expect_of("c9_output_schema").unwrap();
    replay["source"] = json!(format!("{name}: a variant of c9_output_schema"));
    expect["source"] = replay["source"].clone();
    let usage = step_with(&replay, "thread/tokenUsage/updated").unwrap();
    let completed = step_with(&replay, "\"turn/completed\"").unwrap();
    let stale = replay["steps"][usage].clone();
    steps(&mut replay).unwrap().insert(completed + 1, stale);
    check_variant_then(name, &replay, &expect, |pure| {
        let fences = pure.fences.borrow();
        match (fences.get(&0), fences.get(&1)) {
            (Some(&(first, through)), Some(&second)) if through <= first && second == (11, 11) => {
                Ok(())
            }
            _ => Err(format!(
                "decode fences (watermark, delivered) by turn: {fences:?}"
            )),
        }
    })
    .unwrap();
}

/// Arms failpoint `point` with `command` (its `occurrence` and `action`)
/// for this test process; the folder lives as long as the returned guard.
#[cfg(feature = "test-failpoints")]
fn armed(point: &str, command: Value) -> Result<tempfile::TempDir, String> {
    use std::os::unix::fs::DirBuilderExt;
    let points = tempfile::tempdir().map_err(|e| e.to_string())?;
    let dir = points.path().join("points");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| e.to_string())?;
    let token = "x3-armed-failpoint";
    let mut command = command;
    command["token"] = json!(token);
    std::fs::write(dir.join(format!("{point}.json")), command.to_string())
        .map_err(|e| e.to_string())?;
    via_store::failpoint::activate(&dir, token)?;
    Ok(points)
}

/// Arms `codex.connection.message` to fail the connection task when it
/// takes its `occurrence`th admitted message (x.3.2 X0 item 13.2's seam).
#[cfg(feature = "test-failpoints")]
fn connection_task_fails_at(occurrence: usize) -> Result<tempfile::TempDir, String> {
    armed(
        "codex.connection.message",
        json!({"occurrence": occurrence, "action": "fail_io"}),
    )
}

/// How many messages the server wrote up to and including step `at`.
#[cfg(feature = "test-failpoints")]
fn emitted_through(replay: &Value, at: usize) -> usize {
    replay["steps"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .take(at + 1)
        .filter(|step| step.get("emit").is_some())
        .count()
}

/// X0 item 13.2 `connection_task_panic_with_staged_terminal` (B2): the
/// connection task fails as it takes A's `turn/completed`, which is lost
/// with it. The supervisor's abnormal fan-out ends A's lane and signals
/// A's driver at once: the driver latches its own failure (an owned
/// task), and A, with its acceptance and final text delivered, ends
/// `transport_lost`, its cleanup unproven, long before its wall; Host
/// stops the server (the fake takes its SIGTERM).
#[cfg(feature = "test-failpoints")]
#[test]
fn connection_task_panic_with_staged_terminal() {
    let (mut replay, mut expect) = plain("connection_task_panic_with_staged_terminal").unwrap();
    let completed = step_with(&replay, "\"method\":\"turn/completed\"").unwrap();
    let _points = connection_task_fails_at(emitted_through(&replay, completed)).unwrap();
    cut_after(&mut replay, completed, &[sigterm()]).unwrap();
    let base = turn_mut(&mut expect, 0)["expect"].clone();
    failed_after_acceptance(&mut expect, "transport_lost", "uncertain");
    let turn = &mut turn_mut(&mut expect, 0)["expect"];
    turn["usage"] = base["usage"].clone();
    turn["final_text"] = base["final_text"].clone();
    turn["observations_include"] = json!([
        {"kind": "turn.accepted", "vendor_turn_id": TURN}, "final_text",
    ]);
    turn["observations_exclude"] = json!(["turn.late_terminal"]);
    expect["sessions"]["main"]["close"] = Value::Null;
    expect["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": "owned_task"});
    let knobs = conformance_run::Knobs {
        panicked_tasks: 1,
        ..conformance_run::Knobs::default()
    };
    check_variant(
        "connection_task_panic_with_staged_terminal",
        &replay,
        &expect,
        knobs,
    )
    .unwrap();
}

/// X0 item 13.2 `connection_task_panic_idle_driver_reports_loss` (B2), as
/// far as C2 carries it: A completed and its driver is idle when the
/// connection task fails on a later message. A's result stands, and A's
/// driver, with no turn running, still latches its failure (an owned
/// task) through its abnormal-end signal, so Core retires it. The loss
/// record it installs reaches Core with C2's `CloseReport.loss` (x.3.2
/// X5); its handler's record is the adapter's unit test.
#[cfg(feature = "test-failpoints")]
#[test]
fn connection_task_panic_idle_driver_reports_loss() {
    let (mut replay, mut expect) = plain("connection_task_panic_idle_driver_reports_loss").unwrap();
    let completed = step_with(&replay, "\"method\":\"turn/completed\"").unwrap();
    let late = json!({"method": "thread/status/changed",
        "params": {"threadId": THREAD, "status": {"type": "idle"}}});
    cut_after(&mut replay, completed, &[emit(&late), sigterm()]).unwrap();
    let _points = connection_task_fails_at(emitted_through(&replay, completed + 1)).unwrap();
    expect["sessions"]["main"]["close"] = Value::Null;
    expect["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": "owned_task"});
    let knobs = conformance_run::Knobs {
        await_failure: true,
        panicked_tasks: 1,
        ..conformance_run::Knobs::default()
    };
    check_variant(
        "connection_task_panic_idle_driver_reports_loss",
        &replay,
        &expect,
        knobs,
    )
    .unwrap();
}

/// Finding 3 (x.3.2 X3 fix r1): a stop order after acceptance writes the
/// turn's one `turn/interrupt` on the control path; the fake expects it
/// and answers with the interrupted terminal, which ends the turn. A
/// repeated order writes no second interrupt. Its acknowledgement and the
/// P7 window are x.3.2 X4's, so `c3_interrupt_uncertain` stays red for
/// them and this variant leaves them unasserted.
#[test]
fn codex_interrupt_written() {
    for (name, repeat_stop) in [
        ("codex_interrupt_written", false),
        ("codex_interrupt_written_once", true),
    ] {
        let mut replay = replay_of("c3_interrupt_uncertain").unwrap();
        let mut expect = expect_of("c3_interrupt_uncertain").unwrap();
        replay["source"] = json!(format!("{name}: a variant of c3_interrupt_uncertain"));
        expect["source"] = replay["source"].clone();
        let turn = &mut turn_mut(&mut expect, 0)["expect"];
        let fields = turn.as_object_mut().unwrap();
        fields.remove("cleanup_settles");
        fields.remove("stop_facts");
        let why = "the interrupt's acknowledgement and the P7 window are x.3.2 X4's";
        turn["unasserted"].as_array_mut().unwrap().extend([
            json!({"field": "cleanup_settles", "why": why}),
            json!({"field": "stop_facts", "why": why}),
        ]);
        let knobs = conformance_run::Knobs {
            repeat_stop,
            ..conformance_run::Knobs::default()
        };
        check_variant(name, &replay, &expect, knobs).unwrap();
    }
}

/// Ruling 21 (x.3.2 X3 fix r1): the harness measures `cleanup_settles`.
/// A variant of `c3_interrupt_uncertain` whose command ends before the
/// interrupted terminal: no tool is open at the acknowledgement, so the
/// turn settles at its terminal, quiescent, under X4's P7 too.
#[test]
fn codex_interrupt_settles_at_terminal() {
    let name = "codex_interrupt_settles_at_terminal";
    let mut replay = replay_of("c3_interrupt_uncertain").unwrap();
    let mut expect = expect_of("c3_interrupt_uncertain").unwrap();
    replay["source"] = json!(format!("{name}: a variant of c3_interrupt_uncertain"));
    expect["source"] = replay["source"].clone();
    let started = step_with(&replay, "\"type\":\"commandExecution\"").unwrap();
    let ended = line_of(&replay, started)
        .unwrap()
        .replace(
            "\"method\":\"item/started\"",
            "\"method\":\"item/completed\"",
        )
        .replace("\"status\":\"inProgress\"", "\"status\":\"failed\"")
        .replace("\"exitCode\":null", "\"exitCode\":130")
        .replace("\"startedAtMs\"", "\"completedAtMs\"");
    let acknowledged = step_with(&replay, "\"result\":{}}").unwrap();
    steps(&mut replay)
        .unwrap()
        .insert(acknowledged + 1, json!({"emit": {"line": ended}}));
    let exec = "exec-019a0000-0000-7000-8000-000000400004";
    let turn = &mut turn_mut(&mut expect, 0)["expect"];
    turn["cleanup"] = json!("quiescent");
    turn["cleanup_settles"] = json!("at_terminal");
    turn.as_object_mut().unwrap().remove("stop_facts");
    turn["unasserted"]
        .as_array_mut()
        .unwrap()
        .push(json!({"field": "stop_facts",
        "why": "the interrupt's acknowledgement facts are x.3.2 X4's"}));
    turn["observations_include"]
        .as_array_mut()
        .unwrap()
        .push(json!({"kind": "progress", "tools_ended": [exec]}));
    expect["sessions"]["main"]["close"]["cleanup"] = json!("quiescent");
    variant(name, &replay, &expect).unwrap();
}

/// `count` thread-level status changes of the session's thread.
fn status_burst(count: usize) -> Vec<Value> {
    let status = json!({"method": "thread/status/changed",
        "params": {"threadId": THREAD, "status": {"type": "active", "activeFlags": []}}});
    vec![emit(&status); count]
}

/// x.3.2 X3 fix r2 #1: a lane that overflows while its driver is idle
/// (its turn settled; no normalizer takes the lane) reaches the driver's
/// health at once: it latches `overflow`, so Core retires the driver.
/// The burst comes once a second session's `thread/resume` shows the
/// first turn settled; that resume is refused (`session_gone`) after the
/// burst, so the case reads the health only after the burst was routed.
#[test]
fn codex_idle_overflow_latches_health() {
    let name = "codex_idle_overflow_latches_health";
    let (mut replay, mut expect) = plain(name).unwrap();
    let missing = replay_of("c5_resume_missing").unwrap();
    let missing_expect = expect_of("c5_resume_missing").unwrap();
    let reopen = step_with(&missing, "\"method\":\"thread/resume\"").unwrap();
    let completed = step_with(&replay, "\"method\":\"turn/completed\"").unwrap();
    let mut inserted = vec![missing["steps"][reopen].clone()];
    inserted.extend(status_burst(20));
    inserted.push(missing["steps"][reopen + 1].clone());
    for (offset, step) in inserted.into_iter().enumerate() {
        steps(&mut replay)
            .unwrap()
            .insert(completed + 1 + offset, step);
    }
    let mut probe = missing_expect["turns"][0].clone();
    probe["session"] = json!("probe");
    expect["turns"].as_array_mut().unwrap().push(probe);
    expect["sessions"]["probe"] = missing_expect["sessions"]["main"].clone();
    expect["sessions"]["main"]["close"] = Value::Null;
    expect["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": "overflow"});
    expect["launch_checkpoints"] =
        json!({"after_pure": 0, "after_open": {"main": 0, "probe": 0}, "after_turn": [1, 1]});
    variant(name, &replay, &expect).unwrap();
}

/// x.3.2 X3 fix r2 #1 and #2, through the driver: once the acceptance
/// was delivered (a gate), the turn's normalizer is held as it delivers a
/// delta while twenty thread messages arrive, so the lane overflows. The
/// turn ends `overflow` at once, not after the normalizer: what was
/// dropped proves nothing, so its cleanup is uncertain and the
/// generation's cleanup interrupt is written (the fake expects it), and
/// the driver's health latches `overflow`.
#[cfg(feature = "test-failpoints")]
#[test]
fn codex_overflow_interrupts_and_is_uncertain() {
    let name = "codex_overflow_interrupts_and_is_uncertain";
    // The identity's send, the acceptance's, then the delta's.
    let _points = armed(
        "adapter.observation.admitted",
        json!({"occurrence": 3, "action": "delay", "value": 3000}),
    )
    .unwrap();
    let (mut replay, mut expect) = plain(name).unwrap();
    let started = step_with(&replay, "\"method\":\"turn/started\"").unwrap();
    let delta = step_with(&replay, "\"method\":\"item/agentMessage/delta\"").unwrap();
    let mut tail = vec![
        json!({"await_signal": {"signal": "SIGUSR1"}}),
        replay["steps"][delta].clone(),
    ];
    tail.extend(status_burst(20));
    tail.push(json!({"expect": {
        "line": {"method": "turn/interrupt", "params": {"threadId": THREAD, "turnId": TURN}},
        "within_ms": 2000,
    }}));
    tail.push(json!({"await_eof": {}}));
    cut_after(&mut replay, started, &tail).unwrap();
    failed_after_acceptance(&mut expect, "overflow", "uncertain");
    // Steps count from 1: the gate is the step after `turn/started`.
    turn_mut(&mut expect, 0)["gates"] = json!([{"step": started + 2,
        "expect": {"accepted": true, "terminal": null, "error": null}}]);
    expect["sessions"]["main"]["close"] = Value::Null;
    expect["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": "overflow"});
    variant(name, &replay, &expect).unwrap();
}

/// X0, x.3.2 X3 fix r2 #4: before acceptance, a cancel is serviceable
/// while delivery is blocked. The identity's send is held in the sink;
/// the cancel ends the turn `stopped` at once, long before its force.
/// Nothing of the turn was started: only its thread was opened, which
/// the close unsubscribes.
#[cfg(feature = "test-failpoints")]
#[test]
fn codex_stop_while_identity_blocked() {
    let name = "codex_stop_while_identity_blocked";
    let _points = armed(
        "adapter.observation.admitted",
        json!({"occurrence": 1, "action": "pause"}),
    )
    .unwrap();
    let (mut replay, mut expect) = plain(name).unwrap();
    let start = step_with(&replay, "\"method\":\"turn/start\"").unwrap();
    let close = step_with(&replay, "\"method\":\"thread/unsubscribe\"").unwrap();
    steps(&mut replay).unwrap().drain(start..close);
    unaccepted(&mut expect, "stopped", tested());
    let turn = &mut turn_mut(&mut expect, 0)["expect"];
    // A stop is no error (C2 §2: `Stopped` reports none).
    turn["error"] = Value::Null;
    turn["cleanup"] = json!("quiescent");
    turn["warnings"] = json!(["config_switch_unverified"]);
    let knobs = conformance_run::Knobs {
        stop_after: Some(std::time::Duration::from_millis(300)),
        ..conformance_run::Knobs::default()
    };
    check_variant(name, &replay, &expect, knobs).unwrap();
}

/// x.3.2 X3 fix r2 #7, through the adapter: a keeper session holds the
/// shared server (its turn was refused for its effort after it joined).
/// Core drops session A's `run_turn` as it takes A's acceptance. The
/// abandoned turn detaches its generation at once (its unsubscribe is
/// written; the fake expects it), so session B can resume the same thread
/// on the shared server and run its turn; A's health latches
/// `turn_abandoned`.
#[test]
fn codex_abandoned_turn_detaches() {
    let name = "codex_abandoned_turn_detaches";
    let (mut replay, mut expect) = plain(name).unwrap();
    let resumed = replay_of("c5_resume").unwrap();
    let resumed_expect = expect_of("c5_resume").unwrap();
    let refused_expect = expect_of("c7_effort_catalog").unwrap();
    let started = step_with(&replay, "\"method\":\"turn/started\"").unwrap();
    let reopen = step_with(&resumed, "\"method\":\"thread/resume\"").unwrap();
    let mut tail = vec![
        json!({"expect": {
            "line": {"method": "thread/unsubscribe", "params": {"threadId": THREAD}},
            "capture": {"gone": "/id"},
        }}),
        json!({"emit": {"line": "{\"id\":${gone},\"result\":{\"status\":\"unsubscribed\"}}"}}),
    ];
    tail.extend(
        resumed["steps"].as_array().unwrap()[reopen..]
            .iter()
            .cloned(),
    );
    cut_after(&mut replay, started, &tail).unwrap();
    let turn = &mut turn_mut(&mut expect, 0)["expect"];
    turn["terminal"] = Value::Null;
    turn["usage"] = Value::Null;
    turn["final_text"] = Value::Null;
    turn["cleanup"] = Value::Null;
    turn["instance"] = Value::Null;
    turn["observations_include"] = json!([{"kind": "turn.accepted", "vendor_turn_id": TURN}]);
    turn["observations_exclude"] = json!(["final_text"]);
    turn["observations_order"] = json!(["session.vendor_identity_confirmed", "turn.accepted"]);
    let mut keeper = refused_expect["turns"][0].clone();
    keeper["session"] = json!("keeper");
    let mut second = resumed_expect["turns"][0].clone();
    second["session"] = json!("second");
    let main = expect["turns"][0].clone();
    expect["turns"] = json!([keeper, main, second]);
    expect["sessions"]["keeper"] = refused_expect["sessions"]["main"].clone();
    expect["sessions"]["keeper"]["close"] = Value::Null;
    expect["sessions"]["second"] = resumed_expect["sessions"]["main"].clone();
    expect["sessions"]["main"]["close"] = Value::Null;
    expect["sessions"]["main"]["health"] =
        json!({"state": "failed", "first_cause": "turn_abandoned"});
    expect["launch_checkpoints"] = json!({"after_pure": 0,
        "after_open": {"keeper": 0, "main": 0, "second": 0}, "after_turn": [1, 1, 1]});
    let knobs = conformance_run::Knobs {
        abandon_on_accept: Some(1),
        ..conformance_run::Knobs::default()
    };
    check_variant(name, &replay, &expect, knobs).unwrap();
}

/// Runtime §8 (x.3.2 X3 fix r2 #10): the paired `turn/start` reply is the
/// turn's acceptance, a message of its decode fence. A resumed turn (no
/// thread message precedes its fence) is held at a gate right after its
/// reply while its normalizer is held delivering the acceptance: the
/// watermark counts the reply and nothing is delivered through it, so
/// Core's idle frontier sees an outstanding message and decides no idle
/// cancellation (the turn is not cancelled; it completes as recorded).
/// At a second gate, past the hold with the vendor still silent, the
/// delivered acceptance counts: nothing is outstanding. Once the turn
/// settled, delivery caught up with every message.
#[cfg(feature = "test-failpoints")]
#[test]
fn codex_acceptance_is_in_the_decode_fence() {
    let name = "codex_acceptance_is_in_the_decode_fence";
    // Occurrence 1 is the resumed identity's send, 2 the acceptance's.
    let _points = armed(
        "adapter.observation.admitted",
        json!({"occurrence": 2, "action": "delay", "value": 2000}),
    )
    .unwrap();
    let mut replay = replay_of("c5_resume").unwrap();
    let mut expect = expect_of("c5_resume").unwrap();
    replay["source"] = json!(format!("{name}: a variant of c5_resume"));
    expect["source"] = replay["source"].clone();
    let answer = step_with(&replay, "\"result\":{\"turn\"").unwrap();
    for offset in 1..=2 {
        steps(&mut replay).unwrap().insert(
            answer + offset,
            json!({"await_signal": {"signal": "SIGUSR1"}}),
        );
    }
    // Steps count from 1: the gates are the two steps after the reply.
    let gate = json!({"accepted": true, "terminal": null, "error": null});
    turn_mut(&mut expect, 0)["gates"] = json!([
        {"step": answer + 2, "expect": gate},
        {"step": answer + 3, "advance_ms": 2500, "expect": gate},
    ]);
    check_variant_then(name, &replay, &expect, |pure| {
        let held = pure.gate_fences.borrow().clone();
        let settled = pure.fences.borrow().get(&0).copied();
        match (held.as_slice(), settled) {
            ([(1, 0), (1, 1)], Some((decoded, delivered))) if decoded == delivered => Ok(()),
            _ => Err(format!(
                "decode fences (watermark, delivered) at the gate {held:?}, settled {settled:?}"
            )),
        }
    })
    .unwrap();
}

/// C2 §5 (x.3.2 X3 fix r2 #11): the catalog belongs to the live server
/// instance that discovered it. Session `first` runs c7's turn 1 (its
/// effort refused after its server discovered the catalog) and closes,
/// which retires the server. Session `main`, on the same server key,
/// launches a second server, which discovers the catalog again (lifetime
/// 2 expects its `model/list`); `main`'s resumed `ultra` turn is refused
/// from that catalog while the server lives. Once `main` closed too, its
/// server retired and `models` lists nothing: a retired instance's
/// catalog is gone.
#[test]
fn codex_retired_catalog_is_rediscovered() {
    let name = "codex_retired_catalog_is_rediscovered";
    let full = replay_of("c7_effort_catalog").unwrap();
    let mut expect = expect_of("c7_effort_catalog").unwrap();
    let source = format!("{name}: a variant of c7_effort_catalog");
    let mut first = full.clone();
    let listed = step_with(&full, "\"result\":{\"data\"").unwrap();
    cut_after(&mut first, listed, &[json!({"await_eof": {}})]).unwrap();
    let replay = json!({"source": source, "lifetimes": [first, full]});
    expect["source"] = json!(source);
    expect["launches"] = json!(2);
    let mut refused = expect["turns"][0].clone();
    refused["session"] = json!("first");
    let turns = expect["turns"].as_array_mut().unwrap();
    turns.insert(0, refused);
    expect["sessions"]["first"] = expect["sessions"]["main"].clone();
    expect["launch_checkpoints"] = json!({"after_pure": 0,
        "after_open": {"first": 0, "main": 0}, "after_turn": [1, 2, 2, 2]});
    check_variant_then(name, &replay, &expect, |pure| {
        let models = pure.set.models(Some("codex"));
        if models.is_empty() {
            Ok(())
        } else {
            Err(format!("a retired server's catalog is listed: {models:?}"))
        }
    })
    .unwrap();
}

/// x.3.2 X3 fix r2 minor #13: a server's launch ordinal is the order its
/// process started, which is the fake's launch log. Session `first`'s
/// launch fails before any process (its evidence folder cannot be made),
/// so `main`'s launch is the fake's first: the registry names it launch 1
/// and the harness judges it against lifetime 1, the replay's only one.
#[cfg(feature = "test-failpoints")]
#[test]
fn codex_launch_ordinal_counts_processes() {
    let name = "codex_launch_ordinal_counts_processes";
    // Occurrence 1 makes `first`'s turn folder, 2 its server's folder.
    let _points = armed(
        "blob.step.stall",
        json!({"occurrence": 2, "action": "fail_io"}),
    )
    .unwrap();
    let (replay, mut expect) = plain(name).unwrap();
    let replay = json!({"source": replay["source"].clone(), "lifetimes": [replay]});
    let mut first = expect["turns"][0].clone();
    first["session"] = json!("first");
    expect["sessions"]["first"] = expect["sessions"]["main"].clone();
    expect["sessions"]["first"]["close"] = Value::Null;
    let turns = expect["turns"].as_array_mut().unwrap();
    turns.insert(0, first);
    unaccepted(&mut expect, "transport_lost", Value::Null);
    let failed = &mut turn_mut(&mut expect, 0)["expect"];
    if let Some(failed) = failed.as_object_mut() {
        failed.remove("error");
    }
    failed["unasserted"] = json!([{"field": "error", "why":
        "the failed folder is RouteError::Store, which C2's turn error names do not list"}]);
    expect["launch_checkpoints"] = json!({"after_pure": 0,
        "after_open": {"first": 0, "main": 0}, "after_turn": [0, 1]});
    check_variant(name, &replay, &expect, conformance_run::Knobs::default()).unwrap();
}
