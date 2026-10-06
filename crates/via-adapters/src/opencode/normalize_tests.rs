use tokio::time::Instant;
use via_routes::opencode::events::{
    EventData, ExecutionKind, StepKind, TextKind, Tokens, ToolKind, VendorError,
};

use super::normalize::Normalizer;
use crate::{ClassHint, CostProvenance, Observation, StopReason, VendorTerminalStatus};

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
    assert!(
        matches!(&first[..], [Observation::Progress(marks)] if marks.usage.as_ref().is_some_and(|usage| usage.key.as_deref() == Some("one")))
    );
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
    assert!(
        matches!(&compaction[..], [Observation::Progress(marks)] if marks.usage.as_ref().is_some_and(|usage| usage.interval_unverified))
    );
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
        matches!(&normalizer.items(&called, at)[..], [Observation::Progress(marks)] if marks.model && marks.tools_started == [("call".into(), "bash".into())])
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
    assert!(
        matches!(&normalizer.items(&failed, at)[..], [Observation::Progress(marks), Observation::ActionDenied(denied)] if marks.tools_ended == ["call"] && denied.reason == "denied by the vendor's permission policy")
    );
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
