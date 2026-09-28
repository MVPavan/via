//! Core's terminal decision from adapter evidence and the C1 §5 envelope.

use serde_json::json;
use via_adapters::{
    AdapterError, Cleanup, FakeTerminalEvidence, RouteError, RouteFailure, StopCause, StopOrder,
    VendorTerminalStatus, WireCleanup,
};
use via_store::CancelCause;

use super::stop::stop_outcome;
use super::{Accepted, Terminal, failure};
use crate::api::{
    Bound, Cost, Envelope, EventRange, Exit, FailureClass, RawSpan, Requested, RoutePlan,
    Timestamps, Usage, VendorFields, Warning,
};
use crate::{SessionId, TurnNumber};

/// Assembles the C1 §5 envelope; `events` runs from the turn's `turn.queued` to
/// its `turn.ended`, other turns' events of the session included.
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
    (first_seq, last_seq): (u64, u64),
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
            first_seq,
            last_seq,
            count: last_seq + 1 - first_seq,
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
        // Core settles a force stop itself, and `dispose` a stop order's
        // `Stopped`; this is only the C1 §7.6 force row.
        RouteError::ForceStopped { .. } | RouteError::Stopped { .. } => {
            ("cancelled", None, "interrupted")
        }
        // Input may have reached the vendor and no exit is confirmed (§7.6).
        RouteError::TransportLost { .. } => ("unknown", None, "error"),
    }
}

/// Core's disposition of a turn's evidence, with any stop it settles.
pub(super) struct Disposed {
    pub(super) terminal: Terminal,
    /// The C1 §3.5 `cancel` outcome and cleanup to settle: under a stop
    /// order, or when the wall deadline stopped the turn.
    pub(super) stop: Option<(&'static str, &'static str)>,
    /// Who cancelled the turn, recorded when it ends `cancelled` (design §4).
    pub(super) cancel_cause: Option<CancelCause>,
}

/// Design §2's disposition table (C1 §7.6; the first matching row wins).
/// `wall` is the turn's wall deadline: a `Deadline` coincident with an
/// order's `force_at` takes the order's row [r1.9].
pub(super) fn dispose(
    accepted: bool,
    outcome: Result<FakeTerminalEvidence, AdapterError>,
    order: Option<&StopOrder>,
    wall: tokio::time::Instant,
) -> Disposed {
    let Some(order) = order else {
        // C1 §7.6: Core's deadline cancels the turn; Route force-closed its group.
        let stop = match &outcome {
            Err(AdapterError::Route(route))
                if matches!(route.cause, RouteError::Deadline { .. }) =>
            {
                Some(stop_outcome(
                    route.cleanup == Some(WireCleanup::Quiescent),
                    route.forced,
                ))
            }
            Ok(_) | Err(_) => None,
        };
        return Disposed {
            terminal: classify(accepted, outcome),
            stop,
            cancel_cause: None,
        };
    };
    let cause = order.cause;
    let requested = match cause {
        StopCause::Cancel => Some(CancelCause::Cancel),
        StopCause::Close => Some(CancelCause::Close),
        StopCause::IdleDeadline | StopCause::Store => None,
    };
    match outcome {
        Ok(evidence) => {
            let cleanup = cleanup_word(evidence.cleanup == Cleanup::Quiescent);
            let interrupted = evidence.status == VendorTerminalStatus::Interrupted;
            let mut terminal = classify(accepted, Ok(evidence));
            let stop = match (interrupted, cause) {
                (_, StopCause::Store) => {
                    terminal.fail(FailureClass::Store, STORE_STOP);
                    ("requested", cleanup)
                }
                (true, StopCause::Cancel | StopCause::Close) => {
                    terminal.state = "cancelled";
                    terminal.failure = None;
                    terminal.stop_reason = "interrupted";
                    ("acknowledged", cleanup)
                }
                (true, StopCause::IdleDeadline) => {
                    idle(&mut terminal);
                    ("acknowledged", cleanup)
                }
                // Completed or failed on its own: the vendor ignored the order.
                (false, _) => ("requested", cleanup),
            };
            Disposed {
                terminal,
                stop: Some(stop),
                cancel_cause: requested,
            }
        }
        Err(AdapterError::Route(route)) => stopped(route, cause, order, wall, requested),
        // Nothing launched: the adapter refused the turn before Route.
        Err(error) => Disposed {
            terminal: failed_terminal(error),
            stop: Some(("requested", "quiescent")),
            cancel_cause: None,
        },
    }
}

/// Message of a turn stopped because its own Store write failed.
const STORE_STOP: &str = "a turn event could not be recorded";

/// C1 §3.5 cleanup word.
fn cleanup_word(quiescent: bool) -> &'static str {
    if quiescent { "quiescent" } else { "uncertain" }
}

/// The `failed(deadline_idle)` row (design §5).
fn idle(terminal: &mut Terminal) {
    terminal.fail(
        FailureClass::DeadlineIdle,
        "no progress within the idle deadline",
    );
    terminal.stop_reason = "deadline";
}

/// Route's failure under a stop order (design §2's table).
fn stopped(
    route: RouteFailure,
    cause: StopCause,
    order: &StopOrder,
    wall: tokio::time::Instant,
    requested: Option<CancelCause>,
) -> Disposed {
    // Design §2 [r1.8]: quiescent only with Host's absence proof, or with
    // no anchor intent at all (an order set before `execute`, or an anchor
    // intent that did not commit, §7.2 row 3).
    let quiescent = match route.cleanup {
        Some(cleanup) => cleanup == WireCleanup::Quiescent,
        None => {
            !route.launched
                && matches!(
                    route.cause,
                    RouteError::Stopped { .. } | RouteError::Store { .. }
                )
        }
    };
    let (outcome, cleanup) = stop_outcome(quiescent, route.forced);
    let by_order = match route.cause {
        RouteError::Stopped { .. } => true,
        RouteError::Deadline { .. } => order.force_at.instant() == wall,
        RouteError::Protocol { .. }
        | RouteError::TransportLost { .. }
        | RouteError::ProcessExited { .. }
        | RouteError::Overflow { .. }
        | RouteError::Store { .. }
        | RouteError::ForceStopped { .. } => false,
    };
    let launched = route.launched;
    let forced = route.forced;
    let mut terminal = failed_terminal(AdapterError::Route(route));
    if cause == StopCause::Store {
        terminal.fail(FailureClass::Store, STORE_STOP);
        return Disposed {
            terminal,
            stop: Some((outcome, cleanup)),
            cancel_cause: None,
        };
    }
    if !by_order {
        // Deadline before the order's force, process exit, transport loss
        // and other failures keep their own row.
        return Disposed {
            terminal,
            stop: Some((outcome, cleanup)),
            cancel_cause: None,
        };
    }
    match cause {
        StopCause::IdleDeadline => {
            idle(&mut terminal);
            Disposed {
                terminal,
                stop: Some((outcome, cleanup)),
                cancel_cause: None,
            }
        }
        StopCause::Cancel | StopCause::Close | StopCause::Store => {
            terminal.failure = None;
            let stop = if !launched {
                // Nothing launched: stopped before the vendor could act.
                terminal.state = "cancelled";
                terminal.stop_reason = "interrupted";
                ("requested", cleanup)
            } else if forced {
                terminal.state = "cancelled";
                terminal.stop_reason = "interrupted";
                ("forced", cleanup)
            } else {
                // A vendor may have run with neither stop nor terminal proved.
                terminal.state = "unknown";
                terminal.stop_reason = "error";
                ("requested", cleanup)
            };
            Disposed {
                cancel_cause: requested.filter(|_| terminal.state == "cancelled"),
                terminal,
                stop: Some(stop),
            }
        }
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
            journal_uncertain: false,
        })
    }

    /// Causes the fake vendor cannot trigger end to end keep their C1 §8.2 class.
    #[test]
    fn route_causes_keep_their_c1_disposition() {
        let turn = TurnNumber::try_from(1).unwrap();
        for (cause, class) in [
            (RouteError::Overflow { turn }, FailureClass::Overflow),
            (
                RouteError::Store {
                    turn,
                    kind: via_adapters::StoreFailure::Raw,
                },
                FailureClass::Store,
            ),
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
