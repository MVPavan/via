//! The Codex adapter's pure pieces: the server launch recipe and its key,
//! the normalizer over the recorded 0.159.2 exchange, the version and
//! catalog parsing, and the decline bodies against the vendor schemas.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tokio::time::Instant;
use via_routes::codex::{
    Incoming, ModelListResult, Notification, RequestId, ServerRequest, decode, result,
};

use super::launch::{ConfigHash, PROTOCOL_PIN, ServerRecipe};
use super::normalize::{
    DECLINES, NormalizeError, Step, StructuredOutput, TurnNormalizer, catalog_page, decline,
    instance_version, version_status,
};
use crate::VendorTerminalStatus;
use crate::config::BootstrapEnv;
use crate::observation::{
    ClassHint, DenialKind, Observation, ProgressMarks, StopReason, UsageSample,
};
use crate::plan::{Inherit, InheritState, VersionStatus};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex")
}

/// A replay line with each `${capture}` template filled with `1`.
fn filled(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let end = rest[start..].find('}').unwrap();
        out.push('1');
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// Every emitted line of a recorded replay fixture, in order, templates
/// filled.
fn emitted(case: &str) -> Vec<String> {
    let text = std::fs::read_to_string(fixtures().join(format!("{case}.replay.json"))).unwrap();
    let replay: Value = serde_json::from_str(&text).unwrap();
    replay["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|step| step["emit"]["line"].as_str().map(filled))
        .collect()
}

/// The recorded notifications of one turn, decoded.
fn turn_notifications(case: &str, turn_id: &str) -> Vec<Notification> {
    emitted(case)
        .iter()
        .filter(|line| line.contains(turn_id))
        .filter_map(|line| match decode(line.as_bytes()).unwrap() {
            Incoming::Notification(notification) => Some(notification),
            Incoming::Response(_) | Incoming::Request(_) => None,
        })
        .collect()
}

/// A final-text observation's text.
fn final_text(observation: &Observation) -> Option<&str> {
    if let Observation::FinalText(text) = observation {
        Some(text)
    } else {
        None
    }
}

/// A progress observation's marks.
fn progress(observation: &Observation) -> Option<&ProgressMarks> {
    if let Observation::Progress(marks) = observation {
        Some(marks)
    } else {
        None
    }
}

/// What the normalizer makes of one recorded turn: its observations and
/// its terminal step.
fn normalized(case: &str, turn_id: &str, schema: bool) -> (Vec<Observation>, Option<Step>) {
    let mut normalizer = TurnNormalizer::new(schema);
    let mut observations = Vec::new();
    let mut terminal = None;
    for notification in turn_notifications(case, turn_id) {
        match normalizer.observe(&notification, Instant::now()).unwrap() {
            Step::Observations(more) => observations.extend(more),
            Step::Activity => {}
            step @ Step::Terminal { .. } => {
                assert!(terminal.is_none(), "one terminal");
                terminal = Some(step);
            }
        }
    }
    (observations, terminal)
}

fn env() -> BootstrapEnv {
    BootstrapEnv::from_vars([
        ("HOME", "/home/u"),
        ("PATH", "/usr/bin"),
        ("USER", "u"),
        ("LOGNAME", "u"),
        ("LANG", "C.UTF-8"),
        ("XDG_RUNTIME_DIR", "/run/user/1"),
        ("VIA_FAKE_SCENARIO", "/elsewhere"),
    ])
}

fn hooks(state: InheritState) -> Inherit {
    serde_json::from_value(
        json!({"hooks": state, "mcp_servers": "off", "plugins": "on",
        "skills": "on", "agents": "on", "instruction_files": "on"}),
    )
    .unwrap()
}

fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).into(), (*value).into()))
        .collect()
}

/// Packet §4, Q6: the server's argv disables hooks when they are off, its
/// environment is exactly the allow-list plus the supplied
/// `CODEX_SQLITE_HOME`, and it runs in that directory.
#[test]
fn the_server_recipe_is_the_allow_list() {
    let home = Path::new("/state/vendor/codex");
    let recipe = ServerRecipe::new(
        Path::new("/bin/codex"),
        hooks(InheritState::Off),
        &env(),
        home,
    );
    assert_eq!(recipe.program, Path::new("/bin/codex"));
    assert_eq!(recipe.args, ["app-server", "--disable", "hooks"]);
    assert_eq!(recipe.cwd, home);
    assert_eq!(
        recipe.env,
        os(&[
            ("CODEX_SQLITE_HOME", "/state/vendor/codex"),
            ("HOME", "/home/u"),
            ("LANG", "C.UTF-8"),
            ("LOGNAME", "u"),
            ("PATH", "/usr/bin"),
            ("USER", "u"),
            ("XDG_RUNTIME_DIR", "/run/user/1"),
        ])
    );
    let on = ServerRecipe::new(
        Path::new("/bin/codex"),
        hooks(InheritState::On),
        &BootstrapEnv::from_vars([("PATH", "/usr/bin")]),
        home,
    );
    assert_eq!(on.args, ["app-server"]);
    assert_eq!(
        on.env,
        os(&[
            ("CODEX_SQLITE_HOME", "/state/vendor/codex"),
            ("PATH", "/usr/bin")
        ])
    );
}

/// X0 item 3: equal inputs give an equal hash; each recipe component
/// changes it; the display is 16 lowercase hex digits.
#[test]
fn the_config_hash_covers_the_recipe() {
    let binary = Path::new("/opt/vendor/codex");
    let recipe = |hooks_state, home: &str| {
        ServerRecipe::new(binary, hooks(hooks_state), &env(), Path::new(home))
    };
    let base = recipe(InheritState::Off, "/state/vendor/codex");
    let hash = base.config_hash("0.1.0");
    assert_eq!(
        hash,
        recipe(InheritState::Off, "/state/vendor/codex").config_hash("0.1.0")
    );
    let display = hash.display();
    assert_eq!(display.len(), 16);
    assert!(
        display
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    );
    let mut other_env = base.clone();
    other_env.env[1].1 = "/home/v".into();
    let mut other_program = base.clone();
    other_program.program = "/opt/vendor/codex2".into();
    let changed: [(&str, ConfigHash); 5] = [
        (
            "argv",
            recipe(InheritState::On, "/state/vendor/codex").config_hash("0.1.0"),
        ),
        (
            "home and cwd",
            recipe(InheritState::Off, "/other").config_hash("0.1.0"),
        ),
        ("environment", other_env.config_hash("0.1.0")),
        ("program", other_program.config_hash("0.1.0")),
        ("adapter version", base.config_hash("0.2.0")),
    ];
    for (what, other) in changed {
        assert_ne!(hash, other, "{what}");
    }
    assert!(PROTOCOL_PIN.contains("experimental=none"));
}

/// C2 §5 OD1: the version is `userAgent` without VIA's own `via/` prefix,
/// up to the first space; another prefix is no version.
#[test]
fn the_instance_version_comes_from_the_user_agent() {
    let init: via_routes::codex::InitializeResult = result(
        &serde_json::value::to_raw_value(&json!({
            "userAgent": "via/0.159.2 (Linux 6.0.0; x86_64) unknown (via; 0.0.0)"
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(instance_version(&init.user_agent), Some("0.159.2"));
    assert_eq!(instance_version("via/0.160.0"), Some("0.160.0"));
    for malformed in [
        "codex/0.159.2 (Linux)",
        "via/ (Linux)",
        "via/",
        "",
        "0.159.2",
    ] {
        assert_eq!(instance_version(malformed), None, "{malformed:?}");
    }
    assert_eq!(version_status("0.159.2"), VersionStatus::Tested);
    assert_eq!(version_status("0.160.0"), VersionStatus::Untested);
}

/// Packet §3: a `model/list` page gives each model's advertised efforts
/// and the next cursor (the recorded catalog: three models, one page).
#[test]
fn a_model_list_page_parses() {
    let reply = emitted("c7_effort_catalog")
        .into_iter()
        .find(|line| line.contains("supportedReasoningEfforts"))
        .unwrap();
    let Incoming::Response(response) = decode(reply.as_bytes()).unwrap() else {
        panic!("the model/list reply");
    };
    let page = catalog_page(result::<ModelListResult>(&response.outcome.unwrap()).unwrap());
    assert_eq!(page.next_cursor, None);
    let luna = page
        .models
        .iter()
        .find(|model| model.model == "gpt-6-luna")
        .unwrap();
    assert_eq!(luna.efforts, ["low", "medium", "high", "xhigh", "max"]);
    assert!(!luna.supports("ultra"));
    assert!(luna.supports("low"));
    assert_eq!(
        page.models
            .iter()
            .map(|model| model.model.as_str())
            .collect::<Vec<_>>(),
        ["gpt-6.1-sol", "gpt-6-sol", "gpt-6-luna"]
    );
}

/// Packet §5 over recorded c1 turn 1: model progress, keyless `last`
/// usage samples whose sum is the turn's (20522 + 20613 input-side), the
/// final-answer text only (never commentary), and a completed terminal
/// with the thread total in `vendor`.
#[test]
fn a_completed_turn_normalizes() {
    let (observations, terminal) = normalized(
        "c1_commentary_usage",
        "019a0000-0000-7000-8000-000000200001",
        false,
    );
    let finals: Vec<&str> = observations.iter().filter_map(final_text).collect();
    assert_eq!(
        finals,
        ["I couldn\u{2019}t create `note.txt`: the write was rejected."]
    );
    let samples: Vec<&UsageSample> = observations
        .iter()
        .filter_map(progress)
        .filter_map(|marks| marks.usage.as_ref())
        .collect();
    assert_eq!(samples.len(), 2);
    assert!(samples.iter().all(|sample| sample.key.is_none()));
    let sum = |field: fn(&UsageSample) -> Option<u64>| {
        samples.iter().map(|s| field(s).unwrap()).sum::<u64>()
    };
    assert_eq!(sum(|s| s.input), 40982);
    assert_eq!(sum(|s| s.cached_input), 38400);
    assert_eq!(sum(|s| s.output), 153);
    assert_eq!(sum(|s| s.reasoning_output), 26);
    assert_eq!(sum(|s| s.total), 41135);
    assert!(observations.iter().any(|o| matches!(
        o,
        Observation::Progress(marks) if marks.model
    )));
    let Some(Step::Terminal {
        terminal,
        structured,
    }) = terminal
    else {
        panic!("a terminal");
    };
    assert_eq!(terminal.status, VendorTerminalStatus::Completed);
    assert_eq!(terminal.stop_reason, StopReason::EndTurn);
    assert_eq!(terminal.vendor_stop_reason, "completed");
    assert_eq!(terminal.class_hint, None);
    assert!(terminal.usage.is_none() && terminal.cost.is_none());
    assert_eq!(structured, StructuredOutput::NotRequested);
    let vendor: Value = serde_json::from_str(terminal.vendor.unwrap().get()).unwrap();
    assert_eq!(
        vendor,
        json!({"total": {"totalTokens": 41135, "inputTokens": 40982,
            "cachedInputTokens": 38400, "cacheWriteInputTokens": 0, "outputTokens": 153,
            "reasoningOutputTokens": 26},
            "cacheWriteInputTokens": 0, "modelContextWindow": 258_400})
    );
}

/// c1 turn 2: two final-answer messages are two pieces, in order.
#[test]
fn every_final_answer_message_is_final_text() {
    let (observations, _) = normalized(
        "c1_commentary_usage",
        "019a0000-0000-7000-8000-000000200002",
        false,
    );
    let finals: Vec<&str> = observations.iter().filter_map(final_text).collect();
    assert_eq!(
        finals,
        [
            "What would you like me to answer after your clarification: the `note.txt` result or a new request?",
            "What would you like me to answer: the `note.txt` result or a new request?",
        ]
    );
}

/// c11: a tool item's start and completion are `tools_started` (ID and
/// type) and `tools_ended`.
#[test]
fn tool_items_open_and_close() {
    let (observations, _) = normalized(
        "c11_failed_command",
        "019a0000-0000-7000-8000-000000200001",
        false,
    );
    let id = "exec-019a0000-0000-7000-8000-000000400004";
    let started: Vec<_> = observations
        .iter()
        .filter_map(progress)
        .filter(|marks| !marks.tools_started.is_empty())
        .map(|marks| marks.tools_started.clone())
        .collect();
    assert_eq!(
        started,
        [vec![(id.to_owned(), "commandExecution".to_owned())]]
    );
    assert!(observations.iter().any(|o| matches!(
        o,
        Observation::Progress(marks) if marks.tools_ended == [id.to_owned()]
    )));
}

/// Class hints (C2 §6.2): c8's `httpConnectionFailed{401}` is `auth` after
/// the retried errors, which are no terminal; c7's `other` is
/// `vendor_error`; c3 is interrupted with no class.
#[test]
fn failed_and_interrupted_terminals_map() {
    let terminal = |case: &str, turn: &str| match normalized(case, turn, false).1 {
        Some(Step::Terminal { terminal, .. }) => terminal,
        _ => panic!("{case}: no terminal"),
    };
    let auth = terminal("c8_auth", "019a0000-0000-7000-8000-000000200001");
    assert_eq!(auth.status, VendorTerminalStatus::Failed);
    assert_eq!(auth.stop_reason, StopReason::Error);
    assert_eq!(auth.class_hint, Some(ClassHint::Auth));
    assert_eq!(auth.vendor_code.as_deref(), Some("httpConnectionFailed"));
    let model = terminal("c7_bad_model", "019a0000-0000-7000-8000-000000200001");
    assert_eq!(model.class_hint, Some(ClassHint::VendorError));
    assert_eq!(model.vendor_code.as_deref(), Some("other"));
    assert!(model.detail.unwrap().contains("via-nonexistent-model"));
    let interrupted = terminal(
        "c3_interrupt_uncertain",
        "019a0000-0000-7000-8000-000000200001",
    );
    assert_eq!(interrupted.status, VendorTerminalStatus::Interrupted);
    assert_eq!(interrupted.stop_reason, StopReason::Interrupted);
    assert_eq!(interrupted.class_hint, None);
}

/// Each `codexErrorInfo` code's class (C2 §6.2 Codex row); any 401 or 403
/// is `auth` whatever the code.
#[test]
fn each_error_code_has_its_class() {
    let class = |info: Value| {
        let line = json!({"method": "turn/completed", "params": {"threadId": "t",
            "turn": {"id": "u", "status": "failed",
                "error": {"message": "m", "codexErrorInfo": info}}}});
        let Incoming::Notification(notification) = decode(line.to_string().as_bytes()).unwrap()
        else {
            panic!("a notification");
        };
        match TurnNormalizer::new(false)
            .observe(&notification, Instant::now())
            .unwrap()
        {
            Step::Terminal { terminal, .. } => terminal.class_hint,
            Step::Observations(_) | Step::Activity => panic!("no terminal"),
        }
    };
    for (info, expected) in [
        (json!("rateLimitExceeded"), ClassHint::RateLimit),
        (json!("unauthorized"), ClassHint::Auth),
        (json!("contextWindowExceeded"), ClassHint::ContextExceeded),
        (json!("usageLimitExceeded"), ClassHint::BudgetExceeded),
        (json!("sessionBudgetExceeded"), ClassHint::BudgetExceeded),
        (json!("tooManyDenials"), ClassHint::VendorError),
        (json!("flexUnavailable"), ClassHint::VendorError),
        (json!("other"), ClassHint::VendorError),
        (Value::Null, ClassHint::VendorError),
        (
            json!({"httpConnectionFailed": {"httpStatusCode": 403}}),
            ClassHint::Auth,
        ),
        (
            json!({"responseStreamDisconnected": {"httpStatusCode": 401}}),
            ClassHint::Auth,
        ),
        (
            json!({"httpConnectionFailed": {"httpStatusCode": 500}}),
            ClassHint::VendorError,
        ),
    ] {
        assert_eq!(class(info.clone()), Some(expected), "{info}");
    }
}

/// Packet §3: with a schema, the final text is the structured output when
/// it parses as JSON (c9 turn 1); text that does not parse, and no text,
/// are told apart for Core.
#[test]
fn structured_output_comes_from_the_final_text() {
    let structured = |case: &str, turn: &str, schema: bool| match normalized(case, turn, schema).1 {
        Some(Step::Terminal { structured, .. }) => structured,
        _ => panic!("no terminal"),
    };
    let StructuredOutput::Json(value) = structured(
        "c9_output_schema",
        "019a0000-0000-7000-8000-000000200001",
        true,
    ) else {
        panic!("JSON output");
    };
    assert_eq!(
        serde_json::from_str::<Value>(value.get()).unwrap(),
        json!({"answer": "VIA_SCHEMA_159"})
    );
    assert_eq!(
        structured(
            "c9_output_schema",
            "019a0000-0000-7000-8000-000000200002",
            true
        ),
        StructuredOutput::NotJson
    );
    assert_eq!(
        structured(
            "c3_interrupt_uncertain",
            "019a0000-0000-7000-8000-000000200001",
            true
        ),
        StructuredOutput::Missing
    );
    assert_eq!(
        structured(
            "c9_output_schema",
            "019a0000-0000-7000-8000-000000200002",
            false
        ),
        StructuredOutput::NotRequested
    );
}

/// A `turn/completed` still `inProgress` is a protocol error, not a
/// terminal.
#[test]
fn an_in_progress_terminal_is_a_protocol_error() {
    let line = r#"{"method":"turn/completed","params":{"threadId":"t","turn":{"id":"u","status":"inProgress"}}}"#;
    let Incoming::Notification(notification) = decode(line.as_bytes()).unwrap() else {
        panic!("a notification");
    };
    assert!(
        TurnNormalizer::new(false)
            .observe(&notification, Instant::now())
            .is_err()
    );
}

/// Packet §4, §8 `codex_never_ask`: each of the six no-grant bodies
/// validates against its recorded 0.157.1 response schema
/// (`tests/fixtures/codex/schema-0.157.1/`, generated by
/// `codex app-server generate-json-schema`), and grants nothing; every
/// other request, auth refresh, attestation and the legacy approvals
/// included, gets `-32601` under its exact ID.
#[test]
fn the_six_decline_bodies_validate_against_the_schemas() {
    let rows = [
        (
            "item/commandExecution/requestApproval",
            "CommandExecutionRequestApprovalResponse",
        ),
        (
            "item/fileChange/requestApproval",
            "FileChangeRequestApprovalResponse",
        ),
        (
            "item/permissions/requestApproval",
            "PermissionsRequestApprovalResponse",
        ),
        ("item/tool/requestUserInput", "ToolRequestUserInputResponse"),
        (
            "mcpServer/elicitation/request",
            "McpServerElicitationRequestResponse",
        ),
        ("item/tool/call", "DynamicToolCallResponse"),
    ];
    for (method, schema) in rows {
        assert!(DECLINES.declines(method), "{method}");
        let reply: Value =
            serde_json::from_slice(&DECLINES.reply(&RequestId::Int(41), method)).unwrap();
        assert_eq!(reply["id"], 41, "{method}");
        let body = &reply["result"];
        let path = fixtures().join(format!("schema-0.157.1/{schema}.json"));
        let schema_json: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut compiler = boon::Compiler::new();
        let mut schemas = boon::Schemas::new();
        let url = format!("file:///{schema}.json");
        compiler.add_resource(&url, schema_json).unwrap();
        let index = compiler.compile(&url, &mut schemas).unwrap();
        assert!(
            schemas.validate(body, index).is_ok(),
            "{method}: {body} does not validate"
        );
        let text = body.to_string();
        for grant in ["accept", "\"enabled\":true", "\"success\":true"] {
            assert!(!text.contains(grant), "{method} grants: {text}");
        }
    }
    assert_eq!(
        serde_json::from_slice::<Value>(
            &DECLINES.reply(&RequestId::Int(2), "item/commandExecution/requestApproval")
        )
        .unwrap()["result"],
        json!({"decision": "decline"})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(
            &DECLINES.reply(&RequestId::Int(3), "mcpServer/elicitation/request")
        )
        .unwrap()["result"],
        json!({"action": "decline", "content": null})
    );
    for method in [
        "account/chatgptAuthTokens/refresh",
        "attestation/generate",
        "applyPatchApproval",
        "execCommandApproval",
        "item/unknown/request",
    ] {
        assert!(!DECLINES.declines(method), "{method}");
        let reply: Value =
            serde_json::from_slice(&DECLINES.reply(&RequestId::Str("s-7".to_owned()), method))
                .unwrap();
        assert_eq!(
            reply,
            json!({"id": "s-7", "error": {"code": -32601, "message": "Method not supported by VIA"}})
        );
    }
}

/// `vendor.request_declined` for a declined request: the method, a
/// bounded summary, blocking.
#[test]
fn a_declined_request_is_reported_by_method() {
    let request = ServerRequest {
        id: RequestId::Int(5),
        method: "item/fileChange/requestApproval".to_owned(),
        thread_id: Some("t".to_owned()),
        turn_id: Some("u".to_owned()),
        item_id: Some("i".to_owned()),
    };
    let declined = decline(&request);
    assert_eq!(declined.vendor_method, "item/fileChange/requestApproval");
    assert!(declined.blocking);
    assert!(!declined.summary.is_empty() && declined.summary.len() <= 256);
}

/// AD12 and C2 §2: a resume turn of another adapter version is
/// `harness_unavailable` (`adapter_version`); otherwise the first
/// per-turn refusal in C1 member order, or the bound as requested.
#[test]
fn a_resume_turn_is_checked() {
    use super::CodexAdapter;
    use crate::capabilities::BoundMode;
    use crate::plan::{Bound, RefusalKind, TurnParams};

    let full = Bound {
        mode: BoundMode::Full,
        extra_write_dirs: Vec::new(),
        network: true,
    };
    let turn = TurnParams {
        bound: Some(full.clone()),
        ..TurnParams::default()
    };
    let old = CodexAdapter::judge_turn("codex-app-server", "0", &turn, None).unwrap_err();
    assert_eq!(old.kind, RefusalKind::HarnessUnavailable);
    assert_eq!(old.reason, Some("adapter_version"));
    assert_eq!(
        CodexAdapter::judge_turn("codex-app-server", "1", &turn, None)
            .unwrap()
            .effective_bound,
        Some(full)
    );
    let refused = TurnParams {
        effort: Some(String::new()),
        max_steps: Some(3),
        bound: Some(Bound {
            mode: BoundMode::ReadOnly,
            extra_write_dirs: Vec::new(),
            network: false,
        }),
        ..TurnParams::default()
    };
    assert_eq!(
        CodexAdapter::judge_turn("codex-app-server", "1", &refused, None)
            .unwrap_err()
            .kind,
        RefusalKind::InvalidParam { field: "effort" }
    );
    let bound_only = TurnParams {
        bound: refused.bound.clone(),
        ..TurnParams::default()
    };
    assert_eq!(
        CodexAdapter::judge_turn("codex-app-server", "1", &bound_only, None)
            .unwrap_err()
            .kind,
        RefusalKind::BoundUnsupported
    );
}

/// Sol r1 #16, AD18: once the session's server key has a discovered
/// catalog, a later turn's effort the session's model does not advertise is
/// refused before any receipt; a listed
/// effort, a model the catalog does not list, a turn naming no effort or
/// no model, and no catalog at all each pass to `run_turn`.
#[test]
fn a_resume_turn_judges_effort_against_the_cached_catalog() {
    use super::CodexAdapter;
    use super::normalize::DiscoveredModel;
    use crate::plan::{RefusalKind, TurnParams};

    let catalog = [DiscoveredModel {
        model: "gpt-6-luna".to_owned(),
        efforts: vec!["low".to_owned(), "medium".to_owned()],
        hidden: false,
        default: true,
    }];
    let turn = |model: Option<&str>, effort: Option<&str>| TurnParams {
        model: model.map(str::to_owned),
        effort: effort.map(str::to_owned),
        ..TurnParams::default()
    };
    let judge = |turn: &TurnParams, catalog: Option<&[DiscoveredModel]>| {
        CodexAdapter::judge_turn("codex-app-server", "1", turn, catalog)
    };
    let refused = judge(&turn(Some("gpt-6-luna"), Some("xhigh")), Some(&catalog)).unwrap_err();
    assert_eq!(refused.kind, RefusalKind::InvalidParam { field: "effort" });
    assert_eq!(refused.route, Some("codex-app-server"));
    for passes in [
        turn(Some("gpt-6-luna"), Some("low")),
        turn(Some("other"), Some("xhigh")),
        turn(Some("gpt-6-luna"), None),
        turn(None, Some("xhigh")),
    ] {
        assert!(judge(&passes, Some(&catalog)).is_ok(), "{passes:?}");
    }
    assert!(judge(&turn(Some("gpt-6-luna"), Some("xhigh")), None).is_ok());
}

/// Sol r1 #15 (C2 §2 `VendorTerminal`): the normalizer's structured
/// output as the terminal carries it. A value passes through; text that
/// is not JSON is `NotJson` (`reason: invalid`) and text over the bound is
/// `OverLimit` (`reason: validation_limit`), each with no value; no
/// schema, or an empty answer, carries neither.
#[test]
fn structured_output_is_carried_unparsed_when_it_is_no_value() {
    use super::driver::carried;
    use super::normalize::StructuredOutput;
    use crate::UnparsedOutput;

    let value = serde_json::value::to_raw_value(&json!({"answer": 1})).unwrap();
    let (json, unparsed) = carried(StructuredOutput::Json(value));
    assert_eq!(json.unwrap().get(), r#"{"answer":1}"#);
    assert_eq!(unparsed, None);
    for (output, reason) in [
        (StructuredOutput::NotJson, "invalid"),
        (StructuredOutput::OverLimit, "validation_limit"),
    ] {
        let (json, unparsed) = carried(output);
        assert!(json.is_none());
        assert_eq!(unparsed.map(UnparsedOutput::reason), Some(reason));
    }
    for output in [StructuredOutput::NotRequested, StructuredOutput::Missing] {
        let (json, unparsed) = carried(output);
        assert!(json.is_none() && unparsed.is_none());
    }
}

/// One synthetic notification, decoded as the server's would be.
fn note(line: &Value) -> Notification {
    match decode(line.to_string().as_bytes()).unwrap() {
        Incoming::Notification(notification) => notification,
        Incoming::Response(_) | Incoming::Request(_) => panic!("not a notification"),
    }
}

fn final_answer(id: &str, text: &str) -> Notification {
    note(
        &json!({"method": "item/completed", "params": {"threadId": "t", "turnId": "u",
        "item": {"type": "agentMessage", "id": id, "text": text, "phase": "final_answer"}}}),
    )
}

fn completed(status: &str) -> Notification {
    note(
        &json!({"method": "turn/completed", "params": {"threadId": "t",
        "turn": {"id": "u", "items": [], "status": status}}}),
    )
}

fn usage(cache_write: Option<i64>) -> Notification {
    let mut breakdown = json!({"totalTokens": 1, "inputTokens": 1, "cachedInputTokens": 0,
        "outputTokens": 0, "reasoningOutputTokens": 0});
    if let Some(cache_write) = cache_write {
        breakdown["cacheWriteInputTokens"] = cache_write.into();
    }
    note(
        &json!({"method": "thread/tokenUsage/updated", "params": {"threadId": "t",
        "turnId": "u", "tokenUsage": {"total": breakdown, "last": breakdown}}}),
    )
}

/// The structured output of a turn whose final answers are `texts`.
fn structured_of(schema: bool, texts: &[&str]) -> StructuredOutput {
    let mut normalizer = TurnNormalizer::new(schema);
    for (n, text) in texts.iter().enumerate() {
        normalizer
            .observe(&final_answer(&format!("m{n}"), text), Instant::now())
            .unwrap();
    }
    match normalizer
        .observe(&completed("completed"), Instant::now())
        .unwrap()
    {
        Step::Terminal { structured, .. } => structured,
        Step::Observations(_) | Step::Activity => panic!("no terminal"),
    }
}

/// Review r1 #1: with no schema nothing of the final text is retained (it
/// streams as pieces only); with one, up to 4 MiB, then `OverLimit`.
#[test]
fn final_text_retention_is_bounded() {
    let mib = "x".repeat(1024 * 1024);
    let mut plain = TurnNormalizer::new(false);
    let mut schema = TurnNormalizer::new(true);
    for n in 0..3 {
        let answer = final_answer(&format!("m{n}"), &mib);
        let Step::Observations(pieces) = plain.observe(&answer, Instant::now()).unwrap() else {
            panic!("final text pieces");
        };
        assert!(!pieces.is_empty());
        schema.observe(&answer, Instant::now()).unwrap();
    }
    assert_eq!(plain.retained(), 0);
    assert_eq!(schema.retained(), 3 * mib.len());
    assert_eq!(
        structured_of(true, &[&mib, &mib, &mib, &mib]),
        StructuredOutput::NotJson,
        "4 MiB exactly is kept"
    );
    let mut over = TurnNormalizer::new(true);
    for n in 0..5 {
        over.observe(&final_answer(&format!("m{n}"), &mib), Instant::now())
            .unwrap();
    }
    assert_eq!(over.retained(), 0, "the accumulation is dropped");
    assert_eq!(
        structured_of(true, &[&mib, &mib, &mib, &mib, "x"]),
        StructuredOutput::OverLimit
    );
    let deep = format!("{}{}", "[".repeat(65), "]".repeat(65));
    assert_eq!(structured_of(true, &[&deep]), StructuredOutput::OverLimit);
}

/// Review r1 #6: an empty assembled answer is `Missing`, not `NotJson`.
#[test]
fn an_empty_final_answer_is_missing() {
    assert_eq!(structured_of(true, &[""]), StructuredOutput::Missing);
    assert_eq!(structured_of(true, &["", ""]), StructuredOutput::Missing);
    assert_eq!(
        structured_of(true, &["", "{}"]),
        StructuredOutput::Json(serde_json::value::RawValue::from_string("{}".to_owned()).unwrap())
    );
}

/// Review r1 #5: usage accumulation never wraps or panics: an overflow is
/// a normalizer failure.
#[test]
fn usage_overflow_is_an_error() {
    let mut normalizer = TurnNormalizer::new(false);
    normalizer
        .observe(&usage(Some(i64::MAX)), Instant::now())
        .unwrap();
    normalizer
        .observe(&usage(Some(i64::MAX)), Instant::now())
        .unwrap();
    assert!(
        normalizer
            .observe(&usage(Some(i64::MAX)), Instant::now())
            .is_err()
    );
}

/// Review r1 #9: an unavailable cache-write count is never reported as 0:
/// the aggregate is omitted when any sample lacks it.
#[test]
fn an_unavailable_cache_write_count_is_omitted() {
    let vendor = |samples: &[Option<i64>]| {
        let mut normalizer = TurnNormalizer::new(false);
        for sample in samples {
            normalizer.observe(&usage(*sample), Instant::now()).unwrap();
        }
        let Step::Terminal { terminal, .. } = normalizer
            .observe(&completed("completed"), Instant::now())
            .unwrap()
        else {
            panic!("a terminal");
        };
        serde_json::from_str::<Value>(terminal.vendor.unwrap().get()).unwrap()
    };
    assert_eq!(vendor(&[Some(2), Some(3)])["cacheWriteInputTokens"], 5);
    for samples in [&[Some(2), None][..], &[None, Some(3)], &[None]] {
        let vendor = vendor(samples);
        assert!(vendor.get("cacheWriteInputTokens").is_none(), "{vendor}");
        assert_eq!(
            vendor["total"]
                .get("cacheWriteInputTokens")
                .is_some_and(Value::is_number),
            samples.last().unwrap().is_some()
        );
    }
}

fn tool_item(kind: &str, id: &str, status: &str) -> Value {
    match kind {
        "commandExecution" => json!({"type": kind, "id": id, "command": "rm -rf build",
            "cwd": "/w", "commandActions": [], "status": status}),
        "fileChange" => json!({"type": kind, "id": id, "status": status,
            "changes": [{"path": "/w/a.txt", "kind": {"type": "add"}, "diff": ""}]}),
        "sleep" => json!({"type": kind, "id": id, "durationMs": 75_000}),
        _ => panic!("{kind}"),
    }
}

fn item(method: &str, item: &Value) -> Notification {
    note(&json!({"method": method, "params": {"threadId": "t", "turnId": "u", "item": item}}))
}

fn denials(observations: &[Observation]) -> Vec<(DenialKind, String, String)> {
    observations
        .iter()
        .filter_map(|observation| {
            if let Observation::ActionDenied(denial) = observation {
                Some((denial.kind, denial.target.clone(), denial.reason.clone()))
            } else {
                None
            }
        })
        .collect()
}

fn observed(normalizer: &mut TurnNormalizer, notification: &Notification) -> Vec<Observation> {
    match normalizer.observe(notification, Instant::now()).unwrap() {
        Step::Observations(observations) => observations,
        Step::Terminal { .. } | Step::Activity => Vec::new(),
    }
}

/// Review r1 #7: a command or file-change item the vendor declined is
/// `action.denied` (C1 Q9, as the Claude adapter maps it), once per item;
/// one VIA's own decline caused is not.
#[test]
fn a_declined_item_is_a_denial() {
    let reason = "denied by the vendor's permission policy".to_owned();
    let mut normalizer = TurnNormalizer::new(false);
    let command = item(
        "item/completed",
        &tool_item("commandExecution", "c1", "declined"),
    );
    let observations = observed(&mut normalizer, &command);
    assert_eq!(
        denials(&observations),
        [(
            DenialKind::Command,
            "rm -rf build".to_owned(),
            reason.clone()
        )]
    );
    assert!(observations.iter().any(|o| matches!(
        o, Observation::Progress(marks) if marks.tools_ended == ["c1".to_owned()]
    )));
    assert!(
        denials(&observed(&mut normalizer, &command)).is_empty(),
        "once per item"
    );
    let file = item("item/completed", &tool_item("fileChange", "f1", "declined"));
    assert_eq!(
        denials(&observed(&mut normalizer, &file)),
        [(DenialKind::FileWrite, "/w/a.txt".to_owned(), reason)]
    );
    let failed = item(
        "item/completed",
        &tool_item("commandExecution", "c2", "failed"),
    );
    assert!(denials(&observed(&mut normalizer, &failed)).is_empty());

    let mut ours = TurnNormalizer::new(false);
    ours.note_decline(&ServerRequest {
        id: RequestId::Int(4),
        method: "item/commandExecution/requestApproval".to_owned(),
        thread_id: Some("t".to_owned()),
        turn_id: Some("u".to_owned()),
        item_id: Some("c1".to_owned()),
    })
    .unwrap();
    assert!(
        denials(&observed(&mut ours, &command)).is_empty(),
        "VIA's own decline is reported as vendor.request_declined only"
    );
}

/// Review r1 #8: a `sleep` item is a tool: started, it is open, and an
/// interrupted terminal with it still open is not quiescent.
#[test]
fn an_open_sleep_is_an_open_tool() {
    let mut normalizer = TurnNormalizer::new(false);
    let started = observed(
        &mut normalizer,
        &item("item/started", &tool_item("sleep", "s1", "")),
    );
    assert!(started.iter().any(|o| matches!(
        o,
        Observation::Progress(marks)
            if marks.tools_started == [("s1".to_owned(), "sleep".to_owned())]
    )));
    let Step::Terminal { terminal, .. } = normalizer
        .observe(&completed("interrupted"), Instant::now())
        .unwrap()
    else {
        panic!("a terminal");
    };
    assert_eq!(terminal.status, VendorTerminalStatus::Interrupted);
    assert!(normalizer.tools_open(), "the sleep is still open");
    let mut closed = TurnNormalizer::new(false);
    observed(
        &mut closed,
        &item("item/started", &tool_item("sleep", "s1", "")),
    );
    observed(
        &mut closed,
        &item("item/completed", &tool_item("sleep", "s1", "")),
    );
    assert!(!closed.tools_open());
}

/// Review r2 #2: each ID set admits 1,024 IDs and all share the 256 KiB
/// metadata budget; the first ID that cannot be admitted is an explicit
/// overflow, and the normalizer takes nothing after it.
#[test]
fn id_tracking_overflows_explicitly() {
    let mut denied = TurnNormalizer::new(false);
    for n in 0..1024 {
        let declined = item(
            "item/completed",
            &tool_item("commandExecution", &format!("c{n}"), "declined"),
        );
        assert_eq!(denials(&observed(&mut denied, &declined)).len(), 1);
    }
    let next = item(
        "item/completed",
        &tool_item("commandExecution", "c1024", "declined"),
    );
    assert_eq!(
        denied.observe(&next, Instant::now()).unwrap_err(),
        NormalizeError::Overflow
    );
    assert_eq!(
        denied
            .observe(&completed("completed"), Instant::now())
            .unwrap_err(),
        NormalizeError::Overflow,
        "overflowed for good"
    );

    let mut declines = TurnNormalizer::new(false);
    let request = |n: usize| ServerRequest {
        id: RequestId::Int(1),
        method: "item/commandExecution/requestApproval".to_owned(),
        thread_id: None,
        turn_id: None,
        item_id: Some(format!("c{n}")),
    };
    for n in 0..1024 {
        declines.note_decline(&request(n)).unwrap();
    }
    assert_eq!(
        declines.note_decline(&request(1024)).unwrap_err(),
        NormalizeError::Overflow
    );

    let mut bytes = TurnNormalizer::new(false);
    let long_id = |n: usize| format!("{n:0>1024}");
    for n in 0..256 {
        bytes
            .observe(
                &item("item/started", &tool_item("sleep", &long_id(n), "")),
                Instant::now(),
            )
            .unwrap();
    }
    assert_eq!(
        bytes
            .observe(
                &item("item/started", &tool_item("sleep", &long_id(256), "")),
                Instant::now()
            )
            .unwrap_err(),
        NormalizeError::Overflow
    );
}

/// Ruling Q1: a named model is taken as given; with none named, the plan
/// resolves the discovered catalog's default, and before any discovery
/// (or with a catalog naming no default) nothing, which is
/// `unknown_model`.
#[test]
fn the_plan_model_is_the_named_or_the_discovered_default() {
    use super::normalize::DiscoveredModel;
    use super::resolved_model;

    let model = |name: &str, default: bool| DiscoveredModel {
        model: name.to_owned(),
        efforts: vec!["low".to_owned()],
        hidden: false,
        default,
    };
    let catalog = [model("gpt-6.1-sol", false), model("gpt-6-sol", true)];
    assert_eq!(
        resolved_model(Some("named"), None).as_deref(),
        Some("named")
    );
    assert_eq!(
        resolved_model(Some("named"), Some(&catalog)).as_deref(),
        Some("named")
    );
    assert_eq!(resolved_model(None, None), None);
    assert_eq!(resolved_model(Some(""), None), None);
    assert_eq!(
        resolved_model(None, Some(&catalog)).as_deref(),
        Some("gpt-6-sol")
    );
    assert_eq!(resolved_model(None, Some(&catalog[..1])), None);
}

/// X0 item 13.2: a generation's abnormal-end handler, whether or not a
/// turn runs, latches the driver's failure at once (an owned task failed)
/// and, with a registration, installs the loss naming the session's latest
/// turn, `omitted` unknown; a second signal merges into it. Without a
/// registration it installs none.
#[test]
fn abnormal_handler_latches_and_records_loss() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use via_routes::codex::AbnormalEnd;

    use super::delivery::{Losses, ObservationLoss, UNKNOWN};
    use super::driver::abnormal_handler;
    use crate::{DriverFailure, DriverHealth, TurnNumber};

    let turn = TurnNumber::try_from(4).unwrap();
    let losses = Arc::new(Mutex::new(Losses {
        record: None,
        latest: Some(turn),
    }));
    let health = Arc::new(tokio::sync::watch::Sender::new(DriverHealth::Open));
    let registered = Arc::new(AtomicBool::new(true));
    let handler = abnormal_handler(
        Arc::clone(&losses),
        Arc::clone(&health),
        Arc::clone(&registered),
        2,
    );
    handler(AbnormalEnd { first_unqueued: 9 });
    assert_eq!(
        *health.borrow(),
        DriverHealth::Failed {
            first_cause: DriverFailure::OwnedTask
        }
    );
    handler(AbnormalEnd { first_unqueued: 12 });
    assert_eq!(
        losses.lock().unwrap().record,
        Some(ObservationLoss {
            trigger: turn,
            generation: 2,
            first_unqueued: 9,
            omitted: UNKNOWN,
        })
    );

    let bare = Arc::new(Mutex::new(Losses {
        record: None,
        latest: Some(turn),
    }));
    let idle = Arc::new(tokio::sync::watch::Sender::new(DriverHealth::Open));
    registered.store(false, Ordering::Release);
    abnormal_handler(Arc::clone(&bare), Arc::clone(&idle), registered, 2)(AbnormalEnd {
        first_unqueued: 1,
    });
    assert!(bare.lock().unwrap().record.is_none());
    assert!(matches!(*idle.borrow(), DriverHealth::Failed { .. }));
}
