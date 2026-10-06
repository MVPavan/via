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
use crate::config::{BootstrapEnv, CodexSettings};
use crate::observation::{
    ClassHint, DenialKind, Observation, ProgressMarks, StopReason, UsageSample,
};
use crate::plan::{Inherit, InheritState, VersionStatus};
use crate::{TurnNumber, VendorTerminalStatus};

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
    for mut notification in turn_notifications(case, turn_id) {
        match normalizer
            .observe(&mut notification, Instant::now())
            .unwrap()
        {
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
    inherit(state, InheritState::Off)
}

fn inherit(hooks: InheritState, mcp_servers: InheritState) -> Inherit {
    serde_json::from_value(
        json!({"hooks": hooks, "mcp_servers": mcp_servers, "plugins": "on",
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

/// via-7r9: every server VIA starts disables Codex's memories feature,
/// whatever the requested inheritance. Codex 0.160.0 otherwise ran memory
/// extraction and a consolidation agent thread with full access that
/// edited the user's `~/.codex/memories`, outside the caller's turn.
#[test]
fn every_server_disables_memories() {
    for state in [InheritState::Off, InheritState::On] {
        let recipe = ServerRecipe::new(
            Path::new("/bin/codex"),
            (hooks(state), CodexSettings::default()),
            &env(),
            Path::new("/state/vendor/codex"),
        );
        assert!(
            recipe
                .args
                .windows(2)
                .any(|pair| pair == ["--disable", "memories"]),
            "hooks {state:?}: {:?}",
            recipe.args
        );
    }
}

/// Owner 2026-10-05: `harnesses.codex.memories` true omits `--disable memories`, so
/// Codex's own default applies; the argv is in the server key, so servers
/// under the two settings never share a key.
#[test]
fn codex_memories_true_keeps_the_vendor_default() {
    let recipe = |memories| {
        ServerRecipe::new(
            Path::new("/bin/codex"),
            (hooks(InheritState::Off), CodexSettings { memories }),
            &env(),
            Path::new("/state/vendor/codex"),
        )
    };
    assert_eq!(recipe(true).args, ["app-server", "--disable", "hooks"]);
    assert!(
        recipe(false)
            .args
            .windows(2)
            .any(|pair| pair == ["--disable", "memories"])
    );
    assert_ne!(
        recipe(true).config_hash("0.1.0"),
        recipe(false).config_hash("0.1.0")
    );
}

/// C2 §6.3, packet §4: a session's raw arguments follow VIA's switches on
/// the server's argv, so they are in the server key: equal lists share a
/// server, different lists (or none beside some) never do. The launch
/// request counts them: a list that would not fit Host's cap is refused.
#[test]
fn vendor_args_enter_the_server_argv_and_key() {
    let recipe = |list: &[&str]| {
        let args = crate::VendorArgs::try_from(
            list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
        )
        .unwrap();
        ServerRecipe::new(
            Path::new("/bin/codex"),
            (hooks(InheritState::On), CodexSettings::default()),
            &env(),
            Path::new("/state/vendor/codex"),
        )
        .with_vendor_args(&args)
    };
    let with = recipe(&["--strict-config", "-c", "model_verbosity=low"]);
    assert_eq!(
        with.args,
        [
            "app-server",
            "--disable",
            "memories",
            "--strict-config",
            "-c",
            "model_verbosity=low"
        ]
    );
    let key = |list: &[&str]| recipe(list).config_hash("1");
    assert_eq!(
        key(&["--strict-config", "-c", "model_verbosity=low"]),
        with.config_hash("1")
    );
    assert_ne!(key(&["--strict-config"]), key(&[]));
    assert_ne!(
        key(&["--strict-config"]),
        key(&["--analytics-default-enabled"])
    );
    assert_ne!(key(&["-c", "a=1"]), key(&["-ca=1"]));
    assert!(with.fits());
    assert!(!recipe(&[&format!("--code-mode-host={}", "z".repeat(16 * 1024 - 17))]).fits());
}

/// Owner 2026-10-05: for the first release Codex disables nothing but
/// memories, so no request adds `--disable apps`: the user's MCP servers
/// and Codex's built-in apps server load as configured.
#[test]
fn no_request_disables_the_apps_server() {
    use InheritState::{Off, On};
    for (hooks, mcp_servers) in [(On, On), (On, Off), (Off, On), (Off, Off)] {
        let recipe = ServerRecipe::new(
            Path::new("/bin/codex"),
            (inherit(hooks, mcp_servers), CodexSettings::default()),
            &env(),
            Path::new("/state/vendor/codex"),
        );
        assert!(
            !recipe.args.iter().any(|arg| arg == "apps"),
            "{hooks:?} {mcp_servers:?}: {:?}",
            recipe.args
        );
    }
}

/// Owner 2026-10-06 (C2 §6.2): with no switch, a category is `on` where
/// recorded live evidence shows Codex loads the user's configuration for
/// it (packet §4): hooks (the owner's hooks ran), MCP servers (the user's
/// servers and `codex_apps` started) and instruction files
/// (`instructionSources` listed the loaded AGENTS.md), and since
/// `via-5lr.3.4`'s run-5 plugins, skills and agents. Hooks off is the verified
/// `--disable hooks`; an off VIA cannot apply is `unknown`, never a
/// claimed suppression.
#[test]
fn codex_categories_follow_the_recorded_evidence() {
    use crate::plan::Category;
    use InheritState::{Off, On, Unknown};
    let harness = crate::Harness::Vendor(
        crate::harness::HARNESSES
            .iter()
            .find(|row| row.name == super::HARNESS)
            .unwrap(),
    );
    let default = crate::config::AdapterConfig::load(BootstrapEnv::default(), None)
        .unwrap()
        .inherit(harness);
    for category in Category::ALL {
        assert_eq!(default.get(category), On, "{category:?}");
    }
    let planned = |requested| {
        let (plan, warning) = crate::plan::effective_inherit(&super::plan::categories(), requested);
        let listed: Vec<String> = warning
            .and_then(|warning| warning.data)
            .and_then(|data| data["categories"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|entry| entry["category"].as_str().unwrap_or_default().to_owned())
            .collect();
        (plan.effective, listed)
    };
    let states = |effective: Inherit| Category::ALL.map(|category| effective.get(category));
    let (effective, listed) = planned(default);
    assert_eq!(
        states(effective),
        [On; 6],
        "hooks, MCP servers, plugins, skills, agents, instruction files"
    );
    assert!(listed.is_empty(), "{listed:?}");
    let off: Inherit = serde_json::from_value(json!({"hooks": "off", "mcp_servers": "off",
        "plugins": "off", "skills": "off", "agents": "off", "instruction_files": "off"}))
    .unwrap();
    let (effective, listed) = planned(off);
    assert_eq!(
        states(effective),
        [Off, Unknown, Unknown, Unknown, Unknown, Unknown]
    );
    assert_eq!(
        listed,
        [
            "mcp_servers",
            "plugins",
            "skills",
            "agents",
            "instruction_files"
        ]
    );
}

/// Packet §4, Q6: the server's argv disables hooks when they are off, its
/// environment is exactly the allow-list plus the supplied
/// `CODEX_SQLITE_HOME`, and it runs in that directory.
#[test]
fn the_server_recipe_is_the_allow_list() {
    let home = Path::new("/state/vendor/codex");
    let recipe = ServerRecipe::new(
        Path::new("/bin/codex"),
        (hooks(InheritState::Off), CodexSettings::default()),
        &env(),
        home,
    );
    assert_eq!(recipe.program, Path::new("/bin/codex"));
    assert_eq!(
        recipe.args,
        ["app-server", "--disable", "memories", "--disable", "hooks"]
    );
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
        (hooks(InheritState::On), CodexSettings::default()),
        &BootstrapEnv::from_vars([("PATH", "/usr/bin")]),
        home,
    );
    assert_eq!(on.args, ["app-server", "--disable", "memories"]);
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
        ServerRecipe::new(
            binary,
            (hooks(hooks_state), CodexSettings::default()),
            &env(),
            Path::new(home),
        )
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
    assert_eq!(version_status("0.160.0"), VersionStatus::Tested);
    assert_eq!(version_status("0.161.0"), VersionStatus::Untested);
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
        let Incoming::Notification(mut notification) = decode(line.to_string().as_bytes()).unwrap()
        else {
            panic!("a notification");
        };
        match TurnNormalizer::new(false)
            .observe(&mut notification, Instant::now())
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
    let Incoming::Notification(mut notification) = decode(line.as_bytes()).unwrap() else {
        panic!("a notification");
    };
    assert!(
        TurnNormalizer::new(false)
            .observe(&mut notification, Instant::now())
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
            network: true,
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
    let offline = Bound {
        mode: BoundMode::ReadOnly,
        extra_write_dirs: Vec::new(),
        network: false,
    };
    let read_only = TurnParams {
        bound: Some(offline.clone()),
        ..TurnParams::default()
    };
    assert_eq!(
        CodexAdapter::judge_turn("codex-app-server", "1", &read_only, None)
            .unwrap()
            .effective_bound,
        Some(offline)
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
            .observe(&mut final_answer(&format!("m{n}"), text), Instant::now())
            .unwrap();
    }
    match normalizer
        .observe(&mut completed("completed"), Instant::now())
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
        let answer = || final_answer(&format!("m{n}"), &mib);
        let Step::Observations(pieces) = plain.observe(&mut answer(), Instant::now()).unwrap()
        else {
            panic!("final text pieces");
        };
        assert!(!pieces.is_empty());
        schema.observe(&mut answer(), Instant::now()).unwrap();
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
        over.observe(&mut final_answer(&format!("m{n}"), &mib), Instant::now())
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
        .observe(&mut usage(Some(i64::MAX)), Instant::now())
        .unwrap();
    normalizer
        .observe(&mut usage(Some(i64::MAX)), Instant::now())
        .unwrap();
    assert!(
        normalizer
            .observe(&mut usage(Some(i64::MAX)), Instant::now())
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
            normalizer
                .observe(&mut usage(*sample), Instant::now())
                .unwrap();
        }
        let Step::Terminal { terminal, .. } = normalizer
            .observe(&mut completed("completed"), Instant::now())
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

fn observed(normalizer: &mut TurnNormalizer, notification: &mut Notification) -> Vec<Observation> {
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
    let mut command = item(
        "item/completed",
        &tool_item("commandExecution", "c1", "declined"),
    );
    let observations = observed(&mut normalizer, &mut command);
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
        denials(&observed(&mut normalizer, &mut command)).is_empty(),
        "once per item"
    );
    let mut file = item("item/completed", &tool_item("fileChange", "f1", "declined"));
    assert_eq!(
        denials(&observed(&mut normalizer, &mut file)),
        [(DenialKind::FileWrite, "/w/a.txt".to_owned(), reason)]
    );
    let mut failed = item(
        "item/completed",
        &tool_item("commandExecution", "c2", "failed"),
    );
    assert!(denials(&observed(&mut normalizer, &mut failed)).is_empty());

    let mut ours = TurnNormalizer::new(false);
    ours.ledger()
        .note_decline(
            first_turn(),
            &ServerRequest {
                id: RequestId::Int(4),
                method: "item/commandExecution/requestApproval".to_owned(),
                thread_id: Some("t".to_owned()),
                turn_id: Some("u".to_owned()),
                item_id: Some("c1".to_owned()),
            },
        )
        .unwrap();
    assert!(
        denials(&observed(&mut ours, &mut command)).is_empty(),
        "VIA's own decline is reported as vendor.request_declined only"
    );
}

/// The turn [`TurnNormalizer::new`] normalizes.
fn first_turn() -> TurnNumber {
    TurnNumber::try_from(1).unwrap()
}

/// Review r1 #8: a `sleep` item is a tool: started, it is open (in the
/// registration's ledger, x.3.2 X3 §6.3), and an interrupted terminal
/// with it still open is not quiescent.
#[test]
fn an_open_sleep_is_an_open_tool() {
    let mut normalizer = TurnNormalizer::new(false);
    let mut sleep = item("item/started", &tool_item("sleep", "s1", ""));
    normalizer.ledger().track(first_turn(), &sleep).unwrap();
    let started = observed(&mut normalizer, &mut sleep);
    assert!(started.iter().any(|o| matches!(
        o,
        Observation::Progress(marks)
            if marks.tools_started == [("s1".to_owned(), "sleep".to_owned())]
    )));
    let Step::Terminal { terminal, .. } = normalizer
        .observe(&mut completed("interrupted"), Instant::now())
        .unwrap()
    else {
        panic!("a terminal");
    };
    assert_eq!(terminal.status, VendorTerminalStatus::Interrupted);
    assert!(
        normalizer.ledger().tools_open(first_turn()),
        "the sleep is still open"
    );
    let closed = TurnNormalizer::new(false);
    closed.ledger().track(first_turn(), &sleep).unwrap();
    closed
        .ledger()
        .track(
            first_turn(),
            &item("item/completed", &tool_item("sleep", "s1", "")),
        )
        .unwrap();
    assert!(!closed.ledger().tools_open(first_turn()));
}

/// Review r2 #2: each ID set admits 1,024 IDs and all share the 256 KiB
/// metadata budget; the first ID that cannot be admitted is an explicit
/// overflow, and the normalizer takes nothing after it.
#[test]
fn id_tracking_overflows_explicitly() {
    let mut denied = TurnNormalizer::new(false);
    for n in 0..1024 {
        let mut declined = item(
            "item/completed",
            &tool_item("commandExecution", &format!("c{n}"), "declined"),
        );
        assert_eq!(denials(&observed(&mut denied, &mut declined)).len(), 1);
    }
    let mut next = item(
        "item/completed",
        &tool_item("commandExecution", "c1024", "declined"),
    );
    assert_eq!(
        denied.observe(&mut next, Instant::now()).unwrap_err(),
        NormalizeError::Overflow
    );
    assert_eq!(
        denied
            .observe(&mut completed("completed"), Instant::now())
            .unwrap_err(),
        NormalizeError::Overflow,
        "overflowed for good"
    );

    let declines = TurnNormalizer::new(false);
    let request = |n: usize| ServerRequest {
        id: RequestId::Int(1),
        method: "item/commandExecution/requestApproval".to_owned(),
        thread_id: None,
        turn_id: None,
        item_id: Some(format!("c{n}")),
    };
    for n in 0..1024 {
        declines
            .ledger()
            .note_decline(first_turn(), &request(n))
            .unwrap();
    }
    assert_eq!(
        declines
            .ledger()
            .note_decline(first_turn(), &request(1024))
            .unwrap_err(),
        NormalizeError::Overflow
    );

    let bytes = TurnNormalizer::new(false);
    let long_id = |n: usize| format!("{n:0>1024}");
    for n in 0..256 {
        bytes
            .ledger()
            .track(
                first_turn(),
                &item("item/started", &tool_item("sleep", &long_id(n), "")),
            )
            .unwrap();
    }
    assert_eq!(
        bytes
            .ledger()
            .track(
                first_turn(),
                &item("item/started", &tool_item("sleep", &long_id(256), "")),
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

    use super::delivery::{Losses, UNKNOWN};
    use super::driver::abnormal_handler;
    use crate::ObservationLoss;
    use crate::{DriverFailure, DriverHealth, TurnNumber};

    let turn = TurnNumber::try_from(4).unwrap();
    let losses = Arc::new(Mutex::new(Losses {
        record: None,
        latest: Some(turn),
        ..Losses::default()
    }));
    let health = Arc::new(tokio::sync::watch::Sender::new(DriverHealth::Open));
    let registered = Arc::new(AtomicBool::new(true));
    let handler = abnormal_handler(
        Arc::clone(&losses),
        Arc::clone(&health),
        Arc::clone(&registered),
        2,
    );
    handler(AbnormalEnd {
        first_unqueued: 9,
        owner: None,
    });
    assert_eq!(
        *health.borrow(),
        DriverHealth::Failed {
            first_cause: DriverFailure::OwnedTask
        }
    );
    handler(AbnormalEnd {
        first_unqueued: 12,
        owner: None,
    });
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
        ..Losses::default()
    }));
    let idle = Arc::new(tokio::sync::watch::Sender::new(DriverHealth::Open));
    registered.store(false, Ordering::Release);
    abnormal_handler(Arc::clone(&bare), Arc::clone(&idle), registered, 2)(AbnormalEnd {
        first_unqueued: 1,
        owner: None,
    });
    assert!(bare.lock().unwrap().record.is_none());
    assert!(matches!(*idle.borrow(), DriverHealth::Failed { .. }));
}

/// x.3.2 X3 fix r2 #1: a generation's lane-overflow handler, called by
/// the connection task as the overflowed lane drops a message, latches
/// the driver's failure at once (`overflow`, naming the session's latest
/// turn), whether or not a turn runs, and records the loss, `omitted`
/// unknown; a later drop merges into it.
#[test]
fn overflow_handler_latches_at_once() {
    use std::sync::{Arc, Mutex};

    use via_routes::codex::AbnormalEnd;

    use super::delivery::{Losses, UNKNOWN};
    use super::driver::overflow_handler;
    use crate::ObservationLoss;
    use crate::{DriverFailure, DriverHealth, RouteError, TurnNumber};

    let turn = TurnNumber::try_from(3).unwrap();
    let losses = Arc::new(Mutex::new(Losses {
        record: None,
        latest: Some(turn),
        ..Losses::default()
    }));
    let health = Arc::new(tokio::sync::watch::Sender::new(DriverHealth::Open));
    let handler = overflow_handler(Arc::clone(&losses), Arc::clone(&health), 5);
    handler(AbnormalEnd {
        first_unqueued: 17,
        owner: None,
    });
    handler(AbnormalEnd {
        first_unqueued: 17,
        owner: None,
    });
    assert_eq!(
        *health.borrow(),
        DriverHealth::Failed {
            first_cause: DriverFailure::Route(RouteError::Overflow { turn })
        }
    );
    assert_eq!(
        losses.lock().unwrap().record,
        Some(ObservationLoss {
            trigger: turn,
            generation: 5,
            first_unqueued: 17,
            omitted: UNKNOWN,
        })
    );
}

/// Critical review x5: a dropped item routed under an earlier turn (a
/// predecessor's late message) names that turn as the record's trigger;
/// the driver still fails the latest turn `overflow`, and a later drop
/// keeps the trigger.
#[test]
fn overflow_handler_names_the_dropped_items_turn() {
    use std::sync::{Arc, Mutex};

    use via_routes::codex::AbnormalEnd;

    use super::delivery::{Losses, UNKNOWN};
    use super::driver::overflow_handler;
    use crate::ObservationLoss;
    use crate::{DriverFailure, DriverHealth, RouteError, TurnNumber};

    let earlier = TurnNumber::try_from(1).unwrap();
    let latest = TurnNumber::try_from(2).unwrap();
    let losses = Arc::new(Mutex::new(Losses {
        latest: Some(latest),
        ..Losses::default()
    }));
    let health = Arc::new(tokio::sync::watch::Sender::new(DriverHealth::Open));
    let handler = overflow_handler(Arc::clone(&losses), Arc::clone(&health), 1);
    handler(AbnormalEnd {
        first_unqueued: 44,
        owner: Some(earlier),
    });
    handler(AbnormalEnd {
        first_unqueued: 45,
        owner: Some(latest),
    });
    assert_eq!(
        *health.borrow(),
        DriverHealth::Failed {
            first_cause: DriverFailure::Route(RouteError::Overflow { turn: latest })
        }
    );
    assert_eq!(
        losses.lock().unwrap().record,
        Some(ObservationLoss {
            trigger: earlier,
            generation: 1,
            first_unqueued: 44,
            omitted: UNKNOWN,
        })
    );
}

/// X0 §13.2 (x.3.2 X3 fix r2 #5): when a turn's wait resumes with the
/// daemon force and the delivery's decision both ready, the force wins,
/// so a retained terminal never replaces its disposition; the decision
/// wins over the lane's overflow.
#[test]
fn force_wins_over_a_ready_decision() {
    use super::driver::{Cut, ready_cut};

    assert_eq!(ready_cut(true, true, true), Some(Cut::Forced));
    assert_eq!(ready_cut(true, true, false), Some(Cut::Forced));
    assert_eq!(ready_cut(false, true, true), Some(Cut::Decided));
    assert_eq!(ready_cut(false, false, true), Some(Cut::Overflow));
    assert_eq!(ready_cut(false, false, false), None);
}

/// x.3.2 X3 S4 (F2, F3): admission waits for its credit beside every
/// cutoff. With the session's budget full, a turn's credit waits; its
/// stop, the daemon force, its wall, the driver's failure, its
/// registration's failure (with the generation's cause) and its
/// retirement each end the wait at once with nothing reserved (the cap
/// back at its baseline) and that cause, before any job exists. Once the
/// budget has room the credit is taken; a full cap is exhaustion, never a
/// wait.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one case per cutoff arm, each beside its expected cause"
)]
async fn admission_credit_waits_beside_its_cutoffs() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;
    use via_routes::codex::Lane;

    use super::delivery::{LossRecord, Losses, Registration};
    use super::driver::{Orders, Uncredited, credit, unsent_cause};
    use crate::driver::latch;
    use crate::observation::{SessionCap, observation_channel};
    use crate::{
        Deadline, DriverFailure, DriverHealth, RouteError, StopCause, StopOrder, TurnNumber,
    };

    #[derive(Clone, Copy, Debug)]
    enum Arm {
        Stop,
        Force,
        Wall,
        Health,
        Failure,
        Retirement,
    }
    let turn = TurnNumber::try_from(2).unwrap();
    let protocol = DriverFailure::Route(RouteError::Protocol {
        turn: TurnNumber::try_from(1).unwrap(),
        detail: "a message of the session's thread did not decode",
    });
    let arms = [
        Arm::Stop,
        Arm::Force,
        Arm::Wall,
        Arm::Health,
        Arm::Failure,
        Arm::Retirement,
    ];
    for arm in arms {
        let (sink, _received) = observation_channel();
        let cap = SessionCap::new(&sink);
        let registration = Registration::new(4, cap.clone());
        let lane = Lane::default();
        let loss = LossRecord {
            losses: Arc::new(Mutex::new(Losses::default())),
            generation: 1,
        };
        let full = cap.fill_budget().unwrap();
        let soon = Instant::now() + Duration::from_millis(100);
        let (stop, stop_watch) = watch::channel(None);
        let (_close, close) = watch::channel(None);
        let (force_set, mut force) = watch::channel(None);
        let health = watch::Sender::new(DriverHealth::Open);
        let mut orders = Orders {
            stop: stop_watch,
            close,
            wall: Deadline::at(if matches!(arm, Arm::Wall) {
                soon
            } else {
                soon + Duration::from_secs(60)
            }),
            cancel: CancellationToken::new(),
            tool_grace: Duration::from_secs(60),
            first: None,
        };
        let cutoff = async {
            tokio::time::sleep_until(soon).await;
            match arm {
                Arm::Stop => {
                    stop.send_replace(Some(StopOrder {
                        cause: StopCause::Close,
                        requested_at: String::new(),
                        attached: soon,
                        force_at: Deadline::at(soon),
                        close_by: Deadline::at(soon + Duration::from_secs(3)),
                    }));
                }
                Arm::Force => {
                    force_set.send_replace(Some(soon));
                }
                Arm::Wall => {}
                Arm::Health => latch(&health, DriverFailure::TurnAbandoned),
                Arm::Failure => registration.fail(&protocol, (&health, &lane, &loss)),
                Arm::Retirement => registration.retire((&lane, &loss), || {}),
            }
            std::future::pending::<()>().await;
        };
        let waited = tokio::select! {
            waited = tokio::time::timeout(
                Duration::from_secs(5),
                credit(&cap, (&mut orders, &mut force, &health), Some(&registration)),
            ) => waited.unwrap(),
            () = cutoff => unreachable!(),
        };
        let expected = match arm {
            Arm::Stop | Arm::Wall => Uncredited::Ordered,
            Arm::Force => Uncredited::Forced,
            Arm::Health => Uncredited::Failed,
            Arm::Failure => Uncredited::Gone(Some(protocol.clone())),
            Arm::Retirement => Uncredited::Gone(None),
        };
        assert_eq!(waited.err(), Some(expected), "{arm:?}");
        assert!(Instant::now() < soon + Duration::from_secs(1), "{arm:?}");
        assert_eq!(cap.held(), (0, 0), "{arm:?}: nothing reserved");
        let cause = unsent_cause(&orders, &force, turn);
        match arm {
            Arm::Stop => assert_eq!(cause, RouteError::Stopped { turn }),
            Arm::Force => assert_eq!(cause, RouteError::ForceStopped { turn }),
            Arm::Wall => assert_eq!(cause, RouteError::Deadline { turn }),
            Arm::Health | Arm::Failure | Arm::Retirement => {}
        }
        drop(full);
        let taken = credit(&cap, (&mut orders, &mut force, &health), None).await;
        assert!(taken.is_ok(), "{arm:?}: room is taken at once");
        assert_eq!(cap.held().0, 1);
    }
}

/// x.3.2 X3 §4.2 step 1 (F2): a full cap is exhaustion, never a wait.
#[tokio::test]
async fn a_full_cap_is_credit_exhaustion() {
    use std::time::Duration;

    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    use super::driver::{Orders, Uncredited, credit};
    use crate::observation::{SessionCap, observation_channel};
    use crate::{Deadline, DriverHealth};

    let (sink, _received) = observation_channel();
    let cap = SessionCap::new(&sink);
    let held: Vec<_> = std::iter::from_fn(|| cap.slot(0)).collect();
    assert_eq!(held.len(), 1024);
    let (_stop, stop) = watch::channel(None);
    let (_close, close) = watch::channel(None);
    let (_force, mut force) = watch::channel(None);
    let mut orders = Orders {
        stop,
        close,
        wall: Deadline::at(Instant::now() + Duration::from_secs(60)),
        cancel: CancellationToken::new(),
        tool_grace: Duration::from_secs(60),
        first: None,
    };
    let health = watch::Sender::new(DriverHealth::Open);
    assert_eq!(
        credit(&cap, (&mut orders, &mut force, &health), None)
            .await
            .err(),
        Some(Uncredited::Exhausted)
    );
}

/// x.3.2 X3 S11 (r10 #2): a successor's admission waits at the start
/// gate, before any credit, while its predecessor's `Start` holds it.
/// Its stop, the daemon force, its wall, its registration's failure (with
/// the generation's cause) and its retirement each end the wait at once
/// with that cause: nothing reserved, no health latched by the wait. A
/// lane's end opens the gate, and its `push_start` is then refused.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one case per cutoff arm, each beside its expected cause"
)]
async fn the_start_gate_waits_beside_its_cutoffs() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;
    use via_routes::codex::{Lane, LaneEnd};

    use super::delivery::{LossRecord, Losses, Registration};
    use super::driver::{Orders, Uncredited, credited};
    use crate::observation::{SessionCap, observation_channel};
    use crate::{
        Deadline, DecodeWatermark, DriverFailure, DriverHealth, RouteError, StopCause, StopOrder,
    };

    #[derive(Clone, Copy, Debug)]
    enum Arm {
        Stop,
        Force,
        Wall,
        Failure,
        Retirement,
        LaneEnd,
    }
    let protocol = DriverFailure::Route(RouteError::Protocol {
        turn: TurnNumber::try_from(1).unwrap(),
        detail: "a message of the session's thread did not decode",
    });
    let arms = [
        Arm::Stop,
        Arm::Force,
        Arm::Wall,
        Arm::Failure,
        Arm::Retirement,
        Arm::LaneEnd,
    ];
    for arm in arms {
        let (sink, _received) = observation_channel();
        let cap = SessionCap::new(&sink);
        let registration = Registration::new(4, cap.clone());
        let lane = Arc::new(Lane::default());
        let predecessor = TurnNumber::try_from(1).unwrap();
        assert!(lane.push_start(predecessor, DecodeWatermark::default(), Box::new(())));
        let loss = LossRecord {
            losses: Arc::new(Mutex::new(Losses::default())),
            generation: 1,
        };
        let soon = Instant::now() + Duration::from_millis(100);
        let (stop, stop_watch) = watch::channel(None);
        let (_close, close) = watch::channel(None);
        let (force_set, mut force) = watch::channel(None);
        let health = watch::Sender::new(DriverHealth::Open);
        let failing = watch::Sender::new(DriverHealth::Open);
        let mut orders = Orders {
            stop: stop_watch,
            close,
            wall: Deadline::at(if matches!(arm, Arm::Wall) {
                soon
            } else {
                soon + Duration::from_secs(60)
            }),
            cancel: CancellationToken::new(),
            tool_grace: Duration::from_secs(60),
            first: None,
        };
        let cutoff = async {
            tokio::time::sleep_until(soon).await;
            match arm {
                Arm::Stop => {
                    stop.send_replace(Some(StopOrder {
                        cause: StopCause::Close,
                        requested_at: String::new(),
                        attached: soon,
                        force_at: Deadline::at(soon),
                        close_by: Deadline::at(soon + Duration::from_secs(3)),
                    }));
                }
                Arm::Force => {
                    force_set.send_replace(Some(soon));
                }
                Arm::Wall => {}
                Arm::Failure => registration.fail(&protocol, (&failing, &lane, &loss)),
                Arm::Retirement => registration.retire((&lane, &loss), || {}),
                Arm::LaneEnd => lane.end(LaneEnd::Retired),
            }
            std::future::pending::<()>().await;
        };
        let gate = Some((lane.as_ref(), &*registration));
        let waited = tokio::select! {
            waited = tokio::time::timeout(
                Duration::from_secs(5),
                credited(&cap, (&mut orders, &mut force, &health), gate),
            ) => waited.unwrap(),
            () = cutoff => unreachable!(),
        };
        let expected = match arm {
            Arm::Stop | Arm::Wall => Some(Uncredited::Ordered),
            Arm::Force => Some(Uncredited::Forced),
            Arm::Failure => Some(Uncredited::Gone(Some(protocol.clone()))),
            Arm::Retirement => Some(Uncredited::Gone(None)),
            Arm::LaneEnd => None,
        };
        assert_eq!(waited.as_ref().err(), expected.as_ref(), "{arm:?}");
        assert!(Instant::now() < soon + Duration::from_secs(1), "{arm:?}");
        assert_eq!(*health.borrow(), DriverHealth::Open, "{arm:?}");
        if let Arm::LaneEnd = arm {
            assert_eq!(cap.held().0, 1, "past the gate the credit is taken");
            let successor = TurnNumber::try_from(2).unwrap();
            assert!(
                !lane.push_start(successor, DecodeWatermark::default(), Box::new(())),
                "an ended lane refuses the start: no launch"
            );
        } else {
            assert_eq!(cap.held(), (0, 0), "{arm:?}: nothing reserved");
        }
    }
}

/// x.3.2 X3 S11, no stall arm (r10 #2): a vendor slow to answer the
/// predecessor's start is no consumer stall. With the wall far away the
/// successor still waits at the gate past the observation stall bound
/// (lowered for the test), the driver's health clear and nothing
/// reserved; once the predecessor's start is positively unwritten the
/// gate opens and the credit is taken.
#[cfg(feature = "test-failpoints")]
#[test]
fn the_start_gate_has_no_stall_arm() {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;
    use via_routes::codex::Lane;

    use super::delivery::Registration;
    use super::driver::{Orders, credited};
    use crate::observation::{SessionCap, observation_channel};
    use crate::runtime::event_stall;
    use crate::{Deadline, DecodeWatermark, DriverHealth};

    const NAME: &str = "codex::tests::the_start_gate_has_no_stall_arm";
    if std::env::var_os("VIA_TEST_EVENT_STALL_MS").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env("VIA_TEST_EVENT_STALL_MS", "100")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    assert_eq!(event_stall(), Duration::from_millis(100));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (sink, _received) = observation_channel();
        let cap = SessionCap::new(&sink);
        let registration = Registration::new(4, cap.clone());
        let lane = Arc::new(Lane::default());
        let predecessor = TurnNumber::try_from(1).unwrap();
        assert!(lane.push_start(predecessor, DecodeWatermark::default(), Box::new(())));
        let (_stop, stop) = watch::channel(None);
        let (_close, close) = watch::channel(None);
        let (_force, mut force) = watch::channel(None);
        let health = watch::Sender::new(DriverHealth::Open);
        let mut orders = Orders {
            stop,
            close,
            wall: Deadline::at(Instant::now() + Duration::from_secs(3600)),
            cancel: CancellationToken::new(),
            tool_grace: Duration::from_secs(60),
            first: None,
        };
        let gate = Some((lane.as_ref(), &*registration));
        let waiting = credited(&cap, (&mut orders, &mut force, &health), gate);
        tokio::pin!(waiting);
        let past = tokio::time::timeout(event_stall() * 5, &mut waiting).await;
        assert!(past.is_err(), "still waiting past the stall bound");
        assert_eq!(*health.borrow(), DriverHealth::Open);
        assert_eq!(cap.held(), (0, 0));
        lane.start_unwritten(predecessor);
        let credit = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap();
        assert!(credit.is_ok());
        assert_eq!(cap.held().0, 1);
    });
}

/// Sol code r1 #2, x.3.2 X3 §4.3: 15 thread messages are queued and B's
/// `Start` fills the 16th slot, so B's reply cannot queue its `Reply`
/// (the lane overflows), though the response is paired; the consumer
/// failed the generation before the driver polls. A paired refusal then
/// ends through the failure, launched (cleanup uncertain), never
/// `Rejected`; a paired acceptance is recovered, so its cleanup interrupt
/// names its vendor ID.
#[tokio::test]
async fn a_paired_reply_under_a_failed_generation() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::value::RawValue;
    use tokio::sync::{oneshot, watch};
    use tokio_util::sync::CancellationToken;
    use via_routes::SendOutcome;
    use via_routes::codex::{
        BoundedBytes, Lane, LaneItem, RequestId, Response, Routed, RpcError, VendorMessage,
    };

    use super::delivery::{LossRecord, Losses, Registration};
    use super::driver::{Orders, Unanswered, await_reply};
    use crate::observation::{SessionCap, observation_channel};
    use crate::{Deadline, DecodeWatermark, DriverFailure, DriverHealth, RouteError};

    for accepted in [false, true] {
        let (sink, _received) = observation_channel();
        let registration = Registration::new(4, SessionCap::new(&sink));
        let lane = Arc::new(Lane::default());
        let b = TurnNumber::try_from(2).unwrap();
        for seq in 0..15 {
            let status = b"{\"method\":\"thread/status/changed\",\"params\":{}}\n".to_vec();
            let routed = Routed {
                staged: VendorMessage::new(BoundedBytes::try_from_message(status).unwrap()),
                seq,
                turn: None,
                owner: None,
                at: Instant::now(),
                mark: None,
            };
            assert!(lane.push(LaneItem::Message(routed), 64));
        }
        assert!(lane.push_start(b, DecodeWatermark::default(), Box::new(())));
        let (_, pushed) = lane.push_reply(b, Instant::now(), accepted.then(|| "vendor-b".into()));
        assert!(!pushed && lane.overflowed_now(), "the reply overflows");
        let health = watch::Sender::new(DriverHealth::Open);
        let loss = LossRecord {
            losses: Arc::new(Mutex::new(Losses::default())),
            generation: 1,
        };
        let cause = DriverFailure::Route(RouteError::Overflow { turn: b });
        registration.fail(&cause, (&health, &lane, &loss));
        let outcome = if accepted {
            Ok(RawValue::from_string("{\"turn\":{\"id\":\"vendor-b\"}}".into()).unwrap())
        } else {
            Err(RpcError {
                code: -32600,
                message: "refused".into(),
            })
        };
        let (written_set, written) = oneshot::channel();
        written_set.send(SendOutcome::Written).unwrap();
        let (paired, reply) = oneshot::channel();
        paired
            .send(Response {
                id: RequestId::Int(7),
                outcome,
                contradicted: false,
            })
            .unwrap();
        let (_stop, stop) = watch::channel(None);
        let (_close, close) = watch::channel(None);
        let (_force, mut force) = watch::channel(None);
        let mut orders = Orders {
            stop,
            close,
            wall: Deadline::at(Instant::now() + Duration::from_secs(60)),
            cancel: CancellationToken::new(),
            tool_grace: Duration::from_secs(60),
            first: None,
        };
        let ended = await_reply(
            (written, reply),
            (
                &mut orders,
                &mut force,
                Some((lane.as_ref(), &*registration)),
            ),
            &mut |_| {},
        )
        .await;
        let recovered = matches!(&ended, Ok(response) if response.outcome.is_ok());
        let failed = matches!(&ended, Err(Unanswered::Generation { cause: failed, launched: true })
            if *failed == cause);
        assert!(
            if accepted { recovered } else { failed },
            "accepted {accepted}: another end"
        );
    }
}

/// via-25f: a launch on a SQLite home without Codex's `state_5.sqlite`
/// (its first) takes the 300 s handshake bound, since Codex indexes the
/// user's whole session history before it answers `initialize` (55 s
/// live); once the home holds it, the 60 s bound.
#[test]
fn the_first_launch_on_a_home_takes_the_long_handshake_bound() {
    use via_routes::codex::{HandshakeBound, SERVER_FIRST_HANDSHAKE, SERVER_HANDSHAKE};
    let home = tempfile::tempdir().unwrap();
    let bound = super::driver::handshake_bound(home.path());
    assert_eq!(bound, HandshakeBound::First);
    assert_eq!(bound.duration(), SERVER_FIRST_HANDSHAKE);
    assert_eq!(SERVER_FIRST_HANDSHAKE, std::time::Duration::from_secs(300));
    // Codex's index exists before its backfill completes: still cold.
    std::fs::write(home.path().join("state_5.sqlite"), b"").unwrap();
    assert_eq!(
        super::driver::handshake_bound(home.path()),
        HandshakeBound::First
    );
    // VIA's marker, written after a successful handshake: warm.
    super::driver::mark_initialized(home.path());
    let marker = std::fs::metadata(home.path().join(".via-initialized")).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&marker.permissions()) & 0o777,
        0o600
    );
    let bound = super::driver::handshake_bound(home.path());
    assert_eq!(bound, HandshakeBound::Warm);
    assert_eq!(bound.duration(), SERVER_HANDSHAKE);
    assert_eq!(SERVER_HANDSHAKE, std::time::Duration::from_secs(60));
}

/// The child's selector for [`codex_normalize_peak_within_allowance`].
#[cfg(target_os = "linux")]
const NORMALIZE_PEAK: &str = "VIA_NORMALIZE_PEAK_CHILD";

/// A `/proc/self/status` field in bytes.
#[cfg(target_os = "linux")]
fn proc_status(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap();
    kib * 1024
}

/// Review cfix-1 #2 (X0 item 9.2): a maximal escaped `final_answer`
/// (8 MiB line, one `\n` escape) decoded and normalized in a fresh child,
/// its pieces and decoded message held as the consumer holds them under
/// back-pressure. The peak RSS above the RSS before the decode is within
/// `DECODE_ALLOWANCE`, two maximal messages (serde's unescape scratch and
/// the owned text) plus 65,536 nodes at 64 B: the text moves into its
/// pieces, so normalization adds no second copy. RSS is an estimate (256
/// KiB counter granularity), as in `codex_decode_peak_within_allowance`.
#[cfg(target_os = "linux")]
#[test]
#[expect(
    clippy::print_stdout,
    reason = "the child reports its measure; the parent records it"
)]
fn codex_normalize_peak_within_allowance() {
    use via_routes::codex::MESSAGE_BYTES;
    const ALLOWANCE: u64 = 2 * MESSAGE_BYTES as u64 + 65_536 * 64;
    if std::env::var_os(NORMALIZE_PEAK).is_some() {
        let template = r#"{"method":"item/completed","params":{"item":{"type":"agentMessage","id":"m","text":"\nFILL","phase":"final_answer"},"threadId":"t","turnId":"u","completedAtMs":1}}"#;
        let room = MESSAGE_BYTES - 1 - (template.len() - "FILL".len());
        let line = template.replace("FILL", &"x".repeat(room));
        std::fs::write("/proc/self/clear_refs", "5").unwrap();
        let before = proc_status("VmRSS:");
        let Incoming::Notification(mut notification) = decode(line.as_bytes()).unwrap() else {
            panic!("a notification");
        };
        // The decode's own peak, then the normalization's on top of it.
        let decoded = proc_status("VmHWM:").saturating_sub(before);
        let mut normalizer = TurnNormalizer::new(false);
        let step = normalizer
            .observe(&mut notification, Instant::now())
            .unwrap();
        let peak = proc_status("VmHWM:").saturating_sub(before);
        let Step::Observations(pieces) = &step else {
            panic!("final text pieces");
        };
        let text: usize = pieces.iter().filter_map(final_text).map(str::len).sum();
        assert_eq!(text, room + 1, "the whole text, its newline included");
        drop((notification, step));
        println!("decoded {decoded}");
        println!("measured {peak}");
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "codex::tests::codex_normalize_peak_within_allowance",
            "--nocapture",
        ])
        .env(NORMALIZE_PEAK, "1")
        // glibc: a fixed mmap threshold, as the decode measure sets it.
        .env("MALLOC_MMAP_THRESHOLD_", "131072")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let decoded = stdout
        .lines()
        .find_map(|line| line.strip_prefix("decoded "))
        .unwrap_or_else(|| panic!("{stdout}"));
    println!("decode peak, escaped maximal final_answer: {decoded} B");
    let peak: u64 = stdout
        .lines()
        .find_map(|line| line.strip_prefix("measured "))
        .unwrap_or_else(|| panic!("{stdout}"))
        .parse()
        .unwrap();
    println!("decode and normalize peak: {peak} B against {ALLOWANCE} B");
    assert!(peak <= ALLOWANCE, "{peak} B over {ALLOWANCE} B");
}

/// Critical review (C2 §5, §6.3; packet §4): the handshake-refusal key
/// is the server key, `vendor_args` included, plus a digest of every
/// echoed input. Changing the arguments, the model, the directory or the
/// sandbox, each alone, gives a different key; identical inputs give the
/// same one.
#[test]
fn refusal_key_covers_vendor_args_and_each_echoed_input() {
    use super::plan::{Echoed, Sandbox};
    use via_routes::codex::{SandboxMode, SandboxPolicy, testing::TestRuntime};
    let runtime = TestRuntime::new();
    let adapter = super::CodexAdapter::new(
        PathBuf::from("/bin/codex"),
        std::sync::Arc::default(),
        (&env(), CodexSettings::default()),
        runtime.runtime(),
    );
    let args = |list: &[&str]| {
        crate::VendorArgs::try_from(list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
            .unwrap()
    };
    let full = Sandbox {
        mode: SandboxMode::DangerFullAccess,
        policy: SandboxPolicy::DangerFullAccess,
    };
    let read_only = Sandbox {
        mode: SandboxMode::ReadOnly,
        policy: SandboxPolicy::ReadOnly {
            network_access: false,
        },
    };
    let key = |vendor_args: &[&str], model: &str, cwd: &str, sandbox: &Sandbox| {
        adapter.refusal_key(
            Inherit::OD2_DEFAULT,
            &args(vendor_args),
            &Echoed {
                model,
                cwd: Path::new(cwd),
                sandbox,
            },
        )
    };
    let base = key(&[], "gpt-5", "/work", &full);
    assert_eq!(key(&[], "gpt-5", "/work", &full), base);
    for (name, other) in [
        (
            "vendor_args",
            key(&["--strict-config"], "gpt-5", "/work", &full),
        ),
        ("model", key(&[], "gpt-6", "/work", &full)),
        ("cwd", key(&[], "gpt-5", "/other", &full)),
        ("sandbox", key(&[], "gpt-5", "/work", &read_only)),
    ] {
        assert_ne!(other, base, "{name}");
    }
    assert_eq!(
        key(&["--strict-config"], "gpt-5", "/work", &full),
        key(&["--strict-config"], "gpt-5", "/work", &full)
    );
}
