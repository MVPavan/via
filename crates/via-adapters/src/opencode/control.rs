//! `opencode.md` §7.4, §8: one bounded stop for this input, never the shared server.

use std::sync::Arc;

use tokio::time::Instant;
use via_routes::opencode::Server;
use via_routes::opencode::state::InputPhase;
use via_routes::opencode::turn::{self as requests, CancelOutcome, InterruptOutcome};

use super::delivery::{Decision, Delivery};
use super::driver::Turn;
use super::execution;
use crate::driver::ForceWatch;
use crate::driver::turn::CLEANUP_ALLOWANCE;
use crate::{
    AdapterError, Cleanup, Deadline, RouteError, StopAck, StopWatch, TurnEnd, WireCleanup,
};

/// C2 §4.1, `opencode.md` §7.4: the caller order and independent acknowledgement lane.
pub(super) struct Controls {
    pub(super) stop: StopWatch,
    pub(super) force: ForceWatch,
    pub(super) stop_ack: StopAck,
}

/// The two native stop operations, claimed once under the server router (§7.4).
#[derive(Clone, Copy)]
enum Action {
    CancelInput(crate::TurnNumber),
    Interrupt,
}

/// A complete response is not a stream acknowledgement (§8).
#[derive(Clone, Copy)]
enum Effect {
    Settled,
    Inconclusive,
    Authentication,
}

/// §7.4: the stop response and native tool grace retain separate absolute bounds.
#[derive(Clone, Copy)]
struct StopBounds {
    by: Deadline,
    tool_grace: std::time::Duration,
    wall: Deadline,
}

/// §7.4: the turn pipeline stopped; its current sent HTTP exchange stays generation-owned.
pub(super) async fn finish(facts: &mut Turn<'_>, controls: &Controls) -> TurnEnd {
    let initial_cause = cause(facts, controls);
    let Some(running) = facts.running.as_mut() else {
        return execution::ordered_result(facts, Some(initial_cause));
    };
    let server = Arc::clone(&running.server);
    let session = running.session.clone();
    let input = running.input.clone();
    let delivery = Arc::clone(&running.delivery);
    facts.submitted = running.sent.is_sent();
    // The generation owns the pending prompt exchange and its accounting. A stop
    // changes only the input's native cancel/interrupt path, never its HTTP timeout.
    if !facts.submitted {
        return result(facts, controls, Some(initial_cause), Cleanup::Quiescent);
    }
    let initial_by = stop_by(facts, controls);
    let mut stop_changes = controls.stop.clone();
    let changed = server.routing().notify();
    let mut native_cleanup = None;
    loop {
        let by = latest_by(initial_by, controls);
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if delivery.acknowledged_until(native_cutoff(controls)) {
            controls.stop_ack.acknowledged();
        }
        // A retained natural terminal decides without any session-wide interrupt.
        let terminal = delivery.decision();
        tokio::pin!(terminal);
        let action = claim(&server, &session, &input, &delivery, facts.number);
        // A ready decision has priority over polling a new stop request. All three
        // waits are read-only; dropping them retains the original terminal evidence.
        let decision = tokio::select! {
            biased;
            decision = &mut terminal => Some(decision),
            () = tokio::time::sleep_until(by.instant()) => None,
            () = forced(controls.force.clone()) => None,
            () = std::future::ready(()), if action.is_some() => {
                if let Some(action) = action {
                    native_cleanup = send(
                        controls,
                        &server,
                        &session,
                        &input,
                        &delivery,
                        action,
                        StopBounds { by, tool_grace: facts.tool_grace, wall: facts.wall },
                    ).await;
                }
                continue;
            },
            () = stop_changed(&mut stop_changes) => continue,
            () = &mut notified => continue,
        };
        match decision {
            Some(Decision::Terminal) => {
                if delivery.terminal_at().is_some_and(|at| {
                    native_cutoff(controls).is_some_and(|cutoff| at > cutoff.instant())
                }) {
                    return result(
                        facts,
                        controls,
                        Some(cause(facts, controls)),
                        Cleanup::Uncertain,
                    );
                }
                if delivery.acknowledged_until(native_cutoff(controls)) {
                    controls.stop_ack.acknowledged();
                }
                let cleanup = retained_cleanup(
                    &delivery,
                    &server,
                    controls,
                    facts.tool_grace,
                    facts.wall,
                    native_cleanup,
                )
                .await;
                return result(facts, controls, Some(initial_cause), cleanup);
            }
            Some(Decision::Stopped) => {
                return result(facts, controls, None, Cleanup::Uncertain);
            }
            None => {
                return result(
                    facts,
                    controls,
                    Some(cause(facts, controls)),
                    Cleanup::Uncertain,
                );
            }
        }
    }
}

/// §7.4: reuse the frozen verdict or wait under the original native terminal's grace.
async fn retained_cleanup(
    delivery: &Delivery,
    server: &Server,
    controls: &Controls,
    tool_grace: std::time::Duration,
    wall: Deadline,
    retained: Option<Cleanup>,
) -> Cleanup {
    if let Some(cleanup) = retained {
        return cleanup;
    }
    let by = Deadline::at(
        (delivery.terminal_at().unwrap_or_else(Instant::now) + tool_grace).min(wall.instant()),
    );
    // None of these waits consumes retained terminal or cleanup evidence on cancellation.
    tokio::select! {
        cleanup = delivery.cleanup(by) => cleanup,
        _end = server.wait_end() => Cleanup::Uncertain,
        () = forced(controls.force.clone()) => Cleanup::Uncertain,
    }
}

/// §7.4: the router's claims make the cancel/interrupt sequence server-scoped and once-only.
fn claim(
    server: &Server,
    session: &str,
    input: &str,
    delivery: &Delivery,
    turn: crate::TurnNumber,
) -> Option<Action> {
    let phase = server
        .routing()
        .state(session)
        .and_then(|state| state.last)
        .filter(|last| last.input_id == input)
        .map(|last| last.phase);
    if phase == Some(InputPhase::Delivered) || delivery.delivered() {
        server
            .routing()
            .claim_interrupt(session, turn)
            .then_some(Action::Interrupt)
    } else {
        server
            .routing()
            .claim_inbox_cancel(session, turn)
            .then_some(Action::CancelInput(turn))
    }
}

/// §7.4: cancellation uses the original order's cutoff; wall cleanup gets S1's allowance.
fn stop_by(facts: &Turn<'_>, controls: &Controls) -> Deadline {
    if let Some(at) = *controls.force.borrow() {
        return Deadline::at(at);
    }
    if let Some(order) = controls.stop.borrow().as_ref() {
        return Deadline::at(order.force_at.instant().min(facts.wall.instant()));
    }
    Deadline::at(if Instant::now() >= facts.wall.instant() {
        facts.wall.instant() + CLEANUP_ALLOWANCE
    } else {
        (Instant::now() + CLEANUP_ALLOWANCE).min(facts.wall.instant())
    })
}

/// §7.4, C2 §4.1: later orders may shorten the native acknowledgement window.
fn native_cutoff(controls: &Controls) -> Option<Deadline> {
    let stop = controls
        .stop
        .borrow()
        .as_ref()
        .map(|order| order.force_at.instant());
    let force = *controls.force.borrow();
    match (stop, force) {
        (Some(stop), Some(force)) => Some(Deadline::at(stop.min(force))),
        (Some(at), None) | (None, Some(at)) => Some(Deadline::at(at)),
        (None, None) => None,
    }
}

/// §8: tightening a caller order never extends the in-flight request's timeout.
fn latest_by(initial: Deadline, controls: &Controls) -> Deadline {
    Deadline::at(native_cutoff(controls).map_or(initial.instant(), |cutoff| {
        initial.instant().min(cutoff.instant())
    }))
}

/// C2 §4.1: a watch change wakes the original request; it never resends its bytes.
async fn stop_changed(stop: &mut StopWatch) {
    if stop.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn cause(facts: &Turn<'_>, controls: &Controls) -> RouteError {
    if controls.force.borrow().is_some() {
        RouteError::ForceStopped { turn: facts.number }
    } else if controls.stop.borrow().is_some() || facts.driver.cancel.is_cancelled() {
        RouteError::Stopped { turn: facts.number }
    } else {
        RouteError::Deadline { turn: facts.number }
    }
}

/// §8: dropping the owner preserves an incomplete request's first-byte evidence.
struct PendingStop<'a> {
    server: &'a Server,
    session: &'a str,
    sent: &'a requests::SentTracker,
    handled: bool,
}

impl Drop for PendingStop<'_> {
    fn drop(&mut self) {
        if !self.handled {
            if self.sent.is_sent() {
                self.server.drain();
            } else {
                self.server
                    .routing()
                    .request_completed(self.session, requests::Sent::No);
            }
        }
    }
}

/// §7.4, §8: stop mutation facts become authoritative only at their first byte.
fn stop_tracker(
    server: &Arc<Server>,
    session: &str,
    input: &str,
    delivery: &Arc<Delivery>,
    action: Action,
) -> requests::SentTracker {
    requests::SentTracker::new({
        let delivery = Arc::clone(delivery);
        let server = Arc::clone(server);
        let session = session.to_owned();
        let input = input.to_owned();
        move || {
            {
                let mut routing = server.routing();
                routing.request_sent();
                if matches!(action, Action::CancelInput(_)) {
                    routing.inbox_cancel_sent(&session, &input);
                }
            }
            if matches!(action, Action::Interrupt) {
                delivery.note_interrupt_sent();
            }
        }
    })
}

/// §8: reserve the stop pool independently of a pending general prompt.
async fn send(
    controls: &Controls,
    server: &Arc<Server>,
    session: &str,
    input: &str,
    delivery: &Arc<Delivery>,
    action: Action,
    bounds: StopBounds,
) -> Option<Cleanup> {
    {
        let mut routing = server.routing();
        routing.request_started(session);
        if routing.failure().is_some() {
            return None;
        }
    }
    let sent = stop_tracker(server, session, input, delivery, action);
    let mut pending = PendingStop {
        server,
        session,
        sent: &sent,
        handled: false,
    };
    let mut noticed = false;
    let mut stop_changes = controls.stop.clone();
    let changed = server.routing().notify();
    let mut native_cleanup = None;
    let outcome = {
        let request = native_request(server, session, input, action, bounds.by, &sent);
        tokio::pin!(request);
        // §7.4: release the terminal fence and drive admitted tool ends while
        // retaining the same HTTP future. A completed verdict is never recomputed.
        let cleanup = async {
            match delivery.decision().await {
                Decision::Stopped => Cleanup::Uncertain,
                Decision::Terminal => {
                    let by = Deadline::at(
                        (delivery.terminal_at().unwrap_or_else(Instant::now) + bounds.tool_grace)
                            .min(bounds.wall.instant()),
                    );
                    delivery.cleanup(by).await
                }
            }
        };
        tokio::pin!(cleanup);
        loop {
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Action::CancelInput(turn) = action
                && let Some(Action::Interrupt) = claim(server, session, input, delivery, turn)
            {
                let cancel_sent = sent.is_sent();
                // §7.4: keep the original DELETE socket owned while the second
                // reserved stop slot interrupts its delivered input. Only a cancel
                // can enter here, so boxed recursion has depth one.
                let followed_cleanup = Box::pin(send(
                    controls,
                    server,
                    session,
                    input,
                    delivery,
                    Action::Interrupt,
                    bounds,
                ))
                .await;
                native_cleanup = native_cleanup.or(followed_cleanup);
                if !cancel_sent {
                    return native_cleanup;
                }
                continue;
            }
            let response_by = latest_by(bounds.by, controls);
            // Dropping the socket fixes SentTracker; acknowledgement reporting is
            // independent, so a held HTTP response cannot hide native stop evidence.
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(response_by.instant()) => break None,
                () = forced(controls.force.clone()) => break None,
                () = stop_changed(&mut stop_changes) => {},
                () = &mut notified => {},
                cleanup = &mut cleanup, if native_cleanup.is_none() => {
                    native_cleanup = Some(cleanup);
                },
                outcome = &mut request => break Some(outcome),
                decision = delivery.decision(), if !noticed => {
                    if matches!(decision, Decision::Stopped) {
                        break None;
                    }
                    noticed = true;
                    if delivery.acknowledged_until(native_cutoff(controls)) {
                        controls.stop_ack.acknowledged();
                    }
                },
            }
        }
    };
    finish_request(server, session, delivery, outcome, &sent);
    pending.handled = true;
    native_cleanup
}

/// §8: publish response uncertainty before releasing the session's request count.
fn finish_request(
    server: &Server,
    session: &str,
    delivery: &Delivery,
    outcome: Option<Result<Effect, requests::HttpError>>,
    sent: &requests::SentTracker,
) {
    let complete = match &outcome {
        Some(Ok(_)) => true,
        Some(Err(error)) => error.sent == requests::Sent::No || error.is_response_limit(),
        None => !sent.is_sent(),
    };
    match outcome {
        Some(Ok(Effect::Authentication)) => server.fail_protocol(),
        Some(Ok(Effect::Inconclusive)) => server.drain(),
        Some(Err(error)) if error.is_response_limit() => {
            let detected_at = Instant::now();
            server.drain();
            server
                .routing()
                .response_limit(session, delivery.owner(), detected_at);
            delivery.response_limit();
        }
        Some(Err(error)) if error.sent == requests::Sent::Maybe => server.drain(),
        None if sent.is_sent() => server.drain(),
        Some(Ok(Effect::Settled) | Err(_)) | None => {}
    }
    // A successor cannot become eligible before an inconclusive response fences setup.
    if complete {
        let evidence = if sent.is_sent() {
            requests::Sent::Maybe
        } else {
            requests::Sent::No
        };
        server.routing().request_completed(session, evidence);
    }
}

/// §8: complete native response tables, separate from stream acknowledgement.
async fn native_request(
    server: &Server,
    session: &str,
    input: &str,
    action: Action,
    by: Deadline,
    sent: &requests::SentTracker,
) -> Result<Effect, requests::HttpError> {
    match action {
        Action::CancelInput(_) => {
            requests::cancel_input_tracked(server.http(), session, input, by, sent)
                .await
                .map(|outcome| match outcome {
                    CancelOutcome::Settled => Effect::Settled,
                    CancelOutcome::Status(401) => Effect::Authentication,
                    CancelOutcome::Status(_) => Effect::Inconclusive,
                })
        }
        Action::Interrupt => requests::interrupt_tracked(server.http(), session, by, sent)
            .await
            .map(|outcome| match outcome {
                InterruptOutcome::Settled { .. } => Effect::Settled,
                InterruptOutcome::Status(401) => Effect::Authentication,
                InterruptOutcome::Status(_) | InterruptOutcome::Inconclusive => {
                    Effect::Inconclusive
                }
            }),
    }
}

async fn forced(mut force: ForceWatch) {
    if force.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// C2 §4.1: native acknowledgement and tool cleanup remain separate evidence.
fn result(
    facts: &mut Turn<'_>,
    controls: &Controls,
    cause: Option<RouteError>,
    cleanup: Cleanup,
) -> TurnEnd {
    let acknowledged = facts
        .running
        .as_ref()
        .is_some_and(|running| running.delivery.acknowledged_until(native_cutoff(controls)));
    if acknowledged {
        controls.stop_ack.acknowledged();
    }
    if let Some(running) = facts.running.as_mut() {
        running.cleanup = Some(cleanup);
        running.acknowledgement_cutoff = native_cutoff(controls);
    }
    let cause = cause.map(|cause| {
        if facts.running.as_ref().is_some_and(|running| {
            running.server.failure() == Some(via_routes::codex::LossCause::Protocol)
        }) {
            super::delivery::generation_route_error(
                via_routes::codex::LossCause::Protocol,
                facts.number,
            )
        } else {
            cause
        }
    });
    let wall_cleanup = matches!(cause, Some(RouteError::Deadline { .. }));
    let mut end = execution::ordered_result(facts, cause);
    if wall_cleanup
        && end
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.at >= facts.wall.instant())
    {
        end.outcome = facts
            .failed(RouteError::Deadline { turn: facts.number })
            .outcome;
    }
    if let Err(AdapterError::Route(route)) = &mut end.outcome {
        route.acknowledged |= acknowledged;
        route.cleanup.get_or_insert(match cleanup {
            Cleanup::Quiescent => WireCleanup::Quiescent,
            Cleanup::Uncertain | Cleanup::Pending => WireCleanup::Uncertain,
        });
    }
    end
}
