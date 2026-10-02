//! Core's terminal decision from adapter evidence and the C1 §5 envelope.

use serde_json::json;
use via_adapters::{
    AdapterError, ClassHint, Cleanup, RouteError, RouteFailure, StartRejected, StopCause,
    StopOrder, StopReason, TurnEvidence, VendorTerminal, VendorTerminalStatus, VersionStatus,
    WireCleanup,
};
use via_store::{CancelCause, InstanceRecord};

use super::lane::{Identity, VendorRecord};
use super::stop::stop_outcome;
use super::{Accepted, Terminal, failure};
use crate::api::{
    Cost, Envelope, EventRange, EvidenceRef, Exit, FailureClass, PlanFields, TRANSCRIPT_MAX,
    Timestamps, Usage, VendorFields, Warning, encodes_within,
};
use crate::intake::TurnPlan;
use crate::{SessionId, TurnNumber};

/// Assembles the C1 §5 envelope of a turn that reported nothing to its
/// vendor record: `events` runs from the turn's `turn.queued` to
/// its `turn.ended`, other turns' events of the session included. `cwd` is
/// the session's frozen working directory (design §11.1), `None` where the
/// caller did not read it; `folder` is the turn's absolute evidence folder,
/// `None` for a turn never submitted. `identity` is the session's stored
/// one where the caller read it (critical r1 #11); `instance` is the
/// turn's recorded instance version and whether it was tested (C1 §3.7);
/// `plan` is the session's frozen plan and the turn's frozen values
/// (design §5.1 #33).
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is a distinct committed fact of the one turn"
)]
pub(super) fn terminal_envelope(
    session: &SessionId,
    turn: TurnNumber,
    terminal: Terminal,
    accepted: Option<Accepted>,
    (cwd, folder): (Option<String>, Option<String>),
    timestamps: Timestamps,
    duration_ms: Option<u64>,
    (first_seq, last_seq): (u64, u64),
    usage: Usage,
    (identity, instance, plan): (Option<Identity>, Option<InstanceRecord>, &TurnPlan),
) -> Envelope {
    assemble(
        (session, turn, plan),
        terminal,
        accepted,
        (cwd, folder),
        (timestamps, duration_ms),
        (first_seq, last_seq),
        (usage, false),
        VendorRecord {
            identity,
            instance: instance.map(|instance| (instance.vendor_version, instance.tested)),
            ..VendorRecord::default()
        },
    )
}

/// [`terminal_envelope`] of a run turn, with what its observations and its
/// end established (design §5.1 #33): the confirmed identity and
/// transcript, the denials and declines it committed, the instance's
/// version (AD7), the retained terminal's structured output, steps, cost
/// and vendor data (AD4), and the usage ledger's figure, which a turn
/// aggregate supersedes (AD6).
pub(super) fn turn_envelope(
    (session, turn, plan): (&SessionId, TurnNumber, &TurnPlan),
    terminal: Terminal,
    accepted: Option<Accepted>,
    paths: (Option<String>, Option<String>),
    times: (Timestamps, Option<u64>),
    range: (u64, u64),
    vendor: VendorRecord,
) -> Envelope {
    let aggregate = vendor
        .retained
        .as_ref()
        .and_then(|retained| retained.usage.as_ref());
    let figure = vendor.ledger.figure(aggregate);
    let interval = figure.as_ref().is_some_and(|(_, interval)| *interval);
    let usage = Usage::reported(
        figure.map(|(tokens, _)| tokens),
        interval,
        plan.frozen.token_scope(),
    );
    assemble(
        (session, turn, plan),
        terminal,
        accepted,
        paths,
        times,
        range,
        (usage, interval),
        vendor,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "each argument is a distinct committed fact of the one turn"
)]
fn assemble(
    (session, turn, plan): (&SessionId, TurnNumber, &TurnPlan),
    terminal: Terminal,
    accepted: Option<Accepted>,
    (cwd, folder): (Option<String>, Option<String>),
    (timestamps, duration_ms): (Timestamps, Option<u64>),
    (first_seq, last_seq): (u64, u64),
    (usage, interval): (Usage, bool),
    vendor: VendorRecord,
) -> Envelope {
    // AD7: the version the turn's own instance reported; AD12: the
    // adapter that ran the turn, else the session's recorded one.
    let (vendor_version, tested) = vendor.instance.unwrap_or((None, false));
    let version = PlanFields {
        route: plan.frozen.route.clone(),
        adapter_version: vendor
            .adapter_version
            .clone()
            .unwrap_or_else(|| plan.frozen.adapter_version.clone()),
        vendor_version,
        version_status: if tested {
            VersionStatus::Tested
        } else {
            VersionStatus::Untested
        },
    };
    let mut warnings: Vec<Warning> = version.warning().into_iter().collect();
    // C1 §5, AD13: the session's unverified inheritance, on every envelope.
    warnings.extend(plan.frozen.config_warning());
    warnings.extend(terminal.warnings);
    // C1 §5: the turn's own adapter warnings of the closed list.
    warnings.extend(vendor.warnings);
    if interval {
        warnings.push(Warning::USAGE_INTERVAL_UNVERIFIED);
    }
    if terminal
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.cleanup == "uncertain")
    {
        warnings.push(Warning::CANCEL_CLEANUP_UNCERTAIN);
    }
    // Design §6.4: one entry per code; a repeated code keeps its first.
    let mut codes = std::collections::HashSet::new();
    warnings.retain(|warning| codes.insert(warning.code()));
    // C1 §5: each within its message and data caps.
    let warnings = warnings.into_iter().map(Warning::capped).collect();
    let (denied_actions, denied_actions_total) = vendor.denied.into_parts();
    let (auto_declined_requests, auto_declined_requests_total) = vendor.declined.into_parts();
    let retained = vendor.retained.unwrap_or_default();
    let mut data = retained.vendor;
    // The acceptance's own turn ID is the envelope's.
    data.remove("turn_id");
    let (vendor_session_id, transcript) = vendor.identity.map_or((None, None), |identity| {
        (Some(identity.vendor_session_id), identity.transcript)
    });
    // C1 §5: a transcript hint over 4 KiB encoded is `null`.
    let transcript = transcript.filter(|hint| encodes_within(hint, TRANSCRIPT_MAX));
    let envelope = Envelope {
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
        harness: plan.frozen.harness.clone(),
        model: plan.model(),
        effort: plan.effort(),
        warnings,
        plan: version,
        vendor_session_id,
        cwd,
        bound: plan.bound(),
        final_text: terminal.final_text,
        final_text_file: terminal.final_text_file,
        // Validated against the frozen schema before it was kept (#37).
        structured_output: retained.structured_output,
        structured_output_file: retained.structured_output_file,
        leftovers: None,
        denied_actions,
        auto_declined_requests,
        denied_actions_total,
        auto_declined_requests_total,
        steps: retained.steps,
        usage,
        cost: retained.cost.map_or(Cost::UNAVAILABLE, |(usd, scope)| {
            Cost::reported(usd, &scope)
        }),
        timestamps,
        duration_ms,
        exit: terminal.exit,
        events: EventRange {
            first_seq,
            last_seq,
            count: last_seq + 1 - first_seq,
        },
        evidence: EvidenceRef { folder, transcript },
        vendor_options: plan.vendor_options(),
        vendor: VendorFields {
            turn_id: accepted.and_then(|accepted| accepted.vendor_turn_id),
            data,
        },
    };
    // Design §6.4: every member has a fixed maximum, so the envelope fits
    // `ENVELOPE_MAX` by construction; there is no refusal path.
    debug_assert!(
        serde_json::to_vec(&envelope).is_ok_and(|bytes| bytes.len() <= via_store::ENVELOPE_MAX),
        "an envelope exceeds ENVELOPE_MAX"
    );
    envelope
}

/// Maps a typed route cause onto C1 §7.6 state, §8.2 class and stop reason.
fn route_disposition(cause: &RouteError) -> (&'static str, Option<FailureClass>, &'static str) {
    match cause {
        RouteError::Protocol { .. } => ("failed", Some(FailureClass::Protocol), "error"),
        RouteError::ProcessExited { .. } => ("failed", Some(FailureClass::ProcessExited), "error"),
        RouteError::Overflow { .. } => ("failed", Some(FailureClass::Overflow), "error"),
        RouteError::Store { .. } => ("failed", Some(FailureClass::Store), "error"),
        RouteError::Deadline { .. } => ("failed", Some(FailureClass::DeadlineWall), "deadline"),
        // C1 §7.6: Host-confirmed death of the persistent server.
        RouteError::ServerLost { .. } => ("failed", Some(FailureClass::ServerLost), "error"),
        // C1 §8.2: an adapter-side rejection before any vendor submission.
        RouteError::HandshakeRefused { .. } | RouteError::InvalidParam { .. } => {
            ("failed", Some(FailureClass::SubmitFailed), "error")
        }
        RouteError::ResumeMismatch { .. } => {
            ("failed", Some(FailureClass::ResumeMismatch), "error")
        }
        // Core settles a force stop itself, and `dispose` a stop order's
        // `Stopped`; this is only the C1 §7.6 force row.
        RouteError::ForceStopped { .. } | RouteError::Stopped { .. } => {
            ("cancelled", None, "interrupted")
        }
        // Input may have reached the vendor and no exit is confirmed (§7.6).
        RouteError::TransportLost { .. } => ("unknown", None, "error"),
    }
}

/// C1 §5 `failure.data` of an adapter-side `submit_failed`: the reason,
/// and with `invalid_param` the C1 parameter; never vendor text.
fn submit_data(cause: &RouteError) -> Option<serde_json::Value> {
    match cause {
        RouteError::HandshakeRefused { .. } => Some(json!({"reason": "handshake_refused"})),
        RouteError::InvalidParam { field, .. } => {
            Some(json!({"reason": "invalid_param", "field": field}))
        }
        RouteError::Protocol { .. }
        | RouteError::ProcessExited { .. }
        | RouteError::Overflow { .. }
        | RouteError::Store { .. }
        | RouteError::Deadline { .. }
        | RouteError::ServerLost { .. }
        | RouteError::ResumeMismatch { .. }
        | RouteError::ForceStopped { .. }
        | RouteError::Stopped { .. }
        | RouteError::TransportLost { .. } => None,
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

/// Design §2's disposition table (C1 §7.6; the first matching row wins)
/// over the turn's one result (AD4): its retained vendor terminal, if any,
/// and its evidence or typed failure. A failure stays the first cause, as
/// in S1; the retained terminal still gives the envelope its vendor stop
/// reason. `wall` is the turn's wall deadline: a `Deadline` coincident with
/// an order's `force_at` takes the order's row [r1.9]. The terminal's
/// cleanup fact (x.3.2 X0 item 6.5) is the stop's settled cleanup, or
/// without one the `TurnEnd`'s.
pub(super) fn dispose(
    accepted: bool,
    (vendor, outcome): (Option<&VendorTerminal>, Result<TurnEvidence, AdapterError>),
    order: Option<&StopOrder>,
    wall: tokio::time::Instant,
) -> Disposed {
    let quiescent = outcome
        .as_ref()
        .map_or_else(AdapterError::evidence, Clone::clone)
        .cleanup
        == Cleanup::Quiescent;
    let mut disposed = dispose_by(accepted, (vendor, outcome), order, wall);
    disposed.terminal.quiescent = disposed
        .stop
        .map_or(quiescent, |(_, cleanup)| cleanup == "quiescent");
    disposed
}

/// [`dispose`]'s table.
fn dispose_by(
    accepted: bool,
    (vendor, outcome): (Option<&VendorTerminal>, Result<TurnEvidence, AdapterError>),
    order: Option<&StopOrder>,
    wall: tokio::time::Instant,
) -> Disposed {
    let Some(order) = order else {
        // C1 §7.6: Core's deadline cancels the turn; Route stopped it.
        let stop = match &outcome {
            Err(AdapterError::Route(route))
                if matches!(route.cause, RouteError::Deadline { .. }) =>
            {
                Some(stop_outcome(
                    route.cleanup == Some(WireCleanup::Quiescent),
                    route.forced,
                    route.acknowledged,
                ))
            }
            Ok(_) | Err(_) => None,
        };
        return Disposed {
            terminal: classify(accepted, vendor, outcome),
            stop,
            cancel_cause: None,
        };
    };
    let cause = order.cause;
    let requested = match cause {
        StopCause::Cancel => Some(CancelCause::Cancel),
        StopCause::Close => Some(CancelCause::Close),
        StopCause::IdleDeadline | StopCause::Store | StopCause::Protocol => None,
    };
    match outcome {
        Ok(evidence) => {
            let cleanup = cleanup_word(evidence.cleanup == Cleanup::Quiescent);
            let interrupted =
                vendor.is_some_and(|vendor| vendor.status == VendorTerminalStatus::Interrupted);
            let mut terminal = classify(accepted, vendor, Ok(evidence));
            let stop = match (interrupted, cause) {
                (_, StopCause::Store) => {
                    terminal.fail(FailureClass::Store, STORE_STOP);
                    ("requested", cleanup)
                }
                (_, StopCause::Protocol) => {
                    terminal.fail(FailureClass::Protocol, PROTOCOL_STOP);
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
        Err(AdapterError::Route(route)) => stopped((route, vendor), cause, order, wall, requested),
        // A definite rejection or a resume mismatch: the turn fails as it
        // would without the order, which it outlived; the cleanup facts are
        // the rejection's own.
        Err(error) => {
            let cleanup = cleanup_word(error.evidence().cleanup == Cleanup::Quiescent);
            Disposed {
                terminal: failed_terminal(&error, vendor),
                stop: Some(("requested", cleanup)),
                cancel_cause: None,
            }
        }
    }
}

/// Message of a turn stopped because its own Store write failed.
const STORE_STOP: &str = "a turn event could not be recorded";

/// Message of a turn Core stopped because it refused the vendor's evidence
/// (a `protocol` order, Sol r4 R6): an acceptance naming a vendor turn the
/// lane keeps for another turn, or a token count, whose own message
/// [`TOKENS_STOP`] replaces this once the turn's record shows it.
pub(super) const PROTOCOL_STOP: &str = "Core refused the vendor's evidence for the turn";

/// Message of a turn stopped because its acceptance found every vendor
/// turn tombstone of its connection generation taken (C2 §4.1, critical
/// r1 #6).
pub(super) const OVERFLOW_STOP: &str = "the session's vendor turns exhausted their tombstones";

/// Message of a turn whose vendor reported a token count past `i64::MAX`
/// (review r1).
pub(super) const TOKENS_STOP: &str = "the vendor reported a token count that cannot be represented";

/// C1 §3.5 cleanup word: a settled result never carries `pending` (AD9).
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
    (route, vendor): (RouteFailure, Option<&VendorTerminal>),
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
    let (outcome, cleanup) = stop_outcome(quiescent, route.forced, route.acknowledged);
    let by_order = match route.cause {
        RouteError::Stopped { .. } => true,
        RouteError::Deadline { .. } => order.force_at.instant() == wall,
        RouteError::Protocol { .. }
        | RouteError::TransportLost { .. }
        | RouteError::ProcessExited { .. }
        | RouteError::Overflow { .. }
        | RouteError::Store { .. }
        | RouteError::ForceStopped { .. }
        | RouteError::ServerLost { .. }
        | RouteError::HandshakeRefused { .. }
        | RouteError::InvalidParam { .. }
        | RouteError::ResumeMismatch { .. } => false,
    };
    let launched = route.launched;
    let forced = route.forced;
    let shared = route.shared;
    let mut terminal = failed_terminal(&AdapterError::Route(route), vendor);
    if matches!(cause, StopCause::Store | StopCause::Protocol) {
        if cause == StopCause::Store {
            terminal.fail(FailureClass::Store, STORE_STOP);
        } else {
            terminal.fail(FailureClass::Protocol, PROTOCOL_STOP);
        }
        return Disposed {
            terminal,
            stop: Some((outcome, cleanup)),
            cancel_cause: None,
        };
    }
    if !by_order {
        // Deadline before the order's force, process exit, transport loss
        // and other failures keep their own row: the order did not stop
        // the turn, so it is no cause of it (C1 §7.6, fix r2 #3).
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
        StopCause::Cancel | StopCause::Close | StopCause::Store | StopCause::Protocol => {
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
                // A vendor may have run with neither stop nor terminal
                // proved. AD4, C1 §7.6 "Force deadline, shared server": a
                // shared server is never killed, so the outcome is unknown.
                terminal.state = "unknown";
                terminal.stop_reason = "error";
                (if shared { "unknown" } else { outcome }, cleanup)
            };
            Disposed {
                cancel_cause: requested
                    .filter(|_| matches!(terminal.state, "cancelled" | "unknown")),
                terminal,
                stop: Some(stop),
            }
        }
    }
}

/// C1 §7.6 rows 3–4 and §8.2 from the turn's retained vendor terminal: a
/// `completed` terminal is `completed`; a failed or interrupted one fails
/// with the class its hint suggests (`vendor_error` without one) and the
/// vendor's code and detail. Before acceptance the turn is
/// `submit_failed`. The adapter's normalized stop reason is kept (#34,
/// #36); the process exit is optional (#35).
pub(super) fn classify(
    accepted: bool,
    vendor: Option<&VendorTerminal>,
    outcome: Result<TurnEvidence, AdapterError>,
) -> Terminal {
    let evidence = match outcome {
        Ok(evidence) => evidence,
        Err(error) => return failed_terminal(&error, vendor),
    };
    let exit = exit_of(&evidence);
    let Some(vendor) = vendor else {
        // C2 §4.1: a turn ends well only with its terminal; nothing here
        // proves the vendor's result.
        let mut terminal = blank("failed", "error", exit);
        terminal.fail(
            FailureClass::Protocol,
            "the turn ended without a vendor terminal",
        );
        return terminal;
    };
    let failure = if accepted {
        match vendor.status {
            // C1 §7.6 row 3: a decoded `completed` is `completed`; the exit
            // and cleanup stay independent evidence (C1 §7.5).
            VendorTerminalStatus::Completed => None,
            VendorTerminalStatus::Interrupted | VendorTerminalStatus::Failed => Some(failure(
                vendor.class_hint.map_or(FailureClass::VendorError, class),
                vendor
                    .detail
                    .clone()
                    .unwrap_or_else(|| "the vendor reported a failed turn".to_owned()),
                vendor.vendor_code.clone(),
            )),
        }
    } else {
        // A definite failure before acceptance keeps the vendor's code and
        // detail (AD5).
        Some(failure(
            FailureClass::SubmitFailed,
            vendor
                .detail
                .clone()
                .unwrap_or_else(|| "the vendor did not accept the submission".to_owned()),
            vendor.vendor_code.clone(),
        ))
    };
    let stop_reason = match (&failure, accepted, vendor.status) {
        (None, _, _) | (Some(_), true, _) => stop_word(vendor.stop_reason),
        (Some(_), false, VendorTerminalStatus::Interrupted) => "interrupted",
        (Some(_), false, VendorTerminalStatus::Completed | VendorTerminalStatus::Failed) => "error",
    };
    Terminal {
        state: if failure.is_none() {
            "completed"
        } else {
            "failed"
        },
        failure,
        stop_reason,
        vendor_stop_reason: Some(vendor.vendor_stop_reason.clone()),
        // The drive sets the text it accumulated from `final_text` pieces.
        final_text: Some(String::new()),
        final_text_file: None,
        exit,
        warnings: Vec::new(),
        cancel: None,
        // The drive sets the turn's cleanup fact.
        quiescent: false,
    }
}

/// C1 §8.2's class for an adapter's hint (#18, AD11).
fn class(hint: ClassHint) -> FailureClass {
    match hint {
        ClassHint::Auth => FailureClass::Auth,
        ClassHint::RateLimit => FailureClass::RateLimit,
        ClassHint::ContextExceeded => FailureClass::ContextExceeded,
        ClassHint::BudgetExceeded => FailureClass::BudgetExceeded,
        ClassHint::VendorError => FailureClass::VendorError,
        ClassHint::Protocol => FailureClass::Protocol,
        ClassHint::ResumeMismatch => FailureClass::ResumeMismatch,
    }
}

/// C1 §5's `stop_reason` for the adapter's normalized one (AD5).
fn stop_word(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxSteps => "max_steps",
        StopReason::Budget => "budget",
        StopReason::Refusal => "refusal",
        StopReason::Interrupted => "interrupted",
        StopReason::Error => "error",
        StopReason::Other => "other",
    }
}

/// The envelope's `exit`: the confirmed process exit, `null` without one
/// (server routes).
fn exit_of(evidence: &TurnEvidence) -> Option<Exit> {
    evidence.exit.map(|exit| Exit {
        code: exit.code,
        signal: exit.signal,
    })
}

/// A terminal with no failure yet, no text and no cancel.
pub(super) fn blank(
    state: &'static str,
    stop_reason: &'static str,
    exit: Option<Exit>,
) -> Terminal {
    Terminal {
        state,
        failure: None,
        stop_reason,
        vendor_stop_reason: None,
        final_text: Some(String::new()),
        final_text_file: None,
        exit,
        warnings: Vec::new(),
        cancel: None,
        // The drive sets the turn's cleanup fact.
        quiescent: false,
    }
}

/// Keeps the typed cause, the undecoded message's note and the confirmed
/// exit of a failed drive; a retained vendor terminal gives its stop
/// reason (AD4).
fn failed_terminal(error: &AdapterError, vendor: Option<&VendorTerminal>) -> Terminal {
    let mut message = error.to_string();
    let mut vendor_code = None;
    let exit = exit_of(&error.evidence());
    let (state, class, stop_reason, data) = match error {
        AdapterError::Route(route) => {
            let (state, class, stop_reason) = route_disposition(&route.cause);
            (state, class, stop_reason, submit_data(&route.cause))
        }
        // C1 §8.2: a definite rejection before acceptance; only the
        // adapter-side parameter rejection names its reason.
        AdapterError::Rejected { reason, .. } => {
            let data = match reason {
                StartRejected::InvalidParam { field } => {
                    Some(json!({"reason": "invalid_param", "field": field}))
                }
                // The vendor's own code and bounded detail (AD5, C1 §5).
                StartRejected::VendorError(code, detail) => {
                    vendor_code = Some(code.clone());
                    message.clone_from(detail);
                    None
                }
                StartRejected::BoundUnsupported(_)
                | StartRejected::SessionGone
                | StartRejected::Protocol(_) => None,
            };
            ("failed", Some(FailureClass::SubmitFailed), "error", data)
        }
        // C2 §2 Reopen: the vendor returned another session.
        AdapterError::ResumeMismatch { .. } => {
            ("failed", Some(FailureClass::ResumeMismatch), "error", None)
        }
        // No process was launched for the submission.
        AdapterError::Unavailable => ("failed", Some(FailureClass::SubmitFailed), "error", None),
        // The driver lost the turn's task: nothing proves what the vendor
        // did.
        AdapterError::TaskFailed => ("unknown", None, "error", None),
        AdapterError::Open(_) => ("failed", Some(FailureClass::Protocol), "error", None),
    };
    let mut terminal = blank(state, stop_reason, exit);
    terminal.failure = class.map(|class| {
        let mut failure = failure(class, message, vendor_code);
        failure.data = data;
        failure
    });
    terminal.vendor_stop_reason = vendor.map(|vendor| vendor.vendor_stop_reason.clone());
    terminal
}

/// Test builds only (Task 4 design §6.4, §13.2; C1 §5): the encoded
/// envelope with every member at its maximum and worst-case JSON escaping,
/// through the same assembly as a turn's: `denied` denials and `declined`
/// declines whose free strings are `entry_bytes` long, the failure message
/// twice its maximum, every warning code twice with its `message` at
/// 1 KiB and its `data` at 4 KiB encoded, the inline structured output at
/// 32 KiB and a spill file named as well, 16 leftovers, evidence paths at
/// 2 KiB, `cwd` and the transcript hint at 4 KiB encoded, and every ID at
/// 1 KiB of escaped characters.
#[cfg(feature = "test-failpoints")]
#[expect(
    clippy::too_many_lines,
    reason = "one maximal envelope, member by member"
)]
pub fn envelope_at_maximum(
    denied: u64,
    declined: u64,
    entry_bytes: usize,
) -> Result<String, crate::ApiError> {
    use super::lane::Retained;
    use crate::api::{
        Cancel, FINAL_TEXT_INLINE, FinalTextFile, Kept, Requested, STRUCTURED_OUTPUT_INLINE,
        StructuredOutputFile, Tokens, maxima,
    };
    let bad = |_| crate::ApiError::STORE;
    let session = SessionId::try_from("s_zzzzzzzzzzzz").map_err(bad)?;
    let turn = TurnNumber::try_from(u32::MAX).map_err(bad)?;
    let at = "9999-12-31T23:59:59.999Z";
    // C2 A1: IDs, versions, stop reasons and codes are 1 KiB of UTF-8,
    // each byte here a control character encoded as `\u00XX`.
    let id = || "\u{1}".repeat(1024);
    let path = maxima::escaped(2 * 1024);
    let mut warnings = Vec::new();
    for code in maxima::WARNING_CODES.iter().chain(&maxima::WARNING_CODES) {
        warnings.push(maxima::warning(code));
    }
    let terminal = Terminal {
        state: "cancelled",
        failure: Some(failure(
            FailureClass::VendorError,
            "\u{1}".repeat(4096),
            Some(id()),
        )),
        stop_reason: "interrupted",
        vendor_stop_reason: Some(id()),
        // Both the inline text and a named file: more than a turn carries.
        final_text: Some("a".repeat(FINAL_TEXT_INLINE - 2)),
        final_text_file: Some(FinalTextFile {
            path: path.clone(),
            bytes: u64::MAX,
            truncated: true,
        }),
        exit: Some(Exit {
            code: Some(i32::MIN),
            signal: Some(i32::MIN),
        }),
        warnings,
        cancel: Some(Cancel {
            outcome: "acknowledged",
            cleanup: "uncertain",
            requested_at: at.to_owned(),
            settled_at: at.to_owned(),
        }),
        quiescent: false,
    };
    let accepted = Accepted {
        at: at.to_owned(),
        vendor_turn_id: Some(id()),
    };
    let timestamps = Timestamps {
        queued_at: at.to_owned(),
        submitted_at: Some(at.to_owned()),
        accepted_at: Some(at.to_owned()),
        ended_at: at.to_owned(),
    };
    let mut denied_list = Kept::default();
    for index in 0..denied {
        denied_list.push(maxima::denied(entry_bytes, at, index + 1));
    }
    let mut declined_list = Kept::default();
    for index in 0..declined {
        declined_list.push(maxima::declined(entry_bytes, at, index + 1));
    }
    let serde_json::Value::Object(data) = maxima::object_of(16 * 1024) else {
        return Err(crate::ApiError::STORE);
    };
    let vendor = VendorRecord {
        identity: Some(Identity {
            vendor_session_id: id(),
            transcript: Some(maxima::escaped(4 * 1024)),
        }),
        denied: denied_list,
        declined: declined_list,
        instance: Some((Some(id()), true)),
        retained: Some(Retained {
            vendor_stop_reason: id(),
            // Both the inline value and a named file: more than a turn carries.
            structured_output: Some(maxima::object_of(STRUCTURED_OUTPUT_INLINE)),
            structured_output_file: Some(StructuredOutputFile {
                path: path.clone(),
                bytes: u64::MAX,
            }),
            output_invalid: None,
            steps: Some(u64::MAX),
            usage: None,
            cost: Some((f64::MAX, "session_cumulative".to_owned())),
            // AD6: the terminal's vendor data, at most 16 KiB encoded.
            vendor: data,
        }),
        ..VendorRecord::default()
    };
    let max = Some(u64::MAX);
    let usage = Usage::reported(
        Some(Tokens {
            input: max,
            cached_input: max,
            output: max,
            reasoning_output: max,
            total: max,
        }),
        true,
        "vendor_interval",
    );
    let mut envelope = assemble(
        (&session, turn, &TurnPlan::default()),
        terminal,
        Some(accepted),
        (Some(maxima::escaped(4 * 1024)), Some(path)),
        (timestamps, Some(u64::MAX)),
        (1, u64::MAX - 1),
        (usage, true),
        vendor,
    );
    // C2 A1: the harness, route and versions at 1 KiB each.
    envelope.harness = id();
    envelope.plan.route = id();
    envelope.plan.adapter_version = id();
    envelope.model = Requested {
        requested: maxima::escaped(1024),
        resolved: maxima::escaped(1024),
    };
    envelope.effort = Requested {
        requested: Some(maxima::escaped(1024)),
        resolved: Some(maxima::escaped(1024)),
    };
    envelope.bound = maxima::bound();
    envelope.vendor_options = maxima::object_of(16 * 1024);
    // S-LEFTOVER's report at its maximum (C1 §5).
    envelope.leftovers = Some(maxima::leftovers(at));
    serde_json::to_string(&envelope).map_err(|_| crate::ApiError::STORE)
}

#[cfg(test)]
mod tests {
    use super::{FailureClass, TurnNumber};
    use via_adapters::{AdapterError, RouteError, RouteFailure};
    use via_store::CancelCause;

    #[expect(
        clippy::needless_pass_by_value,
        reason = "the cases build their failure inline"
    )]
    fn failed_terminal(error: AdapterError) -> super::Terminal {
        super::failed_terminal(&error, None)
    }

    fn route(cause: RouteError, undecoded: Option<&str>) -> AdapterError {
        AdapterError::Route(RouteFailure {
            cause,
            undecoded: undecoded.map(str::to_owned),
            exit: None,
            launched: false,
            cleanup: None,
            forced: false,
            journal_uncertain: false,
            acknowledged: false,
            shared: false,
        })
    }

    /// Causes the fake vendor cannot trigger end to end keep their C1 §8.2
    /// class; a kept undecoded message is named in `failure.message` (Task 4
    /// design §7.3).
    #[test]
    fn route_causes_keep_their_c1_disposition() {
        let turn = TurnNumber::try_from(1).unwrap();
        for (cause, class) in [
            (RouteError::Overflow { turn }, FailureClass::Overflow),
            (
                RouteError::Store {
                    turn,
                    kind: via_adapters::StoreFailure::Evidence,
                },
                FailureClass::Store,
            ),
            (RouteError::Deadline { turn }, FailureClass::DeadlineWall),
            (
                RouteError::ProcessExited { turn },
                FailureClass::ProcessExited,
            ),
        ] {
            let terminal = failed_terminal(route(cause, None));
            assert_eq!(terminal.state, "failed");
            assert_eq!(terminal.failure.map(|failure| failure.class), Some(class));
        }
        let note = "undecodable vendor message: 9 bytes; first 9 in /s/undecoded.bin";
        let protocol = failed_terminal(route(
            RouteError::Protocol {
                turn,
                detail: "malformed known fake message",
            },
            Some(note),
        ));
        let failure = protocol.failure.expect("a protocol failure");
        assert_eq!(failure.class, FailureClass::Protocol);
        assert!(
            failure.message.ends_with(&format!("; {note}")),
            "{}",
            failure.message
        );
        let lost = failed_terminal(route(RouteError::TransportLost { turn }, None));
        assert_eq!(lost.state, "unknown");
        assert!(lost.failure.is_none());
    }

    fn vendor_terminal(status: via_adapters::VendorTerminalStatus) -> via_adapters::VendorTerminal {
        via_adapters::VendorTerminal {
            at: tokio::time::Instant::now(),
            status,
            stop_reason: via_adapters::StopReason::Error,
            vendor_stop_reason: "error".to_owned(),
            vendor_code: Some("E429".to_owned()),
            class_hint: None,
            detail: Some("quota exhausted".to_owned()),
            structured_output: None,
            steps: None,
            usage: None,
            cost: None,
            vendor: None,
        }
    }

    /// Sol r1 F9: a definite vendor rejection keeps the vendor's code and
    /// its bounded detail in the failure, as does a vendor terminal that
    /// failed before acceptance; only the class is `submit_failed`.
    #[test]
    fn a_vendor_rejection_keeps_its_code_and_detail() {
        let evidence = via_adapters::TurnEvidence {
            exit: None,
            cleanup: via_adapters::Cleanup::Quiescent,
            journal_uncertain: false,
        };
        let rejected = failed_terminal(AdapterError::Rejected {
            reason: via_adapters::StartRejected::VendorError(
                "E429".to_owned(),
                "quota exhausted".to_owned(),
            ),
            evidence: evidence.clone(),
        });
        let failure = rejected.failure.expect("a rejection fails");
        assert_eq!(failure.class, FailureClass::SubmitFailed);
        assert_eq!(failure.vendor_code.as_deref(), Some("E429"));
        assert_eq!(failure.message, "quota exhausted");

        let vendor = vendor_terminal(via_adapters::VendorTerminalStatus::Failed);
        let before = super::classify(false, Some(&vendor), Ok(evidence));
        let failure = before.failure.expect("a failure before acceptance");
        assert_eq!(failure.class, FailureClass::SubmitFailed);
        assert_eq!(failure.vendor_code.as_deref(), Some("E429"));
        assert_eq!(failure.message, "quota exhausted");
    }

    /// Sol r4 R6: a turn stopped by a `protocol` order, as an acceptance
    /// naming a vendor turn the lane keeps for another turn is, fails
    /// `protocol` with a message that claims no unrepresentable token
    /// count, whether the vendor ended the turn or Route stopped it.
    #[test]
    fn a_protocol_order_claims_no_token_count() {
        let turn = TurnNumber::try_from(1).unwrap();
        let now = tokio::time::Instant::now();
        let order = crate::engine::queue::StopSpec::Protocol.order(
            "2026-01-01T00:00:00.000Z".to_owned(),
            now,
            None,
        );
        let evidence = via_adapters::TurnEvidence {
            exit: None,
            cleanup: via_adapters::Cleanup::Quiescent,
            journal_uncertain: false,
        };
        for outcome in [Ok(evidence), Err(route(RouteError::Stopped { turn }, None))] {
            let disposed = super::dispose(true, (None, outcome), Some(&order), now);
            let failure = disposed.terminal.failure.expect("a protocol failure");
            assert_eq!(failure.class, FailureClass::Protocol);
            assert_ne!(failure.message, super::TOKENS_STOP);
            assert_eq!(failure.message, super::PROTOCOL_STOP);
        }
    }

    /// Fix round 1 #3, #5 (runtime §6, C1 §7.6): a caller `cancel` or
    /// `close` that stopped a turn ending `unknown` is recorded as its
    /// cause, whether the force passed unanswered on a shared server or the
    /// transport was lost under the order; a Core deadline's order is no
    /// caller's, so an idle stop followed by transport loss records none.
    #[test]
    fn an_unknown_turn_records_the_callers_stop() {
        let turn = TurnNumber::try_from(1).unwrap();
        let now = tokio::time::Instant::now();
        let at = "2026-01-01T00:00:00.000Z".to_owned();
        let order = |spec: crate::engine::queue::StopSpec| spec.order(at.clone(), now, None);
        let cancel = order(crate::engine::queue::StopSpec::Cancel {
            force_after: std::time::Duration::ZERO,
        });
        let close = order(crate::engine::queue::StopSpec::Close {
            mode: crate::CloseMode::Graceful,
            deadline: now,
        });
        let idle = order(crate::engine::queue::StopSpec::Idle);
        let unanswered = || {
            AdapterError::Route(RouteFailure {
                launched: true,
                shared: true,
                ..route_failure(RouteError::Stopped { turn })
            })
        };
        let lost = || {
            AdapterError::Route(RouteFailure {
                launched: true,
                ..route_failure(RouteError::TransportLost { turn })
            })
        };
        for (outcome, order, cause) in [
            (unanswered(), &cancel, Some(CancelCause::Cancel)),
            (unanswered(), &close, Some(CancelCause::Close)),
            // Fix round 2 #3: an order in force that did not stop the turn
            // is not its cause.
            (lost(), &cancel, None),
            (lost(), &idle, None),
        ] {
            let disposed = super::dispose(true, (None, Err(outcome)), Some(order), now);
            assert_eq!(disposed.terminal.state, "unknown");
            assert_eq!(disposed.cancel_cause, cause, "{:?}", order.cause);
        }
    }

    fn route_failure(cause: RouteError) -> RouteFailure {
        let AdapterError::Route(failure) = route(cause, None) else {
            unreachable!("a route failure")
        };
        failure
    }

    /// C1 §5 (spill amendment): a warning keeps a `message` of at most
    /// 1 KiB, cut at a character boundary, and `data` of at most 4 KiB
    /// encoded, else none; an evidence `transcript` hint over 4 KiB encoded
    /// is `null`, one at 4 KiB is kept.
    #[test]
    fn the_envelope_caps_warning_members_and_the_transcript() {
        use crate::api::{Timestamps, Warning};
        use crate::engine::lane::{Identity, VendorRecord};
        let session = crate::SessionId::try_from("s_aaaaaaaaaaaa").expect("a session ID");
        let at = "2026-01-01T00:00:00.000Z".to_owned();
        let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
        let envelope = |transcript: String| {
            let mut terminal = super::blank("completed", "end_turn", None);
            // A message 1 KiB + 1 encoded, its last character escaped.
            let long = leak(format!("{}\u{1}", "w".repeat(1017)));
            terminal.warnings = vec![
                Warning::new("cancel_cleanup_uncertain", long)
                    .with_data(serde_json::json!({"pad":"p".repeat(4 * 1024 - 9)})),
                Warning::new("deprecated", "short")
                    .with_data(serde_json::json!({"pad":"p".repeat(4 * 1024 - 10)})),
            ];
            let vendor = VendorRecord {
                identity: Some(Identity {
                    vendor_session_id: "v".to_owned(),
                    transcript: Some(transcript),
                }),
                instance: Some((Some("1".to_owned()), true)),
                ..VendorRecord::default()
            };
            let envelope = super::turn_envelope(
                (
                    &session,
                    TurnNumber::try_from(1).expect("a turn"),
                    &crate::intake::TurnPlan::default(),
                ),
                terminal,
                None,
                (None, None),
                (
                    Timestamps {
                        queued_at: at.clone(),
                        submitted_at: None,
                        accepted_at: None,
                        ended_at: at.clone(),
                    },
                    None,
                ),
                (1, 1),
                vendor,
            );
            serde_json::to_value(&envelope).expect("an envelope encodes")
        };
        // `"` + 4,094 bytes of `\u0001` and `a` + `"`: 4 KiB encoded.
        let at_cap = format!("{}aa", "\u{1}".repeat(682));
        let kept = envelope(at_cap.clone());
        assert_eq!(kept["evidence"]["transcript"], at_cap.as_str());
        let over = envelope(format!("{at_cap}a"));
        assert!(
            over["evidence"]["transcript"].is_null(),
            "{}",
            over["evidence"]
        );
        let warnings = kept["warnings"].as_array().expect("warnings");
        assert_eq!(warnings[0]["message"], "w".repeat(1017));
        assert!(warnings[0].get("data").is_none(), "{}", warnings[0]);
        assert_eq!(
            serde_json::to_vec(&warnings[1]["data"])
                .expect("data")
                .len(),
            4 * 1024
        );
    }
}
