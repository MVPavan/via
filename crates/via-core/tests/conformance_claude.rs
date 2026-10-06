//! C2 conformance cases for the Claude Code adapter, red by construction
//! (`via-p98.3.1`; adapters design §6 step 6, §7).
//!
//! Each case pairs `crates/via-adapters/tests/fixtures/claude/<case>.replay.json`
//! (the recorded vendor side, replayed by `via-fake-agent`) with
//! `<case>.expect.json` (what the C2 driver must produce, in the unified
//! expectation schema checked by the shared `support/conformance_expect.rs`).
//! [`drive`] is the one seam. Its pure half (the shared
//! `support/conformance_drive.rs`: describe, plan checks and spawn plans) is
//! in place, so the cases every plan refuses run ([`PURE_CASES`]); the
//! driver half lands with `via-p98.3.2`'s C2 chunk, which removes the other
//! cases' `ignore`s.
//!
//! [`VENDOR_RECORDS`] are fixtures with an expectation that are not adapter
//! conformance cases: they have no case test, but their expectation still
//! validates, and `via-fake-agent`'s `fixtures.rs` still replays and scans
//! them.
//!
//! # Fixture argv
//!
//! The argv is the adapter's own recipe (vendor packet §4) in the restricted
//! mode, MCP servers off, the fixtures were recorded in (the harness
//! configures `harnesses.claude.restricted: true` and
//! `inherit.mcp_servers: false`), in this order:
//! `-p --input-format stream-json --output-format stream-json --verbose
//! --model M (--session-id {capture sid} | --resume <ID>)
//! --restricted --strict-mcp-config --permission-mode dontAsk
//! --permission-prompts none --tools T --allowedTools T`, then
//! `--append-system-prompt`, `--effort` and `--json-schema` (compact, sorted
//! keys) when set. A resume pins the literal `--resume` value, the case's
//! `sessions.main.resume`, so a driver that passes another ID fails the
//! replay's argv check. A new session's `--session-id` stays a capture:
//! the adapter allocates that UUID.
//!
//! # Versions
//!
//! Every launched case pins `instance` `{vendor_version: "2.1.285",
//! version_status: "tested"}`: `via-p98.3.2`'s initial `checked` set holds
//! the fixture version (the live re-probes of 2026-09-30), except
//! `claude_model_refusal`, recorded on 2.1.289 (checked since 2026-10-05).
//! The synthetic untested case is `via-p98.3.2`'s
//! `claude_preflight_pure_version`.

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
const ADAPTER_BEAD: &str = "via-p98.3.2";

/// Vendor-behaviour records: fixtures with an expectation file that are not
/// adapter conformance cases, so they have no case test.
const VENDOR_RECORDS: [(&str, &str); 1] = [(
    "c10_early_eof",
    "stdin EOF right after the prompt still completes the turn: the evidence \
     behind AD15/VC7. No C2 verb makes a correct adapter close stdin early; \
     c7's AD19 order pins the adapter side",
)];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../via-adapters/tests/fixtures/claude")
}

/// Runs the case's sessions and turns through the Claude C2 driver against
/// the fake agent replaying `<case>.replay.json`, and collects the outcome
/// (see the checker's module docs for its obligations: `launches` from
/// `<case>.launches`, the replay's end judged by
/// [`conformance_expect::replay_exit`] for every launch, and stdin EOF after the
/// result at an `await_eof` step): the shared pure half, then the shared
/// run half (`support/conformance_run.rs`).
fn drive(name: &str, expect: &Value) -> Result<Outcome, String> {
    drive_with(name, expect, conformance_run::Knobs::default())
}

/// [`drive`] with the run half's test seams.
fn drive_with(
    name: &str,
    expect: &Value,
    knobs: conformance_run::Knobs,
) -> Result<Outcome, String> {
    let replay = fixtures().join(format!("{name}.replay.json"));
    conformance_drive::Pure::run("claude", name, expect, &replay)?
        .drive(expect, &replay, knobs)
        .map_err(|why| format!("{why} ({ADAPTER_BEAD} case {name})"))
}

fn check_expect(name: &str, expect: &Value) -> Result<(), String> {
    let outcome = drive(name, expect)?;
    conformance_expect::check(expect, &outcome).map_err(|wrong| format!("{name}:\n{wrong}"))
}

fn check(name: &str) -> Result<(), String> {
    check_expect(name, &conformance_expect::load(&fixtures(), name)?)
}

macro_rules! cases {
    (pure: [$($pure:ident),* $(,)?], driven: [$($name:ident),* $(,)?] $(,)?) => {
        /// Every case with a test below.
        const CASES: &[&str] = &[$(stringify!($pure),)* $(stringify!($name)),*];

        /// The cases the pure half settles: every plan refuses.
        const PURE_CASES: &[&str] = &[$(stringify!($pure)),*];

        mod conformance_claude_cases {
            $(
                #[test]
                fn $pure() {
                    super::check(stringify!($pure)).unwrap();
                }
            )*
            $(
                #[test]
                fn $name() {
                    super::check(stringify!($name)).unwrap();
                }
            )*
        }
    };
}

cases! {
    pure: [c0_bad_effort, c5_read_only],
    driven: [
    c0_isolated,
    c0_bad_model,
    claude_model_refusal,
    c0_invalid_resume,
    c1a,
    c1b,
    c1c,
    c1b_resume_mismatch,
    c3_queue,
    c4_never_ask,
    c7_interrupt,
    c9a,
    c9b,
    c11b_stdio_prompt,
    ],
}

/// Packet §9's named tests that are fixture cases, run by their named
/// tests below (`test(/^claude_/)` selects them); a named test may run
/// several fixtures, and variants its fixtures cannot hold (a crash, or
/// more lines than a reviewable file) built from them in scratch.
const NAMED: &[&str] = &[
    "claude_reserved_options",
    "claude_lazy_init_acceptance",
    "claude_lazy_init_acceptance_result_only",
    "claude_fifo_busy_input",
    "claude_identity_resume",
    "claude_schema_replace_clear",
    "claude_agentic_step_limit",
    "claude_instructions_effort",
    "claude_interrupt_pairing",
    "claude_interrupt_pairing_wrong_id",
    "claude_interrupt_pairing_late_response",
    "claude_interrupt_pairing_natural_success",
    "claude_cleanup_not_ack",
    "claude_normalizer_accounting",
    "claude_normalizer_accounting_duplicate_result",
    "claude_normalizer_accounting_cross_generation",
    "claude_normalizer_accounting_mismatch_after_terminal",
    "claude_preflight_pure_version",
    "claude_preflight_pure_version_no_receipt",
    "claude_preflight_pure_version_echo",
];

/// Loads fixture `name`'s replay.
fn replay_of(name: &str) -> Result<Value, String> {
    let text = std::fs::read(fixtures().join(format!("{name}.replay.json")))
        .map_err(|e| format!("{name}: {e}"))?;
    serde_json::from_slice(&text).map_err(|e| format!("{name}: {e}"))
}

/// Runs a variant of a fixture: `replay` and `expect` as the test built
/// them, from a scratch directory (never written under the fixtures).
fn check_variant(
    name: &str,
    replay: &Value,
    expect: &Value,
    knobs: conformance_run::Knobs,
) -> Result<(), String> {
    let outcome = drive_variant(name, replay, expect, knobs)?;
    conformance_expect::check(expect, &outcome).map_err(|wrong| format!("{name}:\n{wrong}"))
}

/// [`check_variant`]'s outcome, unchecked: for what the schema cannot
/// state.
fn drive_variant(
    name: &str,
    replay: &Value,
    expect: &Value,
    knobs: conformance_run::Knobs,
) -> Result<Outcome, String> {
    conformance_expect::validate(expect).map_err(|e| format!("{name}: {e}"))?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join(format!("{name}.replay.json"));
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(replay).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    conformance_drive::Pure::run("claude", name, expect, &path)?
        .drive(expect, &path, knobs)
        .map_err(|why| format!("{why} ({ADAPTER_BEAD} variant {name})"))
}

/// The emit step of `replay`'s lifetime `lifetime` (0-based; the file
/// itself when it has none) whose line contains `marker`, by index.
fn emit_step(
    replay: &mut Value,
    lifetime: Option<usize>,
    marker: &str,
) -> Result<(usize, String), String> {
    let fixture = match lifetime {
        Some(at) => &mut replay["lifetimes"][at],
        None => replay,
    };
    let steps = fixture["steps"].as_array().ok_or("no steps")?;
    steps
        .iter()
        .enumerate()
        .find_map(|(at, step)| {
            let line = step["emit"]["line"].as_str()?;
            line.contains(marker).then(|| (at, line.to_owned()))
        })
        .ok_or_else(|| format!("no emit step with {marker}"))
}

/// An emit step of `line`.
fn emit(line: &Value) -> Value {
    json!({"emit": {"line": line.to_string()}})
}

/// F8 (packet §9 `claude_reserved_options`): every normalized alias of a
/// flag, setting or environment override the recipe owns is refused
/// `vendor_option_conflict`, and any other Claude key `invalid_params`,
/// before vendor I/O: no launch, no file written. The positive half (the
/// canonical parameters give the exact argv) is the launch recipe's unit
/// test against every fixture's argv.
#[test]
fn claude_reserved_options() {
    check("claude_reserved_options").unwrap();
}

/// Packet §2/§5: init comes only after the prompt line, confirms identity
/// and is never acceptance (the fixture's gate holds the fake after init
/// and vendor activity); the first assistant block accepts once. With no
/// init, the sole successful result confirms, then accepts.
#[test]
fn claude_lazy_init_acceptance() {
    check("claude_lazy_init_acceptance").unwrap();
    check("claude_lazy_init_acceptance_result_only").unwrap();
}

/// Packet §6: a second VIA turn queued while the first one's tool runs
/// never reaches the busy process (each lifetime takes exactly one user
/// line); it starts as a new process resuming the confirmed UUID.
#[test]
fn claude_fifo_busy_input() {
    check("claude_fifo_busy_input").unwrap();
}

/// F10 (packet §9 `claude_identity_resume`), the driver half: three
/// launches on one UUID, a new session (`--session-id`, ruling Q4) then
/// two `--resume`s of the exact confirmed ID (the replay pins it). Each
/// launch confirms its own generation once, from its own init, before its
/// acceptance; at the gate after the second prompt nothing of that launch
/// is confirmed, so the earlier confirmation never verifies a reopening.
/// The Core half (status keeps the historical ID with
/// `vendor_identity_verified:false` while reopening, one `session.opened`,
/// one `session.reopened` per reopen) runs through the daemon in
/// via-cli's `claude_identity_resume_through_daemon`.
///
/// Ruling Q5 (generation barrier by construction): a per-turn process is
/// retired, its last message delivered, before the next turn's launch, so
/// no prior-generation message can reach a later turn. Here the fake's
/// progress log shows only the launch order (launch n's stdin EOF before
/// launch n+1 reads its prompt); the barrier under contention, a late
/// prior-generation message held past EOF while the next turn waits, is
/// via-cli's `claude_generation_barrier_holds_late_messages`. The variants keep the chain whole when a launch fails: another
/// session's init (`resume_mismatch`, no confirmation, and the driver ends
/// the connection), a missing session (`session_gone`, and the next launch
/// still resumes the same UUID, never a fresh `--session-id`), and a lost
/// submission (the process exits after the prompt): the next launch reads
/// only its own prompt, never the lost one again.
#[test]
fn claude_identity_resume() {
    let name = "claude_identity_resume";
    let expect = conformance_expect::load(&fixtures(), name).unwrap();
    let replay = fixtures().join(format!("{name}.replay.json"));
    let mut progress = String::new();
    let outcome = conformance_drive::Pure::run("claude", name, &expect, &replay)
        .unwrap()
        .drive_then(
            &expect,
            &replay,
            conformance_run::Knobs::default(),
            |pure| {
                progress = std::fs::read_to_string(pure.case_file("progress"))
                    .map_err(|e| format!("progress: {e}"))?;
                Ok(())
            },
        )
        .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
    let at = |line: &str| {
        progress
            .lines()
            .position(|seen| seen == line)
            .unwrap_or_else(|| panic!("no {line:?} in the progress log:\n{progress}"))
    };
    for n in 1..3 {
        assert!(
            at(&format!("eof launch {n}")) < at(&format!("read 1 launch {}", n + 1)),
            "launch {} read its prompt before launch {n}'s stdin EOF:\n{progress}",
            n + 1
        );
    }
}

/// The chain's UUID, as its third turn's confirmation names it.
fn chain_uuid(expect: &Value) -> String {
    expect["turns"][2]["expect"]["observations_include"][0]["vendor_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// [`claude_identity_resume`] with another session's init on launch 3
/// (`c1b_resume_mismatch`'s AD19 stop): `resume_mismatch`, no confirmation,
/// and the driver's health ends the connection.
#[test]
fn claude_identity_resume_mismatch() {
    let name = "claude_identity_resume";
    let expect = conformance_expect::load(&fixtures(), name).unwrap();
    let base = replay_of(name).unwrap();
    let other = "99999999-9999-4999-8999-999999999999";
    let u1 = chain_uuid(&expect);
    let mut mismatch = base.clone();
    let mut steps = base["lifetimes"][2]["steps"].as_array().unwrap()[..3].to_vec();
    let init = steps[2]["emit"]["line"].as_str().unwrap().to_owned();
    steps[2] = json!({"emit": {"line": init.replace(&u1, other)}});
    let recorded = replay_of("c1b_resume_mismatch").unwrap();
    steps.extend_from_slice(&recorded["steps"].as_array().unwrap()[3..]);
    mismatch["lifetimes"][2]["steps"] = json!(steps);
    let mut wanted = expect.clone();
    let recorded = conformance_expect::load(&fixtures(), "c1b_resume_mismatch").unwrap();
    wanted["sessions"]["main"]["health"] =
        json!({"state": "failed", "first_cause": "resume_mismatch"});
    let mut third = recorded["turns"][0]["expect"].clone();
    third["observations_include"][0]["requested"] = json!(u1);
    third["observations_include"][0]["returned"] = json!(other);
    wanted["turns"][2]["expect"] = third;
    check_variant(
        "claude_identity_resume_mismatch",
        &mismatch,
        &wanted,
        conformance_run::Knobs::default(),
    )
    .unwrap();
}

/// [`claude_identity_resume`] with a missing session on launch 2
/// (`c0_invalid_resume`'s rejection): `session_gone`, and launch 3 still
/// resumes the same UUID, never a fresh `--session-id`.
#[test]
fn claude_identity_resume_missing() {
    let name = "claude_identity_resume";
    let expect = conformance_expect::load(&fixtures(), name).unwrap();
    let base = replay_of(name).unwrap();
    let u1 = chain_uuid(&expect);
    let mut missing = base.clone();
    let recorded = replay_of("c0_invalid_resume").unwrap();
    let rejection = serde_json::to_string(&recorded["steps"])
        .unwrap()
        .replace("33333333-3333-4333-8333-333333333333", &u1);
    missing["lifetimes"][1]["steps"] = serde_json::from_str(&rejection).unwrap();
    missing["lifetimes"][1]["steps"][0] = base["lifetimes"][1]["steps"][0].clone();
    let mut wanted = expect.clone();
    let recorded = conformance_expect::load(&fixtures(), "c0_invalid_resume").unwrap();
    wanted["turns"][1]["expect"] = recorded["turns"][0]["expect"].clone();
    wanted["turns"][1]
        .as_object_mut()
        .unwrap()
        .insert("gates".to_owned(), json!([]));
    // Launch 2 confirmed nothing: launch 3's is the case's second ID.
    wanted["turns"][2]["expect"]["observations_include"][0]["generation"] = json!(2);
    check_variant(
        "claude_identity_resume_missing",
        &missing,
        &wanted,
        conformance_run::Knobs::default(),
    )
    .unwrap();
}

/// [`claude_identity_resume`] with a lost submission on launch 2 (the
/// process exits after the prompt): launch 3 reads only its own prompt,
/// never the lost one again.
#[test]
fn claude_identity_resume_lost() {
    let name = "claude_identity_resume";
    let expect = conformance_expect::load(&fixtures(), name).unwrap();
    let mut lost = replay_of(name).unwrap();
    lost["lifetimes"][1]["steps"] = json!([
        lost["lifetimes"][1]["steps"][0].clone(),
        {"exit": {"code": 1, "stderr": ""}},
    ]);
    let mut wanted = expect;
    let second = &mut wanted["turns"][1];
    second["gates"] = json!([]);
    let lost_turn = &mut second["expect"];
    for (field, value) in [
        ("accepted", json!(false)),
        ("terminal", Value::Null),
        ("usage", Value::Null),
        ("final_text", Value::Null),
        ("instance", Value::Null),
        ("error", json!("process_exit")),
        ("exit", json!({"code": 1, "signal": null})),
        ("observations_include", json!([])),
        (
            "observations_exclude",
            json!(["session.vendor_identity_confirmed", "turn.accepted"]),
        ),
        (
            "observation_counts",
            json!({"turn.accepted": 0, "session.vendor_identity_confirmed": 0}),
        ),
        ("observations_order", json!([])),
    ] {
        lost_turn[field] = value;
    }
    wanted["turns"][2]["expect"]["observations_include"][0]["generation"] = json!(2);
    check_variant(
        "claude_identity_resume_lost",
        &lost,
        &wanted,
        conformance_run::Knobs::default(),
    )
    .unwrap();
}

/// Ruling Q5, the generation barrier's stop and force check: a turn whose
/// stop order (a cancel) or the daemon force is already set when it
/// starts launches nothing (Route's entry check after the barrier wait,
/// which those orders end), so it offers no observation to order: no
/// launch, no acceptance, no terminal; a stop has no failure, the force
/// is `force_stop`. The session stays open.
#[test]
fn claude_generation_barrier_orders_before_launch() {
    let name = "claude_lazy_init_acceptance_result_only";
    let replay = replay_of(name).unwrap();
    let mut expect = conformance_expect::load(&fixtures(), name).unwrap();
    expect["launches"] = json!(0);
    expect["launch_checkpoints"]["after_turn"] = json!([0]);
    expect["sessions"]["main"]["health"] = json!({"state": "open", "first_cause": null});
    let wanted = &mut expect["turns"][0]["expect"];
    for (field, value) in [
        ("accepted", json!(false)),
        ("terminal", Value::Null),
        ("usage", Value::Null),
        ("final_text", Value::Null),
        ("instance", Value::Null),
        ("exit", Value::Null),
        ("group_absent", json!(false)),
        ("observations_include", json!([])),
        (
            "observations_exclude",
            json!(["session.vendor_identity_confirmed"]),
        ),
        ("observation_counts", json!({"turn.accepted": 0})),
        ("observations_order", json!([])),
    ] {
        wanted[field] = value;
    }
    for (variant, knobs, error) in [
        (
            "claude_barrier_stop",
            conformance_run::Knobs {
                stop_before: true,
                ..conformance_run::Knobs::default()
            },
            Value::Null,
        ),
        (
            "claude_barrier_force",
            conformance_run::Knobs {
                force_before: true,
                ..conformance_run::Knobs::default()
            },
            json!("force_stop"),
        ),
    ] {
        let mut expect = expect.clone();
        expect["turns"][0]["expect"]["error"] = error;
        check_variant(variant, &replay, &expect, knobs).unwrap();
    }
}

/// Disjoint schemas A and B, then null, on one UUID: each launch carries
/// only its own turn's schema, and null clears it.
#[test]
fn claude_schema_replace_clear() {
    check("claude_schema_replace_clear").unwrap();
}

/// F8 positive: `--max-turns N` per turn; `error_max_turns` maps failed,
/// `max_steps`, `budget_exceeded`; null omits the flag.
#[test]
fn claude_agentic_step_limit() {
    check("claude_agentic_step_limit").unwrap();
}

/// F8 positive: the frozen instructions on every launch and each turn's
/// explicit effort; the argv-size refusal (ruling Q3) is
/// [`claude_argv_budget_refused_before_launch`], the invalid effort
/// `c0_bad_effort`.
#[test]
fn claude_instructions_effort() {
    check("claude_instructions_effort").unwrap();
}

/// Never-ask: c4's live denial and terminal denial give one
/// `action.denied`; c11b's written decline gives one
/// `vendor.request_declined` and suppresses its denial. An unknown control
/// request is declined at once on the control lane while Core takes no
/// observation and the session's channel is full: Core's consumer holds
/// until the fake has read the decline (its second input line), and the
/// replay requires the decline within 5 s (plus 250 ms of pipe slack).
#[test]
fn claude_never_ask() {
    for (name, kind) in [
        ("c4_never_ask", "action.denied"),
        ("c11b_stdio_prompt", "vendor.request_declined"),
    ] {
        let expect = conformance_expect::load(&fixtures(), name).unwrap();
        let counts = &expect["turns"][0]["expect"]["observation_counts"];
        assert_eq!(counts[kind], 1, "{name}: one {kind}");
        check(name).unwrap();
    }
    let base = replay_of("claude_lazy_init_acceptance").unwrap();
    let sid = "${sid}";
    // The prompt, the vendor's pause and init.
    let mut steps = base["steps"].as_array().unwrap()[..3].to_vec();
    // Past the session channel's 1024 items: progress Core does not take.
    for n in 0..1100 {
        steps.push(emit(&json!({
            "type": "assistant",
            "message": {"id": format!("msg_{n}"), "role": "assistant",
                "content": [{"type": "text", "text": "."}]},
            "session_id": sid,
        })));
    }
    steps.push(emit(&json!({
        "type": "control_request",
        "request_id": "00000000-0000-4000-8000-0000000000aa",
        "request": {"subtype": "elicitation", "message": "Pick one"},
    })));
    steps.push(json!({"expect": {
        "line": {"type": "control_response", "response": {"subtype": "error",
            "request_id": "00000000-0000-4000-8000-0000000000aa",
            "error": "VIA declines unsupported control request"}},
        "within_ms": 5250,
        "absent": ["/response/response"],
    }}));
    steps.push(base["steps"][6].clone());
    steps.push(json!({"await_eof": {}}));
    let mut replay = base.clone();
    replay["source"] = json!("claude_never_ask variant of claude_lazy_init_acceptance");
    replay["steps"] = json!(steps);
    let mut expect = conformance_expect::load(&fixtures(), "claude_lazy_init_acceptance").unwrap();
    let turn = &mut expect["turns"][0];
    turn["gates"] = json!([]);
    turn["expect"]["observation_counts"] = json!({
        "turn.accepted": 1, "vendor.request_declined": 1, "action.denied": 0,
    });
    turn["expect"]["observations_include"] = json!([
        {"kind": "vendor.request_declined", "vendor_method": "elicitation",
            "summary": "an unsupported control request", "blocking": true},
    ]);
    let knobs = conformance_run::Knobs {
        hold_until_read: Some(2),
        ..conformance_run::Knobs::default()
    };
    check_variant("claude_never_ask_full", &replay, &expect, knobs).unwrap();
}

/// F15: VIA's receipt (paired by `request_id`, nested `still_queued`)
/// then the abort terminal acknowledge the cancel, and the gate between
/// them shows the receipt alone acknowledges nothing. A wrong ID, a late
/// receipt, a natural success, a missing terminal and a duplicate cancel
/// never acknowledge falsely; a duplicate cancel writes one interrupt (the
/// replay fails a second line before its `await_eof`).
#[test]
fn claude_interrupt_pairing() {
    for name in [
        "claude_interrupt_pairing",
        "claude_interrupt_pairing_wrong_id",
        "claude_interrupt_pairing_late_response",
        "claude_interrupt_pairing_natural_success",
    ] {
        check(name).unwrap();
    }
    let expect = conformance_expect::load(&fixtures(), "claude_interrupt_pairing").unwrap();
    let replay = replay_of("claude_interrupt_pairing").unwrap();
    let repeat = conformance_run::Knobs {
        repeat_stop: true,
        ..conformance_run::Knobs::default()
    };
    check_variant(
        "claude_interrupt_pairing_duplicate",
        &replay,
        &expect,
        repeat,
    )
    .unwrap();
    // Missing terminal: the receipt, then the process exits with no result.
    let mut missing = replay;
    let steps = missing["steps"].as_array_mut().unwrap();
    steps.truncate(6);
    steps.push(json!({"exit": {"code": 1, "stderr": ""}}));
    let mut expect = expect;
    let turn = &mut expect["turns"][0];
    turn["gates"] = json!([]);
    let wanted = &mut turn["expect"];
    wanted["terminal"] = Value::Null;
    wanted["usage"] = Value::Null;
    wanted["error"] = json!("process_exit");
    wanted["stop_facts"] = json!({"acknowledged": false});
    wanted["exit"] = json!({"code": 1, "signal": null});
    check_variant(
        "claude_interrupt_pairing_missing_terminal",
        &missing,
        &expect,
        conformance_run::Knobs::default(),
    )
    .unwrap();
}

/// F6: a child that survives the leader's exit keeps cleanup unproven
/// (never `quiescent` without `GroupAbsent` for the group), and the
/// anchor's own-group force at `force_at` is never acknowledgement.
///
/// The forced case is built from `claude_interrupt_pairing`: the fake
/// reads VIA's interrupt and never answers; at `force_at` the anchor's
/// own-group `SIGTERM` ends it (its `await_signal` step takes the signal,
/// then it exits 143 on its own, so the replay's verdict stands). The
/// outcome is the cancel's own (`Stopped`, no error), forced and never
/// acknowledged.
#[test]
fn claude_cleanup_not_ack() {
    check("claude_cleanup_not_ack").unwrap();
    let mut replay = replay_of("claude_interrupt_pairing").unwrap();
    let steps = replay["steps"].as_array_mut().unwrap();
    steps.truncate(5);
    steps.push(json!({"await_signal": {"signal": "SIGTERM"}}));
    steps.push(json!({"exit": {"code": 143, "stderr": ""}}));
    let mut expect = conformance_expect::load(&fixtures(), "claude_interrupt_pairing").unwrap();
    let turn = &mut expect["turns"][0];
    turn["gates"] = json!([]);
    let wanted = &mut turn["expect"];
    wanted["terminal"] = Value::Null;
    wanted["usage"] = Value::Null;
    // The cancel's own outcome (`Stopped`, no error), forced at `force_at`.
    wanted["error"] = Value::Null;
    wanted["stop_facts"] = json!({"acknowledged": false, "forced": true});
    wanted["exit"] = json!({"code": 143, "signal": null});
    let knobs = conformance_run::Knobs::default();
    check_variant("claude_cleanup_not_ack_forced", &replay, &expect, knobs).unwrap();
}

/// F18: a synthetic API-error message is never progress, a repeated block
/// starts its call once, usage is the `result.usage` aggregate; a
/// contradictory duplicate result is `protocol` with the first terminal
/// kept; another session's result is `resume_mismatch`; a malformed known
/// message is `protocol`; a lost submission (the process exits after the
/// prompt with no output) never accepts.
#[test]
fn claude_normalizer_accounting() {
    for name in [
        "claude_normalizer_accounting",
        "claude_normalizer_accounting_duplicate_result",
        "claude_normalizer_accounting_cross_generation",
        "claude_normalizer_accounting_mismatch_after_terminal",
    ] {
        check(name).unwrap();
    }
    // C2 §2's third case by init: another session's init after the
    // retained terminal leaves the turn's outcome and fails health.
    let name = "claude_normalizer_accounting_mismatch_after_terminal";
    let mut late_init = replay_of(name).unwrap();
    let (at, line) = emit_step(&mut late_init, None, "\"OTHER\"").unwrap();
    let other: Value = serde_json::from_str(&line).unwrap();
    let (_, init) = emit_step(&mut late_init, None, "\"subtype\":\"init\"").unwrap();
    let init = init.replace("${sid}", other["session_id"].as_str().unwrap());
    late_init["steps"][at] = json!({"emit": {"line": init}});
    let expect = conformance_expect::load(&fixtures(), name).unwrap();
    let knobs = conformance_run::Knobs::default();
    check_variant("claude_late_init_mismatch", &late_init, &expect, knobs).unwrap();
    let base = replay_of("claude_lazy_init_acceptance_result_only").unwrap();
    let mut malformed = base.clone();
    malformed["steps"][2] = emit(&json!({
        "type": "result", "subtype": "success", "is_error": "no", "session_id": "${sid}",
    }));
    // Route's protocol failure force-closes the group: the fake takes the
    // anchor's SIGTERM and exits on its own.
    malformed["steps"][3] = json!({"await_signal": {"signal": "SIGTERM"}});
    malformed["steps"]
        .as_array_mut()
        .unwrap()
        .push(json!({"exit": {"code": 143, "stderr": ""}}));
    let mut expect =
        conformance_expect::load(&fixtures(), "claude_lazy_init_acceptance_result_only").unwrap();
    expect["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": "protocol"});
    let wanted = &mut expect["turns"][0]["expect"];
    for (field, value) in [
        ("accepted", json!(false)),
        ("terminal", Value::Null),
        ("usage", Value::Null),
        ("final_text", Value::Null),
        ("error", json!("protocol")),
        ("exit", json!({"code": 143, "signal": null})),
        ("observations_include", json!([])),
        ("observations_exclude", json!(["turn.accepted"])),
        ("observation_counts", json!({"turn.accepted": 0})),
        ("observations_order", json!([])),
    ] {
        wanted[field] = value;
    }
    check_variant(
        "claude_normalizer_accounting_malformed",
        &malformed,
        &expect,
        conformance_run::Knobs::default(),
    )
    .unwrap();
    let mut lost = base;
    let steps = lost["steps"].as_array_mut().unwrap();
    steps.truncate(1);
    steps.push(json!({"exit": {"code": 1, "stderr": ""}}));
    // A per-turn process's exit fails the turn, not the driver: the next
    // turn launches anew.
    expect["sessions"]["main"]["health"] = json!({"state": "open", "first_cause": null});
    expect["turns"][0]["expect"]["error"] = json!("process_exit");
    expect["turns"][0]["expect"]["exit"] = json!({"code": 1, "signal": null});
    check_variant(
        "claude_normalizer_accounting_lost",
        &lost,
        &expect,
        conformance_run::Knobs::default(),
    )
    .unwrap();
}

/// F13: a version outside `checked` is untested and proceeds, with or
/// without `allow_untested`; `describe` launches nothing and reports no
/// version. A missing `interrupt_receipt_v1` or permission-mode echo fails
/// `protocol` with no resend, and the refusal is cached for that recipe
/// only: a later plan of the same recipe is `version_refused`, the schema
/// recipe is not.
#[test]
fn claude_preflight_pure_version() {
    check("claude_preflight_pure_version").unwrap();
    let name = "claude_preflight_pure_version";
    let expect = conformance_expect::load(&fixtures(), name).unwrap();
    let allow = conformance_run::Knobs {
        allow_untested: true,
        ..conformance_run::Knobs::default()
    };
    check_variant(name, &replay_of(name).unwrap(), &expect, allow).unwrap();
    for name in [
        "claude_preflight_pure_version_no_receipt",
        "claude_preflight_pure_version_echo",
    ] {
        let expect = conformance_expect::load(&fixtures(), name).unwrap();
        let replay = fixtures().join(format!("{name}.replay.json"));
        let outcome = conformance_drive::Pure::run("claude", name, &expect, &replay)
            .unwrap()
            .drive_then(
                &expect,
                &replay,
                conformance_run::Knobs::default(),
                |pure| {
                    let status = |schema: usize| {
                        let request = via_adapters::DescribeRequest {
                            harness: Some("claude".to_owned()),
                            model: Some("haiku".to_owned()),
                            sizes: via_adapters::ParamSizes {
                                output_schema: schema,
                                ..via_adapters::ParamSizes::default()
                            },
                            ..via_adapters::DescribeRequest::default()
                        };
                        let plan = pure.set.plan(&request).map_err(|e| format!("{e:?}"))?;
                        let plan = serde_json::to_value(&plan).map_err(|e| e.to_string())?;
                        Ok::<_, String>(plan["version_status"].clone())
                    };
                    if status(0)? != "refused" || status(2)? == "refused" {
                        return Err(format!(
                            "{name}: cached refusal: plain {}, schema {}",
                            status(0)?,
                            status(2)?
                        ));
                    }
                    Ok(())
                },
            )
            .unwrap();
        conformance_expect::check(&expect, &outcome).unwrap_or_else(|e| panic!("{name}:\n{e}"));
    }
}

/// How a launch fails before any handshake.
#[derive(Clone, Copy)]
enum NoHandshake {
    /// The process exits after the prompt.
    Exit,
    /// The wall passes before init.
    Wall,
    /// The binary cannot be started.
    Spawn,
}

/// C2 §5: only a demonstrated incompatibility is cached. A launch that
/// fails before any handshake (`how`) leaves no refusal: the next plan of
/// the same recipe is not `refused`.
fn never_cached(how: NoHandshake) -> Result<(), String> {
    let name = "claude_lazy_init_acceptance_result_only";
    let mut replay = replay_of(name)?;
    let mut expect = conformance_expect::load(&fixtures(), name)?;
    let prompt = replay["steps"][0].clone();
    let (variant, error) = match how {
        NoHandshake::Exit => {
            replay["steps"] = json!([prompt, {"exit": {"code": 1, "stderr": ""}}]);
            ("claude_uncached_exit", "process_exit")
        }
        NoHandshake::Wall => {
            // C2 §4.1: a private route's wall is Host's force close.
            replay["steps"] = json!([
                prompt,
                {"await_signal": {"signal": "SIGTERM"}},
                {"exit": {"code": 143, "stderr": ""}},
            ]);
            expect["turns"][0]["deadlines"] = json!({"wall_ms": 500, "idle_ms": 60000});
            ("claude_uncached_wall", "deadline")
        }
        NoHandshake::Spawn => ("claude_uncached_spawn", "transport_lost"),
    };
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join(format!("{variant}.replay.json"));
    let bytes = serde_json::to_vec(&replay).map_err(|e| e.to_string())?;
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let pure = conformance_drive::Pure::run("claude", variant, &expect, &path)?;
    let unlinked = matches!(how, NoHandshake::Spawn);
    if unlinked {
        std::fs::remove_file(pure.case_dir.path().join(variant)).map_err(|e| e.to_string())?;
    }
    let outcome = pure.drive_then(&expect, &path, conformance_run::Knobs::default(), |pure| {
        let request = via_adapters::DescribeRequest {
            harness: Some("claude".to_owned()),
            model: Some("haiku".to_owned()),
            ..via_adapters::DescribeRequest::default()
        };
        let plan = pure.set.plan(&request).map_err(|e| format!("{e:?}"))?;
        let plan = serde_json::to_value(&plan).map_err(|e| e.to_string())?;
        if plan["version_status"] == "refused" {
            return Err(format!("{variant}: a refusal was cached: {plan}"));
        }
        Ok(())
    })?;
    let turn = outcome.turns.first().ok_or("no turn")?;
    if turn.accepted
        || turn.error.as_deref() != Some(error)
        || turn.instance.is_some()
        || outcome.launches != u64::from(!unlinked)
    {
        return Err(format!(
            "{variant}: accepted {}, error {:?}, instance {:?}, launches {}",
            turn.accepted, turn.error, turn.instance, outcome.launches
        ));
    }
    Ok(())
}

#[test]
fn claude_refusal_never_cached_on_exit() {
    never_cached(NoHandshake::Exit).unwrap();
}

#[test]
fn claude_refusal_never_cached_on_wall() {
    never_cached(NoHandshake::Wall).unwrap();
}

#[test]
fn claude_refusal_never_cached_on_spawn_failure() {
    never_cached(NoHandshake::Spawn).unwrap();
}

/// S1 rule 4 (review r1 #2): the daemon force decides the turn's outcome
/// (`force_stop`) even when a normalizer verdict posted its abort first;
/// health keeps the verdict's first cause. Here a refused handshake's
/// abort sent the interrupt, which the fake never answers; the force
/// comes while the fake waits (it exits at the anchor's SIGTERM).
#[test]
fn claude_force_outranks_verdict() {
    let name = "claude_preflight_pure_version_no_receipt";
    let mut replay = replay_of(name).unwrap();
    let steps = replay["steps"].as_array_mut().unwrap();
    // The prompt, the pause, init and the interrupt.
    steps.truncate(4);
    steps.push(json!({"await_signal": {"signal": "SIGTERM"}}));
    steps.push(json!({"exit": {"code": 143, "stderr": ""}}));
    let mut expect = conformance_expect::load(&fixtures(), name).unwrap();
    let wanted = &mut expect["turns"][0]["expect"];
    wanted["error"] = json!("force_stop");
    wanted["exit"] = json!({"code": 143, "signal": null});
    let knobs = conformance_run::Knobs {
        force_on: Some("at 5 launch 1"),
        ..conformance_run::Knobs::default()
    };
    check_variant("claude_force_after_verdict", &replay, &expect, knobs).unwrap();
}

/// C2 A1 (review r1 #3): Core stalls past the driver's stall bound (10 s)
/// while the vendor runs; the driver closes the hop and the private
/// connection fails `overflow`, which interrupts the vendor before Host
/// closes it, in AD19's order: the stall is an internal stop order (the
/// interrupt, its terminal awaited, stdin EOF, then the graceful close),
/// escalated by the cleanup allowance as any stop order is. The
/// replay requires the interrupt (a force close without it ends the fake
/// by its SIGTERM) and EOF only after the terminal. The terminal Route
/// read is kept beside the overflow (AD4); its receipt never reached the
/// normalizer (the hop was closed), so it is no acknowledged cancel.
#[test]
fn claude_observation_stall_interrupts() {
    let base = replay_of("claude_lazy_init_acceptance").unwrap();
    // The prompt, the vendor's pause and init.
    let mut steps = base["steps"].as_array().unwrap()[..3].to_vec();
    // Past the session channel's 1024 items.
    for n in 0..1100 {
        steps.push(emit(&json!({
            "type": "assistant",
            "message": {"id": format!("msg_{n}"), "role": "assistant",
                "content": [{"type": "text", "text": "."}]},
            "session_id": "${sid}",
        })));
    }
    steps.push(json!({"expect": {
        "line": {"type": "control_request", "request": {"subtype": "interrupt"}},
        "capture": {"rid": "/request_id"},
    }}));
    steps.push(json!({"emit": {"line": "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":${rid},\"response\":{\"still_queued\":[]}}}"}}));
    steps.push(emit(&json!({
        "type": "result", "subtype": "error_during_execution", "is_error": true,
        "session_id": "${sid}", "stop_reason": "end_turn", "terminal_reason": "aborted_tools",
    })));
    steps.push(json!({"await_eof": {}}));
    let mut replay = base;
    replay["deadline_ms"] = json!(30000);
    replay["steps"] = json!(steps);
    let mut expect = conformance_expect::load(&fixtures(), "claude_lazy_init_acceptance").unwrap();
    expect["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": "overflow"});
    let turn = &mut expect["turns"][0];
    turn["gates"] = json!([]);
    turn["deadlines"] = json!({"wall_ms": 60000, "idle_ms": 60000});
    let wanted = &mut turn["expect"];
    for (field, value) in [
        (
            "terminal",
            json!({"status": "failed", "stop_reason": "error", "class_hint": "vendor_error",
                "vendor_code": "aborted_tools"}),
        ),
        ("usage", Value::Null),
        ("final_text", Value::Null),
        ("error", json!("overflow")),
        (
            "observations_include",
            json!([{"kind": "session.vendor_identity_confirmed", "generation": 1}, "turn.accepted"]),
        ),
        ("observations_exclude", json!(["final_text"])),
    ] {
        wanted[field] = value;
    }
    let knobs = conformance_run::Knobs {
        stall_consumer: true,
        ..conformance_run::Knobs::default()
    };
    check_variant("claude_stall_interrupt", &replay, &expect, knobs).unwrap();
}

/// S1 rule 3 (review r2 #4): a natural terminal decoded before the wall
/// is protected before it waits for read-ahead room. Core holds its
/// observations, so the session channel fills and about 3.6 MiB of
/// assistant messages wait behind the blocked hop when a 0.9 MiB result
/// is decoded: it needs room it does not have. A cancel ordered during
/// that wait sends no interrupt (the fake never reads a second line), and
/// the wall, passing during it, takes the late path: Host force-closes
/// the group (the fake exits on its SIGTERM) while the held messages, the
/// result last, are delivered once Core drains, and the turn keeps its
/// result.
#[test]
fn claude_terminal_protected_before_room() {
    let name = "claude_lazy_init_acceptance";
    let base = replay_of(name).unwrap();
    let mut steps = base["steps"].as_array().unwrap()[..3].to_vec();
    // Past the session channel's 1024 items: the hop blocks.
    for n in 0..1100 {
        steps.push(emit(&json!({
            "type": "assistant",
            "message": {"id": format!("msg_{n}"), "role": "assistant",
                "content": [{"type": "text", "text": "."}]},
            "session_id": "${sid}",
        })));
    }
    // Paced, so Wire's own queue (4 MiB) never holds the burst.
    let pause = json!({"delay": {"ms": 200}});
    steps.push(pause.clone());
    let large = "x".repeat(900 * 1024);
    for n in 0..4 {
        steps.push(pause.clone());
        steps.push(emit(&json!({
            "type": "assistant",
            "message": {"id": format!("msg_large_{n}"), "role": "assistant",
                "content": [{"type": "text", "text": large}]},
            "session_id": "${sid}",
        })));
    }
    let (at, line) = emit_step(&mut base.clone(), None, "\"type\":\"result\"").unwrap();
    assert_eq!(at, 6);
    let mut result: Value = serde_json::from_str(&line).unwrap();
    result["result"] = json!(large);
    steps.push(pause);
    steps.push(emit(&result));
    steps.push(json!({"await_signal": {"signal": "SIGTERM"}}));
    steps.push(json!({"exit": {"code": 143, "stderr": ""}}));
    let mut replay = base;
    replay["steps"] = json!(steps);
    let mut expect = conformance_expect::load(&fixtures(), name).unwrap();
    let turn = &mut expect["turns"][0];
    turn["gates"] = json!([]);
    turn["deadlines"] = json!({"wall_ms": 2500, "idle_ms": 60000});
    let wanted = &mut turn["expect"];
    wanted["final_text"] = json!([large]);
    wanted["exit"] = json!({"code": 143, "signal": null});
    let knobs = conformance_run::Knobs {
        hold_for: Some(std::time::Duration::from_millis(3000)),
        stop_after: Some(std::time::Duration::from_millis(1800)),
        ..conformance_run::Knobs::default()
    };
    let variant = "claude_terminal_room";
    conformance_expect::validate(&expect).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("{variant}.replay.json"));
    std::fs::write(&path, serde_json::to_vec(&replay).unwrap()).unwrap();
    let mut progress = String::new();
    let outcome = conformance_drive::Pure::run("claude", variant, &expect, &path)
        .unwrap()
        .drive_then(&expect, &path, knobs, |pure| {
            progress = std::fs::read_to_string(pure.case_file("progress")).unwrap_or_default();
            Ok(())
        })
        .unwrap();
    assert!(
        !progress.lines().any(|line| line == "read 2 launch 1"),
        "an interrupt followed the decoded result:\n{progress}"
    );
    conformance_expect::check(&expect, &outcome).unwrap();
}

/// Carry-item 1 (Claude ruling C1, packet §5): a session-cumulative cost
/// lower than the session's last one is an unexpected counter reset: the
/// turn warns `cost_counter_reset` (outside C1 §5's closed list, so the
/// checker's schema cannot state it: Core keeps it as a durable `warning`
/// event only) and reports the vendor's value, never a negative delta. A
/// rising cost warns nothing.
#[test]
fn claude_cost_counter_reset_warns() {
    let name = "claude_fifo_busy_input";
    let reset = |outcome: &Outcome, turn: usize| {
        outcome.turns[turn].observations.iter().any(|observation| {
            observation["kind"] == "warning" && observation["code"] == "cost_counter_reset"
        })
    };
    let expect = conformance_expect::load(&fixtures(), name).unwrap();
    let rising = drive(name, &expect).unwrap();
    assert!(
        !reset(&rising, 0) && !reset(&rising, 1),
        "a rising cost warned"
    );
    let mut replay = replay_of(name).unwrap();
    let (at, line) = emit_step(&mut replay, Some(1), "\"type\":\"result\"").unwrap();
    let lowered = line.replace("\"total_cost_usd\":0.002", "\"total_cost_usd\":0.0005");
    assert_ne!(lowered, line);
    replay["lifetimes"][1]["steps"][at] = json!({"emit": {"line": lowered}});
    let mut expect = expect;
    let wanted = &mut expect["turns"][1]["expect"];
    wanted["terminal"]["cost"]["usd"] = json!(0.0005);
    // The warning is the adapter's: the checker's closed set refuses it.
    wanted.as_object_mut().unwrap().remove("warnings");
    let outcome = drive_variant(
        "claude_cost_reset",
        &replay,
        &expect,
        conformance_run::Knobs::default(),
    )
    .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
    assert!(!reset(&outcome, 0), "turn 0 warned");
    assert!(
        reset(&outcome, 1),
        "turn 1 did not warn: {:?}",
        outcome.turns[1].observations
    );
    assert_eq!(
        outcome.turns[1].warnings,
        ["config_switch_unverified", "cost_counter_reset"]
    );
}

/// Carry-item 2 (Q9): `vendor.request_declined` is reported only once the
/// whole decline was written. With the decline's write failed (failpoint
/// `routes.claude.decline`, `fail_io`), c11b's request is unanswered: no
/// decline is reported, no denial is suppressed for it, and the turn
/// fails closed (`protocol`, the group force-closed: the fake takes the
/// anchor's SIGTERM and exits on its own).
#[cfg(feature = "test-failpoints")]
#[test]
fn claude_decline_reported_after_write() {
    use std::os::unix::fs::DirBuilderExt;
    let points = tempfile::tempdir().unwrap();
    let dir = points.path().join("points");
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let token = "c2-decline-token";
    std::fs::write(
        dir.join("routes.claude.decline.json"),
        json!({"token": token, "occurrence": 1, "action": "fail_io"}).to_string(),
    )
    .unwrap();
    via_store::failpoint::activate(&dir, token).unwrap();
    let name = "c11b_stdio_prompt";
    let mut replay = replay_of(name).unwrap();
    let (at, _) = emit_step(&mut replay, None, "\"control_request\"").unwrap();
    let steps = replay["steps"].as_array_mut().unwrap();
    steps.truncate(at + 1);
    steps.push(json!({"await_signal": {"signal": "SIGTERM"}}));
    steps.push(json!({"exit": {"code": 143, "stderr": ""}}));
    let mut expect = conformance_expect::load(&fixtures(), name).unwrap();
    expect["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": "protocol"});
    let wanted = &mut expect["turns"][0]["expect"];
    for (field, value) in [
        ("terminal", Value::Null),
        ("usage", Value::Null),
        ("final_text", Value::Null),
        ("error", json!("protocol")),
        ("exit", json!({"code": 143, "signal": null})),
        (
            "observations_include",
            json!([{"kind": "session.vendor_identity_confirmed", "generation": 1}, "turn.accepted"]),
        ),
        (
            "observations_exclude",
            json!(["vendor.request_declined", "action.denied"]),
        ),
        (
            "observation_counts",
            json!({"turn.accepted": 1, "vendor.request_declined": 0, "action.denied": 0}),
        ),
    ] {
        wanted[field] = value;
    }
    let knobs = conformance_run::Knobs::default();
    check_variant("claude_decline_unwritten", &replay, &expect, knobs).unwrap();
    assert!(
        dir.join("routes.claude.decline.1.ack").exists(),
        "the failpoint never hit"
    );
}

/// The `permission_denials` of a result past the normalizer's tracked
/// bytes (256 KiB): 300 distinct 1000-byte call IDs.
fn overflowing_denials() -> Value {
    (0..300)
        .map(|n| json!({"tool_name": "Edit", "tool_use_id": format!("toolu_{n:04}_{}", "x".repeat(990))}))
        .collect()
}

/// Carry-items 4 and 5: a tracking overflow (C1's sticky `Overflow`)
/// reaches the driver's health (`failed`, `overflow`) and the turn's
/// failure class (`overflow`); a terminal already read stays beside the
/// failure (AD4), with the result's text. The overflow is at the result
/// itself: its denials pass the tracked bytes, so only those admitted
/// before it are reported.
#[test]
fn claude_overflow_keeps_terminal() {
    let name = "claude_lazy_init_acceptance_result_only";
    let mut replay = replay_of(name).unwrap();
    let (at, line) = emit_step(&mut replay, None, "\"type\":\"result\"").unwrap();
    let mut result: Value = serde_json::from_str(&line).unwrap();
    result["permission_denials"] = overflowing_denials();
    replay["steps"][at] = emit(&result);
    let mut expect = conformance_expect::load(&fixtures(), name).unwrap();
    expect["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": "overflow"});
    let wanted = &mut expect["turns"][0]["expect"];
    wanted["error"] = json!("overflow");
    wanted["observations_include"] = json!([
        {"kind": "session.vendor_identity_confirmed", "generation": 1},
        "turn.accepted",
        "action.denied",
        "final_text",
    ]);
    let knobs = conformance_run::Knobs::default();
    check_variant("claude_overflow_terminal", &replay, &expect, knobs).unwrap();
}

/// Carry-item 4 before any terminal: tool calls past the tracked bytes
/// end the turn `overflow` with health latched, and the adapter stops the
/// vendor with the AD19 sequence (the interrupt, the result, stdin EOF).
#[test]
fn claude_overflow_before_terminal() {
    let name = "claude_lazy_init_acceptance_result_only";
    let base = replay_of(name).unwrap();
    let mut steps = base["steps"].as_array().unwrap()[..2].to_vec();
    steps.push(replay_of("claude_lazy_init_acceptance").unwrap()["steps"][2].clone());
    for n in 0..300 {
        steps.push(emit(&json!({
            "type": "assistant",
            "message": {"id": format!("msg_{n}"), "role": "assistant", "content": [{
                "type": "tool_use", "id": format!("toolu_{n:04}"), "name": "Read",
                "input": {"file_path": format!("/work/{n:04}/{}", "p".repeat(990))},
            }]},
            "session_id": "${sid}",
        })));
    }
    steps.push(json!({"expect": {
        "line": {"type": "control_request", "request": {"subtype": "interrupt"}},
        "capture": {"rid": "/request_id"},
    }}));
    steps.push(json!({"emit": {"line": "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":${rid},\"response\":{\"still_queued\":[]}}}"}}));
    steps.push(emit(&json!({
        "type": "result", "subtype": "error_during_execution", "is_error": true,
        "session_id": "${sid}", "stop_reason": "tool_use", "terminal_reason": "aborted_tools",
    })));
    steps.push(json!({"await_eof": {}}));
    let mut replay = base;
    replay["steps"] = json!(steps);
    let mut expect = conformance_expect::load(&fixtures(), name).unwrap();
    expect["sessions"]["main"]["health"] = json!({"state": "failed", "first_cause": "overflow"});
    let wanted = &mut expect["turns"][0]["expect"];
    for (field, value) in [
        ("error", json!("overflow")),
        ("final_text", Value::Null),
        ("usage", Value::Null),
        (
            "instance",
            json!({"vendor_version": "2.1.285", "version_status": "tested"}),
        ),
        (
            "observations_include",
            json!([{"kind": "session.vendor_identity_confirmed", "generation": 1}, "turn.accepted"]),
        ),
        ("observations_exclude", json!(["final_text"])),
    ] {
        wanted[field] = value;
    }
    wanted.as_object_mut().unwrap().remove("terminal");
    wanted["unasserted"] = json!([{"field": "terminal", "why": "read after the overflow: the route's retained result (AD4), pinned by claude_overflow_keeps_terminal's at-result case"}]);
    let knobs = conformance_run::Knobs::default();
    check_variant("claude_overflow_early", &replay, &expect, knobs).unwrap();
}

/// x.3.2 G8 (C2 §2 `ParamSizes`, ruling Q3): instructions or an
/// `output_schema` past Linux's per-argument limit (128 KiB with its NUL)
/// cannot travel as `--append-system-prompt` or `--json-schema`, so the
/// plan refuses them `invalid_params` naming the member, before any
/// receipt or launch. Host's 64 KiB launch request binds first (critical
/// r1 #1): the two at that limit together are refused naming the larger
/// (`instructions` on a tie), and 4 KiB of each plans.
#[test]
fn claude_argv_budget_refused_before_launch() {
    const ARG_MAX: usize = 128 * 1024 - 1;
    let base = conformance_expect::load(&fixtures(), "c0_bad_effort").unwrap();
    let schema_of = |len: usize| {
        // `{"description":"…"}` encodes to 18 bytes besides the text.
        json!({"description": "d".repeat(len - 18)})
    };
    for (member, instructions, schema) in [
        ("instructions", json!("i".repeat(ARG_MAX + 1)), Value::Null),
        ("output_schema", Value::Null, schema_of(ARG_MAX + 1)),
    ] {
        let mut expect = base.clone();
        expect["sessions"]["main"]["instructions"] = instructions;
        let turn = &mut expect["turns"][0];
        turn["params"]["effort"] = json!("low");
        turn["params"]["output_schema"] = schema;
        turn["expect"]["plan_refusal"] = json!(format!("invalid_param:{member}"));
        conformance_expect::validate(&expect).unwrap();
        check_expect("c0_bad_effort", &expect).unwrap_or_else(|e| panic!("{member}: {e}"));
    }
    let mut expect = base.clone();
    expect["sessions"]["main"]["instructions"] = json!("i".repeat(ARG_MAX));
    let turn = &mut expect["turns"][0];
    turn["params"]["effort"] = json!("low");
    turn["params"]["output_schema"] = schema_of(ARG_MAX);
    turn["expect"]["plan_refusal"] = json!("invalid_param:instructions");
    conformance_expect::validate(&expect).unwrap();
    check_expect("c0_bad_effort", &expect).unwrap();
    // Under both limits, both plan: the turn is left to the run half.
    let mut expect = base;
    expect["sessions"]["main"]["instructions"] = json!("i".repeat(4096));
    expect["turns"][0]["params"]["effort"] = json!("low");
    expect["turns"][0]["params"]["output_schema"] = schema_of(4096);
    let replay = fixtures().join("c0_bad_effort.replay.json");
    let pure = conformance_drive::Pure::run("claude", "c0_bad_effort", &expect, &replay).unwrap();
    assert_eq!(pure.pending, [0]);
    assert_eq!(pure.outcome.turns[0].plan_refusal, None);
}

/// Every case, named fixture and vendor record, sorted.
fn listed() -> Vec<&'static str> {
    let mut listed: Vec<&str> = CASES.to_vec();
    listed.extend(NAMED);
    listed.extend(VENDOR_RECORDS.iter().map(|(name, _)| *name));
    listed.sort_unstable();
    listed
}

/// Green now: every expectation file is a case test or a vendor record (not
/// both, each record with its reason), has a replay fixture, names this
/// harness, validates against the unified schema and gates only
/// `await_signal` steps of its replay.
#[test]
fn conformance_claude_cases_match_fixture_files() {
    for (name, why) in VENDOR_RECORDS {
        assert!(!why.is_empty(), "{name}: a vendor record needs its reason");
        assert!(
            !CASES.contains(&name),
            "{name}: both a case and a vendor record"
        );
    }
    assert!(PURE_CASES.iter().all(|name| CASES.contains(name)));
    let dir = fixtures();
    assert_eq!(conformance_expect::case_names(&dir).unwrap(), listed());
    for name in listed() {
        let expect = conformance_expect::load(&dir, name).unwrap();
        assert_eq!(expect["harness"], "claude", "{name}: harness");
        conformance_expect::validate(&expect).unwrap_or_else(|e| panic!("{name}: {e}"));
        let replay = std::fs::read(dir.join(format!("{name}.replay.json")))
            .unwrap_or_else(|e| panic!("{name}: replay fixture: {e}"));
        let replay: Value = serde_json::from_slice(&replay).unwrap();
        conformance_expect::gates_resolve(&expect, &replay)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

/// A named change to an ideal outcome.
type Mutation = (&'static str, fn(&mut Outcome));

/// Green now: the checker accepts the ideal outcome of every case and
/// reports a change in any part of it the case states.
#[test]
fn conformance_claude_checker_detects_each_difference() {
    let dir = fixtures();
    for name in CASES.iter().chain(NAMED) {
        let expect = conformance_expect::load(&dir, name).unwrap();
        conformance_expect::check(&expect, &conformance_expect::ideal(&expect))
            .unwrap_or_else(|e| panic!("{name}: ideal outcome refused:\n{e}"));
        let mutations: [Mutation; 17] = [
            ("usage", |o| {
                o.turns[0].usage = Some(json!({"input_tokens": 1}));
            }),
            ("final_text", |o| {
                o.turns[0].final_text = Some(vec!["other".to_owned()]);
            }),
            ("steer", |o| o.turns[0].steer.push("injected".to_owned())),
            ("launches", |o| o.launches += 1),
            ("accepted", |o| o.turns[0].accepted = !o.turns[0].accepted),
            ("terminal", |o| {
                o.turns[0].terminal = Some(json!({"status": "other"}));
            }),
            ("stop_facts", |o| {
                o.turns[0].stop_facts = Some(json!({"acknowledged": false}));
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

/// Green: `resume_mismatch` is an `AdapterError` kind (C2 §2 identity), and
/// a start rejection is never stated as an error: it is `rejected`.
#[test]
fn conformance_claude_error_kinds_follow_c2() {
    let base = conformance_expect::load(&fixtures(), "c1b_resume_mismatch").unwrap();
    assert_eq!(base["turns"][0]["expect"]["error"], "resume_mismatch");
    conformance_expect::validate(&base).unwrap();
    for wrong in ["rejected", "session_gone", "resume.mismatch"] {
        let mut expect = base.clone();
        expect["turns"][0]["expect"]["error"] = json!(wrong);
        let refused = conformance_expect::validate(&expect);
        assert!(
            refused.as_ref().is_err_and(|e| e.contains("expect.error:")),
            "error {wrong}: {refused:?}"
        );
    }
}

/// Green: an accepted turn that asserts identity confirmation must order it
/// before acceptance (C2 §2 identity), so an expectation cannot drop the
/// order and still validate.
#[test]
fn conformance_claude_identity_order_is_required() {
    let base = conformance_expect::load(&fixtures(), "c1a").unwrap();
    conformance_expect::validate(&base).unwrap();
    for order in [json!([]), json!(["turn.accepted"])] {
        let mut expect = base.clone();
        expect["turns"][0]["expect"]["observations_order"] = order.clone();
        let refused = conformance_expect::validate(&expect);
        assert!(
            refused.as_ref().is_err_and(|e| e.contains("must precede")),
            "order {order}: {refused:?}"
        );
    }
}

/// Green: `final_text` compares the assembled text, not piece boundaries
/// (C2 §4; review F4): c1a's text split in two pieces passes, other text
/// fails.
#[test]
fn conformance_claude_final_text_compares_assembled_text() {
    let expect = conformance_expect::load(&fixtures(), "c1a").unwrap();
    let whole = expect["turns"][0]["expect"]["final_text"][0]
        .as_str()
        .unwrap()
        .to_owned();
    let (head, tail) = whole.split_at(whole.len() / 2);
    let mut outcome = conformance_expect::ideal(&expect);
    outcome.turns[0].final_text = Some(vec![head.to_owned(), tail.to_owned()]);
    conformance_expect::check(&expect, &outcome).unwrap();
    outcome.turns[0].final_text = Some(vec![tail.to_owned(), head.to_owned()]);
    assert!(conformance_expect::check(&expect, &outcome).is_err());
}

/// Green: the shared replay-exit check accepts each fixture's own end and
/// refuses a signal death, the replay's failure code and any other end
/// (review r2 #8). [`drive`] calls it for every launch.
#[test]
fn conformance_claude_replay_exit_is_judged() {
    let checked = conformance_expect::replay_exit_self_check(&fixtures()).unwrap();
    assert!(checked > 0, "no replay lifetimes checked");
}
