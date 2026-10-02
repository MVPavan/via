//! The normalizer over the recorded fixtures' vendor lines (the seed of
//! C2's `claude_normalizer_accounting`), and over synthetic lines for what
//! the recordings lack.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use tokio::time::Instant;
use via_routes::claude::decode;

use super::*;

const NEW_SID: &str = "5bd631dc-9254-48ec-9338-a7dc12c2388c";
const RID: &str = "via-interrupt-1";

fn facts(expected: &str, resume: bool) -> LaunchFacts {
    LaunchFacts {
        expected_session: expected.to_owned(),
        resume,
        connection_id: "connection-1".to_owned(),
        correlation: AcceptanceToken::FIRST,
        schema: false,
        mcp: false,
    }
}

/// What a run of messages produced, flattened.
#[derive(Default)]
struct Run {
    batches: Vec<Batch>,
}

impl Run {
    fn observations(&self) -> impl Iterator<Item = &Observation> {
        self.batches.iter().flat_map(|batch| &batch.observations)
    }

    fn kinds(&self) -> Vec<&'static str> {
        self.observations().map(kind).collect()
    }

    fn count(&self, wanted: &str) -> usize {
        self.kinds().iter().filter(|kind| **kind == wanted).count()
    }

    fn end(&self) -> Option<&End> {
        self.batches.iter().find_map(|batch| batch.end.as_ref())
    }

    fn terminal(&self) -> &VendorTerminal {
        match self.end() {
            Some(End::Terminal(terminal)) => terminal,
            other => panic!("no terminal: {other:?}"),
        }
    }

    fn final_text(&self) -> String {
        self.observations()
            .filter_map(|o| {
                if let Observation::FinalText(text) = o {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect()
    }
}

fn kind(observation: &Observation) -> &'static str {
    match observation {
        Observation::Accepted(_) => "turn.accepted",
        Observation::IdentityConfirmed(_) => "session.vendor_identity_confirmed",
        Observation::Progress(_) => "progress",
        Observation::FinalText(_) => "final_text",
        Observation::ActionDenied(_) => "action.denied",
        Observation::RequestDeclined(_) => "vendor.request_declined",
        Observation::SteerDelivered { .. } => "steer.delivered",
        Observation::Warning(_) => "warning",
        Observation::VendorClosed(_) => "session.vendor_closed",
        Observation::ResumeMismatch { .. } => "resume.mismatch",
        Observation::LateTerminal(_) => "turn.late_terminal",
    }
}

fn feed(normalizer: &mut Normalizer, line: &str) -> Batch {
    let message = decode(line.as_bytes()).unwrap_or_else(|e| panic!("{e}: {line}"));
    let mut batch = normalizer.message(message, Instant::now());
    // The driver writes a decline at once; here every write completes.
    if let Some(pending) = batch.decline.take() {
        batch.observations.push(pending.written());
    }
    batch
}

/// Feeds case `name`'s emitted lines, in order, as one launch named
/// `facts`; an interrupt `expect` step records VIA's interrupt first.
fn replay(name: &str, facts: &LaunchFacts) -> (Normalizer, Run) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/claude/{name}.replay.json"));
    let replay: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let mut normalizer = Normalizer::new(facts.clone());
    let mut run = Run::default();
    for step in replay["steps"].as_array().unwrap() {
        if step["expect"]["line"]["request"]["subtype"] == "interrupt" {
            normalizer.interrupt_sent(RID.to_owned());
        }
        if let Some(line) = step["emit"]["line"].as_str() {
            let line = line
                .replace("${sid}", &facts.expected_session)
                .replace("${rid}", &json!(RID).to_string());
            run.batches.push(feed(&mut normalizer, &line));
        }
    }
    (normalizer, run)
}

fn usage_of(terminal: &VendorTerminal) -> (Option<u64>, Option<u64>, Option<u64>) {
    let usage = terminal.usage.as_ref().unwrap();
    (usage.input, usage.cached_input, usage.output)
}

/// c4: one live denial and its terminal entry give one `action.denied`
/// (Q9: an Edit is a file write, its target the file); identity precedes
/// acceptance; the result usage aggregate and the cumulative cost.
#[test]
fn c4_one_denial_deduplicated() {
    let (normalizer, run) = replay("c4_never_ask", &facts(NEW_SID, false));
    let denials: Vec<_> = run
        .observations()
        .filter_map(|o| {
            if let Observation::ActionDenied(denial) = o {
                Some(denial)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(denials.len(), 1);
    assert_eq!(denials[0].kind, DenialKind::FileWrite);
    assert_eq!(denials[0].target, "/work/project/probe.txt");
    assert_eq!(
        denials[0].reason,
        "denied by the vendor's permission policy (mode)"
    );
    assert_eq!(run.count("vendor.request_declined"), 0);
    let kinds = run.kinds();
    assert_eq!(
        kinds[..2],
        ["session.vendor_identity_confirmed", "turn.accepted"]
    );
    assert_eq!(run.count("turn.accepted"), 1);
    let terminal = run.terminal();
    assert_eq!(terminal.status, VendorTerminalStatus::Completed);
    assert_eq!(terminal.stop_reason, StopReason::EndTurn);
    assert_eq!(usage_of(terminal), (Some(20222), Some(12684), Some(1531)));
    assert_eq!(
        terminal.cost,
        Some(CostReport {
            usd: 0.023_973_4,
            scope: "session_cumulative".to_owned()
        })
    );
    assert_eq!(run.final_text(), "DENIED");
    assert_eq!(normalizer.open_tools(), 0);
    assert_eq!(normalizer.unmatched_tool_results(), 0);
}

/// c11b: a `can_use_tool` request is declined (Q9 fields) and its
/// terminal denial suppressed: one decline, zero denials.
#[test]
fn c11b_decline_suppresses_the_denial() {
    let mut normalizer = Normalizer::new(facts(NEW_SID, false));
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude/c11b_stdio_prompt.replay.json");
    let replay: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let mut run = Run::default();
    for step in replay["steps"].as_array().unwrap() {
        let Some(line) = step["emit"]["line"].as_str() else {
            continue;
        };
        let line = line.replace("${sid}", NEW_SID);
        let message = decode(line.as_bytes()).unwrap();
        let mut batch = normalizer.message(message, Instant::now());
        if let Some(pending) = batch.decline.take() {
            assert_eq!(pending.request_id, "00000000-0000-4000-8000-000000000011");
            // Nothing is reported before the write completes.
            assert!(batch.observations.is_empty());
            batch.observations.push(pending.written());
        }
        run.batches.push(batch);
    }
    let declines: Vec<_> = run
        .observations()
        .filter_map(|o| {
            if let Observation::RequestDeclined(decline) = o {
                Some(decline)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(declines.len(), 1);
    assert_eq!(declines[0].vendor_method, "can_use_tool");
    assert_eq!(declines[0].summary, "Edit /work/project/probe.txt");
    assert!(declines[0].blocking);
    assert_eq!(run.count("action.denied"), 0);
    assert_eq!(run.final_text(), "DENIED");
}

/// `c0_isolated`: the synthetic message is never acceptance, progress or
/// final text; the result accepts, and fails `auth` with the synthetic
/// code and the result text as detail.
#[test]
fn c0_synthetic_message_is_excluded() {
    let (_, run) = replay("c0_isolated", &facts(NEW_SID, false));
    // init, synthetic, result: acceptance comes in the result's batch.
    assert_eq!(run.batches.len(), 3);
    assert!(run.batches[1].observations.is_empty(), "synthetic message");
    assert!(
        run.batches[2]
            .observations
            .iter()
            .any(|o| matches!(o, Observation::Accepted(_)))
    );
    assert_eq!(run.count("progress"), 0);
    assert_eq!(run.count("final_text"), 0);
    let terminal = run.terminal();
    assert_eq!(terminal.status, VendorTerminalStatus::Failed);
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert_eq!(terminal.class_hint, Some(ClassHint::Auth));
    assert_eq!(
        terminal.vendor_code.as_deref(),
        Some("authentication_failed")
    );
    assert_eq!(terminal.vendor_stop_reason, "stop_sequence");
    assert_eq!(
        terminal.detail.as_deref(),
        Some("Not logged in · Please run /login")
    );
    assert_eq!(usage_of(terminal), (Some(0), Some(0), Some(0)));
}

/// `c0_bad_model`: an API 404 with the synthetic `model_not_found` is a
/// vendor error with that code, after acceptance.
#[test]
fn c0_bad_model_is_a_vendor_error() {
    let (_, run) = replay("c0_bad_model", &facts(NEW_SID, false));
    let terminal = run.terminal();
    assert_eq!(terminal.class_hint, Some(ClassHint::VendorError));
    assert_eq!(terminal.vendor_code.as_deref(), Some("model_not_found"));
    assert_eq!(run.count("turn.accepted"), 1);
}

/// `c0_invalid_resume`: a pre-init rejection of a resume naming the session
/// as missing is `SessionGone`; nothing is confirmed or accepted, though it
/// echoes the expected UUID. Not a resume, it is a vendor error.
#[test]
fn c0_pre_init_rejection_confirms_nothing() {
    let sid = "33333333-3333-4333-8333-333333333333";
    let (normalizer, run) = replay("c0_invalid_resume", &facts(sid, true));
    assert!(run.kinds().is_empty(), "{:?}", run.kinds());
    assert!(matches!(
        run.end(),
        Some(End::Rejected(StartRejected::SessionGone))
    ));
    assert_eq!(normalizer.instance(), None);
    let (_, run) = replay("c0_invalid_resume", &facts(sid, false));
    assert!(matches!(
        run.end(),
        Some(End::Rejected(StartRejected::VendorError(code, _))) if code == "error_during_execution"
    ));
}

/// `c1b_resume_mismatch`: init naming another session reports the mismatch
/// and ends the turn, with the instance read.
#[test]
fn c1b_mismatch_at_init() {
    let sid = "11111111-1111-4111-8111-111111111111";
    let (normalizer, run) = replay("c1b_resume_mismatch", &facts(sid, true));
    assert_eq!(run.kinds()[0], "resume.mismatch");
    assert!(matches!(
        &run.observations().next(),
        Some(Observation::ResumeMismatch { requested, returned })
            if requested == sid && returned == "99999999-9999-4999-8999-999999999999"
    ));
    assert!(matches!(run.end(), Some(End::ResumeMismatch)));
    assert_eq!(run.count("session.vendor_identity_confirmed"), 0);
    assert_eq!(
        normalizer.instance(),
        Some(InstanceReport {
            vendor_version: Some("2.1.285".to_owned()),
            version_status: VersionStatus::Tested,
        })
    );
}

/// c1a: a schema run's result: structured output verbatim, stop reason
/// `other` keeping `tool_use`, and the `StructuredOutput` tool opened and
/// ended.
#[test]
fn c1a_structured_output() {
    let mut facts = facts(NEW_SID, false);
    facts.schema = true;
    let (_, run) = replay("c1a", &facts);
    let terminal = run.terminal();
    assert_eq!(terminal.stop_reason, StopReason::Other);
    assert_eq!(terminal.vendor_stop_reason, "tool_use");
    assert_eq!(
        terminal.structured_output.as_ref().map(|raw| raw.get()),
        Some(r#"{"kind":"alpha","nonce":"NONCE0001"}"#)
    );
    assert_eq!(run.final_text(), r#"{"kind":"alpha","nonce":"NONCE0001"}"#);
    let started: Vec<_> = run
        .observations()
        .filter_map(|o| {
            if let Observation::Progress(marks) = o {
                Some(marks.tools_started.clone())
            } else {
                None
            }
        })
        .flatten()
        .collect();
    assert_eq!(
        started,
        [(
            "toolu_01SYNTH000001".to_owned(),
            "StructuredOutput".to_owned()
        )]
    );
    let vendor: Value =
        serde_json::from_str(terminal.vendor.as_ref().map(|raw| raw.get()).unwrap()).unwrap();
    assert_eq!(vendor["cost_basis"]["claude-haiku-4-5-20251001"], "list");
    assert!(vendor.get("cache_creation_input_tokens").is_some());
}

/// c7: VIA's interrupt, its matching nested receipt and the abort
/// terminal acknowledge; the terminal is `interrupted`.
#[test]
fn c7_receipt_and_abort_acknowledge() {
    let (normalizer, run) = replay("c7_interrupt", &facts(NEW_SID, false));
    let terminal = run.terminal();
    assert_eq!(terminal.status, VendorTerminalStatus::Interrupted);
    assert_eq!(terminal.stop_reason, StopReason::Interrupted);
    assert_eq!(terminal.class_hint, None);
    assert_eq!(usage_of(terminal), (Some(7853), Some(6538), Some(139)));
    assert!(normalizer.acknowledged());
    assert_eq!(normalizer.open_tools(), 0);
}

fn init_line(sid: &str) -> String {
    json!({"type":"system","subtype":"init","session_id":sid,"claude_code_version":"2.1.300",
        "permissionMode":"dontAsk","tools":["Bash","Edit","Glob","Grep","Read","Write"],
        "capabilities":["interrupt_receipt_v1"]})
    .to_string()
}

fn tool_use(id: &str, name: &str, input: &Value) -> String {
    json!({"type":"assistant","session_id":NEW_SID,"message":{"id":"m","model":"m",
        "content":[{"type":"tool_use","id":id,"name":name,"input":input}]}})
    .to_string()
}

fn result_line(extra: &Value) -> String {
    let mut result = json!({"type":"result","subtype":"success","is_error":false,
        "session_id":NEW_SID,"result":"ok","stop_reason":"end_turn","num_turns":1,
        "usage":{"input_tokens":1,"cache_creation_input_tokens":2,"cache_read_input_tokens":3,
            "output_tokens":4},"permission_denials":[]});
    for (key, value) in extra.as_object().unwrap() {
        result[key] = value.clone();
    }
    result.to_string()
}

fn run_lines(facts: &LaunchFacts, lines: &[String]) -> (Normalizer, Run) {
    let mut normalizer = Normalizer::new(facts.clone());
    let mut run = Run::default();
    for line in lines {
        run.batches.push(feed(&mut normalizer, line));
    }
    (normalizer, run)
}

/// Interrupt pairing: a receipt with another ID, an error receipt, or no
/// interrupt at all never acknowledges; nonempty `still_queued` is a
/// protocol contradiction; an abort with no VIA interrupt is a failure.
#[test]
fn interrupt_pairing_needs_receipt_and_abort() {
    let abort = result_line(&json!({"subtype":"error_during_execution","is_error":true,
        "terminal_reason":"aborted_tools","stop_reason":"tool_use","result":null}));
    let receipt = |id: &str, subtype: &str, queued: &Value| {
        json!({"type":"control_response","response":{"subtype":subtype,"request_id":id,
            "response":{"still_queued":queued}}})
        .to_string()
    };
    let facts = facts(NEW_SID, false);
    for (lines, acknowledged) in [
        (
            vec![receipt(RID, "success", &json!([])), abort.clone()],
            true,
        ),
        (
            vec![receipt("other", "success", &json!([])), abort.clone()],
            false,
        ),
        (
            vec![receipt(RID, "error", &json!([])), abort.clone()],
            false,
        ),
        (vec![abort.clone()], false),
        (
            vec![receipt(RID, "success", &json!([])), result_line(&json!({}))],
            false,
        ),
    ] {
        let mut normalizer = Normalizer::new(facts.clone());
        feed(&mut normalizer, &init_line(NEW_SID));
        normalizer.interrupt_sent(RID.to_owned());
        for line in &lines {
            feed(&mut normalizer, line);
        }
        assert_eq!(normalizer.acknowledged(), acknowledged, "{lines:?}");
    }
    let mut normalizer = Normalizer::new(facts.clone());
    feed(&mut normalizer, &init_line(NEW_SID));
    normalizer.interrupt_sent(RID.to_owned());
    let queued = feed(&mut normalizer, &receipt(RID, "success", &json!([{"x":1}])));
    assert!(matches!(queued.end, Some(End::Protocol(_))));

    let (_, run) = run_lines(&facts, &[init_line(NEW_SID), abort]);
    assert_eq!(run.terminal().status, VendorTerminalStatus::Failed);
    assert_eq!(run.terminal().class_hint, Some(ClassHint::VendorError));
    assert_eq!(run.terminal().vendor_code.as_deref(), Some("aborted_tools"));
}

/// Packet §5: `error_max_turns` is a failed turn, budget exceeded, stop
/// reason `max_steps`, keeping the vendor code; an HTTP 401 is `auth`
/// without a synthetic code.
#[test]
fn terminal_classes() {
    let facts = facts(NEW_SID, false);
    let max = result_line(&json!({"subtype":"error_max_turns","is_error":true,
        "terminal_reason":"max_turns","num_turns":2,"result":null}));
    let (_, run) = run_lines(&facts, &[init_line(NEW_SID), max]);
    let terminal = run.terminal();
    assert_eq!(terminal.status, VendorTerminalStatus::Failed);
    assert_eq!(terminal.stop_reason, StopReason::MaxSteps);
    assert_eq!(terminal.class_hint, Some(ClassHint::BudgetExceeded));
    assert_eq!(terminal.vendor_code.as_deref(), Some("error_max_turns"));
    assert_eq!(terminal.steps, Some(2));
    let unauthorized = result_line(&json!({"is_error":true,"terminal_reason":"api_error",
        "api_error_status":401,"result":"denied"}));
    let (_, run) = run_lines(&facts, &[init_line(NEW_SID), unauthorized]);
    assert_eq!(run.terminal().class_hint, Some(ClassHint::Auth));
    assert_eq!(run.terminal().detail.as_deref(), Some("denied"));
    assert_eq!(
        run.final_text(),
        "",
        "an error's text is detail, never final text"
    );
}

/// A live denial deduplicates its terminal entry; a denial of a call VIA
/// declined is suppressed in both forms; a repeated tool block starts
/// once; Q9 kinds and targets.
#[test]
fn denials_dedup_and_suppress() {
    let facts = facts(NEW_SID, false);
    let denied = |id: &str, tool: &str| {
        json!({"type":"system","subtype":"permission_denied","tool_name":tool,
            "tool_use_id":id,"decision_reason_type":"mode"})
        .to_string()
    };
    let request = json!({"type":"control_request","request_id":"q1","request":{
        "subtype":"can_use_tool","tool_name":"WebFetch","tool_use_id":"t3",
        "input":{"url":"https://example.invalid/x"}}})
    .to_string();
    let lines = [
        init_line(NEW_SID),
        tool_use("t1", "Bash", &json!({"command":"rm -rf /tmp/x"})),
        tool_use("t1", "Bash", &json!({"command":"rm -rf /tmp/x"})),
        denied("t1", "Bash"),
        tool_use(
            "t3",
            "WebFetch",
            &json!({"url":"https://example.invalid/x"}),
        ),
        request,
        denied("t3", "WebFetch"),
        result_line(&json!({"permission_denials":[
            {"tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"rm -rf /tmp/x"}},
            {"tool_name":"WebFetch","tool_use_id":"t3","tool_input":{}},
            {"tool_name":"Glob","tool_use_id":"t4","tool_input":{"pattern":"*.rs"}},
            {"tool_name":"Agent","tool_use_id":"t5","tool_input":{}}]})),
    ];
    let (_, run) = run_lines(&facts, &lines);
    let denials: Vec<_> = run
        .observations()
        .filter_map(|o| {
            if let Observation::ActionDenied(denial) = o {
                Some((denial.kind, denial.target.as_str()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        denials,
        [
            (DenialKind::Command, "rm -rf /tmp/x"),
            (DenialKind::Other, "*.rs"),
            (DenialKind::Other, "Agent"),
        ]
    );
    let started: usize = run
        .observations()
        .filter_map(|o| {
            if let Observation::Progress(marks) = o {
                Some(marks.tools_started.len())
            } else {
                None
            }
        })
        .sum();
    assert_eq!(started, 2, "a repeated block started twice");
    assert_eq!(run.count("vendor.request_declined"), 1);
    let summary = run.observations().find_map(|o| {
        if let Observation::RequestDeclined(decline) = o {
            Some(decline.summary.clone())
        } else {
            None
        }
    });
    assert_eq!(
        summary.as_deref(),
        Some("WebFetch https://example.invalid/x")
    );
}

/// An unknown control request is declined with its own subtype as the
/// method, even before init.
#[test]
fn unknown_control_requests_are_declined() {
    let mut normalizer = Normalizer::new(facts(NEW_SID, false));
    let request = json!({"type":"control_request","request_id":"q9",
        "request":{"subtype":"elicit"}})
    .to_string();
    let batch = normalizer.message(decode(request.as_bytes()).unwrap(), Instant::now());
    let pending = batch.decline.unwrap();
    assert_eq!(pending.request_id, "q9");
    let Observation::RequestDeclined(decline) = pending.written() else {
        panic!("not a decline")
    };
    assert_eq!(decline.vendor_method, "elicit");
    assert_eq!(decline.summary, "an unsupported control request");
    assert!(decline.blocking);
}

/// The handshake check confirms identity first, then refuses a missing
/// receipt capability, another permission mode or another tool surface;
/// the schema's tool and MCP tools are expected only when launched so.
#[test]
fn handshake_refusals() {
    let base: Value = serde_json::from_str(&init_line(NEW_SID)).unwrap();
    let with = |key: &str, value: Value| {
        let mut init = base.clone();
        init[key] = value;
        init.to_string()
    };
    let tools = |extra: &[&str]| {
        let mut tools = vec!["Bash", "Edit", "Glob", "Grep", "Read", "Write"];
        tools.extend(extra);
        json!(tools)
    };
    let mut schema = facts(NEW_SID, false);
    schema.schema = true;
    let mut mcp = facts(NEW_SID, false);
    mcp.mcp = true;
    let plain = facts(NEW_SID, false);
    for (facts, line, refused) in [
        (&plain, init_line(NEW_SID), None),
        (
            &plain,
            with("capabilities", json!(["msg_lifecycle_v1"])),
            Some(Incompatibility::FeatureAbsent("interrupt_receipt_v1")),
        ),
        (
            &plain,
            with("permissionMode", json!("default")),
            Some(Incompatibility::ReadbackDiffers("permission_mode")),
        ),
        (
            &plain,
            with("tools", tools(&["Task"])),
            Some(Incompatibility::ReadbackDiffers("tools")),
        ),
        (
            &plain,
            with("tools", tools(&["StructuredOutput"])),
            Some(Incompatibility::ReadbackDiffers("tools")),
        ),
        (&schema, with("tools", tools(&["StructuredOutput"])), None),
        (
            &schema,
            init_line(NEW_SID),
            Some(Incompatibility::ReadbackDiffers("tools")),
        ),
        (&mcp, with("tools", tools(&["mcp__x__y"])), None),
    ] {
        let (normalizer, run) = run_lines(facts, std::slice::from_ref(&line));
        assert_eq!(run.kinds(), ["session.vendor_identity_confirmed"], "{line}");
        match (run.end(), refused) {
            (None, None) => {}
            (Some(End::Refused(cause)), Some(expected)) => assert_eq!(*cause, expected),
            (end, expected) => panic!("{line}: {end:?}, expected {expected:?}"),
        }
        assert_eq!(
            normalizer.instance().map(|i| i.version_status),
            Some(VersionStatus::Untested),
            "2.1.300 is outside the checked set"
        );
    }
}

/// Order and contradiction: model output before init and a second result
/// are protocol; a pre-init success confirms, accepts and ends; missing
/// usage counts stay unavailable; unknown traffic is activity only.
#[test]
fn order_and_contradictions() {
    let facts = facts(NEW_SID, false);
    let early = tool_use("t1", "Read", &json!({"file_path":"/a"}));
    let (_, run) = run_lines(&facts, &[early]);
    assert!(matches!(run.end(), Some(End::Protocol(_))));

    let (_, run) = run_lines(
        &facts,
        &[
            init_line(NEW_SID),
            result_line(&json!({})),
            result_line(&json!({})),
        ],
    );
    assert!(matches!(
        run.batches[2].end,
        Some(End::Protocol("a second result"))
    ));

    let sole = result_line(&json!({"usage":{"input_tokens":5,"output_tokens":1}}));
    let unknown = json!({"type":"rate_limit_event"}).to_string();
    let (_, run) = run_lines(&facts, &[unknown, sole]);
    assert_eq!(
        run.kinds(),
        [
            "session.vendor_identity_confirmed",
            "turn.accepted",
            "final_text"
        ]
    );
    let usage = run.terminal().usage.clone().unwrap();
    assert_eq!(
        (usage.input, usage.cached_input, usage.output),
        (None, None, Some(1))
    );
    assert_eq!(usage.total, None);
    let other = result_line(&json!({"session_id":"another"}));
    let (_, run) = run_lines(&facts, &[init_line(NEW_SID), other]);
    assert!(matches!(run.end(), Some(End::ResumeMismatch)));
}
