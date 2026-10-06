//! C2 conformance cases for the Pi RPC adapter, the packet's §8 fixture
//! tests (`via-jt8.3.1`; `docs/specs/vendors/pi.md`), red until
//! `via-jt8.3.2`'s adapter makes them pass.
//!
//! Each named test drives the real `pi-rpc` route against `via-fake-agent`
//! replaying Pi's RPC records (packet §5.1): the canonical shapes are
//! fixtures in `crates/via-adapters/tests/fixtures/pi/`, and the variants a
//! test needs are built here from the same record builders and replayed
//! from a scratch directory, never written under the fixtures. Selection:
//! `cargo nextest run --locked --workspace -E 'test(/^pi_/)'`.
//!
//! # Fixture argv
//!
//! The recipe of packet §4.1 with the harness's default inheritance (OD2:
//! skills and instruction files on, so neither `-ns` nor `-nc`):
//! `--mode rpc --model openai/gpt-6-luna [--thinking L] --session-dir
//! {capture sdir} (--session-id | --session) <ID> --tools
//! read,bash,edit,write --no-approve -ne -np [--append-system-prompt
//! {capture ins}] --offline`. The ID is pinned literally: the derived ID of
//! the harness's first session `s_000000000001` (packet §2.2), so a driver
//! that derives another ID, or continues with the wrong flag, fails the
//! replay's argv check.
//!
//! # Versions
//!
//! The fake is a native binary with no `package.json` near it, so a turn's
//! instance reports `vendor_version: null`, `untested` (packet §3); only
//! `pi_version_read` lays out a package and states the instance.

#[path = "support/conformance_drive.rs"]
mod conformance_drive;
#[path = "support/conformance_expect.rs"]
mod conformance_expect;
#[path = "support/conformance_run.rs"]
mod conformance_run;

use std::path::{Path, PathBuf};

use conformance_drive::Pure;
use conformance_expect::Outcome;
use conformance_run::Knobs;
use serde_json::{Value, json};

/// The bead whose adapter makes these cases pass.
const ADAPTER_BEAD: &str = "via-jt8.3.2";

/// The derived Pi session ID of the harness's first session,
/// `s_000000000001`: SHA-256 over `"via pi session s_000000000001"`, laid
/// out as a version-4 UUID (packet §2.2).
const SID: &str = "187d2602-5d80-45de-a2b2-2080d91eb589";

/// Another Pi session's ID, as `get_state` might report it.
const OTHER_SID: &str = "01a1011e-37f7-7153-82f3-83ed1f708ab9";

/// The one live-qualified model (packet §1), provider-qualified.
const MODEL: &str = "openai/gpt-6-luna";

/// The recipe's explicit tool list (packet §4.1).
const TOOLS: &str = "read,bash,edit,write";

/// The fixture deadline of every built lifetime.
const DEADLINE_MS: u64 = 20_000;

/// The failure text a refused prompt carries in Pi (E29): never copied.
const NO_KEY: &str = "No API key found for the selected model.";

/// A key-like fragment as Pi's 401 echoes it (E53), masked as there; it
/// must never leave the vendor record.
const KEY_FRAGMENT: &str = "zz-via-p************-000";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../via-adapters/tests/fixtures/pi")
}

// ---------------------------------------------------------------------
// Record builders (packet §5.1).
// ---------------------------------------------------------------------

/// How one launch is started (packet §4.1).
#[derive(Clone, Copy, Default)]
struct Argv {
    /// `--session ID` (a confirmed session) instead of `--session-id ID`.
    resume: bool,
    /// `--thinking LEVEL`.
    thinking: Option<&'static str>,
    /// `--append-system-prompt <file>`.
    instructions: bool,
}

/// The recipe's argv for `launch`.
fn argv(launch: Argv) -> Value {
    let mut argv = vec![
        json!("--mode"),
        json!("rpc"),
        json!("--model"),
        json!(MODEL),
    ];
    if let Some(level) = launch.thinking {
        argv.extend([json!("--thinking"), json!(level)]);
    }
    argv.extend([json!("--session-dir"), json!({"capture": "sdir"})]);
    let flag = if launch.resume {
        "--session"
    } else {
        "--session-id"
    };
    argv.extend([json!(flag), json!(SID)]);
    argv.extend(["--tools", TOOLS, "--no-approve", "-ne", "-np"].map(|arg| json!(arg)));
    if launch.instructions {
        argv.extend([json!("--append-system-prompt"), json!({"capture": "ins"})]);
    }
    argv.push(json!("--offline"));
    Value::Array(argv)
}

/// One lifetime (a launch's fixture) of `argv` and `steps`.
fn lifetime(argv: Value, steps: Vec<Value>) -> Value {
    let mut lifetime = json!({"deadline_ms": DEADLINE_MS});
    lifetime["argv"] = argv;
    lifetime["steps"] = Value::Array(steps);
    lifetime
}

/// A replay of one launch.
fn single(source: &str, argv: Value, steps: Vec<Value>) -> Value {
    let mut replay = lifetime(argv, steps);
    replay["source"] = json!(source);
    replay
}

/// A replay of several launches, one lifetime each.
fn lifetimes(source: &str, lifetimes: Vec<Value>) -> Value {
    let mut replay = json!({"source": source});
    replay["lifetimes"] = Value::Array(lifetimes);
    replay
}

/// Removes `key` from the object `value`: the field is then unstated.
fn unset(value: &mut Value, key: &str) {
    if let Some(object) = value.as_object_mut() {
        object.remove(key);
    }
}

/// An emit step of `line`, verbatim.
fn emit_line(line: &str) -> Value {
    json!({"emit": {"line": line}})
}

/// An emit step of one record.
fn emit(record: &Value) -> Value {
    emit_line(&record.to_string())
}

/// An expect step taking one line that holds `line`.
fn expect_line(line: Value) -> Value {
    let mut step = json!({"expect": {}});
    step["expect"]["line"] = line;
    step
}

/// An expect step capturing the line's `id` as `name`.
fn expect_id(line: Value, name: &str) -> Value {
    let mut step = json!({"expect": {"capture": {name: "/id"}}});
    step["expect"]["line"] = line;
    step
}

/// A response to the command whose captured `id` is `id`: `body` is its
/// `data` on success, its `error` otherwise.
fn reply(id: &str, command: &str, success: bool, body: &Value) -> String {
    let mut record = json!({
        "id": "@ID@", "type": "response", "command": command, "success": success,
    });
    if success {
        if !body.is_null() {
            record["data"] = body.clone();
        }
    } else {
        record["error"] = body.clone();
    }
    record
        .to_string()
        .replacen("\"@ID@\"", &format!("${{{id}}}"), 1)
}

/// What the handshake's three replies say (packet §2.1 step 3).
#[derive(Clone)]
struct State {
    session_id: &'static str,
    provider: &'static str,
    model: &'static str,
    thinking: &'static str,
    models: Value,
    commands: Value,
}

impl Default for State {
    fn default() -> Self {
        Self {
            session_id: SID,
            provider: "openai",
            model: "gpt-6-luna",
            thinking: "off",
            models: json!([
                {"provider": "openai", "id": "gpt-6-luna", "name": "GPT-6 Luna"},
                {"provider": "openai", "id": "gpt-6-sol", "name": "GPT-6 Sol"},
            ]),
            commands: json!([
                {"name": "skill:review", "description": "Review a change", "source": "skill"},
            ]),
        }
    }
}

/// The session file Pi names for `session_id` in the session directory.
fn session_file(session_id: &str) -> String {
    format!("${{sdir}}/2026-10-06T00-00-00-000Z_{session_id}.jsonl")
}

/// The three handshake commands, then their replies, in order.
fn handshake(state: &State) -> Vec<Value> {
    let get_state = json!({
        "model": {"provider": state.provider, "id": state.model, "name": "GPT-6 Luna"},
        "thinkingLevel": state.thinking,
        "isStreaming": false,
        "isCompacting": false,
        "sessionFile": session_file(state.session_id),
        "sessionId": state.session_id,
        "messageCount": 0,
        "pendingMessageCount": 0,
    });
    vec![
        expect_id(json!({"type": "get_state"}), "gs"),
        expect_id(json!({"type": "get_available_models"}), "gm"),
        expect_id(json!({"type": "get_commands"}), "gc"),
        emit_line(&reply("gs", "get_state", true, &get_state)),
        emit_line(&reply(
            "gm",
            "get_available_models",
            true,
            &json!({"models": state.models}),
        )),
        emit_line(&reply(
            "gc",
            "get_commands",
            true,
            &json!({"commands": state.commands}),
        )),
    ]
}

/// The prompt line VIA writes for `text`.
fn expect_prompt(text: &str) -> Value {
    expect_id(json!({"type": "prompt", "message": text}), "p")
}

/// The prompt's `started` reply: acceptance (packet §2.1 step 4).
fn started() -> Value {
    emit_line(&reply(
        "p",
        "prompt",
        true,
        &json!({"disposition": "started"}),
    ))
}

/// The prompt line and its `started` reply.
fn prompt(text: &str) -> Vec<Value> {
    vec![expect_prompt(text), started()]
}

/// `agent_start`, `turn_start` and the user message's echo of `text`.
fn echo(text: &str) -> Vec<Value> {
    let user = json!({"role": "user", "content": [{"type": "text", "text": text}], "timestamp": 1});
    vec![
        emit(&json!({"type": "agent_start"})),
        emit(&json!({"type": "turn_start"})),
        emit(&json!({"type": "message_start", "message": user})),
        emit(&json!({"type": "message_end", "message": user})),
    ]
}

/// One model call's usage, as Pi reports it (E24): `input` excludes cache
/// reads.
fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost: f64) -> Value {
    json!({
        "input": input, "output": output, "cacheRead": cache_read,
        "cacheWrite": cache_write, "reasoning": 0,
        "totalTokens": input + output + cache_read + cache_write,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": cost},
    })
}

/// Usage of a call that reported none: all zero (E26).
fn zero_usage() -> Value {
    usage(0, 0, 0, 0, 0.0)
}

/// The canonical call's usage: 80 + 20 cached in, 10 out, $0.5.
fn canonical_usage() -> Value {
    usage(80, 10, 20, 0, 0.5)
}

/// An assistant message with `content`, `stop` and `usage`.
fn assistant(content: Value, stop: &str, usage: &Value, error: Option<&str>) -> Value {
    let mut message = json!({
        "role": "assistant", "api": "openai-responses",
        "provider": "openai", "model": "gpt-6-luna", "usage": usage,
        "stopReason": stop, "timestamp": 2,
    });
    message["content"] = content;
    if let Some(error) = error {
        message["errorMessage"] = json!(error);
    }
    message
}

/// The `message_end` of `message`.
fn message_end(message: &Value) -> Value {
    emit(&json!({"type": "message_end", "message": message}))
}

/// A `message_update` of the streaming event `event`.
fn update(event: Value) -> Value {
    let mut record = json!({
        "type": "message_update",
        // Cumulative; never a usage sample (packet §5.5).
        "usage": usage(70, 3, 0, 0, 0.25),
    });
    record["assistantMessageEvent"] = event;
    emit(&record)
}

/// One streamed text answer: start, the deltas, end, and its `message_end`
/// with `stop` and `usage`.
fn answer(text: &str, stop: &str, usage: &Value) -> Vec<Value> {
    let pending = assistant(json!([]), "pending", &zero_usage(), None);
    let mut steps = vec![
        emit(&json!({"type": "message_start", "message": pending})),
        update(json!({"type": "text_start", "contentIndex": 0})),
        update(json!({"type": "text_delta", "contentIndex": 0, "delta": text})),
        update(json!({"type": "text_end", "contentIndex": 0, "content": text})),
    ];
    let done = assistant(json!([{"type": "text", "text": text}]), stop, usage, None);
    steps.push(message_end(&done));
    steps
}

/// One tool call, `call_1` of `bash`, as an assistant `toolUse` message and
/// its execution start (the tool then runs).
fn tool_call(usage: &Value) -> Vec<Value> {
    let call = json!([{"type": "toolCall", "id": "call_1", "name": "bash",
        "arguments": {"command": "sleep 30"}}]);
    vec![
        update(
            json!({"type": "toolcall_start", "contentIndex": 0, "id": "call_1", "toolName": "bash"}),
        ),
        message_end(&assistant(call, "toolUse", usage, None)),
        emit(
            &json!({"type": "tool_execution_start", "toolCallId": "call_1",
            "toolName": "bash", "args": {"command": "sleep 30"}}),
        ),
    ]
}

/// The tool's end, its result message and the next call's `turn_start`.
fn tool_end(error: bool) -> Vec<Value> {
    let text = if error { "Command aborted" } else { "done\n" };
    let result = json!({"role": "toolResult", "toolCallId": "call_1", "toolName": "bash",
        "content": [{"type": "text", "text": text}], "isError": error, "timestamp": 3});
    vec![
        emit(
            &json!({"type": "tool_execution_end", "toolCallId": "call_1", "toolName": "bash",
            "result": {"content": [{"type": "text", "text": text}]}, "isError": error}),
        ),
        emit(&json!({"type": "message_start", "message": result})),
        emit(&json!({"type": "message_end", "message": result})),
        emit(
            &json!({"type": "turn_end", "message": {"role": "assistant"}, "toolResults": [result]}),
        ),
        emit(&json!({"type": "turn_start"})),
    ]
}

/// `turn_end`, `agent_end` and `agent_settled`; their bodies repeat the
/// last message, which is never normalized again (packet §5.1).
fn settle(last: &Value) -> Vec<Value> {
    vec![
        emit(&json!({"type": "turn_end", "message": last, "toolResults": []})),
        emit(&json!({"type": "agent_end", "messages": [last], "willRetry": false})),
        emit(&json!({"type": "agent_settled"})),
    ]
}

/// The step that seals a lifetime: stdin EOF after `agent_settled`.
fn eof() -> Value {
    json!({"await_eof": {}})
}

/// A whole completed turn on a fresh launch: handshake, prompt, the
/// answer `text` with the canonical usage, settlement and EOF.
fn completed_steps(state: &State, text_prompt: &str, text: &str) -> Vec<Value> {
    let done = assistant(
        json!([{"type": "text", "text": text}]),
        "stop",
        &canonical_usage(),
        None,
    );
    let mut steps = handshake(state);
    steps.extend(prompt(text_prompt));
    steps.extend(echo(text_prompt));
    steps.extend(answer(text, "stop", &canonical_usage()));
    steps.extend(settle(&done));
    steps.push(eof());
    steps
}

// ---------------------------------------------------------------------
// Expectation builders (the unified schema, `conformance_expect.rs`).
// ---------------------------------------------------------------------

/// A case of `turns` on the one session `main`.
fn case(source: &str, launches: u64, turns: Vec<Value>) -> Value {
    let mut case = json!({
        "source": source,
        "harness": "pi",
        "launches": launches,
        "sessions": {"main": session()},
    });
    case["turns"] = Value::Array(turns);
    case
}

/// The session `main`: the live model, no instructions.
fn session() -> Value {
    json!({"model": MODEL, "instructions": null, "cwd": "/work/project",
        "resume": null, "close": null})
}

/// A turn of `prompt` with `expect`.
fn turn(prompt: &str, expect: Value) -> Value {
    let mut turn = json!({
        "session": "main",
        "start_after": null,
        "params": {"prompt": prompt, "effort": null, "bound": null,
            "output_schema": null, "max_steps": null},
        "tool_grace_ms": null,
        "stop": null,
        "steer": [],
    });
    turn["expect"] = expect;
    turn
}

/// The identity confirmation of `SID` on connection `generation`: the
/// ordinal of its connection among the case's confirming connections.
fn confirmed(generation: u64) -> Value {
    json!({"kind": "session.vendor_identity_confirmed", "vendor_session_id": SID,
        "generation": generation})
}

/// The C1 usage of `samples` canonical calls.
fn canonical_turn_usage(samples: u64) -> Value {
    json!({"from": "samples", "input_tokens": 100 * samples,
        "cached_input_tokens": 20 * samples, "output_tokens": 10 * samples,
        "reasoning_output_tokens": 0, "total_tokens": 110 * samples, "scope": "turn"})
}

/// A turn usage whose every component is unknown (packet §5.5).
fn null_usage() -> Value {
    json!({"from": "samples", "input_tokens": null, "cached_input_tokens": null,
        "output_tokens": null, "reasoning_output_tokens": null, "total_tokens": null,
        "scope": "turn"})
}

/// An estimated turn cost of `usd`.
fn estimated(usd: f64) -> Value {
    json!({"usd": usd, "scope": "turn", "provenance": "estimated"})
}

/// The expectation of a completed turn confirmed on connection
/// `generation` (the ordinal [`confirmed`] states), its final
/// text `text`, one canonical call.
fn completed(generation: u64, text: &str) -> Value {
    json!({
        "plan_refusal": null,
        "rejected": null,
        "accepted": true,
        "terminal": {"status": "completed", "stop_reason": "end_turn",
            "vendor_stop_reason": "stop", "class_hint": null, "cost": estimated(0.5)},
        "usage": canonical_turn_usage(1),
        "final_text": [text],
        "error": null,
        "cleanup": "quiescent",
        "exit": {"code": 0, "signal": null},
        "journal_uncertain": false,
        "group_absent": true,
        "warnings": ["config_switch_unverified"],
        "observations_include": [confirmed(generation), "turn.accepted"],
        "observation_counts": {"turn.accepted": 1, "session.vendor_identity_confirmed": 1},
        "observations_order": ["session.vendor_identity_confirmed", "turn.accepted"],
    })
}

/// A turn that ends before acceptance: `rejected` or `error`, the exit
/// `exit` (null when nothing launched), never confirmed.
fn unaccepted(rejected: Option<&str>, error: Option<&str>, exit: Option<i64>) -> Value {
    let launched = exit.is_some();
    json!({
        "plan_refusal": null,
        "rejected": rejected,
        "accepted": false,
        "terminal": null,
        "final_text": null,
        "error": error,
        "cleanup": "quiescent",
        "exit": exit.map(|code| json!({"code": code, "signal": null})),
        "journal_uncertain": false,
        "group_absent": launched,
        "observations_exclude": ["session.vendor_identity_confirmed", "turn.accepted", "final_text"],
        "observation_counts": {"turn.accepted": 0},
    })
}

/// A turn refused at planning: nothing runs.
fn plan_refused(refusal: &str) -> Value {
    json!({"plan_refusal": refusal, "rejected": null, "accepted": false,
        "terminal": null, "error": null, "cleanup": null, "warnings": [],
        "observations_exclude": ["turn.accepted"], "observation_counts": {"turn.accepted": 0}})
}

// ---------------------------------------------------------------------
// Drivers.
// ---------------------------------------------------------------------

/// The pure half's adapter set, its state directory and case directory,
/// handed to a test's preparation before any turn runs.
type Prepare<'a> = Box<dyn FnOnce(&Pure) -> Result<(), String> + 'a>;

/// What a test reads back once every turn settled.
type Then<'a> = Box<dyn FnOnce(&Pure) -> Result<(), String> + 'a>;

/// Drives a built case: `replay` written to a scratch directory, the pure
/// half, `prepare`, then the run half with `knobs`, and `then` once every
/// turn settled.
fn drive_built(
    name: &str,
    replay: &Value,
    expect: &Value,
    knobs: Knobs,
    prepare: Prepare<'_>,
    then: Then<'_>,
) -> Result<Outcome, String> {
    conformance_expect::validate(expect).map_err(|e| format!("{name}: {e}"))?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join(format!("{name}.replay.json"));
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(replay).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let pure = Pure::run("pi", name, expect, &path)?;
    prepare(&pure)?;
    pure.drive_then(expect, &path, knobs, then)
        .map_err(|why| format!("{why} ({ADAPTER_BEAD} variant {name})"))
}

/// [`drive_built`] with no preparation or read-back, then the check.
fn check_built(
    name: &str,
    replay: &Value,
    expect: &Value,
    knobs: Knobs,
) -> Result<Outcome, String> {
    let outcome = drive_built(
        name,
        replay,
        expect,
        knobs,
        Box::new(|_| Ok(())),
        Box::new(|_| Ok(())),
    )
    .map_err(|e| format!("{name}: {e}"))?;
    conformance_expect::check(expect, &outcome).map_err(|e| format!("{name}:\n{e}"))?;
    Ok(outcome)
}

/// Drives fixture `name` from the fixture directory and checks it.
fn check_fixture(name: &str) -> Result<Outcome, String> {
    let expect = conformance_expect::load(&fixtures(), name)?;
    let replay = fixtures().join(format!("{name}.replay.json"));
    let outcome = Pure::run("pi", name, &expect, &replay)
        .and_then(|pure| pure.drive(&expect, &replay, Knobs::default()))
        .map_err(|e| format!("{name}: {e} ({ADAPTER_BEAD} case {name})"))?;
    conformance_expect::check(&expect, &outcome).map_err(|e| format!("{name}:\n{e}"))?;
    Ok(outcome)
}

/// Loads fixture `name`'s replay and expectation.
fn fixture(name: &str) -> Result<(Value, Value), String> {
    let path = fixtures().join(format!("{name}.replay.json"));
    let text = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let replay = serde_json::from_slice(&text).map_err(|e| format!("{name}: {e}"))?;
    Ok((replay, conformance_expect::load(&fixtures(), name)?))
}

/// Every observation of every turn, as text: what a redaction check scans.
fn observed_text(outcome: &Outcome) -> String {
    outcome
        .turns
        .iter()
        .flat_map(|turn| turn.observations.iter())
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The agent directory of a case's state (packet §4.2).
fn agent_dir(pure: &Pure) -> PathBuf {
    pure.state.path().join("vendor").join("pi").join("agent")
}

/// The evidence folder of turn `turn` of the first session.
fn evidence(pure: &Pure, turn: u64) -> PathBuf {
    pure.state
        .path()
        .join("state")
        .join("evidence")
        .join("s_000000000001")
        .join(turn.to_string())
}

// ---------------------------------------------------------------------
// §8 named tests.
// ---------------------------------------------------------------------

/// `pi_plan_pure`: `describe` starts nothing and reports the unread
/// version `null`/`untested`; steer is refused by name (`require`); the
/// per-turn values the route cannot honour (`max_steps`, `output_schema`,
/// the limited bounds, `network:false`, nonempty `extra_write_dirs`), an
/// effort Pi does not take, a prompt over 524,288 or instructions over
/// 262,144 JSON-encoded bytes, and vendor options are refused before any
/// receipt: no launch, no write.
#[test]
fn pi_plan_pure() {
    let outcome = check_fixture("pi_plan_pure").unwrap();
    assert_eq!(outcome.launches, 0);
    // The size limits, built: too large to keep in a fixture file. The
    // prompt's edge (exactly 524,288 bytes passes) is `pi_record_ceiling`'s.
    let (replay, _) = fixture("pi_plan_pure").unwrap();
    let mut expect = case(
        "pi_plan_pure_sizes",
        0,
        vec![turn(
            &"p".repeat(524_288 - 1),
            plan_refused("invalid_param:prompt"),
        )],
    );
    let mut instructions = turn("Say READY.", plan_refused("invalid_param:instructions"));
    instructions["session"] = json!("long");
    expect["turns"].as_array_mut().unwrap().push(instructions);
    let mut long = session();
    long["instructions"] = json!("i".repeat(262_144 - 1));
    expect["sessions"]["long"] = long;
    check_built("pi_plan_pure_sizes", &replay, &expect, Knobs::default()).unwrap();
}

/// `pi_version_read`: the version is `package.json`'s, found from the
/// resolved entry script (a symlink to it, under `dist/bundle/`, takes the
/// package root's file by the `dist` rule) before the launch, with no
/// version process; it is reported on a turn whose Pi exits before the
/// handshake, and `describe` reports it afterwards. A missing, oversize,
/// non-object or non-string file gives `null`/`untested`, and the turn
/// proceeds.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one packet §8 test: its variants side by side"
)]
fn pi_version_read() {
    let exited = |name: &str| {
        let replay = single(
            "synthetic (via-jt8.3.1): Pi exits before its handshake replies, as on a bad model (E30)",
            argv(Argv::default()),
            vec![
                expect_line(json!({"type": "get_state"})),
                expect_line(json!({"type": "get_available_models"})),
                expect_line(json!({"type": "get_commands"})),
                json!({"exit": {"code": 1, "stderr": "Error: Model not found.\n"}}),
            ],
        );
        let mut wanted = unaccepted(Some("protocol"), None, Some(1));
        wanted["instance"] = json!({"vendor_version": "1.0.2", "version_status": "untested"});
        (replay, case(name, 1, vec![turn("Say READY.", wanted)]))
    };
    // A package laid out as npm installs Pi: `<pkg>/package.json`,
    // `<pkg>/dist/package.json` (no version) and the entry
    // `<pkg>/dist/bundle/cli.js`, the case's link resolving to it.
    let layout = |package: Option<&'static [u8]>| -> Prepare<'static> {
        Box::new(move |pure: &Pure| {
            let root = pure.case_dir.path().join("lib").join("pi-coding-agent");
            let bundle = root.join("dist").join("bundle");
            std::fs::create_dir_all(&bundle).map_err(|e| e.to_string())?;
            if let Some(package) = package {
                std::fs::write(root.join("package.json"), package).map_err(|e| e.to_string())?;
            }
            std::fs::write(
                root.join("dist").join("package.json"),
                br#"{"type":"module"}"#,
            )
            .map_err(|e| e.to_string())?;
            let link = pure.fake_link();
            let entry = bundle.join("cli.js");
            let target = std::fs::read_link(&link).map_err(|e| e.to_string())?;
            std::fs::hard_link(&target, &entry)
                .or_else(|_| std::fs::copy(&target, &entry).map(|_| ()))
                .map_err(|e| e.to_string())?;
            std::fs::remove_file(&link).map_err(|e| e.to_string())?;
            std::os::unix::fs::symlink(&entry, &link).map_err(|e| e.to_string())
        })
    };
    let (replay, expect) = exited("pi_version_read");
    let described = std::cell::RefCell::new(None);
    let outcome = drive_built(
        "pi_version_read",
        &replay,
        &expect,
        Knobs::default(),
        layout(Some(
            br#"{"name":"@earendil-works/pi-coding-agent","version":"1.0.2"}"#,
        )),
        Box::new(|pure: &Pure| {
            let plan = pure
                .set
                .plan(&via_adapters::DescribeRequest {
                    harness: Some("pi".to_owned()),
                    model: Some(MODEL.to_owned()),
                    ..via_adapters::DescribeRequest::default()
                })
                .map_err(|refusal| format!("{refusal:?}"))?;
            *described.borrow_mut() = Some(plan.vendor_version.clone());
            Ok(())
        }),
    )
    .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
    // One launch, the turn's own: no `--version` process.
    assert_eq!(outcome.launches, 1);
    assert_eq!(
        described.borrow().clone(),
        Some(Some("1.0.2".to_owned())),
        "describe does not report the version read"
    );
    let oversize = {
        let mut text = br#"{"version":"1.0.2","pad":""#.to_vec();
        text.extend(std::iter::repeat_n(b'x', 64 * 1024));
        text.extend(br#""}"#);
        Box::leak(text.into_boxed_slice()) as &'static [u8]
    };
    let long = {
        let text = format!(r#"{{"version":"{}"}}"#, "1".repeat(65));
        Box::leak(text.into_bytes().into_boxed_slice()) as &'static [u8]
    };
    for (name, package) in [
        ("pi_version_read_missing", None),
        ("pi_version_read_oversize", Some(oversize)),
        ("pi_version_read_array", Some(&br#"["1.0.2"]"#[..])),
        ("pi_version_read_number", Some(&br#"{"version":102}"#[..])),
        ("pi_version_read_long", Some(long)),
    ] {
        let (replay, mut expect) = exited(name);
        expect["turns"][0]["expect"]["instance"] =
            json!({"vendor_version": null, "version_status": "untested"});
        let outcome = drive_built(
            name,
            &replay,
            &expect,
            Knobs::default(),
            layout(package),
            Box::new(|_| Ok(())),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        conformance_expect::check(&expect, &outcome).unwrap_or_else(|e| panic!("{name}:\n{e}"));
        assert_eq!(outcome.launches, 1, "{name}: the turn did not proceed");
    }
}

/// One hostile profile (packet §4.3): what is done to the agent directory,
/// and the words the refusal names.
type Hostile = (
    &'static str,
    fn(&Path) -> std::io::Result<()>,
    &'static [&'static str],
);

/// Writes `settings` as the profile's `settings.json`.
fn settings(agent: &Path, settings: &Value) -> std::io::Result<()> {
    std::fs::write(agent.join("settings.json"), settings.to_string())
}

/// `pi_profile_policy`: the allowed set passes, with and without the files
/// Pi creates itself (E59), and its record `pi-profile.json` holds no
/// `deviceId` value or credential bytes. Every hostile profile is refused
/// before any launch, `handshake_refused`, with a message that names the
/// rule and the entry or key but no value; a refusal is never cached, so
/// the fixed profile runs the next turn.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one packet §8 test: its variants side by side"
)]
fn pi_profile_policy() {
    use std::os::unix::fs::PermissionsExt;
    // Accepted: Pi's own files beside the required settings.
    let replay = single(
        "synthetic (via-jt8.3.1): a canonical turn on a profile with Pi's own files",
        argv(Argv::default()),
        completed_steps(&State::default(), "Say READY.", "READY"),
    );
    let expect = case(
        "pi_profile_policy",
        1,
        vec![turn("Say READY.", completed(1, "READY"))],
    );
    let record = std::cell::RefCell::new(String::new());
    let outcome = drive_built(
        "pi_profile_policy",
        &replay,
        &expect,
        Knobs::default(),
        Box::new(|pure: &Pure| {
            let agent = agent_dir(pure);
            let private = |name: &str, bytes: &[u8]| {
                conformance_drive::write_private(&agent.join(name), bytes)
            };
            private(
                "auth.json",
                b"{\"openai\":{\"credential\":\"cred-bytes-not-for-evidence\"}}",
            )?;
            private("models-store.json", b"{\"models\":[]}")?;
            std::fs::remove_file(agent.join("settings.json")).map_err(|e| e.to_string())?;
            private(
                "settings.json",
                json!({"cacheWarming": "off", "defaultProvider": "openai",
                    "defaultModel": "gpt-6-luna", "lastChangelogVersion": "1.0.2",
                    "deviceId": "device-id-not-for-evidence"})
                .to_string()
                .as_bytes(),
            )?;
            std::fs::create_dir(agent.join("bin")).map_err(|e| e.to_string())?;
            private("bin/rg", b"binary")?;
            std::fs::create_dir(agent.join("sessions")).map_err(|e| e.to_string())
        }),
        Box::new(|pure: &Pure| {
            *record.borrow_mut() =
                std::fs::read_to_string(evidence(pure, 1).join("pi-profile.json"))
                    .map_err(|e| format!("pi-profile.json: {e}"))?;
            Ok(())
        }),
    )
    .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
    let record = record.into_inner();
    assert!(
        record.len() <= 4096,
        "pi-profile.json is {} bytes",
        record.len()
    );
    for secret in ["device-id-not-for-evidence", "cred-bytes-not-for-evidence"] {
        assert!(!record.contains(secret), "pi-profile.json holds {secret}");
    }
    for name in ["auth.json", "settings.json", "cacheWarming", "deviceId"] {
        assert!(
            record.contains(name),
            "pi-profile.json does not name {name}"
        );
    }
    // Refused, each by name, with no launch and no value; the profile is
    // then fixed and the next turn runs: the refusal was never cached.
    let hostile: [Hostile; 17] = [
        (
            "shell_prefix",
            |a| {
                settings(
                    a,
                    &json!({"cacheWarming": "off", "shellCommandPrefix": "value-not-for-message"}),
                )
            },
            &["shellCommandPrefix"],
        ),
        (
            "system_md",
            |a| std::fs::write(a.join("SYSTEM.md"), "value-not-for-message"),
            &["SYSTEM.md"],
        ),
        (
            "append_system_md",
            |a| std::fs::write(a.join("APPEND_SYSTEM.md"), "value-not-for-message"),
            &["APPEND_SYSTEM.md"],
        ),
        (
            "models_json",
            |a| std::fs::write(a.join("models.json"), "{}"),
            &["models.json"],
        ),
        (
            "default_thinking",
            |a| {
                settings(
                    a,
                    &json!({"cacheWarming": "off", "defaultThinkingLevel": "value-not-for-message"}),
                )
            },
            &["defaultThinkingLevel"],
        ),
        (
            "unknown_key",
            |a| {
                settings(
                    a,
                    &json!({"cacheWarming": "off", "zzUnknownKey": "value-not-for-message"}),
                )
            },
            &["zzUnknownKey"],
        ),
        (
            "no_cache_warming",
            |a| settings(a, &json!({"defaultModel": "value-not-for-message"})),
            &["cacheWarming"],
        ),
        (
            "cache_warming_on",
            |a| settings(a, &json!({"cacheWarming": "streaming"})),
            &["cacheWarming"],
        ),
        (
            "malformed_value",
            |a| settings(a, &json!({"cacheWarming": "off", "defaultModel": 7})),
            &["defaultModel"],
        ),
        (
            "settings_not_object",
            |a| std::fs::write(a.join("settings.json"), "[\"value-not-for-message\"]"),
            &["settings.json"],
        ),
        (
            "no_settings",
            |a| std::fs::remove_file(a.join("settings.json")),
            &["settings.json"],
        ),
        (
            "symlink",
            |a| std::os::unix::fs::symlink("/dev/null", a.join("models-store.json")),
            &["models-store.json"],
        ),
        (
            "group_writable",
            |a| {
                std::fs::set_permissions(
                    a.join("settings.json"),
                    std::fs::Permissions::from_mode(0o620),
                )
            },
            &["settings.json"],
        ),
        (
            "auth_group_bits",
            |a| {
                std::fs::write(a.join("auth.json"), "value-not-for-message")?;
                std::fs::set_permissions(
                    a.join("auth.json"),
                    std::fs::Permissions::from_mode(0o640),
                )
            },
            &["auth.json"],
        ),
        (
            "oversize",
            |a| {
                settings(
                    a,
                    &json!({"cacheWarming": "off", "defaultModel": "x".repeat(64 * 1024)}),
                )
            },
            &["settings.json"],
        ),
        // 65 entries checked: `settings.json` and 64 managed binaries.
        (
            "entries_65",
            |a| {
                std::fs::create_dir(a.join("bin"))?;
                for n in 0..64 {
                    std::fs::write(a.join(format!("bin/tool-{n}")), "x")?;
                }
                Ok(())
            },
            &["64"],
        ),
        (
            "bin_symlink",
            |a| {
                std::fs::create_dir(a.join("bin"))?;
                std::os::unix::fs::symlink("/bin/sh", a.join("bin/sh"))
            },
            &["bin/sh"],
        ),
    ];
    for (label, harm, names) in hostile {
        let name = format!("pi_profile_policy_{label}");
        let replay = single(
            "synthetic (via-jt8.3.1): turn 1 refused before launch; turn 2 on the fixed profile",
            argv(Argv::default()),
            completed_steps(&State::default(), "Say READY again.", "READY"),
        );
        let mut refused = unaccepted(None, Some("handshake_refused"), None);
        refused["group_absent"] = json!(false);
        let expect = case(
            &name,
            1,
            vec![
                turn("Say READY.", refused),
                turn("Say READY again.", completed(1, "READY")),
            ],
        );
        let knobs = Knobs {
            change: Some((Some(1), fixed_profile)),
            ..Knobs::default()
        };
        let outcome = drive_built(
            &name,
            &replay,
            &expect,
            knobs,
            Box::new(move |pure: &Pure| {
                harm(&agent_dir(pure)).map_err(|e| format!("{label}: {e}"))
            }),
            Box::new(|_| Ok(())),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        conformance_expect::check(&expect, &outcome).unwrap_or_else(|e| panic!("{name}:\n{e}"));
        let message = outcome.turns[0].message.clone().unwrap_or_default();
        for word in names {
            assert!(
                message.contains(word),
                "{name}: {message:?} does not name {word}"
            );
        }
        assert!(
            !message.contains("value-not-for-message"),
            "{name}: {message:?}"
        );
    }
}

/// `pi_profile_policy`, record half (review r1 minor): the profile
/// changes while the turn executes (Pi's own first-run files appear and a
/// setting is added) and `pi-profile.json` still records the check the
/// launch passed, not a second check after the turn.
#[test]
fn pi_profile_record_is_pre_launch() {
    let mut steps = handshake(&State::default());
    steps.extend(prompt("Say READY."));
    steps.extend(echo("Say READY."));
    steps.push(json!({"await_signal": {"signal": "SIGUSR1"}}));
    let gate = steps.len();
    steps.extend(answer("READY", "stop", &canonical_usage()));
    steps.extend(settle(&assistant(
        json!([{"type": "text", "text": "READY"}]),
        "stop",
        &canonical_usage(),
        None,
    )));
    steps.push(eof());
    let replay = single(
        "synthetic (via-jt8.3.2): the profile changes while the turn executes",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = turn("Say READY.", completed(1, "READY"));
    wanted["gates"] = json!([{"step": gate, "expect": {"accepted": true, "terminal": null}}]);
    let expect = case("pi_profile_record_is_pre_launch", 1, vec![wanted]);
    let knobs = Knobs {
        change: Some((None, |state: &Path| {
            let agent = state.join("vendor").join("pi").join("agent");
            conformance_drive::write_private(&agent.join("models-store.json"), b"{}")
                .map_err(std::io::Error::other)?;
            settings(
                &agent,
                &json!({"cacheWarming": "off", "lastChangelogVersion": "1.0.2"}),
            )
        })),
        ..Knobs::default()
    };
    let record = std::cell::RefCell::new(String::new());
    let outcome = drive_built(
        "pi_profile_record_is_pre_launch",
        &replay,
        &expect,
        knobs,
        Box::new(|_| Ok(())),
        Box::new(|pure: &Pure| {
            let changed = std::fs::read_to_string(agent_dir(pure).join("settings.json"))
                .map_err(|e| format!("settings.json: {e}"))?;
            if !changed.contains("lastChangelogVersion") {
                return Err("the gate's change did not run".to_owned());
            }
            *record.borrow_mut() =
                std::fs::read_to_string(evidence(pure, 1).join("pi-profile.json"))
                    .map_err(|e| format!("pi-profile.json: {e}"))?;
            Ok(())
        }),
    )
    .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
    let record = record.into_inner();
    assert!(record.contains("settings.json"), "{record}");
    for later in ["models-store.json", "lastChangelogVersion"] {
        assert!(
            !record.contains(later),
            "pi-profile.json names {later}: {record}"
        );
    }
}

/// The owner's fix of a hostile profile: the agent directory as it was
/// prepared, the one required setting and nothing else.
fn fixed_profile(state: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let agent = state.join("vendor").join("pi").join("agent");
    std::fs::remove_dir_all(&agent)?;
    std::fs::DirBuilder::new().mode(0o700).create(&agent)?;
    std::fs::write(agent.join("settings.json"), r#"{"cacheWarming":"off"}"#)?;
    std::fs::set_permissions(
        agent.join("settings.json"),
        std::fs::Permissions::from_mode(0o600),
    )
}

/// `pi_uncertain_predecessor` (packet §7.4, R1): turn A leaves its group
/// unproven (its anchor paused at Host's `Stop`, so the group is still
/// present when A's close gives up; the anchor is the workspace `via`
/// binary, built with the failpoints by the workspace's
/// `--features via-cli/test-failpoints` run); B is refused `uncertain_predecessor`
/// without launching, and so is C, because the predicate is A's group,
/// not B's clean outcome. Once A's anchor finishes and Host proves the
/// group absent, D launches. Nothing is resent: the fake's two lifetimes
/// are A's and D's, each with its own prompt.
#[cfg(feature = "test-failpoints")]
#[test]
fn pi_uncertain_predecessor() {
    use std::os::unix::fs::DirBuilderExt;
    let points = tempfile::tempdir().unwrap();
    let dir = points.path().join("points");
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let token = "pi-r1-predecessor-token";
    std::fs::write(
        dir.join("host.anchor.stop_received.json"),
        json!({"token": token, "occurrence": 1, "action": "pause"}).to_string(),
    )
    .unwrap();
    via_store::failpoint::activate(&dir, token).unwrap();
    let replay = lifetimes(
        "synthetic (via-jt8.3.1): A completes; B and C never launch; D continues A's session",
        vec![
            lifetime(
                argv(Argv::default()),
                completed_steps(&State::default(), "Turn A.", "A"),
            ),
            lifetime(
                argv(Argv {
                    resume: true,
                    ..Argv::default()
                }),
                completed_steps(&State::default(), "Turn D.", "D"),
            ),
        ],
    );
    let mut a = completed(1, "A");
    a["cleanup"] = json!("uncertain");
    a["group_absent"] = json!(false);
    // The exit is the vendor's, reported or not when the close gives up.
    unset(&mut a, "exit");
    let refused = || {
        let mut refused = unaccepted(Some("uncertain_predecessor"), None, None);
        refused["group_absent"] = json!(false);
        refused
    };
    let d = completed(2, "D");
    let expect = case(
        "pi_uncertain_predecessor",
        2,
        vec![
            turn("Turn A.", a),
            turn("Turn B.", refused()),
            turn("Turn C.", refused()),
            turn("Turn D.", d),
        ],
    );
    let knobs = Knobs {
        release_before: Some((3, "host.anchor.stop_received")),
        ..Knobs::default()
    };
    check_built("pi_uncertain_predecessor", &replay, &expect, knobs).unwrap();
    assert!(
        dir.join("host.anchor.stop_received.1.ack").exists(),
        "A's anchor never paused"
    );
}

/// `pi_identity_continuation` (packet §2.2): `--session-id` with the
/// derived ID until identity is confirmed, which happens only with the
/// `started` reply (not at the handshake); a rejected turn 1 (no session
/// file) leaves turn 2 creating; once confirmed, `--session`; a missing
/// file then exits before RPC: `Rejected{Protocol}`, and the next turn
/// still continues with `--session`, never a fresh session. A lost
/// `started` reply keeps `--session-id`; a restart with the confirmed ID
/// continues it.
#[test]
fn pi_identity_continuation() {
    let outcome = check_fixture("pi_identity_continuation").unwrap();
    assert_eq!(outcome.launches, 5);
    // A lost `started` reply: Pi exits after the prompt; the next turn
    // still creates (`--session-id`).
    let mut lost = handshake(&State::default());
    lost.push(expect_prompt("Lost."));
    lost.push(json!({"exit": {"code": 0, "stderr": ""}}));
    let replay = lifetimes(
        "synthetic (via-jt8.3.1): turn 1's started reply is lost",
        vec![
            lifetime(argv(Argv::default()), lost),
            lifetime(
                argv(Argv::default()),
                completed_steps(&State::default(), "Again.", "READY"),
            ),
        ],
    );
    let expect = case(
        "pi_identity_continuation_lost",
        2,
        vec![
            turn("Lost.", unaccepted(None, Some("process_exit"), Some(0))),
            turn("Again.", completed(1, "READY")),
        ],
    );
    check_built(
        "pi_identity_continuation_lost",
        &replay,
        &expect,
        Knobs::default(),
    )
    .unwrap();
    // A restart: the session's confirmed ID is continued at once.
    let replay = single(
        "synthetic (via-jt8.3.1): a restarted daemon continues the confirmed ID",
        argv(Argv {
            resume: true,
            ..Argv::default()
        }),
        completed_steps(&State::default(), "After restart.", "READY"),
    );
    let mut expect = case(
        "pi_identity_continuation_restart",
        1,
        vec![turn("After restart.", completed(1, "READY"))],
    );
    expect["sessions"]["main"]["resume"] = json!(SID);
    check_built(
        "pi_identity_continuation_restart",
        &replay,
        &expect,
        Knobs::default(),
    )
    .unwrap();
}

/// `pi_handshake_checks` (packet §2.1 step 3, §3): every check precedes
/// the prompt, so no prompt line is written (the replay fails any line
/// before its `await_eof`). Another `sessionId` is `resume_mismatch`; a
/// model `get_state` does not name exactly, or that the catalog does not
/// list, is `invalid_param:model`; a clamped requested effort is
/// `invalid_param:effort`, and an omitted effort is not checked; a command
/// other than `skill:*` is a refusal (`handshake_refused`), cached, so the
/// next turn is refused at planning; an exit before every reply is
/// `Rejected{Protocol}` with stderr unread.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one packet §8 test: its variants side by side"
)]
fn pi_handshake_checks() {
    let refused_turn = |state: State, effort: Option<&'static str>| {
        let mut steps = handshake(&state);
        steps.extend(terminated());
        single(
            "synthetic (via-jt8.3.1): a handshake check fails; no prompt",
            argv(Argv {
                thinking: effort,
                ..Argv::default()
            }),
            steps,
        )
    };
    let with_effort = |mut expect: Value, effort: &str| {
        expect["turns"][0]["params"]["effort"] = json!(effort);
        expect
    };
    // Another session.
    let replay = refused_turn(
        State {
            session_id: OTHER_SID,
            ..State::default()
        },
        None,
    );
    let mut wanted = unaccepted(None, Some("resume_mismatch"), Some(143));
    wanted["observations_include"] = json!([
        {"kind": "resume.mismatch", "requested": SID, "returned": OTHER_SID}
    ]);
    let mut expect = case("pi_handshake_session", 1, vec![turn("Say READY.", wanted)]);
    expect["sessions"]["main"]["health"] =
        json!({"state": "failed", "first_cause": "resume_mismatch"});
    check_built("pi_handshake_session", &replay, &expect, Knobs::default()).unwrap();
    // A model the reply does not name exactly (Pi fuzzy-matches, E30).
    let fuzzy = State {
        model: "gpt-6-luna-mini",
        ..State::default()
    };
    // A model the catalog does not list.
    let unlisted = State {
        models: json!([{"provider": "openai", "id": "gpt-6-sol"}]),
        ..State::default()
    };
    for (name, state) in [
        ("pi_handshake_model_fuzzy", fuzzy),
        ("pi_handshake_model_unlisted", unlisted),
    ] {
        let replay = refused_turn(state, None);
        let expect = case(
            name,
            1,
            vec![turn(
                "Say READY.",
                unaccepted(Some("invalid_param:model"), None, Some(143)),
            )],
        );
        check_built(name, &replay, &expect, Knobs::default()).unwrap();
    }
    // A clamped effort: `xhigh` requested, `high` applied (E41).
    let replay = refused_turn(
        State {
            thinking: "high",
            ..State::default()
        },
        Some("xhigh"),
    );
    let expect = with_effort(
        case(
            "pi_handshake_effort",
            1,
            vec![turn(
                "Say READY.",
                unaccepted(Some("invalid_param:effort"), None, Some(143)),
            )],
        ),
        "xhigh",
    );
    // The clamp is cached for planning (packet §4.5, review r1 minor):
    // the same model and effort are refused before any receipt.
    let refusals = std::cell::RefCell::new(Value::Null);
    let outcome = drive_built(
        "pi_handshake_effort",
        &replay,
        &expect,
        Knobs::default(),
        Box::new(|_| Ok(())),
        Box::new(|pure: &Pure| {
            let plan = pure
                .set
                .plan(&via_adapters::DescribeRequest {
                    harness: Some("pi".to_owned()),
                    model: Some(MODEL.to_owned()),
                    effort: Some("xhigh".to_owned()),
                    ..via_adapters::DescribeRequest::default()
                })
                .map_err(|refusal| format!("{refusal:?}"))?;
            *refusals.borrow_mut() =
                serde_json::to_value(&plan).map_err(|e| e.to_string())?["refusals"].clone();
            let session = via_adapters::SessionRef {
                harness: "pi".to_owned(),
                route: plan.route.to_owned(),
                adapter_version: plan.adapter_version.clone(),
            };
            let turn = via_adapters::TurnParams {
                model: Some(MODEL.to_owned()),
                effort: Some("xhigh".to_owned()),
                ..via_adapters::TurnParams::default()
            };
            match pure.set.check_turn(&session, &turn) {
                Err(refusal)
                    if refusal.kind
                        == (via_adapters::RefusalKind::InvalidParam { field: "effort" }) =>
                {
                    Ok(())
                }
                other => Err(format!("check_turn did not refuse the clamp: {other:?}")),
            }
        }),
    )
    .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
    let refusals = refusals.into_inner();
    assert!(
        refusals
            .as_array()
            .is_some_and(|all| all.iter().any(|r| r.to_string().contains("effort"))),
        "the clamp was not cached: {refusals}"
    );
    // The requested effort applied: the turn runs.
    let replay = single(
        "synthetic (via-jt8.3.1): effort low, applied",
        argv(Argv {
            thinking: Some("low"),
            ..Argv::default()
        }),
        completed_steps(
            &State {
                thinking: "low",
                ..State::default()
            },
            "Say READY.",
            "READY",
        ),
    );
    let expect = with_effort(
        case(
            "pi_handshake_effort_applied",
            1,
            vec![turn("Say READY.", completed(1, "READY"))],
        ),
        "low",
    );
    check_built(
        "pi_handshake_effort_applied",
        &replay,
        &expect,
        Knobs::default(),
    )
    .unwrap();
    // A non-skill command: `-ne`/`-np` did not hold. Refused and cached:
    // the session's next turn is refused at planning, nothing launched.
    let replay = refused_turn(
        State {
            commands: json!([
                {"name": "skill:review", "source": "skill"},
                {"name": "mcp", "source": "extension"},
            ]),
            ..State::default()
        },
        None,
    );
    let first = unaccepted(None, Some("handshake_refused"), Some(143));
    let expect = case("pi_handshake_command", 1, vec![turn("Say READY.", first)]);
    let status = std::cell::RefCell::new(Value::Null);
    let outcome = drive_built(
        "pi_handshake_command",
        &replay,
        &expect,
        Knobs::default(),
        Box::new(|_| Ok(())),
        Box::new(|pure: &Pure| {
            let plan = pure
                .set
                .plan(&via_adapters::DescribeRequest {
                    harness: Some("pi".to_owned()),
                    model: Some(MODEL.to_owned()),
                    ..via_adapters::DescribeRequest::default()
                })
                .map_err(|refusal| format!("{refusal:?}"))?;
            *status.borrow_mut() =
                serde_json::to_value(&plan).map_err(|e| e.to_string())?["version_status"].clone();
            Ok(())
        }),
    )
    .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
    assert_eq!(status.into_inner(), "refused", "the refusal was not cached");
    // An exit before every reply: protocol, the vendor's words unread.
    let replay = single(
        "synthetic (via-jt8.3.1): Pi exits with one reply written (E12, E30)",
        argv(Argv::default()),
        vec![
            expect_id(json!({"type": "get_state"}), "gs"),
            expect_line(json!({"type": "get_available_models"})),
            expect_line(json!({"type": "get_commands"})),
            emit_line(&reply(
                "gs",
                "get_state",
                true,
                &json!({"model": {"provider": "openai", "id": "gpt-6-luna"},
                    "thinkingLevel": "off", "sessionFile": session_file(SID), "sessionId": SID}),
            )),
            json!({"exit": {"code": 1, "stderr": "Error: stderr-words-not-for-message\n"}}),
        ],
    );
    let expect = case(
        "pi_handshake_exit",
        1,
        vec![turn(
            "Say READY.",
            unaccepted(Some("protocol"), None, Some(1)),
        )],
    );
    let outcome = check_built("pi_handshake_exit", &replay, &expect, Knobs::default()).unwrap();
    let message = outcome.turns[0].message.clone().unwrap_or_default();
    assert!(
        message.contains("pi exited before its handshake replies"),
        "{message:?}"
    );
    assert!(
        !message.contains("stderr-words-not-for-message"),
        "{message:?}"
    );
}

/// A launch whose run, after the handshake and the prompt line, is
/// `run`; `accepted` when `run` holds the `started` reply.
fn protocol_case(name: &str, run: Vec<Value>, accepted: bool) -> (Value, Value) {
    let mut steps = handshake(&State::default());
    steps.extend(run);
    let replay = single(
        "synthetic (via-jt8.3.1): a malformed or out-of-phase record",
        argv(Argv::default()),
        steps,
    );
    let wanted = if accepted {
        json!({
            "plan_refusal": null, "rejected": null, "accepted": true,
            "terminal": null, "error": "protocol", "cleanup": "quiescent",
            "observation_counts": {"turn.accepted": 1},
            "observations_order": ["session.vendor_identity_confirmed", "turn.accepted"],
        })
    } else {
        let mut wanted = unaccepted(None, Some("protocol"), None);
        unset(&mut wanted, "exit");
        wanted["group_absent"] = json!(true);
        wanted
    };
    (replay, case(name, 1, vec![turn("Say READY.", wanted)]))
}

/// The group stop a failure ends a still-running fake with: it takes
/// Host's `SIGTERM` and exits 143 on its own.
fn terminated() -> Vec<Value> {
    vec![
        json!({"await_signal": {"signal": "SIGTERM"}}),
        json!({"exit": {"code": 143, "stderr": ""}}),
    ]
}

/// `pi_protocol_typed` (packet §5.1): a reply with another `command` for a
/// known `id`, a duplicate reply, a reply missing required fields (a
/// `get_state` without `sessionFile` among them), a `parse` reply, `handled`/`queued` dispositions, a lifecycle or message
/// record before `started`, an assistant `message_end` without `content`
/// or `usage` or with a non-numeric usage member, `agent_settled` with no
/// assistant terminal and a second `agent_settled`: every one is
/// `protocol`, and nothing is accepted or resent before `started`.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one packet §8 test: its variants side by side"
)]
fn pi_protocol_typed() {
    let state_reply = |command: &str, data: &Value| reply("gs", command, true, data);
    let good_state = json!({"model": {"provider": "openai", "id": "gpt-6-luna"},
        "thinkingLevel": "off", "sessionFile": session_file(SID), "sessionId": SID});
    // Handshake-phase variants replace the canonical handshake.
    let pre = |name: &str, replies: Vec<Value>| {
        let mut steps = vec![
            expect_id(json!({"type": "get_state"}), "gs"),
            expect_id(json!({"type": "get_available_models"}), "gm"),
            expect_id(json!({"type": "get_commands"}), "gc"),
        ];
        steps.extend(replies);
        steps.extend(terminated());
        let replay = single(
            "synthetic (via-jt8.3.1): a malformed handshake reply",
            argv(Argv::default()),
            steps,
        );
        let mut wanted = unaccepted(None, Some("protocol"), Some(143));
        wanted["group_absent"] = json!(true);
        let expect = case(name, 1, vec![turn("Say READY.", wanted)]);
        check_built(name, &replay, &expect, Knobs::default()).unwrap();
    };
    pre(
        "pi_protocol_wrong_command",
        vec![emit_line(&state_reply("get_commands", &good_state))],
    );
    pre(
        "pi_protocol_duplicate_reply",
        vec![
            emit_line(&state_reply("get_state", &good_state)),
            emit_line(&state_reply("get_state", &good_state)),
        ],
    );
    pre(
        "pi_protocol_missing_field",
        vec![emit_line(&state_reply(
            "get_state",
            &json!({"model": {"provider": "openai", "id": "gpt-6-luna"}, "thinkingLevel": "off"}),
        ))],
    );
    // §5.1: `sessionFile` is required (review r1 minor).
    let mut no_session_file = good_state.clone();
    no_session_file
        .as_object_mut()
        .unwrap()
        .remove("sessionFile");
    pre(
        "pi_protocol_missing_session_file",
        vec![emit_line(&state_reply("get_state", &no_session_file))],
    );
    pre(
        "pi_protocol_parse_reply",
        vec![emit(
            &json!({"type": "response", "command": "parse", "success": false,
            "error": "Failed to parse command"}),
        )],
    );
    pre(
        "pi_protocol_unknown_id",
        vec![emit(
            &json!({"id": "not-via", "type": "response", "command": "get_state",
            "success": true, "data": good_state}),
        )],
    );
    // Prompt-phase variants: after the canonical handshake.
    let mut cases: Vec<(&str, Vec<Value>, bool)> = Vec::new();
    for disposition in ["handled", "queued"] {
        cases.push((
            if disposition == "handled" {
                "pi_protocol_handled"
            } else {
                "pi_protocol_queued"
            },
            vec![
                expect_prompt("Say READY."),
                emit_line(&reply(
                    "p",
                    "prompt",
                    true,
                    &json!({"disposition": disposition}),
                )),
            ],
            false,
        ));
    }
    cases.push((
        "pi_protocol_lifecycle_before_started",
        vec![
            expect_prompt("Say READY."),
            emit(&json!({"type": "agent_start"})),
            started(),
        ],
        false,
    ));
    let mut no_content = assistant(json!([]), "stop", &canonical_usage(), None);
    unset(&mut no_content, "content");
    let mut no_usage = assistant(json!([]), "stop", &canonical_usage(), None);
    unset(&mut no_usage, "usage");
    let mut text_usage = assistant(json!([]), "stop", &canonical_usage(), None);
    text_usage["usage"]["output"] = json!("ten");
    for (name, message) in [
        ("pi_protocol_no_content", no_content),
        ("pi_protocol_no_usage", no_usage),
        ("pi_protocol_text_usage", text_usage),
    ] {
        let mut run = prompt("Say READY.");
        run.extend(echo("Say READY."));
        run.push(message_end(&message));
        cases.push((name, run, true));
    }
    let mut settled_alone = prompt("Say READY.");
    settled_alone.extend(echo("Say READY."));
    settled_alone.push(emit(&json!({"type": "agent_settled"})));
    cases.push(("pi_protocol_settled_without_terminal", settled_alone, true));
    for (name, mut run, accepted) in cases {
        run.extend(terminated());
        let (replay, mut expect) = protocol_case(name, run, accepted);
        expect["turns"][0]["expect"]["exit"] = json!({"code": 143, "signal": null});
        check_built(name, &replay, &expect, Knobs::default()).unwrap();
    }
    // A second `agent_settled`, after stdin EOF: the first terminal stays.
    let mut steps = completed_steps(&State::default(), "Say READY.", "READY");
    steps.push(emit(&json!({"type": "agent_settled"})));
    let replay = single(
        "synthetic (via-jt8.3.1): a second agent_settled after EOF",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = completed(1, "READY");
    wanted["error"] = json!("protocol");
    let expect = case(
        "pi_protocol_second_settled",
        1,
        vec![turn("Say READY.", wanted)],
    );
    check_built(
        "pi_protocol_second_settled",
        &replay,
        &expect,
        Knobs::default(),
    )
    .unwrap();
}

/// `pi_acceptance` (packet §2.1 step 4, §5.5): only `started` accepts, and
/// identity is confirmed with it, carrying the session file as the
/// transcript hint; `success:false` is `Rejected{VendorError}` with
/// VIA-owned text and no code; a lost reply (Pi exits after the prompt)
/// is never accepted; a compaction sample decoded before `started` is
/// delivered after `turn.accepted`.
#[test]
fn pi_acceptance() {
    let outcome = check_fixture("pi_acceptance").unwrap();
    let identity = outcome.turns[0]
        .observations
        .iter()
        .find(|observation| observation["kind"] == "session.vendor_identity_confirmed")
        .unwrap();
    let transcript = identity["transcript"].as_str().unwrap_or_default();
    assert!(
        transcript.ends_with(&format!("_{SID}.jsonl")) && transcript.contains("s_000000000001"),
        "transcript hint {transcript:?}"
    );
    // Refused before a run (E29).
    let mut steps = handshake(&State::default());
    steps.push(expect_prompt("Say READY."));
    steps.push(emit_line(&reply("p", "prompt", false, &json!(NO_KEY))));
    steps.push(eof());
    let replay = single(
        "synthetic (via-jt8.3.1): the prompt refused before a run (E29)",
        argv(Argv::default()),
        steps,
    );
    let expect = case(
        "pi_acceptance_refused",
        1,
        vec![turn(
            "Say READY.",
            unaccepted(Some("vendor_error"), None, Some(0)),
        )],
    );
    let outcome = check_built("pi_acceptance_refused", &replay, &expect, Knobs::default()).unwrap();
    let message = outcome.turns[0].message.clone().unwrap_or_default();
    assert!(
        message.contains("pi refused the prompt before starting a run"),
        "{message:?}"
    );
    assert!(!message.contains("API key"), "{message:?}");
    // The reply lost: Pi exits after reading the prompt.
    let mut steps = handshake(&State::default());
    steps.push(expect_prompt("Say READY."));
    steps.push(json!({"exit": {"code": 0, "stderr": ""}}));
    let replay = single(
        "synthetic (via-jt8.3.1): the started reply lost",
        argv(Argv::default()),
        steps,
    );
    let expect = case(
        "pi_acceptance_lost",
        1,
        vec![turn(
            "Say READY.",
            unaccepted(None, Some("process_exit"), Some(0)),
        )],
    );
    check_built("pi_acceptance_lost", &replay, &expect, Knobs::default()).unwrap();
    // Pre-prompt compaction (E63): its sample is held until acceptance.
    let compacted = usage(40, 8, 0, 0, 0.25);
    let mut steps = handshake(&State::default());
    steps.push(expect_prompt("Say READY."));
    steps.push(emit(
        &json!({"type": "compaction_start", "reason": "threshold"}),
    ));
    steps.push(emit(&json!({"type": "compaction_end", "aborted": false,
        "result": {"summary": "summary", "usage": compacted}})));
    steps.push(started());
    steps.extend(echo("Say READY."));
    steps.extend(answer("READY", "stop", &canonical_usage()));
    steps.extend(settle(&assistant(
        json!([{"type": "text", "text": "READY"}]),
        "stop",
        &canonical_usage(),
        None,
    )));
    steps.push(eof());
    let replay = single(
        "synthetic (via-jt8.3.1): pre-prompt compaction before started (E63)",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = completed(1, "READY");
    wanted["usage"] = json!({"from": "samples", "input_tokens": 140, "cached_input_tokens": 20,
        "output_tokens": 18, "total_tokens": 158, "scope": "turn"});
    wanted["terminal"]["cost"] = estimated(0.75);
    wanted["observations_order"] = json!([
        "session.vendor_identity_confirmed",
        "turn.accepted",
        {"kind": "progress", "usage": {"input": 40, "output": 8, "total": 48}},
    ]);
    let expect = case(
        "pi_acceptance_compaction",
        1,
        vec![turn("Say READY.", wanted)],
    );
    check_built(
        "pi_acceptance_compaction",
        &replay,
        &expect,
        Knobs::default(),
    )
    .unwrap();
}

/// `pi_settled_not_agent_end` (E28): an auto-retried error gives
/// `agent_end` with `willRetry:true` and a second run; the one terminal is
/// the last assistant message at `agent_settled`, and stdin stays open
/// until then (the gate holds the fake after the first `agent_end`, where
/// no terminal exists, and the replay fails an early EOF).
#[test]
fn pi_settled_not_agent_end() {
    let error = assistant(
        json!([]),
        "error",
        &zero_usage(),
        Some("429: {\"error\":\"rate\"}"),
    );
    let done = assistant(
        json!([{"type": "text", "text": "READY"}]),
        "stop",
        &canonical_usage(),
        None,
    );
    let mut steps = handshake(&State::default());
    steps.extend(prompt("Say READY."));
    steps.extend(echo("Say READY."));
    steps.push(message_end(&error));
    steps.push(emit(
        &json!({"type": "turn_end", "message": error, "toolResults": []}),
    ));
    steps.push(emit(
        &json!({"type": "agent_end", "messages": [error], "willRetry": true}),
    ));
    steps.push(json!({"await_signal": {"signal": "SIGUSR1"}}));
    steps.push(emit(
        &json!({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 3,
        "delayMs": 2000, "errorMessage": "429"}),
    ));
    steps.push(emit(&json!({"type": "agent_start"})));
    steps.push(emit(&json!({"type": "turn_start"})));
    steps.extend(answer("READY", "stop", &canonical_usage()));
    steps.push(emit(
        &json!({"type": "turn_end", "message": done, "toolResults": []}),
    ));
    steps.push(emit(
        &json!({"type": "agent_end", "messages": [done], "willRetry": false}),
    ));
    steps.push(emit(
        &json!({"type": "auto_retry_end", "success": true, "attempt": 1}),
    ));
    steps.push(emit(&json!({"type": "agent_settled"})));
    steps.push(eof());
    let gate = steps
        .iter()
        .position(|step| step.get("await_signal").is_some())
        .unwrap()
        + 1;
    let replay = single(
        "synthetic (via-jt8.3.1): auto-retry after a 429 (E28)",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = completed(1, "READY");
    // The retried call reported zeros: unknown, never 0 (packet §5.5).
    wanted["usage"] = null_usage();
    wanted["terminal"]["cost"] = json!({"usd": null, "provenance": "unavailable"});
    let mut turn = turn("Say READY.", wanted);
    turn["gates"] = json!([{"step": gate, "expect": {"accepted": true, "terminal": null,
        "error": null, "cleanup": "pending", "final_text": null}}]);
    let expect = case("pi_settled_not_agent_end", 1, vec![turn]);
    check_built(
        "pi_settled_not_agent_end",
        &replay,
        &expect,
        Knobs::default(),
    )
    .unwrap();
}

/// One terminal-mapping row: the last assistant message and the terminal
/// it gives.
fn terminal_case(
    name: &str,
    last: &Value,
    terminal: Value,
    final_text: Value,
) -> Result<(), String> {
    let mut steps = handshake(&State::default());
    steps.extend(prompt("Say READY."));
    steps.extend(echo("Say READY."));
    steps.push(message_end(last));
    steps.extend(settle(last));
    steps.push(eof());
    let replay = single(
        "synthetic (via-jt8.3.1): one packet §5.3 row",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = completed(1, "");
    wanted["terminal"] = terminal;
    wanted["final_text"] = final_text;
    unset(&mut wanted, "usage");
    let expect = case(name, 1, vec![turn("Say READY.", wanted)]);
    check_built(name, &replay, &expect, Knobs::default()).map(|_| ())
}

/// `pi_terminal_mapping` (packet §5.3): every row; only the terminal
/// message's text is final text, and the system message (every loaded
/// context file) never reaches final text, progress or the envelope.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one packet §8 test: its variants side by side"
)]
fn pi_terminal_mapping() {
    let text = |text: &str| json!([{"type": "text", "text": text}]);
    let failed = |stop: &str, vendor: &str, hint: &str| {
        json!({"status": "failed", "stop_reason": stop, "vendor_stop_reason": vendor,
            "class_hint": hint})
    };
    terminal_case(
        "pi_terminal_length",
        &assistant(text("PARTIAL"), "length", &canonical_usage(), None),
        json!({"status": "completed", "stop_reason": "budget", "vendor_stop_reason": "length",
            "class_hint": null}),
        json!(["PARTIAL"]),
    )
    .unwrap();
    terminal_case(
        "pi_terminal_aborted_unasked",
        &assistant(
            text("s0 s1"),
            "aborted",
            &zero_usage(),
            Some("Request was aborted"),
        ),
        failed("error", "aborted", "vendor_error"),
        Value::Null,
    )
    .unwrap();
    terminal_case(
        "pi_terminal_abort_marker_unasked",
        &assistant(
            json!([]),
            "error",
            &zero_usage(),
            Some("This operation was aborted"),
        ),
        failed("error", "error", "vendor_error"),
        Value::Null,
    )
    .unwrap();
    terminal_case(
        "pi_terminal_auth",
        &assistant(
            json!([]),
            "error",
            &zero_usage(),
            Some("401: {\"message\":\"fake error 401\",\"type\":\"fake_error\",\"code\":\"401\"}"),
        ),
        failed("error", "error", "auth"),
        Value::Null,
    )
    .unwrap();
    terminal_case(
        "pi_terminal_rate_limit",
        &assistant(
            json!([]),
            "error",
            &zero_usage(),
            Some(
                "OpenAI API error (429): {\"message\":\"slow down\",\"type\":\"rate_limit_error\"}",
            ),
        ),
        failed("error", "error", "rate_limit"),
        Value::Null,
    )
    .unwrap();
    terminal_case(
        "pi_terminal_other_error",
        &assistant(text("partial"), "error", &zero_usage(), Some("terminated")),
        failed("error", "error", "vendor_error"),
        Value::Null,
    )
    .unwrap();
    terminal_case(
        "pi_terminal_deferred",
        &assistant(json!([]), "deferred", &zero_usage(), None),
        failed("other", "deferred", "vendor_error"),
        Value::Null,
    )
    .unwrap();
    // `toolUse` as the last message: no terminating tool exists.
    let call = json!([{"type": "toolCall", "id": "call_1", "name": "bash", "arguments": {}}]);
    let last = assistant(call, "toolUse", &canonical_usage(), None);
    let mut run = prompt("Say READY.");
    run.extend(echo("Say READY."));
    run.push(message_end(&last));
    run.extend(settle(&last));
    run.extend(terminated());
    let (replay, mut expect) = protocol_case("pi_terminal_tool_use", run, true);
    expect["turns"][0]["expect"]["exit"] = json!({"code": 143, "signal": null});
    check_built("pi_terminal_tool_use", &replay, &expect, Knobs::default()).unwrap();
    // Earlier text and the system message are never final text.
    let system = json!({"role": "system", "content": "", "timestamp": 1,
        "sections": {"preamble": "SYSTEM-WORDS", "tools": "<tools>read</tools>",
            "project_context": "<project_context>\n<project_instructions path=\"/AGENTS.md\">\nCONTEXT-WORDS\n</project_instructions>\n</project_context>"},
        "toolsAdded": [{"name": "read"}, {"name": "bash"}, {"name": "edit"}, {"name": "write"}]});
    let mut steps = handshake(&State::default());
    steps.extend(prompt("Say READY."));
    steps.push(emit(&json!({"type": "agent_start"})));
    steps.push(emit(&json!({"type": "turn_start"})));
    steps.push(emit(&json!({"type": "message_start", "message": system})));
    steps.push(emit(&json!({"type": "message_end", "message": system})));
    steps.extend(answer("EARLIER-WORDS", "toolUse", &canonical_usage()));
    steps.extend(tool_end(false));
    steps.extend(answer("READY", "stop", &canonical_usage()));
    steps.extend(settle(&assistant(
        text("READY"),
        "stop",
        &canonical_usage(),
        None,
    )));
    steps.push(eof());
    let replay = single(
        "synthetic (via-jt8.3.1): a system patch and earlier text",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = completed(1, "READY");
    wanted["usage"] = canonical_turn_usage(2);
    // The runner states a whole cost as an integer.
    wanted["terminal"]["cost"] = json!({"usd": 1, "scope": "turn", "provenance": "estimated"});
    let expect = case(
        "pi_terminal_final_text",
        1,
        vec![turn("Say READY.", wanted)],
    );
    let outcome =
        check_built("pi_terminal_final_text", &replay, &expect, Knobs::default()).unwrap();
    let seen = observed_text(&outcome);
    for words in ["SYSTEM-WORDS", "CONTEXT-WORDS", "EARLIER-WORDS"] {
        assert!(!seen.contains(words), "an observation carries {words}");
    }
}

/// `pi_progress_deltas` (packet §5.2): every streaming record is one
/// `progress {model:true}`, so a text block streaming longer than the idle
/// deadline keeps the turn alive; `message_update`'s cumulative usage is
/// never a sample (one sample per `message_end`); the delivered position
/// meets the decode watermark when the turn settles (the decode fence).
#[test]
fn pi_progress_deltas() {
    let mut steps = handshake(&State::default());
    steps.extend(prompt("Count slowly."));
    steps.extend(echo("Count slowly."));
    let pending = assistant(json!([]), "pending", &zero_usage(), None);
    steps.push(emit(&json!({"type": "message_start", "message": pending})));
    steps.push(update(json!({"type": "text_start", "contentIndex": 0})));
    let mut text = String::new();
    for n in 0..8 {
        let delta = format!("{n} ");
        text.push_str(&delta);
        steps.push(json!({"delay": {"ms": 250}}));
        steps.push(update(
            json!({"type": "text_delta", "contentIndex": 0, "delta": delta}),
        ));
    }
    steps.push(update(
        json!({"type": "text_end", "contentIndex": 0, "content": text}),
    ));
    let done = assistant(
        json!([{"type": "text", "text": text}]),
        "stop",
        &canonical_usage(),
        None,
    );
    steps.push(message_end(&done));
    steps.extend(settle(&done));
    steps.push(eof());
    let replay = single(
        "synthetic (via-jt8.3.1): one text block streamed over 2 s",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = completed(1, &text);
    // `text_start` and the eight deltas, then the `message_end` sample;
    // `text_end` is activity.
    wanted["observation_counts"]["progress"] = json!(10);
    let mut streamed = turn("Count slowly.", wanted);
    streamed["deadlines"] = json!({"wall_ms": 20_000, "idle_ms": 1_000});
    let expect = case("pi_progress_deltas", 1, vec![streamed]);
    let fences = std::cell::RefCell::new(None);
    let outcome = drive_built(
        "pi_progress_deltas",
        &replay,
        &expect,
        Knobs::default(),
        Box::new(|_| Ok(())),
        Box::new(|pure: &Pure| {
            *fences.borrow_mut() = pure.fences.borrow().get(&0).copied();
            Ok(())
        }),
    )
    .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
    let samples = outcome.turns[0]
        .observations
        .iter()
        .filter(|observation| !observation["usage"].is_null() && observation["kind"] == "progress")
        .count();
    assert_eq!(samples, 1, "a usage snapshot became a sample");
    let (decoded, delivered) = fences.into_inner().unwrap();
    assert_eq!(
        decoded, delivered,
        "the turn settled before its decode fence"
    );
    // Review r1 #5: the runner keeps the turn's idle deadline as Core
    // does. A stream silent past `idle_ms` is stopped (Pi gets the
    // abort; the turn has no stop of its own), so a broken progress mark
    // or idle reconciliation fails the case above.
    let mut steps = handshake(&State::default());
    steps.extend(prompt("Count slowly."));
    steps.extend(echo("Count slowly."));
    steps.push(emit(&json!({"type": "message_start", "message": pending})));
    steps.push(update(json!({"type": "text_start", "contentIndex": 0})));
    steps.push(json!({"delay": {"ms": 1_500}}));
    steps.push(expect_id(json!({"type": "abort"}), "ab"));
    let aborted = assistant(json!([]), "aborted", &zero_usage(), None);
    steps.push(message_end(&aborted));
    steps.extend(settle(&aborted));
    steps.push(emit_line(&reply("ab", "abort", true, &Value::Null)));
    steps.push(eof());
    let replay = single(
        "synthetic (review r1 #5): a stream silent past its idle deadline",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = completed(1, "");
    wanted["terminal"] = json!({"status": "interrupted", "stop_reason": "interrupted",
        "vendor_stop_reason": "aborted", "class_hint": null});
    wanted["final_text"] = Value::Null;
    unset(&mut wanted, "usage");
    wanted["observations_exclude"] = json!(["final_text"]);
    let mut idle = turn("Count slowly.", wanted);
    idle["deadlines"] = json!({"wall_ms": 20_000, "idle_ms": 500});
    let expect = case("pi_progress_idle", 1, vec![idle]);
    check_built("pi_progress_idle", &replay, &expect, Knobs::default()).unwrap();
}

/// `pi_usage_accounting` (packet §5.5): cache and reasoning counters map
/// as Pi reports them (`input` = `input + cacheRead + cacheWrite`); the
/// cost is the estimated sum; an all-zero sample is all-null and makes the
/// turn's components and cost unknown; compaction is one more sample with
/// usage, and an all-null one without.
#[test]
fn pi_usage_accounting() {
    let run =
        |name: &str, calls: Vec<Value>, compaction: Option<Value>, usage: Value, cost: Value| {
            let mut steps = handshake(&State::default());
            steps.extend(prompt("Count."));
            steps.extend(echo("Count."));
            if let Some(compaction) = compaction {
                steps.push(emit(
                    &json!({"type": "compaction_start", "reason": "threshold"}),
                ));
                steps.push(emit(&compaction));
            }
            let last = calls.len() - 1;
            for (at, call) in calls.iter().enumerate() {
                let stop = if at == last { "stop" } else { "toolUse" };
                let message =
                    assistant(json!([{"type": "text", "text": "READY"}]), stop, call, None);
                steps.push(message_end(&message));
                if at == last {
                    steps.extend(settle(&message));
                } else {
                    steps.extend(tool_end(false));
                }
            }
            steps.push(eof());
            let replay = single(
                "synthetic (via-jt8.3.1): per-call usage (E24-E26, E63)",
                argv(Argv::default()),
                steps,
            );
            let mut wanted = completed(1, "READY");
            wanted["usage"] = usage;
            wanted["terminal"]["cost"] = cost;
            let expect = case(name, 1, vec![turn("Count.", wanted)]);
            check_built(name, &replay, &expect, Knobs::default()).unwrap()
        };
    let mut counted = usage(80, 10, 20, 5, 0.25);
    counted["reasoning"] = json!(3);
    counted["totalTokens"] = json!(118);
    let outcome = run(
        "pi_usage_counters",
        vec![counted, canonical_usage()],
        None,
        json!({"from": "samples", "input_tokens": 205, "cached_input_tokens": 40,
            "output_tokens": 20, "reasoning_output_tokens": 3, "total_tokens": 228,
            "scope": "turn"}),
        estimated(0.75),
    );
    assert!(outcome.turns[0].observations.iter().any(|observation| {
        observation["usage"]["input"] == 105 && observation["usage"]["reasoning_output"] == 3
    }));
    run(
        "pi_usage_missing",
        vec![canonical_usage(), zero_usage()],
        None,
        null_usage(),
        json!({"usd": null, "provenance": "unavailable"}),
    );
    run(
        "pi_usage_compaction",
        vec![canonical_usage()],
        Some(json!({"type": "compaction_end", "aborted": false,
            "result": {"summary": "s", "usage": usage(40, 8, 0, 0, 0.25)}})),
        json!({"from": "samples", "input_tokens": 140, "cached_input_tokens": 20,
            "output_tokens": 18, "total_tokens": 158, "scope": "turn"}),
        estimated(0.75),
    );
    run(
        "pi_usage_compaction_failed",
        vec![canonical_usage()],
        Some(json!({"type": "compaction_end", "aborted": true, "errorMessage": "aborted"})),
        null_usage(),
        json!({"usd": null, "provenance": "unavailable"}),
    );
}

/// An abort case: the run after the prompt up to VIA's abort line, then
/// what Pi answers it with.
fn abort_case(
    name: &str,
    (before, after): (Vec<Value>, Vec<Value>),
    stop_after: &str,
    wanted: Value,
) -> Result<Outcome, String> {
    let mut steps = handshake(&State::default());
    steps.extend(prompt("Run sleep 30 with bash."));
    steps.extend(echo("Run sleep 30 with bash."));
    steps.extend(before);
    steps.push(expect_id(json!({"type": "abort"}), "ab"));
    steps.extend(after);
    let replay = single(
        "synthetic (via-jt8.3.1): VIA's abort (E15-E18, E54)",
        argv(Argv::default()),
        steps,
    );
    let mut turn = turn("Run sleep 30 with bash.", wanted);
    turn["stop"] = json!({"kind": "interrupt", "after": stop_after});
    let expect = case(name, 1, vec![turn]);
    check_built(name, &replay, &expect, Knobs::default())
}

/// The abort's paired reply.
fn abort_reply() -> Value {
    emit_line(&reply("ab", "abort", true, &Value::Null))
}

/// An interrupted turn's expectation: acknowledged or not.
fn interrupted(acknowledged: bool, vendor: &str) -> Value {
    let mut wanted = completed(1, "");
    wanted["terminal"] = json!({"status": "interrupted", "stop_reason": "interrupted",
        "vendor_stop_reason": vendor, "class_hint": null});
    wanted["final_text"] = Value::Null;
    wanted["stop_facts"] = json!({"acknowledged": acknowledged, "forced": false});
    unset(&mut wanted, "usage");
    wanted["observations_exclude"] = json!(["final_text"]);
    wanted
}

/// `pi_abort` (packet §7.1): the tool-phase marker (`error` with "This
/// operation was aborted") and the streaming marker (`aborted`), each with
/// the paired reply, acknowledge; the reply follows `agent_settled` and is
/// awaited before stdin EOF. The reply alone never acknowledges: natural
/// completion keeps `Completed`, a 401 racing the abort is `failed(auth)`.
/// No reply by `force_at`: no acknowledgement, the marker terminal not
/// retained, and the group stopped.
#[test]
fn pi_abort() {
    check_fixture("pi_abort").unwrap();
    // Streaming: the partial text is never final text (E16).
    let pending = assistant(json!([]), "pending", &zero_usage(), None);
    let partial = assistant(
        json!([{"type": "text", "text": "s0 s1 "}]),
        "aborted",
        &zero_usage(),
        Some("Request was aborted"),
    );
    let mut after = vec![message_end(&partial)];
    after.extend(settle(&partial));
    after.push(abort_reply());
    after.push(eof());
    abort_case(
        "pi_abort_streaming",
        (
            vec![
                emit(&json!({"type": "message_start", "message": pending})),
                update(json!({"type": "text_start", "contentIndex": 0})),
                update(json!({"type": "text_delta", "contentIndex": 0, "delta": "s0 s1 "})),
            ],
            after,
        ),
        "accepted",
        interrupted(true, "aborted"),
    )
    .unwrap();
    // Natural completion wins the race; the reply follows.
    let done = assistant(
        json!([{"type": "text", "text": "READY"}]),
        "stop",
        &canonical_usage(),
        None,
    );
    let mut after = vec![message_end(&done)];
    after.extend(settle(&done));
    after.push(abort_reply());
    after.push(eof());
    let mut wanted = completed(1, "READY");
    wanted["stop_facts"] = json!({"acknowledged": false, "forced": false});
    abort_case("pi_abort_natural", (Vec::new(), after), "accepted", wanted).unwrap();
    // A 401 racing the abort: not a marker.
    let auth = assistant(
        json!([]),
        "error",
        &zero_usage(),
        Some("401: {\"message\":\"expired\",\"type\":\"auth_error\"}"),
    );
    let mut after = vec![message_end(&auth)];
    after.extend(settle(&auth));
    after.push(abort_reply());
    after.push(eof());
    let mut wanted = completed(1, "");
    wanted["terminal"] = json!({"status": "failed", "stop_reason": "error",
        "vendor_stop_reason": "error", "class_hint": "auth"});
    wanted["final_text"] = Value::Null;
    wanted["stop_facts"] = json!({"acknowledged": false, "forced": false});
    unset(&mut wanted, "usage");
    abort_case(
        "pi_abort_auth_race",
        (Vec::new(), after),
        "accepted",
        wanted,
    )
    .unwrap();
    // The reply delayed past settlement: awaited, then EOF.
    let marker = assistant(
        json!([]),
        "error",
        &zero_usage(),
        Some("This operation was aborted"),
    );
    let mut after = tool_end(true);
    after.push(message_end(&marker));
    after.extend(settle(&marker));
    after.push(json!({"delay": {"ms": 400}}));
    after.push(abort_reply());
    after.push(eof());
    abort_case(
        "pi_abort_late_reply",
        (tool_call(&canonical_usage()), after),
        "tool_started",
        interrupted(true, "error"),
    )
    .unwrap();
    // No reply by `force_at`: not acknowledged, the marker not retained;
    // the group is stopped at `force_at`.
    let mut after = tool_end(true);
    after.push(message_end(&marker));
    after.extend(settle(&marker));
    after.extend(terminated());
    let mut wanted = interrupted(false, "error");
    wanted["terminal"] = Value::Null;
    wanted["error"] = Value::Null;
    wanted["stop_facts"] = json!({"acknowledged": false, "forced": true});
    wanted["exit"] = json!({"code": 143, "signal": null});
    abort_case(
        "pi_abort_no_reply",
        (tool_call(&canonical_usage()), after),
        "tool_started",
        wanted,
    )
    .unwrap();
}

/// `pi_abort` (packet §7.1, review r1 #1): Pi exits before the abort's
/// reply. A marker terminal is not retained either, and the stop order's
/// row applies (nothing forced: Pi ended on its own); an ordinary
/// terminal stands.
#[test]
fn pi_abort_exit_before_reply() {
    let marker = assistant(
        json!([]),
        "error",
        &zero_usage(),
        Some("This operation was aborted"),
    );
    let mut after = tool_end(true);
    after.push(message_end(&marker));
    after.extend(settle(&marker));
    after.push(json!({"exit": {"code": 0, "stderr": ""}}));
    let mut wanted = interrupted(false, "error");
    wanted["terminal"] = Value::Null;
    wanted["error"] = Value::Null;
    wanted["stop_facts"] = json!({"acknowledged": false, "forced": false});
    abort_case(
        "pi_abort_exit_no_reply",
        (tool_call(&canonical_usage()), after),
        "tool_started",
        wanted,
    )
    .unwrap();
    // An ordinary terminal stands when Pi exits before the reply.
    let done = assistant(
        json!([{"type": "text", "text": "READY"}]),
        "stop",
        &canonical_usage(),
        None,
    );
    let mut after = vec![message_end(&done)];
    after.extend(settle(&done));
    after.push(json!({"exit": {"code": 0, "stderr": ""}}));
    let mut wanted = completed(1, "READY");
    wanted["stop_facts"] = json!({"acknowledged": false, "forced": false});
    abort_case(
        "pi_abort_natural_exit",
        (Vec::new(), after),
        "accepted",
        wanted,
    )
    .unwrap();
}

/// `pi_eof_is_stop` (E31): Pi's stdout ends mid-run with exit 0 and no
/// terminal (what EOF on its stdin does): a process exit, never
/// `Completed`, whatever the exit status.
#[test]
fn pi_eof_is_stop() {
    let mut steps = handshake(&State::default());
    steps.extend(prompt("Say READY."));
    steps.extend(echo("Say READY."));
    steps.push(update(json!({"type": "text_start", "contentIndex": 0})));
    steps.push(json!({"exit": {"code": 0, "stderr": ""}}));
    let replay = single(
        "synthetic (via-jt8.3.1): stdout ends mid-run, exit 0 (E31)",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = completed(1, "");
    wanted["terminal"] = Value::Null;
    wanted["final_text"] = Value::Null;
    wanted["error"] = json!("process_exit");
    unset(&mut wanted, "usage");
    let expect = case("pi_eof_is_stop", 1, vec![turn("Say READY.", wanted)]);
    check_built("pi_eof_is_stop", &replay, &expect, Knobs::default()).unwrap();
}

/// `pi_signals_cleanup` (packet §7.2-7.3): the daemon force stops Pi's
/// group with `SIGTERM` (the fake takes it and exits 143, as Pi's handler
/// does; a `SIGINT` would fail the replay); a startup that never answers
/// is bounded by the turn's deadline and the group stop. The leftover scan
/// of escaped tools is Core's (C2 §4.2): the launch environment carries
/// Host's marker, which the launch recipe's unit tests pin.
#[test]
fn pi_signals_cleanup() {
    let mut steps = handshake(&State::default());
    steps.extend(prompt("Run sleep 30 with bash."));
    steps.extend(echo("Run sleep 30 with bash."));
    steps.extend(tool_call(&canonical_usage()));
    steps.push(json!({"await_signal": {"signal": "SIGTERM"}}));
    steps.push(json!({"exit": {"code": 143, "stderr": ""}}));
    let gate = steps.len() - 1;
    let replay = single(
        "synthetic (via-jt8.3.1): the daemon force during a tool",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = completed(1, "");
    wanted["terminal"] = Value::Null;
    wanted["final_text"] = Value::Null;
    wanted["error"] = json!("force_stop");
    wanted["exit"] = json!({"code": 143, "signal": null});
    unset(&mut wanted, "usage");
    let expect = case(
        "pi_signals_force",
        1,
        vec![turn("Run sleep 30 with bash.", wanted)],
    );
    let knobs = Knobs {
        force_on: Some(Box::leak(format!("at {gate} launch 1").into_boxed_str())),
        ..Knobs::default()
    };
    check_built("pi_signals_force", &replay, &expect, knobs).unwrap();
    // A silent startup: no handshake reply until the wall.
    let replay = single(
        "synthetic (via-jt8.3.1): a startup that never answers (E30)",
        argv(Argv::default()),
        vec![
            expect_line(json!({"type": "get_state"})),
            expect_line(json!({"type": "get_available_models"})),
            expect_line(json!({"type": "get_commands"})),
            json!({"await_signal": {"signal": "SIGTERM"}}),
            json!({"exit": {"code": 143, "stderr": ""}}),
        ],
    );
    let mut wanted = unaccepted(None, Some("deadline"), Some(143));
    wanted["group_absent"] = json!(true);
    let mut turn = turn("Say READY.", wanted);
    turn["deadlines"] = json!({"wall_ms": 1_500, "idle_ms": 1_500});
    let expect = case("pi_signals_silent_startup", 1, vec![turn]);
    check_built(
        "pi_signals_silent_startup",
        &replay,
        &expect,
        Knobs::default(),
    )
    .unwrap();
}

/// Progress records that overfill the session channel (1,024 items),
/// within Route's read-ahead (1,024 records past the hop).
const SATURATING: usize = 1_200;

/// `pi_dialog_decline` (packet §6): a dialog request with an `id` (a `-e`
/// extension's `confirm`) and an unknown method with an `id` are each
/// cancelled on the control lane within 5 s while Core takes no
/// observation (the knob holds Core's consumer until the fake read the
/// replies), then reported `vendor.request_declined`; a fire-and-forget
/// `notify` with an `id` is answered and not reported; a dialog without an
/// `id` fails closed as protocol.
#[test]
fn pi_dialog_decline() {
    let request = |id: &str, method: &str, title: &str| {
        emit(
            &json!({"type": "extension_ui_request", "id": id, "method": method,
            "title": title, "message": "Run bash?"}),
        )
    };
    let response = |id: &str| {
        json!({"expect": {"line": {"type": "extension_ui_response", "id": id, "cancelled": true},
            "within_ms": 5_250}})
    };
    let mut steps = handshake(&State::default());
    steps.extend(prompt("Run a tool."));
    steps.extend(echo("Run a tool."));
    // Review r1 #6: more progress than the session channel's 1,024 items
    // while Core takes none, so the Adapter is blocked delivering when the
    // dialogs come; Route still reads and declines each within 5 s.
    let pending = assistant(json!([]), "pending", &zero_usage(), None);
    steps.push(emit(&json!({"type": "message_start", "message": pending})));
    steps.push(update(json!({"type": "text_start", "contentIndex": 0})));
    for _ in 0..SATURATING {
        steps.push(update(
            json!({"type": "text_delta", "contentIndex": 0, "delta": "."}),
        ));
    }
    steps.push(json!({"await_signal": {"signal": "SIGUSR1"}}));
    let gate = steps.len();
    steps.push(request("ui-1", "confirm", "Allow tool?"));
    steps.push(response("ui-1"));
    steps.push(request("ui-2", "frobnicate", "Unknown"));
    steps.push(response("ui-2"));
    steps.push(request("ui-3", "notify", "FYI"));
    steps.push(response("ui-3"));
    steps.extend(answer("READY", "stop", &canonical_usage()));
    steps.extend(settle(&assistant(
        json!([{"type": "text", "text": "READY"}]),
        "stop",
        &canonical_usage(),
        None,
    )));
    steps.push(eof());
    let replay = single(
        "synthetic (via-jt8.3.1): extension dialogs (E44, E64)",
        argv(Argv::default()),
        steps,
    );
    let mut wanted = completed(1, "READY");
    wanted["observations_include"] = json!([
        confirmed(1),
        "turn.accepted",
        {"kind": "vendor.request_declined", "vendor_method": "extension_ui/confirm",
            "summary": "Allow tool?", "blocking": true},
        {"kind": "vendor.request_declined", "vendor_method": "extension_ui/frobnicate",
            "summary": "Unknown", "blocking": true},
    ]);
    wanted["observation_counts"]["vendor.request_declined"] = json!(2);
    // Nothing lost under saturation: the deltas with their `text_start`,
    // then the answer's `text_start`, `text_delta` and `message_end`
    // sample.
    wanted["observation_counts"]["progress"] = json!(SATURATING + 4);
    let mut declined = turn("Run a tool.", wanted);
    // At the gate, before the first dialog: the channel is full.
    declined["gates"] = json!([{"step": gate, "expect": {"terminal": null}}]);
    let expect = case("pi_dialog_decline", 1, vec![declined]);
    // The seventh input line is the last decline: the three handshake
    // commands and the prompt, then three replies.
    let knobs = Knobs {
        hold_until_read: Some(7),
        ..Knobs::default()
    };
    let fences = std::cell::RefCell::new(Vec::new());
    let outcome = drive_built(
        "pi_dialog_decline",
        &replay,
        &expect,
        knobs,
        Box::new(|_| Ok(())),
        Box::new(|pure: &Pure| {
            *fences.borrow_mut() = pure.gate_fences.borrow().clone();
            Ok(())
        }),
    )
    .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
    // Saturated at the gate: Route read past what the Adapter could
    // deliver, the channel's 1,024 items taken and none handled.
    match fences.into_inner().as_slice() {
        [(decoded, delivered)] if *delivered >= 1_024 && decoded > delivered => {}
        other => panic!("not saturated at the gate: (decoded, delivered) {other:?}"),
    }
    // A dialog with no `id` cannot be answered: protocol.
    let mut run = prompt("Say READY.");
    run.extend(echo("Say READY."));
    run.push(emit(
        &json!({"type": "extension_ui_request", "method": "confirm",
        "title": "Allow tool?"}),
    ));
    run.extend(terminated());
    let (replay, mut expect) = protocol_case("pi_dialog_no_id", run, true);
    expect["turns"][0]["expect"]["exit"] = json!({"code": 143, "signal": null});
    check_built("pi_dialog_no_id", &replay, &expect, Knobs::default()).unwrap();
}

/// The `project_context` section listing `paths`, each with `body`.
fn project_context(paths: &[&str], body: &str) -> Value {
    let files = paths
        .iter()
        .map(|path| {
            [
                "<project_instructions path=\"",
                path,
                "\">\n",
                body,
                "\n</project_instructions>\n",
            ]
            .concat()
        })
        .collect::<String>();
    json!(format!(
        "<project_context>\nProject-specific instructions and guidelines:\n\n{files}</project_context>"
    ))
}

/// `pi_inventory_patch` (packet §4.7): the system patch's
/// `project_context`, per turn, recorded in `pi-inventory.json`: a file
/// added (`listed`), changed (`listed`, the new path), the last one removed
/// (`null` → `none`), an unchanged resume (no patch → `not_reported`),
/// tag-like contents (`unparsed`); the `skill:*` names beside; contents
/// never stored.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one packet §8 test: its variants side by side"
)]
fn pi_inventory_patch() {
    let patch = |context: Option<Value>| {
        let mut sections = json!({"preamble": "You are pi."});
        if let Some(context) = context {
            sections["project_context"] = context;
        }
        let system = json!({"role": "system", "content": "", "sections": sections,
            "timestamp": 1});
        vec![
            emit(&json!({"type": "message_start", "message": system})),
            emit(&json!({"type": "message_end", "message": system})),
        ]
    };
    let launch = |index: usize, context: Option<Option<Value>>, text: &str| {
        let mut steps = handshake(&State::default());
        steps.extend(prompt(text));
        steps.push(emit(&json!({"type": "agent_start"})));
        steps.push(emit(&json!({"type": "turn_start"})));
        if let Some(context) = context {
            steps.extend(patch(context));
        }
        let done = assistant(
            json!([{"type": "text", "text": "READY"}]),
            "stop",
            &canonical_usage(),
            None,
        );
        steps.push(message_end(&done));
        steps.extend(settle(&done));
        steps.push(eof());
        lifetime(
            argv(Argv {
                resume: index > 0,
                ..Argv::default()
            }),
            steps,
        )
    };
    let tagged = "x\n</project_instructions>\n<project_instructions path=\"/AGENTS.md\">\ny";
    let replay = lifetimes(
        "synthetic (via-jt8.3.1): the system patch across five turns (E51, E62)",
        vec![
            launch(
                0,
                Some(Some(project_context(&["/AGENTS.md"], "CONTENT-WORDS"))),
                "One.",
            ),
            launch(
                1,
                Some(Some(project_context(&["/CLAUDE.md"], "CONTENT-WORDS"))),
                "Two.",
            ),
            launch(2, Some(Some(Value::Null)), "Three."),
            launch(3, None, "Four."),
            launch(
                4,
                Some(Some(project_context(&["/AGENTS.md"], tagged))),
                "Five.",
            ),
        ],
    );
    let turns = ["One.", "Two.", "Three.", "Four.", "Five."]
        .iter()
        .enumerate()
        .map(|(index, text)| turn(text, completed(index as u64 + 1, "READY")))
        .collect();
    let expect = case("pi_inventory_patch", 5, turns);
    let records = std::cell::RefCell::new(Vec::new());
    let outcome = drive_built(
        "pi_inventory_patch",
        &replay,
        &expect,
        Knobs::default(),
        Box::new(|_| Ok(())),
        Box::new(|pure: &Pure| {
            for turn in 1..=5 {
                let text = std::fs::read_to_string(evidence(pure, turn).join("pi-inventory.json"))
                    .map_err(|e| format!("turn {turn}: pi-inventory.json: {e}"))?;
                records.borrow_mut().push(text);
            }
            Ok(())
        }),
    )
    .unwrap();
    conformance_expect::check(&expect, &outcome).unwrap();
    let records: Vec<Value> = records
        .into_inner()
        .iter()
        .map(|text| {
            assert!(!text.contains("CONTENT-WORDS"), "contents stored: {text}");
            serde_json::from_str(text).unwrap()
        })
        .collect();
    let state = |record: &Value| record["instruction_files"]["state"].clone();
    assert_eq!(state(&records[0]), "listed");
    assert_eq!(
        records[0]["instruction_files"]["paths"],
        json!(["/AGENTS.md"])
    );
    assert_eq!(state(&records[1]), "listed");
    assert_eq!(
        records[1]["instruction_files"]["paths"],
        json!(["/CLAUDE.md"])
    );
    assert_eq!(state(&records[2]), "none");
    assert_eq!(state(&records[3]), "not_reported");
    assert_eq!(state(&records[4]), "unparsed");
    for record in &records {
        assert_eq!(
            record["skills"]["names"],
            json!(["skill:review"]),
            "{record}"
        );
    }
}

/// `pi_detail_redaction` (packet §5.4, E53): a 401 whose body echoes a
/// key-like fragment gives VIA-owned `detail` built from the status and
/// the body's safe `type`, and `vendor_code` from its safe `code`; the
/// fragment never reaches `detail`, `vendor_code`, the failure message,
/// warnings, any observation or the turn's evidence records. An unsafe
/// `code` is dropped. The transcript hint names the session file.
#[test]
fn pi_detail_redaction() {
    let body = |code: &str| {
        format!(
            "OpenAI API error (401): {{\"message\":\"Incorrect API key provided: {KEY_FRAGMENT}.\",\"type\":\"invalid_request_error\",\"code\":\"{code}\",\"param\":null}}"
        )
    };
    for (name, code, vendor_code) in [
        (
            "pi_detail_redaction",
            "invalid_api_key",
            json!("invalid_api_key"),
        ),
        (
            "pi_detail_redaction_unsafe_code",
            "Invalid Key!",
            Value::Null,
        ),
    ] {
        let error = body(code);
        let last = assistant(json!([]), "error", &zero_usage(), Some(&error));
        let mut steps = handshake(&State::default());
        steps.extend(prompt("Say READY."));
        steps.extend(echo("Say READY."));
        steps.push(message_end(&last));
        steps.extend(settle(&last));
        steps.push(eof());
        let replay = single(
            "synthetic (via-jt8.3.1): the live 401 shape (E53), its key masked",
            argv(Argv::default()),
            steps,
        );
        let mut wanted = completed(1, "");
        wanted["terminal"] = json!({"status": "failed", "stop_reason": "error",
            "vendor_stop_reason": "error", "class_hint": "auth",
            "detail": "provider error 401 invalid_request_error", "vendor_code": vendor_code});
        wanted["final_text"] = Value::Null;
        unset(&mut wanted, "usage");
        let expect = case(name, 1, vec![turn("Say READY.", wanted)]);
        let evidence_text = std::cell::RefCell::new(String::new());
        let outcome = drive_built(
            name,
            &replay,
            &expect,
            Knobs::default(),
            Box::new(|_| Ok(())),
            Box::new(|pure: &Pure| {
                let folder = evidence(pure, 1);
                let mut text = String::new();
                for record in ["pi-profile.json", "pi-inventory.json"] {
                    text.push_str(
                        &std::fs::read_to_string(folder.join(record)).unwrap_or_default(),
                    );
                }
                *evidence_text.borrow_mut() = text;
                Ok(())
            }),
        )
        .unwrap();
        conformance_expect::check(&expect, &outcome).unwrap_or_else(|e| panic!("{name}:\n{e}"));
        let turn = &outcome.turns[0];
        let mut seen = observed_text(&outcome);
        seen.push_str(
            &turn
                .terminal
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default(),
        );
        seen.push_str(&turn.message.clone().unwrap_or_default());
        seen.push_str(&turn.warnings.join(" "));
        seen.push_str(&evidence_text.into_inner());
        for leaked in [KEY_FRAGMENT, "Incorrect API key"] {
            assert!(
                !seen.contains(leaked),
                "{name}: {leaked} left the vendor record"
            );
        }
    }
}

/// `pi_record_ceiling` (packet §4.5, runtime §8): a prompt at the admitted
/// maximum, 524,288 JSON-encoded bytes of control characters, is written
/// whole and its echo read back; a record over 1 MiB fails `overflow`,
/// never a short result.
#[test]
fn pi_record_ceiling() {
    let text: String = std::iter::repeat_n('\u{1}', (524_288 - 2) / 6).collect();
    assert_eq!(serde_json::to_string(&text).unwrap().len(), 524_288);
    let mut steps = handshake(&State::default());
    steps.push(
        json!({"expect": {"line": {"type": "prompt"}, "capture": {"p": "/id", "msg": "/message"}}}),
    );
    steps.push(started());
    steps.push(emit(&json!({"type": "agent_start"})));
    let echo = r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":${msg}}],"timestamp":1}}"#;
    steps.push(emit_line(echo));
    let done = assistant(
        json!([{"type": "text", "text": "READY"}]),
        "stop",
        &canonical_usage(),
        None,
    );
    steps.push(message_end(&done));
    steps.extend(settle(&done));
    steps.push(eof());
    let replay = single(
        "synthetic (via-jt8.3.1): the admitted maximum prompt (E47, E57)",
        argv(Argv::default()),
        steps,
    );
    let expect = case(
        "pi_record_ceiling",
        1,
        vec![turn(&text, completed(1, "READY"))],
    );
    check_built("pi_record_ceiling", &replay, &expect, Knobs::default()).unwrap();
    // One byte more is refused before any receipt.
    let longer: String = format!("{text}a");
    let expect = case(
        "pi_record_ceiling_refused",
        0,
        vec![turn(&longer, plan_refused("invalid_param:prompt"))],
    );
    check_built(
        "pi_record_ceiling_refused",
        &replay,
        &expect,
        Knobs::default(),
    )
    .unwrap();
    // A record over 1 MiB.
    let mut run = prompt("Say READY.");
    run.extend(echo_steps("Say READY."));
    run.push(update(json!({"type": "text_delta", "contentIndex": 0,
        "delta": "x".repeat(1024 * 1024 + 1)})));
    run.extend(terminated());
    let (replay, mut expect) = protocol_case("pi_record_overflow", run, true);
    expect["turns"][0]["expect"]["error"] = json!("overflow");
    expect["turns"][0]["expect"]["exit"] = json!({"code": 143, "signal": null});
    check_built("pi_record_overflow", &replay, &expect, Knobs::default()).unwrap();
}

/// [`echo`], under a name that does not shadow a local.
fn echo_steps(text: &str) -> Vec<Value> {
    echo(text)
}

// ---------------------------------------------------------------------
// The fixture files.
// ---------------------------------------------------------------------

/// The fixtures on disk: the canonical shapes the named tests load.
const FIXTURES: [&str; 4] = [
    "pi_abort",
    "pi_acceptance",
    "pi_identity_continuation",
    "pi_plan_pure",
];

/// Every expectation file is a listed fixture with its replay, names this
/// harness, validates against the unified schema and gates only
/// `await_signal` steps of its replay.
#[test]
fn conformance_pi_fixture_files_match() {
    let dir = fixtures();
    assert_eq!(conformance_expect::case_names(&dir).unwrap(), FIXTURES);
    for name in FIXTURES {
        let (replay, expect) = fixture(name).unwrap();
        assert_eq!(expect["harness"], "pi", "{name}: harness");
        conformance_expect::validate(&expect).unwrap_or_else(|e| panic!("{name}: {e}"));
        conformance_expect::gates_resolve(&expect, &replay)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        conformance_expect::check(&expect, &conformance_expect::ideal(&expect))
            .unwrap_or_else(|e| panic!("{name}: ideal outcome refused:\n{e}"));
    }
}

/// The shared replay-exit check accepts each Pi fixture's own end.
#[test]
fn conformance_pi_replay_exit_is_judged() {
    let checked = conformance_expect::replay_exit_self_check(&fixtures()).unwrap();
    assert!(checked > 0, "no replay lifetimes checked");
}
