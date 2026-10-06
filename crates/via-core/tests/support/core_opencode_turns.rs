//! Chunk C: real Core/Host/Wire, request-triggered synthetic vendor events.
use super::{core_opencode, run};
use core_opencode::{
    Case, SES, begin_events, class, event, prompt_response, prompt_route, route, server, step_end,
    success_events, with_route,
};
use serde_json::{Value, json};

/// Collect first, stop every owned process, then assert. RED failures leave no
/// vendor behind and cannot borrow the owner's `OpenCode` service.
fn one(events: Vec<Value>) -> (Value, Vec<Value>) {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&with_route(server(&cwd, None), prompt_route(events)));
    let envelope = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let envelope = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        envelope
    });
    run(case.all_gone());
    (envelope.unwrap(), case.requests())
}

#[test]
fn oc03_spawn_background_wait_events_logs_and_detach() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&server(&cwd, None));
    let (envelope, receipt, events, logs, closed) = run(async {
        let daemon = case.daemon();
        let (session, receipt) = daemon.spawn(&cwd, &json!({})).await;
        let envelope = daemon.wait_result(&session, 1).await;
        let events = daemon.events(&session, 1).await;
        let logs = daemon.logs(&session, 1).await;
        let closed = daemon.close(&session).await;
        daemon.stop().await;
        (envelope, receipt, events, logs, closed)
    });
    run(case.all_gone());
    let envelope = envelope.unwrap();
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["final_text"], "done", "{envelope}");
    assert_eq!(envelope["stop_reason"], "end_turn", "{envelope}");
    assert!(receipt.to_string().contains("queued"), "{receipt}");
    assert!(events.to_string().contains("turn.started"), "{events}");
    assert!(events.to_string().contains("turn.ended"), "{events}");
    assert_eq!(logs["vendor_session_id"], SES, "{logs}");
    assert_eq!(closed["state"], "closed", "{closed}");
    assert_eq!(closed["cleanup"], "quiescent", "{closed}");
    assert!(closed["leftovers"].is_null(), "{closed}");
    assert!(case.requests_to("DELETE", "/api/session/").is_empty());
    let prompts = case.requests_to("POST", &format!("/api/session/{SES}/prompt"));
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["body"]["text"], "Say done.");
    assert_eq!(prompts[0]["body"]["id"].as_str().unwrap().len(), 29);
    assert!(
        prompts[0]["body"]["id"]
            .as_str()
            .unwrap()
            .starts_with("msg_via")
    );
}

#[test]
fn oc03_sse_acceptance_precedes_the_http_response_and_is_deduplicated() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    let mut response = prompt_response(success_events("sse first"));
    response["emit_before_response"] = json!(true);
    response["sleep_ms"] = json!(80);
    case.fixture(&with_route(
        server(&cwd, None),
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            json!([response]),
        ),
    ));
    let (envelope, events) = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let envelope = daemon.wait_result(&session, 1).await;
        let events = daemon.events(&session, 1).await;
        daemon.stop().await;
        (envelope, events)
    });
    run(case.all_gone());
    let envelope = envelope.unwrap();
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(
        events.to_string().matches("turn.started").count(),
        1,
        "{events}"
    );
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .len(),
        1
    );
}

#[test]
fn oc04_two_turns_and_restart_reopen_preserve_identity_and_never_resend() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&server(&cwd, None));
    let (session, first, second) = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let first = daemon.wait_result(&session, 1).await;
        daemon.try_resume(&session, &json!({})).await.unwrap();
        let second = daemon.wait_result(&session, 2).await;
        daemon.stop().await;
        (session, first, second)
    });
    run(case.all_gone());
    let before_reopen = case
        .requests_to("GET", &format!("/api/session/{SES}"))
        .iter()
        .filter(|request| request["target"] == format!("/api/session/{SES}"))
        .count();
    assert_eq!(before_reopen, 1, "turn two checks its current variant");
    let third = run(async {
        let daemon = case.daemon();
        daemon.try_resume(&session, &json!({})).await.unwrap();
        let third = daemon.wait_result(&session, 3).await;
        daemon.stop().await;
        third
    });
    run(case.all_gone());
    for envelope in [first, second, third] {
        let envelope = envelope.unwrap();
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["final_text"], "done", "{envelope}");
    }
    assert_eq!(case.creates().len(), 1);
    assert_eq!(
        case.requests_to("GET", &format!("/api/session/{SES}"))
            .iter()
            .filter(|r| r["target"] == format!("/api/session/{SES}"))
            .count(),
        before_reopen + 1,
        "one identity/settings read on the new server generation"
    );
    assert_eq!(
        case.requests_to("GET", &format!("/api/session/{SES}/inbox"))
            .len(),
        1
    );
    let prompts = case.requests_to("POST", &format!("/api/session/{SES}/prompt"));
    assert_eq!(prompts.len(), 3);
    assert_ne!(prompts[0]["body"]["id"], prompts[1]["body"]["id"]);
    assert_ne!(prompts[1]["body"]["id"], prompts[2]["body"]["id"]);
}

#[test]
fn oc06_final_text_uses_last_owned_step_in_ordinal_order() {
    let mut events = begin_events();
    events.extend([
        event(
            "session.text.ended",
            json!({"assistantMessageID":"$INPUT:a", "ordinal":0,"text":"earlier"}),
        ),
        step_end("$INPUT:a", 11, 0.25),
        event(
            "session.step.started",
            json!({"assistantMessageID":"$INPUT:b"}),
        ),
        event(
            "session.text.ended",
            json!({"assistantMessageID":"$INPUT:b", "ordinal":1,"text":"B"}),
        ),
        event(
            "session.text.ended",
            json!({"assistantMessageID":"$INPUT:b", "ordinal":0,"text":"A"}),
        ),
        step_end("$INPUT:b", 13, 0.5),
        event("session.execution.succeeded", json!({})),
    ]);
    let (envelope, _) = one(events);
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["final_text"], "AB", "{envelope}");
    assert_eq!(envelope["usage"]["input_tokens"], 24, "{envelope}");
}

#[test]
fn oc06_unknown_types_fields_and_child_events_do_not_terminate_root() {
    let mut events = begin_events();
    events.extend([
        event("session.execution.unrecognised", json!({"future":true})),
        event("session.created", json!({"sessionID":"ses_child", "parentID":"$SESSION"})),
        event("session.execution.succeeded", json!({"sessionID":"ses_child"})),
        event("session.text.ended", json!({"assistantMessageID":"$INPUT:a","ordinal":0,"text":"root", "future":{"secret":"ignored"}})),
        step_end("$INPUT:a",11,0.25),
        event("session.execution.succeeded",json!({"future":true})),
    ]);
    let (envelope, _) = one(events);
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["final_text"], "root", "{envelope}");
}

#[test]
fn oc06_malformed_known_payload_fails_protocol() {
    let mut events = begin_events();
    events.push(event(
        "session.text.ended",
        json!({"assistantMessageID":"$INPUT:a","ordinal":0,"text":17}),
    ));
    let (envelope, _) = one(events);
    assert_eq!(class(&envelope), "protocol", "{envelope}");
    assert_eq!(envelope["usage"]["provenance"], "unavailable", "{envelope}");
}

#[test]
fn oc06_non_json_data_fails_generation_protocol() {
    let mut events = begin_events();
    events.push(json!({"raw_data":"not JSON"}));
    let (envelope, _) = one(events);
    assert_eq!(class(&envelope), "protocol", "{envelope}");
}

#[test]
fn oc06_non_increasing_session_sequence_is_protocol() {
    let mut events = begin_events();
    for event in &mut events {
        event["durable"] = json!({"aggregateID":"$SESSION", "seq":7,"version":1});
    }
    let (envelope, _) = one(events);
    assert_eq!(class(&envelope), "protocol", "{envelope}");
}

#[test]
fn oc10_server_death_is_server_lost_and_never_resends() {
    let mut events = begin_events();
    events.push(step_end("$INPUT:a", 11, 0.25));
    events.push(json!({"pause_ms":50}));
    events.push(json!({"exit":true}));
    let (envelope, requests) = one(events);
    assert_eq!(class(&envelope), "server_lost", "{envelope}");
    assert_eq!(envelope["usage"]["provenance"], "unavailable", "{envelope}");
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["target"].as_str().is_some_and(|p| p.ends_with("/prompt")))
            .count(),
        1
    );
}

#[test]
fn oc10_eof_with_live_process_is_unknown_and_usage_unavailable() {
    let mut events = begin_events();
    events.push(step_end("$INPUT:a", 11, 0.25));
    events.push(json!({"close":true}));
    let (envelope, _) = one(events);
    assert_eq!(envelope["state"], "unknown", "{envelope}");
    assert_eq!(envelope["usage"]["provenance"], "unavailable", "{envelope}");
}

#[test]
fn oc10_terminal_before_loss_is_kept() {
    let mut events = success_events("retained");
    events.push(json!({"pause_ms":30}));
    events.push(json!({"close":true}));
    let (envelope, _) = one(events);
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["final_text"], "retained", "{envelope}");
}

#[test]
fn oc11_step_samples_supersede_and_cumulative_usage_is_excluded() {
    let mut events = begin_events();
    events.extend([
        step_end("$INPUT:a", 10, 0.125),
        step_end("$INPUT:a", 17, 0.25),
        event(
            "session.usage.updated",
            json!({"tokens":{"input":9000,"output":9000},"cost":999}),
        ),
        event("session.execution.succeeded", json!({})),
    ]);
    let (envelope, _) = one(events);
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["usage"]["input_tokens"], 17, "{envelope}");
    assert_eq!(envelope["usage"]["cached_input_tokens"], 3, "{envelope}");
    assert_eq!(envelope["usage"]["output_tokens"], 7, "{envelope}");
    assert_eq!(
        envelope["usage"]["reasoning_output_tokens"], 2,
        "{envelope}"
    );
    assert_eq!(envelope["usage"]["scope"], "turn", "{envelope}");
    assert_eq!(envelope["usage"]["provenance"], "reported", "{envelope}");
    assert_eq!(envelope["cost"]["usd"], 0.25, "{envelope}");
}

#[test]
fn oc11_compaction_is_vendor_interval_with_warning() {
    let mut events = begin_events();
    events.extend([
        step_end("$INPUT:a",11,0.25),
        event("session.compaction.ended",json!({"inputID":"msg_compact", "tokens":{"input":5,"output":7,"reasoning":2,"cache":{"read":3,"write":4}}, "cost":0.5})),
        event("session.execution.succeeded",json!({})),
    ]);
    let (envelope, _) = one(events);
    assert_eq!(envelope["usage"]["input_tokens"], 16, "{envelope}");
    assert_eq!(envelope["usage"]["scope"], "vendor_interval", "{envelope}");
    assert_eq!(envelope["cost"]["scope"], "vendor_interval", "{envelope}");
    assert!(
        envelope["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["code"] == "usage_interval_unverified"),
        "{envelope}"
    );
}

#[test]
fn oc11_vendor_error_class_hints_never_return_vendor_text() {
    for (error, expected) in [
        (json!({"type":"provider.auth"}), "auth"),
        (json!({"type":"other","status":403}), "auth"),
        (json!({"type":"provider.rate-limit"}), "rate_limit"),
        (json!({"type":"other","status":429}), "rate_limit"),
        (json!({"type":"provider.quota"}), "budget_exceeded"),
        (json!({"type":"provider.no-route"}), "vendor_error"),
    ] {
        let mut error = error;
        error["message"] = json!("synthetic-vendor-private-text");
        let mut events = begin_events();
        events.push(event("session.execution.failed", json!({"error":error})));
        let (envelope, _) = one(events);
        assert_eq!(class(&envelope), expected, "{envelope}");
        assert!(
            !envelope
                .to_string()
                .contains("synthetic-vendor-private-text"),
            "{envelope}"
        );
    }
}

/// Setup observations precede this turn's admission window (`OpenCode` §7.2).
#[test]
fn oc11_session_creation_events_do_not_taint_the_first_turn_usage() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    let response = json!({
        "status":200,
        "json":core_opencode::session_info(&cwd,"default",&core_opencode::rules(false)),
        "emit_before_response":true,
        "sleep_ms":150,
        "emit":[
            event("session.created",json!({"sessionID":SES})),
            event("session.execution.started",json!({"sessionID":SES})),
            event("session.execution.succeeded",json!({"sessionID":SES}))
        ]
    });
    case.fixture(&with_route(
        server(&cwd, None),
        route("POST", "/api/session", json!([response])),
    ));
    let envelope = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let result = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        result
    });
    run(case.all_gone());
    let envelope = envelope.unwrap();
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["usage"]["provenance"], "reported", "{envelope}");
    assert_eq!(envelope["usage"]["input_tokens"], 11, "{envelope}");
    let frames = case.frames();
    assert_eq!(frames[0]["type"], "session.created", "{frames:?}");
    assert_eq!(frames[1]["type"], "session.execution.started", "{frames:?}");
    assert_eq!(
        frames[2]["type"], "session.execution.succeeded",
        "{frames:?}"
    );
    let prompts = case.requests_to("POST", &format!("/api/session/{SES}/prompt"));
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0]["received_ms"].as_u64().unwrap() >= frames[2]["written_ms"].as_u64().unwrap(),
        "setup execution must precede turn submission: {prompts:?}, {frames:?}"
    );
}

/// Rejections in another session cannot poison a later turn (`OpenCode` §12).
#[test]
fn oc11_unrelated_session_rejection_does_not_taint_successor_usage() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    let response = json!({
        "status":200,
        "json":core_opencode::session_info(&cwd,"default",&core_opencode::rules(false)),
        "emit_before_response":true,
        "sleep_ms":150,
        "emit":[event("session.step.ended",json!({
            "sessionID":"ses_foreign", "assistantMessageID":"msg_foreign", "finish":"stop",
            "tokens":{"input":300,"output":7}, "cost":9
        }))]
    });
    case.fixture(&with_route(
        server(&cwd, None),
        route("GET", &format!("/api/session/{SES}"), json!([response])),
    ));
    let (first, second) = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let first = daemon.wait_result(&session, 1).await;
        daemon.try_resume(&session, &json!({})).await.unwrap();
        let second = daemon.wait_result(&session, 2).await;
        daemon.stop().await;
        (first, second)
    });
    run(case.all_gone());
    for envelope in [first, second] {
        let envelope = envelope.unwrap();
        assert_eq!(envelope["state"], "completed", "{envelope}");
        assert_eq!(envelope["usage"]["provenance"], "reported", "{envelope}");
        assert_eq!(envelope["usage"]["input_tokens"], 11, "{envelope}");
    }
    assert_eq!(
        case.starts(),
        1,
        "the rejection stays in the same generation"
    );
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .len(),
        2
    );
}

#[test]
fn oc05_running_execution_ends_before_successor_prompt_is_sent() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    let mut fixture = server(&cwd, None);
    fixture["routes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["path"] == "/api/event")
        .unwrap()["sse"]["events"] = json!([{"type":"server.connected","properties":{}},event("session.execution.started", json!({"sessionID":SES}))]);
    // The held execution is server state, independent of any turn's lane.
    // A setup readback releases it, so the dispatch rule must re-observe state.
    let mut response = json!({"status":200,"json":core_opencode::session_info(&cwd,"default",&core_opencode::rules(false)),
        "emit":[{"pause_ms":100},event("session.execution.succeeded",json!({"sessionID":SES}))]});
    response["emit_before_response"] = json!(true);
    fixture = with_route(fixture, route("POST", "/api/session", json!([response])));
    case.fixture(&fixture);
    let envelope = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let result = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        result
    });
    run(case.all_gone());
    let envelope = envelope.unwrap();
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["usage"]["provenance"], "reported", "{envelope}");
    assert_eq!(envelope["usage"]["input_tokens"], 11, "{envelope}");
    let prompts = case.requests_to("POST", &format!("/api/session/{SES}/prompt"));
    assert_eq!(prompts.len(), 1);
    let frames = case.frames();
    let release = frames
        .iter()
        .find(|frame| frame["type"] == "session.execution.succeeded")
        .unwrap();
    assert!(
        prompts[0]["received_ms"].as_u64().unwrap() >= release["written_ms"].as_u64().unwrap(),
        "prompt before predecessor terminal: {prompts:?}, {frames:?}"
    );
}

#[test]
fn oc05_running_execution_rejects_after_thirty_seconds_without_prompt() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    let mut fixture = server(&cwd, None);
    fixture["routes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["path"] == "/api/event")
        .unwrap()["sse"]["events"] = json!([{"type":"server.connected","properties":{}},event("session.execution.started", json!({"sessionID":SES}))]);
    case.fixture(&fixture);
    let (envelope, elapsed) = run(async {
        let daemon = case.daemon();
        let started = tokio::time::Instant::now();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let result = daemon.wait_result(&session, 1).await;
        let elapsed = started.elapsed();
        daemon.stop().await;
        (result, elapsed)
    });
    run(case.all_gone());
    let envelope = envelope.unwrap();
    assert_eq!(class(&envelope), "submit_failed", "{envelope}");
    assert_eq!(
        envelope["failure"]["vendor_code"], "session_busy",
        "{envelope}"
    );
    assert!(elapsed >= std::time::Duration::from_secs(30), "{elapsed:?}");
    assert!(elapsed < std::time::Duration::from_secs(40), "{elapsed:?}");
    assert!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .is_empty()
    );
}

/// Interleave two accepted session executions in a single stream. A late
/// terminal and text from either session cannot settle its neighbor.
#[test]
fn oc06_interleaved_sessions_keep_text_and_terminals_with_their_owner() {
    const SECOND: &str = "ses_via0002";
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    let mut fixture = server(&cwd, None);
    let first_info = core_opencode::session_info(&cwd, "default", &core_opencode::rules(false));
    let mut second_info = first_info.clone();
    second_info["data"]["id"] = json!(SECOND);
    fixture = with_route(
        fixture,
        route(
            "GET",
            &format!("/api/session/{SECOND}"),
            json!([{"status":200,"json":second_info}]),
        ),
    );
    fixture = with_route(
        fixture,
        route(
            "POST",
            "/api/session",
            json!([
                {"status":200,"json":first_info},{"status":200,"json":second_info}
            ]),
        ),
    );
    let mut first_begin = begin_events();
    first_begin[3]["data"]["assistantMessageID"] = json!("msg_first");
    fixture = with_route(fixture, prompt_route(first_begin));
    let mut interleaved = begin_events();
    interleaved.extend([
        event(
            "session.text.ended",
            json!({"sessionID":SES,"assistantMessageID":"msg_first","ordinal":0,"text":"first"}),
        ),
        event("session.execution.succeeded", json!({"sessionID":SES})),
        event(
            "session.text.ended",
            json!({"assistantMessageID":"$INPUT:a","ordinal":0,"text":"second"}),
        ),
        step_end("$INPUT:a", 11, 0.25),
        event("session.execution.succeeded", json!({})),
    ]);
    fixture = with_route(
        fixture,
        route(
            "POST",
            &format!("/api/session/{SECOND}/prompt"),
            json!([prompt_response(interleaved)]),
        ),
    );
    case.fixture(&fixture);
    let (accepted, first, second) = run(async {
        let daemon = case.daemon();
        let (a, _) = daemon.spawn(&cwd, &json!({})).await;
        // Core's acceptance confirms the first input reached the owning
        // delivery before the second releases its interleaved frames.
        let by = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut accepted = true;
        while daemon.status(&a).await["active_turn"]["phase"] != "accepted" {
            if tokio::time::Instant::now() >= by {
                accepted = false;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let (b, _) = daemon.spawn(&cwd, &json!({})).await;
        let (first, second) = tokio::join!(daemon.wait_result(&a, 1), daemon.wait_result(&b, 1));
        daemon.stop().await;
        (accepted, first, second)
    });
    run(case.all_gone());
    assert!(
        accepted,
        "the first turn was not accepted within five seconds"
    );
    assert!(
        first.is_ok() && second.is_ok(),
        "first: {first:?}; second: {second:?}; requests: {:?}",
        case.requests()
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first["state"], "completed", "{first}");
    assert_eq!(second["state"], "completed", "{second}");
    assert_eq!(first["final_text"], "first", "{first}");
    assert_eq!(second["final_text"], "second", "{second}");
}

#[test]
fn oc10_two_active_turns_share_one_loss_report_and_group_absence() {
    const SECOND: &str = "ses_via0002";
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    let info = core_opencode::session_info(&cwd, "default", &core_opencode::rules(false));
    let mut second_info = info.clone();
    second_info["data"]["id"] = json!(SECOND);
    let mut fixture = with_route(
        server(&cwd, None),
        route(
            "POST",
            "/api/session",
            json!([
                {"status":200,"json":info},{"status":200,"json":second_info}
            ]),
        ),
    );
    fixture = with_route(
        fixture,
        route(
            "GET",
            &format!("/api/session/{SECOND}"),
            json!([{"status":200,"json":second_info}]),
        ),
    );
    fixture = with_route(fixture, prompt_route(begin_events()));
    let mut crash = begin_events();
    crash.extend([json!({"pause_ms":50}), json!({"exit":true})]);
    fixture = with_route(
        fixture,
        route(
            "POST",
            &format!("/api/session/{SECOND}/prompt"),
            json!([prompt_response(crash)]),
        ),
    );
    case.fixture(&fixture);
    let (accepted, first, second) = run(async {
        let daemon = case.daemon();
        let (a, _) = daemon.spawn(&cwd, &json!({})).await;
        let by = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut accepted = true;
        while daemon.status(&a).await["active_turn"]["phase"] != "accepted" {
            if tokio::time::Instant::now() >= by {
                accepted = false;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let (b, _) = daemon.spawn(&cwd, &json!({})).await;
        let (first, second) = tokio::join!(daemon.wait_result(&a, 1), daemon.wait_result(&b, 1));
        daemon.stop().await;
        (accepted, first, second)
    });
    run(case.all_gone());
    assert!(
        accepted,
        "the first turn was not accepted within five seconds"
    );
    assert!(
        first.is_ok() && second.is_ok(),
        "first: {first:?}; second: {second:?}; requests: {:?}",
        case.requests()
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(class(&first), "server_lost", "{first}");
    assert_eq!(class(&second), "server_lost", "{second}");
    assert_eq!(first["leftovers"]["scope"], "server", "{first}");
    assert_eq!(first["leftovers"], second["leftovers"]);
    assert_eq!(first["leftovers"]["processes"], json!([]), "{first}");
    assert_eq!(first["leftovers"]["total"], 0, "{first}");
    assert!(first["leftovers"]["incomplete"].is_boolean(), "{first}");
    assert_eq!(case.starts(), 1);
    assert_eq!(
        case.proven_server_groups(),
        1,
        "shared server group proven absent"
    );
}

#[test]
fn oc11_unattributed_preterminal_event_makes_retained_usage_unavailable() {
    let mut events = begin_events();
    events.extend([
        step_end("$INPUT:a",11,0.25),
        event("session.step.ended",json!({"assistantMessageID":"msg_unowned", "finish":"stop", "tokens":{"input":300,"output":7}, "cost":9})),
        event("session.execution.succeeded",json!({})),
    ]);
    let (envelope, _) = one(events);
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["usage"]["provenance"], "unavailable", "{envelope}");
    for field in [
        "input_tokens",
        "cached_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ] {
        assert!(envelope["usage"][field].is_null(), "{envelope}");
    }
    assert_eq!(envelope["cost"]["provenance"], "unavailable", "{envelope}");
}

/// A real process crash after Core reports acceptance must recover unknown and
/// leave the caller-owned input untouched. Only the child we spawned is killed.
#[test]
fn oc10_daemon_crash_recovers_unknown_without_resending() {
    if let Some(root) = std::env::var_os("VIA_OC_CRASH_ROOT") {
        let root = std::path::PathBuf::from(root);
        run(async {
            let daemon = core_opencode::Daemon::crash_fixture(&root);
            let cwd = root.join("cwd/a");
            let (session, _) = daemon.spawn(cwd.to_str().unwrap(), &json!({})).await;
            let by = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            while daemon.status(&session).await["active_turn"]["phase"] != "accepted" {
                assert!(
                    tokio::time::Instant::now() < by,
                    "the crash fixture accepts"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let pending = tempfile::NamedTempFile::new_in(&root).unwrap();
            std::fs::write(pending.path(), json!({"session":session}).to_string()).unwrap();
            pending.persist(root.join("crash-accepted.json")).unwrap();
            std::future::pending::<()>().await;
        });
        return;
    }
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&with_route(
        server(&cwd, None),
        prompt_route(begin_events()),
    ));
    let mut child = case
        .crash_child("core_opencode_turns::oc10_daemon_crash_recovers_unknown_without_resending");
    let accepted = run(async {
        let by = tokio::time::Instant::now() + std::time::Duration::from_secs(35);
        loop {
            if let Ok(raw) = std::fs::read(case.crash_marker()) {
                break Some(serde_json::from_slice::<Value>(&raw).unwrap());
            }
            if child.try_wait().unwrap().is_some() || tokio::time::Instant::now() >= by {
                break None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    });
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    let _ = child.wait().unwrap();
    let recovered = run(async {
        let daemon = case.daemon();
        daemon.recover().await;
        let envelope = if let Some(accepted) = &accepted {
            let session =
                via_core::SessionId::try_from(accepted["session"].as_str().unwrap()).unwrap();
            Some(daemon.wait_result(&session, 1).await)
        } else {
            None
        };
        daemon.stop().await;
        envelope
    });
    run(case.all_gone());
    let envelope = recovered
        .expect("child durably accepted before its crash")
        .unwrap();
    assert_eq!(envelope["state"], "unknown", "{envelope}");
    assert_eq!(class(&envelope), "daemon_restart", "{envelope}");
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .len(),
        1
    );
}

/// The fallback must stop and reap the exact owned child even on unwind.
#[test]
fn owned_child_guard_stops_and_reaps_on_unwind() {
    let case = Case::new(&json!({}));
    let child = case.owned_child(std::process::Command::new("/bin/sleep").arg("1"));
    let pid = child.id();
    let started = std::time::Instant::now();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _owned = child;
        panic!("synthetic fixture unwind");
    }));
    let elapsed = started.elapsed();
    assert!(caught.is_err());
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "owned child reaped"
    );
    let pid_file = case.crash_marker().with_extension("pid");
    std::fs::write(&pid_file, pid.to_string()).unwrap();
    let mut probe = case.owned_child(
        std::process::Command::new("pgrep")
            .arg("-F")
            .arg(&pid_file)
            .arg("."),
    );
    assert_eq!(
        probe.wait().unwrap().code(),
        Some(1),
        "owned PID absent from pgrep"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(700),
        "guard waited for natural exit instead of stopping child: {elapsed:?}"
    );
}
