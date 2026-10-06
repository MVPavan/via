//! `opencode.md` §7.2–§7.3: admission, one submission and the outcome boundary.

use super::driver::{Settings, Turn, effort_offered, switch_variant, tracked};
use crate::driver::turn::{CLEANUP_ALLOWANCE, unaccounted};
use crate::opencode::delivery::{Delivery, Registration, Sealed, Stop, generation_route_error};
use crate::opencode::launch;
use crate::{
    AdapterError, Deadline, DriverFailure, DriverHealth, RouteError, StartRejected, TurnEnd,
    TurnEvidence, TurnNumber, VendorCode,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;
use via_routes::opencode::GenerationEnd;
use via_routes::opencode::Server;
use via_routes::opencode::router::LaneFailure;
use via_routes::opencode::turn::{self as requests, Submission};
use via_routes::{CommitOutcome, StoreFailure};

/// §7.1: deterministic caller IDs. Recomputing old IDs never resends them.
fn input_id(session: &crate::SessionId, turn: TurnNumber) -> String {
    let digest = launch::digest(|field| {
        field(b"via-opencode-input-v1");
        field(session.as_str().as_bytes());
        field(&turn.get().to_be_bytes());
    });
    // 22 base62 digits encode the leading 128 hash bits, padded to width.
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    let mut value = u128::from_be_bytes(bytes);
    let alphabet = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut encoded = [b'0'; 22];
    for digit in encoded.iter_mut().rev() {
        *digit = alphabet[usize::try_from(value % 62).unwrap_or(0)];
        value /= 62;
    }
    let mut id = String::from("msg_via");
    id.extend(encoded.into_iter().map(char::from));
    id
}

/// Outcome state is held in the delivery lane; server facts gate dispatch only.
pub(super) struct Running {
    pub(super) server: Arc<Server>,
    pub(super) session: String,
    pub(super) input: String,
    pub(super) delivery: Arc<Delivery>,
    pub(super) sent: Arc<requests::SentTracker>,
    pub(super) prompt_pending: bool,
    pub(super) request_loss: Option<GenerationEnd>,
    pub(super) cleanup: Option<crate::Cleanup>,
    pub(super) acknowledgement_cutoff: Option<Deadline>,
    health: Arc<watch::Sender<DriverHealth>>,
    turn: TurnNumber,
    finished: bool,
}

impl Drop for Running {
    fn drop(&mut self) {
        if !self.finished {
            if self.prompt_pending {
                if self.sent.is_sent() {
                    self.server.drain();
                } else {
                    let mut routing = self.server.routing();
                    routing.request_completed(&self.session);
                    routing.never_sent(&self.session, &self.input);
                }
            }
            self.delivery.seal();
            self.server.routing().settle(&self.session, self.turn);
            crate::driver::latch(&self.health, DriverFailure::TurnAbandoned);
        }
    }
}

/// §6: the identity and checked settings opened for this turn.
pub(super) struct Opened {
    pub(super) server: Arc<Server>,
    pub(super) id: String,
    pub(super) generation: u64,
    pub(super) variant: String,
    pub(super) variant_checked: bool,
}

/// §7.2: admit, submit once and resolve by the ordered delivery lane.
pub(super) async fn execute(
    facts: &mut Turn<'_>,
    settings: &Settings,
    opened: Opened,
    digest: &str,
    activity: crate::TurnActivity,
    prompt: &str,
) -> TurnEnd {
    let (input, delivery) = match prepare(facts, settings, &opened, digest, activity).await {
        Ok(prepared) => prepared,
        Err(end) => return *end,
    };
    submit(facts, &opened.server, &opened.id, &input, delivery, prompt).await
}

async fn prepare(
    facts: &mut Turn<'_>,
    settings: &Settings,
    opened: &Opened,
    digest: &str,
    activity: crate::TurnActivity,
) -> Result<(String, Arc<Delivery>), Box<TurnEnd>> {
    let (server, id, generation) = (&opened.server, &opened.id, opened.generation);
    let registration = facts.session.delivery(facts.driver, server, id, generation);
    let first = server.routing().begin_reopen_cleanup(id);
    if first
        && facts.reopened
        && let Err(end) = cleanup_leftovers(facts, server, id).await
    {
        return Err(end);
    }
    if first {
        server.routing().finish_reopen_cleanup(id);
    }
    let admission_by = facts.request_by();
    if !wait_eligible(server, id, admission_by).await {
        return Err(
            if server.is_draining() || server.failure().is_some() || server.ended().is_some() {
                Box::new(facts.rejected(StartRejected::SessionGone))
            } else {
                session_busy(facts)
            },
        );
    }
    if server.is_draining() || server.failure().is_some() || server.ended().is_some() {
        return Err(Box::new(facts.rejected(StartRejected::SessionGone)));
    }
    if !opened.variant_checked
        && let Some(variant) = &settings.variant
        && let Err(end) = effort_offered(facts, server, settings, variant).await
    {
        return Err(end);
    }
    switch_variant(
        facts,
        server,
        (settings, id, opened.variant.clone()),
        digest,
    )
    .await?;
    let link_by = Deadline::at((Instant::now() + Duration::from_secs(3)).min(facts.wall.instant()));
    match server
        .link_turn(&facts.driver.spec.session_id, facts.number, link_by)
        .await
    {
        CommitOutcome::Committed(()) => {}
        CommitOutcome::NotCommitted(_) => {
            return Err(Box::new(facts.failed(RouteError::Store {
                turn: facts.number,
                kind: StoreFailure::NotCommitted,
            })));
        }
        CommitOutcome::Uncertain(_) => {
            facts.driver.journal.send_replace(true);
            return Err(Box::new(facts.failed(RouteError::Store {
                turn: facts.number,
                kind: StoreFailure::Uncertain,
            })));
        }
    }
    let input = input_id(&facts.driver.spec.session_id, facts.number);
    let delivery = admit_ready(
        facts,
        server,
        id,
        &input,
        &registration,
        activity,
        admission_by,
    )
    .await?;
    let sent = Arc::new(requests::SentTracker::new({
        let server = Arc::clone(server);
        let id = id.clone();
        let input = input.clone();
        move || server.routing().mark_sent(&id, &input)
    }));
    facts.running = Some(Running {
        server: Arc::clone(server),
        session: id.clone(),
        input: input.clone(),
        delivery: Arc::clone(&delivery),
        sent,
        prompt_pending: true,
        request_loss: None,
        cleanup: None,
        acknowledgement_cutoff: None,
        health: Arc::clone(&facts.driver.health),
        turn: facts.number,
        finished: false,
    });
    Ok((input, delivery))
}

fn session_busy(facts: &Turn<'_>) -> Box<TurnEnd> {
    Box::new(facts.rejected(StartRejected::VendorError(
        Some(VendorCode::from("session_busy".to_owned())),
        "the session did not become ready within the prompt admission budget".to_owned(),
    )))
}

async fn admit_ready(
    facts: &Turn<'_>,
    server: &Server,
    id: &str,
    input: &str,
    registration: &Registration,
    activity: crate::TurnActivity,
    by: Deadline,
) -> Result<Arc<Delivery>, Box<TurnEnd>> {
    loop {
        {
            let mut routing = server.routing();
            if server.is_draining() || server.failure().is_some() || server.ended().is_some() {
                return Err(Box::new(facts.rejected(StartRejected::SessionGone)));
            }
            // A foreign execution can begin during the effort/readback or
            // journal steps. Admit only while the rule still holds, without
            // leaving a delivery registered on a rejected turn.
            if routing.eligible(id) {
                let delivery = registration.admit(
                    input.to_owned(),
                    facts.number,
                    activity,
                    facts.instance.clone(),
                );
                routing.register_turn_with_deadline(id, input.to_owned(), facts.number, facts.wall);
                routing.request_started(id);
                if routing.failure().is_some() {
                    delivery.seal();
                    routing.never_sent(id, input);
                    routing.settle(id, facts.number);
                    return Err(Box::new(facts.rejected(StartRejected::SessionGone)));
                }
                return Ok(delivery);
            }
        }
        if !wait_eligible(server, id, by).await {
            return Err(session_busy(facts));
        }
    }
}

/// §8: a dropped socket fixes first-byte evidence before drain withdrawal is reported.
async fn prompt_response(
    facts: &mut Turn<'_>,
    server: &Server,
    id: &str,
    input: &str,
    prompt: &str,
    sent: &requests::SentTracker,
) -> Option<Result<Submission, requests::HttpError>> {
    // Dropping the request closes its socket. The retained first-byte tracker
    // proves whether cancellation may withdraw it or must preserve its effects.
    let outcome = {
        let response =
            requests::prompt_tracked(server.http(), id, input, prompt, facts.request_by(), sent);
        tokio::pin!(response);
        tokio::select! {
            biased;
            () = server.wait_draining() => {
                if sent.is_sent() {
                    response.await
                } else {
                    server.routing().request_completed(id);
                    server.routing().never_sent(id, input);
                    if let Some(running) = facts.running.as_mut() {
                        running.prompt_pending = false;
                    }
                    return None;
                }
            },
            outcome = &mut response => outcome,
        }
    };
    Some(outcome)
}

async fn submit(
    facts: &mut Turn<'_>,
    server: &Arc<Server>,
    id: &str,
    input: &str,
    delivery: Arc<Delivery>,
    prompt: &str,
) -> TurnEnd {
    let sent = facts
        .running
        .as_ref()
        .map(|running| Arc::clone(&running.sent));
    let Some(sent) = sent else {
        return facts.rejected(StartRejected::SessionGone);
    };
    let Some(outcome) = prompt_response(facts, server, id, input, prompt, &sent).await else {
        let _sealed = ordered_result(facts, None);
        return facts.rejected(StartRejected::SessionGone);
    };
    facts.submitted = sent.is_sent();
    match outcome {
        Ok(Submission::Accepted) => {
            {
                let mut routing = server.routing();
                routing.request_completed(id);
                routing.accepted(id, input, Instant::now());
            }
            if let Some(running) = facts.running.as_mut() {
                running.prompt_pending = false;
            }
        }
        Ok(Submission::Status(status @ (400 | 404))) => {
            {
                let mut routing = server.routing();
                routing.request_completed(id);
                routing.not_accepted(id, input);
            }
            if let Some(running) = facts.running.as_mut() {
                running.prompt_pending = false;
            }
            let mut end = ordered_result(facts, None);
            if end.terminal.is_none() {
                end = facts.rejected(if status == 404 {
                    StartRejected::SessionGone
                } else {
                    StartRejected::Protocol("invalid_request".into())
                });
            }
            return end;
        }
        Ok(Submission::Status(401)) => {
            server.fail_protocol();
            {
                let mut routing = server.routing();
                routing.request_completed(id);
                routing.not_accepted(id, input);
            }
            if let Some(running) = facts.running.as_mut() {
                running.prompt_pending = false;
            }
            facts.submitted = false;
            return ordered_result(
                facts,
                Some(RouteError::Protocol {
                    turn: facts.number,
                    detail: "the server refused VIA's credentials",
                }),
            );
        }
        Ok(Submission::Status(_) | Submission::Inconclusive) => {
            {
                let mut routing = server.routing();
                routing.inconclusive(id, input, Instant::now());
            }
            if let Some(running) = facts.running.as_mut() {
                running.prompt_pending = false;
            }
            server.drain();
            server.routing().request_completed(id);
        }
        Err(error) => {
            if let Some(end) = failed_prompt(facts, server, id, input, error).await {
                return end;
            }
        }
    }
    delivery.decision().await;
    complete(facts).await
}

/// §8, §10: place unknown submission before Host's bounded loss classification wait.
async fn failed_prompt(
    facts: &mut Turn<'_>,
    server: &Server,
    id: &str,
    input: &str,
    error: requests::HttpError,
) -> Option<TurnEnd> {
    if error.is_response_limit() {
        // §8–§9: bounds evidence decides this turn, while other sent turns drain.
        let detected_at = Instant::now();
        server.drain();
        {
            let mut routing = server.routing();
            routing.response_limit(id, facts.number, detected_at);
            routing.request_completed(id);
        }
        if let Some(running) = facts.running.as_mut() {
            running.prompt_pending = false;
            running.delivery.response_limit();
        }
        return Some(ordered_result(facts, None));
    }
    if error.sent == requests::Sent::No {
        {
            let mut routing = server.routing();
            routing.request_completed(id);
            routing.never_sent(id, input);
        }
        if let Some(running) = facts.running.as_mut() {
            running.prompt_pending = false;
        }
        facts.submitted = false;
        let mut end = ordered_result(facts, None);
        if end.terminal.is_none() && (server.is_draining() || server.failure().is_some()) {
            end = facts.rejected(StartRejected::SessionGone);
        }
        return Some(end);
    }
    // The boundary precedes Host's wait: acceptance arriving during that
    // wait can never turn an unknown submission into an accepted one.
    server.routing().inconclusive(id, input, Instant::now());
    server.drain();
    if let Some(end) = server.request_lost(facts.wall).await {
        if let Some(running) = facts.running.as_mut() {
            running.request_loss = Some(end);
        }
        return Some(ordered_result(facts, None));
    }
    None
}

/// §7.4: the retained terminal stands while reported tool items finish within grace.
async fn complete(facts: &mut Turn<'_>) -> TurnEnd {
    let Some(running) = facts.running.as_ref() else {
        return ordered_result(facts, None);
    };
    let delivery = Arc::clone(&running.delivery);
    let server = Arc::clone(&running.server);
    if delivery.terminal_at().is_none() {
        // §9: a failed nonterminal lane cannot provide tool cleanup evidence.
        // Queue its native cleanup at the outcome boundary without consuming grace.
        return ordered_result(facts, None);
    }
    let by = Deadline::at(
        (delivery.terminal_at().unwrap_or_else(Instant::now) + facts.tool_grace)
            .min(facts.wall.instant()),
    );
    // Both waits are read-only; dropping them preserves terminal and tool evidence.
    let cleanup = tokio::select! {
        cleanup = delivery.cleanup(by) => cleanup,
        _end = server.wait_end() => crate::Cleanup::Uncertain,
    };
    if let Some(running) = facts.running.as_mut() {
        running.cleanup = Some(cleanup);
    }
    ordered_result(facts, None)
}

async fn wait_eligible(server: &Server, id: &str, by: Deadline) -> bool {
    let changed = server.routing().notify();
    loop {
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if server.is_draining() || server.failure().is_some() || server.ended().is_some() {
            return false;
        }
        if server.routing().eligible(id) {
            return true;
        }
        // Notify, timer and end waits consume no execution state when cancelled.
        tokio::select! {
            () = &mut notified => {},
            () = tokio::time::sleep_until(by.instant()) => return false,
            _end = server.wait_end() => return false,
        }
    }
}

async fn cleanup_leftovers(
    facts: &Turn<'_>,
    server: &Server,
    id: &str,
) -> Result<(), Box<TurnEnd>> {
    let inbox = tracked(
        server,
        Some(id),
        requests::inbox(server.http(), id, facts.request_by()),
    )
    .await;
    let inbox =
        inbox.map_err(|error| Box::new(facts.setup_failed("the reopen inbox listing", error)))?;
    for number in 1..facts.number.get() {
        let Ok(turn) = TurnNumber::try_from(number) else {
            continue;
        };
        let input = input_id(&facts.driver.spec.session_id, turn);
        if !inbox.contains(&input) {
            continue;
        }
        server.routing().expect_cleanup_cancellation(id, &input);
        if server.routing().failure().is_some() {
            return Err(Box::new(facts.rejected(StartRejected::SessionGone)));
        }
        let result = tracked(
            server,
            Some(id),
            requests::cancel_leftover(server.http(), id, &input, facts.request_by()),
        )
        .await;
        result.map_err(|error| {
            Box::new(facts.setup_failed("a leftover inbox cancellation", error))
        })?;
        let changed = server.routing().notify();
        let cancelled_by = facts.request_by();
        loop {
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if server.routing().cleanup_cancelled(id, &input) {
                break;
            }
            // These waits consume no cancellation proof or route state on drop.
            tokio::select! {
                () = &mut notified => {},
                () = tokio::time::sleep_until(cancelled_by.instant()) => {
                    return Err(Box::new(facts.rejected(StartRejected::VendorError(
                        Some(VendorCode::from("session_busy".to_owned())),
                        "a leftover input cancellation was not observed".to_owned(),
                    ))));
                },
                _end = server.wait_end() => {
                    return Err(Box::new(facts.rejected(StartRejected::SessionGone)));
                },
            }
        }
    }
    Ok(())
}

/// §9: native cleanup is reserved before this turn releases successor admission.
fn settle_after_cleanup(running: &Running, turn: TurnNumber, wall: Deadline, overflow: bool) {
    let mut routing = running.server.routing();
    if overflow {
        let by = Deadline::at((Instant::now() + CLEANUP_ALLOWANCE).min(wall.instant()));
        routing.enqueue_cleanup(&running.session, turn, by);
    }
    routing.settle(&running.session, turn);
}

/// §8–§9: positive response/text bounds evidence takes priority over a caller stop.
fn outcome_error(turn: TurnNumber, stop: Option<Stop>, cause: Option<RouteError>) -> RouteError {
    if !matches!(stop, Some(Stop::ResponseLimit | Stop::TextOverflow))
        && let Some(cause) = cause
    {
        return cause;
    }
    match stop {
        Some(Stop::ResponseLimit) => RouteError::Protocol {
            turn,
            detail: "an HTTP response exceeded the OpenCode route bounds",
        },
        Some(Stop::DeclineFailed) => RouteError::Protocol {
            turn,
            detail: "a VIA interactive decline did not settle",
        },
        Some(Stop::Lane(LaneFailure::Protocol)) => RouteError::Protocol {
            turn,
            detail: "a session event did not match the protocol",
        },
        Some(Stop::TextOverflow | Stop::Lane(LaneFailure::Overflow)) => {
            RouteError::Overflow { turn }
        }
        Some(Stop::Generation(GenerationEnd::Lost(loss))) => {
            generation_route_error(loss.cause, turn)
        }
        Some(
            Stop::Generation(GenerationEnd::Retired) | Stop::Detached | Stop::SubmissionUnknown,
        )
        | None => RouteError::TransportLost { turn },
    }
}

/// Seal exactly once: this is the outcome boundary, independent of route state.
pub(super) fn ordered_result(facts: &mut Turn<'_>, cause: Option<RouteError>) -> TurnEnd {
    let Some(mut running) = facts.running.take() else {
        return facts.failed(cause.unwrap_or(RouteError::TransportLost { turn: facts.number }));
    };
    running.finished = true;
    let Sealed {
        cleanup,
        terminal,
        accepted,
        loss,
        stop,
        accounted,
    } = running.delivery.seal_until(running.acknowledgement_cutoff);
    let text_overflow = matches!(stop, Some(Stop::TextOverflow));
    let overflow = text_overflow
        || (terminal.is_none()
            && running.request_loss.is_none()
            && matches!(stop, Some(Stop::Lane(LaneFailure::Overflow))));
    settle_after_cleanup(&running, facts.number, facts.wall, overflow);
    let stop = if matches!(stop, Some(Stop::ResponseLimit | Stop::TextOverflow)) {
        stop
    } else {
        running.request_loss.map(Stop::Generation).or(stop)
    };
    let cleanup = running.cleanup.unwrap_or(cleanup);
    let decline_failed = matches!(stop, Some(Stop::DeclineFailed));
    let response_limit = matches!(stop, Some(Stop::ResponseLimit));
    let terminal_present = terminal.is_some();
    let mut end = if terminal_present && !decline_failed && !response_limit && !text_overflow {
        TurnEnd {
            terminal,
            instance: facts.instance.clone(),
            leftovers: None,
            outcome: Ok(TurnEvidence {
                exit: None,
                cleanup,
                journal_uncertain: false,
            }),
            loss,
            aggregate: None,
        }
    } else {
        let error = outcome_error(facts.number, stop, cause);
        let mut end = facts.failed(error);
        if let Some(Stop::Generation(GenerationEnd::Lost(loss))) = stop {
            if let Err(AdapterError::Route(failure)) = &mut end.outcome {
                failure.exit = loss.exit;
                failure.cleanup = Some(loss.cleanup);
                failure.journal_uncertain = loss.journal_uncertain;
            }
            end.leftovers = running.server.leftovers().map(Into::into);
        }
        end.loss = loss;
        if overflow && let Err(AdapterError::Route(failure)) = &mut end.outcome {
            failure.cleanup = Some(crate::WireCleanup::Uncertain);
        }
        if decline_failed || response_limit || text_overflow {
            end.terminal = terminal;
        }
        end
    };
    if accepted && !accounted {
        unaccounted(&mut end);
        if let Some(terminal) = end.terminal.as_mut() {
            terminal.vendor = None;
        }
    }
    end
}
