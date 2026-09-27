//! Core's terminal decision from adapter evidence and the C1 §5 envelope.

use serde_json::json;
use via_adapters::{AdapterError, Cleanup, FakeTerminalEvidence, RouteError, VendorTerminalStatus};

use super::{Accepted, Terminal, failure};
use crate::api::{
    Bound, Cost, Envelope, EventRange, Exit, FailureClass, RawSpan, Requested, RoutePlan,
    Timestamps, Usage, VendorFields, Warning,
};
use crate::{SessionId, TurnNumber};

/// Assembles the C1 §5 envelope; `last_seq` is `turn.ended`, and S1 turns start at seq 1.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is a distinct committed fact of the one turn"
)]
pub(super) fn terminal_envelope(
    session: &SessionId,
    turn: TurnNumber,
    terminal: Terminal,
    accepted: Option<Accepted>,
    raw_spans: Vec<RawSpan>,
    timestamps: Timestamps,
    duration_ms: Option<u64>,
    last_seq: u64,
) -> Envelope {
    let plan = RoutePlan::fake();
    let mut warnings = plan.warnings();
    warnings.extend(terminal.warnings);
    if terminal
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.cleanup == "uncertain")
    {
        warnings.push(Warning::CANCEL_CLEANUP_UNCERTAIN);
    }
    Envelope {
        api_version: 1,
        session_id: session.clone(),
        turn: turn.get(),
        address: format!("{}/{}", session.as_str(), turn.get()),
        revision: 0,
        state: terminal.state,
        failure: terminal.failure,
        stop_reason: terminal.stop_reason,
        vendor_stop_reason: terminal.vendor_stop_reason,
        cancel: terminal.cancel,
        harness: "fake",
        model: Requested {
            requested: "fake".to_owned(),
            resolved: "fake".to_owned(),
        },
        effort: Requested {
            requested: None,
            resolved: None,
        },
        warnings,
        plan,
        vendor_session_id: None,
        cwd: None,
        bound: Bound::NONE,
        final_text: terminal.final_text,
        structured_output: None,
        denied_actions: [],
        auto_declined_requests: [],
        steps: None,
        usage: Usage::UNAVAILABLE,
        cost: Cost::UNAVAILABLE,
        timestamps,
        duration_ms,
        exit: terminal.exit,
        events: EventRange {
            first_seq: 1,
            last_seq,
            count: last_seq,
        },
        raw_spans,
        vendor_options: json!({}),
        vendor: VendorFields {
            turn_id: accepted.map(|accepted| accepted.vendor_turn_id),
        },
    }
}

/// Maps a typed route cause onto C1 §7.6 state, §8.2 class and stop reason.
fn route_disposition(cause: &RouteError) -> (&'static str, Option<FailureClass>, &'static str) {
    match cause {
        RouteError::Protocol { .. } => ("failed", Some(FailureClass::Protocol), "error"),
        RouteError::ProcessExited { .. } => ("failed", Some(FailureClass::ProcessExited), "error"),
        RouteError::Overflow { .. } => ("failed", Some(FailureClass::Overflow), "error"),
        RouteError::Store { .. } => ("failed", Some(FailureClass::Store), "error"),
        RouteError::Deadline { .. } => ("failed", Some(FailureClass::DeadlineWall), "deadline"),
        // Core settles a force stop itself; this is only the C1 §7.6 force row.
        RouteError::ForceStopped { .. } => ("cancelled", None, "interrupted"),
        // Input may have reached the vendor and no exit is confirmed (§7.6).
        RouteError::TransportLost { .. } => ("unknown", None, "error"),
    }
}

pub(super) fn classify(
    accepted: bool,
    outcome: Result<FakeTerminalEvidence, AdapterError>,
) -> Terminal {
    let evidence = match outcome {
        Ok(evidence) => evidence,
        Err(error) => return failed_terminal(error),
    };
    let failed = |class, message: &str| Some(failure(class, message.to_owned(), None));
    let failure = if accepted {
        match evidence.status {
            VendorTerminalStatus::Completed if evidence.exit.code != Some(0) => failed(
                FailureClass::ProcessExited,
                "the vendor exited unsuccessfully",
            ),
            VendorTerminalStatus::Completed if evidence.cleanup != Cleanup::Quiescent => failed(
                FailureClass::ProcessExited,
                "vendor process group cleanup is unconfirmed",
            ),
            VendorTerminalStatus::Completed => None,
            VendorTerminalStatus::Interrupted | VendorTerminalStatus::Failed => Some(failure(
                FailureClass::VendorError,
                "the vendor reported a failed turn".to_owned(),
                evidence.vendor_code.clone(),
            )),
        }
    } else {
        failed(
            FailureClass::SubmitFailed,
            "the vendor did not accept the submission",
        )
    };
    let stop_reason = match (&failure, evidence.status) {
        (None, _) => canonical_stop_reason(&evidence.stop_reason),
        (Some(_), VendorTerminalStatus::Interrupted) => "interrupted",
        (Some(_), VendorTerminalStatus::Completed | VendorTerminalStatus::Failed) => "error",
    };
    Terminal {
        state: if failure.is_none() {
            "completed"
        } else {
            "failed"
        },
        failure,
        stop_reason,
        vendor_stop_reason: Some(evidence.stop_reason),
        final_text: evidence.final_text,
        exit: Some(Exit {
            code: evidence.exit.code,
            signal: evidence.exit.signal,
        }),
        raw_ref: Some(evidence.terminal_raw),
        raw_incomplete: false,
        warnings: Vec::new(),
        cancel: None,
    }
}

/// Keeps the typed cause, cited frame, confirmed exit and raw completeness of a
/// failed drive.
fn failed_terminal(error: AdapterError) -> Terminal {
    let message = error.to_string();
    let (state, class, stop_reason, route) = match error {
        AdapterError::Route(route) => {
            let (state, class, stop_reason) = route_disposition(&route.cause);
            (state, class, stop_reason, Some(route))
        }
        // No process was launched for the submission.
        AdapterError::Unavailable => ("failed", Some(FailureClass::SubmitFailed), "error", None),
        AdapterError::Open(_) | AdapterError::Protocol => {
            ("failed", Some(FailureClass::Protocol), "error", None)
        }
    };
    Terminal {
        state,
        failure: class.map(|class| failure(class, message, None)),
        stop_reason,
        vendor_stop_reason: None,
        final_text: String::new(),
        exit: route
            .as_ref()
            .and_then(|route| route.exit)
            .map(|exit| Exit {
                code: exit.code,
                signal: exit.signal,
            }),
        raw_ref: route.as_ref().and_then(|route| route.evidence.clone()),
        raw_incomplete: route.is_some_and(|route| route.raw_incomplete),
        warnings: Vec::new(),
        cancel: None,
    }
}

/// Maps a vendor stop word onto C1's closed `stop_reason` set.
fn canonical_stop_reason(vendor: &str) -> &'static str {
    match vendor {
        "end_turn" => "end_turn",
        "max_steps" => "max_steps",
        "budget" => "budget",
        "refusal" => "refusal",
        "interrupted" => "interrupted",
        "deadline" => "deadline",
        "error" => "error",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::{FailureClass, TurnNumber, failed_terminal};
    use via_adapters::{AdapterError, RouteError, RouteFailure};

    fn route(cause: RouteError, raw_incomplete: bool) -> AdapterError {
        AdapterError::Route(RouteFailure {
            cause,
            evidence: None,
            exit: None,
            raw_incomplete,
            launched: false,
            cleanup: None,
            forced: false,
        })
    }

    /// Causes the fake vendor cannot trigger end to end keep their C1 §8.2 class.
    #[test]
    fn route_causes_keep_their_c1_disposition() {
        let turn = TurnNumber::try_from(1).unwrap();
        for (cause, class) in [
            (RouteError::Overflow { turn }, FailureClass::Overflow),
            (RouteError::Store { turn }, FailureClass::Store),
            (RouteError::Deadline { turn }, FailureClass::DeadlineWall),
            (
                RouteError::ProcessExited { turn },
                FailureClass::ProcessExited,
            ),
        ] {
            let terminal = failed_terminal(route(cause, false));
            assert_eq!(terminal.state, "failed");
            assert_eq!(terminal.failure.map(|failure| failure.class), Some(class));
            assert!(!terminal.raw_incomplete);
        }
        let lost = failed_terminal(route(
            RouteError::TransportLost {
                turn,
                evidence: None,
            },
            true,
        ));
        assert_eq!(lost.state, "unknown");
        assert!(lost.failure.is_none());
        assert!(lost.raw_incomplete);
    }
}
