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
    c8_auth,
    c9_output_schema;
    red:
    c0_server_lost = "red until via-5lr.3.2 X4 (server loss across sessions)",
    c2_steer = "red until via-5lr.3.2 X4 (native steer)",
    c3_interrupt_uncertain = "red until via-5lr.3.2 X4 (interrupt and P7)",
    c3_wall_interrupt = "red until via-5lr.3.2 X4 (the wall's soft stop)",
    c4_two_sessions = "red until via-5lr.3.2 X4 (leases across sessions)",
    c7_effort_catalog = "red until C2 gives check_turn the session's model (x.3.2 X3 gap)",
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

/// [`check_variant`] with the default knobs.
fn variant(name: &str, replay: &Value, expect: &Value) -> Result<(), String> {
    check_variant(name, replay, expect, conformance_run::Knobs::default())
}

/// The step that seals a force-closed server's run: Host's close sends
/// the group `SIGTERM`, which the fake takes and then exits on its own.
fn sigterm() -> Value {
    json!({"await_signal": {"signal": "SIGTERM"}})
}

/// F13 (packet §8 `codex_pin_handshake`): one initialize/initialized per
/// connection, with neither an experimental capability nor an opt-out
/// (every fixture's first step pins their absence); the version comes
/// from `userAgent`, and one outside `checked` proceeds as untested; a
/// malformed handshake and a policy or sandbox echo mismatch refuse the
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
    ] {
        let (mut replay, mut expect) = plain(name).unwrap();
        let answer = step_with(&replay, "\"result\":{\"thread\"").unwrap();
        edit_emit(&mut replay, answer, from, to).unwrap();
        cut_after(&mut replay, answer, &[json!({"await_eof": {}})]).unwrap();
        unaccepted(&mut expect, "protocol", tested());
        variant(name, &replay, &expect).unwrap();
    }

    // model/list: a cursor left at the page bound, then a catalog past
    // the byte bound.
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
    unaccepted(&mut expect, "protocol", Value::Null);
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
/// `protocol`; a start the server never answered is no acceptance; a
/// malformed known notification of the turn fails it `protocol`; a second,
/// contradictory `turn/completed` never replaces the retained terminal.
#[test]
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

    // A malformed turn/completed of the turn.
    let (mut replay, mut expect) = plain("codex_start_order_malformed").unwrap();
    let completed = step_with(&replay, completed_marker).unwrap();
    steps(&mut replay).unwrap()[completed] = emit(&json!({
        "method": "turn/completed",
        "params": {"threadId": THREAD, "turn": "not a turn"},
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
    variant("codex_start_order_malformed", &replay, &expect).unwrap();

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
