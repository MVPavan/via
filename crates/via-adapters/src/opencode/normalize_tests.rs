use tokio::time::Instant;
use via_routes::opencode::events::{
    EventData, ExecutionKind, StepKind, TextKind, Tokens, ToolKind, VendorError,
};

use super::normalize::Normalizer;
use crate::{ClassHint, CostProvenance, DenialKind, Observation, StopReason, VendorTerminalStatus};

fn step(
    id: &str,
    kind: StepKind,
    tokens: Option<Tokens>,
    cost: Option<f64>,
    finish: Option<&str>,
) -> EventData {
    EventData::Step {
        kind,
        assistant_message_id: id.into(),
        tokens,
        cost,
        finish: finish.map(str::to_owned),
    }
}

fn text(id: &str, ordinal: u64, contents: &str) -> EventData {
    EventData::Text {
        kind: TextKind::Ended,
        assistant_message_id: id.into(),
        ordinal,
        text: contents.into(),
    }
}

fn counts(input: u64, output: u64) -> Tokens {
    Tokens {
        input: Some(input),
        output: Some(output),
        reasoning: Some(0),
        cache_read: Some(2),
        cache_write: Some(3),
    }
}

fn terminal(kind: ExecutionKind, error: Option<VendorError>, reason: Option<&str>) -> EventData {
    EventData::Execution {
        kind,
        error,
        reason: reason.map(str::to_owned),
    }
}

#[test]
fn oc08_native_user_interrupt_requires_via_interrupt_evidence() {
    let at = Instant::now();
    let event = terminal(ExecutionKind::Interrupted, None, Some("user"));
    let mut normalizer = Normalizer::new();
    assert_eq!(
        normalizer.terminal(&event, at).unwrap().status,
        VendorTerminalStatus::Failed
    );
    normalizer.note_interrupt_sent();
    let stopped = normalizer.terminal(&event, at).unwrap();
    assert_eq!(stopped.status, VendorTerminalStatus::Interrupted);
    assert_eq!(stopped.stop_reason, StopReason::Other);
    assert_eq!(stopped.vendor_stop_reason, "user");
    assert_eq!(stopped.class_hint, None);
}

#[test]
fn oc07_only_successful_correlated_permission_decline_completes_shutdown() {
    use via_routes::opencode::events::InteractiveKind;
    let at = Instant::now();
    let shutdown = terminal(ExecutionKind::Interrupted, None, Some("shutdown"));
    for (kind, call, settled, status) in [
        (
            InteractiveKind::Permission,
            Some("call"),
            true,
            VendorTerminalStatus::Completed,
        ),
        (
            InteractiveKind::Permission,
            None,
            true,
            VendorTerminalStatus::Failed,
        ),
        (
            InteractiveKind::Permission,
            Some("call"),
            false,
            VendorTerminalStatus::Failed,
        ),
        (
            InteractiveKind::Form,
            Some("call"),
            true,
            VendorTerminalStatus::Failed,
        ),
    ] {
        let mut normalizer = Normalizer::new();
        normalizer.note_decline(kind, call, settled);
        let result = normalizer.terminal(&shutdown, at).unwrap();
        assert_eq!(result.status, status, "{kind:?} {call:?} {settled}");
        assert_eq!(result.stop_reason, StopReason::Other);
    }
}

#[test]
fn oc07_declined_call_suppresses_only_its_denial_and_preserves_tool_completion() {
    use via_routes::opencode::events::InteractiveKind;
    let mut normalizer = Normalizer::new();
    normalizer.note_decline(InteractiveKind::Permission, Some("declined"), true);
    for call in ["declined", "other"] {
        let items = normalizer.items(
            &EventData::Tool {
                kind: ToolKind::Failed,
                assistant_message_id: None,
                call_id: call.into(),
                tool: Some("bash".into()),
                error: Some(VendorError {
                    code: "permission.rejected".into(),
                    status: None,
                }),
            },
            Instant::now(),
        );
        assert!(matches!(&items[0], Observation::Progress(marks) if marks.tools_ended == [call]));
        assert_eq!(items.len(), if call == "declined" { 1 } else { 2 });
    }
}

#[test]
fn oc07_rule_denials_keep_the_action_class_when_completion_omits_name() {
    let at = Instant::now();
    for (action, expected) in [
        ("bash", DenialKind::Command),
        ("shell", DenialKind::Command),
        ("edit", DenialKind::FileWrite),
        ("write", DenialKind::FileWrite),
        ("patch", DenialKind::FileWrite),
        ("webfetch", DenialKind::Network),
        ("websearch", DenialKind::Network),
        ("browser", DenialKind::Network),
        ("other", DenialKind::Other),
    ] {
        let mut normalizer = Normalizer::new();
        normalizer.items(
            &EventData::Tool {
                kind: ToolKind::Called,
                assistant_message_id: None,
                call_id: "call".into(),
                tool: Some(action.into()),
                error: None,
            },
            at,
        );
        let items = normalizer.items(
            &EventData::Tool {
                kind: ToolKind::Failed,
                assistant_message_id: None,
                call_id: "call".into(),
                tool: None,
                error: Some(VendorError {
                    code: "permission.rejected".into(),
                    status: None,
                }),
            },
            at,
        );
        assert!(
            matches!(&items[1], Observation::ActionDenied(denial) if denial.kind == expected),
            "{action}: {items:?}"
        );
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the pressure fixture keeps fake setup, saturation, and owned-process cleanup together"
)]
fn oc07_declines_use_reserved_pool_with_general_full_and_observations_saturated() {
    use super::driver_tests::{Lane, SES, event, fixture, full, replace, route, row, run};
    use super::serve_tests::Rig;
    use crate::{
        Deadline, ObservationItem, Prepared, StopAck, TurnActivity, TurnCx, TurnNumber, TurnSpec,
    };
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::watch;
    use tokio::task::JoinSet;

    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        let mut scripted = fixture(
            &cwd,
            vec![
                event(
                    "session.inbox.enqueued",
                    &json!({"sessionID":"$SESSION","inboxID":"$INPUT"}),
                ),
                event(
                    "session.execution.started",
                    &json!({"sessionID":"$SESSION"}),
                ),
                event(
                    "session.inbox.delivered",
                    &json!({"sessionID":"$SESSION","inboxID":"$INPUT"}),
                ),
                event(
                    "session.step.started",
                    &json!({"sessionID":"$SESSION","assistantMessageID":"assistant"}),
                ),
                event(
                    "session.tool.called",
                    &json!({
                        "sessionID":"$SESSION", "assistantMessageID":"assistant",
                        "id":"call", "name":"bash"
                    }),
                ),
                json!({"pause_ms":1000}),
                event(
                    "permission.asked",
                    &json!({
                        "sessionID":"$SESSION", "id":"permission", "action":"shell",
                        "source":{"messageID":"assistant","id":"call"}
                    }),
                ),
            ],
        );
        replace(
            &mut scripted,
            route(
                "GET",
                "/api/session/ses_hold",
                &json!([
                    {"status":200,"sleep_ms":10_000,"raw":"held"}
                ]),
            ),
        );
        replace(
            &mut scripted,
            route(
                "POST",
                &format!("/api/session/{SES}/permission/permission/reply"),
                &json!([
                    {"status":204,"emit":[
                        event("session.tool.failed", &json!({
                            "sessionID":SES, "id":"call",
                            "error":{"type":"permission.rejected"}
                        })),
                        event("session.execution.interrupted", &json!({
                            "sessionID":SES, "reason":"shutdown"
                        }))
                    ]}
                ]),
            ),
        );
        rig.fixture(&scripted);
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let mut held = JoinSet::new();
        let (stop, stop_rx) = watch::channel(None);
        let (force, force_rx) = watch::channel(None);
        let now = Instant::now();
        let context = TurnCx {
            turn: TurnNumber::try_from(1).unwrap(),
            prepared: lane.driver.prepare(),
            capacity: Some(Box::new(())),
            activity: TurnActivity::new(now),
            wall: Deadline::at(now + Duration::from_secs(20)),
            tool_grace: Duration::from_secs(1),
            stop: stop_rx,
            force: force_rx,
            stop_ack: StopAck::new(),
        };
        let (filled, still_full, result) = {
            let future = lane.driver.run_turn(
                TurnSpec {
                    prompt: "fixture".into(),
                    bound: Some(full()),
                    ..TurnSpec::default()
                },
                context,
            );
            tokio::pin!(future);
            let mut early_end = None;
            let setup_by = Instant::now() + Duration::from_secs(5);
            let ready = loop {
                if rig
                    .requests()
                    .iter()
                    .any(|request| request["target"] == format!("/api/session/{SES}/prompt"))
                {
                    break true;
                }
                if Instant::now() >= setup_by {
                    break false;
                }
                // Keeping the same pinned turn future preserves every owned operation.
                tokio::select! {
                    end = &mut future => { early_end = Some(end); break false; },
                    () = tokio::time::sleep(Duration::from_millis(5)) => {},
                }
            };
            if ready
                && let Prepared::Pinned(pin) = lane.driver.prepare()
                && let Some((server, _)) = pin
                    .opencode
                    .as_ref()
                    .and_then(via_routes::opencode::ServerPin::live)
            {
                for _ in 0..4 {
                    let server = Arc::clone(&server);
                    held.spawn(async move {
                        via_routes::opencode::session::get(
                            server.http(),
                            "ses_hold",
                            Deadline::at(Instant::now() + Duration::from_secs(12)),
                        )
                        .await
                    });
                }
            }
            let mut filled = ready;
            while ready && lane.receiver.len() < crate::runtime::OBSERVATION_ITEMS {
                filled &= lane
                    .driver
                    .observations
                    .send(
                        ObservationItem {
                            at: Instant::now(),
                            vendor_turn: None,
                            observation: Observation::Progress(crate::ProgressMarks::default()),
                        },
                        Duration::from_millis(5),
                    )
                    .await
                    .is_ok();
                if !filled {
                    break;
                }
            }
            let decline_by = Instant::now() + Duration::from_secs(6);
            let still_full = loop {
                if early_end.is_some() {
                    break false;
                }
                if rig.requests().iter().any(|request| {
                    request["target"] == format!("/api/session/{SES}/permission/permission/reply")
                }) {
                    break lane.receiver.len() == crate::runtime::OBSERVATION_ITEMS;
                }
                if Instant::now() >= decline_by {
                    break false;
                }
                // No receive arm: the real C2 observation queue stays saturated.
                tokio::select! {
                    end = &mut future => { early_end = Some(end); break false; },
                    () = tokio::time::sleep(Duration::from_millis(5)) => {},
                }
            };
            held.abort_all();
            while held.join_next().await.is_some() {}
            while lane.receiver.try_recv().is_ok() {}
            let result = if let Some(end) = early_end {
                Some(end)
            } else {
                tokio::time::timeout(Duration::from_secs(2), &mut future)
                    .await
                    .ok()
            };
            (filled, still_full, result)
        };
        drop((stop, force));
        lane.close().await;
        let requests = rig.requests();
        let mut frames_path = rig.program.clone().into_os_string();
        frames_path.push(".frames");
        let frame_time = std::fs::read_to_string(std::path::PathBuf::from(frames_path))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|frame| frame["type"] == "permission.asked")
            .and_then(|frame| frame["written_ms"].as_u64());
        rig.finish().await;
        let declines: Vec<_> = requests
            .iter()
            .filter(|request| {
                request["target"] == format!("/api/session/{SES}/permission/permission/reply")
            })
            .collect();
        assert!(
            filled && still_full,
            "decline did not bypass saturated observations: {requests:?}; end: {result:?}"
        );
        assert_eq!(declines.len(), 1, "one reserved-pool decline: {requests:?}");
        let received = declines[0]["received_ms"].as_u64().unwrap();
        assert!(
            received.saturating_sub(frame_time.unwrap()) < 5000,
            "decline exceeded five seconds"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["target"] == "/api/session/ses_hold"
                    && request["received_ms"].as_u64().unwrap() < received)
                .count(),
            4,
            "all four general requests must already be held: {requests:?}"
        );
        assert!(
            matches!(
                result
                    .and_then(|end| end.terminal)
                    .map(|terminal| terminal.status),
                Some(VendorTerminalStatus::Completed)
            ),
            "the successful correlated permission decline must retain its terminal"
        );
    });
}

#[test]
fn oc07_native_settlement_before_http_timeout_drains_without_stopping_live_turn() {
    use super::driver_tests::{Lane, SES, event, fixture, replace, route, row, run};
    use super::serve_tests::Rig;
    use crate::Prepared;
    use serde_json::json;
    use std::time::Duration;

    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        let input = json!({"sessionID":"$SESSION","inboxID":"$INPUT"});
        let session = json!({"sessionID":"$SESSION"});
        let step = json!({"sessionID":"$SESSION","assistantMessageID":"assistant"});
        let call = json!({"sessionID":"$SESSION","id":"call","name":"bash"});
        let permission = json!({
            "sessionID":"$SESSION", "id":"permission", "action":"shell",
            "source":{"messageID":"assistant","id":"call"}
        });
        let mut scripted = fixture(
            &cwd,
            vec![
                event("session.inbox.enqueued", &input),
                event("session.execution.started", &session),
                event("session.inbox.delivered", &input),
                event("session.step.started", &step),
                event("session.tool.called", &call),
                event("permission.asked", &permission),
            ],
        );
        replace(
            &mut scripted,
            route(
                "POST",
                &format!("/api/session/{SES}/permission/permission/reply"),
                &json!([{"status":204, "sleep_ms":10_000, "emit_before_response":true,
                "emit":[event("permission.replied", &json!({
                    "sessionID":SES, "requestID":"permission", "reply":"reject"
                })), json!({"pause_ms":5500}),
                event("session.execution.succeeded", &json!({"sessionID":SES}))]}]),
            ),
        );
        rig.fixture(&scripted);
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let (end, _) = lane.turn(1, None, Duration::from_secs(8)).await;
        let drained = matches!(lane.driver.prepare(), Prepared::NeedsConnection);
        lane.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(drained, "incomplete HTTP response must drain: {requests:?}");
        assert!(
            matches!(
                end.terminal.as_ref().map(|terminal| terminal.status),
                Some(VendorTerminalStatus::Completed)
            ),
            "native settlement ends pending: {end:?}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["target"]
                    == format!("/api/session/{SES}/permission/permission/reply"))
                .count(),
            1
        );
        assert!(
            !requests
                .iter()
                .any(|request| request["target"] == format!("/api/session/{SES}/interrupt")),
            "settled request must not stop turn"
        );
    });
}

#[test]
fn oc10_decline_socket_loss_probes_vendor_death_before_protocol_notice() {
    use super::driver_tests::{Lane, SES, event, fixture, replace, route, row, run};
    use super::serve_tests::Rig;
    use crate::{AdapterError, RouteError};
    use serde_json::json;
    use std::time::Duration;

    run(async {
        let rig = Rig::new(&json!({}));
        let cwd = rig.root().to_str().unwrap().to_owned();
        let input = json!({"sessionID":"$SESSION","inboxID":"$INPUT"});
        let session = json!({"sessionID":"$SESSION"});
        let step = json!({"sessionID":"$SESSION","assistantMessageID":"assistant"});
        let permission = json!({
            "sessionID":"$SESSION", "id":"permission", "action":"shell",
            "source":{"messageID":"assistant","id":"call"}
        });
        let mut scripted = fixture(
            &cwd,
            vec![
                event("session.inbox.enqueued", &input),
                event("session.execution.started", &session),
                event("session.inbox.delivered", &input),
                event("session.step.started", &step),
                event("permission.asked", &permission),
            ],
        );
        replace(
            &mut scripted,
            route(
                "POST",
                &format!("/api/session/{SES}/permission/permission/reply"),
                &json!([{"status":200, "declared_length":1, "raw":"",
                "emit":[{"pause_ms":100},{"exit":true}]}]),
            ),
        );
        rig.fixture(&scripted);
        row(&rig, 1, "running");
        let mut lane = Lane::open(&rig, false);
        let (end, _) = lane.turn(1, None, Duration::from_secs(20)).await;
        lane.close().await;
        let requests = rig.requests();
        rig.finish().await;
        assert!(
            matches!(&end.outcome,
            Err(AdapterError::Route(failure)) if matches!(failure.cause,
                RouteError::ServerLost { .. })),
            "Host death must precede decline failure: {end:?}"
        );
        assert!(
            end.leftovers.is_some(),
            "the shared loss report precedes publication"
        );
        assert!(
            !requests
                .iter()
                .any(|request| request["target"] == format!("/api/session/{SES}/interrupt")),
            "dead vendor must not get an interrupt"
        );
    });
}

#[test]
fn oc03_last_owned_step_text_uses_ordinal_and_encoded_piece_limit() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.items(&step("old", StepKind::Started, None, None, None), at);
    normalizer.items(&text("old", 0, "earlier step"), at);
    normalizer.items(&step("last", StepKind::Started, None, None, None), at);
    let escaped = "\n\"🦀".repeat(50_000);
    normalizer.items(&text("last", 2, "tail"), at);
    normalizer.items(&text("last", 1, &escaped), at);
    let pieces = normalizer.final_text();
    assert!(
        pieces.concat() == format!("{escaped}tail"),
        "last-step text differs"
    );
    assert!(pieces.len() > 1);
    assert!(
        pieces
            .iter()
            .all(|piece| serde_json::to_vec(piece).unwrap().len() <= 256 * 1024)
    );
}

#[test]
fn oc11_step_usage_supersedes_samples_and_missing_is_unavailable() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    let first = normalizer.items(
        &step(
            "one",
            StepKind::Ended,
            Some(counts(3, 4)),
            Some(0.1),
            Some("stop"),
        ),
        at,
    );
    assert!(matches!(&first[..], [Observation::Progress(marks)]
            if marks.usage.as_ref().is_some_and(|usage| usage.key.as_deref() == Some("one"))));
    normalizer.items(
        &step(
            "one",
            StepKind::Ended,
            Some(counts(5, 6)),
            Some(0.2),
            Some("stop"),
        ),
        at,
    );
    normalizer.items(
        &step(
            "two",
            StepKind::Failed,
            Some(Tokens {
                input: None,
                ..counts(7, 8)
            }),
            None,
            None,
        ),
        at,
    );
    let usage = normalizer.usage();
    assert_eq!(usage.input, None);
    assert_eq!(usage.output, Some(14));
    assert_eq!(usage.cached_input, Some(4));
    assert_eq!(usage.reasoning_output, Some(0));
    assert_eq!(usage.total, None);
    assert!(!usage.interval_unverified);
    assert_eq!(normalizer.cost(), None);
}

#[test]
fn oc11_compaction_samples_mark_interval_and_cumulative_is_excluded() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.items(
        &step(
            "one",
            StepKind::Ended,
            Some(counts(3, 4)),
            Some(0.1),
            Some("stop"),
        ),
        at,
    );
    let compaction = normalizer.items(
        &EventData::Compaction {
            key: "inbox_compaction".into(),
            tokens: Some(counts(10, 20)),
            cost: Some(0.2),
        },
        at,
    );
    assert!(matches!(&compaction[..], [Observation::Progress(marks)]
            if marks.usage.as_ref().is_some_and(|usage| usage.interval_unverified)));
    normalizer.items(
        &EventData::Compaction {
            key: "inbox_compaction".into(),
            tokens: Some(counts(11, 21)),
            cost: Some(0.3),
        },
        at,
    );
    assert!(
        normalizer
            .items(
                &EventData::Activity {
                    message_id: None,
                    call_id: None
                },
                at
            )
            .is_empty()
    );
    assert_eq!(normalizer.usage().input, Some(14));
    assert_eq!(normalizer.usage().output, Some(25));
    assert!(normalizer.usage().interval_unverified);
    let cost = normalizer.cost().unwrap();
    assert!((cost.usd - 0.4).abs() < f64::EPSILON);
    assert_eq!(cost.scope, "vendor_interval");
    assert_eq!(cost.provenance, CostProvenance::Reported);
}

#[test]
fn oc03_terminal_finish_and_oc11_class_hints_never_use_vendor_messages() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.items(&step("one", StepKind::Started, None, None, None), at);
    normalizer.items(
        &step(
            "one",
            StepKind::Ended,
            Some(counts(3, 4)),
            Some(0.1),
            Some("length"),
        ),
        at,
    );
    let success = normalizer
        .terminal(&terminal(ExecutionKind::Succeeded, None, None), at)
        .unwrap();
    assert_eq!(success.status, VendorTerminalStatus::Completed);
    assert_eq!(success.stop_reason, StopReason::Budget);
    for (code, status, class) in [
        ("provider.auth", None, ClassHint::Auth),
        ("unknown", Some(403), ClassHint::Auth),
        ("provider.rate-limit", None, ClassHint::RateLimit),
        ("unknown", Some(429), ClassHint::RateLimit),
        ("provider.quota", None, ClassHint::BudgetExceeded),
        ("provider.no-route", None, ClassHint::VendorError),
    ] {
        let failed = normalizer
            .terminal(
                &terminal(
                    ExecutionKind::Failed,
                    Some(VendorError {
                        code: code.into(),
                        status,
                    }),
                    None,
                ),
                at,
            )
            .unwrap();
        assert_eq!(failed.status, VendorTerminalStatus::Failed);
        assert_eq!(failed.vendor_code.as_deref(), Some(code));
        assert_eq!(failed.class_hint, Some(class));
        assert_eq!(failed.detail.as_deref(), Some("OpenCode execution failed"));
    }
    let interrupted = normalizer
        .terminal(
            &terminal(ExecutionKind::Interrupted, None, Some("shutdown")),
            at,
        )
        .unwrap();
    assert_eq!(interrupted.status, VendorTerminalStatus::Failed);
    assert_eq!(
        interrupted.vendor_code.as_deref(),
        Some("interrupted:shutdown")
    );
}

#[test]
fn oc06_tool_completions_always_end_and_permission_rejection_is_denial() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    let called = EventData::Tool {
        kind: ToolKind::Called,
        assistant_message_id: Some("msg".into()),
        call_id: "call".into(),
        tool: Some("bash".into()),
        error: None,
    };
    assert!(
        matches!(&normalizer.items(&called, at)[..], [Observation::Progress(marks)]
            if marks.model && marks.tools_started == [("call".into(), "bash".into())])
    );
    let failed = EventData::Tool {
        kind: ToolKind::Failed,
        assistant_message_id: Some("msg".into()),
        call_id: "call".into(),
        tool: Some("bash".into()),
        error: Some(VendorError {
            code: "permission.rejected".into(),
            status: None,
        }),
    };
    assert!(matches!(&normalizer.items(&failed, at)[..],
            [Observation::Progress(marks), Observation::ActionDenied(denied)]
            if marks.tools_ended == ["call"]
                && denied.reason == "denied by the vendor's permission policy"));
}

#[test]
fn oc11_owned_step_without_end_does_not_report_previous_call_prefix() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.items(
        &step(
            "first",
            StepKind::Ended,
            Some(counts(3, 4)),
            Some(0.1),
            Some("stop"),
        ),
        at,
    );
    normalizer.items(&step("last", StepKind::Started, None, None, None), at);
    let result = normalizer
        .terminal(&terminal(ExecutionKind::Succeeded, None, None), at)
        .unwrap();
    assert_eq!(result.usage.unwrap().input, None);
    assert_eq!(result.cost, None);
    assert!(result.vendor.is_none());
    // A repeated start after the sample must not erase complete usage.
    normalizer.items(
        &step(
            "last",
            StepKind::Ended,
            Some(counts(5, 6)),
            Some(0.2),
            Some("stop"),
        ),
        at,
    );
    normalizer.items(&step("last", StepKind::Started, None, None, None), at);
    assert_eq!(normalizer.usage().input, Some(8));
}

#[test]
fn oc03_only_successful_last_step_finish_sets_completed_stop_reason() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.items(&step("last", StepKind::Started, None, None, None), at);
    normalizer.items(
        &step(
            "last",
            StepKind::Failed,
            Some(counts(3, 4)),
            Some(0.1),
            Some("length"),
        ),
        at,
    );
    let result = normalizer
        .terminal(&terminal(ExecutionKind::Succeeded, None, None), at)
        .unwrap();
    assert_eq!(result.stop_reason, StopReason::Other);
}

#[test]
fn oc06_joined_execution_text_learns_first_owned_step_without_replayed_start() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.items(&text("joined", 0, "joined answer"), at);
    normalizer.items(
        &step(
            "joined",
            StepKind::Ended,
            Some(counts(3, 4)),
            Some(0.1),
            Some("stop"),
        ),
        at,
    );
    assert_eq!(normalizer.final_text().concat(), "joined answer");
    let result = normalizer
        .terminal(&terminal(ExecutionKind::Succeeded, None, None), at)
        .unwrap();
    assert_eq!(result.stop_reason, StopReason::EndTurn);

    // Later events of the joined step cannot displace a newer known step.
    normalizer.items(&step("newer", StepKind::Started, None, None, None), at);
    normalizer.items(&text("newer", 0, "newer answer"), at);
    normalizer.items(&text("joined", 1, "late earlier text"), at);
    normalizer.items(
        &step(
            "joined",
            StepKind::Ended,
            Some(counts(3, 4)),
            Some(0.1),
            Some("length"),
        ),
        at,
    );
    assert_eq!(normalizer.final_text().concat(), "newer answer");
    let result = normalizer
        .terminal(&terminal(ExecutionKind::Succeeded, None, None), at)
        .unwrap();
    assert_eq!(result.stop_reason, StopReason::Other);
}

#[test]
fn oc06_joined_execution_step_end_learns_first_owned_step_without_replayed_start() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.items(
        &step(
            "joined",
            StepKind::Ended,
            Some(counts(3, 4)),
            Some(0.1),
            Some("stop"),
        ),
        at,
    );
    normalizer.items(&text("joined", 0, "joined answer"), at);
    let result = normalizer
        .terminal(&terminal(ExecutionKind::Succeeded, None, None), at)
        .unwrap();
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(normalizer.final_text().concat(), "joined answer");
}

#[test]
fn oc11_joined_step_missing_end_keeps_successor_usage_unavailable() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.items(&text("joined", 0, "joined step text"), at);
    normalizer.items(&step("next", StepKind::Started, None, None, None), at);
    normalizer.items(
        &step(
            "next",
            StepKind::Ended,
            Some(counts(3, 4)),
            Some(0.1),
            Some("stop"),
        ),
        at,
    );
    normalizer.items(&text("next", 0, "final answer"), at);
    let result = normalizer
        .terminal(&terminal(ExecutionKind::Succeeded, None, None), at)
        .unwrap();
    assert_eq!(result.usage, Some(crate::UsageSample::default()));
    assert!(result.cost.is_none());
    assert!(result.vendor.is_none());
    assert_eq!(normalizer.final_text().concat(), "final answer");
}

#[test]
fn oc06_joined_started_order_keeps_latest_step_final_text() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.register_started_steps(&["earlier".into(), "latest".into()]);
    normalizer.items(&text("earlier", 0, "earlier late text"), at);
    normalizer.items(&text("latest", 0, "latest answer"), at);
    assert_eq!(normalizer.final_text().concat(), "latest answer");
    normalizer.items(
        &step(
            "earlier",
            StepKind::Ended,
            Some(counts(3, 4)),
            Some(0.1),
            Some("length"),
        ),
        at,
    );
    normalizer.items(
        &step(
            "latest",
            StepKind::Ended,
            Some(counts(5, 6)),
            Some(0.2),
            Some("stop"),
        ),
        at,
    );
    let result = normalizer
        .terminal(&terminal(ExecutionKind::Succeeded, None, None), at)
        .unwrap();
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.usage.unwrap().input, Some(8));
}

#[test]
fn oc11_joined_started_history_keeps_missing_prior_call_unavailable() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.register_started_steps(&["earlier".into(), "latest".into()]);
    normalizer.items(
        &step(
            "latest",
            StepKind::Ended,
            Some(counts(5, 6)),
            Some(0.2),
            Some("stop"),
        ),
        at,
    );
    let result = normalizer
        .terminal(&terminal(ExecutionKind::Succeeded, None, None), at)
        .unwrap();
    assert_eq!(result.usage, Some(crate::UsageSample::default()));
    assert!(result.cost.is_none());
    assert!(result.vendor.is_none());
}

#[test]
fn oc09_final_text_budget_accepts_exact_capacity_then_rejects_next_candidate() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    // §9's four MiB retained budget charges 64 logical bytes per ordinal entry.
    let part = "x".repeat(512 * 1024 - 64);
    for ordinal in 0..8 {
        normalizer.items(&text("step", ordinal, &part), at);
    }
    assert_eq!(normalizer.final_text().concat(), part.repeat(8));
    assert!(!normalizer.text_overflow());
    normalizer.items(&text("step", 8, "y"), at);
    assert!(normalizer.text_overflow());
    assert!(
        normalizer.final_text().is_empty(),
        "overflow makes candidates unavailable"
    );
    normalizer.items(&step("next", StepKind::Started, None, None, None), at);
    normalizer.items(&text("next", 0, "later"), at);
    assert!(
        normalizer.final_text().is_empty(),
        "turn overflow is sticky across steps"
    );
}

#[test]
fn oc09_empty_final_text_ordinals_consume_retained_budget() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    normalizer.items(&text("step", 0, "x"), at);
    for ordinal in 1..65_535 {
        normalizer.items(&text("step", ordinal, ""), at);
    }
    assert_eq!(normalizer.final_text().concat(), "x", "below four MiB");
    normalizer.items(&text("step", 65_535, ""), at);
    assert!(normalizer.text_overflow());
    assert!(
        normalizer.final_text().is_empty(),
        "empty entries cannot grow without bound"
    );
}

#[test]
fn oc09_final_text_replacement_and_new_step_release_retained_bytes() {
    let mut normalizer = Normalizer::new();
    let at = Instant::now();
    let part = "é".repeat((512 * 1024 - 64) / 2);
    for ordinal in 0..8 {
        normalizer.items(&text("first", ordinal, &part), at);
    }
    assert_eq!(normalizer.final_text().concat(), part.repeat(8));
    normalizer.items(&text("first", 0, "short"), at);
    normalizer.items(&text("first", 8, "other"), at);
    assert_eq!(
        normalizer.final_text().concat(),
        format!("short{}other", part.repeat(7))
    );
    normalizer.items(&step("second", StepKind::Started, None, None, None), at);
    for ordinal in 0..8 {
        normalizer.items(&text("second", ordinal, &part), at);
    }
    assert_eq!(
        normalizer.final_text().concat(),
        part.repeat(8),
        "only the last owned step remains"
    );
    assert!(!normalizer.text_overflow());
}
