//! `OpenCode` §7.4, §8, §11: controls and request outcomes through real Core/Host/Wire.
use super::{core_opencode, run};
use core_opencode::{
    Case, Daemon, SES, begin_events, class, event, prompt_response, route, server, success_events,
    with_route,
};
use serde_json::json;
use std::time::Duration;
use via_core::SessionId;

/// A bounded public acceptance checkpoint; assertions follow owned cleanup.
async fn accepted(daemon: &Daemon, session: &SessionId) -> bool {
    let by = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if daemon.status(session).await["active_turn"]["phase"] == "accepted" {
            return true;
        }
        if tokio::time::Instant::now() >= by {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A stop during create prevents the prompt from reaching the vendor (§7.4).
#[test]
fn oc08_cancel_before_prompt_send_withdraws_without_vendor_stop() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("before-send");
    let mut fixture = server(&cwd, None);
    fixture = with_route(
        fixture,
        route(
            "POST",
            "/api/session",
            json!([
                {"status":200,"sleep_ms":300,
                 "json":core_opencode::session_info(&cwd,"default",&core_opencode::rules(false))}
            ]),
        ),
    );
    case.fixture(&fixture);
    let (setup_seen, ordered, envelope) = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":2500}}))
            .await;
        let by = tokio::time::Instant::now() + Duration::from_secs(2);
        let setup_seen = loop {
            if !case.creates().is_empty() {
                break true;
            }
            if tokio::time::Instant::now() >= by {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let ordered = daemon.cancel(&session, 600).await;
        let envelope = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        (setup_seen, ordered, envelope)
    });
    run(case.all_gone());
    assert!(
        setup_seen && ordered.is_ok(),
        "setup={setup_seen}, cancel={ordered:?}"
    );
    let envelope = envelope.unwrap();
    assert_eq!(envelope["state"], "cancelled", "{envelope}");
    assert!(
        envelope["timestamps"]["accepted_at"].is_null(),
        "{envelope}"
    );
    assert!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .is_empty()
    );
    assert!(
        case.requests_to("DELETE", &format!("/api/session/{SES}/inbox/"))
            .is_empty()
    );
    assert!(
        case.requests_to("POST", &format!("/api/session/{SES}/interrupt"))
            .is_empty()
    );
}

/// Native queued-input cancellation; DELETE is acknowledged only by the stream.
#[test]
fn oc08_queued_input_cancel_is_native_terminal_without_interrupt() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("cancel");
    let fixture = with_route(
        server(&cwd, None),
        core_opencode::prompt_route(vec![event(
            "session.inbox.enqueued",
            json!({"inboxID":"$INPUT"}),
        )]),
    );
    case.fixture(&with_route(
        fixture,
        route(
            "DELETE",
            &format!("/api/session/{SES}/inbox/*"),
            json!([{"status":204,"emit":[
                event("session.inbox.cancelled",json!({"inboxID":"$INPUT"}))
            ]}]),
        ),
    ));
    let (ready, ordered, envelope) = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":2500}}))
            .await;
        let ready = accepted(&daemon, &session).await;
        let ordered = daemon.cancel(&session, 600).await;
        let envelope = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        (ready, ordered, envelope)
    });
    run(case.all_gone());
    assert!(
        ready && ordered.is_ok(),
        "accepted={ready}, cancel={ordered:?}"
    );
    let envelope = envelope.unwrap();
    assert_eq!(envelope["state"], "cancelled", "{envelope}");
    assert_eq!(envelope["cancel"]["outcome"], "acknowledged", "{envelope}");
    assert_eq!(envelope["cancel"]["cleanup"], "quiescent", "{envelope}");
    assert_eq!(
        envelope["vendor_stop_reason"], "input_cancelled",
        "{envelope}"
    );
    assert_eq!(envelope["usage"]["provenance"], "unavailable", "{envelope}");
    assert_eq!(
        case.requests_to("DELETE", &format!("/api/session/{SES}/inbox/"))
            .len(),
        1
    );
    assert!(
        case.requests_to("POST", &format!("/api/session/{SES}/interrupt"))
            .is_empty()
    );
}

/// First delivery winning the DELETE race requires one interrupt, never a resend.
#[test]
fn oc08_delivery_wins_inbox_cancel_race_then_interrupts_once() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("race");
    let mut fixture = with_route(
        server(&cwd, None),
        core_opencode::prompt_route(vec![event(
            "session.inbox.enqueued",
            json!({"inboxID":"$INPUT"}),
        )]),
    );
    fixture = with_route(
        fixture,
        route(
            "DELETE",
            &format!("/api/session/{SES}/inbox/*"),
            json!([
                {"status":204,"emit":[
                    event("session.execution.started",json!({})),
                    event("session.inbox.delivered",json!({"inboxID":"$INPUT"}))
                ]}
            ]),
        ),
    );
    fixture = with_route(
        fixture,
        route(
            "POST",
            &format!("/api/session/{SES}/interrupt"),
            json!([
                {"status":200,"json":{"interrupted":true},"emit":[
                    event("session.execution.interrupted",json!({"reason":"user"}))
                ]}
            ]),
        ),
    );
    case.fixture(&fixture);
    let (ready, ordered, envelope) = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":2500}}))
            .await;
        let ready = accepted(&daemon, &session).await;
        let ordered = daemon.cancel(&session, 700).await;
        let envelope = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        (ready, ordered, envelope)
    });
    run(case.all_gone());
    assert!(
        ready && ordered.is_ok(),
        "accepted={ready}, cancel={ordered:?}"
    );
    let envelope = envelope.unwrap();
    assert_eq!(envelope["state"], "cancelled", "{envelope}");
    assert_eq!(envelope["cancel"]["outcome"], "acknowledged", "{envelope}");
    assert_eq!(envelope["cancel"]["cleanup"], "quiescent", "{envelope}");
    assert_eq!(
        case.requests_to("DELETE", &format!("/api/session/{SES}/inbox/"))
            .len(),
        1
    );
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/interrupt"))
            .len(),
        1
    );
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .len(),
        1
    );
}

/// The order's force deadline preserves the server and accepts a native late revision.
#[test]
fn oc08_force_unknown_revises_after_late_input_cancellation() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("force");
    let fixture = with_route(
        server(&cwd, None),
        core_opencode::prompt_route(vec![event(
            "session.inbox.enqueued",
            json!({"inboxID":"$INPUT"}),
        )]),
    );
    case.fixture(&with_route(
        fixture,
        route(
            "DELETE",
            &format!("/api/session/{SES}/inbox/*"),
            json!([
                {"status":204,"emit":[{"pause_ms":500},
                    event("session.inbox.cancelled",json!({"inboxID":"$INPUT"}))
                ]}
            ]),
        ),
    ));
    let (ready, ordered, initial, revised) = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":2500}}))
            .await;
        let ready = accepted(&daemon, &session).await;
        let ordered = daemon.cancel(&session, 100).await;
        let initial = daemon.wait_result(&session, 1).await;
        let by = tokio::time::Instant::now() + Duration::from_secs(2);
        let revised = loop {
            let envelope = daemon.wait_result(&session, 1).await;
            if envelope
                .as_ref()
                .is_ok_and(|value| value["revision"].as_u64().unwrap_or(0) > 0)
                || tokio::time::Instant::now() >= by
            {
                break envelope;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        daemon.stop().await;
        (ready, ordered, initial, revised)
    });
    run(case.all_gone());
    assert!(
        ready && ordered.is_ok(),
        "accepted={ready}, cancel={ordered:?}"
    );
    let initial = initial.unwrap();
    let revised = revised.unwrap();
    assert_eq!(initial["state"], "unknown", "{initial}");
    assert_eq!(initial["revision"], 0, "{initial}");
    assert_eq!(revised["state"], "cancelled", "{revised}");
    assert_eq!(revised["revision"], 1, "{revised}");
    assert_eq!(case.starts(), 1);
    assert_eq!(
        case.requests_to("DELETE", &format!("/api/session/{SES}/inbox/"))
            .len(),
        1
    );
}

#[test]
fn oc09_prompt_400_and_404_prove_nonacceptance_and_release_successor() {
    for (status, tag) in [(400, "InvalidRequestError"), (404, "SessionNotFoundError")] {
        let case = Case::new(&json!({}));
        let cwd = case.cwd("statuses");
        case.fixture(&with_route(
            server(&cwd, None),
            route(
                "POST",
                &format!("/api/session/{SES}/prompt"),
                json!([
                    {"status":status,"json":{"_tag":tag}},
                    prompt_response(success_events("successor"))
                ]),
            ),
        ));
        let (first, receipt, second) = run(async {
            let daemon = case.daemon();
            let (session, _) = daemon.spawn(&cwd, &json!({})).await;
            let first = daemon.wait_result(&session, 1).await;
            let receipt = daemon.try_resume(&session, &json!({})).await;
            let second = if receipt.is_ok() {
                Some(daemon.wait_result(&session, 2).await)
            } else {
                None
            };
            daemon.stop().await;
            (first, receipt, second)
        });
        run(case.all_gone());
        let first = first.unwrap();
        assert_eq!(class(&first), "submit_failed", "status={status}: {first}");
        assert!(first["timestamps"]["accepted_at"].is_null(), "{first}");
        assert!(receipt.is_ok(), "{receipt:?}");
        let second = second.unwrap().unwrap();
        assert_eq!(second["state"], "completed", "{second}");
        assert_eq!(second["final_text"], "successor", "{second}");
        assert_eq!(case.starts(), 1);
        assert_eq!(
            case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
                .len(),
            2
        );
    }
}

#[test]
fn oc09_prompt_401_fails_generation_protocol() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("unauthorized");
    case.fixture(&with_route(
        server(&cwd, None),
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            json!([{"status":401,"json":{"_tag":"UnauthorizedError"}}]),
        ),
    ));
    let envelope = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let envelope = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        envelope
    });
    run(case.all_gone());
    let envelope = envelope.unwrap();
    assert_eq!(class(&envelope), "protocol", "{envelope}");
    assert!(
        envelope["timestamps"]["accepted_at"].is_null(),
        "{envelope}"
    );
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .len(),
        1
    );
}

/// A conclusive stream terminal precedes the inconclusive HTTP response in lane order.
#[test]
fn oc09_inconclusive_response_keeps_earlier_terminal_and_drains_to_fresh_server() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("drain");
    case.fixture(&with_route(
        server(&cwd, None),
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            json!([
                {"status":409,"json":{"_tag":"ConflictError"},"emit_before_response":true,
                 "sleep_ms":100,"emit":success_events("terminal first")}
            ]),
        ),
    ));
    let (first, fresh) = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let first = daemon.wait_result(&session, 1).await;
        // The next generation loads a fresh response script, never resending turn one.
        case.fixture(&server(&cwd, None));
        let (other, _) = daemon.spawn(&cwd, &json!({})).await;
        let fresh = daemon.wait_result(&other, 1).await;
        daemon.stop().await;
        (first, fresh)
    });
    run(case.all_gone());
    let first = first.unwrap();
    let fresh = fresh.unwrap();
    assert_eq!(first["state"], "completed", "{first}");
    assert_eq!(first["final_text"], "terminal first", "{first}");
    assert_eq!(fresh["state"], "completed", "{fresh}");
    assert_eq!(fresh["final_text"], "done", "{fresh}");
    assert_eq!(case.starts(), 2, "drain retires the first generation");
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .len(),
        2
    );
}

/// Permission denial travels independently; duplicate IDs are declined only once.
#[test]
fn oc07_permission_decline_is_deduplicated_and_ends_owned_execution() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("permission");
    let asked = event(
        "permission.asked",
        json!({
            "id":"perm_one","action":"bash","resources":["*"],
            "source":{"messageID":"$INPUT:a","id":"call_permission"}
        }),
    );
    let mut events = begin_events();
    events.extend([
        event(
            "session.tool.called",
            json!({
                "id":"call_permission","assistantMessageID":"$INPUT:a","name":"bash"
            }),
        ),
        asked.clone(),
        asked,
    ]);
    let fixture = with_route(server(&cwd, None), core_opencode::prompt_route(events));
    case.fixture(&with_route(
        fixture,
        route(
            "POST",
            &format!("/api/session/{SES}/permission/perm_one/reply"),
            json!([
                {"status":204,"emit":[
                    event("permission.replied",json!({"requestID":"perm_one","reply":"reject"})),
                    event("session.tool.failed",json!({
                        "id":"call_permission","name":"bash","error":{"type":"permission.rejected"}
                    })),
                    event("session.execution.interrupted",json!({"reason":"shutdown"}))
                ]}
            ]),
        ),
    ));
    let envelope = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":1800}}))
            .await;
        let envelope = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        envelope
    });
    run(case.all_gone());
    let envelope = envelope.unwrap();
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["auto_declined_requests_total"], 1, "{envelope}");
    assert_eq!(envelope["denied_actions_total"], 0, "{envelope}");
    let replies = case.requests_to("POST", &format!("/api/session/{SES}/permission/"));
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["body"], json!({"decision":"reject"}));
    assert!(
        case.requests_to("POST", &format!("/api/session/{SES}/interrupt"))
            .is_empty()
    );
}

/// An unattributed foreign request is declined without crediting the live root (§11).
#[test]
fn oc07_unattributed_permission_is_declined_without_turn_credit() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("foreign-request");
    let mut events = begin_events();
    events.push(event(
        "permission.asked",
        json!({
            "sessionID":"ses_foreign","id":"perm_foreign","action":"bash","resources":["*"],
            "source":{"messageID":"msg_foreign","id":"call_foreign"}
        }),
    ));
    let fixture = with_route(server(&cwd, None), core_opencode::prompt_route(events));
    case.fixture(&with_route(fixture, route(
        "POST", "/api/session/ses_foreign/permission/perm_foreign/reply", json!([
            {"status":204,"emit":[event("session.execution.succeeded",json!({"sessionID":SES}))]}
        ]),
    )));
    let envelope = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":1800}}))
            .await;
        let envelope = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        envelope
    });
    run(case.all_gone());
    let envelope = envelope.unwrap();
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["auto_declined_requests_total"], 0, "{envelope}");
    let replies = case.requests_to("POST", "/api/session/ses_foreign/permission/");
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["body"], json!({"decision":"reject"}));
}

/// Cancelling a form does not authorize the permission-decline terminal row (§7.3).
#[test]
fn oc07_form_decline_keeps_shutdown_as_vendor_error() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("form");
    let mut events = begin_events();
    events.push(event(
        "form.created",
        json!({
            "form":{"sessionID":"$SESSION","id":"form_one","title":"synthetic-private-title"}
        }),
    ));
    let fixture = with_route(server(&cwd, None), core_opencode::prompt_route(events));
    case.fixture(&with_route(
        fixture,
        route(
            "DELETE",
            &format!("/api/session/{SES}/form/form_one"),
            json!([
                {"status":204,"emit":[
                    event("form.cancelled",json!({"id":"form_one"})),
                    event("session.execution.interrupted",json!({"reason":"shutdown"}))
                ]}
            ]),
        ),
    ));
    let envelope = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":1800}}))
            .await;
        let envelope = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        envelope
    });
    run(case.all_gone());
    let envelope = envelope.unwrap();
    assert_eq!(class(&envelope), "vendor_error", "{envelope}");
    assert_eq!(envelope["auto_declined_requests_total"], 1, "{envelope}");
    assert!(
        !envelope.to_string().contains("synthetic-private-title"),
        "{envelope}"
    );
    let requests = case.requests_to("DELETE", &format!("/api/session/{SES}/form/"));
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["target"],
        format!("/api/session/{SES}/form/form_one?message=declined%20by%20VIA")
    );
}

/// Drain fences future work while both already-sent sessions retain their stream (§8).
#[test]
fn oc09_drain_preserves_an_unrelated_already_sent_turn() {
    const OTHER: &str = "ses_via0002";
    let case = Case::new(&json!({}));
    let cwd = case.cwd("shared-drain");
    let info = core_opencode::session_info(&cwd, "default", &core_opencode::rules(false));
    let mut other = info.clone();
    other["data"]["id"] = json!(OTHER);
    let mut fixture = with_route(
        server(&cwd, None),
        route(
            "POST",
            "/api/session",
            json!([
                {"status":200,"json":info},{"status":200,"json":other}
            ]),
        ),
    );
    let mut first_begin = begin_events();
    first_begin[3]["data"]["assistantMessageID"] = json!("msg_first");
    fixture = with_route(fixture, core_opencode::prompt_route(first_begin));
    let mut interleaved = begin_events();
    interleaved.extend([
        json!({"pause_ms":200}),
        event(
            "session.text.ended",
            json!({
                "sessionID":SES,"assistantMessageID":"msg_first","ordinal":0,"text":"other survives"
            }),
        ),
        event("session.execution.succeeded", json!({"sessionID":SES})),
    ]);
    interleaved.extend(success_events("draining turn survives").into_iter().skip(4));
    fixture = with_route(
        fixture,
        route(
            "POST",
            &format!("/api/session/{OTHER}/prompt"),
            json!([
                {"status":409,"json":{"_tag":"ConflictError"},"emit_before_response":true,
                 "sleep_ms":50,"emit":interleaved}
            ]),
        ),
    );
    case.fixture(&fixture);
    let (ready, first, second) = run(async {
        let daemon = case.daemon();
        let (a, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":2500}}))
            .await;
        let ready = accepted(&daemon, &a).await;
        let (b, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":2500}}))
            .await;
        let (first, second) = tokio::join!(daemon.wait_result(&a, 1), daemon.wait_result(&b, 1));
        daemon.stop().await;
        (ready, first, second)
    });
    run(case.all_gone());
    assert!(ready, "first session did not become accepted");
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first["state"], "completed", "{first}");
    assert_eq!(first["final_text"], "other survives", "{first}");
    assert_eq!(second["state"], "completed", "{second}");
    assert_eq!(second["final_text"], "draining turn survives", "{second}");
    assert_eq!(case.starts(), 1);
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .len(),
        1
    );
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{OTHER}/prompt"))
            .len(),
        1
    );
}

/// Already-delivered input skips DELETE; interrupt=false keeps its natural terminal (§7.4).
#[test]
fn oc08_delivered_interrupt_false_keeps_the_natural_terminal() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("natural-race");
    let mut began = begin_events();
    began[3]["data"]["assistantMessageID"] = json!("msg_natural");
    began.push(event(
        "session.tool.called",
        json!({
            "id":"call_natural","assistantMessageID":"msg_natural","name":"bash"
        }),
    ));
    // An owned tool appearing in public progress proves delivery, even while
    // the original prompt response is still pending.
    let mut initial = prompt_response(began);
    initial["emit_before_response"] = json!(true);
    initial["sleep_ms"] = json!(100);
    let fixture = with_route(
        server(&cwd, None),
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            json!([initial]),
        ),
    );
    case.fixture(&with_route(
        fixture,
        route(
            "POST",
            &format!("/api/session/{SES}/interrupt"),
            json!([
                {"status":200,"json":{"interrupted":false},"emit":[
                    event("session.tool.success",json!({"id":"call_natural","name":"bash"})),
                event("session.text.ended",json!({
                        "assistantMessageID":"msg_natural","ordinal":0,"text":"natural completion"
                    })),
                    core_opencode::step_end("msg_natural",11,0.25),
                    event("session.execution.succeeded",json!({}))
                ]}
            ]),
        ),
    ));
    let (ready, delivered_seen, ordered, envelope) = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":2500}}))
            .await;
        let ready = accepted(&daemon, &session).await;
        let by = tokio::time::Instant::now() + Duration::from_secs(2);
        let delivered_seen = loop {
            if daemon.status(&session).await["progress"]["running_tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool == "bash"))
            {
                break true;
            }
            if tokio::time::Instant::now() >= by {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let ordered = daemon.cancel(&session, 700).await;
        let envelope = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        (ready, delivered_seen, ordered, envelope)
    });
    run(case.all_gone());
    assert!(
        ready && delivered_seen && ordered.is_ok(),
        "accepted={ready}, delivered={delivered_seen}, cancel={ordered:?}"
    );
    let envelope = envelope.unwrap();
    assert_eq!(envelope["state"], "completed", "{envelope}");
    assert_eq!(envelope["final_text"], "natural completion", "{envelope}");
    assert!(
        case.requests_to("DELETE", &format!("/api/session/{SES}/inbox/"))
            .is_empty()
    );
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/interrupt"))
            .len(),
        1
    );
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .len(),
        1
    );
}

/// A failed sent HTTP request uses positive Host exit evidence (§8, §10).
#[test]
fn oc09_prompt_socket_failure_after_byte_with_vendor_exit_is_server_lost() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("http-vendor-death");
    let mut events = begin_events();
    events.extend([
        core_opencode::step_end("$INPUT:a", 11, 0.25),
        json!({"pause_ms":50}),
        json!({"exit":true}),
    ]);
    let mut response = prompt_response(events);
    response["emit_before_response"] = json!(true);
    // The owned fake dies while this handler still owes its first response byte.
    response["sleep_ms"] = json!(500);
    case.fixture(&with_route(
        server(&cwd, None),
        route(
            "POST",
            &format!("/api/session/{SES}/prompt"),
            json!([response]),
        ),
    ));
    let envelope = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":2500}}))
            .await;
        let envelope = daemon.wait_result(&session, 1).await;
        daemon.stop().await;
        envelope
    });
    run(case.all_gone());
    let envelope = envelope.unwrap();
    assert_eq!(class(&envelope), "server_lost", "{envelope}");
    assert_eq!(envelope["leftovers"]["scope"], "server", "{envelope}");
    assert_eq!(envelope["leftovers"]["processes"], json!([]), "{envelope}");
    assert_eq!(envelope["leftovers"]["total"], 0, "{envelope}");
    assert_eq!(
        case.proven_server_groups(),
        1,
        "the owned server group is absent"
    );
    assert_eq!(case.starts(), 1);
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .len(),
        1
    );
}

/// HTTP-limit failure belongs only to the requesting turn (§8, §9).
#[test]
fn oc09_http_limit_fails_its_turn_and_preserves_an_unrelated_sent_turn() {
    // OpenCode §9: ordinary HTTP response body cap, exceeded by one byte.
    const LIMIT_BREACH: usize = 1024 * 1024 + 1;
    const OTHER: &str = "ses_via0002";
    let case = Case::new(&json!({}));
    let cwd = case.cwd("shared-limit");
    let info = core_opencode::session_info(&cwd, "default", &core_opencode::rules(false));
    let mut other = info.clone();
    other["data"]["id"] = json!(OTHER);
    let mut fixture = with_route(
        server(&cwd, None),
        route(
            "POST",
            "/api/session",
            json!([
                {"status":200,"json":info},{"status":200,"json":other}
            ]),
        ),
    );
    let mut first_begin = begin_events();
    first_begin[3]["data"]["assistantMessageID"] = json!("msg_first");
    fixture = with_route(fixture, core_opencode::prompt_route(first_begin));
    let mut interleaved = begin_events();
    interleaved.extend([
        json!({"pause_ms":200}),
        event(
            "session.text.ended",
            json!({
                "sessionID":SES,"assistantMessageID":"msg_first","ordinal":0,"text":"other survives"
            }),
        ),
        event("session.execution.succeeded", json!({"sessionID":SES})),
    ]);
    interleaved.extend(success_events("draining turn survives").into_iter().skip(4));
    fixture = with_route(
        fixture,
        route(
            "POST",
            &format!("/api/session/{OTHER}/prompt"),
            json!([
                {"status":200,"json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},
                 "pad_to":LIMIT_BREACH,"emit_before_response":true,
                 "sleep_ms":50,"emit":interleaved}
            ]),
        ),
    );
    case.fixture(&fixture);
    let (ready, first, second) = run(async {
        let daemon = case.daemon();
        let (a, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":2500}}))
            .await;
        let ready = accepted(&daemon, &a).await;
        let (b, _) = daemon
            .spawn(&cwd, &json!({"deadlines":{"wall_ms":2500}}))
            .await;
        let (first, second) = tokio::join!(daemon.wait_result(&a, 1), daemon.wait_result(&b, 1));
        daemon.stop().await;
        (ready, first, second)
    });
    run(case.all_gone());
    assert!(ready, "first session did not become accepted");
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first["state"], "completed", "{first}");
    assert_eq!(first["final_text"], "other survives", "{first}");
    assert_eq!(second["state"], "failed", "{second}");
    assert_eq!(class(&second), "protocol", "{second}");
    assert_eq!(case.starts(), 1);
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{SES}/prompt"))
            .len(),
        1
    );
    assert_eq!(
        case.requests_to("POST", &format!("/api/session/{OTHER}/prompt"))
            .len(),
        1
    );
}
