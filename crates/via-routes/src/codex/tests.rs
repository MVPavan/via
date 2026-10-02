//! The Codex messages against the recorded 0.159.2 exchange
//! (`crates/via-adapters/tests/fixtures/codex/`) and the 0.157.1 schema
//! (vendors/codex.md §1).

use std::path::Path;

use serde_json::{Value, json};

use super::*;
use crate::OutboundMessage;

/// One encoded line, parsed; it ends in exactly one LF.
fn parsed(line: &[u8]) -> Value {
    let (body, end) = line.split_at(line.len() - 1);
    assert_eq!(end, b"\n", "a line ends in LF");
    assert!(!body.contains(&b'\n'), "one line");
    serde_json::from_slice(body).unwrap()
}

/// What Wire writes for a streamed start: prefix, escaped prompt, suffix.
fn assembled(message: OutboundMessage) -> Vec<u8> {
    match message {
        OutboundMessage::Start {
            prefix,
            prompt,
            suffix,
            escape,
        } => {
            let mut line = prefix;
            escape(&prompt, &mut line);
            line.extend(suffix);
            line
        }
        OutboundMessage::Interrupt(_) | OutboundMessage::Control(_) => {
            panic!("not a streamed start")
        }
    }
}

fn settings(instructions: Option<&str>) -> ThreadSettings<'_> {
    ThreadSettings {
        model: "gpt-6-sol",
        cwd: Path::new("/work/project"),
        developer_instructions: instructions,
        sandbox: SandboxMode::DangerFullAccess,
    }
}

/// Packet §1–§3: every request VIA writes, exactly, as the fixtures
/// expect it: `clientInfo` `via` with no capability or opt-out, the
/// never-ask settings on every thread and turn, explicit nulls where VIA
/// clears, and developer instructions only when set.
#[test]
fn encoders_write_the_recorded_requests() {
    assert_eq!(
        parsed(&initialize(1, "0.1.0").unwrap()),
        json!({"id": 1, "method": "initialize",
            "params": {"clientInfo": {"name": "via", "version": "0.1.0"}}})
    );
    assert_eq!(
        parsed(&initialized().unwrap()),
        json!({"method": "initialized"})
    );
    assert_eq!(
        parsed(&model_list(2, None).unwrap()),
        json!({"id": 2, "method": "model/list", "params": {}})
    );
    assert_eq!(
        parsed(&model_list(3, Some("page-2")).unwrap()),
        json!({"id": 3, "method": "model/list", "params": {"cursor": "page-2"}})
    );
    assert_eq!(
        parsed(&thread_start(4, &settings(None)).unwrap()),
        json!({"id": 4, "method": "thread/start", "params": {"model": "gpt-6-sol",
            "cwd": "/work/project", "sandbox": "danger-full-access",
            "approvalPolicy": "never", "approvalsReviewer": "user", "ephemeral": false}})
    );
    assert_eq!(
        parsed(&thread_start(4, &settings(Some("Be brief.\n"))).unwrap())["params"]["developerInstructions"],
        json!("Be brief.\n")
    );
    assert_eq!(
        parsed(&thread_resume(5, "019a-thread", &settings(Some("Be brief."))).unwrap()),
        json!({"id": 5, "method": "thread/resume", "params": {"threadId": "019a-thread",
            "model": "gpt-6-sol", "cwd": "/work/project",
            "developerInstructions": "Be brief.", "sandbox": "danger-full-access",
            "approvalPolicy": "never", "approvalsReviewer": "user", "excludeTurns": true}})
    );
    let resume = parsed(&thread_resume(5, "019a-thread", &settings(None)).unwrap());
    assert!(resume["params"].get("developerInstructions").is_none());
    let start = TurnStart {
        thread_id: "019a-thread",
        cwd: Path::new("/work/project"),
        model: "gpt-6-sol",
        effort: Some("low"),
        output_schema: None,
        sandbox_policy: &SandboxPolicy::DangerFullAccess,
    };
    assert_eq!(
        parsed(&assembled(
            turn_start(6, &start, "Say \"hi\"\n".to_owned()).unwrap()
        )),
        json!({"id": 6, "method": "turn/start", "params": {"threadId": "019a-thread",
            "input": [{"type": "text", "text": "Say \"hi\"\n"}], "cwd": "/work/project",
            "model": "gpt-6-sol", "effort": "low", "outputSchema": null,
            "approvalPolicy": "never", "approvalsReviewer": "user",
            "sandboxPolicy": {"type": "dangerFullAccess"}}})
    );
    let schema = serde_json::value::to_raw_value(&json!({"type": "object"})).unwrap();
    let with_schema = TurnStart {
        output_schema: Some(&schema),
        effort: None,
        ..start
    };
    let line = parsed(&assembled(
        turn_start(7, &with_schema, "x".to_owned()).unwrap(),
    ));
    assert_eq!(line["params"]["outputSchema"], json!({"type": "object"}));
    assert_eq!(line["params"]["effort"], Value::Null);
    assert_eq!(
        parsed(&turn_steer(8, "019a-thread", "019a-turn", "more").unwrap()),
        json!({"id": 8, "method": "turn/steer", "params": {"threadId": "019a-thread",
            "expectedTurnId": "019a-turn", "input": [{"type": "text", "text": "more"}]}})
    );
    assert_eq!(
        parsed(&turn_interrupt(9, "019a-thread", "019a-turn").unwrap()),
        json!({"id": 9, "method": "turn/interrupt",
            "params": {"threadId": "019a-thread", "turnId": "019a-turn"}})
    );
    assert_eq!(
        parsed(&thread_unsubscribe(10, "019a-thread").unwrap()),
        json!({"id": 10, "method": "thread/unsubscribe",
            "params": {"threadId": "019a-thread"}})
    );
}

/// A cwd that is not UTF-8 cannot be sent as JSON text: an encode error,
/// never a lossy path.
#[test]
fn a_non_utf8_cwd_is_not_encoded() {
    use std::os::unix::ffi::OsStrExt;
    let cwd = Path::new(std::ffi::OsStr::from_bytes(b"/work/\xff"));
    let settings = ThreadSettings {
        cwd,
        ..settings(None)
    };
    assert!(thread_start(1, &settings).is_err());
}

/// The envelope first (coding style §3): an ID with a result or error is
/// a response, an ID with a method a server request, a method alone a
/// notification; anything else is malformed.
#[test]
fn decode_reads_the_envelope_first() {
    let Incoming::Response(response) = decode(br#"{"id":3,"result":{}}"#).unwrap() else {
        panic!("a result is a response");
    };
    assert_eq!(response.id, RequestId::Int(3));
    assert_eq!(response.outcome.unwrap().get(), "{}");
    let Incoming::Response(response) =
        decode(br#"{"id":4,"error":{"code":-32600,"message":"no rollout found"}}"#).unwrap()
    else {
        panic!("an error is a response");
    };
    assert_eq!(
        response.outcome.unwrap_err(),
        RpcError {
            code: -32600,
            message: "no rollout found".to_owned()
        }
    );
    let Incoming::Request(request) = decode(
        br#"{"id":"srv-1","method":"item/commandExecution/requestApproval","params":{"threadId":"t1","turnId":"u1","itemId":"i","startedAtMs":1}}"#,
    )
    .unwrap() else {
        panic!("an ID with a method is a server request");
    };
    assert_eq!(request.id, RequestId::Str("srv-1".to_owned()));
    assert_eq!(request.method, "item/commandExecution/requestApproval");
    assert_eq!(request.thread_id.as_deref(), Some("t1"));
    assert_eq!(request.turn_id.as_deref(), Some("u1"));
    let Incoming::Request(request) =
        decode(br#"{"id":9,"method":"attestation/generate"}"#).unwrap()
    else {
        panic!("a request without params");
    };
    assert_eq!(request.thread_id, None);
    for malformed in [
        &b"not json"[..],
        br#"{"result":{}}"#,
        br#"{"id":1}"#,
        br#"{"id":1,"result":{},"error":{"code":1,"message":"m"}}"#,
        br#"{"id":true,"result":{}}"#,
        b"[1]",
        br#"{"method":7}"#,
    ] {
        assert!(
            decode(malformed).is_err(),
            "{}",
            String::from_utf8_lossy(malformed)
        );
    }
}

/// Recorded 0.159.2 notifications decode to their typed form; an unknown
/// method keeps its name and thread only (activity).
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one recorded line per notification type keeps each beside its decoding"
)]
fn decode_types_the_recorded_notifications() {
    let line = |text: &str| match decode(text.as_bytes()).unwrap() {
        Incoming::Notification(notification) => notification,
        other @ (Incoming::Response(_) | Incoming::Request(_)) => {
            panic!("not a notification: {other:?}")
        }
    };
    let Notification::TurnStarted(started) = line(
        r#"{"method":"turn/started","params":{"threadId":"t1","turn":{"id":"u1","items":[],"itemsView":"notLoaded","status":"inProgress","error":null,"startedAt":1790000000,"completedAt":null,"durationMs":null}},"emittedAtMs":1}"#,
    ) else {
        panic!("turn/started");
    };
    assert_eq!(
        (started.thread_id.as_str(), started.turn.id.as_str()),
        ("t1", "u1")
    );
    assert_eq!(started.turn.status, TurnStatus::InProgress);
    let Notification::ItemStarted(item) = line(
        r#"{"method":"item/started","params":{"item":{"type":"agentMessage","id":"msg_0004","phase":"final_answer","memoryCitation":null,"delivery":null,"questions":null,"text":""},"threadId":"t1","turnId":"u1","startedAtMs":1},"emittedAtMs":1}"#,
    ) else {
        panic!("item/started");
    };
    assert_eq!(item.item.kind, ItemKind::AgentMessage);
    assert_eq!(item.item.phase.as_deref(), Some("final_answer"));
    assert_eq!(item.item.text.as_deref(), Some(""));
    let Notification::ItemCompleted(item) = line(
        r#"{"method":"item/completed","params":{"item":{"type":"commandExecution","id":"exec-4","pluginId":null,"scriptPath":null,"command":"/bin/bash -lc 'ls /x'","cwd":"/work/project","processId":"1004","source":"unifiedExecStartup","status":"failed","commandActions":[],"aggregatedOutput":"ls: x\n","exitCode":2,"durationMs":0},"threadId":"t1","turnId":"u1","completedAtMs":1},"emittedAtMs":1}"#,
    ) else {
        panic!("item/completed");
    };
    assert_eq!(item.item.kind, ItemKind::CommandExecution);
    assert!(item.item.kind.is_tool());
    assert_eq!(item.item.status.as_deref(), Some("failed"));
    let Notification::ItemStarted(item) = line(
        r#"{"method":"item/started","params":{"item":{"type":"hologram","id":"h1"},"threadId":"t1","turnId":"u1","startedAtMs":1}}"#,
    ) else {
        panic!("item/started");
    };
    assert_eq!(item.item.kind, ItemKind::Other("hologram".to_owned()));
    assert!(!item.item.kind.is_tool());
    let Notification::AgentMessageDelta(delta) = line(
        r#"{"method":"item/agentMessage/delta","params":{"threadId":"t1","turnId":"u1","itemId":"msg_0004","delta":"ACK."},"emittedAtMs":1}"#,
    ) else {
        panic!("delta");
    };
    assert_eq!(
        (delta.item_id.as_str(), delta.delta.as_str()),
        ("msg_0004", "ACK.")
    );
    assert!(matches!(
        line(
            r#"{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"t1","turnId":"u1","itemId":"rs_1","summaryIndex":0,"delta":"x"}}"#
        ),
        Notification::ReasoningDelta(_)
    ));
    let Notification::TokenUsage(usage) = line(
        r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"t1","turnId":"u1","tokenUsage":{"total":{"totalTokens":41135,"inputTokens":40982,"cachedInputTokens":38400,"cacheWriteInputTokens":0,"outputTokens":153,"reasoningOutputTokens":26},"last":{"totalTokens":20613,"inputTokens":20588,"cachedInputTokens":20224,"cacheWriteInputTokens":0,"outputTokens":25,"reasoningOutputTokens":0},"modelContextWindow":258400}},"emittedAtMs":1}"#,
    ) else {
        panic!("tokenUsage");
    };
    assert_eq!(usage.usage.last.input_tokens, 20588);
    assert_eq!(usage.usage.total.total_tokens, 41135);
    assert_eq!(usage.usage.last.cache_write_input_tokens, Some(0));
    assert_eq!(usage.usage.model_context_window, Some(258_400));
    let Notification::Error(error) = line(
        r#"{"method":"error","params":{"error":{"message":"Reconnecting... 1/5","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":401}},"additionalDetails":"unexpected status 401","misalignment":null},"willRetry":true,"threadId":"t1","turnId":"u1"}}"#,
    ) else {
        panic!("error");
    };
    assert!(error.will_retry);
    assert_eq!(
        error.error.codex_error_info,
        Some(CodexErrorInfo {
            kind: "responseStreamDisconnected".to_owned(),
            http_status: Some(401)
        })
    );
    let Notification::TurnCompleted(completed) = line(
        r#"{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","items":[],"itemsView":"notLoaded","status":"failed","error":{"message":"The model is not supported.","codexErrorInfo":"other","additionalDetails":null,"misalignment":null},"startedAt":1,"completedAt":2,"durationMs":3}},"emittedAtMs":1}"#,
    ) else {
        panic!("turn/completed");
    };
    assert_eq!(completed.turn.status, TurnStatus::Failed);
    let error = completed.turn.error.unwrap();
    assert_eq!(error.message, "The model is not supported.");
    assert_eq!(
        error.codex_error_info,
        Some(CodexErrorInfo {
            kind: "other".to_owned(),
            http_status: None
        })
    );
    assert_eq!(
        line(
            r#"{"method":"thread/status/changed","params":{"threadId":"t1","status":{"type":"idle"}}}"#
        ),
        Notification::ThreadStatusChanged {
            thread_id: "t1".to_owned()
        }
    );
    assert_eq!(
        line(r#"{"method":"thread/closed","params":{"threadId":"t1"}}"#),
        Notification::ThreadClosed {
            thread_id: "t1".to_owned()
        }
    );
    assert_eq!(
        line(
            r#"{"method":"remoteControl/status/changed","params":{"status":"disabled","serverName":"host"},"emittedAtMs":5}"#
        ),
        Notification::Unknown {
            method: "remoteControl/status/changed".to_owned(),
            thread_id: None
        }
    );
    assert_eq!(
        line(
            r#"{"method":"turn/diff/updated","params":{"threadId":"t1","turnId":"u1","diff":""}}"#
        ),
        Notification::Unknown {
            method: "turn/diff/updated".to_owned(),
            thread_id: Some("t1".to_owned())
        }
    );
}

/// Coding style §3: a malformed message of a known method is a protocol
/// error, never the unknown fallback; IDs past [`crate::SHORT_FIELD_MAX`]
/// are too.
#[test]
fn a_malformed_known_notification_is_an_error() {
    let long = "t".repeat(crate::SHORT_FIELD_MAX + 1);
    for malformed in [
        r#"{"method":"turn/completed","params":{"threadId":"t1"}}"#.to_owned(),
        r#"{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","status":"paused"}}}"#.to_owned(),
        r#"{"method":"item/started","params":{"item":{"type":"agentMessage","id":"m"},"threadId":"t1","turnId":"u1"}}"#.to_owned(),
        r#"{"method":"item/completed","params":{"item":{"id":"m"},"threadId":"t1","turnId":"u1"}}"#.to_owned(),
        r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"t1","turnId":"u1","tokenUsage":{"total":{},"last":{}}}}"#.to_owned(),
        r#"{"method":"item/agentMessage/delta","params":{"threadId":"t1","turnId":"u1","itemId":"m"}}"#.to_owned(),
        r#"{"method":"error","params":{"error":{"message":"x"},"threadId":"t1","turnId":"u1"}}"#.to_owned(),
        r#"{"method":"thread/closed","params":{}}"#.to_owned(),
        format!(r#"{{"method":"thread/closed","params":{{"threadId":"{long}"}}}}"#),
        format!(
            r#"{{"method":"item/started","params":{{"item":{{"type":"reasoning","id":"{long}"}},"threadId":"t1","turnId":"u1"}}}}"#
        ),
    ] {
        assert!(decode(malformed.as_bytes()).is_err(), "{malformed}");
    }
}

/// The paired results VIA reads, from the recorded replies.
#[test]
fn results_decode_from_the_recorded_replies() {
    let raw = |value: Value| serde_json::value::to_raw_value(&value).unwrap();
    let init: InitializeResult = result(&raw(json!({"userAgent":
        "via/0.159.2 (Linux 6.0.0; x86_64) unknown (via; 0.0.0)", "codexHome": "/state/codex-home",
        "platformFamily": "unix", "platformOs": "linux"})))
    .unwrap();
    assert_eq!(
        init.user_agent,
        "via/0.159.2 (Linux 6.0.0; x86_64) unknown (via; 0.0.0)"
    );
    let page: ModelListResult = result(&raw(json!({"data": [{"id": "gpt-6-luna",
        "model": "gpt-6-luna", "displayName": "GPT-6-Luna", "hidden": false,
        "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": "d"},
            {"reasoningEffort": "max", "description": "d"}],
        "defaultReasoningEffort": "medium", "isDefault": false}], "nextCursor": "c2"})))
    .unwrap();
    assert_eq!(page.next_cursor.as_deref(), Some("c2"));
    assert_eq!(page.data[0].model, "gpt-6-luna");
    assert_eq!(
        page.data[0]
            .supported_reasoning_efforts
            .iter()
            .map(|effort| effort.reasoning_effort.as_str())
            .collect::<Vec<_>>(),
        ["low", "max"]
    );
    let thread: ThreadResult = result(&raw(json!({"thread": {"id": "t1", "turns": []},
        "model": "gpt-6-sol", "cwd": "/work/project",
        "instructionSources": ["/work/project/AGENTS.md"], "approvalPolicy": "never",
        "approvalsReviewer": "user", "sandbox": {"type": "dangerFullAccess"},
        "reasoningEffort": "medium"})))
    .unwrap();
    assert_eq!(thread.thread.id, "t1");
    assert_eq!(thread.approval_policy, json!("never"));
    assert_eq!(thread.approvals_reviewer.as_deref(), Some("user"));
    assert_eq!(thread.sandbox, json!({"type": "dangerFullAccess"}));
    assert_eq!(thread.instruction_sources, ["/work/project/AGENTS.md"]);
    let turn: TurnStartResult = result(&raw(json!({"turn": {"id": "u1", "items": [],
        "itemsView": "notLoaded", "status": "inProgress", "error": null}})))
    .unwrap();
    assert_eq!(turn.turn.id, "u1");
    let steer: TurnSteerResult = result(&raw(json!({"turnId": "u1"}))).unwrap();
    assert_eq!(steer.turn_id, "u1");
    for (status, expected) in [
        ("unsubscribed", UnsubscribeStatus::Unsubscribed),
        ("notSubscribed", UnsubscribeStatus::NotSubscribed),
        ("notLoaded", UnsubscribeStatus::NotLoaded),
    ] {
        let reply: UnsubscribeResult = result(&raw(json!({ "status": status }))).unwrap();
        assert_eq!(reply.status, expected);
    }
    assert!(result::<TurnStartResult>(&raw(json!({"turn": {}}))).is_err());
    assert!(result::<UnsubscribeResult>(&raw(json!({"status": "gone"}))).is_err());
}

/// Packet §4: a listed method gets its no-grant body as the result, under
/// the exact incoming ID; anything else gets `-32601`.
#[test]
fn the_decline_table_answers_every_request() {
    const TABLE: DeclineTable = DeclineTable::new(&[(
        "item/commandExecution/requestApproval",
        r#"{"decision":"decline"}"#,
    )]);
    assert_eq!(
        parsed(&TABLE.reply(&RequestId::Int(7), "item/commandExecution/requestApproval")),
        json!({"id": 7, "result": {"decision": "decline"}})
    );
    assert!(TABLE.declines("item/commandExecution/requestApproval"));
    for method in [
        "account/chatgptAuthTokens/refresh",
        "execCommandApproval",
        "x/y",
    ] {
        assert!(!TABLE.declines(method));
        assert_eq!(
            parsed(&TABLE.reply(&RequestId::Str("srv-9".to_owned()), method)),
            json!({"id": "srv-9", "error": {"code": -32601, "message": "Method not supported by VIA"}})
        );
    }
}
