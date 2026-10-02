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
        self.batches
            .iter()
            .find_map(|batch| batch.terminal.as_deref())
            .unwrap_or_else(|| panic!("no terminal: {:?}", self.end()))
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
        let written = normalizer.declined(pending);
        batch.observations.extend(written.observations);
        batch.end = batch.end.or(written.end);
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
            batch
                .observations
                .extend(normalizer.declined(pending).observations);
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
    let written = normalizer.declined(pending);
    let Some(Observation::RequestDeclined(decline)) = written.observations.first() else {
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

fn receipt_line(id: &str, response: &Value) -> String {
    let mut line = json!({"type":"control_response","response":{"subtype":"success",
        "request_id":id}});
    if !response.is_null() {
        line["response"]["response"] = response.clone();
    }
    line.to_string()
}

fn abort_line(subtype: &str) -> String {
    result_line(&json!({"subtype":subtype,"is_error":true,
        "terminal_reason":"aborted_tools","stop_reason":"tool_use","result":null}))
}

/// Review r1 #1: only a qualified receipt (matching, `success`, the nested
/// `still_queued` body) read before an `error_during_execution` /
/// `aborted_tools` terminal acknowledges; acknowledgement is frozen at the
/// terminal; without it the abort is an ordinary vendor terminal.
#[test]
fn receipt_must_precede_the_abort() {
    let queued = json!({"still_queued":[]});
    let receipt = receipt_line(RID, &queued);
    for (lines, interrupted) in [
        (
            vec![receipt.clone(), abort_line("error_during_execution")],
            true,
        ),
        (
            vec![abort_line("error_during_execution"), receipt.clone()],
            false,
        ),
        (
            vec![
                receipt_line(RID, &Value::Null),
                abort_line("error_during_execution"),
            ],
            false,
        ),
        (
            vec![
                receipt_line(RID, &json!({})),
                abort_line("error_during_execution"),
            ],
            false,
        ),
        (vec![receipt.clone(), abort_line("success")], false),
    ] {
        let mut normalizer = Normalizer::new(facts(NEW_SID, false));
        feed(&mut normalizer, &init_line(NEW_SID));
        normalizer.interrupt_sent(RID.to_owned());
        let mut run = Run::default();
        for line in &lines {
            run.batches.push(feed(&mut normalizer, line));
        }
        assert_eq!(normalizer.acknowledged(), interrupted, "{lines:?}");
        let terminal = run.terminal();
        if interrupted {
            assert_eq!(terminal.status, VendorTerminalStatus::Interrupted);
        } else {
            assert_eq!(terminal.status, VendorTerminalStatus::Failed, "{lines:?}");
            assert_eq!(terminal.class_hint, Some(ClassHint::VendorError));
            assert_eq!(terminal.vendor_code.as_deref(), Some("aborted_tools"));
        }
    }
}

/// Review r1 #2: usage sums past `u64` are a protocol failure, never a
/// panic or a wrapped count.
#[test]
fn usage_overflow_is_protocol() {
    for usage in [
        json!({"input_tokens":u64::MAX,"cache_creation_input_tokens":1,
            "cache_read_input_tokens":0,"output_tokens":0}),
        json!({"input_tokens":u64::MAX,"cache_creation_input_tokens":0,
            "cache_read_input_tokens":0,"output_tokens":1}),
    ] {
        let (_, run) = run_lines(
            &facts(NEW_SID, false),
            &[init_line(NEW_SID), result_line(&json!({"usage":usage}))],
        );
        assert!(matches!(run.end(), Some(End::Protocol(_))), "{usage}");
    }
}

/// Review r1 #3: the retained terminal and each emitted observation stay
/// within 256 KiB encoded: a 300 KiB structured output and a progress mark
/// of 150 long tool starts are protocol; 200 KiB is retained.
#[test]
fn retained_payload_is_bounded() {
    let mut schema = facts(NEW_SID, false);
    schema.schema = true;
    let init = json!({"type":"system","subtype":"init","session_id":NEW_SID,
        "claude_code_version":"2.1.300","permissionMode":"dontAsk",
        "tools":["Bash","Edit","Glob","Grep","Read","StructuredOutput","Write"],
        "capabilities":["interrupt_receipt_v1"]})
    .to_string();
    let output = |bytes: usize| json!({"structured_output":{"a":"x".repeat(bytes)}});
    let (_, run) = run_lines(&schema, &[init.clone(), result_line(&output(300 * 1024))]);
    assert!(matches!(run.end(), Some(End::Protocol(_))));
    let (_, run) = run_lines(&schema, &[init, result_line(&output(200 * 1024))]);
    assert!(matches!(run.end(), Some(End::Terminal)));

    let blocks: Vec<Value> = (0..150)
        .map(|i| {
            json!({"type":"tool_use","id":format!("{i:04}{}", "i".repeat(996)),
                "name":"n".repeat(1000),"input":{}})
        })
        .collect();
    let many = json!({"type":"assistant","session_id":NEW_SID,
        "message":{"id":"m","model":"m","content":blocks}})
    .to_string();
    let (_, run) = run_lines(&facts(NEW_SID, false), &[init_line(NEW_SID), many]);
    // These calls pass the tracking byte bound before the progress bound
    // (`progress_encoding_is_exact` covers that one): either way the turn
    // ends without an oversized mark.
    assert!(matches!(run.end(), Some(End::Protocol(_) | End::Overflow)));
    assert_eq!(run.count("progress"), 0);
}

fn denied_line(id: &str, tool: &str) -> String {
    json!({"type":"system","subtype":"permission_denied","tool_name":tool,
        "tool_use_id":id,"decision_reason_type":"mode"})
    .to_string()
}

fn tool_result(id: &str) -> String {
    json!({"type":"user","session_id":NEW_SID,"message":{"role":"user",
        "content":[{"type":"tool_result","tool_use_id":id,"content":"x"}]}})
    .to_string()
}

/// Review r1 #4: the first new ID past a tracking bound is an explicit
/// protocol failure, for each set: calls (open and completed together),
/// denials and declines.
#[test]
fn tracking_overflow_is_explicit() {
    let ids = |n: usize| (0..n).map(|i| format!("t{i}")).collect::<Vec<_>>();
    let protocol = |batch: &Batch| matches!(batch.end, Some(End::Overflow));
    let uses = |ids: &[String]| {
        let blocks: Vec<Value> = ids
            .iter()
            .map(|id| json!({"type":"tool_use","id":id,"name":"Read","input":{}}))
            .collect();
        json!({"type":"assistant","session_id":NEW_SID,
            "message":{"id":"m","model":"m","content":blocks}})
        .to_string()
    };

    // Open calls.
    let mut normalizer = Normalizer::new(facts(NEW_SID, false));
    feed(&mut normalizer, &init_line(NEW_SID));
    assert!(!protocol(&feed(&mut normalizer, &uses(&ids(TRACKED)))));
    assert!(protocol(&feed(
        &mut normalizer,
        &tool_use("new", "Read", &json!({}))
    )));

    // Completed calls count in the same bound.
    let mut normalizer = Normalizer::new(facts(NEW_SID, false));
    feed(&mut normalizer, &init_line(NEW_SID));
    let all = ids(TRACKED);
    feed(&mut normalizer, &uses(&all));
    for id in &all {
        assert!(!protocol(&feed(&mut normalizer, &tool_result(id))));
    }
    assert_eq!(normalizer.open_tools(), 0);
    assert!(protocol(&feed(
        &mut normalizer,
        &tool_use("new", "Read", &json!({}))
    )));

    // Denials.
    let mut normalizer = Normalizer::new(facts(NEW_SID, false));
    feed(&mut normalizer, &init_line(NEW_SID));
    for id in ids(TRACKED) {
        assert!(!protocol(&feed(&mut normalizer, &denied_line(&id, "Read"))));
    }
    assert!(protocol(&feed(
        &mut normalizer,
        &denied_line("new", "Read")
    )));

    // Declines.
    let request = |id: &str| {
        json!({"type":"control_request","request_id":format!("q{id}"),"request":{
            "subtype":"can_use_tool","tool_name":"Read","tool_use_id":id,"input":{}}})
        .to_string()
    };
    let mut normalizer = Normalizer::new(facts(NEW_SID, false));
    feed(&mut normalizer, &init_line(NEW_SID));
    for id in ids(TRACKED) {
        assert!(!protocol(&feed(&mut normalizer, &request(&id))));
    }
    assert!(protocol(&feed(&mut normalizer, &request("new"))));
    // Sticky: nothing is taken after an overflow.
    let after = feed(&mut normalizer, &result_line(&json!({})));
    assert!(protocol(&after) && after.terminal.is_none());

    // By bytes: 300 calls with 1000-byte IDs pass the shared byte bound
    // long before the count bound.
    let long: Vec<String> = (0..300)
        .map(|i| format!("{i:04}{}", "i".repeat(996)))
        .collect();
    let mut normalizer = Normalizer::new(facts(NEW_SID, false));
    feed(&mut normalizer, &init_line(NEW_SID));
    let overflowed = long
        .iter()
        .map(|id| feed(&mut normalizer, &tool_use(id, "Read", &json!({}))))
        .position(|batch| protocol(&batch));
    assert!(overflowed.is_some_and(|at| at < 300), "{overflowed:?}");
}

/// Review r1 #5: a completed call keeps its target: a replayed block does
/// not start it again, and a later denial names what it acted on.
#[test]
fn completed_calls_keep_their_target() {
    let edit = tool_use("t1", "Edit", &json!({"file_path":"/a"}));
    let (_, run) = run_lines(
        &facts(NEW_SID, false),
        &[
            init_line(NEW_SID),
            edit.clone(),
            tool_result("t1"),
            edit,
            denied_line("t1", "Edit"),
            result_line(&json!({"permission_denials":[
                {"tool_name":"Edit","tool_use_id":"t1","tool_input":{}}]})),
        ],
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
    assert_eq!(started, 1, "a completed call started again");
    let targets: Vec<_> = run
        .observations()
        .filter_map(|o| {
            if let Observation::ActionDenied(denial) = o {
                Some(denial.target.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(targets, ["/a"]);
}

/// Review r1 #6: classification reads the qualified combinations. HTTP
/// 401 is `auth` whatever the subtype; `max_turns` without
/// `error_max_turns` is an ordinary vendor error keeping its own code.
#[test]
fn classification_follows_qualified_evidence() {
    let facts = facts(NEW_SID, false);
    let classify = |extra: Value| {
        let (_, run) = run_lines(&facts, &[init_line(NEW_SID), result_line(&extra)]);
        let terminal = run.terminal();
        (
            terminal.stop_reason,
            terminal.class_hint,
            terminal.vendor_code.clone(),
        )
    };
    assert_eq!(
        classify(json!({"subtype":"error_max_turns","is_error":true,
            "terminal_reason":"api_error","api_error_status":401,"result":"x"})),
        (
            StopReason::Error,
            Some(ClassHint::Auth),
            Some("api_error".to_owned())
        )
    );
    assert_eq!(
        classify(json!({"subtype":"error_during_execution","is_error":true,
            "terminal_reason":"max_turns","result":"x"})),
        (
            StopReason::Error,
            Some(ClassHint::VendorError),
            Some("max_turns".to_owned())
        )
    );
    assert_eq!(
        classify(json!({"subtype":"error_max_turns","is_error":true,"result":"x"})),
        (
            StopReason::Error,
            Some(ClassHint::VendorError),
            Some("error_max_turns".to_owned())
        )
    );
    assert_eq!(
        classify(json!({"subtype":"error_max_turns","is_error":true,
            "terminal_reason":"max_turns","result":"x"})),
        (
            StopReason::MaxSteps,
            Some(ClassHint::BudgetExceeded),
            Some("error_max_turns".to_owned())
        )
    );
}

/// Review r1 #7: every init is checked: a same-session init that differs
/// is a protocol contradiction; an identical one is nothing new.
#[test]
fn duplicate_inits_must_agree() {
    let facts = facts(NEW_SID, false);
    let (_, run) = run_lines(&facts, &[init_line(NEW_SID), init_line(NEW_SID)]);
    assert!(run.end().is_none());
    assert_eq!(run.count("session.vendor_identity_confirmed"), 1);
    let mut other: Value = serde_json::from_str(&init_line(NEW_SID)).unwrap();
    other["claude_code_version"] = json!("9.9.9");
    other["permissionMode"] = json!("default");
    let (_, run) = run_lines(&facts, &[init_line(NEW_SID), other.to_string()]);
    assert!(matches!(run.end(), Some(End::Protocol(_))));
}

/// Review r1 #9: suppression waits for the whole decline write; a pending
/// decline dropped unwritten suppresses nothing.
#[test]
fn decline_suppresses_only_once_written() {
    let mut normalizer = Normalizer::new(facts(NEW_SID, false));
    feed(&mut normalizer, &init_line(NEW_SID));
    let request = json!({"type":"control_request","request_id":"q1","request":{
        "subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"t1",
        "input":{"command":"ls"}}})
    .to_string();
    let batch = normalizer.message(decode(request.as_bytes()).unwrap(), Instant::now());
    let pending = batch.decline.unwrap();
    assert!(batch.observations.is_empty());
    drop(pending);
    let denied = feed(&mut normalizer, &denied_line("t1", "Bash"));
    assert!(matches!(
        denied.observations.as_slice(),
        [Observation::ActionDenied(_)]
    ));
}

/// Review r1 #12: unknown result metadata is kept in vendor data, and one
/// oversized member drops alone.
#[test]
fn vendor_data_keeps_what_fits() {
    let facts = facts(NEW_SID, false);
    let vendor = |extra: Value| {
        let (_, run) = run_lines(&facts, &[init_line(NEW_SID), result_line(&extra)]);
        let raw = run
            .terminal()
            .vendor
            .as_ref()
            .map(|raw| raw.get().to_owned());
        serde_json::from_str::<Value>(&raw.unwrap()).unwrap()
    };
    let small = vendor(json!({"new_vendor_stat":3}));
    assert_eq!(small["extra"]["new_vendor_stat"], 3);
    assert_eq!(small["cache_creation_input_tokens"], 2);
    let big = vendor(json!({"new_vendor_stat":3,"huge":"h".repeat(20 * 1024),
        "modelUsage":{"m":{"costBasis":"list"}}}));
    assert_eq!(big["extra"]["new_vendor_stat"], 3);
    assert!(big["extra"].get("huge").is_none());
    assert_eq!(big["cost_basis"]["m"], "list");
}

/// Review r2 #1: the progress mark's exact encoding is bounded. With
/// 4,096 short starts, the largest ID length that encodes within 256 KiB
/// passes and one more byte per ID is protocol.
#[test]
fn progress_encoding_is_exact() {
    let ids = |len: usize| -> Vec<String> {
        (0..TRACKED)
            .map(|i| format!("{i:04}{}", "i".repeat(len - 4)))
            .collect()
    };
    let encoded = |ids: &[String]| {
        let pairs: Vec<Value> = ids.iter().map(|id| json!([id, "R"])).collect();
        json!({"model":true,"tools_started":pairs,"tools_ended":[]})
            .to_string()
            .len()
    };
    let over = (8..200)
        .find(|len| encoded(&ids(*len)) > MAX_OBSERVATION_BYTES)
        .unwrap();
    for (len, fits) in [(over - 1, true), (over, false)] {
        let blocks: Vec<Value> = ids(len)
            .iter()
            .map(|id| json!({"type":"tool_use","id":id,"name":"R","input":{}}))
            .collect();
        let line = json!({"type":"assistant","session_id":NEW_SID,
            "message":{"id":"m","model":"m","content":blocks}})
        .to_string();
        let (_, run) = run_lines(&facts(NEW_SID, false), &[init_line(NEW_SID), line]);
        assert_eq!(run.count("progress") == 1, fits, "{len}");
        assert_eq!(matches!(run.end(), Some(End::Protocol(_))), !fits, "{len}");
    }
}

/// Review r2 #2 (AD4): a denial overflow at the terminal keeps the
/// terminal, its structured output, usage and cost, beside the failure.
#[test]
fn terminal_survives_a_denial_overflow() {
    let mut normalizer = Normalizer::new(facts(NEW_SID, false));
    feed(&mut normalizer, &init_line(NEW_SID));
    for i in 0..TRACKED {
        feed(&mut normalizer, &denied_line(&format!("t{i}"), "Read"));
    }
    let result = result_line(&json!({"structured_output":{"a":1},"total_cost_usd":0.5,
        "permission_denials":[{"tool_name":"Read","tool_use_id":"new","tool_input":{}}]}));
    let batch = feed(&mut normalizer, &result);
    assert!(matches!(batch.end, Some(End::Overflow)), "{:?}", batch.end);
    let terminal = batch.terminal.expect("the terminal is kept");
    assert_eq!(
        terminal.structured_output.as_ref().map(|raw| raw.get()),
        Some(r#"{"a":1}"#)
    );
    assert!(terminal.usage.is_some());
    assert_eq!(terminal.cost.as_ref().map(|cost| cost.usd), Some(0.5));
}

/// Review r2 #4: `terminal_reason: authentication_failed` alone is `auth`,
/// keeping the vendor's code.
#[test]
fn authentication_reason_alone_is_auth() {
    let (_, run) = run_lines(
        &facts(NEW_SID, false),
        &[
            init_line(NEW_SID),
            result_line(&json!({"subtype":"error_during_execution","is_error":true,
                "terminal_reason":"authentication_failed","result":"x"})),
        ],
    );
    assert_eq!(run.terminal().class_hint, Some(ClassHint::Auth));
    assert_eq!(
        run.terminal().vendor_code.as_deref(),
        Some("authentication_failed")
    );
}

/// Review r2 #3: nested raw output within the limits reaches the terminal
/// verbatim, and nested unknown metadata never rejects the result.
#[test]
fn deep_raw_members_reach_the_terminal() {
    let deep = format!("{}{}", "[".repeat(60), "]".repeat(60));
    let base = result_line(&json!({}));
    let with = |member: &str| base.replacen('{', &format!("{{\"{member}\":{deep},"), 1);
    let (_, run) = run_lines(
        &facts(NEW_SID, false),
        &[init_line(NEW_SID), with("structured_output")],
    );
    assert_eq!(
        run.terminal()
            .structured_output
            .as_ref()
            .map(|raw| raw.get()),
        Some(deep.as_str())
    );
    let (_, run) = run_lines(
        &facts(NEW_SID, false),
        &[init_line(NEW_SID), with("new_stat")],
    );
    assert_eq!(run.terminal().status, VendorTerminalStatus::Completed);
}

/// Review r3 #2: a terminal denial's target is read from its member alone;
/// an unrelated member that would not parse as a whole leaves it intact.
#[test]
fn denial_target_reads_its_member_only() {
    let result = result_line(&json!({})).replacen(
        r#""permission_denials":[]"#,
        r#""permission_denials":[{"tool_name":"Write","tool_use_id":"t9","tool_input":{"content":"\ud800","file_path":"/f","big":[1,2,3]}}]"#,
        1,
    );
    let (_, run) = run_lines(&facts(NEW_SID, false), &[init_line(NEW_SID), result]);
    let targets: Vec<_> = run
        .observations()
        .filter_map(|o| {
            if let Observation::ActionDenied(denial) = o {
                Some(denial.target.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(targets, ["/f"]);
}
